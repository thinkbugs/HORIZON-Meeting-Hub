//! Google Cast v2 sender: the interop path to a Chromecast or a Cast-enabled TV.
//!
//! A Cast device is discovered like any other mDNS peer (`_googlecast._tcp`), but
//! its control channel is not HTTP. The sender opens a TLS connection to port
//! 8009 and exchanges length-prefixed protobuf `CastMessage`s over that one
//! socket. Channels are multiplexed by namespace, not by connection:
//!
//! | namespace | purpose |
//! |---|---|
//! | `urn:x-cast:com.google.cast.tp.connection` | `CONNECT` / `CLOSE` between virtual transports |
//! | `urn:x-cast:com.google.cast.tp.heartbeat` | `PING` / `PONG`; the device drops silent senders |
//! | `urn:x-cast:com.google.cast.receiver` | `LAUNCH`, `STOP`, volume, receiver status |
//! | `urn:x-cast:com.google.cast.media` | `LOAD`, `PLAY`, `PAUSE`, `SEEK`, media status |
//!
//! ## Why this is written by hand
//!
//! The offline cargo registry on this machine has no protobuf crate and no TLS
//! crate, so both are built here: the message schema is eight fields and a varint
//! decoder, and the TLS client is SChannel through the `windows` crate. Neither is
//! a general implementation — see the next section for what that costs.
//!
//! ## What the TLS client does and does not check
//!
//! SChannel is acquired with `SCH_CRED_MANUAL_CRED_VALIDATION`, so the handshake
//! completes without a chain walk. That is deliberate: a Cast device presents a
//! certificate from the Google Cast root, which ships inside the official sender
//! SDK and is not in the Windows trust store; a validating handshake would fail
//! on every real device. Pinning that root is the correct follow-up and is not
//! done here because the root is not obtainable offline. What the channel still
//! guarantees is confidentiality and integrity against a passive listener — it
//! does **not** authenticate the receiver, so an active attacker on the LAN could
//! impersonate a television. The sender only ever transmits media URLs the user
//! already chose, and the receiver cannot reach back into this machine, which
//! bounds the damage but not the need to say it plainly.
//!
//! ## Registration
//!
//! This sender launches `CC1AD845`, the Default Media Receiver. It is a Google
//! app, so no developer registration is involved. A custom receiver — our tile
//! stream, for one — would need an app ID registered in the Google Cast developer
//! console and hosted sender origin, which this project does not have. Cast stays
//! an interop format for URLs; the low-latency path is [`crate::wire`].
//!
//! ## Verified
//!
//! Framing, protobuf and the channel conversation are unit-tested against a
//! scripted stream: `CONNECT`, `LAUNCH`, `LOAD`, `PLAY`, volume and `STOP` are
//! checked on the bytes that would go to the device, and the receiver/media
//! statuses are fed back as the device would send them.
//!
//! The TLS client is tested twice. A unit test performs a real request to a
//! public host (`cargo test -p horizon-cast --lib -- --ignored cast`), which
//! proves the SChannel handshake, `EncryptMessage` layout and the decrypt loop
//! against a certificate this machine does *not* trust — the same posture a Cast
//! device requires. The full conversation was then run against a scripted
//! receiver over TLS on loopback (self-signed, Node's `tls` server), with the
//! `cast-probe` binary as the client:
//!
//! ```text
//! tls         established
//! channel     CONNECT sent                     -> receiver-0
//! launched    app CC1AD845 session scripted-session transport web-0
//! volume      set to 0.40
//! loaded      PLAYING at 0.00s (video/mp4)     contentId http://10.0.0.5/demo.mp4
//! sought      2.00s, state PLAYING
//! media       PLAYING at 3.99s / 300.00s       status streamed by the receiver
//! media       PLAYING at 6.00s / 300.00s
//! stopped     receiver app closed
//! ```
//!
//! The receiver reported negotiating TLS 1.2 with ECDHE-RSA-AES128-GCM-SHA256,
//! and logged the matching inbound sequence: `CONNECT`, `LAUNCH 1`,
//! `SET_VOLUME 2`, `CONNECT web-0`, `LOAD 3`, `SEEK 4`, two `PONG`s answering its
//! heartbeats, `STOP 5`, `CLOSE`. That is the whole control plane against a
//! server it had never met, over the TLS implementation in this file.
//!
//! What remains untested is a real Chromecast: none answered mDNS on this LAN
//! (`cast-probe --seconds=6` found zero devices), so the differences a real
//! receiver brings — certificate, app startup latency, DRM/protocol quirks of a
//! specific model — are not covered.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::{CastError, CastResult};

pub const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

pub const RECEIVER_ID: &str = "receiver-0";

/// One `cast_channel.proto` message, in the subset the sender uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CastMessage {
    pub protocol_version: u32,
    pub source_id: String,
    pub destination_id: String,
    pub namespace: String,
    /// 0 = UTF-8 JSON, 1 = binary. Cast only uses the first for its channels.
    pub payload_type: u32,
    pub payload_utf8: Option<String>,
    pub payload_binary: Option<Vec<u8>>,
}

impl CastMessage {
    pub fn binary(source: &str, destination: &str, namespace: &str, payload: Vec<u8>) -> Self {
        Self {
            protocol_version: 0,
            source_id: source.to_string(),
            destination_id: destination.to_string(),
            namespace: namespace.to_string(),
            payload_type: 1,
            payload_utf8: None,
            payload_binary: Some(payload),
        }
    }

    pub fn text(source: &str, destination: &str, namespace: &str, payload: &str) -> Self {
        Self {
            protocol_version: 0,
            source_id: source.to_string(),
            destination_id: destination.to_string(),
            namespace: namespace.to_string(),
            payload_type: 0,
            payload_utf8: Some(payload.to_string()),
            payload_binary: None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        put_varint(&mut out, (1 << 3) | 0);
        put_varint(&mut out, self.protocol_version as u64);
        put_bytes(&mut out, 2, self.source_id.as_bytes());
        put_bytes(&mut out, 3, self.destination_id.as_bytes());
        put_bytes(&mut out, 4, self.namespace.as_bytes());
        put_varint(&mut out, (5 << 3) | 0);
        put_varint(&mut out, self.payload_type as u64);
        if let Some(payload) = &self.payload_utf8 {
            put_bytes(&mut out, 6, payload.as_bytes());
        }
        if let Some(payload) = &self.payload_binary {
            put_bytes(&mut out, 7, payload);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> CastResult<Self> {
        let mut out = Self::default();
        let mut at = 0usize;
        while at < bytes.len() {
            let key = get_varint(bytes, &mut at)?;
            let field = key >> 3;
            let wire = key & 7;
            match (field, wire) {
                (1, 0) => out.protocol_version = get_varint(bytes, &mut at)? as u32,
                (2, 2) => out.source_id = utf8_field(bytes, &mut at)?,
                (3, 2) => out.destination_id = utf8_field(bytes, &mut at)?,
                (4, 2) => out.namespace = utf8_field(bytes, &mut at)?,
                (5, 0) => out.payload_type = get_varint(bytes, &mut at)? as u32,
                (6, 2) => out.payload_utf8 = Some(utf8_field(bytes, &mut at)?),
                (7, 2) => out.payload_binary = Some(len_field(bytes, &mut at)?.to_vec()),
                // A newer device may append fields this sender does not know.
                // Skipping by wire type is what keeps that a non-event.
                (_, 0) => {
                    get_varint(bytes, &mut at)?;
                }
                (_, 1) => at = at.checked_add(8).ok_or(CastError::Malformed("protobuf fixed64"))?,
                (_, 2) => {
                    len_field(bytes, &mut at)?;
                }
                (_, 5) => at = at.checked_add(4).ok_or(CastError::Malformed("protobuf fixed32"))?,
                _ => return Err(CastError::Malformed("unknown protobuf wire type")),
            }
        }
        if out.source_id.is_empty()
            || out.destination_id.is_empty()
            || out.namespace.is_empty()
        {
            return Err(CastError::Malformed("cast message is missing a required field"));
        }
        Ok(out)
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn get_varint(bytes: &[u8], at: &mut usize) -> CastResult<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or(CastError::Malformed("truncated varint"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(CastError::Malformed("varint exceeds 64 bits"))
}

fn len_field<'a>(bytes: &'a [u8], at: &mut usize) -> CastResult<&'a [u8]> {
    let len = get_varint(bytes, at)? as usize;
    let end = at.checked_add(len).ok_or(CastError::Malformed("protobuf length"))?;
    let slice = bytes
        .get(*at..end)
        .ok_or(CastError::Malformed("protobuf field past end"))?;
    *at = end;
    Ok(slice)
}

fn utf8_field(bytes: &[u8], at: &mut usize) -> CastResult<String> {
    String::from_utf8(len_field(bytes, at)?.to_vec())
        .map_err(|_| CastError::Malformed("cast field is not utf-8"))
}

const MAX_FRAME: usize = 1 << 20;

pub fn write_frame<W: Write>(out: &mut W, message: &CastMessage) -> CastResult<()> {
    let body = message.encode();
    if body.len() > MAX_FRAME {
        return Err(CastError::Malformed("cast frame too large"));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame)?;
    Ok(())
}

/// Outcome of a frame read that may be interrupted by a stream timeout.
#[derive(Debug)]
pub enum FramePoll {
    Ready(CastMessage),
    /// No complete frame yet. Anything already received stays in the reader.
    Waiting,
    /// The peer closed the stream between frames.
    Closed,
}

/// Incremental frame reader for streams with a read timeout.
///
/// [`read_frame`] uses `read_exact`: if the timeout fires halfway through a
/// frame, the bytes already consumed are lost and the next decode starts in the
/// middle of a message. A receiver that shortens its read timeout to service
/// playback status cannot afford that, so this reader keeps its own buffer and
/// only returns whole frames.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn poll<R: Read>(&mut self, input: &mut R) -> CastResult<FramePoll> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some(message) = self.take()? {
                return Ok(FramePoll::Ready(message));
            }
            match input.read(&mut chunk) {
                Ok(0) => return Ok(FramePoll::Closed),
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(FramePoll::Waiting);
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(FramePoll::Closed);
                }
                Err(_) => return Err(CastError::Io),
            }
        }
    }

    fn take(&mut self) -> CastResult<Option<CastMessage>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]])
            as usize;
        if len > MAX_FRAME {
            return Err(CastError::Malformed("cast frame too large"));
        }
        if self.buffer.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buffer[4..4 + len].to_vec();
        self.buffer.drain(..4 + len);
        CastMessage::decode(&body).map(Some)
    }
}

/// `Ok(None)` means the peer closed the channel cleanly between frames.
/// `Err(Timeout)` means the stream's read timeout expired before a frame
/// started; the caller can poll again.
pub fn read_frame<R: Read>(input: &mut R) -> CastResult<Option<CastMessage>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            return Err(CastError::Timeout);
        }
        Err(_) => return Err(CastError::Io),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(CastError::Malformed("cast frame too large"));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).map_err(|_| CastError::Io)?;
    CastMessage::decode(&body).map(Some)
}

/// The receiver app this sender is attached to, from a `RECEIVER_STATUS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub app_id: String,
    pub transport_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaStatus {
    pub player_state: String,
    pub current_time: f64,
    pub duration: f64,
    pub media_session_id: i64,
    /// Set when `player_state` is IDLE: FINISHED, CANCELLED, ERROR or
    /// INTERRUPTED.
    pub idle_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Receiver {
        volume: Option<f64>,
        muted: Option<bool>,
        session: Option<Session>,
    },
    Media(MediaStatus),
    /// The channel ended. Nothing else will arrive on it.
    Closed,
}

/// A Cast v2 sender over one already-established stream.
///
/// Generic over the stream so the conversation can be driven by a scripted
/// buffer in tests; [`tls::connect`] builds the SChannel one a device needs.
pub struct Client<S: Read + Write> {
    stream: S,
    reader: FrameReader,
    source_id: String,
    request_id: u64,
    session: Option<Session>,
    last_media_session_id: Option<i64>,
    queued: VecDeque<Event>,
}

impl<S: Read + Write> Client<S> {
    pub fn new(stream: S, source_id: &str) -> Self {
        Self {
            stream,
            reader: FrameReader::new(),
            source_id: source_id.to_string(),
            request_id: 0,
            session: None,
            last_media_session_id: None,
            queued: VecDeque::new(),
        }
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Opens the receiver channel. Every other message goes through it.
    pub fn connect(&mut self) -> CastResult<()> {
        self.send_text(RECEIVER_ID, NS_CONNECTION, &json!({ "type": "CONNECT" }))
    }

    /// Launches the receiver app and returns the transport it opened.
    pub fn launch(&mut self, app_id: &str) -> CastResult<Session> {
        let id = self.next_id();
        self.send_text(
            RECEIVER_ID,
            NS_RECEIVER,
            &json!({ "type": "LAUNCH", "appId": app_id, "requestId": id }),
        )?;
        let payload = self.await_response(id)?;
        let session = payload
            .pointer("/status/applications/0")
            .and_then(parse_session)
            .ok_or(CastError::InvalidState("receiver launched no application"))?;
        self.session = Some(session.clone());
        Ok(session)
    }

    /// Loads a media URL into the launched receiver app.
    pub fn load(&mut self, content_id: &str, content_type: &str) -> CastResult<MediaStatus> {
        let session = self.session.clone().ok_or(CastError::InvalidState("no receiver app"))?;
        self.send_text(
            &session.transport_id,
            NS_CONNECTION,
            &json!({ "type": "CONNECT" }),
        )?;
        let id = self.next_id();
        self.send_text(
            &session.transport_id,
            NS_MEDIA,
            &json!({
                "type": "LOAD",
                "requestId": id,
                "autoplay": true,
                "currentTime": 0,
                "media": {
                    "contentId": content_id,
                    "contentType": content_type,
                    "streamType": "BUFFERED",
                },
            }),
        )?;
        let payload = self.await_response(id)?;
        self.media_status(&payload)
    }

    pub fn play(&mut self) -> CastResult<MediaStatus> {
        self.media_command("PLAY", json!({}))
    }

    pub fn pause(&mut self) -> CastResult<MediaStatus> {
        self.media_command("PAUSE", json!({}))
    }

    pub fn seek(&mut self, seconds: f64) -> CastResult<MediaStatus> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(CastError::InvalidState("seek outside the timeline"));
        }
        self.media_command("SEEK", json!({ "currentTime": seconds }))
    }

    fn media_command(&mut self, kind: &str, extra: Value) -> CastResult<MediaStatus> {
        let session = self.session.clone().ok_or(CastError::InvalidState("no receiver app"))?;
        let id = self.next_id();
        let mut payload = json!({
            "type": kind,
            "requestId": id,
            "mediaSessionId": self.media_session_id().unwrap_or(0),
        });
        if let (Some(map), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                map.insert(key.clone(), value.clone());
            }
        }
        self.send_text(&session.transport_id, NS_MEDIA, &payload)?;
        let response = self.await_response(id)?;
        self.media_status(&response)
    }

    /// Stops the app and closes the session. Cast has no media-level stop.
    pub fn stop(&mut self) -> CastResult<()> {
        let Some(session) = self.session.clone() else {
            return Ok(());
        };
        let id = self.next_id();
        self.send_text(
            RECEIVER_ID,
            NS_RECEIVER,
            &json!({ "type": "STOP", "sessionId": session.session_id, "requestId": id }),
        )?;
        // A device that stopped answering must not hold the caller for the full
        // request wait: the channel is closed right after this either way.
        let _ = self.await_response_for(id, Duration::from_secs(2));
        self.session = None;
        Ok(())
    }

    /// `level` is 0.0..=1.0, the way a device reports volume.
    pub fn set_volume(&mut self, level: f64) -> CastResult<()> {
        if !level.is_finite() || !(0.0..=1.0).contains(&level) {
            return Err(CastError::InvalidState("volume outside 0..1"));
        }
        let id = self.next_id();
        self.send_text(
            RECEIVER_ID,
            NS_RECEIVER,
            &json!({ "type": "SET_VOLUME", "volume": { "level": level }, "requestId": id }),
        )?;
        let _ = self.await_response(id)?;
        Ok(())
    }

    pub fn set_muted(&mut self, muted: bool) -> CastResult<()> {
        let id = self.next_id();
        self.send_text(
            RECEIVER_ID,
            NS_RECEIVER,
            &json!({ "type": "SET_VOLUME", "volume": { "muted": muted }, "requestId": id }),
        )?;
        let _ = self.await_response(id)?;
        Ok(())
    }

    /// Reads one message. Heartbeats are answered inside; `None` is a message
    /// that carried no state a caller could act on.
    pub fn poll(&mut self) -> CastResult<Option<Event>> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        let message = match self.reader.poll(&mut self.stream)? {
            FramePoll::Ready(message) => message,
            FramePoll::Waiting => return Err(CastError::Timeout),
            FramePoll::Closed => return Ok(Some(Event::Closed)),
        };
        let payload = self.absorb(message)?;
        Ok(payload.and_then(|payload| self.event_of(&payload)))
    }

    /// Polite disconnect: the receiver is left running, the channel is not.
    pub fn close(&mut self) {
        let transport = self.session.as_ref().map(|s| s.transport_id.clone());
        let _ = self.send_text(
            RECEIVER_ID,
            NS_CONNECTION,
            &json!({ "type": "CLOSE" }),
        );
        if let Some(transport) = transport {
            let _ = self.send_text(&transport, NS_CONNECTION, &json!({ "type": "CLOSE" }));
        }
    }

    pub fn media_session_id(&self) -> Option<i64> {
        self.last_media_session_id
    }

    fn next_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    fn send_text(&mut self, destination: &str, namespace: &str, payload: &Value) -> CastResult<()> {
        let message = CastMessage::text(
            &self.source_id,
            destination,
            namespace,
            &payload.to_string(),
        );
        write_frame(&mut self.stream, &message)?;
        self.stream.flush().map_err(|_| CastError::Io)
    }

    /// Sends and reads until the message that answers `request_id` arrives.
    /// Unsolicited statuses met on the way are queued for `poll`.
    fn await_response(&mut self, request_id: u64) -> CastResult<Value> {
        self.await_response_for(request_id, Duration::from_secs(10))
    }

    /// The same wait, bounded by wall-clock time instead of by how long the
    /// stream's read timeout happens to be. A sender that reads on a short
    /// cadence to stay responsive still gets to wait out a slow app launch.
    fn await_response_for(&mut self, request_id: u64, total: Duration) -> CastResult<Value> {
        let deadline = Instant::now() + total;
        loop {
            let message = match self.reader.poll(&mut self.stream) {
                Ok(FramePoll::Ready(message)) => message,
                Ok(FramePoll::Closed) => {
                    return Err(CastError::InvalidState("cast channel closed"))
                }
                Ok(FramePoll::Waiting) => {
                    if Instant::now() >= deadline {
                        return Err(CastError::Timeout);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some(payload) = self.absorb(message)? else {
                continue;
            };
            let answered = payload.get("requestId").and_then(Value::as_u64) == Some(request_id);
            if answered {
                if payload.get("type").and_then(Value::as_str) == Some("INVALID_REQUEST") {
                    return Err(CastError::InvalidState("cast receiver rejected the request"));
                }
                return Ok(payload);
            }
            if let Some(event) = self.event_of(&payload) {
                self.queued.push_back(event);
            }
        }
    }

    /// Parses one inbound message and answers heartbeats. `None` means the
    /// message was protocol plumbing, not a status.
    fn absorb(&mut self, message: CastMessage) -> CastResult<Option<Value>> {
        if message.payload_type != 0 {
            return Ok(None);
        }
        let Some(text) = message.payload_utf8.as_deref() else {
            return Ok(None);
        };
        let payload: Value =
            serde_json::from_str(text).map_err(|_| CastError::Malformed("cast payload is not json"))?;
        match message.namespace.as_str() {
            NS_HEARTBEAT => {
                if payload.get("type").and_then(Value::as_str) == Some("PING") {
                    self.send_text(
                        &message.source_id,
                        NS_HEARTBEAT,
                        &json!({ "type": "PONG" }),
                    )?;
                }
                Ok(None)
            }
            NS_CONNECTION => Ok(None),
            _ => Ok(Some(payload)),
        }
    }

    fn event_of(&mut self, payload: &Value) -> Option<Event> {
        match payload.get("type").and_then(Value::as_str)? {
            "RECEIVER_STATUS" => {
                let status = payload.get("status")?;
                let volume = status
                    .pointer("/volume/level")
                    .and_then(Value::as_f64);
                let muted = status.pointer("/volume/muted").and_then(Value::as_bool);
                let session = status
                    .pointer("/applications/0")
                    .and_then(parse_session);
                if let Some(session) = &session {
                    self.session = Some(session.clone());
                }
                Some(Event::Receiver {
                    volume,
                    muted,
                    session,
                })
            }
            "MEDIA_STATUS" => {
                let status = payload.pointer("/status/0")?;
                let parsed = parse_media_status(status)?;
                self.last_media_session_id = Some(parsed.media_session_id);
                Some(Event::Media(parsed))
            }
            _ => None,
        }
    }

    fn media_status(&mut self, payload: &Value) -> CastResult<MediaStatus> {
        let status = payload
            .pointer("/status/0")
            .ok_or(CastError::Malformed("media response carries no status"))?;
        let parsed = parse_media_status(status)
            .ok_or(CastError::Malformed("media status is incomplete"))?;
        self.last_media_session_id = Some(parsed.media_session_id);
        Ok(parsed)
    }
}

fn parse_session(value: &Value) -> Option<Session> {
    Some(Session {
        app_id: value.get("appId")?.as_str()?.to_string(),
        transport_id: value.get("transportId")?.as_str()?.to_string(),
        session_id: value.get("sessionId")?.as_str()?.to_string(),
    })
}

fn parse_media_status(value: &Value) -> Option<MediaStatus> {
    Some(MediaStatus {
        player_state: value.get("playerState")?.as_str()?.to_string(),
        current_time: value.get("currentTime").and_then(Value::as_f64).unwrap_or(0.0),
        duration: value
            .pointer("/media/duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        media_session_id: value.get("mediaSessionId").and_then(Value::as_i64).unwrap_or(0),
        idle_reason: value
            .get("idleReason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The SChannel client that turns a TCP socket into the TLS stream Cast needs.
#[cfg(windows)]
pub mod tls {
    use std::ffi::c_void;
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::ptr;
    use std::time::Duration;

    use windows::core::{HRESULT, PCWSTR, PSTR};
    use windows::Win32::Foundation::{
        GetLastError, SEC_E_INCOMPLETE_MESSAGE, SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED,
        SEC_I_RENEGOTIATE, SYSTEMTIME,
    };
    use windows::Win32::System::SystemInformation::GetSystemTime;
    use windows::Win32::Security::Authentication::Identity::*;
    use windows::Win32::Security::Credentials::SecHandle;
    use windows::Win32::Security::Cryptography::{
        CertCreateSelfSignCertificate, CertFreeCertificateContext, CertGetCertificateContextProperty,
        CertStrToNameW, CryptAcquireContextW, CERT_CREATE_SELFSIGN_FLAGS, CERT_CONTEXT,
        CERT_KEY_PROV_INFO_PROP_ID, CERT_X500_NAME_STR, CRYPT_ALGORITHM_IDENTIFIER,
        CRYPT_DELETEKEYSET, CRYPT_INTEGER_BLOB, CRYPT_KEY_PROV_INFO, X509_ASN_ENCODING,
    };

    use crate::error::{CastError, CastResult};

    const SEC_E_OK: HRESULT = HRESULT(0);

    /// A self-signed certificate generated when the receiver starts.
    ///
    /// Chromium deliberately skips TLS chain validation for Cast devices (they
    /// have no public hostname) and authenticates the *device* separately; see
    /// [`crate::cast_recv`]. Two properties of this certificate still matter to
    /// it: the validity window must already have started, and it must expire
    /// within four days, because the device-auth signature is bound to this
    /// certificate and Chromium reuses its expiry as the signature lifetime.
    /// Regenerating it per process also keeps the key out of on-disk assets.
    pub struct ServerCertificate {
        context: *const CERT_CONTEXT,
        der: Vec<u8>,
        provider: String,
        provider_type: u32,
        container: String,
    }

    // The certificate context is immutable and the key lives in a CNG
    // container; both are safe to share between session threads.
    unsafe impl Send for ServerCertificate {}
    unsafe impl Sync for ServerCertificate {}

    impl ServerCertificate {
        pub fn generate(common_name: &str) -> CastResult<Self> {
            unsafe {
                let now = GetSystemTime();
                let start = shift_days(now, -1);
                let end = shift_days(now, 3);

                let subject = encode_name(common_name)?;
                let algorithm = CRYPT_ALGORITHM_IDENTIFIER {
                    pszObjId: PSTR(SHA256_RSA.as_ptr() as *mut u8),
                    Parameters: CRYPT_INTEGER_BLOB::default(),
                };
                let subject_blob = CRYPT_INTEGER_BLOB {
                    cbData: subject.len() as u32,
                    pbData: subject.as_ptr() as *mut u8,
                };
                // No `pKeyProvInfo`: CryptoAPI generates the key itself. Handing
                // it a key created through the CNG API produces a certificate
                // SChannel then rejects with SEC_E_NO_CREDENTIALS, even though
                // `CryptAcquireCertificatePrivateKey` can find that key.
                let context = CertCreateSelfSignCertificate(
                    None,
                    &subject_blob,
                    CERT_CREATE_SELFSIGN_FLAGS(0),
                    None,
                    Some(&algorithm),
                    Some(&start),
                    Some(&end),
                    None,
                );
                if context.is_null() {
                    return Err(CastError::Hresult(
                        HRESULT::from_win32(GetLastError().0).0,
                    ));
                }
                let (provider, provider_type, container) = match key_provider(context) {
                    Ok(key) => key,
                    Err(error) => {
                        let _ = CertFreeCertificateContext(Some(context));
                        return Err(error);
                    }
                };
                let der = std::slice::from_raw_parts(
                    (*context).pbCertEncoded,
                    (*context).cbCertEncoded as usize,
                )
                .to_vec();
                Ok(Self {
                    context,
                    der,
                    provider,
                    provider_type,
                    container,
                })
            }
        }

        /// The DER Chromium saw, which is also what the device-auth signature
        /// must cover.
        pub fn der(&self) -> &[u8] {
            &self.der
        }
    }

    impl Drop for ServerCertificate {
        fn drop(&mut self) {
            unsafe {
                let _ = CertFreeCertificateContext(Some(self.context));
            }
            delete_csp_key(&self.provider, self.provider_type, &self.container);
        }
    }

    /// Reads back the key container CryptoAPI chose so the key does not outlive
    /// the process that minted it.
    unsafe fn key_provider(context: *const CERT_CONTEXT) -> CastResult<(String, u32, String)> {
        let mut size = 0u32;
        CertGetCertificateContextProperty(context, CERT_KEY_PROV_INFO_PROP_ID, None, &mut size)?;
        if size == 0 {
            return Err(CastError::InvalidState("certificate has no key provider"));
        }
        let mut buffer = vec![0u8; size as usize];
        CertGetCertificateContextProperty(
            context,
            CERT_KEY_PROV_INFO_PROP_ID,
            Some(buffer.as_mut_ptr() as *mut c_void),
            &mut size,
        )?;
        let info = &*(buffer.as_ptr() as *const CRYPT_KEY_PROV_INFO);
        if info.pwszProvName.is_null() || info.pwszContainerName.is_null() {
            return Err(CastError::InvalidState("certificate key provider is unnamed"));
        }
        let provider = PCWSTR(info.pwszProvName.0)
            .to_string()
            .map_err(|_| CastError::InvalidState("certificate provider name is not utf-16"))?;
        let container = PCWSTR(info.pwszContainerName.0)
            .to_string()
            .map_err(|_| CastError::InvalidState("key container name is not utf-16"))?;
        Ok((provider, info.dwProvType, container))
    }

    fn delete_csp_key(provider: &str, provider_type: u32, container: &str) {
        let provider_name = wide(provider);
        let container_name = wide(container);
        let mut handle = 0usize;
        unsafe {
            let _ = CryptAcquireContextW(
                &mut handle,
                PCWSTR(container_name.as_ptr()),
                PCWSTR(provider_name.as_ptr()),
                provider_type,
                CRYPT_DELETEKEYSET,
            );
        }
    }

    /// RSA with SHA-256, the signature algorithm Chromium's Cast path builder
    /// accepts for developer certificates.
    const SHA256_RSA: &[u8] = b"1.2.840.113549.1.1.11\0";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn encode_name(common_name: &str) -> CastResult<Vec<u8>> {
        let text = wide(&format!("CN={common_name}"));
        unsafe {
            let mut size = 0u32;
            CertStrToNameW(
                X509_ASN_ENCODING,
                PCWSTR(text.as_ptr()),
                CERT_X500_NAME_STR,
                None,
                None,
                &mut size,
                None,
            )?;
            let mut encoded = vec![0u8; size as usize];
            CertStrToNameW(
                X509_ASN_ENCODING,
                PCWSTR(text.as_ptr()),
                CERT_X500_NAME_STR,
                None,
                Some(encoded.as_mut_ptr()),
                &mut size,
                None,
            )?;
            encoded.truncate(size as usize);
            Ok(encoded)
        }
    }

    fn shift_days(mut time: SYSTEMTIME, days: i32) -> SYSTEMTIME {
        let mut day = time.wDay as i32 + days;
        let mut month = time.wMonth as i32;
        let mut year = time.wYear as i32;
        while day < 1 {
            month -= 1;
            if month < 1 {
                month = 12;
                year -= 1;
            }
            day += days_in_month(year, month) as i32;
        }
        loop {
            let length = days_in_month(year, month) as i32;
            if day <= length {
                break;
            }
            day -= length;
            month += 1;
            if month > 12 {
                month = 1;
                year += 1;
            }
        }
        time.wDayOfWeek = 0;
        time.wDay = day as u16;
        time.wMonth = month as u16;
        time.wYear = year as u16;
        time
    }

    fn days_in_month(year: i32, month: i32) -> u16 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
            2 => 28,
            _ => 30,
        }
    }

    /// A blocking TLS client stream. Cast's protocol is JSON over TLS with no
    /// HTTP framing, so this is a plain byte stream, not a `BufReader` request.
    pub struct TlsStream {
        socket: TcpStream,
        context: SecHandle,
        sizes: SecPkgContext_StreamSizes,
        /// Encrypted bytes read from the socket but not yet decrypted.
        encrypted: Vec<u8>,
        /// Decrypted bytes not yet handed to the caller.
        plain: Vec<u8>,
        closed: bool,
    }

    impl TlsStream {
        pub fn connect(addr: SocketAddr, server_name: &str, timeout: Duration) -> CastResult<Self> {
            let socket = TcpStream::connect_timeout(&addr, timeout).map_err(|_| CastError::Io)?;
            socket.set_read_timeout(Some(timeout)).map_err(|_| CastError::Io)?;
            socket.set_write_timeout(Some(timeout)).map_err(|_| CastError::Io)?;
            let (socket, context, sizes, leftover) = unsafe { handshake(socket, server_name)? };
            Ok(Self {
                socket,
                context,
                sizes,
                encrypted: leftover,
                plain: Vec::new(),
                closed: false,
            })
        }

        /// Server side of the same channel: completes the handshake a CASTV2
        /// sender starts, using the certificate generated for this receiver run.
        pub fn accept(
            socket: TcpStream,
            timeout: Duration,
            certificate: &ServerCertificate,
        ) -> CastResult<Self> {
            socket.set_read_timeout(Some(timeout)).map_err(|_| CastError::Io)?;
            socket.set_write_timeout(Some(timeout)).map_err(|_| CastError::Io)?;
            let (socket, context, sizes, leftover) =
                unsafe { handshake_server(socket, certificate)? };
            Ok(Self {
                socket,
                context,
                sizes,
                encrypted: leftover,
                plain: Vec::new(),
                closed: false,
            })
        }

        /// Shortens the read wait once the channel is up, so a long-lived
        /// sender can service its command queue between heartbeats without
        /// giving up the patient timeout the handshake and app launch need.
        pub fn set_read_timeout(&self, timeout: Duration) -> CastResult<()> {
            self.socket
                .set_read_timeout(Some(timeout))
                .map_err(|_| CastError::Io)
        }

        fn decrypt_available(&mut self) -> io::Result<bool> {
            if self.encrypted.is_empty() {
                return Ok(false);
            }
            let mut buffers = [
                SecBuffer {
                    cbBuffer: self.encrypted.len() as u32,
                    BufferType: SECBUFFER_DATA,
                    pvBuffer: self.encrypted.as_mut_ptr() as *mut c_void,
                },
                SecBuffer::default(),
                SecBuffer::default(),
                SecBuffer::default(),
            ];
            let desc = SecBufferDesc {
                ulVersion: SECBUFFER_VERSION,
                cBuffers: buffers.len() as u32,
                pBuffers: buffers.as_mut_ptr(),
            };
            let status = unsafe { DecryptMessage(&self.context, &desc, 0, None) };
            if status == SEC_E_INCOMPLETE_MESSAGE {
                return Ok(false);
            }
            if status == SEC_I_CONTEXT_EXPIRED {
                self.closed = true;
                return Ok(true);
            }
            if status == SEC_I_RENEGOTIATE {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "cast channel renegotiated tls",
                ));
            }
            if status != SEC_E_OK {
                return Err(io::Error::other(format!(
                    "DecryptMessage failed: 0x{:08X}",
                    status.0 as u32
                )));
            }
            let mut extra: Option<(usize, usize)> = None;
            for buffer in &mut buffers {
                if buffer.cbBuffer == 0 || buffer.pvBuffer.is_null() {
                    continue;
                }
                let data = unsafe {
                    std::slice::from_raw_parts(buffer.pvBuffer as *const u8, buffer.cbBuffer as usize)
                };
                match buffer.BufferType {
                    SECBUFFER_DATA => self.plain.extend_from_slice(data),
                    SECBUFFER_EXTRA => {
                        extra = Some((buffer.pvBuffer as usize, buffer.cbBuffer as usize));
                    }
                    _ => {}
                }
            }
            // The extra buffer points inside `encrypted`: copy the unconsumed
            // tail down before the next read appends to it.
            let tail = extra.map(|(ptr, len)| {
                let offset = ptr - self.encrypted.as_ptr() as usize;
                self.encrypted[offset..offset + len].to_vec()
            });
            self.encrypted = tail.unwrap_or_default();
            Ok(true)
        }
    }

    impl Read for TlsStream {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            loop {
                if !self.plain.is_empty() {
                    let take = out.len().min(self.plain.len());
                    out[..take].copy_from_slice(&self.plain[..take]);
                    self.plain.drain(..take);
                    return Ok(take);
                }
                if self.decrypt_available()? {
                    if self.closed && self.plain.is_empty() {
                        return Ok(0);
                    }
                    continue;
                }
                let mut chunk = [0u8; 16 * 1024];
                let got = self.socket.read(&mut chunk)?;
                if got == 0 {
                    self.closed = true;
                    return Ok(0);
                }
                self.encrypted.extend_from_slice(&chunk[..got]);
            }
        }
    }

    impl Write for TlsStream {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if input.is_empty() {
                return Ok(0);
            }
            let max = self.sizes.cbMaximumMessage as usize;
            if max == 0 {
                return Err(io::Error::other("stream sizes unavailable"));
            }
            let take = input.len().min(max);
            let header = self.sizes.cbHeader as usize;
            let trailer = self.sizes.cbTrailer as usize;
            let mut packet = vec![0u8; header + take + trailer];
            packet[header..header + take].copy_from_slice(&input[..take]);
            let base = packet.as_mut_ptr();
            // The three views must cover the one allocation SChannel encrypts
            // in place: header, plaintext, trailer.
            let mut buffers = [
                SecBuffer {
                    cbBuffer: header as u32,
                    BufferType: SECBUFFER_STREAM_HEADER,
                    pvBuffer: base as *mut c_void,
                },
                SecBuffer {
                    cbBuffer: take as u32,
                    BufferType: SECBUFFER_DATA,
                    pvBuffer: unsafe { base.add(header) } as *mut c_void,
                },
                SecBuffer {
                    cbBuffer: trailer as u32,
                    BufferType: SECBUFFER_STREAM_TRAILER,
                    pvBuffer: unsafe { base.add(header + take) } as *mut c_void,
                },
                SecBuffer {
                    cbBuffer: 0,
                    BufferType: SECBUFFER_EMPTY,
                    pvBuffer: ptr::null_mut(),
                },
            ];
            let desc = SecBufferDesc {
                ulVersion: SECBUFFER_VERSION,
                cBuffers: buffers.len() as u32,
                pBuffers: buffers.as_mut_ptr(),
            };
            let status = unsafe { EncryptMessage(&self.context, 0, &desc, 0) };
            if status != SEC_E_OK {
                return Err(io::Error::other(format!(
                    "EncryptMessage failed: 0x{:08X}",
                    status.0 as u32
                )));
            }
            self.socket.write_all(&packet)?;
            Ok(take)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.socket.flush()
        }
    }

    impl Drop for TlsStream {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteSecurityContext(&self.context);
            }
        }
    }

    /// Runs the SChannel handshake. The credentials handle is released before
    /// returning; the context outlives it, which SChannel allows. Any bytes the
    /// peer sent behind the handshake are returned for the decrypt loop.
    unsafe fn handshake(
        mut socket: TcpStream,
        server_name: &str,
    ) -> CastResult<(TcpStream, SecHandle, SecPkgContext_StreamSizes, Vec<u8>)> {
        let mut credential = SecHandle::default();
        let mut cred = SCHANNEL_CRED {
            dwVersion: SCHANNEL_CRED_VERSION,
            dwFlags: SCH_CRED_MANUAL_CRED_VALIDATION | SCH_USE_STRONG_CRYPTO,
            ..Default::default()
        };
        AcquireCredentialsHandleW(
            PCWSTR::null(),
            windows::core::w!("Microsoft Unified Security Protocol Provider"),
            SECPKG_CRED_OUTBOUND,
            None,
            Some(&mut cred as *mut SCHANNEL_CRED as *const c_void),
            None,
            None,
            &mut credential,
            None,
        )?;

        let name: Vec<u16> = server_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut context = SecHandle::default();
        let mut have_context = false;
        let mut input: Vec<u8> = Vec::new();

        let result = (|| -> CastResult<(SecHandle, SecPkgContext_StreamSizes, Vec<u8>)> {
            loop {
                let mut input_buffer = SecBuffer {
                    cbBuffer: input.len() as u32,
                    BufferType: SECBUFFER_TOKEN,
                    pvBuffer: input.as_mut_ptr() as *mut c_void,
                };
                let input_desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: if input.is_empty() { 0 } else { 1 },
                    pBuffers: &mut input_buffer,
                };
                let mut output_buffer = SecBuffer {
                    cbBuffer: 0,
                    BufferType: SECBUFFER_TOKEN,
                    pvBuffer: ptr::null_mut(),
                };
                let mut output_desc = SecBufferDesc {
                    ulVersion: SECBUFFER_VERSION,
                    cBuffers: 1,
                    pBuffers: &mut output_buffer,
                };
                let mut attributes = 0u32;
                let status = InitializeSecurityContextW(
                    Some(&credential),
                    if have_context {
                        Some(&context as *const SecHandle)
                    } else {
                        None
                    },
                    Some(name.as_ptr()),
                    ISC_REQ_ALLOCATE_MEMORY
                        | ISC_REQ_CONFIDENTIALITY
                        | ISC_REQ_INTEGRITY
                        | ISC_REQ_SEQUENCE_DETECT
                        | ISC_REQ_REPLAY_DETECT
                        | ISC_REQ_STREAM
                        | ISC_REQ_MANUAL_CRED_VALIDATION,
                    0,
                    SECURITY_NATIVE_DREP,
                    if input_desc.cBuffers == 0 {
                        None
                    } else {
                        Some(&input_desc as *const SecBufferDesc)
                    },
                    0,
                    if have_context {
                        None
                    } else {
                        Some(&mut context as *mut SecHandle)
                    },
                    Some(&mut output_desc as *mut SecBufferDesc),
                    &mut attributes,
                    None,
                );

                if output_buffer.cbBuffer > 0 && !output_buffer.pvBuffer.is_null() {
                    let token = std::slice::from_raw_parts(
                        output_buffer.pvBuffer as *const u8,
                        output_buffer.cbBuffer as usize,
                    );
                    socket.write_all(token).map_err(|_| CastError::Io)?;
                    let _ = FreeContextBuffer(output_buffer.pvBuffer);
                }

                // SChannel consumes the input; an EXTRA buffer is the part of
                // the next record that already arrived in the same packet.
                let leftover = if input_desc.cBuffers > 0
                    && input_buffer.BufferType == SECBUFFER_EXTRA
                    && input_buffer.cbBuffer > 0
                {
                    let offset = input.len() - input_buffer.cbBuffer as usize;
                    input[offset..].to_vec()
                } else {
                    Vec::new()
                };

                if status == SEC_E_OK {
                    have_context = true;
                    let mut sizes = SecPkgContext_StreamSizes::default();
                    QueryContextAttributesW(
                        &context,
                        SECPKG_ATTR_STREAM_SIZES,
                        &mut sizes as *mut _ as *mut c_void,
                    )?;
                    return Ok((context, sizes, leftover));
                }
                if status == SEC_I_CONTINUE_NEEDED {
                    have_context = true;
                    input = leftover;
                } else if status == SEC_E_INCOMPLETE_MESSAGE {
                    input = leftover;
                } else {
                    return Err(CastError::Hresult(status.0));
                }

                let mut chunk = [0u8; 16 * 1024];
                let got = socket.read(&mut chunk).map_err(|_| CastError::Io)?;
                if got == 0 {
                    return Err(CastError::Io);
                }
                input.extend_from_slice(&chunk[..got]);
            }
        })();

        let _ = FreeCredentialsHandle(&credential);
        result.map(|(context, sizes, leftover)| (socket, context, sizes, leftover))
    }

    /// The identity a Cast sender is shown. Compiled in so the receiver has one
    /// before its first connection; the key is used for nothing else.
    /// Runs the inbound half of the SChannel handshake a CASTV2 sender starts.
    unsafe fn handshake_server(
        mut socket: TcpStream,
        certificate: &ServerCertificate,
    ) -> CastResult<(TcpStream, SecHandle, SecPkgContext_StreamSizes, Vec<u8>)> {
        let mut chain = [certificate.context as *mut CERT_CONTEXT];
        let mut cred = SCHANNEL_CRED {
            dwVersion: SCHANNEL_CRED_VERSION,
            cCreds: 1,
            paCred: chain.as_mut_ptr(),
            dwFlags: SCH_USE_STRONG_CRYPTO,
            // Without this SChannel reads `paCred` as certificate hashes and
            // reports SEC_E_NO_CREDENTIALS for a perfectly good certificate.
            dwCredFormat: SCH_CRED_FORMAT_CERT_CONTEXT,
            ..Default::default()
        };
        let mut credential = SecHandle::default();
        let acquired = AcquireCredentialsHandleW(
            PCWSTR::null(),
            windows::core::w!("Microsoft Unified Security Protocol Provider"),
            SECPKG_CRED_INBOUND,
            None,
            Some(&mut cred as *mut SCHANNEL_CRED as *const c_void),
            None,
            None,
            &mut credential,
            None,
        );

        let result = match acquired {
            Ok(()) => {
                let mut context = SecHandle::default();
                let mut have_context = false;
                let mut input: Vec<u8> = Vec::new();
                let mut need_read = true;
                loop {
                    if need_read {
                        let mut chunk = [0u8; 16 * 1024];
                        match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => break Err(CastError::Io),
                            Ok(got) => input.extend_from_slice(&chunk[..got]),
                        }
                    }

                    let mut input_buffer = SecBuffer {
                        cbBuffer: input.len() as u32,
                        BufferType: SECBUFFER_TOKEN,
                        pvBuffer: input.as_mut_ptr() as *mut c_void,
                    };
                    let input_desc = SecBufferDesc {
                        ulVersion: SECBUFFER_VERSION,
                        cBuffers: 1,
                        pBuffers: &mut input_buffer,
                    };
                    let mut output_buffer = SecBuffer {
                        cbBuffer: 0,
                        BufferType: SECBUFFER_TOKEN,
                        pvBuffer: ptr::null_mut(),
                    };
                    let mut output_desc = SecBufferDesc {
                        ulVersion: SECBUFFER_VERSION,
                        cBuffers: 1,
                        pBuffers: &mut output_buffer,
                    };
                    let mut attributes = 0u32;
                    let status = AcceptSecurityContext(
                        Some(&credential),
                        if have_context {
                            Some(&context as *const SecHandle)
                        } else {
                            None
                        },
                        Some(&input_desc as *const SecBufferDesc),
                        ASC_REQ_ALLOCATE_MEMORY
                            | ASC_REQ_CONFIDENTIALITY
                            | ASC_REQ_INTEGRITY
                            | ASC_REQ_SEQUENCE_DETECT
                            | ASC_REQ_REPLAY_DETECT
                            | ASC_REQ_STREAM,
                        SECURITY_NATIVE_DREP,
                        if have_context {
                            None
                        } else {
                            Some(&mut context as *mut SecHandle)
                        },
                        Some(&mut output_desc as *mut SecBufferDesc),
                        &mut attributes,
                        None,
                    );

                    if output_buffer.cbBuffer > 0 && !output_buffer.pvBuffer.is_null() {
                        let token = std::slice::from_raw_parts(
                            output_buffer.pvBuffer as *const u8,
                            output_buffer.cbBuffer as usize,
                        );
                        let written = socket.write_all(token);
                        let _ = FreeContextBuffer(output_buffer.pvBuffer);
                        if written.is_err() {
                            break Err(CastError::Io);
                        }
                    }

                    let leftover = if input_buffer.BufferType == SECBUFFER_EXTRA
                        && input_buffer.cbBuffer > 0
                    {
                        let offset = input.len() - input_buffer.cbBuffer as usize;
                        input[offset..].to_vec()
                    } else {
                        Vec::new()
                    };

                    if status == SEC_E_OK {
                        let mut sizes = SecPkgContext_StreamSizes::default();
                        match QueryContextAttributesW(
                            &context,
                            SECPKG_ATTR_STREAM_SIZES,
                            &mut sizes as *mut _ as *mut c_void,
                        ) {
                            Ok(()) => break Ok((context, sizes, leftover)),
                            Err(_) => {
                                break Err(CastError::InvalidState(
                                    "cast receiver has no stream sizes",
                                ))
                            }
                        }
                    }
                    if status == SEC_I_CONTINUE_NEEDED {
                        have_context = true;
                        input = leftover;
                        need_read = true;
                        continue;
                    }
                    if status == SEC_E_INCOMPLETE_MESSAGE {
                        // The peer split a handshake message across segments;
                        // SChannel expects the same bytes back with more behind.
                        need_read = true;
                        continue;
                    }
                    if std::env::var_os("HORIZON_CAST_DEBUG").is_some() {
                        eprintln!(
                            "cast: AcceptSecurityContext failed: 0x{:08X}, {} input bytes",
                            status.0 as u32,
                            input.len()
                        );
                    }
                    break Err(CastError::Hresult(status.0));
                }
            }
            Err(error) => {
                if std::env::var_os("HORIZON_CAST_DEBUG").is_some() {
                    eprintln!(
                        "cast: AcquireCredentialsHandle failed: 0x{:08X}",
                        error.code().0 as u32
                    );
                }
                Err(CastError::Hresult(error.code().0))
            }
        };

        let _ = FreeCredentialsHandle(&credential);
        result.map(|(context, sizes, leftover)| (socket, context, sizes, leftover))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn generated_certificate_is_usable_as_schannel_credential() {
            let certificate =
                ServerCertificate::generate("HORIZON Test").expect("certificate generated");
            assert!(!certificate.der().is_empty());
            assert!(!certificate.container.is_empty());
            let mut chain = [certificate.context as *mut CERT_CONTEXT];
            let mut cred = SCHANNEL_CRED {
                dwVersion: SCHANNEL_CRED_VERSION,
                cCreds: 1,
                paCred: chain.as_mut_ptr(),
                dwFlags: SCH_USE_STRONG_CRYPTO,
                dwCredFormat: SCH_CRED_FORMAT_CERT_CONTEXT,
                ..Default::default()
            };
            let mut credential = SecHandle::default();
            let result = unsafe {
                AcquireCredentialsHandleW(
                    PCWSTR::null(),
                    windows::core::w!("Microsoft Unified Security Protocol Provider"),
                    SECPKG_CRED_INBOUND,
                    None,
                    Some(&mut cred as *mut SCHANNEL_CRED as *const c_void),
                    None,
                    None,
                    &mut credential,
                    None,
                )
            };
            if let Err(error) = result {
                panic!(
                    "SChannel refused the certificate: 0x{:08X}",
                    error.code().0 as u32
                );
            }
            unsafe {
                let _ = FreeCredentialsHandle(&credential);
            }
        }
    }
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};
    use std::sync::{Arc, Mutex};

    fn json_frame(source: &str, destination: &str, namespace: &str, payload: Value) -> Vec<u8> {
        let message = CastMessage::text(source, destination, namespace, &payload.to_string());
        let body = message.encode();
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Stands in for the TLS stream: pre-scripted bytes in, everything the
    /// client writes collected for inspection.
    struct Script {
        input: Cursor<Vec<u8>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for Script {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(out)
        }
    }

    impl Write for Script {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(input);
            Ok(input.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn launch_status(request_id: u64) -> Vec<u8> {
        json_frame(
            RECEIVER_ID,
            "sender-0",
            NS_RECEIVER,
            json!({
                "type": "RECEIVER_STATUS",
                "requestId": request_id,
                "status": {
                    "volume": { "level": 0.5, "muted": false },
                    "applications": [{
                        "appId": DEFAULT_MEDIA_RECEIVER,
                        "sessionId": "7f3e",
                        "transportId": "web-9",
                        "displayName": "Default Media Receiver"
                    }]
                }
            }),
        )
    }

    fn media_status(request_id: Option<u64>, state: &str, time: f64) -> Vec<u8> {
        let mut payload = json!({
            "type": "MEDIA_STATUS",
            "status": [{
                "mediaSessionId": 7,
                "playerState": state,
                "currentTime": time,
                "media": { "duration": 120.0 }
            }]
        });
        if let Some(id) = request_id {
            payload["requestId"] = json!(id);
        }
        json_frame("web-9", "sender-0", NS_MEDIA, payload)
    }

    fn receiver_ok(request_id: u64) -> Vec<u8> {
        json_frame(
            RECEIVER_ID,
            "sender-0",
            NS_RECEIVER,
            json!({ "type": "RECEIVER_STATUS", "requestId": request_id, "status": {} }),
        )
    }

    #[test]
    fn a_message_round_trips_through_its_wire_bytes() {
        let message = CastMessage::text(
            "sender-0",
            RECEIVER_ID,
            NS_HEARTBEAT,
            r#"{"type":"PING"}"#,
        );
        let encoded = message.encode();
        // Field 1 (protocol version) is a varint zero; field 2 is the source id.
        assert_eq!(&encoded[..4], &[0x08, 0x00, 0x12, 0x08]);
        assert_eq!(&encoded[4..12], b"sender-0");
        assert_eq!(CastMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn unknown_protobuf_fields_are_skipped_not_fatal() {
        let mut encoded = CastMessage::text("a", "b", NS_CONNECTION, "{}").encode();
        // Field 8, varint wire type: a newer device's addition this build
        // does not know about.
        encoded.extend_from_slice(&[0x40, 0x2a]);
        let decoded = CastMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.source_id, "a");
        assert_eq!(decoded.payload_utf8.as_deref(), Some("{}"));
    }

    #[test]
    fn frames_preserve_lengths_and_report_clean_eof() {
        let first = CastMessage::text("a", "b", NS_MEDIA, "{\"x\":1}");
        let second = CastMessage::text("c", "d", NS_RECEIVER, "{}");
        let mut wire = Vec::new();
        write_frame(&mut wire, &first).unwrap();
        write_frame(&mut wire, &second).unwrap();
        let mut cursor = Cursor::new(wire);
        assert_eq!(read_frame(&mut cursor).unwrap(), Some(first));
        assert_eq!(read_frame(&mut cursor).unwrap(), Some(second));
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_an_empty_message() {
        let message = CastMessage::text("a", "b", NS_MEDIA, "{}");
        let body = message.encode();
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&body[..body.len() - 1]);
        assert!(read_frame(&mut Cursor::new(wire)).is_err());
    }

    /// Pre-scripted reads: bytes, a timeout, or EOF.
    struct Steps {
        steps: std::collections::VecDeque<Result<Vec<u8>, std::io::ErrorKind>>,
    }

    impl Steps {
        fn new(steps: Vec<Result<Vec<u8>, std::io::ErrorKind>>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
            }
        }
    }

    impl Read for Steps {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                Some(Ok(bytes)) => {
                    assert!(bytes.len() <= out.len());
                    out[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Some(Err(kind)) => Err(kind.into()),
                None => Ok(0),
            }
        }
    }

    #[test]
    fn a_frame_interrupted_by_a_read_timeout_is_not_lost() {
        let message = CastMessage::text("a", "b", NS_MEDIA, "{\"x\":1}");
        let mut wire = Vec::new();
        write_frame(&mut wire, &message).unwrap();
        let split = wire.len() / 2;
        let mut source = Steps::new(vec![
            Ok(wire[..split].to_vec()),
            Err(std::io::ErrorKind::WouldBlock),
            Ok(wire[split..].to_vec()),
        ]);
        let mut reader = FrameReader::new();

        assert!(matches!(
            reader.poll(&mut source).unwrap(),
            FramePoll::Waiting
        ));
        match reader.poll(&mut source).unwrap() {
            FramePoll::Ready(ready) => assert_eq!(ready, message),
            other => panic!("expected the whole frame, got {other:?}"),
        }
    }

    #[test]
    fn the_frame_reader_takes_each_frame_then_reports_eof() {
        let first = CastMessage::text("a", "b", NS_MEDIA, "{\"n\":1}");
        let second = CastMessage::text("c", "d", NS_RECEIVER, "{}");
        let mut wire = Vec::new();
        write_frame(&mut wire, &first).unwrap();
        write_frame(&mut wire, &second).unwrap();
        let mut source = Steps::new(vec![Ok(wire), Ok(Vec::new())]);
        let mut reader = FrameReader::new();

        match reader.poll(&mut source).unwrap() {
            FramePoll::Ready(ready) => assert_eq!(ready, first),
            other => panic!("expected the first frame, got {other:?}"),
        }
        match reader.poll(&mut source).unwrap() {
            FramePoll::Ready(ready) => assert_eq!(ready, second),
            other => panic!("expected the second frame, got {other:?}"),
        }
        assert!(matches!(
            reader.poll(&mut source).unwrap(),
            FramePoll::Closed
        ));
    }

    #[test]
    fn the_whole_cast_conversation_sends_the_expected_bytes() {
        let mut input = Vec::new();
        input.extend(launch_status(1));
        input.extend(media_status(Some(2), "PLAYING", 1.5));
        input.extend(media_status(Some(3), "PLAYING", 4.0));
        input.extend(receiver_ok(4));
        input.extend(receiver_ok(5));
        // A heartbeat and an unsolicited status, met by `poll`.
        input.extend(json_frame(
            RECEIVER_ID,
            "sender-0",
            NS_HEARTBEAT,
            json!({ "type": "PING" }),
        ));
        input.extend(media_status(None, "PLAYING", 6.0));

        let written = Arc::new(Mutex::new(Vec::new()));
        let script = Script {
            input: Cursor::new(input),
            written: written.clone(),
        };
        let mut client = Client::new(script, "sender-0");

        client.connect().unwrap();
        let session = client.launch(DEFAULT_MEDIA_RECEIVER).unwrap();
        assert_eq!(session.transport_id, "web-9");
        assert_eq!(session.session_id, "7f3e");

        let loaded = client
            .load("http://192.168.0.9:9000/a.mp4", "video/mp4")
            .unwrap();
        assert_eq!(loaded.media_session_id, 7);
        assert_eq!(loaded.duration, 120.0);

        let playing = client.play().unwrap();
        assert_eq!(playing.player_state, "PLAYING");
        client.set_volume(0.3).unwrap();
        client.stop().unwrap();
        assert!(client.session().is_none());

        // Heartbeat in, PONG out; poll reports nothing for it.
        assert!(client.poll().unwrap().is_none());
        match client.poll().unwrap() {
            Some(Event::Media(status)) => assert_eq!(status.current_time, 6.0),
            other => panic!("expected a media status, got {other:?}"),
        }

        let bytes = written.lock().unwrap().clone();
        let mut cursor = Cursor::new(bytes);
        let mut sent = Vec::new();
        while let Some(message) = read_frame(&mut cursor).unwrap() {
            sent.push(message);
        }
        let describe = |message: &CastMessage| {
            (
                message.destination_id.clone(),
                message.namespace.clone(),
                message.payload_utf8.clone().unwrap(),
            )
        };
        let first = describe(&sent[0]);
        assert_eq!(first.0, RECEIVER_ID);
        assert_eq!(first.1, NS_CONNECTION);
        assert!(first.2.contains("CONNECT"));

        let launch = describe(&sent[1]);
        assert_eq!(launch.1, NS_RECEIVER);
        assert!(launch.2.contains("\"LAUNCH\""));
        assert!(launch.2.contains(DEFAULT_MEDIA_RECEIVER));

        let transport_connect = describe(&sent[2]);
        assert_eq!(transport_connect.0, "web-9");
        assert_eq!(transport_connect.1, NS_CONNECTION);

        let load = describe(&sent[3]);
        assert_eq!(load.0, "web-9");
        assert!(load.2.contains("\"LOAD\""));
        assert!(load.2.contains("http://192.168.0.9:9000/a.mp4"));
        assert!(load.2.contains("video/mp4"));

        let play = describe(&sent[4]);
        assert!(play.2.contains("\"PLAY\""));
        assert!(play.2.contains("\"mediaSessionId\":7"));

        let volume = describe(&sent[5]);
        assert_eq!(volume.1, NS_RECEIVER);
        assert!(volume.2.contains("SET_VOLUME"));

        let stop = describe(&sent[6]);
        assert_eq!(stop.1, NS_RECEIVER);
        assert!(stop.2.contains("STOP"));
        assert!(stop.2.contains("7f3e"));

        let pong = describe(&sent[7]);
        assert_eq!(pong.1, NS_HEARTBEAT);
        assert!(pong.2.contains("PONG"));
    }

    #[test]
    fn a_rejected_request_is_an_error_not_a_status() {
        let rejected = json_frame(
            RECEIVER_ID,
            "sender-0",
            NS_RECEIVER,
            json!({ "type": "INVALID_REQUEST", "requestId": 1, "reason": "bad app" }),
        );
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut client = Client::new(
            Script {
                input: Cursor::new(rejected),
                written,
            },
            "sender-0",
        );
        assert!(client.launch(DEFAULT_MEDIA_RECEIVER).is_err());
    }

    #[test]
    fn seeking_outside_the_timeline_never_reaches_the_device() {
        let mut client = Client::new(
            Script {
                input: Cursor::new(Vec::new()),
                written: Arc::new(Mutex::new(Vec::new())),
            },
            "sender-0",
        );
        assert!(client.seek(-1.0).is_err());
        assert!(client.set_volume(1.5).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "needs internet access to a TLS host"]
    fn tls_client_carries_an_http_request_to_a_public_host() {
        use std::net::ToSocketAddrs;
        use std::time::Duration;
        let addr = ("example.com", 443)
            .to_socket_addrs()
            .expect("dns")
            .next()
            .expect("an address");
        let mut stream = tls::TlsStream::connect(addr, "example.com", Duration::from_secs(10))
            .expect("handshake");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            .expect("request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("response");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200"), "{text:.200}");
    }
}
