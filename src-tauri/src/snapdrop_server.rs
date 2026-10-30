//! SnapDrop-compatible LAN transfer hub.
//!
//! The desktop is a peer in the room like any browser tab, but a headless one:
//! it appears in every client's peer list with `rtcSupported: false`, so a phone
//! that picks it sends files over the same WebSocket relay a pair of browsers
//! would use. That is the whole point of the feature — "发到本机" — and it needs
//! no WebRTC stack on this side: the relayed chunk protocol (`header`, binary
//! chunks, `partition`, `partition-received`) is enough to reassemble a file.
//!
//! Everything lives on one `0.0.0.0:<port>` listener: `/` serves the SnapDrop
//! page, `/server/webrtc` is the signalling socket, `/upload` accepts plain
//! multipart for any client that would rather POST.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Multipart, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tauri::Emitter;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tower_http::services::ServeDir;

/// The headless desktop peer. Clients never see this id; they see the name in
/// `peers`, which is why the home device keeps the plain device name while
/// browser tabs get a suffix — otherwise a phone would see two identical names.
const HUB_ID: &str = "horizon-hub";

pub type ConfigStore = Arc<RwLock<SnapdropConfig>>;

struct Peer {
    name: String,
    rtc_supported: bool,
    tx: mpsc::UnboundedSender<Message>,
}

type PeerMap = Arc<RwLock<HashMap<String, Peer>>>;

#[derive(Clone)]
struct AppState {
    peers: PeerMap,
    config: ConfigStore,
}

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

fn emit_to_ui(event: &str, payload: serde_json::Value) {
    if let Some(app) = APP.get() {
        let _ = app.emit(event, payload);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapdropConfig {
    // The frontend reads these as `deviceName` / `storageDir`; the aliases keep
    // a config file written before the rename readable.
    #[serde(alias = "device_name")]
    pub device_name: String,
    #[serde(alias = "storage_dir")]
    pub storage_dir: String,
    #[serde(alias = "lan_ip")]
    pub lan_ip: String,
    pub port: u16,
}

impl Default for SnapdropConfig {
    fn default() -> Self {
        Self {
            device_name: "MeetingHub".to_string(),
            storage_dir: dirs_default_storage(),
            lan_ip: "127.0.0.1".to_string(),
            port: 7878,
        }
    }
}

fn dirs_default_storage() -> String {
    if let Some(home) = dirs_next::home_dir() {
        home.join("Desktop").join("MeetingHub").to_string_lossy().to_string()
    } else {
        "C:\\Users\\Public\\Desktop\\MeetingHub".to_string()
    }
}

fn get_config_path() -> PathBuf {
    if let Some(data_dir) = dirs_next::data_dir() {
        data_dir.join("horizon-launcher").join("snapdrop-config.json")
    } else {
        PathBuf::from("snapdrop-config.json")
    }
}

pub fn load_config() -> SnapdropConfig {
    let config_path = get_config_path();
    if config_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            if let Ok(config) = serde_json::from_str(&content) {
                return config;
            }
        }
    }
    SnapdropConfig::default()
}

pub fn save_config(config: &SnapdropConfig) -> Result<(), String> {
    let config_path = get_config_path();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&config_path, content).map_err(|e| e.to_string())?;
    Ok(())
}

fn generate_peer_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

fn send_json(tx: &mpsc::UnboundedSender<Message>, value: serde_json::Value) {
    if let Ok(text) = serde_json::to_string(&value) {
        let _ = tx.send(Message::Text(text));
    }
}

// ------------------------------------------------------------------ sockets

async fn webrtc_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, addr, state))
}

async fn handle_socket(socket: WebSocket, _addr: SocketAddr, state: AppState) {
    let peer_id = generate_peer_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let (mut ws_tx, mut ws_rx) = socket.split();

    let pump = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let device_name = state.config.read().await.device_name.clone();
    let own_name = format!("{} · {}", device_name, &peer_id[..4]);

    {
        let mut peers = state.peers.write().await;
        peers.insert(
            peer_id.clone(),
            Peer {
                name: own_name.clone(),
                rtc_supported: true,
                tx: tx.clone(),
            },
        );
    }

    send_json(
        &tx,
        serde_json::json!({
            "type": "display-name",
            "message": { "displayName": own_name, "deviceName": own_name }
        }),
    );

    // SnapDrop's client builds its peer list only from this message; without it
    // a phone sees an empty grid and has nobody to send to.
    {
        let peers = state.peers.read().await;
        let list: Vec<serde_json::Value> = peers
            .iter()
            .filter(|(id, _)| id.as_str() != peer_id)
            .map(|(id, peer)| {
                // The bundled client renders `peer.name.displayName`; a bare
                // string here leaves every row in its peer grid unlabelled.
                let name = if id == HUB_ID {
                    device_name.clone()
                } else {
                    peer.name.clone()
                };
                serde_json::json!({
                    "id": id,
                    "name": { "displayName": name, "deviceName": name },
                    "rtcSupported": peer.rtc_supported
                })
            })
            .collect();
        send_json(&tx, serde_json::json!({ "type": "peers", "peers": list }));
        let joined = serde_json::json!({
            "id": peer_id,
            "name": { "displayName": own_name, "deviceName": own_name },
            "rtcSupported": true
        });
        for (id, peer) in peers.iter() {
            if id.as_str() != peer_id {
                send_json(&peer.tx, serde_json::json!({ "type": "peer-joined", "peer": joined }));
            }
        }
    }

    // Binary chunks carry no recipient, so the last relayed text message's `to`
    // is what they belong to — that is how the SnapDrop client uses this socket.
    let mut last_to: Option<String> = None;

    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                match value.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
                    "disconnect" => break,
                    "pong" => {}
                    _ => {
                        let Some(to) = value.get("to").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            continue;
                        };
                        last_to = Some(to.clone());
                        let mut relay = value.clone();
                        if let Some(obj) = relay.as_object_mut() {
                            obj.remove("to");
                            obj.insert("sender".into(), serde_json::json!(peer_id));
                        }
                        let peers = state.peers.read().await;
                        if let Some(recipient) = peers.get(&to) {
                            send_json(&recipient.tx, relay);
                        }
                    }
                }
            }
            Message::Binary(chunk) => {
                let Some(to) = last_to.clone() else { continue };
                let peers = state.peers.read().await;
                if let Some(recipient) = peers.get(&to) {
                    let _ = recipient.tx.send(Message::Binary(chunk));
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    {
        let mut peers = state.peers.write().await;
        peers.remove(&peer_id);
        for peer in peers.values() {
            send_json(
                &peer.tx,
                serde_json::json!({ "type": "peer-left", "peerId": peer_id }),
            );
        }
    }

    pump.abort();
}

// ------------------------------------------------------------- hub (this PC)

struct Incoming {
    name: String,
    path: PathBuf,
    file: tokio::fs::File,
    expected: u64,
    received: u64,
    reported: f64,
}

async fn reply(peers: &PeerMap, to: &Option<String>, value: serde_json::Value) {
    let Some(id) = to else { return };
    let map = peers.read().await;
    if let Some(peer) = map.get(id) {
        // Clients route relayed traffic by `sender`; a reply without it is
        // dropped as an unknown message type and the transfer never advances.
        let mut value = value;
        if let Some(obj) = value.as_object_mut() {
            obj.entry("sender")
                .or_insert_with(|| serde_json::json!(HUB_ID));
        }
        send_json(&peer.tx, value);
    }
}

async fn hub_loop(
    mut rx: mpsc::UnboundedReceiver<Message>,
    config: ConfigStore,
    peers: PeerMap,
) {
    let mut incoming: Option<Incoming> = None;
    // The relay stamps `sender` on every text message; binary chunks reuse it.
    let mut sender: Option<String> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if let Some(from) = value.get("sender").and_then(|v| v.as_str()) {
                    sender = Some(from.to_string());
                }
                match value.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
                    "header" => {
                        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("file");
                        let expected = value.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                        let dir = PathBuf::from(config.read().await.storage_dir.clone());
                        if let Err(error) = tokio::fs::create_dir_all(&dir).await {
                            eprintln!("[snapdrop] 无法创建存储目录：{error}");
                            continue;
                        }
                        let path = unique_path(&dir, &sanitize_file_name(name));
                        match tokio::fs::File::create(&path).await {
                            Ok(file) => {
                                incoming = Some(Incoming {
                                    name: path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_default(),
                                    path,
                                    file,
                                    expected,
                                    received: 0,
                                    reported: 0.0,
                                });
                                emit_to_ui(
                                    "snapdrop-progress",
                                    serde_json::json!({ "name": name, "progress": 0.0 }),
                                );
                                // An empty file has no chunk to check, so the
                                // byte-count finish never fires for it.
                                if expected == 0 {
                                    finish_file(&mut incoming, &peers, &sender).await;
                                }
                            }
                            Err(error) => eprintln!("[snapdrop] 无法创建文件：{error}"),
                        }
                    }
                    "partition" => {
                        // The sender waits for this before shipping the next
                        // partition; skipping it stalls the transfer.
                        reply(
                            &peers,
                            &sender,
                            serde_json::json!({
                                "type": "partition-received",
                                "offset": value.get("offset").cloned().unwrap_or_default()
                            }),
                        )
                        .await;
                        if let Some(state) = incoming.as_ref() {
                            if state.received >= state.expected {
                                finish_file(&mut incoming, &peers, &sender).await;
                            }
                        }
                    }
                    "text" => {
                        let body = value.get("text").and_then(|v| v.as_str()).unwrap_or_default();
                        let dir = PathBuf::from(config.read().await.storage_dir.clone());
                        let _ = tokio::fs::create_dir_all(&dir).await;
                        let path = dir.join("收到的文字.txt");
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or_default();
                        let entry = format!("[{stamp}] {body}\n");
                        if let Ok(mut file) = tokio::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .await
                        {
                            let _ = file.write_all(entry.as_bytes()).await;
                        }
                        reply(
                            &peers,
                            &sender,
                            serde_json::json!({ "type": "transfer-complete" }),
                        )
                        .await;
                        emit_to_ui(
                            "snapdrop-file",
                            serde_json::json!({
                                "name": "收到的文字.txt",
                                "path": path.to_string_lossy(),
                                "bytes": entry.len()
                            }),
                        );
                    }
                    _ => {}
                }
            }
            Message::Binary(chunk) => {
                let Some(state) = incoming.as_mut() else { continue };
                if state.file.write_all(&chunk).await.is_err() {
                    incoming = None;
                    continue;
                }
                state.received += chunk.len() as u64;
                let progress = if state.expected == 0 {
                    1.0
                } else {
                    (state.received as f64 / state.expected as f64).min(1.0)
                };
                if progress - state.reported >= 0.05 || progress >= 1.0 {
                    state.reported = progress;
                    reply(
                        &peers,
                        &sender,
                        serde_json::json!({ "type": "progress", "progress": progress }),
                    )
                    .await;
                    emit_to_ui(
                        "snapdrop-progress",
                        serde_json::json!({ "name": state.name, "progress": progress }),
                    );
                }
                // The bundled client does not send a closing `partition` when
                // the file ends mid-partition, so the partition arm alone would
                // leave the last file open (and the sender waiting forever).
                let complete = incoming
                    .as_ref()
                    .map(|state| state.expected > 0 && state.received >= state.expected)
                    .unwrap_or(false);
                if complete {
                    finish_file(&mut incoming, &peers, &sender).await;
                }
            }
            _ => {}
        }
    }
}

async fn finish_file(incoming: &mut Option<Incoming>, peers: &PeerMap, sender: &Option<String>) {
    let Some(mut state) = incoming.take() else { return };
    if let Err(error) = state.file.flush().await {
        eprintln!("[snapdrop] 写入失败：{error}");
        return;
    }
    drop(state.file);
    emit_to_ui(
        "snapdrop-file",
        serde_json::json!({
            "name": state.name,
            "path": state.path.to_string_lossy(),
            "bytes": state.received
        }),
    );
    reply(
        peers,
        sender,
        serde_json::json!({ "type": "transfer-complete" }),
    )
    .await;
}

fn sanitize_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file")
        .trim()
        .to_string();
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_string()
    } else {
        cleaned
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for index in 1..1000 {
        let file = match &ext {
            Some(ext) => format!("{stem} ({index}).{ext}"),
            None => format!("{stem} ({index})"),
        };
        let next = dir.join(file);
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{stem}-{}", uuid::Uuid::new_v4()))
}

// ------------------------------------------------------------------ upload

async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let storage_dir = PathBuf::from(state.config.read().await.storage_dir.clone());

    if let Err(e) = tokio::fs::create_dir_all(&storage_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("无法创建存储目录：{}", e),
        );
    }

    let mut saved_files = Vec::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let file_name = field.file_name().unwrap_or("unknown").to_string();
        let data = match field.bytes().await {
            Ok(data) => data,
            Err(_) => continue,
        };

        let file_path = unique_path(&storage_dir, &sanitize_file_name(&file_name));
        if let Err(e) = tokio::fs::write(&file_path, &data).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("保存文件失败：{}", e),
            );
        }
        saved_files.push(file_name);
        emit_to_ui(
            "snapdrop-file",
            serde_json::json!({
                "name": file_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                "path": file_path.to_string_lossy(),
                "bytes": data.len()
            }),
        );
    }

    (
        StatusCode::OK,
        format!("成功保存 {} 个文件", saved_files.len()),
    )
}

// Generate QR code as base64 PNG
pub fn generate_qr_code(url: &str) -> Result<String, String> {
    let code = QrCode::new(url.as_bytes()).map_err(|e| e.to_string())?;
    let image = code.render::<image::Luma<u8>>().build();
    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    image
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &png_bytes,
    ))
}

pub struct Started {
    pub url: String,
    pub config: ConfigStore,
}

/// Binds and starts serving. The returned task runs on whatever runtime this is
/// awaited from, so the caller must await it from a long-lived one — a private
/// runtime dropped right after `block_on` takes the listener with it.
pub async fn start_snapdrop_server(
    port: u16,
    assets_dir: PathBuf,
    app: tauri::AppHandle,
) -> Result<Started, String> {
    let _ = APP.set(app);

    let lan_ip = lan_ip();
    let mut saved = load_config();
    saved.lan_ip = lan_ip.clone();
    saved.port = port;
    let device_name = saved.device_name.clone();
    let _ = save_config(&saved);
    let config: ConfigStore = Arc::new(RwLock::new(saved));

    let peers: PeerMap = Arc::new(RwLock::new(HashMap::new()));
    let (hub_tx, hub_rx) = mpsc::unbounded_channel::<Message>();
    {
        let mut map = peers.write().await;
        map.insert(
            HUB_ID.to_string(),
            Peer {
                name: device_name,
                rtc_supported: false,
                tx: hub_tx,
            },
        );
    }
    tokio::spawn(hub_loop(hub_rx, config.clone(), peers.clone()));

    let state = AppState {
        peers: peers.clone(),
        config: config.clone(),
    };

    let app_router = Router::new()
        .route("/server/webrtc", get(webrtc_handler))
        .route("/upload", post(upload_file))
        .fallback_service(ServeDir::new(assets_dir))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|error| format!("无法监听端口 {port}：{error}"))?;

    let url = format!("http://{}:{}", lan_ip, port);
    eprintln!("[snapdrop] listening on {url}");

    tokio::spawn(async move {
        if let Err(error) = axum::serve(
            listener,
            app_router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            eprintln!("[snapdrop] server stopped：{error}");
        }
    });

    Ok(Started { url, config })
}

/// The address a phone on the same LAN has to type. Taken from the routing
/// table rather than from an adapter list, so a Hyper-V or VPN adapter cannot
/// win just by being enumerated first.
pub fn lan_ip() -> String {
    let primary = horizon_cast::discovery::primary_interface();
    if !primary.is_unspecified() {
        return primary.to_string();
    }
    "127.0.0.1".to_string()
}
