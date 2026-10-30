//! The H.264 TCP connection a sender opens after `SETUP` says it may mirror.
//!
//! This is not RTP. The sender opens one TCP connection to the port named in the
//! SETUP reply and writes a sequence of self-describing frames:
//!
//! ```text
//! 128-byte header | payload
//! ```
//!
//! The header is little-endian, unlike most of AirPlay:
//!
//! | offset | type | meaning |
//! |---|---|---|
//! | 0 | u32 | payload length |
//! | 4 | u16 | payload type: 0 video, 1 codec config |
//! | 6 | u16 | option |
//! | 8 | u64 | NTP timestamp (video only) |
//! | 40, 44 | f32 | source width, height (codec config only) |
//! | 56, 60 | f32 | output width, height (codec config only) |
//!
//! Video payloads are AVCC — a sequence of 4-byte big-endian lengths followed by
//! NAL units — encrypted with AES-128-CTR. The keystream is derived from three
//! values that all arrive before the connection does: the AES key from the first
//! SETUP, the X25519 shared secret from `pair-verify`, and the decimal
//! `streamConnectionID` from the second SETUP.
//!
//! ```text
//! eaes = SHA-512(aesKey ‖ shared)
//! key  = SHA-512("AirPlayStreamKey" ‖ connectionID ‖ eaes[0..16])[0..16]
//! iv   = SHA-512("AirPlayStreamIV"  ‖ connectionID ‖ eaes[0..16])[0..16]
//! ```
//!
//! The CTR state runs across frame boundaries, so a payload cannot be decrypted
//! on its own; [`MirrorCipher`] is created once per connection. Each connection
//! restarts the stream.
//!
//! ## What the sink receives
//!
//! Frames are converted to Annex B, with the sender's SPS/PPS prepended to every
//! IDR. That makes each key frame independently decodable: a decoder that joins
//! late, or drops a frame, can start at the next IDR without having seen the
//! codec-config packet. Width and height come from the config packet; PTS is the
//! sender's NTP timestamp in microseconds.
//!
//! ## Audio
//!
//! The SETUP stream list usually names an AAC-ELD audio stream too. It is
//! negotiated in [`super::session`] but its samples are not decoded here or
//! anywhere else in this crate. The limitation is deliberate and documented: a
//! picture without sound is honest, a silent success is not.

use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sha2::{Digest, Sha512};

use crate::airplay::crypto::AesCtr;
use crate::error::{CastError, CastResult};

const HEADER_LENGTH: usize = 128;
/// A frame larger than this is corruption, not video. The largest 4K IDR this
/// receiver has seen is under 2 MiB.
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
/// SPS and PPS together are a few dozen bytes in practice; the reference caps
/// its combined buffer at 100 KiB.
const MAX_SPS_PPS: usize = 102_400;

pub const PAYLOAD_VIDEO: u16 = 0;
pub const PAYLOAD_CODEC: u16 = 1;

/// One decrypted, Annex-B frame, ready for a decoder.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    /// Sender timestamp in microseconds.
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    /// `pts_us` is the sender's wall clock (`Date.now()` µs), so a preview can
    /// compare it with its own clock for latency diagnostics.
    pub wallclock: bool,
}

/// Where frames go. The engine does not know about Tauri, WebCodecs or windows;
/// the desktop shell supplies a sink that forwards frames to the webview.
pub trait FrameSink: Send + Sync {
    fn on_frame(&self, frame: VideoFrame);
}

/// Any consumer can be a sink through a closure.
impl<F> FrameSink for F
where
    F: Fn(VideoFrame) + Send + Sync,
{
    fn on_frame(&self, frame: VideoFrame) {
        self(frame)
    }
}

#[derive(Default)]
pub struct MirrorStats {
    pub connections: AtomicU64,
    pub codec_configs: AtomicU64,
    pub frames: AtomicU64,
    pub keyframes: AtomicU64,
    pub bytes: AtomicU64,
    pub width: AtomicU64,
    pub height: AtomicU64,
    pub pts_us: AtomicU64,
}

#[derive(Debug, Clone, Default)]
pub struct MirrorStatus {
    pub connections: u64,
    pub codec_configs: u64,
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub pts_us: u64,
}

impl MirrorStats {
    pub fn snapshot(&self) -> MirrorStatus {
        MirrorStatus {
            connections: self.connections.load(Ordering::Relaxed),
            codec_configs: self.codec_configs.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            keyframes: self.keyframes.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            width: self.width.load(Ordering::Relaxed) as u32,
            height: self.height.load(Ordering::Relaxed) as u32,
            pts_us: self.pts_us.load(Ordering::Relaxed),
        }
    }
}

/// Parsed 128-byte frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub size: u32,
    pub payload_type: u16,
    pub option: u16,
    pub ntp: u64,
    pub width_source: u32,
    pub height_source: u32,
    pub width: u32,
    pub height: u32,
}

impl FrameHeader {
    pub fn parse(bytes: &[u8]) -> CastResult<Self> {
        if bytes.len() < HEADER_LENGTH {
            return Err(CastError::Malformed("mirror header is short"));
        }
        let size = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let payload_type = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) & 0xff;
        let option = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
        let ntp = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let width_source = real_at(bytes, 40);
        let height_source = real_at(bytes, 44);
        let width = real_at(bytes, 56);
        let height = real_at(bytes, 60);
        Ok(Self {
            size,
            payload_type,
            option,
            ntp,
            width_source,
            height_source,
            width,
            height,
        })
    }

    /// The sender's NTP timestamp as microseconds. The upper half counts
    /// seconds since 1900, the lower half is the binary fraction.
    pub fn pts_us(&self) -> u64 {
        let seconds = (self.ntp >> 32) & 0xffff_ffff;
        let fraction = self.ntp & 0xffff_ffff;
        seconds * 1_000_000 + ((fraction * 1_000_000) >> 32)
    }

    /// True when the sender filled in a plausible geometry. Some senders leave
    /// these zero on every frame; the codec config is the authority.
    pub fn source_size(&self) -> Option<(u32, u32)> {
        (self.width_source > 0 && self.height_source > 0)
            .then_some((self.width_source, self.height_source))
    }
}

/// Records what the first frame of a session carries as its presentation
/// stamp, next to the local clock in the same units. Whether a sender's NTP
/// field is a wall clock at all decides if an absolute latency figure means
/// anything, and only the wire can say.
pub fn trace_frame(pts_us: u64, arrived_us: u64) {
    let unix_ntp_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_micros() as u64 + 2_208_988_800_000_000)
        .unwrap_or(0);
    crate::airplay::session::tracef(format_args!(
        "mirror first pts {pts_us} (local NTP {unix_ntp_us}, age {} ms), monotonic arrival {arrived_us} us",
        (unix_ntp_us as i64 - pts_us as i64) / 1000
    ));
}

fn real_at(bytes: &[u8], at: usize) -> u32 {    let value = f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    if value.is_finite() && value > 0.0 {
        value as u32
    } else {
        0
    }
}

/// The AES-CTR stream for one mirror connection.
pub struct MirrorCipher {
    cipher: AesCtr,
    next_decrypt: usize,
}

impl MirrorCipher {
    pub fn new(aes_key: &[u8], ecdh_shared: &[u8], connection_id: &str) -> CastResult<Self> {
        let (key, iv) = stream_keys(aes_key, ecdh_shared, connection_id)?;
        Ok(Self {
            cipher: AesCtr::new(&key, &iv)?,
            next_decrypt: 0,
        })
    }

    pub fn decrypt(&mut self, data: &mut [u8]) -> CastResult<()> {
        self.next_decrypt += data.len();
        self.cipher.apply_keystream(data)
    }

    pub fn decrypted_bytes(&self) -> usize {
        self.next_decrypt
    }
}

/// The key schedule, exposed so a test can check it against an independent
/// implementation without opening a socket.
pub fn stream_keys(
    aes_key: &[u8],
    ecdh_shared: &[u8],
    connection_id: &str,
) -> CastResult<([u8; 16], [u8; 16])> {
    if aes_key.len() != 16 {
        return Err(CastError::Malformed("mirror aes key is not 16 bytes"));
    }
    if ecdh_shared.len() != 32 {
        return Err(CastError::Malformed("mirror shared secret is not 32 bytes"));
    }
    let mut hasher = Sha512::new();
    hasher.update(aes_key);
    hasher.update(ecdh_shared);
    let eaes = hasher.finalize();

    let key = labelled_key("AirPlayStreamKey", connection_id, &eaes);
    let iv = labelled_key("AirPlayStreamIV", connection_id, &eaes);
    Ok((key, iv))
}

fn labelled_key(label: &str, connection_id: &str, eaes: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label.as_bytes());
    hasher.update(connection_id.as_bytes());
    hasher.update(&eaes[..16]);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// What one AVCC pass learned about an access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvccInfo {
    /// NAL units converted; zero means the payload was not AVCC.
    pub units: usize,
    /// The unit contains an IDR, so a decoder can start here without history.
    pub has_idr: bool,
}

/// Rewrites 4-byte AVCC length prefixes as Annex-B start codes.
///
/// IDR detection rides along in the same pass: the NAL header is the byte
/// straight after the length being overwritten, and a second byte-wide scan of a
/// 200 KB frame is cost paid on the socket thread while the next frame queues.
pub fn avcc_to_annex_b(data: &mut [u8]) -> AvccInfo {
    let mut offset = 0usize;
    let mut info = AvccInfo { units: 0, has_idr: false };
    while offset + 4 <= data.len() {
        let length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if length == 0 || offset + 4 + length > data.len() {
            break;
        }
        info.has_idr |= data[offset + 4] & 0x1f == 5;
        data[offset..offset + 4].copy_from_slice(&[0, 0, 0, 1]);
        offset += 4 + length;
        info.units += 1;
    }
    info
}

/// NAL unit types in an Annex-B access unit, in wire order. Emulation-prevention
/// bytes make `00 00 01` impossible inside a NAL payload, so a byte scan cannot
/// invent a start code.
pub fn nal_types(data: &[u8]) -> Vec<u32> {
    let mut types = Vec::new();
    let mut at = 0usize;
    while at + 3 <= data.len() {
        let three = data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1;
        let four = !three
            && data[at] == 0
            && data[at + 1] == 0
            && data[at + 2] == 0
            && at + 3 < data.len()
            && data[at + 3] == 1;
        if !three && !four {
            at += 1;
            continue;
        }
        let header_at = at + if four { 4 } else { 3 };
        match data.get(header_at) {
            Some(header) => types.push(u32::from(header & 0x1f)),
            None => break,
        }
        at = header_at + 1;
    }
    types
}

/// Turns the codec-config payload into Annex-B SPS+PPS.
pub fn codec_config(data: &[u8]) -> CastResult<Vec<u8>> {
    if data.len() < 8 {
        return Err(CastError::Malformed("codec config is short"));
    }
    let sps_length = u16::from_be_bytes([data[6], data[7]]) as usize;
    let sps_end = 8usize
        .checked_add(sps_length)
        .filter(|end| *end <= data.len())
        .ok_or(CastError::Malformed("codec config sps length"))?;
    let sps = &data[8..sps_end];

    let count_at = sps_end;
    if count_at >= data.len() || data[count_at] == 0 {
        return Err(CastError::Malformed("codec config has no pps"));
    }
    if count_at + 3 > data.len() {
        return Err(CastError::Malformed("codec config pps length is missing"));
    }
    let pps_length = u16::from_be_bytes([data[count_at + 1], data[count_at + 2]]) as usize;
    let pps_end = count_at
        .checked_add(3)
        .and_then(|start| start.checked_add(pps_length))
        .filter(|end| *end <= data.len())
        .ok_or(CastError::Malformed("codec config pps length"))?;
    let pps = &data[count_at + 3..pps_end];

    if sps.is_empty() || pps.is_empty() || sps.len() + pps.len() > MAX_SPS_PPS {
        return Err(CastError::Malformed("codec config parameter sets"));
    }
    let mut out = Vec::with_capacity(8 + sps.len() + pps.len());
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(sps);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(pps);
    Ok(out)
}

/// What the session passes to [`MirrorListener::start`].
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Preferred port; zero asks the OS.
    pub port: u16,
    pub aes_key: Vec<u8>,
    pub ecdh_shared: Vec<u8>,
    /// `streamConnectionID` exactly as SETUP received it, in decimal.
    pub connection_id: String,
}

/// Accepts mirror connections until stopped. Each connection is served on its
/// own thread, so a reconnect does not block the next sender.
pub struct MirrorListener {
    port: u16,
    stop: Arc<AtomicBool>,
    stats: Arc<MirrorStats>,
    /// A sender that takes the port from a session-level SETUP reply and never
    /// sends a stream SETUP leaves the connection id unknowable until the first
    /// socket arrives, so the value stays mutable until the session ends.
    connection_id: Arc<Mutex<String>>,
    thread: Option<JoinHandle<()>>,
}

impl MirrorListener {
    pub fn start(config: MirrorConfig, sink: Arc<dyn FrameSink>) -> CastResult<Self> {
        // Fail before binding if the key material cannot produce a cipher; a
        // listener that accepts and then cannot decrypt only confuses a sender.
        let _ = MirrorCipher::new(&config.aes_key, &config.ecdh_shared, &config.connection_id)?;

        let listener = TcpListener::bind(("0.0.0.0", config.port)).map_err(|_| CastError::Io)?;
        listener.set_nonblocking(true).map_err(|_| CastError::Io)?;
        let port = listener.local_addr().map_err(|_| CastError::Io)?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(MirrorStats::default());
        let connection_id = Arc::new(Mutex::new(config.connection_id.clone()));
        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        let thread_connection = Arc::clone(&connection_id);
        let thread = thread::Builder::new()
            .name("airplay-mirror".to_string())
            .spawn(move || {
                accept_loop(
                    listener,
                    config,
                    thread_connection,
                    sink,
                    thread_stats,
                    thread_stop,
                )
            })
            .map_err(|_| CastError::Io)?;
        Ok(Self {
            port,
            stop,
            stats,
            connection_id,
            thread: Some(thread),
        })
    }

    pub fn local_port(&self) -> u16 {
        self.port
    }

    /// Adopts the id from a later stream SETUP. Frames are only decrypted once a
    /// connection arrives, so this still takes effect for a sender that dials
    /// after answering the media SETUP.
    pub fn set_connection_id(&self, connection_id: &str) {
        if let Ok(mut current) = self.connection_id.lock() {
            if current.as_str() != connection_id {
                *current = connection_id.to_string();
            }
        }
    }

    pub fn stats(&self) -> Arc<MirrorStats> {
        Arc::clone(&self.stats)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for MirrorListener {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: TcpListener,
    config: MirrorConfig,
    connection_id: Arc<Mutex<String>>,
    sink: Arc<dyn FrameSink>,
    stats: Arc<MirrorStats>,
    stop: Arc<AtomicBool>,
) {
    let parameter_sets = Arc::new(Mutex::new(None::<Vec<u8>>));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let id = connection_id
                    .lock()
                    .map(|id| id.clone())
                    .unwrap_or_default();
                crate::airplay::session::tracef(format_args!(
                    "mirror stream connected by {peer} (connection id {id})"
                ));
                stats.connections.fetch_add(1, Ordering::Relaxed);
                serve(
                    stream,
                    &config,
                    &id,
                    &sink,
                    &stats,
                    &parameter_sets,
                    &stop,
                );
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn serve(
    mut stream: TcpStream,
    config: &MirrorConfig,
    connection_id: &str,
    sink: &Arc<dyn FrameSink>,
    stats: &Arc<MirrorStats>,
    parameter_sets: &Arc<Mutex<Option<Vec<u8>>>>,
    stop: &Arc<AtomicBool>,
) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let Ok(mut cipher) = MirrorCipher::new(&config.aes_key, &config.ecdh_shared, connection_id)
    else {
        return;
    };

    let mut header = [0u8; HEADER_LENGTH];
    while !stop.load(Ordering::Relaxed) {
        let Some(header_length) = read_full(&mut stream, &mut header, stop) else {
            return;
        };
        if header_length < HEADER_LENGTH {
            return;
        }
        // An HTTP request here means the sender resolved the wrong port. There
        // is no framing to resynchronise on, so close instead of guessing.
        if header.starts_with(b"POST") || header.starts_with(b"GET") {
            crate::airplay::session::trace_body("mirror-http", &header[..64]);
            return;
        }
        let header = match FrameHeader::parse(&header) {
            Ok(header) => header,
            Err(error) => {
                crate::airplay::session::tracef(format_args!(
                    "mirror stream header rejected ({error:?})"
                ));
                crate::airplay::session::trace_body("mirror-header", &header[..64]);
                return;
            }
        };
        let size = header.size as usize;
        if size == 0 {
            continue;
        }
        if size > MAX_PAYLOAD {
            return;
        }

        let mut payload = vec![0u8; size];
        match read_full(&mut stream, &mut payload, stop) {
            Some(length) if length == size => {}
            _ => return,
        }

        match header.payload_type {
            PAYLOAD_CODEC => {
                if let Ok(sets) = codec_config(&payload) {
                    if let Ok(mut slot) = parameter_sets.lock() {
                        *slot = Some(sets);
                    }
                    let width = if header.width_source > 0 {
                        header.width_source
                    } else {
                        header.width
                    };
                    let height = if header.height_source > 0 {
                        header.height_source
                    } else {
                        header.height
                    };
                    if width > 0 && height > 0 {
                        stats.width.store(width as u64, Ordering::Relaxed);
                        stats.height.store(height as u64, Ordering::Relaxed);
                    }
                    crate::airplay::session::tracef(format_args!(
                        "mirror codec config {width}x{height}"
                    ));
                    stats.codec_configs.fetch_add(1, Ordering::Relaxed);
                }
            }
            PAYLOAD_VIDEO => {
                if cipher.decrypt(&mut payload).is_err() {
                    crate::airplay::session::tracef(format_args!(
                        "mirror frame failed to decrypt ({} bytes)",
                        header.size
                    ));
                    return;
                }
                let info = avcc_to_annex_b(&mut payload);
                if info.units == 0 {
                    continue;
                }
                if stats.frames.load(Ordering::Relaxed) == 0 {
                    crate::airplay::session::tracef(format_args!(
                        "mirror first video frame {} bytes, NALs {:?}",
                        payload.len(),
                        nal_types(&payload)
                    ));
                }
                // An access unit may open with a delimiter or the parameter
                // sets, so the IDR can be anywhere in it. macOS puts it past
                // the first NAL, and reading only byte 4 reports zero key
                // frames for a stream that is otherwise fine.
                let keyframe = info.has_idr;
                if keyframe {
                    let sets = parameter_sets
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(sets) = sets {
                        let mut together = Vec::with_capacity(sets.len() + payload.len());
                        together.extend_from_slice(&sets);
                        together.extend_from_slice(&payload);
                        payload = together;
                    }
                }
                let (width, height) = match header.source_size() {
                    Some(size) => size,
                    None => (
                        stats.width.load(Ordering::Relaxed) as u32,
                        stats.height.load(Ordering::Relaxed) as u32,
                    ),
                };
                stats.frames.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                stats.pts_us.store(header.pts_us(), Ordering::Relaxed);
                if keyframe {
                    stats.keyframes.fetch_add(1, Ordering::Relaxed);
                }
                sink.on_frame(VideoFrame {
                    data: payload,
                    pts_us: header.pts_us(),
                    width,
                    height,
                    keyframe,
                    wallclock: false,
                });
            }
            _ => {}
        }
    }
}

/// Reads until `buffer` is full or the peer closes. Returns `None` when the
/// connection is gone, and the filled length when a timeout left it partial (the
/// caller treats that as a broken frame).
fn read_full(stream: &mut TcpStream, buffer: &mut [u8], stop: &Arc<AtomicBool>) -> Option<usize> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return None,
            Ok(read) => filled += read,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                // A timeout mid-header is not fatal: mobile senders pause when
                // the screen is idle. Keep waiting unless the listener stopped.
                continue;
            }
            Err(_) => return None,
        }
    }
    Some(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration as StdDuration;

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn test_config() -> MirrorConfig {
        MirrorConfig {
            port: 0,
            aes_key: (1u8..=16).collect(),
            ecdh_shared: (0..32u32).map(|index| (index * 7 + 3) as u8).collect(),
            connection_id: "1234567890123".to_string(),
        }
    }

    fn codec_payload() -> Vec<u8> {
        // version, profile, compat, level, reserved, reserved+cnt, SPS length,
        // SPS, PPS count, PPS length, PPS.
        vec![
            1, 0x42, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x03, 0x67, 0x42, 0x00, 0x01, 0x00, 0x02,
            0x68, 0xce,
        ]
    }

    fn header(payload_type: u16, size: usize, ntp: u64) -> [u8; HEADER_LENGTH] {
        let mut header = [0u8; HEADER_LENGTH];
        header[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        header[4..6].copy_from_slice(&payload_type.to_le_bytes());
        header[8..16].copy_from_slice(&ntp.to_le_bytes());
        header
    }

    #[test]
    fn the_key_schedule_matches_an_independent_implementation() {
        let (key, iv) = stream_keys(
            &(1u8..=16).collect::<Vec<_>>(),
            &(0..32u32).map(|index| (index * 7 + 3) as u8).collect::<Vec<_>>(),
            "1234567890123",
        )
        .unwrap();
        // Node: SHA-512 over the same labels and halves.
        assert_eq!(key.to_vec(), hex("e284629303417c44525c2baa1c8e5f61"));
        assert_eq!(iv.to_vec(), hex("92da9f54c51a88671f4a0c952fc69933"));
    }

    #[test]
    fn a_header_is_little_endian_and_pts_is_microseconds() {
        let mut bytes = header(PAYLOAD_CODEC, 16, 0);
        bytes[40..44].copy_from_slice(&1920.0f32.to_le_bytes());
        bytes[44..48].copy_from_slice(&1080.0f32.to_le_bytes());
        bytes[56..60].copy_from_slice(&1280.0f32.to_le_bytes());
        bytes[60..64].copy_from_slice(&720.0f32.to_le_bytes());
        let ntp = (1234u64 << 32) | 0x8000_0000;
        bytes[8..16].copy_from_slice(&ntp.to_le_bytes());

        let parsed = FrameHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.size, 16);
        assert_eq!(parsed.payload_type, PAYLOAD_CODEC);
        assert_eq!(parsed.width_source, 1920);
        assert_eq!(parsed.height_source, 1080);
        assert_eq!(parsed.width, 1280);
        assert_eq!(parsed.height, 720);
        assert_eq!(parsed.pts_us(), 1_234_500_000);
    }

    #[test]
    fn avcc_lengths_become_start_codes() {
        let mut payload = hex("000000056501020304000000074105060708090a");
        assert_eq!(
            avcc_to_annex_b(&mut payload),
            AvccInfo { units: 2, has_idr: true }
        );
        assert_eq!(
            payload,
            hex("000000016501020304000000014105060708090a")
        );
        let mut not_avcc = hex("ffffffff00000000");
        assert_eq!(avcc_to_annex_b(&mut not_avcc).units, 0);
    }

    /// macOS opens an access unit with an AUD or the parameter sets, so the IDR
    /// is never the first unit. A single-pass scan that only read byte 4 reported
    /// a healthy stream as having no key frames at all.
    #[test]
    fn a_late_idr_is_still_reported_as_a_key_frame() {
        let mut payload = hex("0000000209f0000000046742000a00000003658884");
        assert_eq!(
            avcc_to_annex_b(&mut payload),
            AvccInfo { units: 3, has_idr: true }
        );
        let mut inter = hex("0000000209f00000000441050607");
        assert_eq!(
            avcc_to_annex_b(&mut inter),
            AvccInfo { units: 2, has_idr: false }
        );
    }

    #[test]
    fn the_codec_config_yields_sps_and_pps_in_annex_b() {
        let config = codec_config(&codec_payload()).unwrap();
        assert_eq!(
            config,
            hex("000000016742000000000168ce")
        );
        assert!(codec_config(&codec_payload()[..7]).is_err());
        let mut no_pps = codec_payload();
        no_pps[11] = 0;
        assert!(codec_config(&no_pps).is_err());
    }

    #[test]
    fn a_scripted_sender_streams_one_key_frame() {
        let config = test_config();
        let (sender, receiver) = mpsc::channel::<VideoFrame>();
        let sink = Arc::new(move |frame: VideoFrame| {
            let _ = sender.send(frame);
        });
        let listener = MirrorListener::start(config.clone(), sink).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", listener.local_port())).unwrap();

        // Codec config first, as a real sender does.
        let codec = codec_payload();
        let mut codec_header = header(PAYLOAD_CODEC, codec.len(), 0);
        codec_header[40..44].copy_from_slice(&1920.0f32.to_le_bytes());
        codec_header[44..48].copy_from_slice(&1080.0f32.to_le_bytes());
        stream.write_all(&codec_header).unwrap();
        stream.write_all(&codec).unwrap();
        // Then an IDR: AVCC whose access unit opens with an access unit
        // delimiter, exactly as a macOS sender does.
        let units: [&[u8]; 2] = [&[0x09], &[0x65, 1, 2, 3, 4]];
        let mut avcc = Vec::new();
        for nalu in units {
            avcc.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nalu);
        }
        let (key, iv) =
            stream_keys(&config.aes_key, &config.ecdh_shared, &config.connection_id).unwrap();
        let mut cipher = AesCtr::new(&key, &iv).unwrap();
        let encrypted = cipher.process(&avcc).unwrap();
        let ntp = (77u64 << 32) | 0x4000_0000;
        stream
            .write_all(&header(PAYLOAD_VIDEO, encrypted.len(), ntp))
            .unwrap();
        stream.write_all(&encrypted).unwrap();

        let frame = receiver
            .recv_timeout(StdDuration::from_secs(5))
            .expect("a frame reaches the sink");
        assert_eq!(
            frame.data,
            hex("000000016742000000000168ce0000000109000000016501020304")
        );
        assert!(frame.keyframe);
        assert_eq!(frame.pts_us, 77 * 1_000_000 + 250_000);
        assert_eq!((frame.width, frame.height), (1920, 1080));

        let status = listener.stats().snapshot();
        assert_eq!(status.connections, 1);
        assert_eq!(status.codec_configs, 1);
        assert_eq!(status.frames, 1);
        assert_eq!(status.keyframes, 1);
        listener.stop();
    }

    #[test]
    fn a_corrupt_payload_closes_the_connection_without_a_frame() {
        let config = test_config();
        let (sender, receiver) = mpsc::channel::<VideoFrame>();
        let sink = Arc::new(move |frame: VideoFrame| {
            let _ = sender.send(frame);
        });
        let listener = MirrorListener::start(config, sink).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", listener.local_port())).unwrap();
        // A payload whose length prefix points past its own end is not AVCC.
        stream
            .write_all(&header(PAYLOAD_VIDEO, 8, 0))
            .unwrap();
        stream.write_all(&[0, 0, 0, 16, 0x65, 0, 0, 0]).unwrap();
        assert!(receiver.recv_timeout(StdDuration::from_millis(500)).is_err());
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        listener.stop();
    }
}
