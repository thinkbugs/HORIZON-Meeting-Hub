//! The wire format of the self-built channel: a tile codec, not a video codec.
//!
//! This module exists because of a measurement, not a preference. P0-B drove the
//! Media Foundation H.264 decoder through every knob it exposes, and the in-box
//! Microsoft decoder holds 27 frames in flight no matter what is pushed into it
//! (333 ms even with `AVDecNumWorkerThreads = 1`) — see the table in
//! [`crate::codec`]. A codec with a pipeline that long cannot sit inside a 40 ms
//! budget, and no amount of network tuning recovers it. H.264 therefore stays
//! where it belongs: the interop path for receivers that already own a decoder,
//! namely DLNA, AirPlay and Cast. The channel this engine controls end to end
//! speaks screen content instead.
//!
//! - **one frame in, one frame out, by construction.** A tile is compressed on
//!   its own and painted on its own. No inter-frame prediction means nothing to
//!   buffer, nothing to reorder and no GOP to wait for, so latency is the
//!   compressor's cost and nothing else.
//! - **lossless where the content is text.** A whiteboard, a slide, a terminal
//!   and a window caption are why this product exists, and a lossy codec makes
//!   them unreadable at exactly the moment the room is looking at them. Flat
//!   tiles are cheap to compress, so being exact costs less than being lossy.
//! - **lossy only where it wins.** A tile deflate cannot shrink is photographic;
//!   JPEG at an adaptive quality is both smaller and better looking there, so the
//!   codec measures each tile and keeps the smaller payload.
//! - **delta by tile.** Only tiles differing from what the peer actually holds
//!   are sent, which is what turns a pointer moving over a static desktop from
//!   33 MB into a few hundred bytes.
//!
//! The base-tracking rule that makes deltas safe: the encoder diffs against the
//! picture it has *proven* the peer holds. [`WireEncoder::commit`] adopts the
//! tiles it sent; [`WireEncoder::rollback`] puts them back, so a frame the peer
//! never received cannot poison the reference the next frame is cut against. On
//! the receive side the rule is trivially satisfied — every tile is
//! self-contained and the stream is applied in order — so a receiver only has to
//! refuse a delta whose `base_id` is not what it last applied, which is
//! [`CastError::NeedsKeyframe`].
//!
//! ## What this costs, measured
//!
//! Every number below is from this host — an 8-thread desktop with an AMD R5 220
//! driving one 1920x1080 panel — taken by `capture-probe` on the live desktop and
//! `codec-bench` on synthetic frames. The budgets are
//! [`crate::metrics::Budget::LOW_LATENCY_1080P30`].
//!
//! Steady state: 400 frames of a real desktop with the pointer moving, 4.3 tiles
//! and 665 bytes per frame.
//!
//! | stage | p50 | p95 | budget |
//! |---|---|---|---|
//! | copy into the encoder | 0.00 ms | 2.62 ms | 7 ms, shared with capture |
//! | encode | 0.11 ms | 4.71 ms | 12 ms |
//! | decode | 0.05 ms | 0.14 ms | 6 ms |
//! | capture → applied | 0.17 ms | 4.81 ms | 40 ms for the whole pipeline |
//!
//! A resync frame — all 510 tiles of that same desktop, 53,342 bytes — encodes in
//! 5.5 ms and decodes in 2.5 ms. Both inside budget, and neither without lanes:
//! the same frame costs 20.4 ms to encode and 10.0 ms to decode on one thread.
//!
//! Full-frame synthetic 1080p, which a live desktop never quite produces, and the
//! reason the lane count is a parameter at all:
//!
//! | frame | bytes | encode 1 → 8 → 16 lanes | decode 1 → 8 lanes |
//! |---|---|---|---|
//! | ui, every tile lossless | 124,619 | 22 → 9 → 10 ms | 11 → 3 ms |
//! | photo, every tile JPEG | 496,596 | 169 → 31 → 25 ms | 42 → 9 ms |
//! | mixed | 302,337 | 96 → 21 → 19 ms | 26 → 5 ms |
//!
//! Two honest conclusions sit in that table.
//!
//! A photographic key frame costs the sender 25–33 ms even at sixteen lanes,
//! which is outside the 12 ms encode budget and will stay there: the pure-Rust
//! JPEG encoder costs about four times what deflate costs per tile. The codec's
//! answer is not to be faster, it is to be rare — [`WireEncoder::key_threshold`]
//! promotes a delta only past 55% of the grid, and continuous photographic
//! content belongs to the H.264 interop path this module deliberately does not
//! implement.
//!
//! Scaling stops at about eight lanes, and a sixteen-lane run is sometimes slower
//! than an eight-lane one, on both sides. That is 510 tiles divided by the lane
//! floor meeting the memory bandwidth of an integrated board. The floor is what a
//! two-core laptop receiver is designed for, not the sender's best case.

use std::io::Cursor;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use image::codecs::jpeg::{JpegDecoder, JpegEncoder};
use image::{ExtendedColorType, ImageDecoder};

use crate::error::{CastError, CastResult};
use crate::metrics::now_us;

/// Tile edge in pixels. 64 is the unit a delta screen frame is measured in:
/// small enough that a drag-select or a blinking caret touches a handful of
/// tiles, large enough that a tile header is not the payload's competitor.
pub const TILE: u16 = 64;

const MAGIC: u32 = 0x4854_4c31; // "HTL1"
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 32;
/// x, y, w, h, mode, reserved, payload length.
const RECORD_BYTES: usize = 14;
/// Smallest batch of tiles worth handing to another OS thread. A lane costs a
/// thread spawn and a compressor, so a cursor move of four tiles must not pay for
/// either — measured as the point where a full-frame encode stops getting slower
/// with more lanes. The receiver uses the same floor for the same reason.
const TILES_PER_LANE: usize = 32;
/// How many idle lanes an encoder or a decoder keeps warm. Beyond this the pool
/// would hold megabytes of deflate state for a peer that never uses it.
const MAX_POOLED_LANES: usize = 32;

/// How one frame relates to the picture the peer already holds. A key frame is
/// self-contained — every tile in the grid is present — so it is also the resync
/// point any listener can join at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Delta = 0,
    Key = 1,
}

/// How one tile's pixels are carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TileMode {
    /// BGRA bytes verbatim: chosen when compression would only add bytes.
    Raw = 0,
    /// BGRA bytes through deflate — bit-exact, which is what text needs.
    Deflate = 1,
    /// The tile as a JPEG stream, for content deflate cannot help.
    Jpeg = 2,
}

impl TileMode {
    fn from_u8(v: u8) -> CastResult<Self> {
        match v {
            0 => Ok(TileMode::Raw),
            1 => Ok(TileMode::Deflate),
            2 => Ok(TileMode::Jpeg),
            _ => Err(CastError::Malformed("tile mode")),
        }
    }

    pub fn is_lossy(self) -> bool {
        self == TileMode::Jpeg
    }
}

/// A pixel rectangle in the frame's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn x1(&self) -> u32 {
        self.x as u32 + self.w as u32
    }

    pub fn y1(&self) -> u32 {
        self.y as u32 + self.h as u32
    }
}

/// A tile coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub cx: u16,
    pub cy: u16,
}

/// The tile lattice over a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub width: u16,
    pub height: u16,
    pub tile: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Grid {
    pub fn new(width: u16, height: u16, tile: u16) -> Self {
        let tile = tile.max(1);
        Self {
            width,
            height,
            tile,
            cols: width.div_ceil(tile).max(1),
            rows: height.div_ceil(tile).max(1),
        }
    }

    pub fn count(&self) -> usize {
        self.cols as usize * self.rows as usize
    }

    /// The pixel rectangle a tile covers, cropped to the frame. Edge tiles are
    /// short, which is why the size travels in each record instead of relying on
    /// the decoder agreeing about a constant.
    pub fn rect(&self, t: Tile) -> Rect {
        let x = t.cx * self.tile;
        let y = t.cy * self.tile;
        Rect {
            x,
            y,
            w: self.width.saturating_sub(x).min(self.tile),
            h: self.height.saturating_sub(y).min(self.tile),
        }
    }

    /// Inclusive tile-coordinate range covering a pixel rectangle, clamped to the
    /// grid. A dirty rect from DXGI is allowed to run off the edge of the desktop;
    /// the clamped answer is the tiles that actually exist.
    pub fn tile_range(&self, r: Rect) -> Option<(u16, u16, u16, u16)> {
        if r.w == 0 || r.h == 0 {
            return None;
        }
        let t = self.tile.max(1) as u32;
        let x0 = (r.x as u32 / t).min(self.cols as u32 - 1) as u16;
        let y0 = (r.y as u32 / t).min(self.rows as u32 - 1) as u16;
        let x1 = (r.x1().saturating_sub(1) / t).min(self.cols as u32 - 1) as u16;
        let y1 = (r.y1().saturating_sub(1) / t).min(self.rows as u32 - 1) as u16;
        Some((x0, y0, x1, y1))
    }

    pub fn all(&self) -> Vec<Tile> {
        let mut v = Vec::with_capacity(self.count());
        for cy in 0..self.rows {
            for cx in 0..self.cols {
                v.push(Tile { cx, cy });
            }
        }
        v
    }
}

/// Row-major bit set over the grid, so overlapping dirty rects cannot make one
/// tile be sent twice inside a frame.
struct TileMask {
    bits: Vec<u64>,
}

impl TileMask {
    fn new(len: usize) -> Self {
        Self {
            bits: vec![0; len.div_ceil(64).max(1)],
        }
    }

    /// Set a bit, returning whether it was newly set.
    fn set(&mut self, i: usize) -> bool {
        let (word, bit) = (i / 64, i % 64);
        let was = self.bits[word] & (1u64 << bit);
        self.bits[word] |= 1u64 << bit;
        was == 0
    }

    fn clear(&mut self) {
        self.bits.iter_mut().for_each(|w| *w = 0);
    }

    #[cfg(test)]
    fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// Bytes-per-frame governor behind the JPEG quality knob.
///
/// Lossless tiles are never traded away for it — a soft window caption is not a
/// bitrate saving — so this steers only the quality of tiles that were already
/// going to be lossy.
#[derive(Debug, Clone, Copy)]
pub struct RateControl {
    pub quality: u8,
    pub target_bytes_per_frame: usize,
    pub min_quality: u8,
    pub max_quality: u8,
    ema_bytes: f64,
}

impl RateControl {
    pub fn new(bitrate_bps: u64, fps: u32) -> Self {
        let mut this = Self {
            quality: 82,
            target_bytes_per_frame: 0,
            min_quality: 40,
            max_quality: 92,
            ema_bytes: 0.0,
        };
        this.retune(bitrate_bps, fps);
        this
    }

    /// Move the target without forgetting the quality the governor has learned.
    /// The transport retunes this from its own loss estimate, and a rate change is
    /// a new target rather than a new picture.
    pub fn retune(&mut self, bitrate_bps: u64, fps: u32) {
        self.target_bytes_per_frame =
            (bitrate_bps / 8 / fps.max(1) as u64).max(1024) as usize;
    }

    /// Fold in one encoded frame and move quality toward the target. The ±15%
    /// deadband is the whole design: chasing a target one frame at a time makes a
    /// scrollbar drag look like the picture is breathing.
    pub fn observe(&mut self, bytes: usize) {
        let alpha = 0.25;
        self.ema_bytes = if self.ema_bytes == 0.0 {
            bytes as f64
        } else {
            alpha * bytes as f64 + (1.0 - alpha) * self.ema_bytes
        };
        let t = self.target_bytes_per_frame as f64;
        if self.ema_bytes > t * 1.15 {
            self.quality = self.quality.saturating_sub(4).max(self.min_quality);
        } else if self.ema_bytes < t * 0.85 {
            self.quality = (self.quality + 2).min(self.max_quality);
        }
    }

    pub fn ema_bytes(&self) -> f64 {
        self.ema_bytes
    }

    /// The frame is over its target, so a tile that merely *resists* deflate is
    /// worth a JPEG trial even if it looks compressible. This is the only way the
    /// governor can act on photographic content at all: quality moves the JPEG
    /// encoder, and a codec that never reaches the JPEG branch has no knob left.
    pub fn over_target(&self) -> bool {
        self.ema_bytes > self.target_bytes_per_frame as f64 * 1.15
    }
}

/// The result of encoding one frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub bytes: Vec<u8>,
    pub frame_id: u32,
    /// The frame the peer must already hold for this one to be complete.
    pub base_id: u32,
    pub kind: FrameKind,
    pub capture_us: u64,
    pub tiles: usize,
    pub lossless: usize,
    pub lossy: usize,
    /// Compressor wall cost — the number this codec exists to win on.
    pub encode_us: u64,
}

struct Payload {
    mode: TileMode,
    bytes: Vec<u8>,
}

/// One lane's reusable working set: the deflate compressor, the tight tile pixels,
/// the BGRA→RGB shuffle and the two compressed outputs.
///
/// The compressor is the reason this type exists. `flate2`'s stream API builds a
/// fresh deflate state — a few hundred kilobytes of tables, allocated and
/// initialised — per stream, and the first version of this codec opened one per
/// tile. At 1080p a key frame is 510 tiles, so the frame had a cost floor of
/// "510 state builds" that no number of threads could remove: measured on this
/// host, a full-desktop encode took 26.7 ms with one thread and 24.9 ms with
/// eight. Reused per lane, the same frame is a lane-count fixed cost instead, and
/// the pool on [`WireEncoder`] keeps even the lanes alive between frames.
struct Lane {
    compress: Compress,
    pixels: Vec<u8>,
    deflate: Vec<u8>,
    rgb: Vec<u8>,
    jpeg: Vec<u8>,
    /// Size of the tile currently in `pixels`, so the two compressors do not have
    /// to be told the grid's geometry again.
    tile_w: usize,
    tile_h: usize,
}

impl Lane {
    fn new() -> Self {
        Self {
            compress: Compress::new(Compression::fast(), false),
            pixels: Vec::new(),
            deflate: Vec::new(),
            rgb: Vec::new(),
            jpeg: Vec::new(),
            tile_w: 0,
            tile_h: 0,
        }
    }

    /// Copy one tile out of a padded source into tight storage, so the compressors
    /// see a rectangle and not a stride.
    fn grab(&mut self, src: &[u8], src_stride: usize, r: Rect) {
        let row = r.w as usize * 4;
        self.tile_w = r.w as usize;
        self.tile_h = r.h as usize;
        self.pixels.clear();
        self.pixels.reserve(row * r.h as usize);
        for y in 0..r.h as usize {
            let s = (r.y as usize + y) * src_stride + r.x as usize * 4;
            self.pixels.extend_from_slice(&src[s..s + row]);
        }
    }

    /// Deflate the tile just grabbed; returns the compressed length. Worst-case
    /// deflate output is a few bytes longer than its input, and reserving for that
    /// is what lets the call below reuse one buffer forever instead of growing it.
    fn deflate(&mut self) -> usize {
        let raw = self.pixels.len();
        self.compress.reset();
        self.deflate.clear();
        self.deflate.reserve(raw + 512);
        let ok = matches!(
            self.compress
                .compress_vec(&self.pixels, &mut self.deflate, FlushCompress::Finish),
            Ok(Status::StreamEnd)
        );
        if ok {
            self.deflate.len()
        } else {
            raw + 1
        }
    }

    /// JPEG the tile just grabbed, into reusable storage; returns the length.
    ///
    /// The BGRA→RGB shuffle stays here rather than in
    /// [`codec`](crate::codec)'s SSE2 converter, which produces NV12 for H.264 and
    /// would have to be a second, different kernel.
    fn jpeg(&mut self, quality: u8) -> Option<usize> {
        let (w, h) = (self.tile_w, self.tile_h);
        self.rgb.clear();
        self.rgb.resize(w * h * 3, 0);
        for (dst, src) in self.rgb.chunks_exact_mut(3).zip(self.pixels.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
        }
        self.jpeg.clear();
        JpegEncoder::new_with_quality(&mut self.jpeg, quality)
            .encode(&self.rgb, w as u32, h as u32, ExtendedColorType::Rgb8)
            .ok()?;
        Some(self.jpeg.len())
    }
}

/// One frame's worth of tile encoder.
///
/// `base` is the picture the peer is proven to hold; `staged` is the picture the
/// frame just encoded describes. Between frames the two are identical, which is
/// what makes [`WireEncoder::rollback`] a copy out of `base` rather than an undo
/// log — and why both buffers only ever move by the dirty area, never by a
/// full-frame memcpy.
pub struct WireEncoder {
    grid: Grid,
    base: Vec<u8>,
    staged: Vec<u8>,
    touched: Vec<Tile>,
    mask: TileMask,
    rate: RateControl,
    frame_id: u32,
    last_committed: u32,
    /// Kept so a retune from the transport can recompute the per-frame target
    /// without the caller having to remember what this encoder was built for.
    fps: u32,
    threads: usize,
    /// Idle lanes, kept alive across frames so a compressor is built once per lane
    /// per process rather than once per tile.
    pool: std::sync::Mutex<Vec<Lane>>,
    /// A tile whose deflate output exceeds this fraction of its raw size gets a
    /// JPEG trial.
    ///
    /// The line is one bit per pixel — about 2 KB for a 64×64 BGRA tile — because
    /// that is where JPEG starts to win on this codec's own measurements, not
    /// because a ratio of "colour" was estimated: a dithered gradient costs 5 KB
    /// through deflate (ratio 0.30) and 1.4 KB as JPEG, while dense text costs
    /// 240 bytes and never reaches the trial. Ratio alone was the first version's
    /// gate at 0.62, and a full-desktop photographic frame went through it
    /// untouched: 510 tiles, 2.5 MB, `0` of them lossy. The margin rule in
    /// `WireEncoder::encode_one` still decides the winner, so a trial that
    /// loses costs a little CPU and no quality.
    pub max_deflate_ratio: f64,
    /// Fraction of the grid that may change before a delta is promoted to a key
    /// frame, at which point re-sending everything is no more expensive and every
    /// listener gains a resync point.
    pub key_threshold: f64,
    /// `false` pins every tile to the lossless path: a whiteboard session where
    /// one soft stroke costs more than the bitrate.
    pub allow_lossy: bool,
}

impl WireEncoder {
    pub fn new(width: u16, height: u16, bitrate_bps: u64, fps: u32) -> Self {
        Self::with_tile(width, height, bitrate_bps, fps, TILE)
    }

    pub fn with_tile(width: u16, height: u16, bitrate_bps: u64, fps: u32, tile: u16) -> Self {
        let grid = Grid::new(width, height, tile);
        let bytes = grid.width as usize * grid.height as usize * 4;
        Self {
            grid,
            base: vec![0; bytes],
            staged: vec![0; bytes],
            touched: Vec::new(),
            mask: TileMask::new(grid.count()),
            rate: RateControl::new(bitrate_bps, fps),
            frame_id: 0,
            last_committed: 0,
            fps: fps.max(1),
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(1, 8),
            pool: std::sync::Mutex::new(Vec::new()),
            max_deflate_ratio: 0.12,
            key_threshold: 0.55,
            allow_lossy: true,
        }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn quality(&self) -> u8 {
        self.rate.quality
    }

    /// Retune the JPEG quality target from the transport's estimate of what the
    /// link will carry. Only the lossy knob moves; lossless tiles are never traded
    /// away for a bitrate, and the learned quality survives the retune.
    pub fn set_bitrate(&mut self, bitrate_bps: u64) {
        self.rate.retune(bitrate_bps, self.fps);
    }

    pub fn set_threads(&mut self, n: usize) {
        self.threads = n.clamp(1, 16);
    }

    /// Encode one captured frame. `src` is BGRA with the capture's own (possibly
    /// padded) stride; `dirty` is what the operating system says changed.
    ///
    /// An empty `dirty` means the capture reported no change and the frame comes
    /// back with zero tiles, which is the correct and cheap answer — a receiver
    /// still gets its sequence number advanced. A *refresh* is a different
    /// request: pass `force_key` when a peer joins, when one reports
    /// [`CastError::NeedsKeyframe`], or when the channel wants a resync point.
    /// The first frame that carries anything is promoted to a key on its own, so
    /// a caller that forgets to force one still cannot strand a listener.
    pub fn encode(
        &mut self,
        src: &[u8],
        src_stride: usize,
        dirty: &[Rect],
        capture_us: u64,
    ) -> CastResult<EncodedFrame> {
        self.encode_with(src, src_stride, dirty, capture_us, false)
    }

    pub fn encode_with(
        &mut self,
        src: &[u8],
        src_stride: usize,
        dirty: &[Rect],
        capture_us: u64,
        force_key: bool,
    ) -> CastResult<EncodedFrame> {
        let t0 = now_us();
        if src_stride < self.grid.width as usize * 4 {
            return Err(CastError::Malformed("capture stride is shorter than the row"));
        }
        self.frame_id += 1;
        self.touched.clear();
        self.mask.clear();

        let candidates = if force_key {
            self.grid.all()
        } else {
            let mut v = Vec::new();
            for r in dirty {
                let Some((x0, y0, x1, y1)) = self.grid.tile_range(*r) else {
                    continue;
                };
                for cy in y0..=y1 {
                    for cx in x0..=x1 {
                        if self.mask.set(self.index(cx, cy)) {
                            v.push(Tile { cx, cy });
                        }
                    }
                }
            }
            v
        };

        // Only tiles that really differ reach the wire. This step is what turns
        // "a window moved" into the few hundred kilobytes it costs instead of the
        // whole desktop.
        let mut changed: Vec<Tile> = if force_key {
            candidates
        } else {
            candidates
                .into_iter()
                .filter(|t| self.tile_changed(src, src_stride, *t))
                .collect()
        };
        changed.sort_by_key(|t| t.cy * self.grid.cols + t.cx);

        let key = force_key
            || (self.last_committed == 0 && !changed.is_empty())
            || changed.len() as f64 > self.key_threshold * self.grid.count() as f64;
        let tiles = if key {
            // A key frame has to describe the current picture everywhere, not just
            // where this capture reported motion.
            self.grid.all()
        } else {
            changed
        };
        self.touched.clone_from(&tiles);

        let payloads = self.encode_tiles(src, src_stride, &tiles);
        let mut lossless = 0usize;
        let mut lossy = 0usize;
        for p in &payloads {
            if p.mode.is_lossy() {
                lossy += 1;
            } else {
                lossless += 1;
            }
        }

        let base_id = self.last_committed;
        let kind = if key { FrameKind::Key } else { FrameKind::Delta };
        let mut bytes = Vec::with_capacity(HEADER_BYTES + tiles.len() * 512);
        write_header(
            &mut bytes,
            kind,
            self.grid.tile,
            self.frame_id,
            base_id,
            capture_us,
            self.grid.width,
            self.grid.height,
            tiles.len(),
        );
        for (t, p) in tiles.iter().zip(payloads.iter()) {
            let r = self.grid.rect(*t);
            bytes.extend_from_slice(&r.x.to_le_bytes());
            bytes.extend_from_slice(&r.y.to_le_bytes());
            bytes.extend_from_slice(&r.w.to_le_bytes());
            bytes.extend_from_slice(&r.h.to_le_bytes());
            bytes.push(p.mode as u8);
            bytes.push(0);
            bytes.extend_from_slice(&(p.bytes.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&p.bytes);
        }

        self.stage_tiles(src, src_stride, &tiles);
        let encode_us = now_us() - t0;
        self.rate.observe(bytes.len());
        Ok(EncodedFrame {
            bytes,
            frame_id: self.frame_id,
            base_id,
            kind,
            capture_us,
            tiles: tiles.len(),
            lossless,
            lossy,
            encode_us,
        })
    }

    /// The peer has this frame. Its tiles are now the reference.
    pub fn commit(&mut self, frame_id: u32) {
        if frame_id != self.frame_id {
            // An acknowledgement for a frame already rolled back. Adopting it
            // would move the reference to a picture nobody holds.
            return;
        }
        std::mem::swap(&mut self.base, &mut self.staged);
        self.last_committed = frame_id;
        self.touched.clear();
        self.mask.clear();
    }

    /// The peer never got this frame. Put the reference back the way it was.
    pub fn rollback(&mut self) {
        let tiles = std::mem::take(&mut self.touched);
        let width = self.grid.width as usize;
        for t in tiles {
            let r = self.grid.rect(t);
            // `base` was never touched, so it still holds the truth for this tile.
            copy_tile_into(&self.base, &mut self.staged, width, r);
        }
        self.mask.clear();
    }

    /// The picture the peer holds, tight-stride BGRA.
    pub fn reference(&self) -> &[u8] {
        &self.base
    }

    pub fn last_committed(&self) -> u32 {
        self.last_committed
    }

    fn index(&self, cx: u16, cy: u16) -> usize {
        cy as usize * self.grid.cols as usize + cx as usize
    }

    fn tile_changed(&self, src: &[u8], src_stride: usize, t: Tile) -> bool {
        let r = self.grid.rect(t);
        let row_bytes = r.w as usize * 4;
        for row in 0..r.h as usize {
            let d = (r.y as usize + row) * self.grid.width as usize * 4 + r.x as usize * 4;
            let s = (r.y as usize + row) * src_stride + r.x as usize * 4;
            // Slice equality lowers to memcmp, which is already wider than any
            // hand-rolled u64 loop here would be.
            if self.base[d..d + row_bytes] != src[s..s + row_bytes] {
                return true;
            }
        }
        false
    }

    fn stage_tiles(&mut self, src: &[u8], src_stride: usize, tiles: &[Tile]) {
        let width = self.grid.width as usize;
        for t in tiles {
            let r = self.grid.rect(*t);
            let row_bytes = r.w as usize * 4;
            for row in 0..r.h as usize {
                let s = (r.y as usize + row) * src_stride + r.x as usize * 4;
                let d = (r.y as usize + row) * width * 4 + r.x as usize * 4;
                self.staged[d..d + row_bytes].copy_from_slice(&src[s..s + row_bytes]);
            }
        }
    }

    fn encode_tiles(&self, src: &[u8], src_stride: usize, tiles: &[Tile]) -> Vec<Payload> {
        if tiles.is_empty() {
            return Vec::new();
        }
        // A lane is an OS thread, so ten tiles of work do not pay for one: below
        // the threshold, going parallel makes a cursor move *slower* than staying
        // on the caller's thread. The divisor is the smallest batch worth a lane.
        let lanes = if tiles.len() < TILES_PER_LANE * 2 {
            1
        } else {
            self.threads.min(tiles.len() / TILES_PER_LANE)
        };
        if lanes < 2 {
            let mut lane = self.take_lane();
            let out = tiles
                .iter()
                .map(|t| self.encode_one(&mut lane, src, src_stride, *t))
                .collect();
            self.give_lane(lane);
            return out;
        }
        // Dynamic distribution. Tiles are row-ordered, which means a contiguous
        // range is a contiguous piece of *content*: a static partition hands one
        // lane the whole photographic region and lets the others finish early, so
        // the frame costs what its slowest lane cost. One shared cursor makes
        // per-tile cost the only thing that decides who does what.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let parts: Vec<Vec<(usize, Payload)>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..lanes)
                .map(|_| {
                    s.spawn(|| {
                        let mut lane = self.take_lane();
                        let mut part = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(t) = tiles.get(i) else { break };
                            part.push((i, self.encode_one(&mut lane, src, src_stride, *t)));
                        }
                        self.give_lane(lane);
                        part
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("tile encoder lane panicked"))
                .collect()
        });
        // Back into tile order. The record stream is positional, so this is about
        // a predictable byte stream rather than correctness.
        let mut slots: Vec<Option<Payload>> = (0..tiles.len()).map(|_| None).collect();
        for part in parts {
            for (i, p) in part {
                slots[i] = Some(p);
            }
        }
        slots.into_iter().map(|p| p.expect("tile lost its lane")).collect()
    }

    /// Borrow a lane from the pool, or build one. Locked once per lane per frame,
    /// never per tile.
    fn take_lane(&self) -> Lane {
        let mut pool = self.pool.lock().unwrap_or_else(|p| p.into_inner());
        pool.pop().unwrap_or_else(Lane::new)
    }

    fn give_lane(&self, lane: Lane) {
        let mut pool = self.pool.lock().unwrap_or_else(|p| p.into_inner());
        if pool.len() < MAX_POOLED_LANES {
            pool.push(lane);
        }
    }

    /// One tile: grab, deflate, then let JPEG at it if it resisted deflate **or**
    /// if the rate governor is short of bytes. Keeping the smaller output by a
    /// clear margin is a measured decision; the alternative is a heuristic about
    /// colour counts that has to be re-tuned for every desktop.
    fn encode_one(&self, lane: &mut Lane, src: &[u8], src_stride: usize, t: Tile) -> Payload {
        let r = self.grid.rect(t);
        lane.grab(src, src_stride, r);
        let raw_len = lane.pixels.len();
        let n = lane.deflate();
        let ratio = n as f64 / raw_len.max(1) as f64;
        if self.allow_lossy && (ratio > self.max_deflate_ratio || self.rate.over_target()) {
            if let Some(j) = lane.jpeg(self.rate.quality) {
                if j * 4 < n * 3 {
                    return Payload {
                        mode: TileMode::Jpeg,
                        bytes: lane.jpeg.clone(),
                    };
                }
            }
        }
        if n < raw_len {
            Payload {
                mode: TileMode::Deflate,
                bytes: lane.deflate[..n].to_vec(),
            }
        } else {
            Payload {
                mode: TileMode::Raw,
                bytes: lane.pixels.clone(),
            }
        }
    }
}

/// A tile region the receiver has to repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Update {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// One frame as the receiver sees it.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub frame_id: u32,
    pub base_id: u32,
    pub kind: FrameKind,
    pub capture_us: u64,
    pub updates: Vec<Update>,
    pub lossy: usize,
    pub decode_us: u64,
}

/// Scratch for one decoding lane.
///
/// The receiver's equivalent of [`Lane`], and for the same measured reason: a
/// `Decompress` is a few hundred kilobytes of tables, and the pure-Rust JPEG
/// decoder is slower per tile than the encoder was. `codec_bench` on a
/// photographic 1080p key — 510 tiles, every one of them JPEG — cost the single
/// threaded receiver 54 ms, nine times the 6 ms the budget table allows for the
/// whole stage. A key frame is also the worst moment to stall: it is what a new
/// listener joins on, and content that fills the screen with photographs keeps
/// promoting deltas to keys.
struct DecLane {
    dz: Decompress,
    px: Vec<u8>,
    rgb: Vec<u8>,
}

impl DecLane {
    fn new() -> Self {
        Self {
            dz: Decompress::new(false),
            px: Vec::new(),
            rgb: Vec::new(),
        }
    }

    /// Paint `records` into `fb`, whose rows are `row_bytes` apart and whose first
    /// row is the desktop's row `y0`.
    ///
    /// The `y0` shift is the whole reason this is a band rather than a tile list:
    /// a grid row owns an exclusive slice of the framebuffer, so several lanes can
    /// write real pixels at once with no `unsafe`, no lock on the buffer, and no
    /// second 8 MB picture to copy out of afterwards.
    fn paint(
        &mut self,
        fb: &mut [u8],
        row_bytes: usize,
        y0: u16,
        records: &[&Record],
        width: u16,
    ) -> CastResult<usize> {
        let mut lossy = 0usize;
        for rec in records {
            if rec.y < y0 {
                return Err(CastError::Malformed("tile painted above its band"));
            }
            let r = Rect::new(rec.x, rec.y - y0, rec.w, rec.h);
            // `parse` only promises a tile fits the *frame*; a hand-built record
            // could sit off the grid and reach past the band this lane owns. The
            // check is per record, not per pixel, and it is the only thing between
            // a peer's bytes and a slice index.
            if r.y as usize + r.h as usize > fb.len() / row_bytes {
                return Err(CastError::Malformed("tile does not fit its band"));
            }
            match rec.mode {
                TileMode::Raw => self.write_bgra(&rec.payload, fb, row_bytes, r)?,
                TileMode::Deflate => self.apply_deflate(&rec.payload, fb, row_bytes, r)?,
                TileMode::Jpeg => {
                    jpeg_decode(&rec.payload, r, &mut self.px, &mut self.rgb)?;
                    write_rgb(&self.rgb, fb, r, width);
                    lossy += 1;
                }
            }
        }
        Ok(lossy)
    }

    fn write_bgra(
        &mut self,
        bgra: &[u8],
        fb: &mut [u8],
        row_bytes: usize,
        r: Rect,
    ) -> CastResult<()> {
        let need = r.w as usize * r.h as usize * 4;
        if bgra.len() != need {
            return Err(CastError::Malformed("tile payload length"));
        }
        copy_tile_in(bgra, fb, row_bytes / 4, r);
        Ok(())
    }

    /// Inflate one tile straight into the framebuffer.
    ///
    /// The output room is the tile's own byte count, which is the guard that keeps
    /// a peer from turning a 200-byte payload into a huge allocation: a stream that
    /// wants more than one tile of pixels does not finish, and a stream that
    /// produces less is not a tile.
    fn apply_deflate(
        &mut self,
        payload: &[u8],
        fb: &mut [u8],
        row_bytes: usize,
        r: Rect,
    ) -> CastResult<()> {
        let need = r.w as usize * r.h as usize * 4;
        self.dz.reset(false);
        self.px.clear();
        self.px.reserve(need);
        let done = matches!(
            self.dz
                .decompress_vec(payload, &mut self.px, FlushDecompress::Finish),
            Ok(Status::StreamEnd)
        );
        if !done || self.px.len() != need {
            return Err(CastError::Malformed(
                "deflate tile did not fill its rectangle",
            ));
        }
        copy_tile_in(&self.px, fb, row_bytes / 4, r);
        Ok(())
    }
}

/// A JPEG tile decodes to RGB, and its chroma is 4:2:0 or 4:2:2 depending on
/// what the encoder picked — never 4:4:4, which is why a text tile must go
/// through the lossless path instead of trusting this one.
///
/// `fb` is one lane's band, so `r` is already expressed relative to its first row.
fn write_rgb(rgb: &[u8], fb: &mut [u8], r: Rect, width: u16) {
    let row_px = r.w as usize;
    for row in 0..r.h as usize {
        for col in 0..row_px {
            let s = row * row_px * 3 + col * 3;
            let d = (r.y as usize + row) * width as usize * 4 + (r.x as usize + col) * 4;
            fb[d] = rgb[s + 2];
            fb[d + 1] = rgb[s + 1];
            fb[d + 2] = rgb[s];
            fb[d + 3] = 0xff;
        }
    }
}

/// The receiver side: a framebuffer and the rules for painting tiles into it.
pub struct WireDecoder {
    grid: Grid,
    fb: Vec<u8>,
    last_applied: u32,
    threads: usize,
    /// Idle lanes, kept alive across frames for the reason [`WireEncoder::pool`]
    /// documents.
    pool: std::sync::Mutex<Vec<DecLane>>,
}

impl WireDecoder {
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_tile(width, height, TILE)
    }

    pub fn with_tile(width: u16, height: u16, tile: u16) -> Self {
        let grid = Grid::new(width, height, tile);
        let fb = vec![0u8; grid.width as usize * grid.height as usize * 4];
        Self {
            grid,
            fb,
            last_applied: 0,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .clamp(1, 8),
            pool: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn set_threads(&mut self, n: usize) {
        self.threads = n.clamp(1, 16);
    }

    pub fn width(&self) -> u16 {
        self.grid.width
    }

    pub fn height(&self) -> u16 {
        self.grid.height
    }

    /// Tight-stride BGRA of everything received so far — the surface a presenter
    /// uploads.
    pub fn frame(&self) -> &[u8] {
        &self.fb
    }

    pub fn frame_mut(&mut self) -> &mut [u8] {
        &mut self.fb
    }

    pub fn last_applied(&self) -> u32 {
        self.last_applied
    }

    pub fn decode(&mut self, bytes: &[u8]) -> CastResult<DecodedFrame> {
        let t0 = now_us();
        let f = parse(bytes)?;
        if f.width != self.grid.width || f.height != self.grid.height {
            return Err(CastError::Malformed("frame size changed mid-session"));
        }
        if f.kind == FrameKind::Delta && f.base_id != self.last_applied {
            // The reference this delta was cut against is not what we hold.
            // Painting it would leave tiles of an older picture on screen and the
            // sender has no way to find out, so refuse and ask for a key frame.
            return Err(CastError::NeedsKeyframe);
        }
        let lossy = self.paint(&f.records)?;
        let updates = f
            .records
            .iter()
            .map(|r| Update {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
            })
            .collect();
        self.last_applied = f.frame_id;
        Ok(DecodedFrame {
            frame_id: f.frame_id,
            base_id: f.base_id,
            kind: f.kind,
            capture_us: f.capture_us,
            updates,
            lossy,
            decode_us: now_us() - t0,
        })
    }

    /// Paint every record of one frame, on as many lanes as the record count is
    /// worth — the mirror of [`WireEncoder::encode_tiles`], with the same floor.
    ///
    /// The unit here is a grid *row*, not a tile, and that is a deliberate
    /// difference. Every tile in a grid row owns the same exclusive band of
    /// framebuffer bytes, so handing whole bands to lanes needs no lock on the
    /// framebuffer, no `unsafe` and no second 8 MB picture to gather from
    /// afterwards. Stealing bands with a shared cursor would mean holding that
    /// lock for the length of a paint, so the lanes take rows round-robin instead;
    /// round-robin rather than contiguous ranges because photographic rows sit next
    /// to each other, and giving one lane the whole photograph is the exact
    /// mistake the encoder's cursor exists to avoid.
    fn paint(&mut self, records: &[Record]) -> CastResult<usize> {
        let grid = self.grid;
        let row_bytes = grid.width as usize * 4;
        let rows = grid.rows as usize;
        let mut groups: Vec<Vec<&Record>> = vec![Vec::new(); rows];
        for r in records {
            let cy = (r.y as usize / grid.tile as usize).min(rows - 1);
            groups[cy].push(r);
        }
        // One mutable slice per non-empty grid row.
        let mut rest: &mut [u8] = self.fb.as_mut_slice();
        let mut bands: Vec<(u16, &mut [u8], &[&Record])> = Vec::with_capacity(rows);
        for cy in 0..rows {
            let y = cy as u16 * grid.tile;
            let h = grid.height.saturating_sub(y).min(grid.tile) as usize;
            let (head, tail) = rest.split_at_mut(h * row_bytes);
            if !groups[cy].is_empty() {
                bands.push((y, head, &groups[cy]));
            }
            rest = tail;
        }

        let lanes = if records.len() < TILES_PER_LANE * 2 {
            1
        } else {
            self.threads.min(bands.len())
        };
        if lanes < 2 {
            // The free functions take the pool field, not `self`, so the band
            // slices above — which borrow `self.fb` — stay alive across the call.
            let mut lane = take_dlane(&self.pool);
            let mut lossy = 0usize;
            let mut failed = None;
            for (y0, fb, recs) in bands.iter_mut() {
                match lane.paint(fb, row_bytes, *y0, recs, grid.width) {
                    Ok(n) => lossy += n,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            give_dlane(&self.pool, lane);
            return match failed {
                Some(e) => Err(e),
                None => Ok(lossy),
            };
        }

        let mut parts: Vec<Vec<(u16, &mut [u8], &[&Record])>> =
            (0..lanes).map(|_| Vec::new()).collect();
        for (i, band) in bands.into_iter().enumerate() {
            parts[i % lanes].push(band);
        }
        let pool = &self.pool;
        let results: Vec<(usize, Option<CastError>)> = std::thread::scope(|s| {
            let handles: Vec<_> = parts
                .into_iter()
                .map(|mine| {
                    s.spawn(move || {
                        let mut lane = take_dlane(pool);
                        let mut lossy = 0usize;
                        let mut failed = None;
                        for (y0, mut fb, recs) in mine {
                            match lane.paint(&mut fb, row_bytes, y0, recs, grid.width) {
                                Ok(n) => lossy += n,
                                Err(e) => {
                                    failed = Some(e);
                                    break;
                                }
                            }
                        }
                        give_dlane(pool, lane);
                        (lossy, failed)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("decode lane panicked"))
                .collect()
        });
        let mut lossy = 0usize;
        for (n, failed) in results {
            if let Some(e) = failed {
                return Err(e);
            }
            lossy += n;
        }
        Ok(lossy)
    }
}

/// Free rather than methods because a lane is taken while the framebuffer is
/// already lent out to a band, and `&self` would collide with it.
fn take_dlane(pool: &std::sync::Mutex<Vec<DecLane>>) -> DecLane {
    let mut pool = pool.lock().unwrap_or_else(|p| p.into_inner());
    pool.pop().unwrap_or_else(DecLane::new)
}

fn give_dlane(pool: &std::sync::Mutex<Vec<DecLane>>, lane: DecLane) {
    let mut pool = pool.lock().unwrap_or_else(|p| p.into_inner());
    if pool.len() < MAX_POOLED_LANES {
        pool.push(lane);
    }
}

/// Everything a frame container says, without applying it. The transport uses
/// this to decide whether a fragment stream is worth reassembling; the probe uses
/// it to report tile counts without owning a framebuffer.
#[derive(Debug)]
pub struct ParsedFrame {
    pub kind: FrameKind,
    pub tile: u16,
    pub frame_id: u32,
    pub base_id: u32,
    pub capture_us: u64,
    pub width: u16,
    pub height: u16,
    pub records: Vec<Record>,
}

#[derive(Debug)]
pub struct Record {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub mode: TileMode,
    pub payload: Vec<u8>,
}

pub fn parse(bytes: &[u8]) -> CastResult<ParsedFrame> {
    if bytes.len() < HEADER_BYTES {
        return Err(CastError::Malformed("short frame header"));
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(CastError::Malformed("bad magic"));
    }
    if bytes[4] != VERSION {
        return Err(CastError::Malformed("unsupported wire version"));
    }
    let kind = match bytes[5] {
        0 => FrameKind::Delta,
        1 => FrameKind::Key,
        _ => return Err(CastError::Malformed("frame kind")),
    };
    let tile = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    let frame_id = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let base_id = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let capture_us = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let width = u16::from_le_bytes(bytes[24..26].try_into().unwrap());
    let height = u16::from_le_bytes(bytes[26..28].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
    if count > 1_000_000 {
        return Err(CastError::Malformed("implausible tile count"));
    }
    let mut p = HEADER_BYTES;
    let mut records = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        if bytes.len() < p + RECORD_BYTES {
            return Err(CastError::Malformed("truncated tile record"));
        }
        let x = u16::from_le_bytes(bytes[p..p + 2].try_into().unwrap());
        let y = u16::from_le_bytes(bytes[p + 2..p + 4].try_into().unwrap());
        let w = u16::from_le_bytes(bytes[p + 4..p + 6].try_into().unwrap());
        let h = u16::from_le_bytes(bytes[p + 6..p + 8].try_into().unwrap());
        let mode = TileMode::from_u8(bytes[p + 8])?;
        let len = u32::from_le_bytes(bytes[p + 10..p + 14].try_into().unwrap()) as usize;
        if (x as u32 + w as u32) > width as u32 || (y as u32 + h as u32) > height as u32 {
            return Err(CastError::Malformed("tile runs off the frame"));
        }
        if w == 0 || h == 0 || len > 1 << 22 {
            return Err(CastError::Malformed("tile record is not renderable"));
        }
        p += RECORD_BYTES;
        if bytes.len() < p + len {
            return Err(CastError::Malformed("truncated tile payload"));
        }
        records.push(Record {
            x,
            y,
            w,
            h,
            mode,
            payload: bytes[p..p + len].to_vec(),
        });
        p += len;
    }
    if p != bytes.len() {
        return Err(CastError::Malformed("trailing bytes after the last tile"));
    }
    Ok(ParsedFrame {
        kind,
        tile,
        frame_id,
        base_id,
        capture_us,
        width,
        height,
        records,
    })
}

fn write_header(
    out: &mut Vec<u8>,
    kind: FrameKind,
    tile: u16,
    frame_id: u32,
    base_id: u32,
    capture_us: u64,
    width: u16,
    height: u16,
    count: usize,
) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.push(VERSION);
    out.push(kind as u8);
    out.extend_from_slice(&tile.to_le_bytes());
    out.extend_from_slice(&frame_id.to_le_bytes());
    out.extend_from_slice(&base_id.to_le_bytes());
    out.extend_from_slice(&capture_us.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&(count as u32).to_le_bytes());
}

/// Copy a tile out of a tight-stride picture into another tight-stride picture.
///
/// A free function rather than a method because `self.base` and `self.staged`
/// cannot be borrowed at the same time from inside `&mut self`, and the
/// alternative — casting around it — is not worth the unsafe.
fn copy_tile_into(from: &[u8], to: &mut [u8], width: usize, r: Rect) {
    let row_bytes = r.w as usize * 4;
    for row in 0..r.h as usize {
        let y = r.y as usize + row;
        let x = r.x as usize * 4;
        let s = y * width * 4 + x;
        to[s..s + row_bytes].copy_from_slice(&from[s..s + row_bytes]);
    }
}

/// Copy a tightly packed tile into a tight-stride framebuffer at its position.
fn copy_tile_in(tile: &[u8], to: &mut [u8], width: usize, r: Rect) {
    let row_bytes = r.w as usize * 4;
    for row in 0..r.h as usize {
        let s = row * row_bytes;
        let d = (r.y as usize + row) * width * 4 + r.x as usize * 4;
        to[d..d + row_bytes].copy_from_slice(&tile[s..s + row_bytes]);
    }
}

/// Test-only deflate, for building payloads by hand. The production path is
/// [`Lane::deflate`], which reuses its stream.
#[cfg(test)]
fn deflate(src: &[u8], level: Compression) -> CastResult<Vec<u8>> {
    let mut enc = Compress::new(level, false);
    let mut out = Vec::with_capacity(src.len() + 512);
    match enc.compress_vec(src, &mut out, FlushCompress::Finish) {
        Ok(Status::StreamEnd) => Ok(out),
        _ => Err(CastError::Malformed("deflate did not finish")),
    }
}

/// Test-only inflate with the destination size known up front. `max` is the tile's
/// own byte count, which is what keeps a peer from turning a 200-byte payload into
/// a 4 GB allocation.
#[cfg(test)]
fn inflate(src: &[u8], max: usize) -> CastResult<Vec<u8>> {
    let mut dec = Decompress::new(false);
    let mut out = Vec::with_capacity(max);
    match dec.decompress_vec(src, &mut out, FlushDecompress::Finish) {
        Ok(Status::StreamEnd) => Ok(out),
        _ => Err(CastError::Malformed("inflated tile is larger than any tile")),
    }
}

fn jpeg_decode(src: &[u8], r: Rect, px: &mut Vec<u8>, rgb: &mut Vec<u8>) -> CastResult<()> {
    let dec = JpegDecoder::new(Cursor::new(src)).map_err(|_| CastError::Malformed("jpeg tile"))?;
    let (w, h) = dec.dimensions();
    if w != r.w as u32 || h != r.h as u32 {
        return Err(CastError::Malformed("jpeg tile size"));
    }
    let bpp = dec.color_type().bytes_per_pixel() as usize;
    if bpp != 1 && bpp != 3 {
        return Err(CastError::Malformed("jpeg tile colour type"));
    }
    // Both buffers are the lane's, for the reason [`DecLane`] gives: 510 tiles of
    // a key frame is 510 allocations if they are not. A three-channel tile decodes
    // straight into the caller's RGB buffer; a greyscale one needs a staging pass.
    let out = if bpp == 3 {
        &mut *rgb
    } else {
        &mut *px
    };
    out.clear();
    out.resize(w as usize * h as usize * bpp, 0);
    dec.read_image(out)
        .map_err(|_| CastError::Malformed("jpeg tile"))?;
    // A greyscale tile comes back one byte per pixel; the framebuffer wants
    // three, and expanding here is cheaper than rejecting a frame.
    if bpp == 3 {
        return Ok(());
    }
    rgb.clear();
    rgb.resize(w as usize * h as usize * 3, 0);
    for (dst, &g) in rgb.chunks_exact_mut(3).zip(&*px) {
        dst[0] = g;
        dst[1] = g;
        dst[2] = g;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic desktop: near-white background, a black bar that stands in for
    /// text on a slide.
    fn desktop(w: u16, h: u16) -> Vec<u8> {
        let stride = w as usize * 4;
        let mut b = vec![0u8; stride * h as usize];
        for px in b.chunks_exact_mut(4) {
            px[0] = 252;
            px[1] = 250;
            px[2] = 250;
            px[3] = 255;
        }
        for y in 10..30usize.min(h as usize) {
            for x in 10..120usize.min(w as usize) {
                let d = y * stride + x * 4;
                b[d] = 20;
                b[d + 1] = 20;
                b[d + 2] = 20;
            }
        }
        b
    }

    /// A patch that stands in for a photograph: three independent high-entropy
    /// channels per pixel. The independence matters — a pattern where one channel
    /// is a function of another carries 8 bits of entropy per pixel, deflate
    /// finds almost all of it, and the tile never reaches the decision this is
    /// meant to test.
    fn noise_patch(b: &mut [u8], stride: usize, x0: usize, y0: usize, size: usize) {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        };
        for y in 0..size {
            for x in 0..size {
                let d = (y0 + y) * stride + (x0 + x) * 4;
                b[d] = next();
                b[d + 1] = next();
                b[d + 2] = next();
                b[d + 3] = 255;
            }
        }
    }

    #[test]
    fn grid_crops_edge_tiles_and_clamps_rects() {
        let g = Grid::new(100, 60, 64);
        assert_eq!((g.cols, g.rows), (2, 1));
        assert_eq!(g.rect(Tile { cx: 1, cy: 0 }), Rect::new(64, 0, 36, 60));
        // A dirty rect the OS reports past the right edge still maps onto the
        // tiles that exist.
        assert_eq!(g.tile_range(Rect::new(60, 0, 200, 600)), Some((0, 0, 1, 0)));
        assert_eq!(g.tile_range(Rect::new(0, 0, 0, 10)), None);
        // A 1x1 rect at the last pixel of a 100x60 frame is tile (1, 0).
        assert_eq!(g.tile_range(Rect::new(99, 59, 1, 1)), Some((1, 0, 1, 0)));
    }

    #[test]
    fn mask_dedupes_overlapping_dirty_rects() {
        let mut m = TileMask::new(200);
        assert!(m.set(7));
        assert!(!m.set(7));
        assert_eq!(m.count(), 1);
        m.clear();
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn key_frame_then_delta_round_trips_bit_for_bit() {
        let (w, h) = (320u16, 192u16);
        let stride = w as usize * 4;
        let src = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.set_threads(1);
        let key = enc
            .encode(&src, stride, &[Rect::new(0, 0, w, h)], 1234)
            .unwrap();
        assert_eq!(key.kind, FrameKind::Key);
        assert_eq!(key.base_id, 0);
        assert_eq!(key.tiles, enc.grid().count());
        enc.commit(key.frame_id);

        let mut dec = WireDecoder::new(w, h);
        let got = dec.decode(&key.bytes).unwrap();
        assert_eq!(got.capture_us, 1234);
        assert_eq!(dec.frame(), &src[..]);
        assert_eq!(got.lossy, 0, "a flat desktop must not lose a pixel");

        // One caret moved, and the wire carries one tile.
        let mut moved = src.clone();
        for y in 100..108usize {
            let d = y * stride + 200 * 4;
            moved[d..d + 4].copy_from_slice(&[9, 9, 9, 255]);
        }
        let delta = enc
            .encode(&moved, stride, &[Rect::new(200, 100, 4, 8)], 2345)
            .unwrap();
        assert_eq!(delta.kind, FrameKind::Delta);
        assert_eq!(delta.base_id, key.frame_id);
        assert_eq!(delta.tiles, 1);
        assert!(delta.bytes.len() * 4 < key.bytes.len());
        enc.commit(delta.frame_id);
        let got = dec.decode(&delta.bytes).unwrap();
        assert_eq!(got.updates.len(), 1);
        assert_eq!(dec.frame(), &moved[..]);
    }

    #[test]
    fn reported_but_unchanged_tiles_cost_nothing_on_the_wire() {
        let (w, h) = (256u16, 128u16);
        let stride = w as usize * 4;
        let src = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        let first = enc.encode(&src, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        enc.commit(first.frame_id);
        // The OS says a region changed; the pixels say it did not. The frame must
        // come back empty rather than shipping a tile nobody asked for.
        let same = enc
            .encode(&src, stride, &[Rect::new(8, 8, 64, 64)], 2)
            .unwrap();
        assert_eq!(same.tiles, 0);
        assert_eq!(same.bytes.len(), HEADER_BYTES);
        enc.commit(same.frame_id);
        let again = enc
            .encode(&src, stride, &[Rect::new(8, 8, 64, 64)], 3)
            .unwrap();
        assert_eq!(again.base_id, 2);
        // An empty delta still has to be decodable, or the receiver's sequence
        // bookkeeping would stall on it.
        let mut dec = WireDecoder::new(w, h);
        dec.decode(&first.bytes).unwrap();
        dec.decode(&same.bytes).unwrap();
        assert_eq!(dec.last_applied(), 2);
    }

    #[test]
    fn decoding_on_lanes_paints_exactly_what_decoding_on_one_paints() {
        // 512x512 is 64 tiles — the first frame big enough for the receiver to
        // split, and it splits into eight bands of eight tiles. The patch starts at
        // y=96, which is inside no tile: it crosses the band boundary at 128, so a
        // band that borrowed one row too far would show up here and nowhere else.
        let (w, h) = (512u16, 512u16);
        let stride = w as usize * 4;
        let mut src = desktop(w, h);
        noise_patch(&mut src, stride, 128, 96, 256);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.set_threads(4);
        let key = enc
            .encode(&src, stride, &[Rect::new(0, 0, w, h)], 1)
            .unwrap();
        assert!(key.lossy > 0, "the photograph must reach the jpeg path");

        let mut one = WireDecoder::new(w, h);
        one.set_threads(1);
        let serial = one.decode(&key.bytes).unwrap();

        let mut many = WireDecoder::new(w, h);
        many.set_threads(8);
        let parallel = many.decode(&key.bytes).unwrap();

        assert_eq!(parallel.lossy, serial.lossy);
        assert_eq!(
            many.frame(),
            one.frame(),
            "a lane painted outside its band"
        );
        // The same frame, lossless: then the lanes have to reproduce the desktop
        // itself, not merely agree with a serial run of the same bytes.
        let mut enc2 = WireEncoder::new(w, h, 8_000_000, 30);
        let flat = enc2
            .encode(&desktop(w, h), stride, &[Rect::new(0, 0, w, h)], 2)
            .unwrap();
        assert_eq!(flat.lossy, 0);
        let mut dec = WireDecoder::new(w, h);
        dec.set_threads(8);
        dec.decode(&flat.bytes).unwrap();
        assert_eq!(dec.frame(), &desktop(w, h)[..]);
    }

    #[test]
    fn photographic_tiles_take_the_smaller_of_the_two_paths() {
        let (w, h) = (256u16, 256u16);
        let stride = w as usize * 4;
        let mut src = desktop(w, h);
        noise_patch(&mut src, stride, 128, 128, 128);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.set_threads(2);
        let key = enc.encode(&src, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        let parsed = parse(&key.bytes).unwrap();
        let lossy = parsed
            .records
            .iter()
            .filter(|r| r.mode == TileMode::Jpeg)
            .count();
        assert!(
            lossy > 0,
            "random noise is the one case JPEG has to win"
        );
        assert_eq!(key.lossy, lossy);
        // Everything outside the photograph is still exact.
        let mut dec = WireDecoder::new(w, h);
        dec.decode(&key.bytes).unwrap();
        for y in 0..128usize {
            let a = y * stride;
            assert_eq!(&dec.frame()[a..a + 128 * 4], &src[a..a + 128 * 4]);
        }
    }

    #[test]
    fn lossless_only_mode_never_ships_a_jpeg_tile() {
        let (w, h) = (128u16, 128u16);
        let stride = w as usize * 4;
        let mut src = desktop(w, h);
        noise_patch(&mut src, stride, 0, 0, 128);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.allow_lossy = false;
        let key = enc.encode(&src, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        assert_eq!(key.lossy, 0);
        let mut dec = WireDecoder::new(w, h);
        dec.decode(&key.bytes).unwrap();
        assert_eq!(dec.frame(), &src[..]);
    }

    #[test]
    fn rollback_restores_the_reference_the_next_delta_is_cut_against() {
        let (w, h) = (256u16, 128u16);
        let stride = w as usize * 4;
        let base = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.set_threads(1);
        let key = enc.encode(&base, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        enc.commit(key.frame_id);
        assert_eq!(enc.reference(), &base[..]);
        let mut dec = WireDecoder::new(w, h);
        dec.decode(&key.bytes).unwrap();

        // Frame 2 repaints the left half; the peer never gets it.
        let mut f2 = base.clone();
        let fill = [1u8, 2, 3, 255].repeat(128);
        for y in 0..h as usize {
            let d = y * stride;
            f2[d..d + 128 * 4].copy_from_slice(&fill);
        }
        let lost = enc
            .encode(&f2, stride, &[Rect::new(0, 0, 128, h)], 2)
            .unwrap();
        assert!(lost.tiles > 0);
        enc.rollback();
        assert_eq!(enc.reference(), &base[..]);

        // Frame 3 equals the base on the left. Without the rollback those tiles
        // would look unchanged and the peer would show the base forever.
        let next = enc
            .encode(&base, stride, &[Rect::new(0, 0, 128, h)], 3)
            .unwrap();
        assert_eq!(next.base_id, key.frame_id);
        assert_eq!(next.tiles, 0);
        // A frame that changes something the lost frame also changed is still a
        // delta against the base, so it repaints correctly.
        let mut f4 = base.clone();
        for y in 0..h as usize {
            let d = y * stride + 64 * 4;
            f4[d..d + 8].copy_from_slice(&[7, 7, 7, 255, 8, 8, 8, 255]);
        }
        let repainted = enc
            .encode(&f4, stride, &[Rect::new(0, 0, 128, h)], 4)
            .unwrap();
        assert_eq!(repainted.base_id, key.frame_id);
        assert!(repainted.tiles >= 1);
        enc.commit(repainted.frame_id);
        dec.decode(&repainted.bytes).unwrap();
        assert_eq!(dec.frame(), &f4[..]);
    }

    #[test]
    fn delta_with_the_wrong_base_is_refused_not_painted() {
        let (w, h) = (128u16, 128u16);
        let stride = w as usize * 4;
        let src = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        let key = enc.encode(&src, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        enc.commit(key.frame_id);
        let mut f2 = src.clone();
        f2[0..4].copy_from_slice(&[0, 0, 0, 255]);
        let delta = enc
            .encode(&f2, stride, &[Rect::new(0, 0, 4, 4)], 2)
            .unwrap();
        // A receiver that never saw the key frame cannot use the delta.
        let mut late = WireDecoder::new(w, h);
        assert!(matches!(
            late.decode(&delta.bytes),
            Err(CastError::NeedsKeyframe)
        ));
        assert_eq!(late.last_applied(), 0);
        // The same receiver is fine once it gets the key frame.
        late.decode(&key.bytes).unwrap();
        late.decode(&delta.bytes).unwrap();
        assert_eq!(late.frame(), &f2[..]);
    }

    #[test]
    fn a_wide_change_becomes_a_key_frame_any_listener_can_join_at() {
        let (w, h) = (640u16, 360u16);
        let stride = w as usize * 4;
        let src = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        let key = enc.encode(&src, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        enc.commit(key.frame_id);
        let mut moved = src.clone();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let d = y * stride + x * 4;
                moved[d] = (x * 7 + y * 3) as u8;
            }
        }
        let big = enc
            .encode(&moved, stride, &[Rect::new(0, 0, w, h)], 2)
            .unwrap();
        assert_eq!(big.kind, FrameKind::Key);
        assert_eq!(big.tiles, enc.grid().count());
        enc.commit(big.frame_id);
        let mut late = WireDecoder::new(w, h);
        late.decode(&big.bytes).unwrap();
        assert_eq!(late.frame(), &moved[..]);
    }

    #[test]
    fn a_padded_capture_stride_is_read_at_the_right_offset() {
        // 1366-wide panels report a stride wider than width*4; if the codec ever
        // assumed tight input, the picture would shear row by row.
        let (w, h) = (128u16, 64u16);
        let tight = desktop(w, h);
        let stride = 128 * 4 + 32;
        let mut padded = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            padded[y * stride..y * stride + w as usize * 4]
                .copy_from_slice(&tight[y * w as usize * 4..(y + 1) * w as usize * 4]);
        }
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        let key = enc.encode(&padded, stride, &[Rect::new(0, 0, w, h)], 1).unwrap();
        enc.commit(key.frame_id);
        assert_eq!(enc.reference(), &tight[..]);
        let mut dec = WireDecoder::new(w, h);
        dec.decode(&key.bytes).unwrap();
        assert_eq!(dec.frame(), &tight[..]);
    }

    #[test]
    fn malformed_containers_are_rejected() {
        let (w, h) = (128u16, 128u16);
        let src = desktop(w, h);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        let key = enc
            .encode(&src, w as usize * 4, &[Rect::new(0, 0, w, h)], 7)
            .unwrap();
        let good = key.bytes.clone();
        assert!(parse(&good[..HEADER_BYTES - 1]).is_err());
        let mut bad = good.clone();
        bad[0] ^= 0xff;
        assert!(matches!(
            parse(&bad),
            Err(CastError::Malformed("bad magic"))
        ));
        let mut bad = good.clone();
        bad.truncate(good.len() - 1);
        assert!(matches!(
            parse(&bad),
            Err(CastError::Malformed("truncated tile payload"))
        ));
        // A record whose tile runs off the frame.
        let mut bad = good.clone();
        bad[HEADER_BYTES] = 200;
        assert!(matches!(
            parse(&bad),
            Err(CastError::Malformed("tile runs off the frame"))
        ));
        // An inflated tile that promises more bytes than any tile can hold. The
        // receiver must refuse the container instead of trusting the length.
        let mut bad = good.clone();
        let len_at = HEADER_BYTES + 10;
        bad[len_at..len_at + 4].copy_from_slice(&(1u32 << 23).to_le_bytes());
        assert!(matches!(
            parse(&bad),
            Err(CastError::Malformed("tile record is not renderable"))
        ));
    }

    #[test]
    fn a_low_entropy_pattern_stays_lossless_and_a_high_entropy_one_does_not() {
        // The gate is one bit per pixel of deflate output, so what matters is which
        // mode each pattern *ends up in* — asserted through the production path,
        // not as a ratio band someone has to re-tune.
        //
        // `tied` is one noise channel folded into three: 8 bits of real entropy,
        // which JPEG cannot beat without softening a pattern that deflate already
        // carries. `free` is three independent channels: 24 bits, which deflate
        // cannot approach and JPEG can.
        let mut tied = vec![0u8; 64 * 64 * 4];
        let mut free = vec![0u8; 64 * 64 * 4];
        noise_patch(&mut free, 64 * 4, 0, 0, 64);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for p in tied.chunks_exact_mut(4) {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = (state >> 33) as u8;
            p[0] = v;
            p[1] = v.rotate_left(3);
            p[2] = !v;
            p[3] = 255;
        }
        let enc = WireEncoder::new(64, 64, 8_000_000, 30);
        let mut lane = Lane::new();
        let mode_of = |enc: &WireEncoder, lane: &mut Lane, src: &[u8]| {
            enc.encode_one(lane, src, 64 * 4, Tile { cx: 0, cy: 0 }).mode
        };
        let tied_out = mode_of(&enc, &mut lane, &tied);
        let free_out = mode_of(&enc, &mut lane, &free);
        let tied_deflate = deflate(&tied, Compression::fast()).unwrap().len();
        let free_deflate = deflate(&free, Compression::fast()).unwrap().len();
        assert_eq!(
            tied_out,
            TileMode::Deflate,
            "a one-channel pattern must stay lossless: deflate={tied_deflate} ratio={:.2}",
            tied_deflate as f64 / tied.len() as f64
        );
        assert_eq!(
            free_out,
            TileMode::Jpeg,
            "a three-channel pattern must reach the lossy branch and win there: \
             deflate={free_deflate} ratio={:.2}",
            free_deflate as f64 / free.len() as f64
        );
    }

    #[test]
    fn a_congested_channel_tries_lossy_even_on_a_tile_deflate_handles() {
        // The rate governor's only lever is the JPEG branch, so a frame that is
        // over target must open the gate even for a tile deflate compresses. This
        // is the difference between a video-heavy desktop that adapts and one that
        // simply exceeds the link.
        let mut mid = vec![0u8; 64 * 64 * 4];
        noise_patch(&mut mid, 64 * 4, 0, 0, 64);
        let mut enc = WireEncoder::new(64, 64, 64_000, 30);
        let full = [Rect::new(0, 0, 64, 64)];
        // Under target: quality is free, and the tile's own ratio decides.
        let calm = enc.encode(&mid, 64 * 4, &full, 1).unwrap();
        // Now push the estimate far over the 266-byte target and encode again.
        for _ in 0..12 {
            enc.rate.observe(200_000);
        }
        assert!(enc.rate.over_target());
        let starved = enc.encode(&mid, 64 * 4, &full, 2).unwrap();
        assert!(
            starved.bytes.len() < calm.bytes.len(),
            "over target the frame must get smaller: {} vs {}",
            starved.bytes.len(),
            calm.bytes.len()
        );
        assert_eq!(starved.lossy, 1, "and by taking the lossy branch");
    }

    #[test]
    fn rate_control_moves_toward_the_target_within_its_limits() {
        let mut rc = RateControl::new(2_400_000, 30);
        assert_eq!(rc.target_bytes_per_frame, 10_000);
        let start = rc.quality;
        for _ in 0..12 {
            rc.observe(40_000);
        }
        assert!(rc.quality < start, "overshoot must lower quality");
        assert!(rc.quality >= rc.min_quality);
        for _ in 0..60 {
            rc.observe(1_000);
        }
        assert!(rc.quality <= rc.max_quality);
        assert!(rc.ema_bytes() < 40_000.0);
    }

    #[test]
    fn parallel_and_serial_encoders_agree_on_the_bytes() {
        let (w, h) = (320u16, 256u16);
        let stride = w as usize * 4;
        let mut src = desktop(w, h);
        noise_patch(&mut src, stride, 0, 128, 64);
        noise_patch(&mut src, stride, 192, 64, 64);
        let full = [Rect::new(0, 0, w, h)];
        // A key frame spreads across every lane, so it is the frame that proves
        // the work split cannot change the stream.
        let mut one = WireEncoder::new(w, h, 8_000_000, 30);
        one.set_threads(1);
        let a = one.encode(&src, stride, &full, 1).unwrap();
        let mut many = WireEncoder::new(w, h, 8_000_000, 30);
        many.set_threads(8);
        let b = many.encode(&src, stride, &full, 1).unwrap();
        assert_eq!(a.bytes, b.bytes, "lane count must not change the stream");
        assert_eq!(a.tiles, 20);
        assert_eq!(a.lossy, 2, "both noise tiles must be JPEG");
        one.commit(a.frame_id);
        many.commit(a.frame_id);

        // And so does the delta that follows it.
        let mut moved = src.clone();
        for y in 0..h as usize {
            let d = y * stride + 4 * 4;
            moved[d..d + 8].copy_from_slice(&[1, 1, 1, 255, 2, 2, 2, 255]);
        }
        let dirty = [Rect::new(4, 0, 8, h)];
        let c = one.encode(&moved, stride, &dirty, 2).unwrap();
        let d = many.encode(&moved, stride, &dirty, 2).unwrap();
        assert_eq!(c.kind, FrameKind::Delta);
        assert_eq!(c.bytes, d.bytes);
    }

    #[test]
    fn an_inflated_tile_cannot_exceed_its_own_size() {
        let big = vec![0xa5u8; 64 * 64 * 4];
        let small = deflate(&big, Compression::fast()).unwrap();
        assert!(small.len() * 20 < big.len(), "the fixture must compress");
        assert_eq!(inflate(&small, big.len()).unwrap(), big);
        assert!(matches!(
            inflate(&small, 1024),
            Err(CastError::Malformed(_))
        ));
    }

    #[test]
    fn every_byte_of_a_key_frame_lands_in_the_framebuffer() {
        // A frame whose size is not a multiple of the tile exercises every cropped
        // edge tile at once.
        let (w, h) = (150u16, 110u16);
        let stride = w as usize * 4;
        let mut src = desktop(w, h);
        noise_patch(&mut src, stride, 64, 32, 64);
        let mut enc = WireEncoder::new(w, h, 8_000_000, 30);
        enc.set_threads(3);
        let key = enc.encode_with(&src, stride, &[], 1, true).unwrap();
        assert_eq!(key.kind, FrameKind::Key);
        enc.commit(key.frame_id);
        let mut dec = WireDecoder::new(w, h);
        let got = dec.decode(&key.bytes).unwrap();
        assert_eq!(got.updates.len(), enc.grid().count());
        for y in 0..h as usize {
            let a = y * stride;
            let exact = &src[a..a + 64 * 4];
            assert_eq!(&dec.frame()[a..a + 64 * 4], exact, "row {y} left column");
        }
    }
}
