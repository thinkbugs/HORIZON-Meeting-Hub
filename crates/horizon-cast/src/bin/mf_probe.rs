//! Media Foundation codec probe: the P0-B gate.
//!
//! Answers, on the machine that will actually run the product:
//! - which H.264 encoders/decoders exist, hardware and software, by name;
//! - whether `MF_LOW_LATENCY` really gives one-in/one-out;
//! - encode and decode cost per frame at 720p and 1080p;
//! - whether feeding RGB32 beats converting to NV12 first.
//!
//! Everything the latency budget in [`horizon_cast::metrics::Budget`] assumes is
//! verified here before any network code is written.

use horizon_cast::codec::{
    self, bgra_to_nv12, decoder_candidates, encoder_candidates, has_start_code, nal_types,
    AccessUnit, CodecProp, DecodedFrame, EncoderConfig, H264Decoder, H264Encoder, InputFormat,
    PropStage, PropValue,
};
use horizon_cast::metrics::{now_us, Budget, PipelineStats, Samples, Timing};
use windows::Win32::Media::MediaFoundation::{
    MFVideoFormat_H264, CODECAPI_AVDecNumWorkerThreads, CODECAPI_AVLowLatencyMode,
};
use std::io::Write;

#[derive(Clone, Copy)]
struct Scenario {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_mbps: u32,
    frames: u32,
    input: InputFormat,
}

fn label(s: &Scenario) -> String {
    format!(
        "{}x{}@{} {}k {}",
        s.width,
        s.height,
        s.fps,
        s.bitrate_mbps * 1000,
        match s.input {
            InputFormat::Rgb32 => "rgb32",
            InputFormat::Nv12 => "nv12",
        }
    )
}

/// A frame with real spatial structure and motion, because a flat test pattern
/// encodes in microseconds and tells us nothing.
fn synth_frame(width: usize, height: usize, n: u32, dst: &mut [u8]) {
    let shift = (n as usize) * 4;
    for y in 0..height {
        let row = y * width * 4;
        for x in 0..width {
            let o = row + x * 4;
            let v = ((x + shift) as u8) ^ ((y as u8).wrapping_mul(3));
            dst[o] = v;
            dst[o + 1] = v.rotate_right(3);
            dst[o + 2] = (x as u8).wrapping_add(y as u8);
            dst[o + 3] = 255;
        }
    }
    // A moving bright bar guarantees the encoder cannot trivially skip the frame.
    let bar_x = (shift * 3) % width.max(1);
    for y in 0..height {
        for x in 0..80usize.min(width) {
            let o = (y * width + (bar_x + x) % width.max(1)) * 4;
            dst[o] = 255;
            dst[o + 1] = 255;
            dst[o + 2] = 255;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let only_1080 = args.iter().any(|a| a == "--1080");
    let mut out = std::io::stdout();

    println!("== HORIZON codec probe ==");
    if let Err(e) = codec::startup() {
        eprintln!("MFStartup failed: {e}");
        std::process::exit(1);
    }
    println!("Media Foundation started (MF_VERSION negotiated)");

    for (kind, hardware) in [("encoder", false), ("encoder", true), ("decoder", false), ("decoder", true)] {
        let list = if kind == "encoder" {
            encoder_candidates(hardware)
        } else {
            decoder_candidates(hardware)
        };
        match list {
            Ok(v) => {
                println!("{kind} [{}/{}] = {}", if hardware { "hw" } else { "sw" }, v.len(), 
                    if v.is_empty() { "none".to_string() } else {
                        v.iter().map(|m| m.label()).collect::<Vec<_>>().join(", ")
                    });
            }
            Err(e) => println!("{kind} {} enum error: {e}", if hardware { "hw" } else { "sw" }),
        }
    }

    // Capability dump before timing: whether the encoder takes RGB32 decides if
    // the capture stage has to own a colour conversion. Guessing wrong here means
    // rewriting the whole frame path later.
    for (what, category, input) in [
        ("encoder inputs", codec::MFT_CATEGORY_VIDEO_ENCODER, true),
        ("decoder outputs", codec::MFT_CATEGORY_VIDEO_DECODER, false),
    ] {
        for (mft, subtypes) in codec::stream_types(&category, false, input, Some(&MFVideoFormat_H264))
        {
            println!("{what} [{mft}] = {}", subtypes.join(", "));
        }
    }

    let scenarios = if only_1080 {
        vec![
            Scenario { width: 1920, height: 1080, fps: 30, bitrate_mbps: 8, frames: 240, input: InputFormat::Rgb32 },
            Scenario { width: 1920, height: 1080, fps: 30, bitrate_mbps: 8, frames: 240, input: InputFormat::Nv12 },
        ]
    } else {
        vec![
            Scenario { width: 1280, height: 720, fps: 30, bitrate_mbps: 4, frames: 240, input: InputFormat::Rgb32 },
            Scenario { width: 1280, height: 720, fps: 30, bitrate_mbps: 4, frames: 240, input: InputFormat::Nv12 },
            Scenario { width: 1920, height: 1080, fps: 30, bitrate_mbps: 8, frames: 240, input: InputFormat::Rgb32 },
            Scenario { width: 1920, height: 1080, fps: 30, bitrate_mbps: 8, frames: 240, input: InputFormat::Nv12 },
        ]
    };

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    println!("logical cpus = {threads}");

    // Ask the encoder what it thinks the H.264 profile and level enums are, then
    // negotiate with its own numbers.
    let adv = match codec::advertised_h264_profile() {
        Ok((p, l, n)) => {
            println!("advertised [{n}] profile={p:#06x} level={l}");
            (p, l)
        }
        Err(e) => {
            println!("advertised profile unavailable ({e}), using defaults");
            (0, 0)
        }
    };
    let _ = out.flush();

    if args.iter().any(|a| a == "--ladder") {
        println!("\n--- profile/level ladder ---");
        // Every encoding of "Constrained Baseline" worth trying, plus the plain
        // profile ids and both level conventions (idc and idc*10).
        const PROFILES: [u32; 5] = [66, 322, 88, 77, 100];
        const LEVELS: [u32; 8] = [30, 31, 40, 41, 300, 310, 400, 410];
        for (p, l) in codec::profile_ladder(1920, 1080, 30, 8_000_000, &PROFILES, &LEVELS) {
            match sps_for(p, l) {
                Some((sp, sl)) => println!(
                    "accepted set profile={p} level={l} -> sps profile_idc={sp} level_idc={sl}"
                ),
                None => println!("accepted set profile={p} level={l} -> no SPS decoded"),
            }
        }
    }

    if args.iter().any(|a| a == "--decapi") {
        decapi_sweep(120);
        return;
    }

    for s in scenarios {
        run(&s, threads, adv);
    }
}

/// Encode a few flat frames with one candidate setting and read the answer out of
/// the SPS. A return code says the attribute was accepted; only the bitstream says
/// what the receiver will actually see.
fn sps_for(profile: u32, level: u32) -> Option<(u8, u8)> {
    let mut cfg = EncoderConfig::new(1920, 1080, 30, 8_000_000, InputFormat::Nv12);
    cfg.profile = profile;
    cfg.level = level;
    let mut enc = H264Encoder::open(cfg).ok()?;
    let frame = vec![16u8; 1920 * 1080 * 3 / 2];
    for _ in 0..8 {
        if let Ok(Some(au)) = enc.encode(&frame) {
            if let Some(v) = codec::sps_profile_level(&au.bytes) {
                return Some(v);
            }
        }
    }
    None
}

fn run(s: &Scenario, threads: usize, adv: (u32, u32)) {
    let name = label(s);
    let mut cfg = EncoderConfig::new(
        s.width,
        s.height,
        s.fps,
        s.bitrate_mbps * 1_000_000,
        s.input,
    );
    if adv.0 != 0 && adv.1 != 0 {
        cfg.profile = adv.0;
        cfg.level = adv.1;
    }
    let mut enc = match H264Encoder::open(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("\n--- {name} ---");
            if matches!(s.input, InputFormat::Rgb32) {
                // The finding, not a failure: no in-box encoder takes RGB, so the
                // capture stage must own a BGRA -> NV12 conversion.
                println!("rgb32 input rejected as expected ({e}) - conversion is mandatory");
            } else {
                println!("encoder open FAILED {e}");
            }
            return;
        }
    };
    println!("\n--- {name} ---");
    println!("encoder: {}", enc.description());
    println!("negotiation: {}", enc.negotiation());
    println!("encoder low latency api: {}", enc.low_latency_api());

    let w = s.width as usize;
    let h = s.height as usize;
    let bgra_len = w * h * 4;
    let nv12_len = w * h * 3 / 2;
    let mut bgra = vec![0u8; bgra_len];
    let mut nv12 = vec![0u8; nv12_len];
    let mut convert = Samples::new(4096);
    let mut synth = Samples::new(4096);
    let mut stats = PipelineStats::new();
    let mut bus = 0usize;
    let mut aus: Vec<Vec<u8>> = Vec::with_capacity(s.frames as usize);
    let mut first_au: Option<AccessUnit> = None;
    let mut keys = 0usize;
    let mut sent = 0usize;

    stats.reset_clock();
    for n in 0..s.frames {
        let ts = Timing::start();
        synth_frame(w, h, n, &mut bgra);
        synth.push(ts.us());
        let encode_input = if matches!(s.input, InputFormat::Nv12) {
            // Timed on its own: synthesis is the expensive part of generating a
            // test frame, and folding it into the converter's number is how a
            // 1 ms stage gets reported as 10 ms.
            let tc = Timing::start();
            bgra_to_nv12(&bgra, w, h, &mut nv12, threads);
            convert.push(tc.us());
            &nv12[..]
        } else {
            &bgra[..]
        };
        sent += 1;
        match enc.encode(encode_input) {
            Ok(Some(au)) => {
                bus += au.bytes.len();
                if au.keyframe {
                    keys += 1;
                }
                if first_au.is_none() {
                    first_au = Some(au.clone());
                }
                stats.per_stage[horizon_cast::metrics::Stage::Encode.index()].push(au.encode_us);
                aus.push(au.bytes);
            }
            Ok(None) => stats.note_dropped(),
            Err(e) => {
                println!("{name}: encode error at frame {n}: {e}");
                return;
            }
        }
    }
    let wall = stats.elapsed().as_secs_f64();
    println!(
        "encode: {sent} frames in {wall:.2}s = {:.1} fps | keyframes={keys} bitrate={:.2} Mbps",
        sent as f64 / wall,
        bus as f64 * 8.0 / wall / 1e6
    );
    println!(
        "one-in/one-out: in={sent} out={} ratio={:.3} queue_depth={}",
        aus.len(),
        enc.output_ratio(),
        enc.queue_depth()
    );
    if convert.count() > 0 {
        println!("convert  {}", convert.summary());
        println!("  budget {:.1}ms", Budget::LOW_LATENCY_1080P30.convert_us as f64 / 1000.0);
        println!("  (synthetic frame generation, not part of the pipeline: {})", synth.summary());
    }
    if let Some(au) = &first_au {
        println!(
            "first AU: {} bytes, start_code={} nal={:?}",
            au.bytes.len(),
            has_start_code(&au.bytes),
            &nal_types(&au.bytes)[..nal_types(&au.bytes).len().min(6)]
        );
        match codec::sps_profile_level(&au.bytes) {
            // The claim is Constrained Baseline; profile_idc 66 is the proof.
            Some((p, l)) => println!("sps: profile_idc={p} level_idc={l}"),
            None => println!("sps: no SPS in the first AU"),
        }
        // The receiver's startup delay is this number, not a guess: a decoder that
        // fills its reference list before emitting anything spends num_ref_frames
        // frame-times holding the first picture.
        match codec::sps_num_ref_frames(&au.bytes) {
            Some(n) => println!("sps: num_ref_frames={n}"),
            None => println!("sps: num_ref_frames not parsed"),
        }
    }
    println!(
        "submit: {sent} frames in, {} AUs out, {} frames still queued",
        aus.len(),
        sent - aus.len()
    );
    let enc_p95 =
        stats.per_stage[horizon_cast::metrics::Stage::Encode.index()].percentile_us(95.0);
    let verdict = if (enc_p95 as u64) < Budget::LOW_LATENCY_1080P30.encode_us {
        "OK"
    } else {
        "OVER"
    };
    println!(
        "  encode budget {:.1}ms -> p95 {:.1}ms {verdict}",
        Budget::LOW_LATENCY_1080P30.encode_us as f64 / 1000.0,
        enc_p95 as f64 / 1000.0,
    );

    // Decode what we just produced: the receiver has to keep up with the sender.
    let mut dec = match H264Decoder::open(s.width, s.height) {
        Ok(d) => d,
        Err(e) => {
            println!("{name}: decoder open FAILED {e}");
            return;
        }
    };
    println!("decoder: {}", dec.description());
    println!("decoder codec api: {}", dec.api_report());
    let mut dstats = PipelineStats::new();
    dstats.reset_clock();
    let mut decoded: Vec<DecodedFrame> = Vec::new();
    let mut first_out = usize::MAX;
    for (i, au) in aus.iter().enumerate() {
        match dec.decode(au) {
            Ok(frames) => {
                if !frames.is_empty() && i < first_out {
                    first_out = i;
                }
                for f in frames {
                    dstats.per_stage[horizon_cast::metrics::Stage::Decode.index()].push(f.decode_us);
                    dstats.frames += 1;
                    if decoded.len() < 2 {
                        decoded.push(f);
                    }
                }
            }
            Err(e) => {
                println!("{name}: decode error: {e}");
                return;
            }
        }
    }
    // Whatever is still inside the decoder after end-of-stream is pipeline delay,
    // and delay at 30 fps is seconds, not milliseconds.
    let pending = dec.flush().map(|v| v.len()).unwrap_or(0);
    let dwall = dstats.elapsed().as_secs_f64();
    println!(
        "decode: {} frames in {dwall:.2}s = {:.1} fps | ratio={:.3} queue={}",
        dstats.frames,
        dstats.frames as f64 / dwall,
        dec.output_ratio(),
        dec.queue_depth()
    );
    println!(
        "decoder delay: first output at AU #{first_out}, {pending} frames only released on flush"
    );
    println!("{}", dstats.report());
    println!(
        "  decode budget {:.1}ms -> p95 {:.1}ms",
        Budget::LOW_LATENCY_1080P30.decode_us as f64 / 1000.0,
        dstats.per_stage[horizon_cast::metrics::Stage::Decode.index()].percentile_us(95.0) as f64
            / 1000.0
    );
    if let Some(f) = decoded.first() {
        let mean_y: u64 = f.y.iter().map(|v| *v as u64).sum::<u64>() / f.y.len() as u64;
        println!("  decoded frame mean luma = {mean_y} (must be 16..=235)");
    }
    let _ = now_us();
}

/// Encode a scenario and keep only the access units, for the decode-side sweeps.
fn collect_aus(s: &Scenario, threads: usize) -> Option<Vec<Vec<u8>>> {
    let mut enc = match H264Encoder::open(EncoderConfig::new(
        s.width,
        s.height,
        s.fps,
        s.bitrate_mbps * 1_000_000,
        s.input,
    )) {
        Ok(e) => e,
        Err(e) => {
            println!("sweep: encoder open failed {e}");
            return None;
        }
    };
    let w = s.width as usize;
    let h = s.height as usize;
    let mut bgra = vec![0u8; w * h * 4];
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    let mut aus: Vec<Vec<u8>> = Vec::with_capacity(s.frames as usize);
    for n in 0..s.frames {
        synth_frame(w, h, n, &mut bgra);
        bgra_to_nv12(&bgra, w, h, &mut nv12, threads);
        match enc.encode(&nv12[..]) {
            Ok(Some(au)) => aus.push(au.bytes),
            Ok(None) => {}
            Err(e) => {
                println!("sweep: encode error at {n}: {e}");
                return None;
            }
        }
    }
    Some(aus)
}

/// Decode the same bitstream under one set of codec-API properties and report the
/// only numbers that matter for a receiver: when the first frame lands, how many
/// are still held at the end, and what each frame costs.
fn decode_report(
    name: &str,
    width: u32,
    height: u32,
    fps: u32,
    aus: &[Vec<u8>],
    props: &[CodecProp],
) {
    let mut dec = match H264Decoder::open_with(width, height, fps, props) {
        Ok(d) => d,
        Err(e) => {
            println!("{name:28} open failed: {e}");
            return;
        }
    };
    let mut times = Samples::new(4096);
    // How far behind the frame it came from each output is, measured from the
    // timestamps we handed in. Delay shows up as a constant; a drop shows up as
    // zero after the first frames, and only one of those is fixable.
    let mut lag = Samples::new(4096);
    let mut first = usize::MAX;
    let mut out = 0usize;
    let mut fail = String::new();
    for (i, au) in aus.iter().enumerate() {
        match dec.decode(au) {
            Ok(frames) => {
                if !frames.is_empty() && i < first {
                    first = i;
                }
                for f in frames {
                    times.push(f.decode_us);
                    if f.pts_us != i64::MIN {
                        lag.push((i as i64 * 33_333 - f.pts_us).max(0) as u64);
                    }
                    out += 1;
                }
            }
            Err(e) => {
                fail = format!("decode error at AU #{i}: {e}");
                break;
            }
        }
    }
    let mut held = 0usize;
    if fail.is_empty() {
        match dec.flush() {
            Ok(frames) => {
                held = frames.len();
                for f in frames {
                    times.push(f.decode_us);
                    out += 1;
                }
            }
            Err(e) => fail = format!("flush failed: {e}"),
        }
    }
    let (need_more, last_err) = dec.stall_stats();
    let first_txt = if first == usize::MAX { "never".to_string() } else { format!("#{first}") };
    let lag_txt = if lag.count() == 0 {
        "no pts".to_string()
    } else {
        format!(
            "p50={:.0}ms p95={:.0}ms",
            lag.percentile_us(50.0) as f64 / 1000.0,
            lag.percentile_us(95.0) as f64 / 1000.0
        )
    };
    println!(
        "{name:28} api[{}] first={first_txt} out={out}/{} held_at_eos={held} lag {lag_txt} decode p50={:.2}ms p95={:.2}ms need_more={need_more} last_err={last_err:#010x}{}",
        dec.api_report(),
        aus.len(),
        times.percentile_us(50.0) as f64 / 1000.0,
        times.percentile_us(95.0) as f64 / 1000.0,
        if fail.is_empty() { String::new() } else { format!(" | {fail}") },
    );
}

/// P0-B, second half: what actually makes the in-box decoder release a frame per
/// frame.
///
/// The default path holds 27 access units before its first output — ~900 ms at
/// 30 fps — and neither `MF_LOW_LATENCY` nor `CODECAPI_AVLowLatencyMode` (which the
/// decoder rejects with `E_INVALIDARG` at both setup stages) changes it. The SPS says
/// `num_ref_frames = 1`, so it is not the reference list. This sweeps one variable at
/// a time against a single fixed bitstream and reports the frame that first comes
/// out, so the measurement — not the documentation — decides the receiver's design.
fn decapi_sweep(frames: u32) {
    let s = Scenario {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate_mbps: 4,
        frames,
        input: InputFormat::Nv12,
    };
    let aus = match collect_aus(&s, 4) {
        Some(a) => a,
        None => return,
    };
    println!(
        "\n--- decoder pipeline sweep ({} AUs, one bitstream) ---",
        aus.len()
    );
    let threads_at = |n: i32| {
        CodecProp::new(
            "AVDecNumWorkerThreads",
            CODECAPI_AVDecNumWorkerThreads,
            PropValue::Int(n),
            PropStage::BeforeTypes,
        )
    };
    let low_latency = || {
        CodecProp::new(
            "AVLowLatencyMode",
            CODECAPI_AVLowLatencyMode,
            PropValue::Bool(true),
            PropStage::BeforeTypes,
        )
    };
    let cases: Vec<(&str, u32, Vec<CodecProp>)> = vec![
        ("default threads, no fps", 0, vec![]),
        ("default threads, fps=30", s.fps, vec![]),
        ("threads=1, no fps", 0, vec![threads_at(1)]),
        ("threads=1, fps=30", s.fps, vec![threads_at(1)]),
        ("threads=2, fps=30", s.fps, vec![threads_at(2)]),
        ("threads=4, fps=30", s.fps, vec![threads_at(4)]),
        ("threads=8, fps=30", s.fps, vec![threads_at(8)]),
        (
            "threads=1+lowlatency, fps",
            s.fps,
            vec![threads_at(1), low_latency()],
        ),
        ("threads=1, lowlatency@start", s.fps, vec![
            CodecProp::new(
                "AVDecNumWorkerThreads",
                CODECAPI_AVDecNumWorkerThreads,
                PropValue::Int(1),
                PropStage::BeforeTypes,
            ),
            CodecProp::new(
                "AVLowLatencyMode",
                CODECAPI_AVLowLatencyMode,
                PropValue::Bool(true),
                PropStage::AfterStreamStart,
            ),
        ]),
    ];
    for (name, fps, props) in cases {
        decode_report(name, s.width, s.height, fps, &aus, &props);
    }
}
