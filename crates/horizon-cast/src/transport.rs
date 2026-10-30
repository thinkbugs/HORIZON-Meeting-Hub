//! The channel's transport: one encoded frame in, one complete frame out.
//!
//! This module is deliberately **I/O free**. It cuts frames into datagrams,
//! reassembles them, decides what to retransmit, paces the result and measures the
//! round trip — but it never touches a socket. [`crate::session`] owns the sockets
//! and the threads. The reason is the same one that makes the tile codec testable:
//! every rule here is a rule about bytes, and a rule about bytes that needs a
//! network to test is a rule nobody will test.
//!
//! ## Why there is no reordering buffer
//!
//! A delta is cut against the picture the sender has *proven* the peer holds
//! ([`crate::wire`]'s commit rule), and a frame is proven only by the acknowledgement
//! this module sends. So a delta's `base_id` is always a frame the receiver has
//! already applied, which means the receiver can hand a completed frame straight to
//! the decoder and let [`crate::wire::WireDecoder`] check the base. A jitter buffer
//! would add a buffer's worth of latency to re-derive an ordering the protocol
//! already guarantees.
//!
//! ## What the reliability model is, and is not
//!
//! UDP plus three control packets:
//! - **Ack** — the receiver applied frame N. The sender's `commit_floor` advances
//!   over the *contiguous* prefix, because a delta cut against N is meaningless if
//!   N never landed.
//! - **Nack** — the receiver knows exactly which fragments of frame N it lacks, so
//!   it asks for those and nothing else. Thirty missing fragments inside a fifty
//!   fragment frame do not re-send the twenty it has.
//! - **KeyRequest** — the receiver gave up on a frame it can no longer assemble.
//!   This is the only expensive answer, and it is the honest one: a delta whose
//!   base is gone cannot be reconstructed by retransmitting anything.
//!
//! Loss on a LAN is not congestion, it is a burst overrunning a switch buffer, so
//! the pacer spreads a frame wider than one burst allowance and the retransmission
//! window is sized in bytes rather than frames. Both are measured, at the bottom of
//! this file.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::error::{CastError, CastResult};
use crate::metrics::now_us;

/// On-wire format version. A peer with a different number is not talked to at all,
/// because guessing which fields a stranger put where is how a decoder bug becomes
/// a security bug.
pub const VERSION: u8 = 1;

/// version, kind, session, frame id, fragment index, fragment count.
pub const HEADER_BYTES: usize = 16;

/// Payload bytes per datagram.
///
/// 1500 MTU minus 28 bytes of IP and UDP headers is 1472, and this is deliberately
/// not that: a VLAN tag takes 4, PPPoE 8, and a driver may add more. 1200 is the
/// number every path in a building carries without fragmenting, and an
/// IP-fragmented datagram costs a whole retransmission when any one piece is lost.
pub const MTU_PAYLOAD: usize = 1200;

/// How many rounds of retransmission a frame gets before the receiver gives up on
/// it and asks for a key frame instead. Three is length ÷ RTT on a LAN: enough for
/// a lost reply, short enough that a picture never waits a tenth of a second for a
/// datagram that is not coming.
const MAX_NACK_ROUNDS: u8 = 3;

/// Minimum spacing between two NACKs for the same frame, and the grace a frame gets
/// before its first one.
///
/// A hole at fragment 4 makes every one of the forty datagrams after it a "gap
/// noticed" event; without the spacing the receiver would answer each of them with
/// the same NACK. The same interval before the first request is what keeps a
/// reordered burst from being read as a loss: on a LAN, if a datagram is going to
/// arrive late rather than never, it has done so well inside six milliseconds.
const NACK_INTERVAL: Duration = Duration::from_millis(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    Data = 0,
    Nack = 1,
    KeyRequest = 2,
    Ack = 3,
    Probe = 4,
    ProbeAck = 5,
}

impl PacketKind {
    fn from_u8(v: u8) -> CastResult<Self> {
        match v {
            0 => Ok(PacketKind::Data),
            1 => Ok(PacketKind::Nack),
            2 => Ok(PacketKind::KeyRequest),
            3 => Ok(PacketKind::Ack),
            4 => Ok(PacketKind::Probe),
            5 => Ok(PacketKind::ProbeAck),
            // A peer newer than us may have added one; ignoring the packet is right,
            // guessing at its layout is not.
            _ => Err(CastError::Unsupported("packet kind")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: PacketKind,
    pub session: u32,
    /// For `Data`: the frame being carried. For `Ack` and `KeyRequest`: the frame
    /// being spoken about. For `Probe`: the probe's sequence number.
    pub frame_id: u32,
    pub frag: u16,
    pub frags: u16,
}

impl Header {
    pub fn body<'a>(&self, packet: &'a [u8]) -> CastResult<&'a [u8]> {
        if packet.len() < HEADER_BYTES {
            return Err(CastError::Malformed("short datagram"));
        }
        Ok(&packet[HEADER_BYTES..])
    }
}

pub fn read_header(packet: &[u8]) -> CastResult<Header> {
    if packet.len() < HEADER_BYTES {
        return Err(CastError::Malformed("short datagram"));
    }
    if packet[0] != VERSION {
        return Err(CastError::Unsupported("transport version"));
    }
    let kind = PacketKind::from_u8(packet[1])?;
    let session = u32::from_le_bytes(packet[2..6].try_into().unwrap());
    let frame_id = u32::from_le_bytes(packet[6..10].try_into().unwrap());
    let frag = u16::from_le_bytes(packet[10..12].try_into().unwrap());
    let frags = u16::from_le_bytes(packet[12..14].try_into().unwrap());
    Ok(Header {
        kind,
        session,
        frame_id,
        frag,
        frags,
    })
}

fn write_header(out: &mut Vec<u8>, h: &Header) {
    out.clear();
    out.push(VERSION);
    out.push(h.kind as u8);
    out.extend_from_slice(&h.session.to_le_bytes());
    out.extend_from_slice(&h.frame_id.to_le_bytes());
    out.extend_from_slice(&h.frag.to_le_bytes());
    out.extend_from_slice(&h.frags.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
}

/// Build one data datagram into `out`, which is reused across every fragment of
/// every frame so that a 414-datagram key frame does not become 414 allocations.
pub fn write_data(
    out: &mut Vec<u8>,
    session: u32,
    frame_id: u32,
    frag: u16,
    frags: u16,
    payload: &[u8],
) {
    write_header(
        out,
        &Header {
            kind: PacketKind::Data,
            session,
            frame_id,
            frag,
            frags,
        },
    );
    out.extend_from_slice(payload);
}

pub fn control_packet(kind: PacketKind, session: u32, frame_id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + body.len());
    write_header(
        &mut out,
        &Header {
            kind,
            session,
            frame_id,
            frag: 0,
            frags: 0,
        },
    );
    out.extend_from_slice(body);
    out
}

pub fn frag_count(bytes: usize, mtu: usize) -> u16 {
    let mtu = mtu.max(1);
    bytes.div_ceil(mtu).max(1).min(u16::MAX as usize) as u16
}

/// One missing-fragment request: fragments `[base, base+64)` of `frame_id`, with a
/// bit set for each one the receiver still needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NackEntry {
    pub frame_id: u32,
    pub base: u16,
    pub bits: u64,
}

impl NackEntry {
    /// 16 bytes per entry; three fit in a 1200-byte datagram with room to spare,
    /// which is what bounds a NACK's cost to one packet per burst of holes.
    pub const BYTES: usize = 16;

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&(self.base as u32).to_le_bytes());
        out.extend_from_slice(&self.bits.to_le_bytes());
    }

    fn read(body: &[u8]) -> CastResult<Vec<NackEntry>> {
        if body.len() % Self::BYTES != 0 {
            return Err(CastError::Malformed("nack body"));
        }
        let mut out = Vec::with_capacity(body.len() / Self::BYTES);
        for chunk in body.chunks_exact(Self::BYTES) {
            let frame_id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let base = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            let bits = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
            if base > u16::MAX as u32 {
                return Err(CastError::Malformed("nack base"));
            }
            out.push(NackEntry {
                frame_id,
                base: base as u16,
                bits,
            });
        }
        Ok(out)
    }

    pub fn frags(&self) -> impl Iterator<Item = u16> + '_ {
        (0..64u16).filter(move |i| self.bits & (1u64 << i) != 0).map(move |i| self.base.saturating_add(i))
    }
}

// ---------------------------------------------------------------- pacing

/// Token bucket over datagram bytes.
///
/// The whole point is that a *steady* frame never waits: a live 1080p desktop
/// measured 665 bytes per frame on this codec, which is one datagram, so the
/// bucket is full at every frame's start and the pacing costs nothing. A resync
/// frame is 45 datagrams of text or 414 of photograph, and sending those back to
/// back is what overruns a switch port's buffer and turns one lost datagram into a
/// retransmission round. The bucket spreads exactly that case and only that case.
#[derive(Debug, Clone)]
pub struct Pacer {
    /// Bytes per second the bucket refills at. Deliberately well above the media
    /// rate: this is a burst limiter, not network congestion control.
    pub rate_bps: u64,
    /// How much may leave without any wait at all.
    pub bucket_bytes: usize,
    tokens: f64,
    last: Instant,
    pub waited_us: u64,
}

impl Pacer {
    pub fn new(rate_bps: u64, bucket_bytes: usize) -> Self {
        Self {
            rate_bps: rate_bps.max(1),
            bucket_bytes: bucket_bytes.max(1),
            tokens: bucket_bytes.max(1) as f64,
            last: Instant::now(),
            waited_us: 0,
        }
    }

    /// Ask permission to write `bytes`. Returns how long to wait first.
    ///
    /// The caller sleeps; this type never does, so a test can move a clock instead
    /// of spending the wall time.
    pub fn reserve(&mut self, bytes: usize, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last);
        self.tokens = (self.tokens + elapsed.as_secs_f64() * self.rate_bps as f64 / 8.0)
            .min(self.bucket_bytes as f64);
        let need = bytes as f64;
        if self.tokens >= need {
            self.tokens -= need;
            self.last = now;
            return Duration::ZERO;
        }
        let missing = need - self.tokens;
        let wait = Duration::from_secs_f64(missing * 8.0 / self.rate_bps as f64);
        self.tokens = 0.0;
        // The wait is spent refilling the bucket, and the bytes it produced are the
        // ones this call is spending. Moving `last` to the end of the wait keeps the
        // next call from crediting the same interval a second time — the bug that
        // made a hundred-fragment burst look twice as fast as it was.
        self.last = now + wait;
        self.waited_us += wait.as_micros() as u64;
        wait
    }
}

// ---------------------------------------------------------------- round trip

/// Smoothed round-trip time. The sample is measured by the *sender's* own clock on
/// both ends of the round trip, so it needs no clock agreement between machines.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rtt {
    pub rtt_us: u64,
    pub jitter_us: u64,
    pub samples: u64,
}

impl Rtt {
    pub fn observe(&mut self, sample_us: u64) {
        if self.samples == 0 {
            self.rtt_us = sample_us;
            self.jitter_us = sample_us / 2;
        } else {
            let delta = self.rtt_us.abs_diff(sample_us);
            // The same 1/8 weighting the kernel uses for TCP: a single stalled ACK
            // must not be read as a link change.
            self.rtt_us = (self.rtt_us * 7 + sample_us) / 8;
            self.jitter_us = (self.jitter_us * 7 + delta) / 8;
        }
        self.samples += 1;
    }

    pub fn is_measured(&self) -> bool {
        self.samples > 0
    }
}

// ---------------------------------------------------------------- congestion

/// What the channel is willing to put on the wire, in bits per second.
///
/// The control law is additive-increase / multiplicative-decrease on the *media
/// rate* the tile encoder is told to hit, driven by the only signal this transport
/// has: how many fragments had to be retransmitted. Congestion on a LAN looks like
/// a hole, so a hole is the signal — and the window is a full second of frames, so
/// one unlucky burst does not move the rate.
#[derive(Debug, Clone)]
pub struct RateGovernor {
    pub bitrate_bps: u64,
    pub min_bps: u64,
    pub max_bps: u64,
    frags_sent: u64,
    frags_lost: u64,
    since: Instant,
}

impl RateGovernor {
    pub fn new(bitrate_bps: u64) -> Self {
        Self {
            bitrate_bps,
            min_bps: 1_000_000,
            max_bps: 80_000_000,
            frags_sent: 0,
            frags_lost: 0,
            since: Instant::now(),
        }
    }

    pub fn note_sent(&mut self, frags: u64) {
        self.frags_sent += frags;
    }

    pub fn note_lost(&mut self, frags: u64) {
        self.frags_lost += frags;
    }

    /// Fold in one second's worth of evidence. Returns `true` when the rate moved,
    /// so the caller knows whether to retune the encoder.
    pub fn tick(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.since) < Duration::from_secs(1) {
            return false;
        }
        self.since = now;
        let sent = std::mem::take(&mut self.frags_sent);
        let lost = std::mem::take(&mut self.frags_lost);
        if sent < 20 {
            return false;
        }
        let loss = lost as f64 / sent as f64;
        let before = self.bitrate_bps;
        if loss > 0.02 {
            // Halve, not shave: the queue that dropped the datagram is still full,
            // and a 10% trim would take five more seconds of drops to matter.
            self.bitrate_bps = (self.bitrate_bps / 2).max(self.min_bps);
        } else if loss < 0.005 {
            self.bitrate_bps = (self.bitrate_bps + self.bitrate_bps / 16).min(self.max_bps);
        }
        self.bitrate_bps != before
    }

    pub fn loss_ratio(&self) -> f64 {
        if self.frags_sent == 0 {
            0.0
        } else {
            self.frags_lost as f64 / self.frags_sent as f64
        }
    }
}

// ---------------------------------------------------------------- sender

#[derive(Debug, Clone, Copy)]
pub struct SenderConfig {
    pub session: u32,
    pub mtu: usize,
    /// Retain this many bytes of already-sent frames for retransmission.
    pub window_bytes: usize,
    /// ...and no more than this many frames, because a frame is only retained until
    /// it is acknowledged and a peer that never acknowledges must not grow the
    /// sender without bound.
    pub window_frames: usize,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            session: 0,
            mtu: MTU_PAYLOAD,
            // Four photo-quality key frames, or a thousand 665-byte deltas.
            window_bytes: 2 << 20,
            window_frames: 8,
        }
    }
}

struct Retained {
    id: u32,
    bytes: Vec<u8>,
    frags: u16,
    acked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frag {
    pub frame_id: u32,
    pub frag: u16,
}

/// What the sender wants written to the socket, in reply to something it received.
#[derive(Debug, Default)]
pub struct Reaction {
    /// Datagrams to send to the peer, already built.
    pub packets: Vec<Vec<u8>>,
    /// The peer cannot be repaired by retransmission; cut a fresh key frame.
    pub want_key: bool,
}

impl Reaction {
    fn silent() -> Self {
        Self::default()
    }
}

#[derive(Debug, Default, Clone)]
pub struct SenderStats {
    pub frames: u64,
    pub datagrams: u64,
    pub bytes: u64,
    pub retransmits: u64,
    pub acks: u64,
    pub nacks: u64,
    pub key_demands: u64,
    /// Frames evicted from the window while still unacknowledged — the number that
    /// says the window is too small for the link's RTT.
    pub evicted: u64,
}

impl SenderStats {
    pub fn report(&self) -> String {
        format!(
            "frames={} datagrams={} bytes={} retx={} acks={} nacks={} keydemand={} evicted={}",
            self.frames,
            self.datagrams,
            self.bytes,
            self.retransmits,
            self.acks,
            self.nacks,
            self.key_demands,
            self.evicted
        )
    }
}

pub struct Sender {
    cfg: SenderConfig,
    window: VecDeque<Retained>,
    window_used: usize,
    /// `None` until a contiguous prefix of the window has been acknowledged — the
    /// first frame has no predecessor to advance from.
    acked_through: Option<u32>,
    pub pacer: Pacer,
    pub rtt: Rtt,
    pub rate: RateGovernor,
    pub stats: SenderStats,
    probe_seq: u32,
    probe_at: Option<(u32, u64)>,
    next_probe: Instant,
    key_demand: bool,
}

impl Sender {
    pub fn new(cfg: SenderConfig, pacer: Pacer, bitrate_bps: u64) -> Self {
        Self {
            cfg,
            window: VecDeque::new(),
            window_used: 0,
            acked_through: None,
            pacer,
            rtt: Rtt::default(),
            rate: RateGovernor::new(bitrate_bps),
            stats: SenderStats::default(),
            probe_seq: 0,
            probe_at: None,
            next_probe: Instant::now(),
            key_demand: false,
        }
    }

    pub fn session(&self) -> u32 {
        self.cfg.session
    }

    pub fn mtu(&self) -> usize {
        self.cfg.mtu
    }

    /// Take a frame the caller is about to send. Returns the fragment count; the
    /// caller then writes fragments `0..count` with [`Sender::datagram`].
    ///
    /// The bytes are retained because a lost fragment is retransmitted from these,
    /// never by re-encoding: the encoder has moved on by the time the NACK arrives.
    pub fn begin_frame(&mut self, frame_id: u32, bytes: &[u8]) -> u16 {
        let frags = frag_count(bytes.len(), self.cfg.mtu);
        self.window_used += bytes.len();
        self.window.push_back(Retained {
            id: frame_id,
            bytes: bytes.to_vec(),
            frags,
            acked: false,
        });
        while self.window.len() > self.cfg.window_frames
            || (self.window_used > self.cfg.window_bytes && self.window.len() > 1)
        {
            let old = self.window.pop_front().expect("window is not empty");
            self.window_used -= old.bytes.len();
            if !old.acked {
                self.stats.evicted += 1;
                // The peer may still be missing pieces of it and there is nothing
                // left to retransmit from.
                self.key_demand = true;
            }
        }
        self.stats.frames += 1;
        self.stats.datagrams += frags as u64;
        self.stats.bytes += bytes.len() as u64;
        self.rate.note_sent(frags as u64);
        frags
    }

    /// One datagram, built into `scratch` and returned. `scratch` is reused across
    /// every fragment of every frame.
    pub fn datagram<'a>(
        &self,
        scratch: &'a mut Vec<u8>,
        frame_id: u32,
        frag: u16,
    ) -> CastResult<&'a [u8]> {
        let frame = self
            .window
            .iter()
            .find(|f| f.id == frame_id)
            .ok_or(CastError::InvalidState("fragment of a frame no longer retained"))?;
        if frag >= frame.frags {
            return Err(CastError::Malformed("fragment index out of range"));
        }
        let start = frag as usize * self.cfg.mtu;
        let end = (start + self.cfg.mtu).min(frame.bytes.len());
        write_data(scratch, self.cfg.session, frame_id, frag, frame.frags, &frame.bytes[start..end]);
        Ok(scratch)
    }

    /// The highest frame the peer has acknowledged, or `None` on the very first one.
    ///
    /// Contiguous on purpose: an ack for frame 9 proves nothing about frame 8, and
    /// the encoder's reference may only advance to a picture the peer really holds.
    pub fn commit_floor(&self) -> Option<u32> {
        self.acked_through
    }

    pub fn take_key_demand(&mut self) -> bool {
        std::mem::take(&mut self.key_demand)
    }

    /// A probe is due when nothing has been measured for a while, or when the last
    /// one is old enough to be lost. It returns the datagram to send.
    pub fn probe_if_due(&mut self, now: Instant) -> Option<Vec<u8>> {
        if now < self.next_probe {
            return None;
        }
        self.next_probe = now + Duration::from_millis(200);
        self.probe_seq = self.probe_seq.wrapping_add(1);
        let stamp = now_us();
        self.probe_at = Some((self.probe_seq, stamp));
        Some(control_packet(
            PacketKind::Probe,
            self.cfg.session,
            self.probe_seq,
            &stamp.to_le_bytes(),
        ))
    }

    pub fn on_packet(&mut self, packet: &[u8]) -> CastResult<Reaction> {
        let h = read_header(packet)?;
        if h.session != self.cfg.session {
            // A stale session on the same port. Answering it would put our media in
            // front of somebody else's.
            return Ok(Reaction::silent());
        }
        let body = h.body(packet)?;
        match h.kind {
            PacketKind::Ack => {
                self.note_ack(h.frame_id);
                Ok(Reaction::silent())
            }
            PacketKind::Nack => {
                self.stats.nacks += 1;
                let entries = NackEntry::read(body)?;
                let mut reaction = Reaction::silent();
                let mut lost = 0u64;
                for e in &entries {
                    let known = self.window.iter().any(|f| f.id == e.frame_id);
                    if !known {
                        // It was evicted, so its bytes are gone; the receiver will
                        // not be able to complete it by any means.
                        self.key_demand = true;
                        continue;
                    }
                    for frag in e.frags() {
                        let mut scratch = Vec::with_capacity(HEADER_BYTES + self.cfg.mtu);
                        if self.datagram(&mut scratch, e.frame_id, frag).is_ok() {
                            lost += 1;
                            reaction.packets.push(scratch);
                        }
                    }
                }
                self.stats.retransmits += lost;
                self.rate.note_lost(lost);
                Ok(reaction)
            }
            PacketKind::KeyRequest => {
                self.stats.key_demands += 1;
                self.key_demand = true;
                Ok(Reaction::silent())
            }
            PacketKind::ProbeAck => {
                if body.len() >= 8 {
                    let stamp = u64::from_le_bytes(body[0..8].try_into().unwrap());
                    if let Some((seq, sent)) = self.probe_at {
                        if seq == h.frame_id && stamp == sent {
                            self.rtt.observe(now_us().saturating_sub(sent));
                            self.probe_at = None;
                        }
                    }
                }
                Ok(Reaction::silent())
            }
            PacketKind::Probe => {
                // Echo the same eight opaque bytes; the peer measures with its own
                // clock, so nothing here needs the two machines to agree on a time.
                Ok(Reaction {
                    packets: vec![control_packet(
                        PacketKind::ProbeAck,
                        self.cfg.session,
                        h.frame_id,
                        body,
                    )],
                    want_key: false,
                })
            }
            PacketKind::Data => Ok(Reaction::silent()),
        }
    }

    fn note_ack(&mut self, frame_id: u32) {
        self.stats.acks += 1;
        let mut found = false;
        for f in self.window.iter_mut() {
            if f.id == frame_id {
                f.acked = true;
                found = true;
            }
        }
        if !found && self.acked_through.is_none_or(|a| frame_id > a) {
            // Acknowledged after the frame left the window: still proof it landed.
            self.acked_through = Some(frame_id);
        }
        while let Some(front) = self.window.front() {
            if !front.acked {
                break;
            }
            self.acked_through = Some(front.id);
            self.window_used -= front.bytes.len();
            self.window.pop_front();
        }
    }
}

// ---------------------------------------------------------------- receiver

#[derive(Debug, Clone, Copy)]
pub struct ReceiverConfig {
    pub session: u32,
    pub mtu: usize,
    /// Completed-but-unclaimed frames do not exist here — a frame is handed up the
    /// moment it is whole — so this bounds the *incomplete* ones, which is a
    /// reordering and loss budget rather than a playback buffer.
    pub slots: usize,
    /// How long an incomplete frame is kept before it is abandoned for a key frame.
    pub give_up: Duration,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            session: 0,
            mtu: MTU_PAYLOAD,
            slots: 8,
            give_up: Duration::from_millis(250),
        }
    }
}

/// A frame whose last fragment just arrived.
#[derive(Debug, Clone)]
pub struct Complete {
    pub frame_id: u32,
    pub bytes: Vec<u8>,
    pub fragments: u16,
}

struct Slot {
    id: u32,
    frags: u16,
    have: Vec<u64>,
    got: u16,
    /// The highest fragment index seen so far. Nothing above it is ever asked for:
    /// it may simply still be in flight, and a request for it would re-send most of
    /// a frame that is already on its way.
    highest: u16,
    buf: Vec<u8>,
    first_at: Instant,
    /// When a datagram last arrived (new or duplicate). The tail request below
    /// needs to know that the burst has gone quiet, which is a different question
    /// from how old the frame is.
    last_at: Instant,
    rounds: u8,
    last_nack: Option<Instant>,
}

impl Slot {
    fn new(id: u32, frags: u16, now: Instant, mtu: usize) -> Self {
        Self {
            id,
            frags,
            have: vec![0; (frags as usize).div_ceil(64).max(1)],
            got: 0,
            highest: 0,
            // The exact length is only known once the last fragment is in; growing
            // into a hole leaves zeroes that the tile parser refuses as trailing or
            // short data, which is the behaviour we want if one ever leaks out.
            buf: Vec::with_capacity(frags as usize * mtu),
            first_at: now,
            last_at: now,
            rounds: 0,
            last_nack: None,
        }
    }

    fn set(&mut self, frag: u16) -> bool {
        self.highest = self.highest.max(frag);
        let (word, bit) = (frag as usize / 64, frag as usize % 64);
        if word >= self.have.len() {
            return false;
        }
        let was = self.have[word] & (1u64 << bit) != 0;
        self.have[word] |= 1u64 << bit;
        if !was {
            self.got += 1;
        }
        !was
    }

    fn complete(&self) -> bool {
        self.got == self.frags
    }

    /// Every hole at or below `up_to`, as one 64-fragment bitmap per window that has
    /// any. A 414 fragment key frame is seven windows, so a lost fragment near the
    /// end is asked for in its own entry rather than silently ignored.
    fn holes(&self, up_to: u16) -> Vec<NackEntry> {
        let mut out = Vec::new();
        let mut base = 0u16;
        while base <= up_to && out.len() < 8 {
            let mut bits = 0u64;
            for i in 0..64u16 {
                let Some(frag) = base.checked_add(i) else { break };
                if frag > up_to || frag >= self.frags {
                    break;
                }
                let (word, bit) = (frag as usize / 64, frag as usize % 64);
                if word < self.have.len() && self.have[word] & (1u64 << bit) == 0 {
                    bits |= 1u64 << i;
                }
            }
            if bits != 0 {
                out.push(NackEntry {
                    frame_id: self.id,
                    base,
                    bits,
                });
            }
            base = base.wrapping_add(64);
        }
        out
    }
}

#[derive(Debug, Default, Clone)]
pub struct ReceiverStats {
    pub datagrams: u64,
    pub bytes: u64,
    pub frames: u64,
    pub dupes: u64,
    /// Frames that arrived with a gap in their fragment numbering.
    pub gapped: u64,
    pub nacks: u64,
    pub acked: u64,
    pub key_requests: u64,
    pub abandoned: u64,
    /// Completions dropped because the picture they describe is older than what the
    /// decoder already painted.
    pub stale: u64,
    /// Frames whose first datagram never arrived at all.
    pub missed: u64,
}

impl ReceiverStats {
    pub fn report(&self) -> String {
        format!(
            "datagrams={} bytes={} frames={} dupes={} gapped={} nacks={} acked={} keyreq={} abandoned={} stale={} missed={}",
            self.datagrams,
            self.bytes,
            self.frames,
            self.dupes,
            self.gapped,
            self.nacks,
            self.acked,
            self.key_requests,
            self.abandoned,
            self.stale,
            self.missed
        )
    }
}

pub struct Receiver {
    cfg: ReceiverConfig,
    slots: VecDeque<Slot>,
    /// The highest frame id the decoder has applied, which is what an Ack claims
    /// and what makes a late completion stale.
    applied: u32,
    seen_highest: u32,
    pending: Vec<Vec<u8>>,
    pub stats: ReceiverStats,
}

impl Receiver {
    pub fn new(cfg: ReceiverConfig) -> Self {
        Self {
            cfg,
            slots: VecDeque::new(),
            applied: 0,
            seen_highest: 0,
            pending: Vec::new(),
            stats: ReceiverStats::default(),
        }
    }

    pub fn session(&self) -> u32 {
        self.cfg.session
    }

    pub fn applied(&self) -> u32 {
        self.applied
    }

    /// Feed one datagram. The returned frame is whole and in wire format; whether
    /// its base is what the decoder holds is the decoder's question, not ours.
    pub fn on_packet(&mut self, packet: &[u8], now: Instant) -> CastResult<Option<Complete>> {
        let h = read_header(packet)?;
        if h.session != self.cfg.session {
            return Ok(None);
        }
        let body = h.body(packet)?;
        match h.kind {
            PacketKind::Data => self.on_data(h, body, now),
            PacketKind::Probe => {
                self.pending.push(control_packet(
                    PacketKind::ProbeAck,
                    self.cfg.session,
                    h.frame_id,
                    body,
                ));
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn on_data(&mut self, h: Header, body: &[u8], now: Instant) -> CastResult<Option<Complete>> {
        if h.frags == 0 || h.frag >= h.frags {
            return Err(CastError::Malformed("fragment index out of range"));
        }
        self.stats.datagrams += 1;
        self.stats.bytes += body.len() as u64;
        if h.frame_id > self.seen_highest + 1 && self.seen_highest != 0 {
            let missed = (h.frame_id - self.seen_highest - 1) as u64;
            self.stats.missed += missed;
        }
        self.seen_highest = self.seen_highest.max(h.frame_id);
        if h.frame_id <= self.applied {
            // The picture it describes has already been replaced on screen.
            self.stats.stale += 1;
            return Ok(None);
        }

        let idx = match self.slots.iter().position(|s| s.id == h.frame_id) {
            Some(i) => i,
            None => {
                if self.slots.len() >= self.cfg.slots.max(1) {
                    let old = self.slots.pop_front().expect("slots is not empty");
                    self.stats.abandoned += 1;
                    self.stats.key_requests += 1;
                    self.pending.push(control_packet(
                        PacketKind::KeyRequest,
                        self.cfg.session,
                        old.id,
                        &[],
                    ));
                }
                self.slots
                    .push_back(Slot::new(h.frame_id, h.frags, now, self.cfg.mtu));
                self.slots.len() - 1
            }
        };
        let slot = &mut self.slots[idx];
        if slot.frags != h.frags {
            return Err(CastError::Malformed("fragment count changed mid-frame"));
        }
        if !slot.set(h.frag) {
            slot.last_at = now;
            self.stats.dupes += 1;
            return Ok(None);
        }
        slot.last_at = now;
        let off = h.frag as usize * self.cfg.mtu;
        if slot.buf.len() < off + body.len() {
            slot.buf.resize(off + body.len(), 0);
        }
        slot.buf[off..off + body.len()].copy_from_slice(body);
        if !self.slots[idx].complete() {
            return Ok(None);
        }
        let slot = self.slots.remove(idx).expect("slot exists");
        self.stats.frames += 1;
        Ok(Some(Complete {
            frame_id: slot.id,
            bytes: slot.buf,
            fragments: slot.frags,
        }))
    }

    /// The decoder accepted the frame. This is the only thing that advances the
    /// picture the sender may cut the next delta against, so it is sent at once.
    pub fn on_applied(&mut self, frame_id: u32) {
        self.applied = self.applied.max(frame_id);
        // A duplicate that arrived between the frame's completion and this call can
        // have opened a new slot for an id that is already done. Drop it here rather
        // than let it age into a key-frame request for a picture on screen.
        self.slots.retain(|s| s.id != frame_id);
        self.stats.acked += 1;
        self.pending.push(control_packet(
            PacketKind::Ack,
            self.cfg.session,
            frame_id,
            &[],
        ));
    }

    /// The decoder refused the frame — its base is not what we hold. Nothing can be
    /// repaired by retransmission now.
    pub fn on_rejected(&mut self, frame_id: u32) {
        self.stats.key_requests += 1;
        self.pending.push(control_packet(
            PacketKind::KeyRequest,
            self.cfg.session,
            frame_id,
            &[],
        ));
    }

    /// Age out frames that a NACK round did not rescue, and ask again for the holes
    /// that a previous round did not fix.
    ///
    /// This is the receiver's only clock: it should be called every few milliseconds,
    /// and the spacing between two requests for one frame — and the grace before the
    /// first one — is [`NACK_INTERVAL`]. Requests deliberately do not come from the
    /// data path: at the first hole, the rest of the frame is usually still in
    /// flight, and asking for it would re-send datagrams that are about to arrive.
    pub fn tick(&mut self, now: Instant) -> CastResult<()> {
        let give_up = self.cfg.give_up;
        let mut i = 0;
        while i < self.slots.len() {
            if now.saturating_duration_since(self.slots[i].first_at) < give_up {
                i += 1;
                continue;
            }
            let slot = self.slots.remove(i).expect("index is in range");
            self.stats.abandoned += 1;
            self.stats.key_requests += 1;
            self.pending.push(control_packet(
                PacketKind::KeyRequest,
                self.cfg.session,
                slot.id,
                &[],
            ));
        }
        let mut i = 0;
        while i < self.slots.len() {
            if self.poll_slot(i, now) {
                continue;
            }
            i += 1;
        }
        Ok(())
    }

    /// Ask for one frame's holes, or give up on it once retransmission has had its
    /// rounds. Returns `true` when the slot was removed.
    fn poll_slot(&mut self, idx: usize, now: Instant) -> bool {
        let slot = &self.slots[idx];
        if slot.complete() {
            return false;
        }
        let due = now.saturating_duration_since(slot.last_nack.unwrap_or(slot.first_at))
            >= NACK_INTERVAL;
        if !due {
            return false;
        }
        let mut holes = slot.holes(slot.highest);
        if holes.is_empty()
            && slot.highest + 1 < slot.frags
            && now.saturating_duration_since(slot.last_at) >= NACK_INTERVAL
        {
            // The *last* fragment was the one lost. No later datagram will ever
            // reveal it as a hole, so once the burst has gone quiet the missing tail
            // is asked for by name. Without this the frame would sit until the
            // give-up age and cost a key frame for one datagram.
            holes = slot.holes(slot.frags - 1);
        }
        if holes.is_empty() {
            // Everything up to the highest fragment seen is present and the burst
            // has not gone quiet yet: the rest is still on its way.
            return false;
        }
        let rounds = slot.rounds;
        if rounds >= MAX_NACK_ROUNDS {
            let id = slot.id;
            self.slots.remove(idx);
            self.stats.abandoned += 1;
            self.stats.key_requests += 1;
            self.pending.push(control_packet(
                PacketKind::KeyRequest,
                self.cfg.session,
                id,
                &[],
            ));
            return true;
        }
        if rounds == 0 {
            self.stats.gapped += 1;
        }
        let slot = &mut self.slots[idx];
        slot.rounds = rounds + 1;
        slot.last_nack = Some(now);
        self.stats.nacks += 1;
        self.pending.push(control_packet(
            PacketKind::Nack,
            self.cfg.session,
            0,
            &nack_body(&holes),
        ));
        false
    }

    /// Everything the receiver wants to say, in order. Cleared by the call.
    pub fn take_control(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending)
    }

    pub fn pending_control(&self) -> usize {
        self.pending.len()
    }
}

fn nack_body(entries: &[NackEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * NackEntry::BYTES);
    for e in entries {
        e.write(&mut out);
    }
    out
}

// ---------------------------------------------------------------- sockets

/// Bind a casting socket with buffers sized for a resync burst.
///
/// 4 MB of receive buffer is about eight photographic key frames: enough that a
/// receiver which is busy decoding one can still hold the next, and small enough
/// that a stopped process cannot eat the machine's memory.
pub fn bind(addr: &str) -> CastResult<UdpSocket> {
    let socket = UdpSocket::bind(addr).map_err(|_| CastError::Io)?;
    tune(&socket, 4 << 20);
    Ok(socket)
}

pub fn tune(socket: &UdpSocket, buffer_bytes: usize) {
    let sref = socket2::SockRef::from(socket);
    // Failing to grow a buffer is not fatal: the kernel's default still works, it
    // just drops a burst sooner, and the retransmission path exists for that.
    let _ = sref.set_recv_buffer_size(buffer_bytes);
    let _ = sref.set_send_buffer_size(buffer_bytes);
    let _ = sref.set_nonblocking(true);
}

pub fn peer_addr(peer: &str) -> CastResult<SocketAddr> {
    use std::net::ToSocketAddrs;
    peer.to_socket_addrs()
        .map_err(|_| CastError::Io)?
        .next()
        .ok_or(CastError::InvalidState("peer address did not resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_sender() -> SenderConfig {
        SenderConfig {
            session: 7,
            ..Default::default()
        }
    }

    fn cfg_receiver() -> ReceiverConfig {
        ReceiverConfig {
            session: 7,
            ..Default::default()
        }
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    fn deliver(
        sender: &mut Sender,
        receiver: &mut Receiver,
        frame_id: u32,
        bytes: &[u8],
        drop: &[u16],
        now: Instant,
    ) -> Option<Complete> {
        let frags = sender.begin_frame(frame_id, bytes);
        let mut scratch = Vec::new();
        let mut whole = None;
        for frag in 0..frags {
            if drop.contains(&frag) {
                continue;
            }
            let pkt = {
                let d = sender.datagram(&mut scratch, frame_id, frag).unwrap();
                d.to_vec()
            };
            if let Some(c) = receiver.on_packet(&pkt, now).unwrap() {
                whole = Some(c);
            }
        }
        whole
    }

    #[test]
    fn a_frame_round_trips_through_fragments_out_of_order_and_duplicated() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        let mut receiver = Receiver::new(cfg_receiver());
        let frame = noise(37_000);
        let now = Instant::now();
        let frags = sender.begin_frame(1, &frame);
        assert_eq!(frags, frag_count(frame.len(), MTU_PAYLOAD));

        let mut scratch = Vec::new();
        let mut whole = None;
        // Reverse order is the worst case for a reassembler that assumes the start.
        for frag in (0..frags).rev() {
            let pkt = sender.datagram(&mut scratch, 1, frag).unwrap().to_vec();
            whole = receiver.on_packet(&pkt, now).unwrap().or(whole);
            // ...and every datagram twice, because UDP may duplicate.
            let _ = receiver.on_packet(&pkt, now).unwrap();
        }
        let got = whole.expect("the last fragment completes the frame");
        assert_eq!(got.frame_id, 1);
        assert_eq!(got.bytes, frame);
        // Every fragment but the one that completed the frame arrived twice, and the
        // extra copy of that last one opens a fresh slot instead of counting as one.
        assert_eq!(receiver.stats.dupes, frags as u64 - 1);
        assert!(receiver.take_control().is_empty(), "nothing was missing");
    }

    #[test]
    fn a_missing_fragment_is_nacked_and_the_retransmit_completes_the_frame() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        let mut receiver = Receiver::new(cfg_receiver());
        let frame = noise(20_000);
        let now = Instant::now();
        let frags = sender.begin_frame(3, &frame);
        let mut scratch = Vec::new();
        let mut whole = None;
        for frag in 0..frags {
            if frag == 4 {
                continue;
            }
            let pkt = sender.datagram(&mut scratch, 3, frag).unwrap().to_vec();
            whole = receiver.on_packet(&pkt, now).unwrap().or(whole);
        }
        assert!(whole.is_none(), "a hole is not a frame");

        // Nothing is said while the frame is still arriving: at the first hole the
        // rest is in flight, and asking for it would re-send datagrams that land a
        // moment later. The request comes from the tick, after the grace.
        assert!(receiver.take_control().is_empty());
        receiver.tick(now + NACK_INTERVAL).unwrap();
        let control = receiver.take_control();
        assert_eq!(control.len(), 1, "one NACK, not one per missing fragment");
        let h = read_header(&control[0]).unwrap();
        assert_eq!(h.kind, PacketKind::Nack);
        assert_eq!(h.session, 7);
        let entries = NackEntry::read(h.body(&control[0]).unwrap()).unwrap();
        let wanted: Vec<u16> = entries.iter().flat_map(|e| e.frags()).collect();
        assert_eq!(wanted, vec![4], "and it names the hole, not the whole frame");

        let reaction = sender.on_packet(&control[0]).unwrap();
        assert_eq!(reaction.packets.len(), 1);
        assert_eq!(sender.stats.retransmits, 1);
        let got = receiver
            .on_packet(&reaction.packets[0], now)
            .unwrap()
            .expect("the retransmit fills the hole");
        assert_eq!(got.bytes, frame);

        // And the ack it produces is what moves the sender's commit floor.
        assert_eq!(sender.commit_floor(), None);
        receiver.on_applied(got.frame_id);
        for pkt in receiver.take_control() {
            sender.on_packet(&pkt).unwrap();
        }
        assert_eq!(sender.commit_floor(), Some(3));
        assert!(sender.stats.evicted == 0);
    }

    #[test]
    fn a_lost_tail_fragment_is_requested_once_the_burst_goes_quiet() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        let mut receiver = Receiver::new(cfg_receiver());
        let frame = noise(20_000);
        let now = Instant::now();
        let frags = sender.begin_frame(5, &frame);
        let mut scratch = Vec::new();
        for frag in 0..frags - 1 {
            let pkt = sender.datagram(&mut scratch, 5, frag).unwrap().to_vec();
            receiver.on_packet(&pkt, now).unwrap();
        }
        // No later fragment exists to overtake the hole, so nothing is due while
        // the burst is still running...
        receiver.tick(now).unwrap();
        assert!(receiver.take_control().is_empty());

        // ...but a quiet interval means the last datagram never made it.
        receiver.tick(now + NACK_INTERVAL).unwrap();
        let control = receiver.take_control();
        assert_eq!(control.len(), 1);
        let entries =
            NackEntry::read(read_header(&control[0]).unwrap().body(&control[0]).unwrap()).unwrap();
        let wanted: Vec<u16> = entries.iter().flat_map(|e| e.frags()).collect();
        assert_eq!(wanted, vec![frags - 1]);

        let reaction = sender.on_packet(&control[0]).unwrap();
        assert_eq!(reaction.packets.len(), 1);
        let got = receiver
            .on_packet(&reaction.packets[0], now + NACK_INTERVAL)
            .unwrap()
            .expect("the tail completes the frame");
        assert_eq!(got.bytes, frame);
    }

    #[test]
    fn the_commit_floor_stops_at_the_first_unacknowledged_frame() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        for id in 1..=3 {
            sender.begin_frame(id, &noise(100));
        }
        let ack = |id: u32| control_packet(PacketKind::Ack, 7, id, &[]);
        sender.on_packet(&ack(3)).unwrap();
        assert_eq!(
            sender.commit_floor(),
            None,
            "an ack for 3 says nothing about 1"
        );
        sender.on_packet(&ack(1)).unwrap();
        assert_eq!(sender.commit_floor(), Some(1));
        sender.on_packet(&ack(2)).unwrap();
        assert_eq!(sender.commit_floor(), Some(3), "the prefix is now contiguous");
    }

    #[test]
    fn the_pacer_lets_one_datagram_through_and_spreads_a_key_frame() {
        let now = Instant::now();
        let mut pacer = Pacer::new(8_000_000, 48_000);
        assert_eq!(
            pacer.reserve(1200, now),
            Duration::ZERO,
            "a steady frame is inside the bucket"
        );
        assert_eq!(pacer.reserve(1200, now), Duration::ZERO);
        // Drain the bucket, then ask for more than it holds.
        let mut at = now;
        let mut total = 0u64;
        for _ in 0..60 {
            let w = pacer.reserve(1200, at);
            total += w.as_micros() as u64;
            at += w;
        }
        assert!(total > 0, "414 fragments cannot leave at once");
        // 62 * 1200 = 74.4 KB against a 48 KB bucket at 8 Mbit/s (1 MB/s): the
        // 26.4 KB over budget is ~26 ms of refill, and the pacer must charge about
        // that and not a round trip per datagram.
        assert!(
            (20_000..=35_000).contains(&total),
            "pacing overhead should be dozens of milliseconds, got {total}us"
        );
    }

    #[test]
    fn a_probe_round_trip_produces_a_measured_rtt() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(1_000_000, 1200), 8_000_000);
        let now = Instant::now();
        let probe = sender.probe_if_due(now).expect("first probe is due");
        assert!(sender.probe_if_due(now).is_none(), "and not two of them");
        let mut receiver = Receiver::new(cfg_receiver());
        receiver.on_packet(&probe, now).unwrap();
        let echo = receiver.take_control().pop().unwrap();
        assert_eq!(read_header(&echo).unwrap().kind, PacketKind::ProbeAck);
        std::thread::sleep(Duration::from_millis(5));
        sender.on_packet(&echo).unwrap();
        assert!(sender.rtt.is_measured());
        assert!(
            sender.rtt.rtt_us >= 5_000,
            "rtt={}us must be at least the sleep",
            sender.rtt.rtt_us
        );
    }

    #[test]
    fn the_governor_halves_on_sustained_loss_and_climbs_without_it() {
        let mut g = RateGovernor::new(20_000_000);
        // Taken after construction: `since` is set inside `new`, and the first
        // window is measured from there.
        let t = Instant::now();
        g.note_sent(1000);
        g.note_lost(100);
        assert!(g.tick(t + Duration::from_millis(1100)));
        assert_eq!(g.bitrate_bps, 10_000_000);

        g.note_sent(1000);
        assert!(g.tick(t + Duration::from_millis(2200)));
        assert_eq!(g.bitrate_bps, 10_625_000, "additive increase");

        // A window with too little traffic says nothing and must not move.
        assert!(!g.tick(t + Duration::from_millis(3300)));
        assert_eq!(g.bitrate_bps, 10_625_000);
    }

    #[test]
    fn a_stale_completion_is_dropped_instead_of_repainting_an_older_picture() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        let mut receiver = Receiver::new(cfg_receiver());
        let now = Instant::now();
        let a = noise(3_000);
        let whole = deliver(&mut sender, &mut receiver, 1, &a, &[2], now);
        assert!(whole.is_none());
        // Meanwhile a later frame lands and is applied.
        let b = noise(3_100);
        let later = deliver(&mut sender, &mut receiver, 2, &b, &[], now).unwrap();
        receiver.on_applied(later.frame_id);
        // Now the straggler's missing piece arrives.
        let mut scratch = Vec::new();
        let pkt = sender.datagram(&mut scratch, 1, 2).unwrap().to_vec();
        assert!(receiver.on_packet(&pkt, now).unwrap().is_none());
        assert_eq!(receiver.stats.stale, 1);
    }

    #[test]
    fn a_kind_or_version_from_the_future_is_refused() {
        let mut pkt = control_packet(PacketKind::Ack, 7, 1, &[]);
        pkt[0] = 2;
        assert!(matches!(
            read_header(&pkt),
            Err(CastError::Unsupported(_))
        ));
        let mut bad = control_packet(PacketKind::Ack, 7, 1, &[]);
        bad[1] = 9;
        assert!(matches!(read_header(&bad), Err(CastError::Unsupported(_))));
        let mut short = control_packet(PacketKind::Ack, 7, 1, &[]);
        short.truncate(15);
        assert!(matches!(read_header(&short), Err(CastError::Malformed(_))));
    }

    #[test]
    fn a_nack_lists_only_the_fragments_it_is_missing() {
        let mut receiver = Receiver::new(cfg_receiver());
        let mut sender = Sender::new(cfg_sender(), Pacer::new(100_000_000, 1 << 20), 8_000_000);
        // 50 fragments, so a hole in the last stretch is a different bitmap word
        // than a hole at the start.
        let frame = noise(60_000);
        assert_eq!(frag_count(frame.len(), MTU_PAYLOAD), 50);
        let now = Instant::now();
        sender.begin_frame(9, &frame);
        let mut scratch = Vec::new();
        let skip: Vec<u16> = (0..3).chain(40..42).collect();
        for frag in 0..50u16 {
            if skip.contains(&frag) {
                continue;
            }
            let pkt = sender.datagram(&mut scratch, 9, frag).unwrap().to_vec();
            receiver.on_packet(&pkt, now).unwrap();
        }
        receiver.tick(now + NACK_INTERVAL).unwrap();
        let control = receiver.take_control();
        assert_eq!(control.len(), 1, "one request for the burst");
        let entries =
            NackEntry::read(read_header(&control[0]).unwrap().body(&control[0]).unwrap()).unwrap();
        let wanted: Vec<u16> = entries.iter().flat_map(|e| e.frags()).collect();
        assert_eq!(wanted, vec![0, 1, 2, 40, 41]);

        let reaction = sender.on_packet(&control[0]).unwrap();
        assert_eq!(reaction.packets.len(), 5, "exactly the holes, nothing else");

        // A second request is spaced, not sent for every datagram that lands in
        // between: respacing is the tick's job and the interval is the limit.
        receiver.tick(now + NACK_INTERVAL / 2).unwrap();
        assert!(receiver.take_control().is_empty(), "too soon to ask again");
        receiver.tick(now + NACK_INTERVAL * 2).unwrap();
        assert_eq!(receiver.take_control().len(), 1);
    }

    #[test]
    fn an_evicted_frame_asks_for_a_key_frame_instead_of_a_retransmit() {
        let mut cfg = cfg_sender();
        cfg.window_frames = 2;
        cfg.window_bytes = 1 << 30;
        let mut sender = Sender::new(cfg, Pacer::new(100_000_000, 1 << 20), 8_000_000);
        for id in 1..=4 {
            sender.begin_frame(id, &noise(100));
        }
        assert!(sender.take_key_demand(), "frames 1 and 2 were evicted");
        let entry = NackEntry {
            frame_id: 1,
            base: 0,
            bits: 1,
        };
        let nack = control_packet(PacketKind::Nack, 7, 0, &nack_body(&[entry]));
        let reaction = sender.on_packet(&nack).unwrap();
        assert!(reaction.packets.is_empty());
        assert!(sender.take_key_demand(), "and a NACK makes it explicit");
    }

    #[test]
    fn a_packet_from_another_session_is_ignored_by_both_ends() {
        let mut sender = Sender::new(cfg_sender(), Pacer::new(1_000_000, 1200), 8_000_000);
        let mut receiver = Receiver::new(cfg_receiver());
        let pkt = control_packet(PacketKind::Ack, 99, 4, &[]);
        let r = sender.on_packet(&pkt).unwrap();
        assert!(r.packets.is_empty() && !r.want_key);
        assert_eq!(sender.commit_floor(), None);
        let now = Instant::now();
        sender.begin_frame(1, &noise(50));
        let mut scratch = Vec::new();
        let mut data = sender.datagram(&mut scratch, 1, 0).unwrap().to_vec();
        // Same port, another casting session: the bytes are well formed and must
        // still be counted by neither end.
        data[2] = 99;
        assert!(receiver.on_packet(&data, now).unwrap().is_none());
        assert_eq!(receiver.stats.datagrams, 0, "nothing was even counted");
    }
}
