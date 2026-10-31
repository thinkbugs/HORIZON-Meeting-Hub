//! Browser-sender mirroring over WebSocket.
//!
//! A Chrome or Edge extension captures a tab or screen, encodes H.264 with
//! WebCodecs and streams it here. The wire format is deliberately the same
//! 24-byte header the desktop shell already uses for AirPlay frames, so both
//! senders feed one preview pipeline:
//!
//! ```text
//! 0 u8 kind (1 H.264 Annex B, 2 PCM s16le)
//! 1 u8 flags (bit 0: keyframe, bit 1: pts is sender wall clock)
//! 2 u16 reserved
//! 4 u64 pts_us (video) / zero (audio)
//! 12 u16 width (video) / sample rate / 100 (audio)
//! 14 u16 height (video) / channels (audio)
//! 16 u32 payload length
//! 20 4 bytes reserved
//! 24 payload
//! ```
//!
//! The channel is a plain RFC 6455 WebSocket on `ws://<host>:9407/stream`. A
//! text `hello` opens the stream, binary messages carry media, and the socket
//! closing ends the session. The URL may carry `?token=...`; when the receiver
//! was given a token, every other connection is refused.
//!
//! The same port also answers `GET /info` with a small CORS-enabled JSON
//! description, which is how an extension finds receivers by scanning its own
//! subnet. That reply names the receiver but never reveals its token.
//!
//! No third-party WebSocket crate is involved: the handshake, frame parser and
//! pong replies are here, which keeps the receiver buildable offline and the
//! protocol debuggable from the log.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use crate::airplay::mirror::{FrameSink, VideoFrame};
use crate::airplay::session::{AudioSink, WaveOutSink};
use crate::cast_recv::sha1;
use crate::error::{CastError, CastResult};

/// The port the extension connects to.
pub const BROWSER_MIRROR_PORT: u16 = 9407;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const HEADER_LENGTH: usize = 24;
const KIND_VIDEO: u8 = 1;
const KIND_AUDIO: u8 = 2;
/// A 4K keyframe is well under this; anything larger is a broken sender.
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const MAX_REQUEST: usize = 16 * 1024;
const READ_POLL: Duration = Duration::from_millis(500);
/// Sent to the sender when a preview needs an IDR to resynchronise.
const KEYFRAME_REQUEST: &[u8] = br#"{"type":"keyframe"}"#;

#[derive(Debug, Clone)]
pub struct BrowserMirrorConfig {
    pub address: Ipv4Addr,
    pub port: u16,
    pub name: String,
    /// When set, the WebSocket URL must carry the same `?token=`.
    pub token: Option<String>,
    /// The port the file-transfer hub and media library are served on, when they
    /// are up.
    ///
    /// `GET /info` is the one address a sender already has, so it is the only
    /// honest place to publish the rest of the room's ports from: a sender that
    /// guessed 7878 would be guessing at a port the operator can change. Left
    /// `None` when the hub never bound, and the reply then simply has no `hub`
    /// field — the sender says 「该接收端没有开启片库服务」 rather than probing a
    /// port that may belong to something else.
    pub hub_port: Option<u16>,
}

impl Default for BrowserMirrorConfig {
    fn default() -> Self {
        Self {
            address: Ipv4Addr::UNSPECIFIED,
            port: BROWSER_MIRROR_PORT,
            name: "HORIZON".to_string(),
            token: None,
            hub_port: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct BrowserMirrorStats {
    connected: AtomicBool,
    active_connection: AtomicU64,
    next_connection: AtomicU64,
    frames: AtomicU64,
    keyframes: AtomicU64,
    video_bytes: AtomicU64,
    audio_bytes: AtomicU64,
    width: AtomicU64,
    height: AtomicU64,
    peer: Mutex<Option<String>>,
}

/// What the launcher shows about the browser mirror channel.
#[derive(Debug, Clone, Default)]
pub struct BrowserMirrorStatus {
    pub listening: bool,
    pub port: u16,
    pub connected: bool,
    pub peer: Option<String>,
    pub frames: u64,
    pub keyframes: u64,
    pub video_bytes: u64,
    pub audio_bytes: u64,
    pub width: u32,
    pub height: u32,
}

impl BrowserMirrorStats {
    fn snapshot(&self, port: u16, listening: bool) -> BrowserMirrorStatus {
        BrowserMirrorStatus {
            listening,
            port,
            connected: self.connected.load(Ordering::Relaxed),
            peer: self.peer.lock().ok().and_then(|peer| peer.clone()),
            frames: self.frames.load(Ordering::Relaxed),
            keyframes: self.keyframes.load(Ordering::Relaxed),
            video_bytes: self.video_bytes.load(Ordering::Relaxed),
            audio_bytes: self.audio_bytes.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed) as u32,
            height: self.height.load(Ordering::Relaxed) as u32,
        }
    }
}

/// The WebSocket channel the extension streams to.
pub struct BrowserMirrorReceiver {
    stop: Arc<AtomicBool>,
    stats: Arc<BrowserMirrorStats>,
    active: Arc<Mutex<Option<TcpStream>>>,
    keyframe: Arc<AtomicBool>,
    port: u16,
    thread: Option<JoinHandle<()>>,
}

impl BrowserMirrorReceiver {
    pub fn start(config: BrowserMirrorConfig, sink: Arc<dyn FrameSink>) -> CastResult<Self> {
        let addr = SocketAddr::from((config.address, config.port));
        let listener = TcpListener::bind(addr).map_err(|_| CastError::Io)?;
        listener.set_nonblocking(true).map_err(|_| CastError::Io)?;
        let port = listener.local_addr().map_err(|_| CastError::Io)?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(BrowserMirrorStats::default());
        let active = Arc::new(Mutex::new(None));
        let keyframe = Arc::new(AtomicBool::new(false));
        if let Ok(mut peer) = stats.peer.lock() {
            *peer = None;
        }

        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        let thread_active = Arc::clone(&active);
        let thread_keyframe = Arc::clone(&keyframe);
        let thread = thread::Builder::new()
            .name("browser-mirror".to_string())
            .spawn(move || {
                accept_loop(
                    listener,
                    config,
                    thread_stop,
                    thread_stats,
                    thread_active,
                    thread_keyframe,
                    sink,
                )
            })
            .map_err(|_| CastError::Io)?;

        Ok(Self {
            stop,
            stats,
            active,
            keyframe,
            port,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Asks the connected sender for an IDR. The session thread delivers the
    /// request on its next loop turn, so writes never interleave.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn status(&self) -> BrowserMirrorStatus {
        self.stats
            .snapshot(self.port, !self.stop.load(Ordering::Relaxed))
    }

    /// Stops listening and drops the active sender. Safe to call twice.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        close_active(&self.active);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BrowserMirrorReceiver {
    fn drop(&mut self) {
        // `stop` normally consumed the receiver; this covers a panic path.
        self.stop.store(true, Ordering::Relaxed);
        close_active(&self.active);
    }
}

fn close_active(active: &Mutex<Option<TcpStream>>) {
    let stream = active.lock().ok().and_then(|mut slot| slot.take());
    if let Some(stream) = stream {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

fn accept_loop(
    listener: TcpListener,
    config: BrowserMirrorConfig,
    stop: Arc<AtomicBool>,
    stats: Arc<BrowserMirrorStats>,
    active: Arc<Mutex<Option<TcpStream>>>,
    keyframe: Arc<AtomicBool>,
    sink: Arc<dyn FrameSink>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // Winsock gives a non-blocking listener's children the same
                // mode; the session thread wants blocking reads with a timeout.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);

                let session_stats = Arc::clone(&stats);
                let session_sink = Arc::clone(&sink);
                let session_config = config.clone();
                let session_keyframe = Arc::clone(&keyframe);
                let session_active = Arc::clone(&active);
                thread::Builder::new()
                    .name("browser-mirror-session".to_string())
                    .spawn(move || {
                        session(
                            stream,
                            peer,
                            session_config,
                            session_stats,
                            session_keyframe,
                            session_sink,
                            session_active,
                        )
                    })
                    .ok();
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn session(
    mut stream: TcpStream,
    peer: SocketAddr,
    config: BrowserMirrorConfig,
    stats: Arc<BrowserMirrorStats>,
    pending_keyframe: Arc<AtomicBool>,
    sink: Arc<dyn FrameSink>,
    active: Arc<Mutex<Option<TcpStream>>>,
) {
    let id = stats.next_connection.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let handshake = match read_request(&mut stream) {
        Ok(Some((text, leftover))) => match http_reply(&text, &config) {
            FirstResponse::Upgrade(reply) => Some((reply.into_bytes(), leftover)),
            FirstResponse::Close(reply) => {
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        },
        Ok(None) => None,
        Err(_) => None,
    };
    let Some((reply, leftover)) = handshake else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    // Getting here means the sender passed the token check, and only a real screen
    // may take the room over. The eviction used to happen on `accept()`, before any
    // request was read: the extension's own discovery sweep holds 48 connections
    // open at once, so a LAN scan of `/info` shut down the live mirror between the
    // connect and the handshake. Measured on this machine — 48 parallel `/info`
    // probes answered 1 and killed 47 with a network error.
    if let Ok(clone) = stream.try_clone() {
        if let Ok(mut slot) = active.lock() {
            if let Some(old) = slot.replace(clone) {
                let _ = old.shutdown(Shutdown::Both);
            }
        }
    }
    if stream.write_all(&reply).is_err() || stream.flush().is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(READ_POLL));

    let reader_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let mut reader = WsReader::new(reader_stream, leftover);
    let audio = PcmPlayer::spawn();

    stats.active_connection.store(id, Ordering::Relaxed);
    stats.connected.store(true, Ordering::Relaxed);
    if let Ok(mut slot) = stats.peer.lock() {
        *slot = Some(peer.to_string());
    }

    loop {
        // A preview that fell behind asks for a fresh IDR; the request goes out
        // on this thread so it can never interleave with a pong or close frame.
        if pending_keyframe.swap(false, Ordering::Relaxed) {
            let _ = write_frame(&mut stream, 0x01, KEYFRAME_REQUEST);
        }
        match reader.next() {
            Ok(Some(WsMessage::Binary(body))) => {
                if let Some(packet) = parse_media(&body) {
                    match packet {
                        MediaPacket::Video {
                            keyframe,
                            wallclock,
                            pts_us,
                            width,
                            height,
                            data,
                        } => {
                            stats.frames.fetch_add(1, Ordering::Relaxed);
                            stats.video_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                            if keyframe {
                                stats.keyframes.fetch_add(1, Ordering::Relaxed);
                            }
                            stats.width.store(width as u64, Ordering::Relaxed);
                            stats.height.store(height as u64, Ordering::Relaxed);
                            sink.on_frame(VideoFrame {
                                data: data.to_vec(),
                                pts_us,
                                width: width as u32,
                                height: height as u32,
                                keyframe,
                                wallclock,
                            });
                        }
                        MediaPacket::Audio {
                            rate,
                            channels,
                            data,
                        } => {
                            stats.audio_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                            audio.push(rate, channels, data);
                        }
                    }
                }
            }
            Ok(Some(WsMessage::Text(text))) => {
                if handle_text(&text, &mut stream, &config) {
                    break;
                }
            }
            Ok(Some(WsMessage::Ping(payload))) => {
                let _ = write_frame(&mut stream, 0x0a, &payload);
            }
            Ok(Some(WsMessage::Pong)) => break,
            Ok(Some(WsMessage::Close)) => break,
            Ok(None) => break,
            Err(CastError::Timeout) => continue,
            Err(_) => break,
        }
    }

    let _ = write_frame(&mut stream, 0x08, &[0x03, 0xe8]);
    // Half-close and drain the peer's answer. Closing a socket that still has
    // an unread close frame turns the teardown into a reset, which browsers
    // report as an abnormal 1006 instead of a normal stop.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let mut discard = [0u8; 1024];
    while matches!(stream.read(&mut discard), Ok(read) if read > 0) {}
    let _ = stream.shutdown(Shutdown::Both);
    drop(audio);

    if stats.active_connection.load(Ordering::Relaxed) == id {
        stats.connected.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = stats.peer.lock() {
            *slot = None;
        }
    }
}

/// `Ok(None)` when the peer left before finishing the request.
fn read_request(stream: &mut TcpStream) -> CastResult<Option<(String, Vec<u8>)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).map_err(|_| CastError::Io)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&buffer[..end]).into_owned();
            let leftover = buffer[end + 4..].to_vec();
            return Ok(Some((text, leftover)));
        }
        if buffer.len() > MAX_REQUEST {
            return Err(CastError::Malformed("websocket request too large"));
        }
    }
}

/// Either the 101 response, or a complete refusal response.
fn handshake_reply(request: &str, token: Option<&str>) -> Result<String, String> {
    let mut lines = request.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    let reject = |status: &str| {
        format!("HTTP/1.1 {status}\r\n{CORS}Connection: close\r\nContent-Length: 0\r\n\r\n")
    };

    if method != "GET" || !version.starts_with("HTTP/1.") {
        return Err(reject("405 Method Not Allowed"));
    }
    if !target.starts_with("/stream") {
        return Err(reject("404 Not Found"));
    }
    if let Some(expected) = token {
        if query_value(target, "token") != Some(expected) {
            return Err(reject("403 Forbidden"));
        }
    }

    let mut key = None;
    let mut upgrade = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("sec-websocket-key") {
            key = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("upgrade") && value.eq_ignore_ascii_case("websocket") {
            upgrade = true;
        }
    }
    if !upgrade {
        return Err(reject("400 Bad Request"));
    }
    let Some(key) = key else {
        return Err(reject("400 Bad Request"));
    };
    if base64_decode_len(&key) != Some(16) {
        return Err(reject("400 Bad Request"));
    }

    Ok(format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    ))
}

/// The first response on a connection. `Close` is a complete HTTP reply: the
/// launcher's `GET /info` discovery probe takes this branch and the socket is
/// finished with, so the WebSocket session loop never starts.
enum FirstResponse {
    Upgrade(String),
    Close(String),
}

/// `Access-Control-Allow-Origin` is what lets the extension read `/info` from a
/// `chrome-extension://` page. A LAN scan has no other way to find us: an
/// extension cannot send a UDP multicast, and `/info` never carries the code.
const CORS: &str = "Access-Control-Allow-Origin: *\r\n";
/// Current Chrome counts a scan from an extension page as local-network access,
/// so it preflights the request; an unanswered preflight means `/info` is never
/// read and the receiver stays invisible.
const CORS_PREFLIGHT: &str = "HTTP/1.1 204 No Content\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
         Access-Control-Allow-Private-Network: true\r\n\
         Access-Control-Max-Age: 600\r\n\
         Connection: close\r\n\r\n";

fn info_reply(config: &BrowserMirrorConfig) -> String {
    let mut body = json!({
        "service": "horizon-browser",
        "name": config.name,
        "port": config.port,
        "path": "/stream",
        // Whether the sender has to type the code shown on the big screen.
        "paired": config.token.is_some(),
    });
    if let Some(hub) = config.hub_port {
        body["hub"] = serde_json::Value::from(hub);
    }
    // The length header is in bytes, and the device name is free text a room may
    // have set to Chinese: `body.len()` would count UTF-16 code units in JS terms
    // and under-report here, truncating the reply at the boundary.
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Cache-Control: no-store\r\n\
         {CORS}\
         Access-Control-Allow-Private-Network: true\r\n\
         Connection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn http_reply(request: &str, config: &BrowserMirrorConfig) -> FirstResponse {
    let start = request.split("\r\n").next().unwrap_or("");
    let mut words = start.split_whitespace();
    if words.next().unwrap_or("") == "OPTIONS" {
        return FirstResponse::Close(CORS_PREFLIGHT.to_string());
    }
    let target = words.next().unwrap_or("").to_string();
    if target.starts_with("/info") || target == "/" || target.is_empty() {
        return FirstResponse::Close(info_reply(config));
    }
    match handshake_reply(request, config.token.as_deref()) {
        Ok(reply) => FirstResponse::Upgrade(reply),
        Err(refusal) => FirstResponse::Close(refusal),
    }
}

fn query_value<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// RFC 6455 section 4.2.2: SHA-1 of the key and the protocol GUID, base64.
fn accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + WS_GUID.len());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    base64_encode(&sha1(&input))
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode_len(text: &str) -> Option<usize> {
    if text.len() % 4 != 0 {
        return None;
    }
    let padding = text.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return None;
    }
    Some(text.len() / 4 * 3 - padding)
}

fn handle_text(text: &str, stream: &mut TcpStream, config: &BrowserMirrorConfig) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("hello") => {
            let reply = json!({
                "type": "welcome",
                "receiver": config.name,
                "version": 1,
            });
            let _ = write_frame(stream, 0x01, reply.to_string().as_bytes());
            false
        }
        Some("stop") => true,
        _ => false,
    }
}

fn write_frame<W: Write>(out: &mut W, opcode: u8, payload: &[u8]) -> CastResult<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    match payload.len() {
        len if len < 126 => frame.push(len as u8),
        len if len <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    out.write_all(&frame).map_err(|_| CastError::Io)?;
    out.flush().map_err(|_| CastError::Io)
}

#[derive(Debug, PartialEq)]
enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// One inbound WebSocket frame. The reader owns a byte buffer so a timeout can
/// interrupt between reads without losing a partially received frame.
struct WsReader<R: Read> {
    input: R,
    buffer: Vec<u8>,
    fragment: Option<(bool, Vec<u8>)>,
}

impl<R: Read> WsReader<R> {
    fn new(input: R, leftover: Vec<u8>) -> Self {
        Self {
            input,
            buffer: leftover,
            fragment: None,
        }
    }

    fn next(&mut self) -> CastResult<Option<WsMessage>> {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if self.buffer.len() >= 2 {
                if let Some(message) = self.take()? {
                    return Ok(Some(message));
                }
            }
            match self.input.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err(CastError::Timeout);
                }
                Err(_) => return Err(CastError::Io),
            }
        }
    }

    fn take(&mut self) -> CastResult<Option<WsMessage>> {
        let first = self.buffer[0];
        let second = self.buffer[1];
        if first & 0x70 != 0 {
            return Err(CastError::Malformed("websocket reserved bits"));
        }
        let fin = first & 0x80 != 0;
        let opcode = first & 0x0f;
        if second & 0x80 == 0 {
            return Err(CastError::Malformed("client websocket frame is not masked"));
        }
        let short = (second & 0x7f) as usize;
        let (length, mut at) = match short {
            126 => {
                if self.buffer.len() < 4 {
                    return Ok(None);
                }
                (
                    u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize,
                    4,
                )
            }
            127 => {
                if self.buffer.len() < 10 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&self.buffer[2..10]);
                (u64::from_be_bytes(raw) as usize, 10)
            }
            length => (length, 2),
        };
        if length > MAX_PAYLOAD {
            return Err(CastError::Malformed("websocket payload too large"));
        }
        if opcode >= 0x08 && (!fin || length > 125) {
            return Err(CastError::Malformed("fragmented control frame"));
        }
        let mask_at = at;
        at += 4;
        if self.buffer.len() < at + length {
            return Ok(None);
        }
        let mut payload = self.buffer[at..at + length].to_vec();
        let mask = [
            self.buffer[mask_at],
            self.buffer[mask_at + 1],
            self.buffer[mask_at + 2],
            self.buffer[mask_at + 3],
        ];
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index & 3];
        }
        self.buffer.drain(..at + length);

        match opcode {
            0x00 => {
                let Some((text, mut accumulated)) = self.fragment.take() else {
                    return Err(CastError::Malformed("unexpected continuation"));
                };
                accumulated.extend_from_slice(&payload);
                if fin {
                    Ok(Some(finish_message(text, accumulated)?))
                } else {
                    self.fragment = Some((text, accumulated));
                    Ok(None)
                }
            }
            0x01 | 0x02 => {
                if fin {
                    Ok(Some(finish_message(opcode == 0x01, payload)?))
                } else {
                    self.fragment = Some((opcode == 0x01, payload));
                    Ok(None)
                }
            }
            0x08 => Ok(Some(WsMessage::Close)),
            0x09 => Ok(Some(WsMessage::Ping(payload))),
            0x0a => Ok(Some(WsMessage::Pong)),
            _ => Err(CastError::Malformed("unknown websocket opcode")),
        }
    }
}

fn finish_message(text: bool, payload: Vec<u8>) -> CastResult<WsMessage> {
    if text {
        String::from_utf8(payload)
            .map(WsMessage::Text)
            .map_err(|_| CastError::Malformed("websocket text is not utf-8"))
    } else {
        Ok(WsMessage::Binary(payload))
    }
}

enum MediaPacket<'a> {
    Video {
        keyframe: bool,
        wallclock: bool,
        pts_us: u64,
        width: u16,
        height: u16,
        data: &'a [u8],
    },
    Audio {
        rate: u16,
        channels: u16,
        data: &'a [u8],
    },
}

fn parse_media(body: &[u8]) -> Option<MediaPacket<'_>> {
    if body.len() < HEADER_LENGTH {
        return None;
    }
    let kind = body[0];
    let flags = body[1];
    let pts_us = u64::from_le_bytes([
        body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
    ]);
    let first = u16::from_le_bytes([body[12], body[13]]);
    let second = u16::from_le_bytes([body[14], body[15]]);
    let declared = u32::from_le_bytes([body[16], body[17], body[18], body[19]]) as usize;
    if declared == 0 || HEADER_LENGTH + declared > body.len() {
        return None;
    }
    let data = &body[HEADER_LENGTH..HEADER_LENGTH + declared];
    match kind {
        KIND_VIDEO => Some(MediaPacket::Video {
            keyframe: flags & 1 != 0,
            wallclock: flags & 2 != 0,
            pts_us,
            width: first,
            height: second,
            data,
        }),
        KIND_AUDIO => Some(MediaPacket::Audio {
            // The header spends 16 bits on the rate in units of 100 Hz.
            rate: first.saturating_mul(100),
            channels: second,
            data,
        }),
        _ => None,
    }
}

enum AudioCommand {
    Open { rate: u16, channels: u16 },
    Write(Vec<u8>),
}

/// Plays PCM off the network thread. A full queue drops the oldest packet: a
/// choppy half second is better than stalling the video behind it.
struct PcmPlayer {
    sender: SyncSender<AudioCommand>,
    opened: Mutex<Option<(u16, u16)>>,
}

impl PcmPlayer {
    fn spawn() -> Self {
        let (sender, receiver) = sync_channel::<AudioCommand>(12);
        thread::Builder::new()
            .name("browser-mirror-audio".to_string())
            .spawn(move || {
                let mut sink: Option<WaveOutSink> = None;
                let mut format = (0u16, 0u16);
                for command in receiver {
                    match command {
                        AudioCommand::Open { rate, channels } => {
                            if format != (rate, channels) {
                                format = (rate, channels);
                                sink = WaveOutSink::open(rate as u32, channels).ok();
                            }
                        }
                        AudioCommand::Write(pcm) => {
                            if let Some(device) = sink.as_mut() {
                                if device.write(&pcm).is_err() {
                                    sink = None;
                                    format = (0, 0);
                                }
                            }
                        }
                    }
                }
            })
            .ok();
        Self {
            sender,
            opened: Mutex::new(None),
        }
    }

    fn push(&self, rate: u16, channels: u16, pcm: &[u8]) {
        if rate == 0 || channels == 0 {
            return;
        }
        let mut opened = match self.opened.lock() {
            Ok(opened) => opened,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *opened != Some((rate, channels)) {
            // Blocking is fine here: the player is empty on a format change.
            let _ = self.sender.send(AudioCommand::Open { rate, channels });
            *opened = Some((rate, channels));
        }
        drop(opened);
        let _ = self.sender.try_send(AudioCommand::Write(pcm.to_vec()));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn the_accept_key_matches_the_rfc_vector() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn a_websocket_request_is_answered_with_101() {
        let request = "GET /stream?token=abc HTTP/1.1\r\n\
                       Host: 192.168.0.10:9407\r\n\
                       Upgrade: websocket\r\n\
                       Connection: Upgrade\r\n\
                       Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                       Sec-WebSocket-Version: 13";
        let reply = handshake_reply(request, Some("abc")).unwrap();
        assert!(reply.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(reply.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let request = "GET /stream?token=nope HTTP/1.1\r\n\
                       Upgrade: websocket\r\n\
                       Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";
        let reply = handshake_reply(request, Some("abc")).unwrap_err();
        assert!(reply.starts_with("HTTP/1.1 403 Forbidden"));
    }

    #[test]
    fn an_info_probe_answers_without_leaking_the_code() {
        let config = BrowserMirrorConfig {
            token: Some("204815".to_string()),
            ..BrowserMirrorConfig::default()
        };
        let FirstResponse::Close(reply) = http_reply("GET /info HTTP/1.1\r\nHost: x\r\n", &config)
        else {
            panic!("a discovery probe must not become a websocket session");
        };
        assert!(reply.starts_with("HTTP/1.1 200 OK"));
        assert!(reply.contains("Access-Control-Allow-Origin: *"));
        assert!(reply.contains("\"paired\":true"));
        assert!(!reply.contains("204815"));
        assert!(
            !reply.contains("\"hub\""),
            "a receiver whose hub never bound must not advertise a port"
        );
    }

    /// The sender reaches the library through this one reply, so the port it
    /// carries has to be the port the hub actually bound.
    #[test]
    fn the_info_probe_publishes_the_hub_port_and_keeps_the_body_length_honest() {
        let config = BrowserMirrorConfig {
            name: "戎耀会".to_string(),
            token: Some("204815".to_string()),
            hub_port: Some(7878),
            ..BrowserMirrorConfig::default()
        };
        let FirstResponse::Close(reply) = http_reply("GET /info HTTP/1.1\r\nHost: x\r\n", &config)
        else {
            panic!("a discovery probe must not become a websocket session");
        };
        assert!(reply.contains("\"hub\":7878"), "{reply}");
        assert!(!reply.contains("204815"), "the code stays out of discovery");

        // A Chinese device name is multi-byte: the header has to count bytes, or
        // a sender would read a truncated body and lose the `hub` field.
        let declared: usize = reply
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .and_then(|rest| rest.trim().parse().ok())
            })
            .expect("a length header");
        let body = reply.split("\r\n\r\n").nth(1).expect("a body");
        assert_eq!(declared, body.len(), "{body}");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("valid json");
        assert_eq!(parsed["name"], "戎耀会");
        assert_eq!(parsed["hub"], 7878);
    }

    #[test]
    fn a_private_network_preflight_is_answered() {
        let config = BrowserMirrorConfig::default();
        let FirstResponse::Close(reply) = http_reply(
            "OPTIONS /info HTTP/1.1\r\nHost: x\r\nAccess-Control-Request-Private-Network: true\r\n",
            &config,
        ) else {
            panic!("a preflight must not become a websocket session");
        };
        assert!(reply.starts_with("HTTP/1.1 204 No Content"));
        assert!(reply.contains("Access-Control-Allow-Private-Network: true"));
        assert!(reply.contains("Access-Control-Allow-Methods: GET, OPTIONS"));
    }

    /// Drops whatever it is given: these tests assert on the socket, not on a
    /// picture.
    struct ProbeOnlySink;

    impl FrameSink for ProbeOnlySink {
        fn on_frame(&self, _frame: VideoFrame) {}
    }

    /// Port 0 lets the OS pick, so a sweep test never races the real 9407.
    fn sweep_receiver() -> BrowserMirrorReceiver {
        BrowserMirrorReceiver::start(
            BrowserMirrorConfig {
                address: Ipv4Addr::LOCALHOST,
                port: 0,
                name: "扫描测试".to_string(),
                token: Some("246810".to_string()),
                hub_port: Some(7878),
            },
            Arc::new(ProbeOnlySink),
        )
        .expect("receiver bound")
    }

    /// Everything the receiver answers before it hangs up.
    fn read_reply(stream: &mut TcpStream) -> String {
        let mut text = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(read) = stream.read(&mut chunk) {
            if read == 0 {
                break;
            }
            text.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    /// Consumed one byte at a time up to the header terminator, so the tail of a
    /// reply never looks like the first bytes of the next read.
    fn read_reply_head(stream: &mut TcpStream) -> String {
        let mut text = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).unwrap_or(0) == 1 {
            text.push(byte[0]);
            if text.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    fn upgrade_request(token: &str) -> String {
        format!(
            "GET /stream?token={token} HTTP/1.1\r\n\
             Host: 127.0.0.1:9407\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        )
    }

    #[test]
    fn a_discovery_sweep_answers_every_probe_it_hits() {
        let receiver = sweep_receiver();
        let port = receiver.port();
        // An extension scanning its subnet holds every probe open at once; 48 is
        // the width the stub harness measured against 9407 and the hub's 7878.
        let probes: Vec<_> = (0..48)
            .map(|_| {
                thread::Builder::new()
                    .spawn(move || {
                        let mut stream =
                            TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                                .or_else(|_| {
                                    thread::sleep(Duration::from_millis(50));
                                    TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                                })
                                .ok()?;
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .ok()?;
                        stream
                            .write_all(b"GET /info HTTP/1.1\r\nHost: x\r\n\r\n")
                            .ok()?;
                        Some(read_reply(&mut stream))
                    })
                    .expect("probe thread")
            })
            .collect();

        let replies: Vec<String> = probes
            .into_iter()
            .map(|probe| {
                probe
                    .join()
                    .expect("no probe may panic")
                    .expect("every probe reached the receiver")
            })
            .collect();

        assert_eq!(
            replies.len(),
            48,
            "a sweep must not answer itself with a refusal"
        );
        for reply in &replies {
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
            assert!(
                reply.contains("\"hub\":7878"),
                "a truncated reply loses the hub port the sender needs: {reply}"
            );
        }
        receiver.stop();
    }

    /// A sender that passed the token check and now holds the room.
    fn live_screen(port: u16, token: &str) -> TcpStream {
        let mut screen = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        screen
            .write_all(upgrade_request(token).as_bytes())
            .expect("upgrade request");
        assert!(
            read_reply_head(&mut screen).starts_with("HTTP/1.1 101"),
            "the sender must get the websocket it asked for"
        );
        screen
    }

    /// A live session that is merely idle receives nothing — the server only
    /// writes a pong when the client pings — so a read that is still timing out
    /// is the proof the socket was never kicked. A reset or a clean close would
    /// answer with an error or zero bytes here instead.
    fn assert_still_streaming(screen: &mut TcpStream, receiver: &BrowserMirrorReceiver) {
        screen
            .set_read_timeout(Some(Duration::from_millis(1500)))
            .expect("read timeout");
        let mut quiet = [0u8; 64];
        let read = screen.read(&mut quiet);
        let outcome = format!(
            "{:?}",
            read.as_ref().map_err(|error| (error.kind(), error.to_string()))
        );
        assert!(
            matches!(
                &read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    )
            ),
            "the live screen was disturbed: {outcome} / {:?}",
            receiver.status()
        );
    }

    /// A probe answered `Close` must not reach the hand-over at all.
    fn probe(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("probe connect");
        stream
            .write_all(request.as_bytes())
            .expect("probe request");
        read_reply(&mut stream)
    }

    #[test]
    fn a_probe_leaves_the_screen_that_is_streaming_alone() {
        let receiver = sweep_receiver();
        let port = receiver.port();
        let mut screen = live_screen(port, "246810");

        // This is what a second device, or the live sender's own next sweep,
        // sends while the room is taken. It used to shut the session down between
        // the connect and the handshake.
        for _ in 0..8 {
            let reply = probe(port, "GET /info HTTP/1.1\r\nHost: x\r\n\r\n");
            assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        }

        assert_still_streaming(&mut screen, &receiver);
        receiver.stop();
    }

    /// The hand-over is what makes an uninvited connection dangerous, so it is
    /// gated on the same check as the session: a sender that guessed the wrong
    /// code learns it is not wanted without costing the room its picture.
    #[test]
    fn a_sender_with_the_wrong_code_cannot_take_over_a_live_screen() {
        let receiver = sweep_receiver();
        let port = receiver.port();
        let mut screen = live_screen(port, "246810");

        assert!(
            probe(port, &upgrade_request("000000")).starts_with("HTTP/1.1 403"),
            "a wrong code is still a refusal"
        );

        assert_still_streaming(&mut screen, &receiver);
        receiver.stop();
    }

    fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index & 3]),
        );
        frame
    }

    #[test]
    fn a_masked_text_frame_is_unmasked() {
        let wire = masked_frame(0x01, br#"{"type":"hello"}"#);
        let mut reader = WsReader::new(Cursor::new(wire), Vec::new());
        assert_eq!(
            reader.next().unwrap(),
            Some(WsMessage::Text(r#"{"type":"hello"}"#.to_string()))
        );
    }

    #[test]
    fn two_frames_in_one_read_are_both_delivered() {
        let mut wire = masked_frame(0x02, &[1, 2, 3]);
        wire.extend(masked_frame(0x09, b"ping"));
        // The reader must not consume bytes belonging to the next frame.
        let mut reader = WsReader::new(Cursor::new(wire), Vec::new());
        assert_eq!(
            reader.next().unwrap(),
            Some(WsMessage::Binary(vec![1, 2, 3]))
        );
        assert_eq!(
            reader.next().unwrap(),
            Some(WsMessage::Ping(b"ping".to_vec()))
        );
        assert_eq!(reader.next().unwrap(), None);
    }

    #[test]
    fn a_frame_split_across_reads_survives() {
        let wire = masked_frame(0x02, &[9; 200]);
        let (head, tail) = wire.split_at(60);
        let mut reader = WsReader::new(Cursor::new(tail.to_vec()), head.to_vec());
        assert_eq!(
            reader.next().unwrap(),
            Some(WsMessage::Binary(vec![9; 200]))
        );
    }

    #[test]
    fn an_unmasked_client_frame_is_rejected() {
        let mut reader = WsReader::new(Cursor::new(vec![0x81, 0x02, b'h', b'i']), Vec::new());
        assert!(reader.next().is_err());
    }

    #[test]
    fn media_headers_carry_video_and_audio_geometry() {
        let mut video = vec![0u8; HEADER_LENGTH];
        video[0] = KIND_VIDEO;
        video[1] = 1;
        video[4..12].copy_from_slice(&7_000_000u64.to_le_bytes());
        video[12..14].copy_from_slice(&1920u16.to_le_bytes());
        video[14..16].copy_from_slice(&1080u16.to_le_bytes());
        video[16..20].copy_from_slice(&4u32.to_le_bytes());
        video.extend_from_slice(&[0, 0, 0, 1]);
        match parse_media(&video) {
            Some(MediaPacket::Video {
                keyframe,
                pts_us,
                width,
                height,
                data,
                ..
            }) => {
                assert!(keyframe);
                assert_eq!(pts_us, 7_000_000);
                assert_eq!((width, height), (1920, 1080));
                assert_eq!(data, &[0, 0, 0, 1]);
            }
            other => panic!("expected video, got {}", other.is_some()),
        }

        let mut audio = vec![0u8; HEADER_LENGTH];
        audio[0] = KIND_AUDIO;
        audio[12..14].copy_from_slice(&480u16.to_le_bytes());
        audio[14..16].copy_from_slice(&2u16.to_le_bytes());
        audio[16..20].copy_from_slice(&4u32.to_le_bytes());
        audio.extend_from_slice(&[1, 0, 2, 0]);
        assert!(matches!(
            parse_media(&audio),
            Some(MediaPacket::Audio {
                rate: 48_000,
                channels: 2,
                ..
            })
        ));
    }

    #[test]
    fn a_truncated_media_frame_is_dropped() {
        let mut body = vec![0u8; HEADER_LENGTH];
        body[0] = KIND_VIDEO;
        body[16..20].copy_from_slice(&100u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 10]);
        assert!(parse_media(&body).is_none());
    }

    #[test]
    fn base64_padding_is_counted_not_guessed() {
        assert_eq!(base64_decode_len("dGhlIHNhbXBsZSBub25jZQ=="), Some(16));
        assert_eq!(base64_decode_len("abc"), None);
        assert_eq!(base64_decode_len("===="), None);
    }
}
