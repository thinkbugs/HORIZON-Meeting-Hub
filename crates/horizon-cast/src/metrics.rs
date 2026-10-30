//! The measurement system. Nothing in this engine is "optimised" without a
//! number coming out of here.
//!
//! Two kinds of number matter and they are deliberately kept apart:
//! - **stage cost** — CPU time spent inside one stage of one pipeline; always
//!   non-negative, measured with a monotonic clock inside one process.
//! - **glass-to-glass latency** — capture mark to present mark, which can only
//!   be measured if the clock is comparable across machines. QPC is comparable
//!   across processes on one host, so the loopback rig used in P1-C gives a
//!   true pipeline number minus only the wire time.

use std::time::{Duration, Instant};

/// Every stage a frame passes through, in order. The ordering is the protocol
/// ordering too: a stage mark must never precede the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(usize)]
pub enum Stage {
    Capture = 0,
    Convert = 1,
    Encode = 2,
    Send = 3,
    Network = 4,
    Dejitter = 5,
    Decode = 6,
    Present = 7,
}

pub const STAGES: [Stage; 8] = [
    Stage::Capture,
    Stage::Convert,
    Stage::Encode,
    Stage::Send,
    Stage::Network,
    Stage::Dejitter,
    Stage::Decode,
    Stage::Present,
];

impl Stage {
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Stage::Capture => "capture",
            Stage::Convert => "convert",
            Stage::Encode => "encode",
            Stage::Send => "send",
            Stage::Network => "network",
            Stage::Dejitter => "dejitter",
            Stage::Decode => "decode",
            Stage::Present => "present",
        }
    }
}

/// Microseconds on a clock that is comparable across processes on this host.
///
/// On Windows this is `QueryPerformanceCounter`, which is the same timer every
/// process sees; on other platforms it degrades to a per-process clock, which is
/// still enough for stage costs.
pub fn now_us() -> u64 {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static FREQ: OnceLock<i64> = OnceLock::new();
        let freq = *FREQ.get_or_init(|| {
            let mut f = 0i64;
            unsafe { QueryPerformanceFrequency(&mut f) };
            if f <= 0 { 1_000_000 } else { f }
        });
        let mut ticks = 0i64;
        unsafe { QueryPerformanceCounter(&mut ticks) };
        let ticks = ticks.max(0) as u128;
        ((ticks * 1_000_000) / freq as u128) as u64
    }
    #[cfg(not(windows))]
    {
        std::time::UNIX_EPOCH
            .elapsed()
            .map(|e| e.as_micros() as u64)
            .unwrap_or(0)
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(lpperformancecountervalue: *mut i64) -> i32;
    fn QueryPerformanceFrequency(lpfrequency: *mut i64) -> i32;
}

/// Marks for one frame's journey. Zero means "this stage has not been reached".
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameClock {
    pub frame_id: u64,
    pub marks: [u64; 8],
}

impl FrameClock {
    pub fn new(frame_id: u64) -> Self {
        Self { frame_id, marks: [0; 8] }
    }

    pub fn mark(&mut self, stage: Stage) {
        self.marks[stage.index()] = now_us();
    }

    pub fn at(&self, stage: Stage) -> u64 {
        self.marks[stage.index()]
    }

    /// Inclusive span between two marks, or `None` if either is missing.
    pub fn span(&self, from: Stage, to: Stage) -> Option<u64> {
        let (a, b) = (self.at(from), self.at(to));
        (a != 0 && b >= a).then(|| b - a)
    }

    /// Capture to present — the only number the person in the room feels.
    pub fn glass_to_glass(&self) -> Option<u64> {
        self.span(Stage::Capture, Stage::Present)
    }

    /// How much of the journey the clock actually covers, used to reject
    /// half-measured frames instead of averaging them into the result.
    pub fn completeness(&self) -> usize {
        self.marks.iter().filter(|m| **m != 0).count()
    }
}

/// Exact percentile over a bounded window of microsecond samples.
///
/// A window (not a reservoir, not an estimate) because every real decision here
/// is about steady-state behaviour, and because p95 over 20 000 samples sorted
/// lazily is cheap enough to be honest.
#[derive(Debug, Clone)]
pub struct Samples {
    window: usize,
    ring: Vec<u32>,
    next: usize,
    filled: bool,
    total: u64,
    sum: u64,
    max: u32,
}

impl Samples {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.clamp(16, 1 << 20),
            ring: Vec::new(),
            next: 0,
            filled: false,
            total: 0,
            sum: 0,
            max: 0,
        }
    }

    pub fn push(&mut self, us: u64) {
        let v = us.min(u32::MAX as u64) as u32;
        if self.ring.len() < self.window {
            self.ring.push(v);
        } else {
            self.ring[self.next] = v;
            self.filled = true;
        }
        self.next = (self.next + 1) % self.window;
        self.total += 1;
        self.sum += v as u64;
        self.max = self.max.max(v);
    }

    pub fn count(&self) -> u64 {
        self.total
    }

    pub fn window_len(&self) -> usize {
        self.ring.len()
    }

    pub fn mean_us(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.sum as f64 / self.total as f64
        }
    }

    pub fn max_us(&self) -> u32 {
        self.max
    }

    /// `p` in `[0, 100]` by nearest rank, over the current window only.
    pub fn percentile_us(&self, p: f64) -> u32 {
        if self.ring.is_empty() {
            return 0;
        }
        let mut sorted = self.ring.clone();
        sorted.sort_unstable();
        let n = sorted.len() as f64;
        let rank = (p / 100.0 * n).ceil() as usize;
        sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
    }

    pub fn summary(&self) -> String {
        format!(
            "n={} mean={:.2}ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            self.total,
            self.mean_us() / 1000.0,
            self.percentile_us(50.0) as f64 / 1000.0,
            self.percentile_us(95.0) as f64 / 1000.0,
            self.percentile_us(99.0) as f64 / 1000.0,
            self.max_us() as f64 / 1000.0
        )
    }

    /// Fraction of samples above a threshold — the "did we blow the frame budget"
    /// question, which is what a stutter actually is.
    pub fn above(&self, us: u32) -> f64 {
        if self.ring.is_empty() {
            return 0.0;
        }
        self.ring.iter().filter(|v| **v > us).count() as f64 * 100.0 / self.ring.len() as f64
    }
}

/// Per-stage cost for every frame the pipeline has completed.
#[derive(Debug)]
pub struct PipelineStats {
    pub per_stage: [Samples; 8],
    pub glass_to_glass: Samples,
    pub frames: u64,
    pub dropped: u64,
    started: Option<Instant>,
}

impl PipelineStats {
    pub fn new() -> Self {
        Self {
            per_stage: std::array::from_fn(|_| Samples::new(20_000)),
            glass_to_glass: Samples::new(20_000),
            frames: 0,
            dropped: 0,
            started: None,
        }
    }

    pub fn reset_clock(&mut self) {
        self.started = Some(Instant::now());
    }

    /// Fold one completed frame in. Only stages whose predecessor is also marked
    /// contribute, so a frame that died mid-pipeline cannot fake a cost.
    pub fn record(&mut self, clock: &FrameClock) {
        self.frames += 1;
        for pair in STAGES.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            if let Some(us) = clock.span(from, to) {
                self.per_stage[to.index()].push(us);
            }
        }
        if let Some(us) = clock.glass_to_glass() {
            self.glass_to_glass.push(us);
        }
    }

    pub fn note_dropped(&mut self) {
        self.dropped += 1;
    }

    pub fn elapsed(&self) -> Duration {
        self.started.map(|s| s.elapsed()).unwrap_or_default()
    }

    /// Sustained frames per second over the recorded window.
    pub fn fps(&self) -> f64 {
        let e = self.elapsed().as_secs_f64();
        if e > 0.0 {
            self.frames as f64 / e
        } else {
            0.0
        }
    }

    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "frames={} dropped={} fps={:.1}\n",
            self.frames,
            self.dropped,
            self.fps()
        ));
        for stage in STAGES {
            let s = &self.per_stage[stage.index()];
            if s.count() > 0 {
                out.push_str(&format!("{:>9}  {}\n", stage.name(), s.summary()));
            }
        }
        if self.glass_to_glass.count() > 0 {
            out.push_str(&format!("    e2e  {}\n", self.glass_to_glass.summary()));
        }
        out
    }
}

/// Wall-clock guard for a single stage: `let _t = Timing::start(); … t.ms()`.
#[derive(Debug, Clone, Copy)]
pub struct Timing(Instant);

impl Timing {
    pub fn start() -> Self {
        Timing(Instant::now())
    }

    pub fn us(&self) -> u64 {
        self.0.elapsed().as_micros() as u64
    }

    pub fn ms(&self) -> f64 {
        self.0.elapsed().as_secs_f64() * 1000.0
    }
}

/// Latency budget used to judge every design decision. Kept next to the clock
/// so a stage and its budget are read together.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub capture_us: u64,
    pub convert_us: u64,
    pub encode_us: u64,
    pub network_us: u64,
    pub dejitter_us: u64,
    pub decode_us: u64,
    pub present_us: u64,
}

impl Budget {
    /// 1080p30 on a gigabit LAN: the target is a sub-frame budget on every
    /// stage and ≤ 60 ms end to end.
    ///
    /// Every number below is now against a measurement on this machine, taken by
    /// `mf-probe` (see the module doc of [`crate::codec`] for the full table):
    /// - convert: BGRA→NV12 SSE2, p50 2.9 ms / p95 3.6 ms at 720p, p95 4.0 ms at
    ///   1080p — inside 4 ms, but with no room, which is why the tile codec keeps
    ///   BGRA and does not convert at all.
    /// - encode: MF H.264 with `MF_LOW_LATENCY`, p95 5.7 ms at 720p and 11.3 ms
    ///   at 1080p — 12 ms is the measured ceiling, not a guess.
    /// - decode: the in-box H.264 decoder holds 27 frames no matter which
    ///   codec-API property is pushed, so 6 ms is the budget for a *one-frame*
    ///   decoder. A stage that cannot meet it is not slow, it is architecturally
    ///   wrong, and it belongs in the interop path instead.
    pub const LOW_LATENCY_1080P30: Budget = Budget {
        capture_us: 3_000,
        convert_us: 4_000,
        encode_us: 12_000,
        network_us: 4_000,
        dejitter_us: 8_000,
        decode_us: 6_000,
        present_us: 3_000,
    };

    pub fn total_us(&self) -> u64 {
        self.capture_us
            + self.convert_us
            + self.encode_us
            + self.network_us
            + self.dejitter_us
            + self.decode_us
            + self.present_us
    }

    pub fn stage_us(&self, stage: Stage) -> u64 {
        match stage {
            Stage::Capture => self.capture_us,
            Stage::Convert => self.convert_us,
            Stage::Encode => self.encode_us,
            Stage::Send => self.network_us / 8,
            Stage::Network => self.network_us,
            Stage::Dejitter => self.dejitter_us,
            Stage::Decode => self.decode_us,
            Stage::Present => self.present_us,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic_and_comparable() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
        // Two reads inside one QPC tick come back equal, so monotonicity is `>=`.
        // Proving the clock *advances* needs wall time passing, not a counter loop:
        // at opt-level 3 the loop folds to a closed form and measures nothing, and at
        // opt-level 0 it is slow but its length is not something the test can pin.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(now_us() > a, "clock did not advance");
    }

    #[test]
    fn spans_reject_partial_frames() {
        let mut c = FrameClock::new(7);
        c.mark(Stage::Capture);
        assert_eq!(c.completeness(), 1);
        assert!(c.glass_to_glass().is_none());
        c.marks[Stage::Present.index()] = c.at(Stage::Capture).saturating_sub(1);
        assert!(c.span(Stage::Capture, Stage::Present).is_none());
        c.mark(Stage::Present);
        assert!(c.glass_to_glass().is_some());
    }

    #[test]
    fn percentiles_are_exact_over_the_window() {
        let mut s = Samples::new(1_000);
        for i in 1..=100 {
            s.push(i * 1_000);
        }
        assert_eq!(s.percentile_us(50.0), 50_000);
        assert_eq!(s.percentile_us(95.0), 95_000);
        assert_eq!(s.max_us(), 100_000);
        assert_eq!(s.count(), 100);
        assert!((s.above(90_000) - 10.0).abs() < 0.01);
    }

    #[test]
    fn window_evicts_oldest_and_keeps_totals() {
        let mut s = Samples::new(16);
        for i in 0..1_000u64 {
            s.push(i);
        }
        assert_eq!(s.count(), 1_000);
        assert_eq!(s.window_len(), 16);
        assert_eq!(s.percentile_us(100.0), 999);
        assert_eq!(s.percentile_us(0.0), 984);
    }

    #[test]
    fn budget_totals_are_the_sum_of_stages() {
        let b = Budget::LOW_LATENCY_1080P30;
        assert_eq!(b.total_us(), 40_000);
        for stage in STAGES {
            if stage == Stage::Send {
                continue;
            }
            assert!(b.stage_us(stage) > 0, "{} has no budget", stage.name());
        }
    }
}
