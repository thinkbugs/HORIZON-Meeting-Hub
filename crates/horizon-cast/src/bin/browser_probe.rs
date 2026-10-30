//! Receives a browser extension's H.264/PCM WebSocket stream.
//!
//! ```text
//! browser-probe [--port=9407] [--seconds=20] [--token=CODE] [--out=frames.h264]
//! ```
//!
//! Point a sender at `ws://<this machine>:<port>/stream?token=<CODE>`. The probe
//! prints connection and media counters, and `--out` writes the Annex B video so
//! ffprobe can confirm the stream the extension produced.

use std::fs::File;
use std::io::Write;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use horizon_cast::airplay::mirror::{FrameSink, VideoFrame};
use horizon_cast::browser::{BrowserMirrorConfig, BrowserMirrorReceiver};
use horizon_cast::discovery::primary_interface;

fn main() {
    let args = Args::parse();
    let sink = Arc::new(ProbeSink {
        frames: AtomicU64::new(0),
        keyframes: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        announced: AtomicBool::new(false),
        file: Mutex::new(args.out.as_ref().and_then(|path| File::create(path).ok())),
    });
    let receiver = BrowserMirrorReceiver::start(
        BrowserMirrorConfig {
            address: Ipv4Addr::UNSPECIFIED,
            port: args.port,
            name: "HORIZON browser-probe".to_string(),
            token: args.token.clone(),
        },
        sink.clone(),
    )
    .unwrap_or_else(|error| {
        eprintln!("browser-probe: cannot listen on port {}: {error}", args.port);
        std::process::exit(1);
    });

    let query = args
        .token
        .as_ref()
        .map(|token| format!("?token={token}"))
        .unwrap_or_default();
    println!(
        "listening   ws://{}:{}/stream{} ({}s)",
        primary_interface(),
        receiver.port(),
        query,
        args.seconds
    );

    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds.max(1.0));
    let mut last = String::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let status = receiver.status();
        let line = format!(
            "{} frames={} key={} {}x{} video={} KiB audio={} KiB",
            match (&status.peer, status.connected) {
                (Some(peer), true) => format!("peer {peer}"),
                _ => "waiting".to_string(),
            },
            status.frames,
            status.keyframes,
            status.width,
            status.height,
            status.video_bytes / 1024,
            status.audio_bytes / 1024,
        );
        if line != last {
            println!("{line}");
            last = line;
        }
    }
    receiver.stop();

    let frames = sink.frames.load(Ordering::Relaxed);
    let bytes = sink.bytes.load(Ordering::Relaxed);
    println!("done        {frames} frames, {bytes} bytes");
    if let Some(path) = args.out {
        println!("wrote       {path}");
    }
}

struct Args {
    port: u16,
    seconds: f64,
    token: Option<String>,
    out: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            port: horizon_cast::browser::BROWSER_MIRROR_PORT,
            seconds: 20.0,
            token: None,
            out: None,
        };
        for arg in std::env::args().skip(1) {
            if let Some(value) = arg.strip_prefix("--port=") {
                args.port = value.parse().unwrap_or(args.port);
            } else if let Some(value) = arg.strip_prefix("--seconds=") {
                args.seconds = value.parse().unwrap_or(args.seconds);
            } else if let Some(value) = arg.strip_prefix("--token=") {
                args.token = Some(value.to_string());
            } else if let Some(value) = arg.strip_prefix("--out=") {
                args.out = Some(value.to_string());
            } else if arg == "--help" || arg == "-h" {
                println!(
                    "usage: browser-probe [--port=9407] [--seconds=20] [--token=CODE] [--out=frames.h264]"
                );
                std::process::exit(0);
            }
        }
        args
    }
}

struct ProbeSink {
    frames: AtomicU64,
    keyframes: AtomicU64,
    bytes: AtomicU64,
    announced: AtomicBool,
    file: Mutex<Option<File>>,
}

impl FrameSink for ProbeSink {
    fn on_frame(&self, frame: VideoFrame) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        if frame.keyframe {
            self.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        self.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
        if !self.announced.swap(true, Ordering::Relaxed) {
            println!(
                "first frame {}x{} {}B key={} pts={}us",
                frame.width,
                frame.height,
                frame.data.len(),
                frame.keyframe,
                frame.pts_us
            );
        }
        if let Ok(mut slot) = self.file.lock() {
            if let Some(file) = slot.as_mut() {
                let _ = file.write_all(&frame.data);
            }
        }
    }
}
