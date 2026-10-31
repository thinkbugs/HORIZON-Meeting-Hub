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
//! multipart for any client that would rather POST, and `/media/<id>` serves the
//! projection library so a file sent to the hub is playable by anything on the LAN
//! that can name it.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, DefaultBodyLimit, Multipart, Path as PathSeg, State,
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
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

use crate::media_http;
use crate::media_stage::MediaRuntime;

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
    /// The library, the queue and the picture surface behind them. Shared with the
    /// DLNA renderer, so a controller and a phone are looking at one state.
    runtime: Arc<MediaRuntime>,
}

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

pub(crate) fn emit_to_ui(event: &str, payload: serde_json::Value) {
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

/// A guard, not a policy. The transfer page has no reason to refuse a two-hour
/// recording, so the multipart body limit is disabled on these routes and this
/// number is what stops a client that keeps sending from filling the disk.
const UPLOAD_BYTE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

/// `POST /upload` — the plain-multipart path for a client that will not use the
/// relay. Each chunk lands on disk as it arrives; the old version buffered the
/// whole body, which turned a 500 MB video into a 500 MB allocation.
///
/// The reply carries CORS headers because the client that asks "did it arrive?"
/// is a browser page on another origin: without them the bytes land and the
/// sender still sees a failed fetch, which is the worst possible answer to give
/// someone holding a folder of files.
async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Response {
    let storage_dir = PathBuf::from(state.config.read().await.storage_dir.clone());

    if let Err(e) = tokio::fs::create_dir_all(&storage_dir).await {
        return with_cors(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("无法创建存储目录：{e}"),
            )
                .into_response(),
        );
    }

    let mut saved_files = Vec::new();

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let file_name = field.file_name().unwrap_or("unknown").to_string();
        let file_path = unique_path(&storage_dir, &sanitize_file_name(&file_name));
        let received = match media_http::field_to_file(&mut field, &file_path, UPLOAD_BYTE_LIMIT)
            .await
        {
            Ok(received) => received,
            Err((error, _)) => {
                let _ = tokio::fs::remove_file(&file_path).await;
                return with_cors((StatusCode::BAD_REQUEST, error).into_response());
            }
        };
        saved_files.push(file_name);
        emit_to_ui(
            "snapdrop-file",
            serde_json::json!({
                "name": file_path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                "path": file_path.to_string_lossy(),
                "bytes": received
            }),
        );
    }

    with_cors((StatusCode::OK, format!("成功保存 {} 个文件", saved_files.len())).into_response())
}

/// `POST /api/media/upload` — the same bytes, but the destination is the
/// projection library, so the file is immediately a queue item and a `/media/<id>`
/// address rather than something in a downloads folder.
async fn upload_to_library(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Response {
    let library = Arc::clone(&state.runtime.library);
    let mut added = Vec::new();
    while let Ok(Some(mut field)) = multipart.next_field().await {
        // A text field in the same request is not media; skip it instead of
        // storing a stray `title=...` file in the library folder.
        if field.file_name().is_none() {
            continue;
        }
        let desired = field.file_name().unwrap_or("unknown").to_string();
        let mime = field.content_type().unwrap_or("application/octet-stream").to_string();
        let (name, path) = library.stage(&desired);
        let received = match media_http::field_to_file(&mut field, &path, UPLOAD_BYTE_LIMIT).await {
            Ok(received) => received,
            Err((error, _)) => {
                let _ = tokio::fs::remove_file(&path).await;
                return with_cors(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": error })),
                    )
                        .into_response(),
                );
            }
        };
        if received == 0 {
            let _ = tokio::fs::remove_file(&path).await;
            continue;
        }
        added.push(library.add_file(name, &desired, received, &mime));
    }
    if !added.is_empty() {
        emit_to_ui("media-library", state.runtime.stage.library_json());
    }
    // The item ids are the whole reply, so a sender that cannot read them has no
    // way to tell a stored file from a body that stopped halfway — this answer
    // crosses origins like every other media route does.
    with_cors(Json(serde_json::json!({ "items": added })).into_response())
}

/// `GET /media/:id` — one library file, with ranges.
async fn media_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathSeg(id): PathSeg<String>,
) -> Response {
    media_http::serve(&state.runtime.library, Method::GET, &headers, &id).await
}

/// `HEAD /media/:id` — the same headers and status, no bytes. A media engine uses
/// this to learn the length before it starts asking for ranges.
async fn media_head(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathSeg(id): PathSeg<String>,
) -> Response {
    media_http::serve(&state.runtime.library, Method::HEAD, &headers, &id).await
}

/// `GET|HEAD /media/:id/<name>` — the same file, addressed by id.
///
/// The name is not looked at: the library resolves the id and the id alone, so a
/// wrong, stale or hand-typed name is a wrong caption and never a different byte.
/// It is in the URL because every client that has only an address to go on reads
/// the kind and the title out of the last segment — and MediaEngine, told it is
/// fetching `…/media/<id>`, puts a still photo through the video decoder and
/// answers `SRC_NOT_SUPPORTED`.
async fn media_get_named(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathSeg((id, _name)): PathSeg<(String, String)>,
) -> Response {
    media_http::serve(&state.runtime.library, Method::GET, &headers, &id).await
}

async fn media_head_named(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathSeg((id, _name)): PathSeg<(String, String)>,
) -> Response {
    media_http::serve(&state.runtime.library, Method::HEAD, &headers, &id).await
}

/// `GET /api/media/<what>` — `library`, `queue` or `status`.
async fn api_read(State(state): State<AppState>, PathSeg(what): PathSeg<String>) -> Response {
    api(&state, &what, &serde_json::json!({}))
}

/// `POST /api/media/<action>` — everything that changes state, and the same three
/// reads for a client that would rather send a body.
async fn api_write(
    State(state): State<AppState>,
    PathSeg(action): PathSeg<String>,
    Json(args): Json<serde_json::Value>,
) -> Response {
    api(&state, &action, &args)
}

/// Both doors into one dispatcher. The launcher's own UI sends the same action
/// names over IPC, so a phone and the big screen cannot drift apart.
fn api(state: &AppState, action: &str, args: &serde_json::Value) -> Response {
    let started = std::time::Instant::now();
    let response = state.runtime.stage.invoke(action, args);
    let body = match response {
        Ok(value) => {
            eprintln!("[media] {action} ok in {:?}", started.elapsed());
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            eprintln!("[media] {action} failed: {error}");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error })),
            )
                .into_response()
        }
    };
    // The extension pages are served from this same origin, but a controller on
    // another address that posts to the queue should not be blocked by a CORS
    // preflight it cannot answer.
    with_cors(body)
}

fn with_cors(mut body: Response) -> Response {
    let headers = body.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,HEAD,OPTIONS"),
    );
    body
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

/// The whole listener's route table, kept apart from binding so a test can put it
/// on a real socket without a Tauri app handle.
fn hub_router(state: AppState, assets_dir: PathBuf) -> Router {
    // The body limit is disabled on the two POST routes: axum's 2 MiB default is
    // a memory guard for a buffered body, and these handlers never buffer one —
    // they stream to disk under `UPLOAD_BYTE_LIMIT`.
    Router::new()
        .route("/server/webrtc", get(webrtc_handler))
        .route("/media/:id", get(media_get).head(media_head))
        .route(
            "/media/:id/:name",
            get(media_get_named).head(media_head_named),
        )
        .route(
            "/upload",
            post(upload_file).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/media/upload",
            post(upload_to_library)
                .options(preflight)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/media/:action",
            get(api_read).post(api_write).options(preflight),
        )
        .fallback_service(ServeDir::new(assets_dir))
        .with_state(state)
}

async fn preflight() -> Response {
    with_cors((StatusCode::NO_CONTENT, "").into_response())
}

/// Binds and starts serving. The returned task runs on whatever runtime this is
/// awaited from, so the caller must await it from a long-lived one — a private
/// runtime dropped right after `block_on` takes the listener with it.
pub async fn start_snapdrop_server(
    port: u16,
    assets_dir: PathBuf,
    app: tauri::AppHandle,
    runtime: Arc<MediaRuntime>,
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

    let url = format!("http://{}:{}", lan_ip, port);
    runtime.stage.set_base(&url);

    let state = AppState {
        peers: peers.clone(),
        config: config.clone(),
        runtime,
    };
    let app_router = hub_router(state, assets_dir);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|error| format!("无法监听端口 {port}：{error}"))?;

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

/// The route table on a real socket. These are HTTP tests rather than handler
/// tests on purpose: the thing that has to be proven is that hyper, axum's
/// multipart reader and a byte-range body agree over a wire, which no amount of
/// calling `serve()` directly would show.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::media_library::Library;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "horizon-hub-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Hub {
        addr: SocketAddr,
        library: Arc<Library>,
        storage: PathBuf,
        _dirs: (TempDir, TempDir),
    }

    /// The stage these tests stand up: it only has to say `ok` and report a phase,
    /// because the assertions here are about HTTP, not about decoding.
    struct StubTransport;

    impl horizon_cast::player::Transport for StubTransport {
        fn load(&self, _uri: &str) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn play(&self) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn pause(&self) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn stop(&self) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn seek(&self, _seconds: f64) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn set_volume(&self, _volume: f64) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn set_mute(&self, _muted: bool) -> horizon_cast::CastResult<()> {
            Ok(())
        }
        fn state(&self) -> horizon_cast::player::PlaybackState {
            horizon_cast::player::PlaybackState::default()
        }
    }

    impl Hub {
        async fn start(label: &str) -> Self {
            let library_dir = TempDir::new(&format!("{label}-library"));
            let storage_dir = TempDir::new(&format!("{label}-storage"));
            let mut config = SnapdropConfig::default();
            config.storage_dir = storage_dir.0.to_string_lossy().into_owned();
            let library = Arc::new(Library::open(library_dir.0.clone()));
            let runtime = Arc::new(crate::media_stage::MediaRuntime::with_transport(
                Arc::new(StubTransport),
                Arc::clone(&library),
                false,
            ));
            let state = AppState {
                peers: Arc::new(RwLock::new(HashMap::new())),
                config: Arc::new(RwLock::new(config)),
                runtime,
            };
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind an ephemeral port");
            let addr = listener.local_addr().expect("local addr");
            let assets = storage_dir.0.clone();
            tokio::spawn(async move {
                let _ = axum::serve(
                    listener,
                    hub_router(state, assets).into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await;
            });
            Self {
                addr,
                library,
                storage: storage_dir.0.clone(),
                _dirs: (library_dir, storage_dir),
            }
        }

        /// Put a file in the library folder and list it, the way an upload would.
        fn store(&self, name: &str, bytes: &[u8]) -> String {
            std::fs::write(self.library.root().join(name), bytes).expect("write file");
            self.library
                .add_file(name.to_string(), name, bytes.len() as u64, "test")
                .id
        }

        async fn send(&self, head: String, body: &[u8]) -> (String, Vec<u8>) {
            let mut stream = TcpStream::connect(self.addr).await.expect("connect");
            stream.write_all(head.as_bytes()).await.expect("send head");
            if !body.is_empty() {
                stream.write_all(body).await.expect("send body");
            }
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.expect("read response");
            let split = received
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("a complete header block");
            (
                String::from_utf8_lossy(&received[..split]).into_owned(),
                received[split + 4..].to_vec(),
            )
        }

        async fn get(&self, path: &str, range: Option<&str>) -> (String, Vec<u8>) {
            let mut head = format!(
                "GET {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n",
                path = path
            );
            if let Some(range) = range {
                head.push_str(&format!("Range: {range}\r\n"));
            }
            head.push_str("\r\n");
            self.send(head, &[]).await
        }

        async fn request(&self, method: &str, path: &str) -> (String, Vec<u8>) {
            self.send(
                format!("{method} {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n\r\n"),
                &[],
            )
            .await
        }
    }

    fn status_of(head: &str) -> u16 {
        head.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    fn header_of(head: &str, name: &str) -> Option<String> {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    /// A pattern rather than zeros, so a slice returned by the server can be
    /// matched against the byte offsets it claims to cover.
    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    fn multipart(filename: &str, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"--hubboundary\r\n");
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(payload);
        body.extend_from_slice(b"\r\n--hubboundary--\r\n");
        body
    }

    #[tokio::test]
    async fn a_library_file_is_served_whole_and_by_range() {
        let hub = Hub::start("ranges").await;
        let payload = pattern(1_000_000);
        let id = hub.store("clip.mp4", &payload);
        let path = format!("/media/{id}");

        let (head, body) = hub.get(&path, None).await;
        assert_eq!(status_of(&head), 200, "{head}");
        assert_eq!(header_of(&head, "Accept-Ranges").as_deref(), Some("bytes"));
        assert_eq!(header_of(&head, "Content-Type").as_deref(), Some("video/mp4"));
        assert_eq!(body.len(), 1_000_000);
        assert_eq!(body, payload, "the whole file has to come back byte for byte");

        let (head, body) = hub.get(&path, Some("bytes=100-199")).await;
        assert_eq!(status_of(&head), 206, "{head}");
        assert_eq!(
            header_of(&head, "Content-Range").as_deref(),
            Some("bytes 100-199/1000000")
        );
        assert_eq!(header_of(&head, "Content-Length").as_deref(), Some("100"));
        assert_eq!(body, &payload[100..200]);

        // The two shapes a media engine actually uses: run to the end, and count
        // back from it to read a trailing `moov`.
        let (_, body) = hub.get(&path, Some("bytes=999900-")).await;
        assert_eq!(body, &payload[999900..]);
        let (head, body) = hub.get(&path, Some("bytes=-50")).await;
        assert_eq!(status_of(&head), 206);
        assert_eq!(
            header_of(&head, "Content-Range").as_deref(),
            Some("bytes 999950-999999/1000000")
        );
        assert_eq!(body, &payload[999950..]);

        // A request that runs off the end is clamped, not refused.
        let (head, body) = hub.get(&path, Some("bytes=999990-1200000")).await;
        assert_eq!(status_of(&head), 206);
        assert_eq!(body.len(), 10, "clamped to what the file has: {head}");
    }

    #[tokio::test]
    async fn a_name_on_the_end_of_the_address_is_a_label_not_a_lookup() {
        let hub = Hub::start("named").await;
        let payload = pattern(4096);
        let id = hub.store("现场 照片.jpg", &payload);
        let named = format!("/media/{}/%E7%8E%B0%E5%9C%BA%20%E7%85%A7%E7%89%87.jpg", id);

        // The whole point of the trailing name: a client that only has this address
        // can tell a photo from a film, and the bytes are the same file.
        let (head, body) = hub.get(&named, None).await;
        assert_eq!(status_of(&head), 200, "{head}");
        assert_eq!(body, payload, "the named address is the same file");

        let (head, body) = hub.get(&named, Some("bytes=2000-2099")).await;
        assert_eq!(status_of(&head), 206, "{head}");
        assert_eq!(body, &payload[2000..2100], "ranges work both ways");

        // The name is decoration: a lie about it cannot read a different file,
        // because only the id is looked up.
        let (head, body) = hub.get(&format!("/media/{id}/index.html"), None).await;
        assert_eq!(status_of(&head), 200, "{head}");
        assert_eq!(body, payload, "a wrong name still serves the id's bytes");

        // And the extensionless address still answers, for whatever a controller
        // was given before this shape existed.
        let (head, _) = hub.get(&format!("/media/{id}"), None).await;
        assert_eq!(status_of(&head), 200, "{head}");
    }

    #[tokio::test]
    async fn head_reports_the_length_without_sending_bytes() {
        let hub = Hub::start("head").await;
        let id = hub.store("clip.mp4", &pattern(4096));
        let (head, body) = hub.request("HEAD", &format!("/media/{id}")).await;
        assert_eq!(status_of(&head), 200, "{head}");
        assert_eq!(header_of(&head, "Content-Length").as_deref(), Some("4096"));
        assert!(body.is_empty(), "a HEAD response carries no bytes");
    }

    #[tokio::test]
    async fn an_unsatisfiable_range_answers_with_the_real_length() {
        let hub = Hub::start("416").await;
        let id = hub.store("clip.mp4", &pattern(1000));
        let (head, _) = hub.get(&format!("/media/{id}"), Some("bytes=5000-6000")).await;
        assert_eq!(status_of(&head), 416, "{head}");
        assert_eq!(header_of(&head, "Content-Range").as_deref(), Some("bytes */1000"));
    }

    #[tokio::test]
    async fn an_id_that_is_not_in_the_library_reads_nothing() {
        let hub = Hub::start("404").await;
        let id = hub.store("clip.mp4", &pattern(1000));
        let (head, _) = hub.get(&format!("/media/{id}"), None).await;
        assert_eq!(status_of(&head), 200);
        // The other id cannot reach the file that is there: ids are the only
        // naming a client gets, and a name it invents resolves to nothing.
        let (head, _) = hub.get("/media/deadbeef0000", None).await;
        assert_eq!(status_of(&head), 404, "{head}");
        let (head, _) = hub.get("/media/..%2f..%2findex.html", None).await;
        assert_eq!(status_of(&head), 404, "no path reaches the disk through this route");
    }

    #[tokio::test]
    async fn a_upload_beyond_the_default_body_limit_lands_in_the_library() {
        let hub = Hub::start("upload").await;
        // axum's default limit is 2 MiB; three mebibytes is the point of the test.
        let payload = pattern(3 * 1024 * 1024);
        let body = multipart("会议记录.mp4", &payload);
        let head = format!(
            "POST /api/media/upload HTTP/1.1\r\n\
             Host: hub\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary=hubboundary\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        let (response, _) = hub.send(head, &body).await;
        assert_eq!(status_of(&response), 200, "{response}");

        let items = hub.library.items();
        assert_eq!(items.len(), 1, "the upload became one library entry");
        assert_eq!(items[0].title, "会议记录.mp4");
        assert_eq!(items[0].bytes, payload.len() as u64);

        // Read it back through the same endpoint a player would use, so the test
        // covers the write path and the read path agreeing on the bytes.
        let (head, served) = hub.get(&format!("/media/{}", items[0].id), Some("bytes=2000000-")).await;
        assert_eq!(status_of(&head), 206);
        assert_eq!(served, &payload[2000000..]);
    }

    #[tokio::test]
    async fn the_upload_route_answers_a_preflight() {
        let hub = Hub::start("upload-preflight").await;
        // The extension popup posts multipart from its own origin, so the browser
        // asks before sending; an unanswered OPTIONS looks to it like a dead hub.
        let (head, _) = hub.request("OPTIONS", "/api/media/upload").await;
        assert_eq!(status_of(&head), 204, "{head}");
        assert_eq!(
            header_of(&head, "Access-Control-Allow-Origin").as_deref(),
            Some("*")
        );
        assert!(header_of(&head, "Access-Control-Allow-Methods")
            .unwrap_or_default()
            .contains("POST"));
    }

    #[tokio::test]
    async fn the_tail_of_a_half_gigabyte_file_arrives_without_the_head() {
        let hub = Hub::start("big").await;
        let length: u64 = 500 * 1024 * 1024;
        let file = std::fs::File::create(hub.library.root().join("big.mp4")).expect("create");
        file.set_len(length).expect("extend");
        drop(file);
        let id = hub
            .library
            .add_file("big.mp4".to_string(), "big.mp4", length, "video/mp4")
            .id;

        let started = Instant::now();
        let (head, body) = hub
            .get(&format!("/media/{id}"), Some("bytes=-1024"))
            .await;
        let elapsed = started.elapsed();
        assert_eq!(status_of(&head), 206, "{head}");
        assert_eq!(
            header_of(&head, "Content-Range").as_deref(),
            Some("bytes 524286976-524287999/524288000")
        );
        assert_eq!(body.len(), 1024);
        assert!(
            elapsed < Duration::from_secs(5),
            "a seek to the tail must not read the first {length} bytes ({elapsed:?})"
        );
    }

    #[tokio::test]
    async fn the_transfer_page_upload_still_lands_on_disk() {
        let hub = Hub::start("transfer").await;
        let payload = pattern(3 * 1024 * 1024);
        let body = multipart("报价单.pdf", &payload);
        let head = format!(
            "POST /upload HTTP/1.1\r\n\
             Host: hub\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary=hubboundary\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        let (response, _) = hub.send(head, &body).await;
        assert_eq!(status_of(&response), 200, "{response}");
        let written = std::fs::read(hub.storage.join("报价单.pdf")).expect("stored file");
        assert_eq!(written, payload);
    }
}
