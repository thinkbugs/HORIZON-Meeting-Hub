//! End-to-end measurement for the self-built channel: capture → tile encode →
//! UDP on the loopback interface → reassembly → decode.
//!
//! ## What this number is, and what it is not
//!
//! `present_us - capture_us` is real: both stamps come from `QueryPerformanceCounter`
//! and both ends run in this process, so nothing is estimated. It is **end to end
//! to the framebuffer ready**, not glass to glass: there is no presenter yet, so
//! the upload and the display's scanout are not in it. It is a lower bound on what
//! a person would see.
//!
//! Loopback is also not a LAN: it removes switch queues, Wi-Fi and the driver's
//! interrupt coalescing. What it does measure is the part this engine owns — the
//! encode, the fragmentation and pacing, the reassembly, the acknowledgement, the
//! decode, and the round trip that one-frame-in-flight forces on the next frame.
//! `codec-bench` measures the tile codec alone; this bench is the first place the
//! transport and the session are on the path.
//!
//! ## Sources
//!
//! Live capture is used when this desktop can provide it. When it cannot — a
//! service session, a display driver DXGI will not bind, a locked screen — the
//! bench says so and falls back to a synthetic 1080p desktop with a moving block,
//! so a number is always produced and the label always says which desktop produced
//! it. `--synthetic` forces the fallback.
//!
//! Usage: `cast-bench [--frames=300] [--seconds=10] [--synthetic]`

use std::time::{Duration, Instant};

use horizon_cast::capture::{Backend, Capture};
use horizon_cast::metrics::{now_us, Budget, Samples};
use horizon_cast::session::{ReceiverConfig, ReceiverLink, SenderConfig, SenderLink};
use horizon_cast::wire::{FrameKind, Rect};

/// One session id for both ends: this bench is the only peer on the loopback.
const SESSION: u32 = 0xC0DE;

/// The channel is tuned for 30 fps, so the synthetic source is paced to it. A
/// free-running source would report a frame rate no desktop produces and would
/// make bytes/frame a measure of the loop, not of the channel.
const FPS: u32 = 30;

fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("cast-bench: {e}");
        std::process::exit(1);
    }
}

struct Args {
    frames: usize,
    seconds: f64,
    synthetic: bool,
}

impl Args {
    fn parse() -> Self {
        let mut a = Self {
            frames: 400,
            seconds: 12.0,
            synthetic: false,
        };
        for arg in std::env::args().skip(1) {
            if let Some(v) = arg.strip_prefix("--frames=") {
                a.frames = v.parse().unwrap_or(a.frames);
            } else if let Some(v) = arg.strip_prefix("--seconds=") {
                a.seconds = v.parse().unwrap_or(a.seconds);
            } else if arg == "--synthetic" {
                a.synthetic = true;
            } else if arg == "--help" || arg == "-h" {
                println!("usage: cast-bench [--frames=N] [--seconds=S] [--synthetic]");
                std::process::exit(0);
            }
        }
        a
    }
}

/// One frame ready to be encoded, borrowed from whichever source produced it.
struct Shot<'a> {
    bgra: &'a [u8],
    stride: usize,
    dirty: Vec<Rect>,
    captured_us: u64,
    copy_us: u64,
}

enum Source {
    Live(Capture),
    Synthetic(Synthetic),
}

impl Source {
    fn name(&self) -> &'static str {
        match self {
            Source::Live(_) => "live",
            Source::Synthetic(_) => "synthetic",
        }
    }

    fn size(&self) -> (u16, u16) {
        match self {
            Source::Live(c) => (c.width(), c.height()),
            Source::Synthetic(s) => (s.w, s.h),
        }
    }

    /// How long to block waiting for the desktop to present something. Duplication
    /// blocks until a present, so a long timeout costs nothing; BitBlt has no event
    /// and is spaced by the timeout itself.
    fn tick(&self) -> Duration {
        match self {
            Source::Live(c) if c.backend() == Backend::Dxgi => Duration::from_millis(400),
            Source::Live(_) => Duration::from_millis(33),
            Source::Synthetic(_) => Duration::ZERO,
        }
    }

    fn next(&mut self) -> Option<Shot<'_>> {
        let tick = self.tick();
        match self {
            Source::Live(cap) => {
                let f = cap.capture(tick).ok().flatten()?;
                Some(Shot {
                    bgra: f.bgra,
                    stride: f.stride,
                    dirty: f.dirty,
                    captured_us: f.captured_us,
                    copy_us: f.copy_us,
                })
            }
            Source::Synthetic(s) => Some(s.step()),
        }
    }
}

/// A 1080p desktop that moves on its own: a solid block on a tile checkerboard,
/// with only the old and new block regions reported as dirty.
///
/// The content is deliberately compressible and lossless in the tile codec, for the
/// same reason `codec-bench`'s "ui" frame is: a photographic block would measure
/// JPEG, which the codec benchmark already covers, and would make every frame a
/// candidate for key promotion.
struct Synthetic {
    w: u16,
    h: u16,
    px: Vec<u8>,
    frame: u32,
    prev: Rect,
}

const BLOCK_W: u16 = 320;
const BLOCK_H: u16 = 200;

impl Synthetic {
    fn new(w: u16, h: u16) -> Self {
        let mut this = Self {
            w,
            h,
            px: vec![0u8; w as usize * h as usize * 4],
            frame: 0,
            prev: Rect::new(0, 0, 0, 0),
        };
        this.paint_background(Rect::new(0, 0, w, h));
        this
    }

    fn background(x: u16, y: u16) -> [u8; 4] {
        let parity = ((x / 64) + (y / 64)) & 1;
        if parity == 0 {
            [32 + (x % 64) as u8 / 4, 34, 44, 255]
        } else {
            [48, 51, 64 + (y % 64) as u8 / 4, 255]
        }
    }

    fn paint_background(&mut self, r: Rect) {
        let stride = self.w as usize * 4;
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let d = y as usize * stride + x as usize * 4;
                self.px[d..d + 4].copy_from_slice(&Self::background(x, y));
            }
        }
    }

    fn paint_block(&mut self, r: Rect) {
        let stride = self.w as usize * 4;
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let d = y as usize * stride + x as usize * 4;
                self.px[d..d + 4].copy_from_slice(&[210, 128, 48, 255]);
            }
        }
    }

    fn step(&mut self) -> Shot<'_> {
        let t0 = now_us();
        let cx = self.w as f64 / 2.0 + (self.w as f64 / 3.0) * (self.frame as f64 * 0.05).cos();
        let cy = self.h as f64 / 2.0 + (self.h as f64 / 3.0) * (self.frame as f64 * 0.07).sin();
        let x = (cx as u16).saturating_sub(BLOCK_W / 2).min(self.w - BLOCK_W);
        let y = (cy as u16).saturating_sub(BLOCK_H / 2).min(self.h - BLOCK_H);
        let next = Rect::new(x, y, BLOCK_W, BLOCK_H);
        if self.prev.w != 0 {
            self.paint_background(self.prev);
        }
        self.paint_block(next);
        self.frame += 1;
        let mut dirty = Vec::with_capacity(2);
        if self.prev.w != 0 {
            dirty.push(self.prev);
        }
        dirty.push(next);
        self.prev = next;
        Shot {
            bgra: &self.px,
            stride: self.w as usize * 4,
            dirty,
            captured_us: now_us(),
            copy_us: now_us() - t0,
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let mut source = open_source(args.synthetic)?;
    let (w, h) = source.size();
    println!(
        "HORIZON channel, source={} {w}x{h}, {}",
        source.name(),
        if args.synthetic {
            "synthetic desktop".to_string()
        } else {
            "loopback UDP".to_string()
        }
    );

    let mut rx = ReceiverLink::bind(
        "127.0.0.1:0",
        ReceiverConfig {
            session: SESSION,
            width: w,
            height: h,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let peer = rx.local_addr().map_err(|e| e.to_string())?;
    let mut tx = SenderLink::connect(
        "127.0.0.1:0",
        peer,
        SenderConfig {
            session: SESSION,
            width: w,
            height: h,
            fps: FPS,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;

    let mut copy = Samples::new(5000);
    let mut encode = Samples::new(5000);
    let mut decode = Samples::new(5000);
    let mut e2e = Samples::new(5000);
    let mut bytes = 0u64;
    let mut tiles = 0u64;
    let mut keys = 0u64;
    let mut first_e2e = None;
    let mut first_detail = (0usize, 0u16, 0u64, 0u64);
    let mut frames = 0usize;
    let start = Instant::now();
    let wall = Duration::from_secs_f64(args.seconds.max(1.0));
    let cadence = Duration::from_secs_f64(1.0 / FPS as f64);
    let mut due = start;

    while frames < args.frames && start.elapsed() < wall {
        // Live capture paces itself on the desktop's present; the synthetic one
        // has to be told, or it would report hundreds of frames per second.
        if matches!(source, Source::Synthetic(_)) {
            let now = Instant::now();
            if due < now {
                // The previous frame waited on an ack longer than one cadence.
                // Do not burst to catch up: a desktop that fell behind presents
                // the next frame at the next tick, it does not present two.
                due = now;
            }
            due += cadence;
            if due > now {
                std::thread::sleep(due - now);
            }
        }
        let Some(shot) = source.next() else {
            continue;
        };
        let Some(sent) = tx
            .cast(shot.bgra, shot.stride, &shot.dirty, shot.captured_us)
            .map_err(|e| e.to_string())?
        else {
            // The previous frame is still unacknowledged. Nothing to poll for this
            // capture; the next iteration drains the ack and catches up.
            continue;
        };
        copy.push(shot.copy_us);
        encode.push(sent.encode_us);
        bytes += sent.wire_bytes as u64;
        tiles += sent.tiles as u64;
        if sent.kind == FrameKind::Key {
            keys += 1;
        }
        let Some(got) = rx.poll(Duration::from_millis(200)).map_err(|e| e.to_string())? else {
            continue;
        };
        decode.push(got.frame.decode_us);
        let e2e_us = got.present_us.saturating_sub(got.frame.capture_us);
        e2e.push(e2e_us);
        if first_e2e.is_none() {
            first_detail = (sent.wire_bytes, sent.datagrams, sent.encode_us, sent.pause_us);
        }
        first_e2e.get_or_insert(e2e_us);
        frames += 1;
        if frames % 100 == 0 {
            println!("  {frames} frames, e2e p50 {}us", e2e.percentile_us(50.0));
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    if frames == 0 {
        return Err("no frame completed; nothing to report".to_string());
    }
    let budget = Budget::LOW_LATENCY_1080P30;
    let sender = tx.session().stats.clone();
    let receiver_t = rx.session().transport_stats().clone();

    println!(
        "\n{frames} frames in {elapsed:.1}s ({:.1} fps), key={keys} delta={}",
        frames as f64 / elapsed,
        frames as u64 - keys
    );
    println!(
        "  {:<12}  {}",
        if matches!(source, Source::Synthetic(_)) {
            "synth paint"
        } else {
            "capture copy"
        },
        copy.summary()
    );
    println!("  encode        {}", encode.summary());
    println!("  decode        {}", decode.summary());
    println!("  e2e to fb     {}", e2e.summary());
    println!(
        "  first frame   {:.2}ms (key: {} bytes in {} datagrams, encode {}us, paced {}us)",
        first_e2e.unwrap_or(0) as f64 / 1000.0,
        first_detail.0,
        first_detail.1,
        first_detail.2,
        first_detail.3
    );
    println!(
        "  wire          {:.0} bytes/frame, {:.1} tiles/frame",
        bytes as f64 / frames as f64,
        tiles as f64 / frames as f64
    );
    println!("  sender        {}", sender.report());
    println!("  transport rx  {}", receiver_t.report());
    println!("  receiver      {}", rx.session().stats.report());
    println!(
        "  rtt           {}us, bitrate {} Mbit/s, quality {}",
        tx.session().rtt_us(),
        tx.session().bitrate_bps() / 1_000_000,
        tx.session().quality()
    );

    let p50 = e2e.percentile_us(50.0) as u64;
    let p95 = e2e.percentile_us(95.0) as u64;
    let stage = encode.percentile_us(50.0) as u64 + decode.percentile_us(50.0) as u64;
    println!(
        "\n  e2e p50 {:.2}ms = encode {:.2}ms + decode {:.2}ms + {:.2}ms socket/framing",
        p50 as f64 / 1000.0,
        encode.percentile_us(50.0) as f64 / 1000.0,
        decode.percentile_us(50.0) as f64 / 1000.0,
        (p50.saturating_sub(stage)) as f64 / 1000.0
    );
    println!(
        "  e2e p95 {:.2}ms vs {}ms budget (framebuffer ready, not glass to glass): {}",
        p95 as f64 / 1000.0,
        budget.total_us() / 1000,
        if p95 <= budget.total_us() {
            "INSIDE"
        } else {
            "OVER"
        }
    );
    Ok(())
}

fn open_source(force_synthetic: bool) -> Result<Source, String> {
    if !force_synthetic {
        match Capture::open(0) {
            Ok(cap) => return Ok(Source::Live(cap)),
            Err(e) => println!("  live capture unavailable ({e}); using the synthetic desktop"),
        }
    }
    Ok(Source::Synthetic(Synthetic::new(1920, 1080)))
}
