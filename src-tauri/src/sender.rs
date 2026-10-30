//! Google Cast sender session owned by the 投屏 panel.
//!
//! The protocol lives in `horizon_cast::cast`; this module owns the one
//! long-running session the panel can start, control and stop. The control
//! channel is a single blocking TLS socket, so the worker thread is its only
//! owner: panel actions are messages to that thread, not a second client.
//!
//! The worker reads on a sub-second cadence. That keeps heartbeats answered and
//! applies a pause/stop within about a second, while the patient waits the
//! protocol needs (handshake, receiver app launch, media load) stay bounded by
//! wall-clock deadlines inside the client.
//!
//! What this sender does not do is hide the transport's limits: Cast carries a
//! URL that the television fetches itself, so the address must be reachable
//! from the TV, and only the Default Media Receiver is launched (a custom
//! receiver needs a Google Cast developer registration this project does not
//! have). Screen mirroring is not part of the protocol here.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{self, Sender as CommandSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use horizon_cast::cast::{self, Client, Event, MediaStatus, DEFAULT_MEDIA_RECEIVER};
use horizon_cast::CastError;
use serde::Serialize;

pub const CAST_PORT: u16 = 8009;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(900);

// ------------------------------------------------------------------ state

#[derive(Default)]
pub struct Sender {
    session: Mutex<Option<Session>>,
}

struct Session {
    commands: CommandSender<Command>,
    status: Arc<Mutex<CastSenderStatus>>,
    thread: thread::JoinHandle<()>,
}

impl Session {
    fn snapshot(&self) -> CastSenderStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }
}

enum Command {
    Play,
    Pause,
    Stop,
}

impl Sender {
    pub fn send(
        &self,
        device: String,
        host: String,
        url: String,
    ) -> Result<CastSenderStatus, String> {
        let url = url.trim().to_string();
        validate_media_url(&url)?;
        let (socket, server_name) = resolve(&host)?;
        let content_type = guess_content_type(&url);
        let device = device.trim().to_string();
        let device = if device.is_empty() {
            server_name.clone()
        } else {
            device
        };

        self.stop();

        let worker_content_type = content_type.clone();
        let status = Arc::new(Mutex::new(CastSenderStatus {
            device,
            host: socket.to_string(),
            url,
            content_type,
            phase: "connecting".into(),
            position: 0.0,
            duration: 0.0,
            volume: None,
            observed_at: now_ms(),
            error: None,
        }));
        let worker_status = Arc::clone(&status);
        let (commands, queue) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("cast-sender".into())
            .spawn(move || run(worker_status, queue, socket, server_name, worker_content_type))
            .map_err(|error| format!("无法启动投屏发送线程：{error}"))?;

        let snapshot = status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| "投屏发送状态不可用".to_string())?;
        *self.lock()? = Some(Session {
            commands,
            status,
            thread,
        });
        Ok(snapshot)
    }

    /// Stops the running session and forgets it. The worker is told to close
    /// the receiver app and the thread ends on its own; nothing here waits on
    /// the device, because a television that stopped answering must not hold
    /// the panel.
    pub fn stop(&self) {
        let session = self.session.lock().ok().and_then(|mut slot| slot.take());
        if let Some(session) = session {
            let _ = session.commands.send(Command::Stop);
        }
    }

    /// Process exit: ask the worker to close the receiver app and give it a
    /// short grace period. Without the STOP the television would keep playing
    /// after the launcher is gone; waiting longer than this is not worth
    /// delaying exit for a device that has stopped answering.
    pub fn shutdown(&self) {
        let session = self.session.lock().ok().and_then(|mut slot| slot.take());
        let Some(session) = session else {
            return;
        };
        let _ = session.commands.send(Command::Stop);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !session.thread.is_finished() {
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn control(&self, action: &str) -> Result<CastSenderStatus, String> {
        if action == "stop" {
            let mut snapshot = self
                .session
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(Session::snapshot))
                .unwrap_or_default();
            self.stop();
            snapshot.phase = "stopped".into();
            return Ok(snapshot);
        }

        let command = match action {
            "play" => Command::Play,
            "pause" => Command::Pause,
            other => return Err(format!("未知的投屏操作：{other}")),
        };
        let slot = self
            .session
            .lock()
            .map_err(|_| "投屏发送状态不可用".to_string())?;
        let Some(session) = slot.as_ref() else {
            return Err("当前没有进行中的投屏发送".into());
        };
        session
            .commands
            .send(command)
            .map_err(|_| "投屏发送会话已经结束".to_string())?;
        Ok(session.snapshot())
    }

    pub fn status(&self) -> Option<CastSenderStatus> {
        self.session
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Session::snapshot))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Session>>, String> {
        self.session
            .lock()
            .map_err(|_| "投屏发送状态不可用".to_string())
    }
}

// --------------------------------------------------------------- worker

fn run(
    shared: Arc<Mutex<CastSenderStatus>>,
    commands: mpsc::Receiver<Command>,
    socket: SocketAddr,
    server_name: String,
    content_type: String,
) {
    let stream = match cast::tls::TlsStream::connect(socket, &server_name, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(error) => return fail(&shared, format!("无法连接电视的 Cast 通道：{}", explain(error))),
    };
    if let Err(error) = stream.set_read_timeout(POLL_INTERVAL) {
        return fail(&shared, format!("无法配置 Cast 通道：{}", explain(error)));
    }

    let mut client = Client::new(stream, "sender-0");
    if let Err(error) = client.connect() {
        return fail(&shared, format!("无法打开 Cast 控制通道：{}", explain(error)));
    }

    set_phase(&shared, "launching");
    if let Err(error) = client.launch(DEFAULT_MEDIA_RECEIVER) {
        return fail(&shared, format!("电视没有启动播放器：{}", explain(error)));
    }

    set_phase(&shared, "loading");
    let url = match shared.lock() {
        Ok(status) => status.url.clone(),
        Err(_) => return,
    };
    match client.load(&url, &content_type) {
        Ok(media) => apply_media(&shared, media),
        Err(error) => return fail(&shared, format!("电视无法加载这个地址：{}", explain(error))),
    }

    loop {
        match commands.try_recv() {
            Ok(Command::Stop) | Err(TryRecvError::Disconnected) => {
                let _ = client.stop();
                client.close();
                set_phase(&shared, "stopped");
                return;
            }
            Ok(Command::Play) => match client.play() {
                Ok(media) => apply_media(&shared, media),
                Err(error) => return fail(&shared, format!("继续播放失败：{}", explain(error))),
            },
            Ok(Command::Pause) => match client.pause() {
                Ok(media) => apply_media(&shared, media),
                Err(error) => return fail(&shared, format!("暂停失败：{}", explain(error))),
            },
            Err(TryRecvError::Empty) => {}
        }

        match client.poll() {
            Ok(Some(Event::Media(media))) => apply_media(&shared, media),
            Ok(Some(Event::Receiver { volume, .. })) => {
                if let (Some(volume), Ok(mut status)) = (volume, shared.lock()) {
                    status.volume = Some(volume);
                }
            }
            Ok(Some(Event::Closed)) => {
                set_phase(&shared, "stopped");
                return;
            }
            // A read timeout is the loop's tick, not a failure.
            Ok(None) | Err(CastError::Timeout) => {}
            Err(error) => {
                return fail(&shared, format!("投屏连接中断：{}", explain(error)));
            }
        }
    }
}

fn apply_media(shared: &Arc<Mutex<CastSenderStatus>>, media: MediaStatus) {
    if let Ok(mut status) = shared.lock() {
        status.phase = media.player_state.to_ascii_lowercase();
        status.position = media.current_time;
        status.duration = media.duration;
        status.observed_at = now_ms();
        status.error = None;
    }
}

fn set_phase(shared: &Arc<Mutex<CastSenderStatus>>, phase: &str) {
    if let Ok(mut status) = shared.lock() {
        status.phase = phase.to_string();
        status.observed_at = now_ms();
    }
}

fn fail(shared: &Arc<Mutex<CastSenderStatus>>, message: String) {
    if let Ok(mut status) = shared.lock() {
        status.phase = "error".into();
        status.error = Some(message);
        status.observed_at = now_ms();
    }
}

fn explain(error: CastError) -> String {
    match error {
        CastError::Timeout => "电视没有及时响应".into(),
        CastError::Io => "与电视的连接中断".into(),
        CastError::InvalidState(reason) => format!("电视拒绝了请求（{reason}）"),
        CastError::Hresult(code) => format!("加密通道握手失败（{}）", CastError::hresult_hex(code)),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------- helpers

/// `ip`, `ip:port`, or a host name; the control port defaults to 8009.
fn resolve(host: &str) -> Result<(SocketAddr, String), String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err("请先选择或输入电视地址".into());
    }
    let has_port = trimmed
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    let with_port = if has_port {
        trimmed.to_string()
    } else {
        format!("{trimmed}:{CAST_PORT}")
    };
    let server_name = with_port
        .rsplit_once(':')
        .map(|(name, _)| name.trim_matches(['[', ']']).to_string())
        .unwrap_or_else(|| trimmed.to_string());
    let socket = with_port
        .to_socket_addrs()
        .map_err(|error| format!("无法解析地址 {trimmed}：{error}"))?
        .next()
        .ok_or_else(|| format!("地址 {trimmed} 没有可用结果"))?;
    Ok((socket, server_name))
}

fn validate_media_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("只能投放 http(s) 媒体地址，电视需要能直接访问它".into());
    }
    if lower.contains(char::is_whitespace) {
        return Err("媒体地址中不能包含空格".into());
    }
    if url.len() > 2048 {
        return Err("媒体地址过长".into());
    }
    Ok(())
}

/// Default Media Receiver picks its player from the MIME type; the extension
/// is the only hint a URL carries, and a wrong guess is reported by the device
/// on load rather than silently playing nothing.
fn guess_content_type(url: &str) -> String {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("");
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "ts" | "m2ts" => "video/mp2t",
        "m3u8" => "application/x-mpegurl",
        "mp3" => "audio/mpeg",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "video/mp4",
    }
    .to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- serialized

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CastSenderStatus {
    pub device: String,
    /// `ip:port` actually used for the control channel.
    pub host: String,
    pub url: String,
    pub content_type: String,
    /// `connecting`, `launching`, `loading`, a media player state (`playing`,
    /// `paused`, `buffering`, …), or a terminal `stopped` / `error`.
    pub phase: String,
    pub position: f64,
    pub duration: f64,
    pub volume: Option<f64>,
    /// Epoch milliseconds when `position` was read, so the panel can keep the
    /// clock moving between status pushes.
    pub observed_at: u64,
    pub error: Option<String>,
}

impl Default for CastSenderStatus {
    fn default() -> Self {
        Self {
            device: String::new(),
            host: String::new(),
            url: String::new(),
            content_type: String::new(),
            phase: "idle".into(),
            position: 0.0,
            duration: 0.0,
            volume: None,
            observed_at: now_ms(),
            error: None,
        }
    }
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn cast_send(
    sender: tauri::State<'_, Sender>,
    device: String,
    host: String,
    url: String,
) -> Result<CastSenderStatus, String> {
    sender.send(device, host, url)
}

#[tauri::command]
pub async fn cast_control(
    sender: tauri::State<'_, Sender>,
    action: String,
) -> Result<CastSenderStatus, String> {
    sender.control(&action)
}

#[tauri::command]
pub async fn cast_status(sender: tauri::State<'_, Sender>) -> Result<Option<CastSenderStatus>, String> {
    Ok(sender.status())
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_port_is_assumed_only_when_none_was_given() {
        let (socket, name) = resolve("192.168.0.23").unwrap();
        assert_eq!(socket.port(), CAST_PORT);
        assert_eq!(name, "192.168.0.23");

        let (socket, _) = resolve("127.0.0.1:9009").unwrap();
        assert_eq!(socket.to_string(), "127.0.0.1:9009");

        assert!(resolve("   ").is_err());
    }

    #[test]
    fn the_mime_type_follows_the_extension_not_the_query_string() {
        assert_eq!(guess_content_type("http://a/b.mp4"), "video/mp4");
        assert_eq!(
            guess_content_type("https://a/b.M3U8?token=1#t=3"),
            "application/x-mpegurl"
        );
        assert_eq!(guess_content_type("http://a/b.mp3?x=.mp4"), "audio/mpeg");
    }

    #[test]
    fn only_media_urls_the_tv_can_fetch_are_accepted() {
        assert!(validate_media_url("http://10.0.0.5/a.mp4").is_ok());
        assert!(validate_media_url("https://example.com/a.m3u8?x=1").is_ok());
        assert!(validate_media_url("file:///C:/a.mp4").is_err());
        assert!(validate_media_url("http://a/b c.mp4").is_err());
        assert!(validate_media_url("").is_err());
    }
}
