//! The casting session: the two state machines that sit on [`crate::wire`] and
//! [`crate::transport`], plus the sockets they run on.
//!
//! ## One picture in flight
//!
//! A delta is cut against [`crate::wire::WireEncoder`]'s *committed* picture, and
//! that picture may only advance when the peer says it applied the frame. Encoding
//! ahead of the acknowledgement would produce deltas against a base the sender hopes
//! the peer has; if it does not, the receiver can reconstruct nothing and the only
//! repair left is a key frame. So the sender encodes the next frame only after the
//! previous one is acknowledged, and a capture that arrives in the meantime is
//! dropped rather than queued:
//!
//! - at 30 fps a frame has 33 ms, and a LAN round trip is a fraction of a
//!   millisecond, so the wait is almost never taken;
//! - when it is taken, the frame being skipped is already stale. Sending it would
//!   add a frame of latency to everything behind it, which is the one thing this
//!   engine exists to avoid.
//!
//! The same rule is why there is no send queue anywhere in this file. A queue is a
//! latency budget spent on frames nobody will see; dropping is the cheaper answer.
//!
//! ## What the acknowledgement buys
//!
//! `commit` on an Ack, `rollback` on a key request or a timeout. Rollback is exact
//! because nothing was encoded after the rolled-back frame — the encoder's `base`
//! still holds the picture the peer is proven to have, and `staged` only needs the
//! tiles of the one frame undone. That is the same invariant
//! [`crate::wire::WireEncoder::rollback`] documents, and the one-frame rule is what
//! makes it hold.
//!
//! ## Sockets, and why there is no network thread
//!
//! Both link types below are single-threaded: a non-blocking socket is serviced by
//! the same loop that captures and presents. The alternative — a thread per
//! direction with a hand-off queue — buys nothing here and costs the one thing
//! being measured: every queue crossing on the frame path is latency, and the
//! capture loop already wakes 30 times a second. The sender drains its socket at
//! the top of every [`SenderLink::cast`], the receiver during
//! [`ReceiverLink::poll`], and both pace by sleeping in the writer, which is the
//! only place waiting is legitimate.
//!
//! ## Not here yet
//!
//! The frame size is fixed for the life of a session. A display resolution change
//! ends the session and the caller starts a new one with new dimensions; the wire
//! header carries the size so the receiver can say why it refused, but a session
//! that silently reallocated its framebuffer mid-stream would be a presentation
//! problem in P1-C, not a transport one.
//!
//! ## Measured
//!
//! `cast-bench` runs both ends in one process over loopback UDP, in release mode,
//! with QPC stamps on both sides, so `present_us - capture_us` is a real pipeline
//! latency and not an estimate. 1920×1080, 30 fps channel, one frame in flight,
//! against [`crate::metrics::Budget::LOW_LATENCY_1080P30`] (40 ms):
//!
//! ```text
//! live desktop, mostly static        600 frames   p50 0.23 ms  p95 2.28 ms  p99 4.98 ms
//!   encode p50 0.10 ms · decode p50 0.04 ms · 2.8 tiles/frame · 0.6 KB/frame · first key 16.8 ms
//!
//! synthetic moving block, paced 30    300 frames   p50 2.32 ms  p95 3.92 ms  p99 6.75 ms
//!   encode p50 1.18 ms · decode p50 0.69 ms · 24.7 tiles/frame · 15.8 KB/frame · first key 45.1 ms
//! ```
//!
//! The bench paid for itself before the numbers were believable: the first key
//! frame measured 249 ms, and the cause was not the encoder (13 ms) but the writer
//! sleeping once per datagram — 430 of them — with Windows rounding each sleep up
//! to about half a millisecond. Coalescing the waits into [`PACER_NAP`]-sized naps
//! took that frame to 45 ms, which is its encode plus the pacing it actually asks
//! for plus a handful of rounded sleeps. Every steady-state frame is far below the
//! nap threshold, so the fix costs nothing on the normal path.
//!
//! Neither run retransmitted, timed out, or asked for a key: the loss paths above
//! are exercised by unit tests, not by an idle loopback. What the loopback cannot
//! measure is the LAN — switch queues, Wi-Fi retries, interrupt coalescing — and
//! what the bench cannot measure is the monitor: this is latency to the
//! framebuffer being ready, not glass to glass, because no presenter exists yet.
//! The live run is also a quiet desktop; its 0.6 KB frames measure idle cost, not
//! a scrolling window, which is closer to the synthetic case. Both are lower
//! bounds on what a person would see.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::error::{CastError, CastResult};
use crate::metrics::now_us;
use crate::transport::{self, Pacer, MTU_PAYLOAD};
use crate::wire::{DecodedFrame, FrameKind, Rect, WireDecoder, WireEncoder};

/// The pacer's ceiling, not the media rate. It exists to spread a key frame's
/// burst across the link instead of handing it to a switch buffer in one go, and
/// 400 Mbit/s lets a half-megabyte photographic key leave in about ten milliseconds
/// while leaving steady traffic untouched.
pub const PACER_BPS: u64 = 400_000_000;

/// How much a frame may hand the link before the pacer makes it wait.
const PACER_BURST: usize = 64 * 1024;

/// The smallest pacing wait worth sleeping for. A key frame is hundreds of
/// datagrams and each one's share of the pacer is tens of microseconds; Windows
/// charges about half a millisecond for every `sleep`, so sleeping per datagram
/// turns 9 ms of intended pacing into 230 ms of actual delay — measured, on the
/// first synthetic key frame. Waits are therefore accumulated and paid in one
/// nap, which bounds the overshoot to one sleep per millisecond of pacing.
const PACER_NAP: Duration = Duration::from_millis(1);

// ------------------------------------------------------------------ sender

#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub session: u32,
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub mtu: usize,
    /// Encode lanes; `0` keeps the encoder's own choice.
    pub threads: usize,
    /// How long an unacknowledged frame is given before it is rolled back and the
    /// next frame is forced to a key. Three frame intervals: long enough for every
    /// retransmission round to have had its chance, short enough that a lost frame
    /// does not cost a tenth of a second of frozen picture.
    pub ack_timeout: Duration,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            session: 0,
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_bps: 8_000_000,
            mtu: MTU_PAYLOAD,
            threads: 0,
            ack_timeout: Duration::from_millis(100),
        }
    }
}

/// One datagram and the wait the pacer owes it.
///
/// The wait is carried rather than slept here so the state machine stays free of
/// wall time: a test can sum the waits instead of spending them.
#[derive(Debug, Clone)]
pub struct Datagram {
    pub wait: Duration,
    pub bytes: Vec<u8>,
}

/// What one captured frame cost to put on the wire.
#[derive(Debug, Clone, Copy)]
pub struct SentInfo {
    pub frame_id: u32,
    pub kind: FrameKind,
    pub wire_bytes: usize,
    pub datagrams: u16,
    pub tiles: usize,
    pub encode_us: u64,
    /// Total time the pacer asked the writer to wait.
    pub pause_us: u64,
}

#[derive(Debug, Default, Clone)]
pub struct SenderStats {
    pub frames: u64,
    /// Captures dropped because the previous frame was still unacknowledged.
    pub skipped: u64,
    pub acked: u64,
    pub timeouts: u64,
    pub key_demands: u64,
    pub retransmits: u64,
    pub rate_changes: u64,
    pub bytes: u64,
}

impl SenderStats {
    pub fn report(&self) -> String {
        format!(
            "frames={} skipped={} acked={} timeouts={} keydemand={} retx={} ratechanges={} bytes={}",
            self.frames,
            self.skipped,
            self.acked,
            self.timeouts,
            self.key_demands,
            self.retransmits,
            self.rate_changes,
            self.bytes
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct InFlight {
    frame_id: u32,
    sent_at: Instant,
}

/// The writing state machine. Owns the encoder and the transport sender; owns no
/// socket, so every rule below can be tested by moving bytes between two sessions.
pub struct SenderSession {
    enc: WireEncoder,
    net: transport::Sender,
    cfg: SenderConfig,
    in_flight: Option<InFlight>,
    /// The next frame must describe the whole picture: set on the first frame (a
    /// listener joining a static desktop still deserves to see it), after a key
    /// request, and after an unacknowledged frame was rolled back.
    key_due: bool,
    scratch: Vec<u8>,
    pub stats: SenderStats,
}

impl SenderSession {
    pub fn new(cfg: SenderConfig) -> Self {
        let mut enc = WireEncoder::new(cfg.width, cfg.height, cfg.bitrate_bps, cfg.fps);
        if cfg.threads > 0 {
            enc.set_threads(cfg.threads);
        }
        let net_cfg = transport::SenderConfig {
            session: cfg.session,
            mtu: cfg.mtu,
            ..Default::default()
        };
        let scratch = Vec::with_capacity(transport::HEADER_BYTES + cfg.mtu);
        Self {
            enc,
            net: transport::Sender::new(
                net_cfg,
                Pacer::new(PACER_BPS, PACER_BURST),
                cfg.bitrate_bps,
            ),
            cfg,
            in_flight: None,
            key_due: true,
            scratch,
            stats: SenderStats::default(),
        }
    }

    pub fn id(&self) -> u32 {
        self.cfg.session
    }

    pub fn in_flight(&self) -> Option<u32> {
        self.in_flight.map(|f| f.frame_id)
    }

    pub fn last_committed(&self) -> u32 {
        self.enc.last_committed()
    }

    pub fn quality(&self) -> u8 {
        self.enc.quality()
    }

    pub fn rtt_us(&self) -> u64 {
        self.net.rtt.rtt_us
    }

    pub fn bitrate_bps(&self) -> u64 {
        self.net.rate.bitrate_bps
    }

    pub fn transport_stats(&self) -> &transport::SenderStats {
        &self.net.stats
    }

    /// Encode one capture and cut it into datagrams.
    ///
    /// Returns `None` when a previous frame is still unacknowledged, in which case
    /// `stats.skipped` advances and nothing else happens. `dirty` is the capture's
    /// hint and may be empty when nothing changed; the first frame is a key whether
    /// the desktop changed or not.
    pub fn send(
        &mut self,
        src: &[u8],
        src_stride: usize,
        dirty: &[Rect],
        capture_us: u64,
        now: Instant,
        out: &mut Vec<Datagram>,
    ) -> CastResult<Option<SentInfo>> {
        if self.in_flight.is_some() {
            self.stats.skipped += 1;
            return Ok(None);
        }
        let force_key = std::mem::take(&mut self.key_due);
        let frame = self
            .enc
            .encode_with(src, src_stride, dirty, capture_us, force_key)?;
        let frags = self.net.begin_frame(frame.frame_id, &frame.bytes);
        let mut pause_us = 0u64;
        for frag in 0..frags {
            let wait = {
                self.scratch.clear();
                let datagram = self.net.datagram(&mut self.scratch, frame.frame_id, frag)?;
                let wait = self.net.pacer.reserve(datagram.len(), now);
                out.push(Datagram {
                    wait,
                    bytes: datagram.to_vec(),
                });
                wait
            };
            pause_us += wait.as_micros() as u64;
        }
        self.in_flight = Some(InFlight {
            frame_id: frame.frame_id,
            sent_at: now,
        });
        self.stats.frames += 1;
        self.stats.bytes += frame.bytes.len() as u64;
        Ok(Some(SentInfo {
            frame_id: frame.frame_id,
            kind: frame.kind,
            wire_bytes: frame.bytes.len(),
            datagrams: frags,
            tiles: frame.tiles,
            encode_us: frame.encode_us,
            pause_us,
        }))
    }

    /// Fold in one packet from the peer. Anything the peer asks for is appended to
    /// `out`; an acknowledgement is what moves the encoder's reference forward.
    pub fn accept(
        &mut self,
        packet: &[u8],
        now: Instant,
        out: &mut Vec<Datagram>,
    ) -> CastResult<()> {
        let header = transport::read_header(packet)?;
        if header.session != self.cfg.session {
            // A stale session on the same port. The transport would ignore it too;
            // checked here so an Ack from a stranger can never commit a frame.
            return Ok(());
        }
        let reaction = self.net.on_packet(packet)?;
        let before = out.len();
        for pkt in reaction.packets {
            let wait = self.net.pacer.reserve(pkt.len(), now);
            out.push(Datagram { wait, bytes: pkt });
        }
        self.stats.retransmits += (out.len() - before) as u64;

        if self.net.take_key_demand() {
            self.stats.key_demands += 1;
            self.give_up_on_in_flight();
        }
        if header.kind == transport::PacketKind::Ack
            && self.in_flight.map(|f| f.frame_id) == Some(header.frame_id)
        {
            self.enc.commit(header.frame_id);
            self.in_flight = None;
            self.stats.acked += 1;
        }
        Ok(())
    }

    /// Timers and feedback: give up on a frame the peer never acknowledged, probe
    /// the link for a round-trip time, and retune the encoder from the transport's
    /// loss estimate. Returns any probe due to be written.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Datagram>) {
        if self.in_flight.map_or(false, |f| {
            now.saturating_duration_since(f.sent_at) >= self.cfg.ack_timeout
        }) {
            self.stats.timeouts += 1;
            self.give_up_on_in_flight();
        }
        if let Some(probe) = self.net.probe_if_due(now) {
            out.push(Datagram {
                wait: Duration::ZERO,
                bytes: probe,
            });
        }
        if self.net.rate.tick(now) {
            self.enc.set_bitrate(self.net.rate.bitrate_bps);
            self.stats.rate_changes += 1;
        }
    }

    fn give_up_on_in_flight(&mut self) {
        if self.in_flight.take().is_some() {
            // Nothing has been encoded since, so the encoder can put the reference
            // back exactly; the next frame is a key because the peer's history is
            // no longer known.
            self.enc.rollback();
            self.key_due = true;
        }
    }
}

// ---------------------------------------------------------------- receiver

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub session: u32,
    pub width: u16,
    pub height: u16,
    /// Decode lanes; `0` keeps the decoder's own choice.
    pub threads: usize,
    pub slots: usize,
    pub give_up: Duration,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            session: 0,
            width: 1920,
            height: 1080,
            threads: 0,
            slots: 8,
            give_up: Duration::from_millis(250),
        }
    }
}

/// One frame that reached the framebuffer.
#[derive(Debug, Clone)]
pub struct Received {
    pub frame: DecodedFrame,
    /// Receiver clock at hand-off. `frame.capture_us` is the sender's stamp from
    /// the same kind of clock, so on one host `present_us - capture_us` is the
    /// pipeline's true end-to-end latency; across machines it is two unrelated
    /// clocks and must not be subtracted.
    pub present_us: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ReceiverStats {
    pub frames: u64,
    pub key_frames: u64,
    pub delta_frames: u64,
    /// Deltas refused because their base is not what this decoder holds.
    pub resyncs: u64,
    /// Frames that completed but could not be parsed or painted.
    pub bad_frames: u64,
}

impl ReceiverStats {
    pub fn report(&self) -> String {
        format!(
            "frames={} key={} delta={} resyncs={} bad={}",
            self.frames, self.key_frames, self.delta_frames, self.resyncs, self.bad_frames
        )
    }
}

/// The reading state machine. Owns the decoder and the transport receiver.
pub struct ReceiverSession {
    dec: WireDecoder,
    net: transport::Receiver,
    cfg: ReceiverConfig,
    last_error: Option<CastError>,
    pub stats: ReceiverStats,
}

impl ReceiverSession {
    pub fn new(cfg: ReceiverConfig) -> Self {
        let mut dec = WireDecoder::new(cfg.width, cfg.height);
        if cfg.threads > 0 {
            dec.set_threads(cfg.threads);
        }
        let net_cfg = transport::ReceiverConfig {
            session: cfg.session,
            mtu: MTU_PAYLOAD,
            slots: cfg.slots,
            give_up: cfg.give_up,
        };
        Self {
            dec,
            net: transport::Receiver::new(net_cfg),
            cfg,
            last_error: None,
            stats: ReceiverStats::default(),
        }
    }

    pub fn id(&self) -> u32 {
        self.cfg.session
    }

    pub fn width(&self) -> u16 {
        self.dec.width()
    }

    pub fn height(&self) -> u16 {
        self.dec.height()
    }

    /// Tight-stride BGRA of the last applied frame — what a presenter uploads.
    pub fn frame(&self) -> &[u8] {
        self.dec.frame()
    }

    pub fn last_applied(&self) -> u32 {
        self.dec.last_applied()
    }

    pub fn last_error(&self) -> Option<CastError> {
        self.last_error
    }

    pub fn transport_stats(&self) -> &transport::ReceiverStats {
        &self.net.stats
    }

    /// Feed one datagram. Whole frames are decoded, applied and acknowledged here;
    /// a frame whose base is missing is refused and answered with a key request.
    pub fn accept(&mut self, packet: &[u8], now: Instant) -> CastResult<Option<Received>> {
        let Some(complete) = self.net.on_packet(packet, now)? else {
            return Ok(None);
        };
        match self.dec.decode(&complete.bytes) {
            Ok(frame) => {
                let present_us = now_us();
                if frame.kind == FrameKind::Key {
                    self.stats.key_frames += 1;
                } else {
                    self.stats.delta_frames += 1;
                }
                self.stats.frames += 1;
                self.net.on_applied(frame.frame_id);
                Ok(Some(Received { frame, present_us }))
            }
            Err(CastError::NeedsKeyframe) => {
                // Expected after any lost base frame. Retransmission cannot repair
                // it, so say so at once and let the sender cut a key.
                self.stats.resyncs += 1;
                self.net.on_rejected(complete.frame_id);
                Ok(None)
            }
            Err(e) => {
                // A frame that does not parse is not repaired by retransmission
                // either. Count it, ask for a key and keep the session alive: one
                // corrupt frame must not end a share.
                self.stats.bad_frames += 1;
                self.last_error = Some(e);
                self.net.on_rejected(complete.frame_id);
                Ok(None)
            }
        }
    }

    /// Run the reassembler's timers — retransmission requests, unacknowledged
    /// frames — and return the control datagrams they produced.
    pub fn tick(&mut self, now: Instant) -> CastResult<Vec<Vec<u8>>> {
        self.net.tick(now)?;
        Ok(self.net.take_control())
    }

    /// Control datagrams owed to the peer, including the Ack for a frame just
    /// applied. Cleared by the call.
    pub fn take_control(&mut self) -> Vec<Vec<u8>> {
        self.net.take_control()
    }
}

// ------------------------------------------------------------------- links

/// Bound UDP socket for the writing end.
pub struct SenderLink {
    sock: UdpSocket,
    peer: SocketAddr,
    session: SenderSession,
    buf: Vec<u8>,
    out: Vec<Datagram>,
    pub written: u64,
}

impl SenderLink {
    pub fn connect(local: &str, peer: SocketAddr, cfg: SenderConfig) -> CastResult<Self> {
        Ok(Self {
            sock: transport::bind(local)?,
            peer,
            session: SenderSession::new(cfg),
            buf: vec![0; 64 * 1024],
            out: Vec::new(),
            written: 0,
        })
    }

    pub fn local_addr(&self) -> CastResult<SocketAddr> {
        self.sock.local_addr().map_err(|_| CastError::Io)
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn session(&self) -> &SenderSession {
        &self.session
    }

    pub fn set_peer(&mut self, peer: SocketAddr) {
        self.peer = peer;
    }

    /// Drain acknowledgements, run the timers, encode one capture and write it.
    ///
    /// The socket is drained *before* encoding: an ack that arrived while the
    /// caller was capturing is what lets this frame be sent instead of dropped.
    /// Pacing sleeps happen in the writer, after the encoder has released the
    /// frame, so a spread key frame delays the link and not the capture thread's
    /// work on the next one.
    pub fn cast(
        &mut self,
        src: &[u8],
        src_stride: usize,
        dirty: &[Rect],
        capture_us: u64,
    ) -> CastResult<Option<SentInfo>> {
        let now = Instant::now();
        self.service(now)?;
        self.session.tick(now, &mut self.out);
        self.write()?;
        let sent = self.session.send(src, src_stride, dirty, capture_us, now, &mut self.out)?;
        self.write()?;
        Ok(sent)
    }

    fn service(&mut self, now: Instant) -> CastResult<()> {
        loop {
            match self.sock.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    if from == self.peer {
                        self.session.accept(&self.buf[..n], now, &mut self.out)?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                // Windows reports an ICMP port-unreachable from a previous send as
                // a receive error. It says nothing about the current frame.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return Err(CastError::Io),
            }
        }
    }

    fn write(&mut self) -> CastResult<()> {
        let mut owed = Duration::ZERO;
        for d in self.out.drain(..) {
            owed += d.wait;
            if owed >= PACER_NAP {
                std::thread::sleep(owed);
                owed = Duration::ZERO;
            }
            match self.sock.send_to(&d.bytes, self.peer) {
                Ok(n) => self.written += n as u64,
                // A full kernel buffer is the link telling us it is saturated. The
                // datagram is dropped and the peer's NACK is the recovery.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(CastError::Io),
            }
        }
        Ok(())
    }
}

/// Bound UDP socket for the reading end.
pub struct ReceiverLink {
    sock: UdpSocket,
    peer: Option<SocketAddr>,
    session: ReceiverSession,
    buf: Vec<u8>,
    pub received: u64,
}

impl ReceiverLink {
    pub fn bind(local: &str, cfg: ReceiverConfig) -> CastResult<Self> {
        Ok(Self {
            sock: transport::bind(local)?,
            peer: None,
            session: ReceiverSession::new(cfg),
            buf: vec![0; 64 * 1024],
            received: 0,
        })
    }

    pub fn local_addr(&self) -> CastResult<SocketAddr> {
        self.sock.local_addr().map_err(|_| CastError::Io)
    }

    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    pub fn session(&self) -> &ReceiverSession {
        &self.session
    }

    /// Wait up to `budget` for a whole frame.
    ///
    /// Every millisecond of the wait runs the reassembler's timers, so a missing
    /// fragment is asked for without the caller having a second loop. The first
    /// datagram carrying this session's id binds the peer: a stranger who finds
    /// the port is never answered, and cannot redirect an acknowledgement.
    pub fn poll(&mut self, budget: Duration) -> CastResult<Option<Received>> {
        let deadline = Instant::now() + budget;
        loop {
            let now = Instant::now();
            for pkt in self.session.tick(now)? {
                self.send_control(&pkt);
            }
            match self.sock.recv_from(&mut self.buf) {
                Ok((n, from)) => {
                    self.received += n as u64;
                    let ours = match transport::read_header(&self.buf[..n]) {
                        Ok(h) => h.session == self.session.id(),
                        Err(_) => false,
                    };
                    if !ours {
                        continue;
                    }
                    if self.peer.is_none() {
                        self.peer = Some(from);
                    }
                    if self.peer != Some(from) {
                        continue;
                    }
                    let got = self.session.accept(&self.buf[..n], Instant::now())?;
                    self.flush();
                    if got.is_some() {
                        return Ok(got);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => return Err(CastError::Io),
            }
        }
    }

    fn flush(&mut self) {
        let Some(peer) = self.peer else { return };
        for pkt in self.session.take_control() {
            let _ = self.sock.send_to(&pkt, peer);
        }
    }

    fn send_control(&mut self, pkt: &[u8]) {
        if let Some(peer) = self.peer {
            let _ = self.sock.send_to(pkt, peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::PacketKind;
    use crate::wire::TILE;

    const W: u16 = 128;
    const H: u16 = 128;
    const SESSION: u32 = 42;

    fn sender_cfg() -> SenderConfig {
        SenderConfig {
            session: SESSION,
            width: W,
            height: H,
            ..Default::default()
        }
    }

    fn receiver_cfg() -> ReceiverConfig {
        ReceiverConfig {
            session: SESSION,
            width: W,
            height: H,
            ..Default::default()
        }
    }

    /// Compressible but not trivial: flat tiles and a few hard edges, so the tile
    /// codec stays lossless and a framebuffer comparison is exact.
    fn screen_at(w: u16, h: u16, seed: u8) -> Vec<u8> {
        let stride = w as usize * 4;
        let mut px = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let d = y * stride + x * 4;
                let band = ((x / 16) as u8).wrapping_add(seed);
                px[d] = band;
                px[d + 1] = band.wrapping_mul(3);
                px[d + 2] = if (y / 16) % 2 == 0 { 200 } else { 40 };
                px[d + 3] = 255;
            }
        }
        px
    }

    fn screen(seed: u8) -> Vec<u8> {
        screen_at(W, H, seed)
    }

    fn whole_screen() -> Vec<Rect> {
        vec![Rect::new(0, 0, W, H)]
    }

    /// Move every datagram from the sender to the receiver, returns what completed.
    fn deliver(sender_out: &[Datagram], receiver: &mut ReceiverSession, now: Instant) -> Option<Received> {
        let mut got = None;
        for d in sender_out {
            if let Some(r) = receiver.accept(&d.bytes, now).unwrap() {
                got = Some(r);
            }
        }
        got
    }

    fn settle(sender: &mut SenderSession, receiver: &mut ReceiverSession, now: Instant) {
        for pkt in receiver.take_control() {
            sender.accept(&pkt, now, &mut Vec::new()).unwrap();
        }
    }

    #[test]
    fn a_key_frame_round_trips_and_the_reference_advances_only_on_the_ack() {
        let mut sender = SenderSession::new(sender_cfg());
        let mut receiver = ReceiverSession::new(receiver_cfg());
        let now = Instant::now();
        let px = screen(0);

        let mut out = Vec::new();
        let sent = sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .expect("the first frame is never blocked");
        assert_eq!(sent.kind, FrameKind::Key, "a listener joining a static screen still sees it");
        assert_eq!(sender.last_committed(), 0, "encoded is not committed");

        let got = deliver(&out, &mut receiver, now).expect("the frame is whole");
        assert_eq!(got.frame.frame_id, sent.frame_id);
        assert_eq!(receiver.frame(), &px[..], "lossless content is exact");
        assert_eq!(sender.last_committed(), 0, "the ack has not been read yet");

        settle(&mut sender, &mut receiver, now);
        assert_eq!(sender.last_committed(), sent.frame_id);
        assert_eq!(sender.in_flight(), None);
        assert_eq!(sender.stats.skipped, 0);
    }

    #[test]
    fn a_capture_that_arrives_while_a_frame_is_in_flight_is_dropped_not_queued() {
        let mut sender = SenderSession::new(sender_cfg());
        let now = Instant::now();
        let px = screen(0);
        let mut out = Vec::new();
        let first = sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .expect("first frame");
        assert!(sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .is_none());
        assert_eq!(sender.stats.skipped, 1);
        assert_eq!(sender.in_flight(), Some(first.frame_id));
        assert_eq!(sender.last_committed(), 0);
    }

    #[test]
    fn an_unacknowledged_frame_times_out_and_the_next_one_is_a_key() {
        let mut sender = SenderSession::new(sender_cfg());
        let now = Instant::now();
        let px = screen(0);
        let mut out = Vec::new();
        sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .unwrap();
        let later = now + sender_cfg().ack_timeout + Duration::from_millis(1);
        sender.tick(later, &mut out);
        assert_eq!(sender.stats.timeouts, 1);
        assert_eq!(sender.in_flight(), None);
        assert_eq!(sender.last_committed(), 0, "nothing was committed");

        out.clear();
        let next = sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), later, &mut out)
            .unwrap()
            .expect("a rolled-back frame frees the encoder");
        assert_eq!(next.kind, FrameKind::Key);
    }

    #[test]
    fn a_key_request_rolls_back_and_the_next_frame_is_a_key() {
        let mut sender = SenderSession::new(sender_cfg());
        let now = Instant::now();
        let px = screen(0);
        let mut out = Vec::new();
        sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .unwrap();

        let ask = transport::control_packet(PacketKind::KeyRequest, SESSION, 1, &[]);
        sender.accept(&ask, now, &mut out).unwrap();
        assert_eq!(sender.in_flight(), None);
        assert_eq!(sender.stats.key_demands, 1);

        out.clear();
        let next = sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(next.kind, FrameKind::Key);
    }

    #[test]
    fn a_lost_fragment_is_nacked_and_repaired_inside_the_session() {
        // Big enough that the key frame needs several datagrams; the 128×128 screen
        // the other tests use compresses into one.
        let (w, h) = (512u16, 512u16);
        let mut sender = SenderSession::new(SenderConfig {
            session: SESSION,
            width: w,
            height: h,
            ..Default::default()
        });
        let mut receiver = ReceiverSession::new(ReceiverConfig {
            session: SESSION,
            width: w,
            height: h,
            ..Default::default()
        });
        let now = Instant::now();
        let px = screen_at(w, h, 0);
        let full = [Rect::new(0, 0, w, h)];
        let mut out = Vec::new();
        let sent = sender
            .send(&px, w as usize * 4, &full, now_us(), now, &mut out)
            .unwrap()
            .unwrap();
        assert!(sent.datagrams > 2, "this frame has to have fragments to lose one");

        let mut partial = out.clone();
        partial.remove(1);
        assert!(deliver(&partial, &mut receiver, now).is_none());

        // The receiver's timers turn the hole into a request.
        let control = receiver.tick(now + Duration::from_millis(8)).unwrap();
        assert_eq!(control.len(), 1);
        let mut repair = Vec::new();
        for pkt in &control {
            sender.accept(pkt, now, &mut repair).unwrap();
        }
        assert_eq!(repair.len(), 1, "exactly the missing fragment comes back");

        let got = deliver(&repair, &mut receiver, now).expect("the retransmit completes it");
        assert_eq!(got.frame.frame_id, sent.frame_id);
        assert_eq!(receiver.frame(), &px[..]);
        settle(&mut sender, &mut receiver, now);
        assert_eq!(sender.last_committed(), sent.frame_id);
    }

    #[test]
    fn a_delta_without_its_base_is_refused_and_answered_with_a_key_request() {
        // A standalone encoder produces a key and then a delta against it.
        let mut enc = WireEncoder::new(W, H, 8_000_000, 30);
        let a = screen(0);
        let key = enc
            .encode_with(&a, W as usize * 4, &whole_screen(), now_us(), true)
            .unwrap();
        enc.commit(key.frame_id);
        let mut b = a.clone();
        b[0..64].fill(7);
        let delta = enc
            .encode(&b, W as usize * 4, &whole_screen(), now_us())
            .unwrap();
        assert_eq!(delta.kind, FrameKind::Delta);
        assert_eq!(delta.base_id, key.frame_id);

        // The receiver never saw the key, so the delta's base is not what it holds.
        let mut receiver = ReceiverSession::new(receiver_cfg());
        let now = Instant::now();
        let mut pkt = Vec::new();
        let n = transport::frag_count(delta.bytes.len(), MTU_PAYLOAD);
        for frag in 0..n {
            let start = frag as usize * MTU_PAYLOAD;
            let end = (start + MTU_PAYLOAD).min(delta.bytes.len());
            transport::write_data(&mut pkt, SESSION, delta.frame_id, frag, n, &delta.bytes[start..end]);
            receiver.accept(&pkt, now).unwrap();
        }
        assert_eq!(receiver.stats.resyncs, 1);
        let control = receiver.take_control();
        assert_eq!(control.len(), 1);
        assert_eq!(
            transport::read_header(&control[0]).unwrap().kind,
            PacketKind::KeyRequest
        );
    }

    #[test]
    fn a_session_bound_to_another_id_is_not_talked_to() {
        let mut sender = SenderSession::new(sender_cfg());
        let now = Instant::now();
        let px = screen(0);
        let mut out = Vec::new();
        sender
            .send(&px, W as usize * 4, &whole_screen(), now_us(), now, &mut out)
            .unwrap()
            .unwrap();

        let mut stranger = Vec::new();
        let ack = transport::control_packet(PacketKind::Ack, SESSION + 1, 1, &[]);
        sender.accept(&ack, now, &mut stranger).unwrap();
        assert_eq!(sender.last_committed(), 0, "a stranger's ack proves nothing");
        assert!(stranger.is_empty());
    }

    #[test]
    fn the_tile_grid_is_what_the_sender_dims_and_the_receiver_uses() {
        // A guard against the two ends being constructed with different sizes: the
        // wire frame carries its own size, and the decoder refuses a mismatch.
        assert_eq!(W % TILE, 0);
        let sender = SenderSession::new(sender_cfg());
        let receiver = ReceiverSession::new(receiver_cfg());
        assert_eq!(sender.enc.grid().width, receiver.width());
        assert_eq!(sender.enc.grid().height, receiver.height());
    }

    #[test]
    fn a_frame_crosses_loopback_udp_and_the_ack_commits_it() {
        let mut receiver = ReceiverLink::bind("127.0.0.1:0", receiver_cfg()).unwrap();
        let addr = receiver.local_addr().unwrap();
        let mut sender =
            SenderLink::connect("127.0.0.1:0", addr, sender_cfg()).unwrap();
        let px = screen(0);

        let sent = sender
            .cast(&px, W as usize * 4, &whole_screen(), now_us())
            .unwrap()
            .expect("nothing is in flight");
        assert_eq!(sent.kind, FrameKind::Key);

        let got = receiver
            .poll(Duration::from_millis(500))
            .unwrap()
            .expect("the frame arrives over loopback");
        assert_eq!(got.frame.frame_id, sent.frame_id);
        assert_eq!(receiver.session().frame(), &px[..]);

        // The next cast drains the ack first, which is what lets it encode at all.
        let second = sender
            .cast(&px, W as usize * 4, &whole_screen(), now_us())
            .unwrap()
            .expect("the ack arrived while the caller slept");
        assert_eq!(second.frame_id, sent.frame_id + 1);
        assert_eq!(sender.session().last_committed(), sent.frame_id);
        assert_eq!(receiver.session().last_applied(), sent.frame_id);

        let third = receiver
            .poll(Duration::from_millis(500))
            .unwrap()
            .expect("a static screen still sends its frame");
        assert_eq!(third.frame.frame_id, second.frame_id);
        assert_eq!(third.frame.updates.len(), 0, "nothing changed, nothing repainted");
    }
}
