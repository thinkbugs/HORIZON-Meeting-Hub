//! The AirPlay 1 receiver session: RTSP on TCP, audio RTP on UDP, PCM out.
//!
//! ## Shape of a session
//!
//! One TCP connection carries the whole conversation. `OPTIONS` proves the
//! receiver is alive and may carry `Apple-Challenge`; `ANNOUNCE` names the codec
//! and the wrapped AES key; `SETUP` asks for the three UDP ports the receiver
//! will listen on (audio, control, timing); `RECORD` starts the stream. From then
//! on the sender pushes RTP packets at an even pace until `FLUSH`, `PAUSE` or
//! `TEARDOWN`.
//!
//! The audio thread owns the UDP sockets and the sink. It decrypts each payload,
//! puts packets back in sequence, converts big-endian L16 to the little-endian
//! PCM every Windows audio API expects, applies the sender's volume, and writes
//! to the sink. RTSP and audio are separate threads because a sender expects its
//! `SET_PARAMETER` volume change to be answered while audio is flowing.
//!
//! ## Timing: what is deliberately missing
//!
//! shairport-sync reconstructs the sender's clock from `0xd4` sync packets and an
//! NTP-like exchange on the timing port, then schedules playback against it.
//! This receiver plays packets as they become contiguous, with a short reorder
//! window and Apple-style `0xd5` resend requests for gaps. On a LAN that is
//! continuous audio at the cost of a few tens of milliseconds of buffering. It
//! does *not* correct clock drift over a long session and it does not honour the
//! sender's absolute latency: after an hour, the sender and receiver clocks will
//! have drifted by whatever the two crystals disagree by. That is a real limit,
//! not a shortcut: fixing it needs the timing loop, not a bigger buffer.
//!
//! The sync and timing sockets are still bound and drained. A sender that finds
//! no timing port falls back to different pacing; refusing to bind one is worse
//! than ignoring its contents. The mirror path is stricter: a sender that
//! negotiated `timingProtocol: NTP` and is told a `timingPort` expects it to
//! answer, so that endpoint runs a real NTP server (see `ensure_timing`) rather
//! than dangling.
//!
//! ## Verified
//!
//! `airplay-probe --selftest` drives a real TCP connection and real UDP sockets
//! with a scripted sender: OPTIONS with a challenge, ANNOUNCE, SETUP, RECORD,
//! five RTP packets, SET_PARAMETER, FLUSH, TEARDOWN. It checks the reply headers,
//! the sequence-ordered PCM and the WAV file on disk. No Apple hardware was
//! reachable, so interop with iTunes or an iPhone remains unverified.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::airplay::crypto::{AesStream, RsaIdentity};
use crate::airplay::fairplay;
use crate::airplay::fp_replies;
use crate::airplay::mdns::{Advert, Responder};
use crate::airplay::mirror::{FrameSink, MirrorConfig, MirrorListener, VideoFrame};
use crate::airplay::pairing::{Identity, Pairing, VerifyReply};
use crate::airplay::plist::Plist;
use crate::airplay::rtsp::{parse_transport, Codec, Request, Response, Sdp};
use crate::discovery::primary_interface;
use crate::error::{CastError, CastResult};

/// AirPlay 1's conventional RTSP port. The advertisement carries the real port,
/// so a busy 5000 is not fatal; it just moves to an ephemeral one.
pub const DEFAULT_RTSP_PORT: u16 = 5000;

/// The mirrored-video data port the model this receiver advertises serves by
/// convention. A sender that never asks for a stream port may dial it directly,
/// so it is bound for the whole session and named in the SETUP reply.
const LEGACY_VIDEO_PORT: u16 = 7100;

#[derive(Clone)]
pub struct Config {
    pub name: String,
    pub bind: Ipv4Addr,
    /// Preferred TCP port. Zero asks the OS for any free port.
    pub port: u16,
    /// The RTSP port actually bound, filled in by [`Receiver::bind`]. The
    /// mirroring handshake echoes it as `timingPort`/`eventPort`.
    pub rtsp_port: u16,
    /// The device id sent in `Apple-Challenge` replies and mDNS. Derived from the
    /// name when absent, because reading the real adapter MAC needs a Windows
    /// API and the value is only used as an identifier.
    pub mac: Option<[u8; 6]>,
    /// Write the session to a WAV file instead of the sound card.
    pub output: Option<PathBuf>,
    /// Do not play and do not write: for the self test.
    pub silent: bool,
    /// Preferred TCP port for the mirror connection; zero asks the OS.
    pub mirror_port: u16,
    /// Where decrypted H.264 frames go. `None` accepts and counts them.
    pub frame_sink: Option<Arc<dyn FrameSink>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "HORIZON Meeting Hub".to_string(),
            bind: Ipv4Addr::UNSPECIFIED,
            port: DEFAULT_RTSP_PORT,
            rtsp_port: 0,
            mac: None,
            output: None,
            silent: false,
            mirror_port: 0,
            frame_sink: None,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("name", &self.name)
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("rtsp_port", &self.rtsp_port)
            .field("mac", &self.mac)
            .field("output", &self.output)
            .field("silent", &self.silent)
            .field("mirror_port", &self.mirror_port)
            .field("frame_sink", &self.frame_sink.is_some())
            .finish()
    }
}

/// Counters shared with the UI and the probe.
#[derive(Default)]
pub struct Stats {
    pub packets: AtomicU64,
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub missing: AtomicU64,
    pub resend_requests: AtomicU64,
    pub resent_packets: AtomicU64,
    pub sync_packets: AtomicU64,
    pub late_drops: AtomicU64,
    /// RAOP volume in millidecibels: 0 is full scale, −144000 is mute.
    pub volume_milli: AtomicI32,
    /// True while a sender's RTSP connection is being served.
    pub in_session: AtomicBool,
    /// The sender completed `/pair-verify` M2.
    pub paired: AtomicBool,
    /// The sender completed the 164-byte `/fp-setup` exchange.
    pub fairplay: AtomicBool,
    /// A mirror TCP listener is running for this session.
    pub mirroring: AtomicBool,
    pub mirror_frames: AtomicU64,
    pub mirror_keyframes: AtomicU64,
    pub mirror_bytes: AtomicU64,
    pub mirror_width: AtomicU64,
    pub mirror_height: AtomicU64,
    /// How old a frame already was when it landed here, measured against the
    /// sender's own NTP capture stamp: the wire half of the end-to-end delay.
    /// Sum and count give the average, max is what the eye catches.
    pub mirror_latency_sum_us: AtomicU64,
    pub mirror_latency_max_us: AtomicU64,
    /// Same, but relative to the first frame of the session, so a clock offset
    /// between the two machines cannot hide a queue that keeps growing.
    pub mirror_drift_sum_us: AtomicU64,
    pub mirror_drift_max_us: AtomicU64,
    /// Longest wait between two arriving frames. A sender that paces at 60 fps
    /// and then stalls for 40 ms is jank, even when the average is fine.
    pub mirror_gap_sum_us: AtomicU64,
    pub mirror_gap_max_us: AtomicU64,
    /// Microseconds this process spends handing each frame onward, which is
    /// where a slow decrypt or an over-copying forwarder shows up.
    pub mirror_pipeline_us: AtomicU64,
}

/// Seconds between the NTP epoch (1900) and the Unix epoch.
const NTP_EPOCH_US: u64 = 2_208_988_800_000_000;

/// Wall clock in microseconds since the Unix epoch, or `None` before 1970.
fn unix_us() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_micros() as u64)
}

/// Microseconds since an arbitrary start, for measuring work rather than time.
fn monotonic_us() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let started = START.get_or_init(std::time::Instant::now);
    started.elapsed().as_micros() as u64
}

fn observe(sum: &AtomicU64, max: &AtomicU64, sample: u64) {
    sum.fetch_add(sample, Ordering::Relaxed);
    max.fetch_max(sample, Ordering::Relaxed);
}


impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            packets: self.packets.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            missing: self.missing.load(Ordering::Relaxed),
            resend_requests: self.resend_requests.load(Ordering::Relaxed),
            resent_packets: self.resent_packets.load(Ordering::Relaxed),
            sync_packets: self.sync_packets.load(Ordering::Relaxed),
            late_drops: self.late_drops.load(Ordering::Relaxed),
            volume_milli: self.volume_milli.load(Ordering::Relaxed),
            in_session: self.in_session.load(Ordering::Relaxed),
            paired: self.paired.load(Ordering::Relaxed),
            fairplay: self.fairplay.load(Ordering::Relaxed),
            mirroring: self.mirroring.load(Ordering::Relaxed),
            mirror_frames: self.mirror_frames.load(Ordering::Relaxed),
            mirror_keyframes: self.mirror_keyframes.load(Ordering::Relaxed),
            mirror_bytes: self.mirror_bytes.load(Ordering::Relaxed),
            mirror_width: self.mirror_width.load(Ordering::Relaxed) as u32,
            mirror_height: self.mirror_height.load(Ordering::Relaxed) as u32,
            mirror_latency_us: self.mirror_latency_sum_us.load(Ordering::Relaxed),
            mirror_latency_max_us: self.mirror_latency_max_us.load(Ordering::Relaxed),
            mirror_drift_us: self.mirror_drift_sum_us.load(Ordering::Relaxed),
            mirror_drift_max_us: self.mirror_drift_max_us.load(Ordering::Relaxed),
            mirror_gap_us: self.mirror_gap_sum_us.load(Ordering::Relaxed),
            mirror_gap_max_us: self.mirror_gap_max_us.load(Ordering::Relaxed),
            mirror_pipeline_us: self.mirror_pipeline_us.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub packets: u64,
    pub frames: u64,
    pub bytes: u64,
    pub missing: u64,
    pub resend_requests: u64,
    pub resent_packets: u64,
    pub sync_packets: u64,
    pub late_drops: u64,
    pub volume_milli: i32,
    pub in_session: bool,
    pub paired: bool,
    pub fairplay: bool,
    pub mirroring: bool,
    pub mirror_frames: u64,
    pub mirror_keyframes: u64,
    pub mirror_bytes: u64,
    pub mirror_width: u32,
    pub mirror_height: u32,
    /// Microseconds, summed over the frames seen so far: divide by
    /// `mirror_frames` for the average.
    pub mirror_latency_us: u64,
    pub mirror_latency_max_us: u64,
    pub mirror_drift_us: u64,
    pub mirror_drift_max_us: u64,
    pub mirror_gap_us: u64,
    pub mirror_gap_max_us: u64,
    pub mirror_pipeline_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    pub peer: Option<SocketAddr>,
    pub codec: String,
    pub encrypted: bool,
    pub packets: u64,
    pub frames: u64,
    pub seconds: f64,
    pub missing: u64,
    pub resend_requests: u64,
    pub sync_packets: u64,
    pub volume_db: f32,
    pub output: Option<PathBuf>,
    pub paired: bool,
    pub fairplay: bool,
    pub mirror_frames: u64,
    pub mirror_keyframes: u64,
    pub mirror_bytes: u64,
    pub mirror_width: u32,
    pub mirror_height: u32,
    /// Sum of the NTP age of every frame, in microseconds.
    pub mirror_latency_us: u64,
    pub mirror_latency_max_us: u64,
    pub mirror_drift_us: u64,
    pub mirror_drift_max_us: u64,
    pub mirror_gap_max_us: u64,
    pub mirror_pipeline_us: u64,
}

/// Where decoded PCM goes. Samples arrive little-endian because that is what
/// every Windows consumer wants; the wire order never escapes this module.
pub trait AudioSink: Send {
    fn write(&mut self, pcm_le: &[u8]) -> CastResult<()>;
    fn flush(&mut self) -> CastResult<()> {
        Ok(())
    }
    fn finish(&mut self) -> CastResult<()> {
        Ok(())
    }
}

pub struct NullSink;

impl AudioSink for NullSink {
    fn write(&mut self, _pcm_le: &[u8]) -> CastResult<()> {
        Ok(())
    }
}

/// A WAV file sink. The header is written with zero sizes and patched on finish,
/// so a crash leaves a file that still has a plausible header rather than one
/// truncated at the first write.
pub struct WavSink {
    file: File,
    sample_rate: u32,
    channels: u16,
    data_len: u32,
    path: PathBuf,
}

impl WavSink {
    pub fn create(
        path: PathBuf,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
    ) -> CastResult<Self> {
        let mut file = File::create(&path).map_err(|_| CastError::Io)?;
        file.write_all(&wav_header(sample_rate, channels, bits_per_sample, 0))
            .map_err(|_| CastError::Io)?;
        Ok(Self {
            file,
            sample_rate,
            channels,
            data_len: 0,
            path,
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl AudioSink for WavSink {
    fn write(&mut self, pcm_le: &[u8]) -> CastResult<()> {
        self.file.write_all(pcm_le).map_err(|_| CastError::Io)?;
        self.data_len = self.data_len.saturating_add(pcm_le.len() as u32);
        Ok(())
    }

    fn finish(&mut self) -> CastResult<()> {
        self.file.flush().map_err(|_| CastError::Io)?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| CastError::Io)?;
        self.file
            .write_all(&wav_header(self.sample_rate, self.channels, 16, self.data_len))
            .map_err(|_| CastError::Io)?;
        self.file.flush().map_err(|_| CastError::Io)?;
        Ok(())
    }
}

fn wav_header(sample_rate: u32, channels: u16, bits: u16, data_len: u32) -> [u8; 44] {
    let channels = channels.max(1);
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * block_align as u32;
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_len.to_le_bytes());
    header
}

/// Streaming playback through the Windows wave mapper. Four prepared buffers are
/// rotated; a full slot waits for the oldest to finish, which turns the sound
/// card into the clock and keeps the audio thread from running ahead.
#[cfg(windows)]
pub struct WaveOutSink {
    handle: windows::Win32::Media::Audio::HWAVEOUT,
    slots: Vec<Slot>,
    next: usize,
    pending: Vec<u8>,
    chunk: usize,
    closed: bool,
}

#[cfg(windows)]
struct Slot {
    header: Box<windows::Win32::Media::Audio::WAVEHDR>,
    buffer: Box<[u8]>,
    submitted: bool,
}

#[cfg(windows)]
unsafe impl Send for WaveOutSink {}

#[cfg(windows)]
impl WaveOutSink {
    pub fn open(sample_rate: u32, channels: u16) -> CastResult<Self> {
        use windows::Win32::Media::Audio::*;

        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * channels as u32 * 2,
            nBlockAlign: channels * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut handle = HWAVEOUT::default();
        // MMSYSERR_NOERROR is zero; the crate does not export the name.
        let status = unsafe {
            waveOutOpen(
                Some(&mut handle),
                WAVE_MAPPER,
                &format,
                None,
                None,
                CALLBACK_NULL,
            )
        };
        if status != 0 {
            return Err(CastError::Hresult(status as i32));
        }

        let chunk = 8192usize;
        let mut slots = Vec::with_capacity(4);
        for _ in 0..4 {
            let mut buffer = vec![0u8; chunk].into_boxed_slice();
            let mut header = Box::new(WAVEHDR {
                lpData: windows::core::PSTR(buffer.as_mut_ptr()),
                dwBufferLength: 0,
                dwBytesRecorded: 0,
                dwUser: 0,
                dwFlags: 0,
                dwLoops: 0,
                lpNext: std::ptr::null_mut(),
                reserved: 0,
            });
            let status = unsafe {
                waveOutPrepareHeader(
                    handle,
                    &mut *header,
                    std::mem::size_of::<WAVEHDR>() as u32,
                )
            };
            if status != 0 {
                unsafe {
                    let _ = waveOutClose(handle);
                }
                return Err(CastError::Hresult(status as i32));
            }
            slots.push(Slot {
                header,
                buffer,
                submitted: false,
            });
        }
        Ok(Self {
            handle,
            slots,
            next: 0,
            pending: Vec::with_capacity(chunk),
            chunk,
            closed: false,
        })
    }

    fn write_slot(&mut self, bytes: usize) -> CastResult<()> {
        use windows::Win32::Media::Audio::*;

        let slot = &mut self.slots[self.next];
        let deadline = Instant::now() + Duration::from_millis(500);
        while slot.submitted && slot.header.dwFlags & WHDR_DONE == 0 {
            if Instant::now() > deadline {
                return Err(CastError::Timeout);
            }
            thread::sleep(Duration::from_millis(2));
        }
        slot.buffer[..bytes].copy_from_slice(&self.pending[..bytes]);
        slot.header.dwBufferLength = bytes as u32;
        slot.header.dwFlags &= !WHDR_DONE;
        let status = unsafe {
            waveOutWrite(
                self.handle,
                &mut *slot.header,
                std::mem::size_of::<WAVEHDR>() as u32,
            )
        };
        if status != 0 {
            return Err(CastError::Hresult(status as i32));
        }
        slot.submitted = true;
        self.pending.drain(..bytes);
        self.next = (self.next + 1) % self.slots.len();
        Ok(())
    }

    fn close_device(&mut self) {
        use windows::Win32::Media::Audio::*;
        if self.closed {
            return;
        }
        self.closed = true;
        unsafe {
            let _ = waveOutReset(self.handle);
            for slot in &mut self.slots {
                if slot.submitted {
                    let _ = waveOutUnprepareHeader(
                        self.handle,
                        &mut *slot.header,
                        std::mem::size_of::<WAVEHDR>() as u32,
                    );
                    slot.submitted = false;
                }
            }
            let _ = waveOutClose(self.handle);
        }
    }
}

#[cfg(windows)]
impl Drop for WaveOutSink {
    fn drop(&mut self) {
        self.close_device();
    }
}

#[cfg(windows)]
impl AudioSink for WaveOutSink {
    fn write(&mut self, pcm_le: &[u8]) -> CastResult<()> {
        self.pending.extend_from_slice(pcm_le);
        while self.pending.len() >= self.chunk {
            self.write_slot(self.chunk)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> CastResult<()> {
        self.pending.clear();
        unsafe {
            let _ = windows::Win32::Media::Audio::waveOutReset(self.handle);
        }
        Ok(())
    }

    fn finish(&mut self) -> CastResult<()> {
        use windows::Win32::Media::Audio::WHDR_DONE;

        let bytes = self.pending.len();
        if bytes > 0 {
            self.write_slot(bytes)?;
        }
        // Let the queued chunks play out, bounded so a wedged driver cannot hold
        // the session thread forever.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && self
                .slots
                .iter()
                .any(|slot| slot.submitted && slot.header.dwFlags & WHDR_DONE == 0)
        {
            thread::sleep(Duration::from_millis(5));
        }
        self.close_device();
        Ok(())
    }
}

/// The listener plus the advertisement. One receiver owns one RTSP port and one
/// speaker entry; accepting is serial because RAOP senders expect to be the only
/// writer to a speaker.
pub struct Receiver {
    listener: TcpListener,
    advert: Advert,
    /// Kept alive for the lifetime of the receiver; dropping it sends goodbye
    /// records and stops the responder thread.
    _responder: Responder,
    /// The pairing identity advertised as `pk`. Reused for every session so a
    /// sender's verification matches the record it resolved.
    identity: Identity,
    config: Config,
    /// The sender currently connected, if any. Cloned so a stop request from
    /// another thread can shut the session down instead of waiting for TEARDOWN.
    active: std::sync::Mutex<Option<TcpStream>>,
}

impl Receiver {
    pub fn bind(mut config: Config) -> CastResult<Self> {
        let address = if config.bind.is_unspecified() {
            primary_interface()
        } else {
            config.bind
        };
        if config.bind.is_unspecified() {
            config.bind = Ipv4Addr::UNSPECIFIED;
        }
        let listener = match TcpListener::bind((config.bind, config.port)) {
            Ok(listener) => listener,
            // The conventional port is a courtesy, not a requirement: the mDNS
            // record carries whatever port we actually got.
            Err(_) if config.port != 0 => TcpListener::bind((config.bind, 0))?,
            Err(_) => return Err(CastError::Io),
        };
        let port = listener.local_addr()?.port();
        config.rtsp_port = port;
        let mac = config.mac.unwrap_or_else(|| device_mac(&config.name));
        let identity = Identity::random()?;
        let advert = Advert::new(config.name.clone(), port, mac, address)
            .with_public_key(identity.public_key_hex());
        let responder = Responder::start(advert.clone())?;
        Ok(Self {
            listener,
            advert,
            _responder: responder,
            identity,
            config,
            active: std::sync::Mutex::new(None),
        })
    }

    pub fn port(&self) -> u16 {
        self.advert.port
    }

    pub fn advert(&self) -> &Advert {
        &self.advert
    }

    /// Blocks until one sender connects, then serves the whole session.
    pub fn accept(&self, stats: Arc<Stats>) -> CastResult<SessionReport> {
        match self.accept_until(stats, &AtomicBool::new(false))? {
            Some(report) => Ok(report),
            None => Err(CastError::InvalidState("accept loop stopped")),
        }
    }

    /// `accept`, but returns `None` once `stop` is set. The UI needs to be able
    /// to close a receiver that no sender has connected to, so the listener is
    /// polled instead of blocking indefinitely.
    pub fn accept_until(
        &self,
        stats: Arc<Stats>,
        stop: &AtomicBool,
    ) -> CastResult<Option<SessionReport>> {
        self.listener.set_nonblocking(true).map_err(|_| CastError::Io)?;
        let accepted = loop {
            if stop.load(Ordering::Relaxed) {
                self.listener.set_nonblocking(false).ok();
                return Ok(None);
            }
            match self.listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    self.listener.set_nonblocking(false).ok();
                    return Err(error.into());
                }
            }
        };
        self.listener.set_nonblocking(false).ok();
        let (stream, peer) = accepted;
        tracef(format_args!("accepted connection from {peer}"));
        // Windows hands the accepted socket the listener's non-blocking flag, and
        // the listener is non-blocking so `stop` can be polled. Left alone, every
        // read races the sender's first packet: a sender that connects and sends
        // its OPTIONS even a few milliseconds later fails with WouldBlock, which
        // we reported as a 400 and then hung up. macOS does exactly that.
        stream.set_nonblocking(false).map_err(|_| CastError::Io)?;
        // The first request needs a deadline — a sender may connect and never
        // speak. Everything after it stays blocking: RTSP is legitimately silent
        // for the whole length of a mirror session.
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .map_err(|_| CastError::Io)?;
        stream.set_nodelay(true).ok();
        let local_ip = stream
            .local_addr()
            .map(|addr| match addr.ip() {
                std::net::IpAddr::V4(ip) => ip,
                std::net::IpAddr::V6(_) => primary_interface(),
            })
            .unwrap_or_else(|_| primary_interface());
        if let (Ok(clone), Ok(mut active)) = (stream.try_clone(), self.active.lock()) {
            *active = Some(clone);
        }
        let result = (|| {
            let mut session = Session::new(
                stream,
                peer,
                local_ip,
                self.advert.mac,
                self.config.clone(),
                self.identity.clone(),
            )?;
            session.run(stats)
        })();
        if let Ok(mut active) = self.active.lock() {
            *active = None;
        }
        result.map(Some)
    }

    /// Ends the sender's connection, if one is active. Used by app shutdown and
    /// by the UI's stop button; the accept loop then sees the closed stream.
    pub fn disconnect(&self) {
        if let Ok(active) = self.active.lock() {
            if let Some(stream) = active.as_ref() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// One RTSP connection, its audio stream and its mirror connection.
pub struct Session {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    peer: SocketAddr,
    local_ip: Ipv4Addr,
    mac: [u8; 6],
    config: Config,
    rsa: Option<RsaIdentity>,
    sdp: Option<Sdp>,
    aes: Option<AesStream>,
    endpoints: Option<Endpoints>,
    live: Option<LiveStream>,
    volume_db: f32,
    /// The protocol version of the most recent request, echoed in every reply.
    version: String,
    pairing: Pairing,
    /// The 164-byte FairPlay key message from the second `/fp-setup`.
    key_message: Option<Vec<u8>>,
    /// `ekey` from the initial SETUP plist: the FairPlay-wrapped stream key.
    mirror_key: Option<Vec<u8>>,
    mirror: Option<MirrorStream>,
    /// `eiv` from the initial SETUP plist. This receiver does not decode the
    /// mirrored AAC-ELD audio, so the value is kept only for diagnostics.
    _mirror_iv: Option<Vec<u8>>,
    /// `isScreenMirroringSession` from the initial SETUP.
    mirror_session: bool,
    mirror_audio: Option<MirrorAudio>,
    timing: Option<TimingServer>,
    event: Option<EventChannel>,
}

struct Endpoints {
    audio: UdpSocket,
    control: UdpSocket,
    timing: UdpSocket,
    sender_control: SocketAddr,
    /// Bound but unused until the clock-recovery loop exists; kept so the shape
    /// of SETUP matches what senders expect.
    _sender_timing: SocketAddr,
    base_seq: Option<u16>,
}

struct LiveStream {
    stop: Arc<AtomicBool>,
    flush: Arc<AtomicBool>,
    join: Option<JoinHandle<CastResult<AudioReport>>>,
}

struct MirrorStream {
    listener: MirrorListener,
}

/// The AAC-ELD sockets from SETUP stream 96. The packets are drained and
/// discarded: this receiver negotiates mirrored audio but does not decode it.
struct MirrorAudio {
    _control: UdpSocket,
    _data: UdpSocket,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for MirrorAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The UDP timing endpoint named by `timingPort` in the SETUP reply.
///
/// macOS opens a mirror session with `timingProtocol: NTP` and expects a real
/// UDP port back. Answering with the RTSP port (as this receiver used to) gives
/// the sender a timing server that never speaks, and it tears the session down
/// after RECORD. Packets are traced so an unexpected dialect is visible.
struct TimingServer {
    port: u16,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for TimingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The TCP endpoint named by `eventPort` in the SETUP reply.
///
/// macOS dials this as soon as SETUP names it and holds the connection open for
/// the whole session. It carries receiver-to-sender events, which an NTP mirror
/// session never sends — but `eventPort: 0` is not "no channel": senders treat
/// a missing or zero event port as a broken receiver and abandon the session
/// immediately after RECORD. The connection is therefore accepted and kept
/// open even though nothing is ever written to it.
struct EventChannel {
    port: u16,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Counts frames for the UI and hands them to the shell's sink, if any.
struct BridgeSink {
    inner: Option<Arc<dyn FrameSink>>,
    stats: Arc<Stats>,
    /// `monotonic_us` at the previous frame, for the inter-arrival gap.
    previous_us: AtomicU64,
    /// First frame's sender stamp and the local time it arrived. Lateness is
    /// measured against that pair, so an offset between the two clocks — or a
    /// sender whose timestamps are not wall clock at all — cancels out.
    base_pts: AtomicU64,
    base_now: AtomicU64,
    /// The last 100 frames on their own. Cumulative averages hide a stall that
    /// happened a minute ago, and a stall is exactly what the trace is for.
    window: Mutex<MirrorWindow>,
}

#[derive(Default)]
struct MirrorWindow {
    frames: u32,
    gap_sum_us: u64,
    gap_max_us: u64,
    drift_max_us: u64,
    pipeline_sum_us: u64,
    bytes: u64,
}

impl FrameSink for BridgeSink {
    fn on_frame(&self, frame: VideoFrame) {
        let started = monotonic_us();
        let previous = self.previous_us.swap(started, Ordering::Relaxed);
        let gap = if previous == 0 { 0 } else { started - previous };
        if gap > 0 {
            observe(
                &self.stats.mirror_gap_sum_us,
                &self.stats.mirror_gap_max_us,
                gap,
            );
        }
        let mut late = 0u64;
        if frame.pts_us > 0 {
            if self.base_pts.load(Ordering::Relaxed) == 0 {
                self.base_pts.store(frame.pts_us, Ordering::Relaxed);
                self.base_now.store(started, Ordering::Relaxed);
                crate::airplay::mirror::trace_frame(frame.pts_us, started);
            } else {
                let expected = frame
                    .pts_us
                    .saturating_sub(self.base_pts.load(Ordering::Relaxed));
                let actual = started - self.base_now.load(Ordering::Relaxed);
                // How far behind the sender's own timeline this frame landed
                // here: queueing anywhere in the pipeline, in microseconds.
                late = actual.saturating_sub(expected);
                if late < 60_000_000 {
                    observe(
                        &self.stats.mirror_drift_sum_us,
                        &self.stats.mirror_drift_max_us,
                        late,
                    );
                }
            }
            if let Some(now) = unix_us() {
                let age = (now + NTP_EPOCH_US).saturating_sub(frame.pts_us);
                if age < 60_000_000 {
                    observe(
                        &self.stats.mirror_latency_sum_us,
                        &self.stats.mirror_latency_max_us,
                        age,
                    );
                }
            }
        }
        self.stats.mirror_frames.fetch_add(1, Ordering::Relaxed);
        self.stats
            .mirror_bytes
            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        if frame.keyframe {
            self.stats.mirror_keyframes.fetch_add(1, Ordering::Relaxed);
        }
        if frame.width > 0 && frame.height > 0 {
            self.stats
                .mirror_width
                .store(frame.width as u64, Ordering::Relaxed);
            self.stats
                .mirror_height
                .store(frame.height as u64, Ordering::Relaxed);
        }
        let frame_bytes = frame.data.len();
        if let Some(inner) = &self.inner {
            inner.on_frame(frame);
        }
        let pipeline = monotonic_us() - started;
        self.stats.mirror_pipeline_us.fetch_add(pipeline, Ordering::Relaxed);
        let Ok(mut window) = self.window.lock() else {
            return;
        };
        window.frames += 1;
        window.gap_sum_us += gap;
        window.gap_max_us = window.gap_max_us.max(gap);
        window.drift_max_us = window.drift_max_us.max(late);
        window.pipeline_sum_us += pipeline;
        window.bytes += frame_bytes as u64;
        if window.frames >= 100 {
            if trace_enabled() {
                let millis = |micros: u64| micros as f64 / 1000.0;
                tracef(format_args!(
                    "mirror window: {} frames {} KB, gap avg {:.1} ms max {:.1} ms, behind max {:.1} ms, hand-off avg {:.2} ms",
                    window.frames,
                    window.bytes / 1024,
                    millis(window.gap_sum_us / window.frames as u64),
                    millis(window.gap_max_us),
                    millis(window.drift_max_us),
                    millis(window.pipeline_sum_us / window.frames as u64),
                ));
            }
            *window = MirrorWindow::default();
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AudioReport {
    packets: u64,
    frames: u64,
    bytes: u64,
    missing: u64,
    resend_requests: u64,
    resent_packets: u64,
    sync_packets: u64,
    seconds: f64,
}

impl Session {
    pub fn new(
        stream: TcpStream,
        peer: SocketAddr,
        local_ip: Ipv4Addr,
        mac: [u8; 6],
        config: Config,
        identity: Identity,
    ) -> CastResult<Self> {
        let reader = BufReader::new(stream.try_clone().map_err(|_| CastError::Io)?);
        Ok(Self {
            stream,
            reader,
            peer,
            local_ip,
            mac,
            config,
            rsa: None,
            sdp: None,
            aes: None,
            endpoints: None,
            live: None,
            volume_db: 0.0,
            version: "RTSP/1.0".to_string(),
            pairing: Pairing::new(identity),
            key_message: None,
            mirror_key: None,
            _mirror_iv: None,
            mirror_session: false,
            mirror: None,
            mirror_audio: None,
            timing: None,
            event: None,
        })
    }

    pub fn run(&mut self, stats: Arc<Stats>) -> CastResult<SessionReport> {
        stats.in_session.store(true, Ordering::Relaxed);
        tracef(format_args!("session start with {}", self.peer));
        let result = self.run_inner(Arc::clone(&stats));
        if let Err(error) = &result {
            eprintln!("airplay: session with {} ended: {error}", self.peer);
        }
        stats.in_session.store(false, Ordering::Relaxed);
        result
    }

    fn run_inner(&mut self, stats: Arc<Stats>) -> CastResult<SessionReport> {
        let mut report = SessionReport {
            peer: Some(self.peer),
            ..SessionReport::default()
        };
        let mut requests = 0usize;
        loop {
            let request = match Request::read(&mut self.reader) {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(error) => {
                    tracef(format_args!("read of the next request failed: {error}"));
                    // Before the first request there is nothing to answer: a
                    // sender that connected and stayed silent until our deadline
                    // is not a protocol violation, it is just not a session.
                    if requests == 0 {
                        break;
                    }
                    let _ = self.respond(Response::new(400, None));
                    return Err(error);
                }
            };
            if requests == 0 {
                // Only the first request is bounded; after this the sender is
                // known to speak RTSP, and silence is normal.
                self.stream.set_read_timeout(None).ok();
            }
            requests += 1;
            self.version = request.version.clone();
            let cseq = request.cseq();
            let path = request_path(&request.uri);
            tracef(format_args!(
                "<- {} {} {}",
                request.method, request.uri, request.version
            ));
            for (name, value) in &request.headers {
                tracef(format_args!("   {name}: {value}"));
            }
            trace_body("body", &request.body);
            match request.method.as_str() {
                "OPTIONS" => self.options(&request, cseq)?,
                "GET" | "POST" => {
                    if path.eq_ignore_ascii_case("/info") {
                        // AirPlay 1 senders GET it; some POST. Same plist either way.
                        self.info(cseq)?;
                    } else if path.eq_ignore_ascii_case("/pair-setup") && request.method == "POST" {
                        self.pair_setup(cseq)?;
                    } else if path.eq_ignore_ascii_case("/pair-verify") && request.method == "POST" {
                        self.pair_verify(&request, cseq, &stats)?;
                    } else if path.eq_ignore_ascii_case("/fp-setup") && request.method == "POST" {
                        self.fp_setup(&request, cseq, &stats)?;
                    } else if path.eq_ignore_ascii_case("/setup") && request.method == "POST" {
                        // iOS and macOS open the mirror with HTTP framing and the
                        // path as the whole target; the RTSP `SETUP` method is
                        // the RAOP spelling of the same request.
                        self.setup(&request, cseq, &stats)?;
                    } else if path.eq_ignore_ascii_case("/record") && request.method == "POST" {
                        self.record(&request, cseq, &stats)?;
                    } else if path.eq_ignore_ascii_case("/pair-pin-start")
                        || path.eq_ignore_ascii_case("/pair-setup-pin")
                    {
                        // The advert says `pinRequired=false`; a sender that asks
                        // anyway is told plainly that this path does not exist.
                        self.respond(Response::new(404, cseq))?;
                    } else if path.eq_ignore_ascii_case("/feedback")
                        || path.eq_ignore_ascii_case("/setProperty")
                    {
                        self.respond(Response::new(200, cseq))?;
                    } else {
                        self.respond(Response::new(404, cseq))?;
                    }
                }
                "ANNOUNCE" => self.announce(&request, cseq)?,
                "SETUP" => self.setup(&request, cseq, &stats)?,
                "RECORD" => self.record(&request, cseq, &stats)?,
                "SET_PARAMETER" => self.set_parameter(&request, cseq, &stats)?,
                "GET_PARAMETER" => self.get_parameter(&request, cseq)?,
                "FLUSH" | "PAUSE" => {
                    if let Some(live) = &self.live {
                        live.flush.store(true, Ordering::Relaxed);
                    }
                    self.respond(Response::new(200, cseq))?;
                }
                "SETPEERS" => self.respond(Response::new(200, cseq))?,
                "TEARDOWN" => {
                    let mut response = Response::new(200, cseq);
                    response.set_header("Connection", "close");
                    self.respond(response)?;
                    break;
                }
                _ => {
                    self.respond(Response::new(405, cseq))?;
                }
            }
        }

        tracef(format_args!(
            "session with {} saw {requests} request(s)",
            self.peer
        ));
        if let Some(live) = self.live.take() {
            live.stop.store(true, Ordering::Relaxed);
            if let Some(join) = live.join {
                match join.join() {
                    Ok(Ok(audio)) => {
                        report.packets = audio.packets;
                        report.frames = audio.frames;
                        report.seconds = audio.seconds;
                        report.missing = audio.missing;
                        report.resend_requests = audio.resend_requests;
                        report.sync_packets = audio.sync_packets;
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(CastError::InvalidState("audio thread panicked")),
                }
            }
        }
        // Dropping the listener stops its accept loop before the counters are
        // read; the drop can wait one poll interval, which is why it comes first.
        self.mirror = None;
        self.mirror_audio = None;
        self.timing = None;
        self.event = None;
        stats.mirroring.store(false, Ordering::Relaxed);
        self.copy_mirror_stats(&mut report, &stats);
        report.codec = self
            .sdp
            .as_ref()
            .map(|sdp| sdp.encoding.clone())
            .unwrap_or_else(|| report.codec.clone());
        // The AES handle itself is owned by the audio thread once RECORD starts,
        // so the report reads what ANNOUNCE promised rather than the handle.
        report.encrypted = self
            .sdp
            .as_ref()
            .map_or(false, |sdp| sdp.rsa_aes_key.is_some() && sdp.aes_iv.is_some());
        report.volume_db = self.volume_db;
        report.output = self.config.output.clone();
        report.paired = self.pairing.is_verified();
        report.fairplay = self.key_message.is_some();
        Ok(report)
    }

    /// Copies the live mirror counters into the report. The bridge sink updates
    /// them as frames arrive, so they survive the listener's shutdown.
    fn copy_mirror_stats(&self, report: &mut SessionReport, stats: &Arc<Stats>) {
        let snapshot = stats.snapshot();
        report.mirror_frames = snapshot.mirror_frames;
        report.mirror_keyframes = snapshot.mirror_keyframes;
        report.mirror_bytes = snapshot.mirror_bytes;
        report.mirror_width = snapshot.mirror_width;
        report.mirror_height = snapshot.mirror_height;
        report.mirror_latency_us = snapshot.mirror_latency_us;
        report.mirror_latency_max_us = snapshot.mirror_latency_max_us;
        report.mirror_drift_us = snapshot.mirror_drift_us;
        report.mirror_drift_max_us = snapshot.mirror_drift_max_us;
        report.mirror_gap_max_us = snapshot.mirror_gap_max_us;
        report.mirror_pipeline_us = snapshot.mirror_pipeline_us;
        if self.sdp.is_none() && (snapshot.mirror_frames > 0 || self.mirror_session) {
            report.codec = "H.264".to_string();
        }
    }

    fn options(&mut self, request: &Request, cseq: Option<u32>) -> CastResult<()> {
        let mut response = Response::new(200, cseq);
        response.set_header(
            "Public",
            "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
        );
        response.set_header("Audio-Jack-Status", "connected; type=analog");
        if let Some(challenge) = request.header("Apple-Challenge") {
            if let Some(bytes) = crate::airplay::crypto::base64_decode(challenge.trim()) {
                let address = self.local_ip.octets();
                let mac = self.mac;
                match self.identity()?.answer_challenge(&bytes, address, mac) {
                    Ok(answer) => response.set_header("Apple-Response", answer),
                    // A receiver that answers with garbage is worse than one
                    // that does not answer: the sender would reject the session
                    // later with no clue. Omitting the header is a visible
                    // refusal at the point where it happened.
                    Err(error) => eprintln!("airplay: Apple-Challenge failed: {error}"),
                }
            }
        }
        self.respond(response)
    }

    /// `GET /info`: the binary plist a sender reads before it decides to mirror.
    /// The feature bitmap must match the mDNS `features`/`ft` values exactly;
    /// a mismatch sends iOS down the PIN-pairing path this receiver refuses.
    fn info(&mut self, cseq: Option<u32>) -> CastResult<()> {
        let plist = self.info_plist();
        let mut response = Response::new(200, cseq);
        response.set_body("application/x-apple-binary-plist", plist.to_bytes());
        self.respond(response)
    }

    /// The `GET /info` body. A modern sender also asks for this same dictionary
    /// inside the SETUP reply (`combinedGetInfoWithControlSetup`), so it is built
    /// once here and reused by both paths.
    fn info_plist(&self) -> Plist {
        // The sender sizes its mirror encoder from this entry, so the values are
        // the ones a working AirPlay 1 receiver publishes: `refreshRate` is a
        // frame *duration* in seconds (1/60), not a rate, and the geometry is
        // integral. Anything else leaves the sender unable to configure the
        // encoder, and it abandons the session right after RECORD, before it
        // ever opens a data connection.
        let display = Plist::dict()
            .set("rotation", false)
            .set("width", 1920i64)
            .set("height", 1080i64)
            .set("widthPhysical", 0i64)
            .set("heightPhysical", 0i64)
            .set("widthPixels", 1920i64)
            .set("heightPixels", 1080i64)
            .set("refreshRate", 1.0f64 / 60.0f64)
            .set("maxFPS", 60i64)
            .set("overscanned", false)
            .set("uuid", self.display_uuid())
            .set("features", 14i64);
        let audio_format = |kind: i64| {
            Plist::dict()
                .set("type", kind)
                .set("audioInputFormats", 67_108_860i64)
                .set("audioOutputFormats", 67_108_860i64)
        };
        let latency = |kind: i64| {
            Plist::dict()
                .set("type", kind)
                .set("audioType", "default")
                .set("inputLatencyMicros", 0i64)
                .set("outputLatencyMicros", 0i64)
        };
        let plist = Plist::dict()
            .set("features", crate::airplay::mdns::FEATURES as i64)
            .set("name", self.config.name.clone())
            .set("displays", Plist::Array(vec![display]))
            // No `supportedFormats`: an AppleTV3,2 has none, and a sender that
            // reads a `screenStream` mask picks the mirrored audio codec from it.
            // Advertising a mask the sender cannot start aborts the session right
            // after RECORD, before it asks for a stream — UxPlay, which macOS
            // mirrors to, publishes no mask for the same reason.
            .set(
                "audioFormats",
                Plist::Array(vec![audio_format(100), audio_format(101)]),
            )
            .set(
                "audioLatencies",
                Plist::Array(vec![latency(100), latency(101)]),
            )
            .set("vv", 2i64)
            .set("protocolVersion", "1.1")
            .set("manufacturer", "Apple Inc.")
            .set("statusFlags", 68i64)
            .set("initialVolume", 0f64)
            .set("volumeControlType", 0i64)
            .set("passwordRequired", false)
            .set("pinRequired", false)
            .set("keepAliveLowPower", true)
            .set("keepAliveSendStatsAsBody", true)
            .set("sourceVersion", "220.68")
            // Binary, like every real receiver: the sender copies this into its
            // endpoint description and reuses it for pairing.
            .set("pk", self.pairing.identity().public_key().to_vec())
            .set("deviceID", self.mac_colons())
            .set("macAddress", self.mac_colons())
            // Must match the mDNS `model`: this receiver speaks the AirPlay 1
            // setup path, and claiming a newer model makes senders ask for the
            // AirPlay 2 one.
            .set("model", "AppleTV3,2");
        plist
    }

    fn pair_setup(&mut self, cseq: Option<u32>) -> CastResult<()> {
        let mut response = Response::new(200, cseq);
        response.set_body("application/octet-stream", self.pairing.pair_setup());
        self.respond(response)
    }

    fn pair_verify(
        &mut self,
        request: &Request,
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        match self.pairing.pair_verify(&request.body) {
            Ok(VerifyReply::Challenge(body)) => {
                let mut response = Response::new(200, cseq);
                response.set_body("application/octet-stream", body);
                self.respond(response)
            }
            Ok(VerifyReply::Verified) => {
                stats.paired.store(true, Ordering::Relaxed);
                self.respond(Response::new(200, cseq))
            }
            Err(error) => {
                eprintln!("airplay: pair-verify refused: {error}");
                self.respond(Response::new(400, cseq))
            }
        }
    }

    /// `/fp-setup` runs twice: a 16-byte message gets one of four fixed FPLY
    /// replies, and the 164-byte key message is stored for the SETUP unwrap.
    fn fp_setup(
        &mut self,
        request: &Request,
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        if !self.pairing.is_verified() {
            self.respond(Response::new(401, cseq))?;
            return Ok(());
        }
        let body = request.body.as_slice();
        if body.len() < 16 || body[4] != 0x03 {
            self.respond(Response::new(400, cseq))?;
            return Ok(());
        }
        match body.len() {
            16 => {
                let mode = body[14] as usize;
                let Some(reply) = fp_replies::REPLIES.get(mode) else {
                    self.respond(Response::new(400, cseq))?;
                    return Ok(());
                };
                let mut response = Response::new(200, cseq);
                response.set_body("application/octet-stream", reply.to_vec());
                self.respond(response)
            }
            164 => {
                let mut reply = Vec::with_capacity(32);
                reply.extend_from_slice(&fairplay::FP_HEADER);
                reply.extend_from_slice(&body[144..164]);
                self.key_message = Some(body.to_vec());
                stats.fairplay.store(true, Ordering::Relaxed);
                let mut response = Response::new(200, cseq);
                response.set_body("application/octet-stream", reply);
                self.respond(response)
            }
            _ => {
                self.respond(Response::new(400, cseq))?;
                Ok(())
            }
        }
    }

    fn mac_colons(&self) -> String {
        self.mac
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Stable per-receiver display identifier. Senders key their video stream
    /// configuration on it, and an empty one is treated as "no display".
    fn display_uuid(&self) -> String {
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&self.mac);
        bytes[6..].copy_from_slice(&[0x48, 0x4F, 0x52, 0x49, 0x5A, 0x4F, 0x4E, 0x00, 0x00, 0x01]);
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32],
        )
    }

    fn announce(&mut self, request: &Request, cseq: Option<u32>) -> CastResult<()> {
        let sdp = match Sdp::parse(&request.body_text()) {
            Ok(sdp) => sdp,
            Err(_) => {
                self.respond(Response::new(400, cseq))?;
                return Ok(());
            }
        };
        match sdp.codec() {
            Codec::L16 => {}
            Codec::AppleLossless => {
                eprintln!("airplay: refusing Apple Lossless; this receiver decodes L16 only");
                self.respond(Response::new(403, cseq))?;
                return Ok(());
            }
            Codec::Other => {
                eprintln!("airplay: refusing unsupported codec {:?}", sdp.encoding);
                self.respond(Response::new(403, cseq))?;
                return Ok(());
            }
        }

        // Replacing a stream: stop the old one before pointing at the new SDP.
        self.stop_stream()?;
        self.endpoints = None;

        self.aes = match (&sdp.rsa_aes_key, &sdp.aes_iv) {
            (Some(wrapped), Some(iv)) => {
                let key = self.identity()?.unwrap_aes_key(wrapped).map_err(|error| {
                    eprintln!("airplay: the sender's AES key did not unwrap: {error}");
                    CastError::Malformed("raop aes key")
                })?;
                if key.len() != 16 {
                    return Err(CastError::Malformed("raop aes key is not 16 bytes"));
                }
                Some(AesStream::new(&key, iv)?)
            }
            (None, None) => None,
            _ => return Err(CastError::Malformed("half an aes key")),
        };
        self.sdp = Some(sdp);
        let mut response = Response::new(200, cseq);
        response.set_header("Audio-Jack-Status", "connected; type=analog");
        self.respond(response)
    }

    fn setup(
        &mut self,
        request: &Request,
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        // Mirroring SETUP is a binary plist; RAOP SETUP is a Transport header.
        if request.body.starts_with(b"bplist") {
            return self.setup_plist(&request.uri, &request.body, cseq, stats);
        }
        if self.sdp.is_none() {
            self.respond(Response::new(451, cseq))?;
            return Ok(());
        }
        let Some(value) = request.header("Transport") else {
            self.respond(Response::new(461, cseq))?;
            return Ok(());
        };
        let transport = parse_transport(value);
        let (Some(control_port), Some(timing_port)) = (transport.control_port, transport.timing_port)
        else {
            self.respond(Response::new(461, cseq))?;
            return Ok(());
        };

        if self.endpoints.is_none() {
            let audio = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            let control = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            let timing = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            audio.set_read_timeout(Some(Duration::from_millis(200))).ok();
            control.set_nonblocking(true).ok();
            timing.set_nonblocking(true).ok();
            self.endpoints = Some(Endpoints {
                audio,
                control,
                timing,
                sender_control: SocketAddr::new(self.peer.ip(), control_port),
                _sender_timing: SocketAddr::new(self.peer.ip(), timing_port),
                base_seq: None,
            });
        }

        let endpoints = self.endpoints.as_ref().expect("just bound");
        let audio_port = local_port(&endpoints.audio)?;
        let control_port = local_port(&endpoints.control)?;
        let timing_port = local_port(&endpoints.timing)?;
        let mut response = Response::new(200, cseq);
        response.set_header(
            "Transport",
            format!(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
                 control_port={control_port};timing_port={timing_port};\
                 server_port={audio_port}"
            ),
        );
        response.set_header("Session", "1");
        self.respond(response)
    }

    /// The three plist SETUPs of a mirroring session: the initial one carries
    /// `ekey`/`eiv`, then stream 110 names the mirror connection and stream 96
    /// asks where the mirrored audio goes.
    fn setup_plist(
        &mut self,
        uri: &str,
        body: &[u8],
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        let plist = match Plist::parse(body) {
            Ok(plist) => plist,
            Err(error) => {
                eprintln!("airplay: SETUP plist did not parse: {error}");
                self.respond(Response::new(400, cseq))?;
                return Ok(());
            }
        };
        if trace_enabled() {
            let keys = plist
                .as_dict()
                .map(|map| map.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_else(|| "<not a dict>".to_string());
            let types = plist
                .get("streams")
                .and_then(|value| value.as_array())
                .map(|streams| {
                    streams
                        .iter()
                        .filter_map(|stream| stream.get("type").and_then(|kind| kind.as_i64()))
                        .map(|kind| kind.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|| "-".to_string());
            tracef(format_args!("SETUP plist keys [{keys}] streams [{types}]"));
            // Values matter for the handshake dialect (e.g. `et`, the encryption
            // type, and `timingProtocol`), and a SETUP plist is small enough to
            // print whole.
            tracef(format_args!("SETUP plist {plist:?}"));
        }
        // Keys may ride on the stream SETUP instead of a control SETUP of their
        // own: that is the media-first dialect, where the audio stream carries
        // `ekey`/`eiv` and the video stream follows. Both shapes must arm the
        // session, or the mirror listener starts without a cipher.
        if let Some(key) = plist.get("ekey").and_then(|value| value.as_data()) {
            self.mirror_key = Some(key.to_vec());
            // A FairPlay key only ever arrives for a mirroring session; that
            // holds even when the sender omits `isScreenMirroringSession`.
            self.mirror_session = true;
        }
        if let Some(iv) = plist.get("eiv").and_then(|value| value.as_data()) {
            self._mirror_iv = Some(iv.to_vec());
        }
        if let Some(flag) = plist
            .get("isScreenMirroringSession")
            .and_then(|value| value.as_bool())
        {
            self.mirror_session |= flag;
        }
        let Some(streams) = plist.get("streams").and_then(|value| value.as_array()) else {
            // `timingPort` is a UDP NTP endpoint and `eventPort` a TCP event
            // channel, not the RTSP port. A sender that is told zero for either
            // treats the receiver as broken and abandons the session right after
            // RECORD, before it ever asks for a stream port.
            let timing = self.ensure_timing().unwrap_or(0);
            let event = self.ensure_event().unwrap_or(0);
            if let Some(sender_timing) = plist.get("timingPort").and_then(|value| value.as_i64()) {
                // The sender writes its port into an unsigned 16-bit slot that
                // plists wider than 0x7FFF read back as negative.
                self.probe_sender_timing((sender_timing as u64 & 0xFFFF) as u16);
            }
            let combined = plist
                .get("combinedGetInfoWithControlSetup")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            tracef(format_args!(
                "SETUP 1 reply timingPort={timing} eventPort={event} combinedInfo={combined}"
            ));
            // A control-only SETUP asks for the session endpoints and nothing
            // else. Answering it with stream ports the sender never requested
            // is read as the answer to a stream SETUP that was never sent, and
            // the sender abandons the session right after RECORD. Its stream
            // SETUPs arrive on their own connection ids a moment later.
            let mut reply = Plist::dict()
                .set("timingPort", timing as i64)
                .set("eventPort", event as i64);
            // Both endpoints are still brought up now, unadvertised: a sender
            // that dials the conventional mirror port straight after RECORD must
            // find a listener, and the stream SETUPs that follow reuse these.
            let _ = self.ensure_mirror_audio();
            if self.mirror_session {
                match session_connection_id(uri) {
                    Some(id) => {
                        if let Err(error) = self.ensure_mirror(&id, stats) {
                            tracef(format_args!(
                                "SETUP 1 cannot pre-start the mirror stream: {error}"
                            ));
                        }
                    }
                    None => tracef(format_args!(
                        "SETUP 1 url {uri} has no stream connection id"
                    )),
                }
            }
            if combined {
                reply = reply.set("info", self.info_plist());
            }
            if trace_enabled() {
                let keys = reply
                    .as_dict()
                    .map(|map| map.keys().cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                tracef(format_args!("SETUP 1 reply keys [{keys}]"));
            }
            let mut response = Response::new(200, cseq);
            response.set_body("application/x-apple-binary-plist", reply.to_bytes());
            return self.respond(response);
        };

        if streams.is_empty() {
            self.respond(Response::new(400, cseq))?;
            return Ok(());
        }
        // A sender may ask for the video stream and the mirrored audio stream in
        // the same SETUP; the reply must answer each requested type in order, or
        // the sender concludes part of the session was refused.
        let mut replies = Vec::with_capacity(streams.len());
        for stream in streams {
            match stream.get("type").and_then(|value| value.as_i64()) {
                Some(110) => {
                    let Some(connection) = stream
                        .get("streamConnectionID")
                        .and_then(|value| value.as_i64())
                    else {
                        self.respond(Response::new(400, cseq))?;
                        return Ok(());
                    };
                    // The keystream label uses the id as an unsigned decimal string.
                    let connection_id = (connection as u64).to_string();
                    match self.ensure_mirror(&connection_id, stats) {
                        Ok(port) => replies.push(
                            Plist::dict()
                                .set("type", 110i64)
                                .set("dataPort", port),
                        ),
                        Err(error) => {
                            eprintln!("airplay: cannot start the mirror stream: {error}");
                            self.respond(Response::new(500, cseq))?;
                            return Ok(());
                        }
                    }
                }
                Some(96) => match self.ensure_mirror_audio() {
                    Ok((control, data)) => replies.push(
                        Plist::dict()
                            .set("type", 96i64)
                            .set("controlPort", control)
                            .set("dataPort", data),
                    ),
                    Err(error) => {
                        eprintln!("airplay: cannot bind mirrored audio: {error}");
                        self.respond(Response::new(500, cseq))?;
                        return Ok(());
                    }
                },
                other => {
                    eprintln!("airplay: SETUP names unsupported stream {other:?}");
                    self.respond(Response::new(400, cseq))?;
                    return Ok(());
                }
            }
        }
        let reply = Plist::dict().set("streams", Plist::Array(replies));
        let mut response = Response::new(200, cseq);
        response.set_body("application/x-apple-binary-plist", reply.to_bytes());
        self.respond(response)
    }

    /// Starts the mirror listener once per session and returns its port. Every
    /// key it needs must already be present: a listener that accepts and cannot
    /// decrypt would leave the sender waiting forever.
    fn ensure_mirror(&mut self, connection_id: &str, stats: &Arc<Stats>) -> CastResult<u16> {
        if let Some(mirror) = &self.mirror {
            // The session-level reply can only guess the id from the RTSP URL.
            // A stream SETUP carries the authoritative one, and the cipher is
            // built when the socket arrives, so adopting it here still counts.
            mirror.listener.set_connection_id(connection_id);
            return Ok(mirror.listener.local_port());
        }
        let ekey = self
            .mirror_key
            .as_deref()
            .ok_or(CastError::InvalidState("mirror SETUP arrived before ekey"))?;
        let key_message = self
            .key_message
            .as_deref()
            .ok_or(CastError::InvalidState("mirror SETUP arrived before fp-setup"))?;
        let shared = self
            .pairing
            .shared_secret()
            .ok_or(CastError::InvalidState("mirror SETUP arrived before pair-verify"))?;
        let aes_key = fairplay::decrypt_aes_key(key_message, ekey)
            .ok_or(CastError::Malformed("fairplay key unwrap failed"))?;
        let sink: Arc<dyn FrameSink> = Arc::new(BridgeSink {
            inner: self.config.frame_sink.clone(),
            stats: Arc::clone(stats),
            previous_us: AtomicU64::new(0),
            base_pts: AtomicU64::new(0),
            base_now: AtomicU64::new(0),
            window: Mutex::new(MirrorWindow::default()),
        });
        let config = |port| MirrorConfig {
            port,
            aes_key: aes_key.to_vec(),
            ecdh_shared: shared.to_vec(),
            connection_id: connection_id.to_string(),
        };
        // The advertised model serves mirrored video on a fixed port; a sender
        // that never asks for a stream port may dial that one directly.
        let listener = if self.config.mirror_port == 0 {
            MirrorListener::start(config(LEGACY_VIDEO_PORT), Arc::clone(&sink))
                .or_else(|_| MirrorListener::start(config(0), Arc::clone(&sink)))
        } else {
            MirrorListener::start(config(self.config.mirror_port), Arc::clone(&sink))
        }?;
        let port = listener.local_port();
        tracef(format_args!("mirror listener on tcp {port}"));
        self.mirror = Some(MirrorStream { listener });
        stats.mirroring.store(true, Ordering::Relaxed);
        Ok(port)
    }

    /// Binds the two sockets SETUP stream 96 asks about. Their packets are
    /// drained and counted as zero: this receiver has no AAC-ELD decoder, and
    /// the module note says so rather than pretending the audio plays.
    fn ensure_mirror_audio(&mut self) -> CastResult<(u16, u16)> {
        if let Some(audio) = &self.mirror_audio {
            return Ok((local_port(&audio._control)?, local_port(&audio._data)?));
        }
        let control = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let data = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        control
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        data.set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_control = control.try_clone().map_err(|_| CastError::Io)?;
        let thread_data = data.try_clone().map_err(|_| CastError::Io)?;
        let join = thread::Builder::new()
            .name("airplay-mirror-audio".to_string())
            .spawn(move || {
                let mut buffer = [0u8; 2048];
                while !thread_stop.load(Ordering::Relaxed) {
                    let _ = thread_data.recv_from(&mut buffer);
                    let _ = thread_control.recv_from(&mut buffer);
                }
            })
            .map_err(|_| CastError::Io)?;
        let ports = (local_port(&control)?, local_port(&data)?);
        self.mirror_audio = Some(MirrorAudio {
            _control: control,
            _data: data,
            stop,
            join: Some(join),
        });
        Ok(ports)
    }

    /// Binds the UDP NTP endpoint advertised as `timingPort` and serves it for
    /// the rest of the session. A sender that negotiated `timingProtocol: NTP`
    /// times its stream from this exchange; with nothing listening it concludes
    /// the receiver is broken and tears the session down after RECORD.
    fn ensure_timing(&mut self) -> CastResult<u16> {
        if let Some(timing) = &self.timing {
            return Ok(timing.port);
        }
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let port = local_port(&socket)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_socket = socket.try_clone().map_err(|_| CastError::Io)?;
        let join = thread::Builder::new()
            .name("airplay-timing".to_string())
            .spawn(move || {
                let mut buffer = [0u8; 128];
                while !thread_stop.load(Ordering::Relaxed) {
                    let Ok((len, from)) = thread_socket.recv_from(&mut buffer) else {
                        continue;
                    };
                    tracef(format_args!(
                        "timing packet {} bytes from {from}",
                        len
                    ));
                    trace_body("timing", &buffer[..len]);
                    if let Some(reply) = ntp_reply(&buffer[..len]) {
                        let _ = thread_socket.send_to(&reply, from);
                    }
                }
            })
            .map_err(|_| CastError::Io)?;
        tracef(format_args!("timing endpoint listening on udp {port}"));
        self.timing = Some(TimingServer {
            port,
            stop,
            join: Some(join),
        });
        Ok(port)
    }

    /// Legacy mirroring runs its clock on the *sender*: the SETUP carries the
    /// NTP port it serves, and a real receiver probes it to relate the sender's
    /// boot clock to its own. The probes double as a wire-level check that the
    /// timing channel is up before the stream starts.
    fn probe_sender_timing(&self, port: u16) {
        if port == 0 {
            return;
        }
        let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
            return;
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        let target = SocketAddr::new(self.peer.ip(), port);
        let spawned = thread::Builder::new()
            .name("airplay-timing-probe".to_string())
            .spawn(move || {
                for sequence in 0..3u8 {
                    let mut packet = [0u8; 48];
                    packet[0] = 0x23;
                    packet[2] = 6;
                    packet[3] = 0xEC;
                    packet[40..48].copy_from_slice(&ntp_now().to_be_bytes());
                    if socket.send_to(&packet, target).is_err() {
                        continue;
                    }
                    let mut buffer = [0u8; 96];
                    match socket.recv_from(&mut buffer) {
                        Ok((len, from)) => {
                            tracef(format_args!(
                                "timing probe {sequence} answered {len} bytes by {from}"
                            ));
                            trace_body("timing-probe", &buffer[..len]);
                        }
                        Err(error) => tracef(format_args!(
                            "timing probe {sequence} to {target} unanswered: {error}"
                        )),
                    }
                    thread::sleep(Duration::from_millis(120));
                }
            });
        if spawned.is_err() {
            tracef(format_args!("timing probe thread could not start"));
        }
    }

    /// Binds the TCP event channel advertised as `eventPort`, accepting and
    /// holding every connection the sender opens. Nothing is written to it in an
    /// NTP mirror session; the sender only needs to see the channel exist.
    fn ensure_event(&mut self) -> CastResult<u16> {
        if let Some(event) = &self.event {
            return Ok(event.port);
        }
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        listener.set_nonblocking(true).ok();
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("airplay-event".to_string())
            .spawn(move || {
                let mut held: Vec<TcpStream> = Vec::new();
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            tracef(format_args!("event channel connected by {peer}"));
                            stream.set_nodelay(true).ok();
                            // Read the channel so a sender that speaks first on
                            // it shows up in the trace instead of blocking in a
                            // socket buffer we never look at.
                            if let Ok(mut reader) = stream.try_clone() {
                                let _ = reader.set_read_timeout(Some(Duration::from_millis(200)));
                                let _ = thread::Builder::new()
                                    .name("airplay-event-read".to_string())
                                    .spawn(move || {
                                        let mut buffer = [0u8; 2048];
                                        loop {
                                            match reader.read(&mut buffer) {
                                                Ok(0) => {
                                                    tracef(format_args!(
                                                        "event channel closed by {peer}"
                                                    ));
                                                    break;
                                                }
                                                Ok(len) => {
                                                    tracef(format_args!(
                                                        "event channel {len} bytes from {peer}"
                                                    ));
                                                    trace_body("event", &buffer[..len]);
                                                }
                                                Err(error)
                                                    if error.kind()
                                                        == std::io::ErrorKind::WouldBlock
                                                        || error.kind()
                                                            == std::io::ErrorKind::TimedOut => {}
                                                Err(_) => break,
                                            }
                                        }
                                    });
                            }
                            // Held only to keep the channel open; the sender
                            // never expects a reply on it.
                            held.push(stream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                        }
                        Err(_) => break,
                    }
                    // A sender may reconnect (for example after a resume); drop
                    // the previous sockets so they cannot pile up.
                    if held.len() > 8 {
                        held.drain(..held.len() - 8);
                    }
                }
            })
            .map_err(|_| CastError::Io)?;
        tracef(format_args!("event channel listening on tcp {port}"));
        self.event = Some(EventChannel {
            port,
            stop,
            join: Some(join),
        });
        Ok(port)
    }

    fn record(
        &mut self,
        request: &Request,
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        // A duplicate RECORD (some senders retry) keeps the running stream.
        if self.live.is_some() {
            let mut response = Response::new(200, cseq);
            response.set_header("Audio-Latency", "11025");
            response.set_header("Audio-Jack-Status", "connected; type=analog");
            return self.respond(response);
        }
        // Mirroring RECORD starts nothing new: the data connection carries the
        // frames as soon as SETUP answered. The headers are not decorative —
        // the sender parses `Audio-Latency` to schedule the mirrored audio and
        // treats a zero as "no audio clock", which makes it abandon the session.
        //
        // macOS sends RECORD straight after the key SETUP and only then opens
        // the 110 stream, unlike iOS which does the stream SETUP first. A 451
        // here (the RAOP answer when no SDP exists) is fatal to it, so any
        // session the sender declared as mirroring is acknowledged.
        if self.mirror.is_some() || self.mirror_session {
            let mut response = Response::new(200, cseq);
            response.set_header("Audio-Latency", "11025");
            response.set_header("Audio-Jack-Status", "connected; type=analog");
            return self.respond(response);
        }
        let Some(sdp) = self.sdp.clone() else {
            self.respond(Response::new(451, cseq))?;
            return Ok(());
        };
        let Some(mut endpoints) = self.endpoints.take() else {
            self.respond(Response::new(400, cseq))?;
            return Ok(());
        };
        endpoints.base_seq = request.header("RTP-Info").and_then(rtp_info_seq);

        let sink = self.make_sink(&sdp)?;
        let aes = self.aes.take();
        let stop = Arc::new(AtomicBool::new(false));
        let flush = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_flush = Arc::clone(&flush);
        let thread_stats = Arc::clone(stats);
        let join = thread::Builder::new()
            .name("airplay-audio".to_string())
            .spawn(move || {
                audio_loop(
                    endpoints,
                    sdp,
                    aes,
                    sink,
                    thread_stop,
                    thread_flush,
                    thread_stats,
                )
            })
            .map_err(|_| CastError::Io)?;
        self.live = Some(LiveStream {
            stop,
            flush,
            join: Some(join),
        });

        let mut response = Response::new(200, cseq);
        // 11025 frames at 44.1 kHz is RAOP's base latency; senders add their own.
        response.set_header("Audio-Latency", "11025");
        self.respond(response)
    }

    fn make_sink(&self, sdp: &Sdp) -> CastResult<Box<dyn AudioSink>> {
        if self.config.silent {
            return Ok(Box::new(NullSink));
        }
        if let Some(path) = &self.config.output {
            return Ok(Box::new(WavSink::create(
                path.clone(),
                sdp.sample_rate,
                sdp.channels.into(),
                16,
            )?));
        }
        #[cfg(windows)]
        {
            Ok(Box::new(WaveOutSink::open(
                sdp.sample_rate,
                sdp.channels.into(),
            )?))
        }
        #[cfg(not(windows))]
        {
            Err(CastError::Unsupported("no audio output"))
        }
    }

    fn set_parameter(
        &mut self,
        request: &Request,
        cseq: Option<u32>,
        stats: &Arc<Stats>,
    ) -> CastResult<()> {
        let body = request.body_text();
        let text = if body.trim().is_empty() {
            request.header("volume").unwrap_or("").to_string()
        } else {
            body
        };
        if let Some(db) = parse_volume(&text) {
            self.volume_db = db;
            stats
                .volume_milli
                .store((db * 1000.0) as i32, Ordering::Relaxed);
        }
        self.respond(Response::new(200, cseq))
    }

    fn get_parameter(&mut self, request: &Request, cseq: Option<u32>) -> CastResult<()> {
        let mut response = Response::new(200, cseq);
        if request.body_text().contains("volume") {
            response.set_body(
                "text/parameters",
                format!("volume: {:.6}\r\n", self.volume_db),
            );
        }
        self.respond(response)
    }

    fn identity(&mut self) -> CastResult<&RsaIdentity> {
        if self.rsa.is_none() {
            self.rsa = Some(RsaIdentity::raop()?);
        }
        Ok(self.rsa.as_ref().expect("just created"))
    }

    fn respond(&mut self, response: Response) -> CastResult<()> {
        let mut response = response;
        // Real receivers name themselves in every reply, and senders key legacy
        // behaviour off this string; a reply without it is the one shape no
        // working receiver ever sends.
        if response.header("Server").is_none() {
            response.set_header("Server", "AirTunes/220.68");
        }
        tracef(format_args!(
            "-> {} {} ({} body bytes)",
            response.status,
            response.reason,
            response.body.len()
        ));
        response.set_version(self.version.clone());
        response
            .write_to(&mut self.stream)
            .map_err(|_| CastError::Io)
    }

    fn stop_stream(&mut self) -> CastResult<()> {
        if let Some(live) = self.live.take() {
            live.stop.store(true, Ordering::Relaxed);
            if let Some(join) = live.join {
                match join.join() {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Err(CastError::InvalidState("audio thread panicked")),
                }
            }
        }
        Ok(())
    }
}

/// `HORIZON_AIRPLAY_TRACE=1` prints the RTSP exchange to stderr. A sender's
/// specific complaints are never in the response we send — they are in the
/// request it stopped at — and this is the only way to see that without a
/// packet capture on the sender's own machine.
pub(crate) fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("HORIZON_AIRPLAY_TRACE").is_some())
}

pub(crate) fn tracef(args: std::fmt::Arguments<'_>) {
    if !trace_enabled() {
        return;
    }
    let line = format!("airplay trace [{:>8.3}] {args}\n", trace_seconds());
    eprint!("{line}");
    // A windowed app has no console, so the same trace goes to a file when one
    // is asked for. Without this, diagnosing a release build means reading
    // numbers off the panel.
    if let Some(path) = std::env::var_os("HORIZON_AIRPLAY_TRACE_FILE") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Seconds since the first traced line. A sender's decision to abort is
/// usually a timeout, and timeouts are only visible with timestamps.
fn trace_seconds() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Bodies are the interesting part only while they are short: pairing keys and
/// FairPlay messages fit, audio never does.
pub(crate) fn trace_body(label: &str, body: &[u8]) {
    if !trace_enabled() || body.is_empty() {
        return;
    }
    let shown = body.len().min(160);
    let mut hex = String::with_capacity(shown * 3);
    for byte in &body[..shown] {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    if body.len() > shown {
        eprintln!(
            "airplay trace [{:>8.3}]   {label} {} bytes: {hex}…",
            trace_seconds(),
            body.len()
        );
    } else {
        eprintln!(
            "airplay trace [{:>8.3}]   {label} {} bytes: {hex}",
            trace_seconds(),
            body.len()
        );
    }
}

fn local_port(socket: &UdpSocket) -> CastResult<u16> {
    Ok(socket.local_addr()?.port())
}

/// Answers a client NTP request the way the reference receiver does: the
/// client's transmit timestamp goes back as `originate`, and both server
/// timestamps are the moment the reply is built.
fn ntp_reply(packet: &[u8]) -> Option<Vec<u8>> {
    // 48 bytes is the shortest legal NTP packet; a mode of 3 is a client.
    if packet.len() < 48 || packet[0] & 0x07 != 3 {
        return None;
    }
    let now = ntp_now();
    let mut reply = vec![0u8; 48];
    reply[0] = 0x24; // leap 0, version 4, mode 4 (server)
    reply[2] = 6; // stratum 6: an uncalibrated relay clock
    reply[3] = 0xEC; // poll 14, precision -20
    reply[24..32].copy_from_slice(&packet[40..48]);
    reply[32..40].copy_from_slice(&now.to_be_bytes());
    reply[40..48].copy_from_slice(&now.to_be_bytes());
    Some(reply)
}

fn ntp_now() -> u64 {
    const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = unix.as_secs() + NTP_UNIX_OFFSET;
    let fraction = ((unix.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

/// The path part of a request URI. AirPlay senders address both absolute URLs
/// (`rtsp://host:port/info`) and bare paths; `*` is the OPTIONS target.
fn request_path(uri: &str) -> &str {
    let path = match uri.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(at) => &rest[at..],
            None => "/",
        },
        None => uri,
    };
    path.split('?').next().unwrap_or(path)
}

/// The per-session stream connection id: the decimal last path segment of a
/// mirroring SETUP (`rtsp://host:port/16879056225190431574`). The video cipher
/// labels its keystream with it, so a reply that names a video port needs it.
fn session_connection_id(uri: &str) -> Option<String> {
    let segment = request_path(uri).rsplit('/').next()?;
    if segment.is_empty() || !segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(segment.to_string())
}

fn rtp_info_seq(value: &str) -> Option<u16> {
    for part in value.split(';') {
        if let Some(number) = part.trim().strip_prefix("seq=") {
            return number.trim().parse().ok();
        }
    }
    None
}

fn parse_volume(text: &str) -> Option<f32> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("volume:") {
            return value.trim().parse::<f32>().ok().map(|db| db.clamp(-144.0, 0.0));
        }
    }
    None
}

// ------------------------------------------------------------------ rtp

#[derive(Debug, Clone, Copy)]
pub struct RtpPacket<'a> {
    pub seq: u16,
    pub timestamp: u32,
    pub resent: bool,
    pub payload: &'a [u8],
}

/// Parses the two RAOP packet kinds that carry audio. The first byte is the RTP
/// version; the second is `0x60` for audio and `0x56` for a retransmission, with
/// the marker bit possibly set. A retransmission carries four extra bytes (the
/// sequence number and count of the request that asked for it) before the
/// payload.
pub fn parse_rtp(buffer: &[u8]) -> Option<RtpPacket<'_>> {
    if buffer.len() < 12 {
        return None;
    }
    let kind = buffer[1] & 0x7f;
    let (resent, skip) = match kind {
        0x60 => (false, 12),
        0x56 => (true, 16),
        _ => return None,
    };
    if buffer.len() <= skip {
        return None;
    }
    Some(RtpPacket {
        seq: u16::from_be_bytes([buffer[2], buffer[3]]),
        timestamp: u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]),
        resent,
        payload: &buffer[skip..],
    })
}

/// Puts packets back in sequence with a bounded window. It does not wait forever
/// for a lost packet: once `max_pending` packets are held, the oldest is played
/// and the gap is reported.
pub struct Reorderer {
    expected: Option<u16>,
    pending: BTreeMap<u16, Vec<u8>>,
    max_pending: usize,
    skipped: u16,
}

impl Reorderer {
    pub fn new(base: Option<u16>) -> Self {
        Self {
            expected: base,
            pending: BTreeMap::new(),
            max_pending: 8,
            skipped: 0,
        }
    }

    /// Returns the payloads that are now contiguous, oldest first.
    pub fn push(&mut self, seq: u16, payload: Vec<u8>) -> Vec<Vec<u8>> {
        if payload.is_empty() {
            return Vec::new();
        }
        let expected = *self.expected.get_or_insert(seq);
        let distance = seq.wrapping_sub(expected);
        if distance >= 0x8000 {
            // Older than what already played: a duplicate or a very late packet.
            return Vec::new();
        }
        self.pending.entry(seq).or_insert(payload);

        while self.pending.len() > self.max_pending {
            // A hole is not going to fill. Move to the oldest held packet and
            // count the frames that were never heard. One skip per push is
            // enough: the drain below consumes the contiguous run that follows.
            let Some(&next) = self.pending.keys().next() else {
                break;
            };
            let expected = self.expected.unwrap_or(next);
            if next == expected {
                break;
            }
            self.skipped = self.skipped.wrapping_add(next.wrapping_sub(expected));
            self.expected = Some(next);
            break;
        }

        let mut ready = Vec::new();
        while let Some(expected) = self.expected {
            let Some(payload) = self.pending.remove(&expected) else {
                break;
            };
            ready.push(payload);
            self.expected = Some(expected.wrapping_add(1));
        }
        ready
    }

    /// The first hole and how many packets it spans, if any are held behind it.
    pub fn missing(&self) -> Option<(u16, u16)> {
        let expected = self.expected?;
        let first_held = *self.pending.keys().next()?;
        let count = first_held.wrapping_sub(expected);
        if count == 0 || count >= 0x8000 {
            None
        } else {
            Some((expected, count))
        }
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.expected = None;
    }

    pub fn take_skipped(&mut self) -> u16 {
        let skipped = self.skipped;
        self.skipped = 0;
        skipped
    }
}

// -------------------------------------------------------------- audio loop

#[allow(clippy::too_many_arguments)]
fn audio_loop(
    endpoints: Endpoints,
    sdp: Sdp,
    aes: Option<AesStream>,
    mut sink: Box<dyn AudioSink>,
    stop: Arc<AtomicBool>,
    flush: Arc<AtomicBool>,
    stats: Arc<Stats>,
) -> CastResult<AudioReport> {
    let frame_bytes = sdp.frames_per_packet * sdp.channels as usize * 2;
    if frame_bytes == 0 {
        return Err(CastError::Malformed("zero frame size"));
    }
    let mut reorder = Reorderer::new(endpoints.base_seq);
    let mut packet = vec![0u8; 4096];
    let mut pcm = Vec::with_capacity(frame_bytes);
    let started = Instant::now();
    let mut last_resend = Instant::now() - Duration::from_secs(1);
    let mut report = AudioReport::default();
    let mut draining = false;

    loop {
        if !draining && stop.load(Ordering::Relaxed) {
            // TEARDOWN can arrive while audio is still queued. Switch to
            // non-blocking and keep reading until the socket is empty, so the
            // last packets sent before the teardown are not thrown away.
            draining = true;
            endpoints.audio.set_nonblocking(true).ok();
        }

        // The control and timing sockets are drained every loop; their contents
        // only matter for diagnostics (and a future timing loop).
        drain_control(&endpoints.control, &stats, &mut report);
        drain_timing(&endpoints.timing);

        if flush.swap(false, Ordering::Relaxed) {
            reorder.clear();
            sink.flush()?;
        }

        match endpoints.audio.recv_from(&mut packet) {
            Ok((length, _from)) => {
                let Some(rtp) = parse_rtp(&packet[..length]) else {
                    continue;
                };
                if rtp.resent {
                    report.resent_packets += 1;
                    stats.resent_packets.fetch_add(1, Ordering::Relaxed);
                }
                let payload = match &aes {
                    Some(aes) => match aes.decrypt(rtp.payload) {
                        Ok(clear) => clear,
                        Err(_) => continue,
                    },
                    None => rtp.payload.to_vec(),
                };
                if payload.len() < frame_bytes {
                    continue;
                }
                let payload = payload[..frame_bytes].to_vec();
                let played = reorder.push(rtp.seq, payload);
                for clear in played {
                    report.packets += 1;
                    report.frames += sdp.frames_per_packet as u64;
                    report.bytes += clear.len() as u64;
                    stats.packets.fetch_add(1, Ordering::Relaxed);
                    stats.frames.fetch_add(sdp.frames_per_packet as u64, Ordering::Relaxed);
                    stats.bytes.fetch_add(clear.len() as u64, Ordering::Relaxed);
                    if (clear.len() & 1) != 0 {
                        continue;
                    }
                    // The sender can change volume mid-stream; a parameters
                    // update on the RTSP thread must reach the next packet, so
                    // the shared counter is the source of truth.
                    let volume = stats.volume_milli.load(Ordering::Relaxed) as f32 / 1000.0;
                    let gain = gain_for(volume);
                    to_little_endian_gain(&clear, gain, &mut pcm);
                    sink.write(&pcm)?;
                }
                let skipped = reorder.take_skipped() as u64;
                if skipped > 0 {
                    report.missing += skipped;
                    stats.missing.fetch_add(skipped, Ordering::Relaxed);
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if draining {
                    break;
                }
            }
            Err(_) => break,
        }

        if !draining && last_resend.elapsed() >= Duration::from_millis(250) {
            if let Some((first, count)) = reorder.missing() {
                if send_resend(&endpoints.control, endpoints.sender_control, first, count).is_ok() {
                    report.resend_requests += 1;
                    stats.resend_requests.fetch_add(1, Ordering::Relaxed);
                    last_resend = Instant::now();
                }
            }
        }
    }

    sink.finish()?;
    report.seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

fn drain_control(socket: &UdpSocket, stats: &Arc<Stats>, report: &mut AudioReport) {
    let mut buffer = [0u8; 512];
    while let Ok((length, _)) = socket.recv_from(&mut buffer) {
        // 0xd4 is the sender's sync packet: an RTP timestamp and an NTP time.
        if length >= 20 && buffer[1] == 0xd4 {
            report.sync_packets += 1;
            stats.sync_packets.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn drain_timing(socket: &UdpSocket) {
    let mut buffer = [0u8; 128];
    while socket.recv_from(&mut buffer).is_ok() {
        // Reserved for the clock-recovery loop described in the module docs.
    }
}

/// `0xd5` resend request: the sender answers with `0xd6` audio packets carrying
/// the same sequence numbers.
fn send_resend(socket: &UdpSocket, to: SocketAddr, first: u16, count: u16) -> CastResult<()> {
    let mut request = [0u8; 8];
    request[0] = 0x80;
    request[1] = 0xd5;
    request[2..4].copy_from_slice(&1u16.to_be_bytes());
    request[4..6].copy_from_slice(&first.to_be_bytes());
    request[6..8].copy_from_slice(&count.to_be_bytes());
    socket.send_to(&request, to).map_err(|_| CastError::Io)?;
    Ok(())
}

fn gain_for(volume_db: f32) -> f32 {
    if volume_db >= 0.0 {
        1.0
    } else {
        10f32.powf(volume_db / 20.0)
    }
}

/// L16 is big-endian; Windows wants little-endian. Volume is applied here so the
/// sink stays a byte pipe.
fn to_little_endian_gain(be: &[u8], gain: f32, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(be.len());
    for sample in be.chunks_exact(2) {
        let value = i16::from_be_bytes([sample[0], sample[1]]);
        let scaled = if (gain - 1.0).abs() < f32::EPSILON {
            value
        } else {
            (value as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16
        };
        out.extend_from_slice(&scaled.to_le_bytes());
    }
}

fn device_mac(name: &str) -> [u8; 6] {
    // FNV-1a: stable across runs so the speaker does not appear twice when the
    // receiver restarts.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut mac = [0u8; 6];
    for (at, byte) in mac.iter_mut().enumerate() {
        *byte = (hash >> (at * 8)) as u8;
    }
    mac[0] = (mac[0] & 0xfc) | 0x02; // locally administered, unicast
    mac
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_audio_packet_parses_and_a_resend_skips_its_extra_word() {
        let mut packet = vec![0u8; 12 + 1408];
        packet[0] = 0x80;
        packet[1] = 0x60;
        packet[2..4].copy_from_slice(&7u16.to_be_bytes());
        packet[4..8].copy_from_slice(&1234u32.to_be_bytes());
        packet[12] = 0xaa;
        let parsed = parse_rtp(&packet).unwrap();
        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.timestamp, 1234);
        assert!(!parsed.resent);
        assert_eq!(parsed.payload.len(), 1408);

        let mut resend = vec![0u8; 16 + 1408];
        resend[1] = 0xd6; // marker bit set, Apple resend
        resend[2..4].copy_from_slice(&8u16.to_be_bytes());
        resend[16] = 0xbb;
        let parsed = parse_rtp(&resend).unwrap();
        assert!(parsed.resent);
        assert_eq!(parsed.seq, 8);
        assert_eq!(parsed.payload[0], 0xbb);
    }

    #[test]
    fn reordering_holds_a_gap_and_releases_it_in_order() {
        let mut reorder = Reorderer::new(Some(10));
        assert!(reorder.push(12, vec![12]).is_empty());
        assert_eq!(reorder.missing(), Some((10, 2)));
        assert_eq!(reorder.push(10, vec![10]), vec![vec![10]]);
        // 11 is still missing, so 12 stays held.
        assert!(reorder.push(12, vec![12]).is_empty());
        assert_eq!(reorder.push(11, vec![11]), vec![vec![11], vec![12]]);
        assert!(reorder.missing().is_none());
    }

    #[test]
    fn a_gap_that_never_fills_is_skipped_not_awaited_forever() {
        let mut reorder = Reorderer::new(Some(1));
        for seq in 3..=12u16 {
            reorder.push(seq, vec![seq as u8]);
        }
        assert!(reorder.take_skipped() >= 1);
        // Once the window is exceeded, the oldest held packet plays.
        let played = reorder.push(13, vec![13]);
        assert!(!played.is_empty());
    }

    #[test]
    fn volume_and_gain_convert_to_little_endian() {
        let be = [0x00, 0x01, 0xff, 0xff]; // 1, -1
        let mut out = Vec::new();
        to_little_endian_gain(&be, 1.0, &mut out);
        assert_eq!(out, vec![0x01, 0x00, 0xff, 0xff]);
        to_little_endian_gain(&be, 0.5, &mut out);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 0);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 0);
    }

    #[test]
    fn a_wav_header_describes_cd_audio() {
        let header = wav_header(44_100, 2, 16, 1764);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 1800);
        assert_eq!(u16::from_le_bytes(header[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(header[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u32::from_le_bytes(header[28..32].try_into().unwrap()), 176_400);
        assert_eq!(u32::from_le_bytes(header[40..44].try_into().unwrap()), 1764);
    }

    #[test]
    fn rtp_info_and_volume_parse() {
        assert_eq!(rtp_info_seq("rtptime=99;seq=4242"), Some(4242));
        assert_eq!(rtp_info_seq("seq=7"), Some(7));
        assert_eq!(rtp_info_seq("rtptime=1"), None);
        assert_eq!(parse_volume("\r\nvolume: -12.5\r\n"), Some(-12.5));
        assert_eq!(parse_volume("volume: -500"), Some(-144.0));
        assert_eq!(parse_volume("progress: 1/2/3"), None);
    }

    #[test]
    fn a_derived_mac_is_stable_and_local() {
        let first = device_mac("HORIZON Meeting Hub");
        assert_eq!(first, device_mac("HORIZON Meeting Hub"));
        assert_eq!(first[0] & 0x03, 0x02);
        assert_ne!(first, device_mac("another receiver"));
    }

    /// End-to-end: a scripted sender over a real TCP socket and real UDP ports.
    #[test]
    fn a_scripted_sender_completes_a_session_and_writes_pcm() {
        let config = Config {
            name: "Test Receiver".to_string(),
            port: 0,
            silent: false,
            output: Some(std::env::temp_dir().join(format!(
                "horizon-airplay-{}.wav",
                std::process::id()
            ))),
            ..Config::default()
        };
        let wav_path = config.output.clone().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stats = Arc::new(Stats::default());
        let session_stats = Arc::clone(&stats);
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            let mut session = Session::new(
                stream,
                peer,
                Ipv4Addr::LOCALHOST,
                [2, 0, 0, 0, 0, 1],
                config,
                Identity::from_seed([0x11; 32]),
            )
            .unwrap();
            session.run(session_stats)
        });

        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        sender.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(sender.try_clone().unwrap());

        let mut options = Request::new("OPTIONS", "*");
        options.set_header("CSeq", "1");
        options.set_header("Apple-Challenge", "AAECAwQFBgcICQoLDA0ODw==");
        options.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);
        assert!(response.header("Apple-Response").is_some());

        let sdp_text = "v=0\r\no=iTunes 1 0 IN IP4 127.0.0.1\r\ns=iTunes\r\n\
            c=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n\
            a=rtpmap:96 L16/44100/2\r\n";
        let mut announce = Request::new("ANNOUNCE", "rtsp://127.0.0.1/1");
        announce.set_header("CSeq", "2");
        announce.set_header("Content-Type", "application/sdp");
        announce.body = sdp_text.as_bytes().to_vec();
        announce.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);

        let sender_control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sender_timing = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut setup = Request::new("SETUP", "rtsp://127.0.0.1/1");
        setup.set_header("CSeq", "3");
        setup.set_header(
            "Transport",
            format!(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
                 control_port={};timing_port={}",
                sender_control.local_addr().unwrap().port(),
                sender_timing.local_addr().unwrap().port()
            ),
        );
        setup.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);
        let transport = parse_transport(response.header("Transport").unwrap());
        let audio_port = response
            .header("Transport")
            .unwrap()
            .split(';')
            .find_map(|part| part.trim().strip_prefix("server_port="))
            .unwrap()
            .parse::<u16>()
            .unwrap();
        assert!(audio_port > 0);
        assert!(transport.interleaved.is_some());

        // Set the volume before RECORD so every packet in this test carries the
        // same gain; the mid-stream update is covered by the shared counter.
        let mut parameter = Request::new("SET_PARAMETER", "rtsp://127.0.0.1/1");
        parameter.set_header("CSeq", "4");
        parameter.set_header("Content-Type", "text/parameters");
        parameter.body = b"volume: -20.0\r\n".to_vec();
        parameter.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);

        let mut record = Request::new("RECORD", "rtsp://127.0.0.1/1");
        record.set_header("CSeq", "5");
        record.set_header("RTP-Info", "seq=100;rtptime=1000");
        record.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("Audio-Latency"), Some("11025"));

        let audio = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        for offset in 0..5u16 {
            let mut packet = vec![0u8; 12 + 1408];
            packet[1] = 0x60;
            packet[2..4].copy_from_slice(&(100 + offset).to_be_bytes());
            packet[4..8].copy_from_slice(&(1000 + offset as u32 * 352).to_be_bytes());
            for frame in 0..352usize {
                let value = (offset as i16) * 100 + frame as i16;
                packet[12 + frame * 4..12 + frame * 4 + 2]
                    .copy_from_slice(&value.to_be_bytes());
                packet[12 + frame * 4 + 2..12 + frame * 4 + 4]
                    .copy_from_slice(&(-value).to_be_bytes());
            }
            audio
                .send_to(&packet, (Ipv4Addr::LOCALHOST, audio_port))
                .unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        let mut teardown = Request::new("TEARDOWN", "rtsp://127.0.0.1/1");
        teardown.set_header("CSeq", "6");
        teardown.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);
        drop(sender);

        let report = server.join().unwrap().unwrap();
        assert_eq!(report.codec, "L16");
        assert!(!report.encrypted);
        assert_eq!(report.packets, 5);
        assert_eq!(report.frames, 5 * 352);
        assert_eq!(report.volume_db, -20.0);

        let bytes = std::fs::read(&wav_path).unwrap();
        assert_eq!(bytes.len(), 44 + 5 * 1408);
        // Volume −20 dB is a gain of 0.1. Frame 10 carries 10, so it must come
        // out as 1 (and its right channel as −1), little-endian.
        let at = 44 + 10 * 4;
        let left = i16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let right = i16::from_le_bytes([bytes[at + 2], bytes[at + 3]]);
        assert_eq!(left, 1);
        assert_eq!(right, -1);
        let _ = std::fs::remove_file(&wav_path);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.packets, 5);
        assert_eq!(snapshot.volume_milli, -20_000);
    }

    #[test]
    fn alac_is_answered_with_403_and_no_stream() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            silent: true,
            ..Config::default()
        };
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            let mut session =
                Session::new(stream, peer, Ipv4Addr::LOCALHOST, [2, 0, 0, 0, 0, 1], config,
                    Identity::from_seed([0x22; 32]))
                    .unwrap();
            session.run(Arc::new(Stats::default()))
        });
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut reader = BufReader::new(sender.try_clone().unwrap());
        let mut announce = Request::new("ANNOUNCE", "rtsp://127.0.0.1/1");
        announce.set_header("CSeq", "1");
        announce.body = b"v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n".to_vec();
        announce.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 403);
        drop(reader);
        drop(sender);
        let report = server.join().unwrap().unwrap();
        assert!(report.codec.is_empty());
    }

    #[test]
    fn a_stopped_receiver_returns_instead_of_waiting_for_a_sender() {
        let receiver = Receiver::bind(Config {
            name: "Stop Test".to_string(),
            port: 0,
            silent: true,
            ..Config::default()
        })
        .unwrap();
        let receiver = Arc::new(receiver);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_receiver = Arc::clone(&receiver);
        let thread_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            thread_receiver.accept_until(Arc::new(Stats::default()), &thread_stop)
        });
        thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        let result = worker.join().unwrap().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn disconnecting_ends_an_idle_rtsp_session() {
        // A sender connects but never sends a request: without disconnect the
        // accept loop would sit in a blocking read until the socket closes.
        let receiver = Arc::new(
            Receiver::bind(Config {
                name: "Disconnect Test".to_string(),
                port: 0,
                silent: true,
                ..Config::default()
            })
            .unwrap(),
        );
        let port = receiver.port();
        let thread_receiver = Arc::clone(&receiver);
        let stop = Arc::new(AtomicBool::new(false));
        let worker =
            thread::spawn(move || thread_receiver.accept_until(Arc::new(Stats::default()), &stop));
        let sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        thread::sleep(Duration::from_millis(100));
        receiver.disconnect();
        drop(sender);
        // Whether the abrupt close surfaces as `Ok` or an I/O error does not
        // matter; what matters is that `accept_until` is no longer blocked.
        let _ = worker.join().unwrap();
    }

    /// The full mirroring handshake: HTTP-framed `/info`, pairing, both FairPlay
    /// messages, the three plist SETUPs, RECORD, one encrypted H.264 frame and
    /// TEARDOWN. The sender is scripted, so this proves the receiver's own bytes
    /// and key schedule, not interop with an Apple device.
    #[test]
    fn a_scripted_sender_pairs_and_mirrors_one_key_frame() {
        const X_BASE: [u8; 32] = [
            9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];
        let config = Config {
            name: "Mirror Test".to_string(),
            port: 0,
            rtsp_port: 0,
            silent: true,
            ..Config::default()
        };
        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<VideoFrame>();
        let config = Config {
            frame_sink: Some(Arc::new(move |frame: VideoFrame| {
                let _ = frame_tx.send(frame);
            })),
            ..config
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            rtsp_port: port,
            ..config
        };
        let identity = Identity::from_seed([0x44; 32]);
        let public_key = *identity.public_key();
        let stats = Arc::new(Stats::default());
        let session_stats = Arc::clone(&stats);
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            let mut session = Session::new(
                stream,
                peer,
                Ipv4Addr::LOCALHOST,
                [2, 0, 0, 0, 0, 9],
                config,
                identity,
            )
            .unwrap();
            session.run(session_stats)
        });

        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        sender
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(sender.try_clone().unwrap());

        let send = |sender: &mut TcpStream,
                    reader: &mut BufReader<TcpStream>,
                    request: &Request|
         -> Response {
            request.write_to(sender).unwrap();
            let response = Response::read(reader).unwrap().unwrap();
            assert_eq!(
                response.version, "HTTP/1.1",
                "the reply must mirror the request version"
            );
            response
        };
        let http = |method: &str, uri: &str, cseq: u32| {
            let mut request = Request::new(method, uri);
            request.version = "HTTP/1.1".to_string();
            request.set_header("CSeq", cseq.to_string());
            request
        };

        // 1. /info: feature bitmap and the advertised identity.
        let response = send(&mut sender, &mut reader, &http("GET", "/info", 1));
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("Content-Type"),
            Some("application/x-apple-binary-plist")
        );
        let info = Plist::parse(&response.body).expect("info plist");
        assert_eq!(
            info.get("features").and_then(|value| value.as_i64()),
            Some(crate::airplay::mdns::FEATURES as i64)
        );
        // The sender copies `pk` into its endpoint description, so it has to be
        // the raw 32-byte key rather than the hex rendering.
        assert_eq!(
            info.get("pk").and_then(|value| value.as_data()),
            Some(public_key.as_slice())
        );
        let display = info
            .get("displays")
            .and_then(|value| value.as_array())
            .and_then(|displays| displays.first())
            .expect("info advertises one display");
        assert!(matches!(display.get("width"), Some(Plist::Integer(width)) if *width == 1920));
        assert!(matches!(display.get("height"), Some(Plist::Integer(height)) if *height == 1080));
        assert!(
            matches!(display.get("refreshRate"), Some(Plist::Real(rate)) if (*rate - 1.0 / 60.0).abs() < 1e-9),
            "refreshRate is a frame duration, not a rate"
        );
        assert!(display.get("uuid").and_then(|value| value.as_str()).is_some());

        // 2. pair-setup returns the same key.
        let response = send(&mut sender, &mut reader, &http("POST", "/pair-setup", 2));
        assert_eq!(response.status, 200);
        assert_eq!(response.body, public_key.to_vec());

        // 3. pair-verify M1: the receiver answers with its ephemeral key and an
        //    encrypted signature over both ephemeral keys.
        let x_secret = [0x21u8; 32];
        let x_public = crate::airplay::nacl::x25519(&x_secret, &X_BASE);
        let ed_seed = [0x55u8; 32];
        let ed_public = crate::airplay::nacl::ed25519_public_key(&ed_seed);
        let mut request = http("POST", "/pair-verify", 3);
        request.body = [vec![1, 0, 0, 0], x_public.to_vec(), ed_public.to_vec()].concat();
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), 96);
        let mut ecdh_ours = [0u8; 32];
        ecdh_ours.copy_from_slice(&response.body[..32]);
        let shared = crate::airplay::nacl::x25519(&x_secret, &ecdh_ours);
        let key = fairplay::sha512_pair(b"Pair-Verify-AES-Key", &shared);
        let iv = fairplay::sha512_pair(b"Pair-Verify-AES-IV", &shared);
        let mut cipher = crate::airplay::crypto::AesCtr::new(&key[..16], &iv[..16]).unwrap();
        let signature = cipher.process(&response.body[32..96]).unwrap();
        let mut signed = [0u8; 64];
        signed[..32].copy_from_slice(&ecdh_ours);
        signed[32..].copy_from_slice(&x_public);
        assert!(crate::airplay::nacl::ed25519_verify(
            &public_key,
            &signed,
            signature.as_slice().try_into().unwrap()
        ));

        // M2 signs the two keys in the other order through the same keystream.
        let mut answer = [0u8; 64];
        answer[..32].copy_from_slice(&x_public);
        answer[32..].copy_from_slice(&ecdh_ours);
        let own = crate::airplay::nacl::ed25519_sign(&ed_seed, &answer);
        let mut request = http("POST", "/pair-verify", 4);
        request.body = [vec![0, 0, 0, 0], cipher.process(&own).unwrap()].concat();
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        assert!(stats.snapshot().paired);

        // 4. Both FairPlay messages. The 16-byte one selects a fixed reply; the
        //    164-byte one stores the key message and echoes its last 20 bytes.
        let mut request = http("POST", "/fp-setup", 5);
        let mut fp_request = vec![0u8; 16];
        fp_request[4] = 0x03;
        fp_request[14] = 0x01;
        request.body = fp_request;
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), 142);
        assert_eq!(&response.body[..8], b"FPLY\x03\x01\x02\x00");

        let mut key_message: Vec<u8> = (0..164u8).map(|index| index.wrapping_mul(3)).collect();
        key_message[..12].copy_from_slice(&fairplay::FP_HEADER);
        let mut request = http("POST", "/fp-setup", 6);
        request.body = key_message.clone();
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), 32);
        assert_eq!(&response.body[..12], &fairplay::FP_HEADER);
        assert_eq!(&response.body[12..], &key_message[144..164]);
        assert!(stats.snapshot().fairplay);

        // 5. Initial SETUP: ekey/eiv, then the two stream SETUPs.
        let ekey: Vec<u8> = (0..72u8).map(|index| index.wrapping_mul(5).wrapping_add(1)).collect();
        let initial = Plist::dict()
            .set("ekey", ekey.clone())
            .set("eiv", vec![0u8; 16])
            .set("isScreenMirroringSession", true)
            .set("timingPort", 6000i64)
            .to_bytes();
        let mut request = http("POST", "/setup", 7);
        request.body = initial;
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        let reply = Plist::parse(&response.body).expect("initial setup plist");
        let timing_port = reply
            .get("timingPort")
            .and_then(|value| value.as_i64())
            .expect("timingPort") as u16;
        assert!(timing_port > 0, "timingPort must name a real UDP endpoint");
        assert_ne!(timing_port, port, "timingPort is not the RTSP port");
        let event_port = reply
            .get("eventPort")
            .and_then(|value| value.as_i64())
            .expect("eventPort") as u16;
        assert!(event_port > 0, "eventPort must name a live TCP endpoint");
        assert_ne!(event_port, port, "eventPort is not the RTSP port");
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, event_port))
            .expect("event channel accepts connections");

        // The advertised endpoint must actually answer: a sender that times its
        // stream from an NTP server which stays silent aborts the session.
        let clock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        clock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut ntp = [0u8; 48];
        ntp[0] = 0x23; // version 4, mode 3 (client)
        ntp[40..48].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        clock
            .send_to(&ntp, (Ipv4Addr::LOCALHOST, timing_port))
            .unwrap();
        let mut answer = [0u8; 64];
        let (len, _) = clock.recv_from(&mut answer).expect("NTP reply");
        assert_eq!(len, 48);
        assert_eq!(answer[0] & 0x07, 4, "server mode");
        assert_eq!(&answer[24..32], &[1, 2, 3, 4, 5, 6, 7, 8]);

        let connection_id = 987_654_321i64;
        let mirror_request = Plist::dict()
            .set(
                "streams",
                Plist::Array(vec![Plist::dict()
                    .set("type", 110i64)
                    .set("streamConnectionID", connection_id)]),
            )
            .to_bytes();
        let mut request = http("POST", "/setup", 8);
        request.body = mirror_request;
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        let reply = Plist::parse(&response.body).expect("stream 110 plist");
        let mirror_port = reply
            .get("streams")
            .and_then(|value| value.as_array())
            .and_then(|streams| streams.first())
            .and_then(|stream| stream.get("dataPort"))
            .and_then(|value| value.as_i64())
            .expect("dataPort") as u16;
        assert!(mirror_port > 0);

        let audio_request = Plist::dict()
            .set(
                "streams",
                Plist::Array(vec![Plist::dict().set("type", 96i64)]),
            )
            .to_bytes();
        let mut request = http("POST", "/setup", 9);
        request.body = audio_request;
        let response = send(&mut sender, &mut reader, &request);
        assert_eq!(response.status, 200);
        let reply = Plist::parse(&response.body).expect("stream 96 plist");
        let stream = reply
            .get("streams")
            .and_then(|value| value.as_array())
            .and_then(|streams| streams.first())
            .expect("stream");
        assert!(
            stream
                .get("controlPort")
                .and_then(|value| value.as_i64())
                .unwrap_or(0)
                > 0
        );
        assert!(
            stream
                .get("dataPort")
                .and_then(|value| value.as_i64())
                .unwrap_or(0)
                > 0
        );

        // 6. RECORD acknowledges the stream and reports the audio latency the
        //    sender schedules the mirrored audio with.
        let response = send(&mut sender, &mut reader, &http("RECORD", "/record", 10));
        assert_eq!(response.status, 200);
        assert_eq!(response.header("Audio-Latency"), Some("11025"));
        assert_eq!(
            response.header("Audio-Jack-Status"),
            Some("connected; type=analog")
        );

        // 7. Mirror connection: codec config, then one encrypted IDR. The AES
        //    key is what the receiver will derive from the same ekey and key
        //    message, which is the whole point of the FairPlay exchange.
        let aes_key = fairplay::decrypt_aes_key(&key_message, &ekey).expect("fairplay key");
        let (stream_key, stream_iv) = crate::airplay::mirror::stream_keys(
            &aes_key,
            &shared,
            &connection_id.to_string(),
        )
        .unwrap();
        let mut mirror = TcpStream::connect((Ipv4Addr::LOCALHOST, mirror_port)).unwrap();
        let codec = vec![
            1u8, 0x42, 0x00, 0x1e, 0x00, 0x01, 0x00, 0x03, 0x67, 0x42, 0x00, 0x01, 0x00, 0x02,
            0x68, 0xce,
        ];
        let mut codec_header = [0u8; 128];
        codec_header[0..4].copy_from_slice(&(codec.len() as u32).to_le_bytes());
        codec_header[4..6].copy_from_slice(&1u16.to_le_bytes());
        codec_header[40..44].copy_from_slice(&640.0f32.to_le_bytes());
        codec_header[44..48].copy_from_slice(&480.0f32.to_le_bytes());
        mirror.write_all(&codec_header).unwrap();
        mirror.write_all(&codec).unwrap();

        let nalu = vec![0u8, 0, 0, 5, 0x65, 1, 2, 3, 4];
        let mut cipher = crate::airplay::crypto::AesCtr::new(&stream_key, &stream_iv).unwrap();
        let encrypted = cipher.process(&nalu).unwrap();
        let ntp = (1234u64 << 32) | 0x8000_0000;
        let mut video_header = [0u8; 128];
        video_header[0..4].copy_from_slice(&(encrypted.len() as u32).to_le_bytes());
        video_header[4..6].copy_from_slice(&0u16.to_le_bytes());
        video_header[8..16].copy_from_slice(&ntp.to_le_bytes());
        video_header[40..44].copy_from_slice(&640.0f32.to_le_bytes());
        video_header[44..48].copy_from_slice(&480.0f32.to_le_bytes());
        mirror.write_all(&video_header).unwrap();
        mirror.write_all(&encrypted).unwrap();

        let frame = frame_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the decrypted frame reaches the sink");
        assert!(frame.keyframe);
        assert_eq!(frame.pts_us, 1_234_500_000);
        assert_eq!((frame.width, frame.height), (640, 480));
        assert_eq!(
            frame.data,
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0x00, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 1, 2, 3, 4
            ]
        );
        drop(mirror);

        // 8. TEARDOWN ends the whole session.
        let response = send(&mut sender, &mut reader, &http("TEARDOWN", "/teardown", 11));
        assert_eq!(response.status, 200);
        drop(reader);
        drop(sender);

        let report = server.join().unwrap().unwrap();
        assert_eq!(report.codec, "H.264");
        assert!(report.paired);
        assert!(report.fairplay);
        assert_eq!(report.mirror_frames, 1);
        assert_eq!(report.mirror_keyframes, 1);
        assert_eq!((report.mirror_width, report.mirror_height), (640, 480));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.mirror_frames, 1);
        assert!(!snapshot.mirroring);
    }

    #[test]
    fn fairplay_and_pin_paths_refuse_a_sender_that_did_not_pair() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let identity = Identity::from_seed([0x66; 32]);
        let config = Config {
            name: "Guard Test".to_string(),
            port: 0,
            silent: true,
            ..Config::default()
        };
        let server = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            let mut session = Session::new(
                stream,
                peer,
                Ipv4Addr::LOCALHOST,
                [2, 0, 0, 0, 0, 10],
                config,
                identity,
            )
            .unwrap();
            session.run(Arc::new(Stats::default()))
        });
        let mut sender = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        sender
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(sender.try_clone().unwrap());

        let mut request = Request::new("POST", "/fp-setup");
        request.version = "HTTP/1.1".to_string();
        request.set_header("CSeq", "1");
        let mut fp_request = vec![0u8; 16];
        fp_request[4] = 0x03;
        request.body = fp_request;
        request.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 401);

        for (cseq, path) in [(2u32, "/pair-pin-start"), (3, "/pair-setup-pin")] {
            let mut request = Request::new("POST", path);
            request.version = "HTTP/1.1".to_string();
            request.set_header("CSeq", cseq.to_string());
            request.write_to(&mut sender).unwrap();
            let response = Response::read(&mut reader).unwrap().unwrap();
            assert_eq!(response.status, 404, "{path}");
        }

        let mut teardown = Request::new("TEARDOWN", "/teardown");
        teardown.version = "HTTP/1.1".to_string();
        teardown.set_header("CSeq", "4");
        teardown.write_to(&mut sender).unwrap();
        let response = Response::read(&mut reader).unwrap().unwrap();
        assert_eq!(response.status, 200);
        drop(reader);
        drop(sender);
        let report = server.join().unwrap().unwrap();
        assert!(!report.paired);
        assert!(!report.fairplay);
    }
}
