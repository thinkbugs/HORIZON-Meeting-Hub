//! Receiver lifecycle for the launcher UI.
//!
//! The engine lives in `horizon-cast` and knows nothing about Tauri. This module
//! owns the two long-running receivers an operator toggles from the 投屏 panel,
//! and turns their counters into the shape the panel draws:
//!
//! - AirPlay (RAOP) audio runs an accept loop on its own thread. Audio goes to
//!   the default output device; the statistics are cumulative for the lifetime
//!   of the receiver, which is what a status strip wants to show.
//! - DLNA video runs the HTTP control plane on top of the stage's player. It is
//!   handed a `Transport` rather than creating one, so a controller, a phone and
//!   the big screen all drive the same queue in front of the same surface.
//!
//! Both are idempotent: asking to start a running receiver returns its status
//! instead of binding a second one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use horizon_cast::airplay::mdns::Advert;
use horizon_cast::airplay::mirror::VideoFrame;
use horizon_cast::airplay::session::{
    Config as AirplayConfig, Receiver as AirplayReceiver, SessionReport, Stats as AirplayStats,
};
use horizon_cast::browser::{
    BrowserMirrorConfig, BrowserMirrorReceiver, BROWSER_MIRROR_PORT,
};
use horizon_cast::discovery::{primary_interface, Discovery, Protocol};
use horizon_cast::dlna::{Dmr, DmrConfig};
use horizon_cast::player::{Phase, Transport};
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, Response as IpcResponse};
use tauri::Manager;

/// The port the renderer's description and control endpoints listen on. Fixed so
/// a controller that saved this device keeps finding it at the same address after
/// a restart; the firewall rule is program-scoped, so it needs no new entry.
const DMR_HTTP_PORT: u16 = 8290;

// ------------------------------------------------------------------ state

#[derive(Default)]
pub struct Receivers {
    airplay: Mutex<Option<AirplayRuntime>>,
    dlna: Mutex<Option<DlnaRuntime>>,
    browser: Mutex<Option<BrowserRuntime>>,
}

struct AirplayRuntime {
    advert: Advert,
    stats: Arc<AirplayStats>,
    stop: Arc<AtomicBool>,
    receiver: Arc<AirplayReceiver>,
    thread: Option<JoinHandle<()>>,
    last: Arc<Mutex<Option<SessionReport>>>,
    frames: Arc<FrameHub>,
}

/// Carries decrypted mirror frames from the engine thread to the webview.
///
/// The box between them is a Tauri channel that only exists while the 投屏 panel
/// shows the preview; the engine keeps running with no channel attached. Each
/// message is one frame with a fixed 24-byte header so the frontend can read it
/// without a second serialization:
///
/// ```text
/// 0 u8 kind (1 = H.264 Annex B) | 1 u8 flags (bit 0: keyframe, bit 1: pts is
/// sender wall clock, bit 2: a frame was dropped just before this one)
/// 4 u64 pts_us | 12 u16 width | 14 u16 height | 16 u32 payload length
/// 24 payload
/// ```
/// Frames a preview can fall behind before they are discarded. A drop leaves a
/// hole in the reference chain, and an AirPlay sender emits its only IDR when the
/// mirror connection opens, so a hole is not recoverable mid-session: this queue
/// is sized to absorb a webview hiccup rather than to bound latency, which the
/// preview does by painting only the newest decoded frame.
const FRAME_QUEUE: usize = 16;

/// One preview owns the hub at a time. Attach records the owner token; detach
/// only clears a matching token, because a preview that is being replaced
/// re-attaches while the previous instance's cleanup is still in flight.
struct FrameHub {
    state: Arc<FrameState>,
}

struct FrameState {
    slot: Mutex<FrameSlot>,
    ready: Condvar,
    closed: AtomicBool,
    dropped: AtomicU64,
}

/// Frames a join buffer may hold, and the byte budget it may not exceed. Four
/// seconds of 1080p H.264 fits comfortably, which is long enough to cover the
/// status poll that opens the preview; a run that outgrows the caps is dropped
/// whole, because a run with its head removed cannot be decoded.
const JOIN_FRAMES: usize = 120;
const JOIN_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct FrameSlot {
    owner: Option<String>,
    channel: Option<Channel<IpcResponse>>,
    queue: VecDeque<Vec<u8>>,
    /// A frame was dropped since the last delivery; the next message tells the
    /// preview that its reference chain has a hole.
    lost: bool,
    /// Contiguous messages from the newest IDR to the newest frame, headers
    /// included. A preview that attaches mid-stream has no other entry point:
    /// AirPlay sends its opening IDR once, when the mirror connection starts,
    /// so replaying this run is what lets a late preview decode at all.
    join: VecDeque<Vec<u8>>,
    join_bytes: usize,
    /// Replay handed to the current preview when it attached. Drained before
    /// live frames so the backlog cap cannot push the IDR out of the queue.
    replay: VecDeque<Vec<u8>>,
}

impl FrameHub {
    fn new() -> Self {
        let state = Arc::new(FrameState {
            slot: Mutex::new(FrameSlot::default()),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        });

        // Delivery runs here, never on the receiver's socket thread: a webview
        // that stops draining must not stall the network read loop.
        let worker = Arc::clone(&state);
        thread::Builder::new()
            .name("frame-forward".into())
            .spawn(move || {
                let mut slot = match worker.slot.lock() {
                    Ok(slot) => slot,
                    Err(_) => return,
                };
                loop {
                    if worker.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut message = match slot.replay.pop_front() {
                        Some(message) => message,
                        None => match slot.queue.pop_front() {
                            Some(message) => message,
                            None => {
                                slot = match worker.ready.wait(slot) {
                                    Ok(slot) => slot,
                                    Err(_) => break,
                                };
                                continue;
                            }
                        },
                    };
                    let Some(channel) = slot.channel.clone() else {
                        slot.queue.clear();
                        slot.replay.clear();
                        continue;
                    };
                    // The first frame after a hole carries bit 2, unless it is
                    // itself an IDR: a keyframe restarts the reference chain.
                    if slot.lost {
                        slot.lost = false;
                        if message[1] & 1 == 0 {
                            message[1] |= 4;
                        }
                    }
                    drop(slot);
                    let _ = channel.send(IpcResponse::new(message));
                    slot = match worker.slot.lock() {
                        Ok(slot) => slot,
                        Err(_) => break,
                    };
                }
            })
            .ok();

        Self { state }
    }

    fn attach(&self, owner: String, channel: Channel<IpcResponse>) {
        if let Ok(mut slot) = self.state.slot.lock() {
            slot.owner = Some(owner);
            slot.channel = Some(channel);
            // Anything queued for the previous preview is stale; the join
            // buffer is not, it starts at an IDR and ends at the newest frame.
            slot.queue.clear();
            slot.replay = slot.join.clone();
            slot.lost = false;
            self.state.ready.notify_one();
        }
    }

    fn detach(&self, owner: &str) {
        if let Ok(mut slot) = self.state.slot.lock() {
            if slot.owner.as_deref() == Some(owner) {
                slot.owner = None;
                slot.channel = None;
                slot.queue.clear();
                slot.replay.clear();
            }
        }
    }

    fn send(&self, frame: &VideoFrame) {
        let Ok(mut slot) = self.state.slot.lock() else {
            return;
        };
        let message = frame_message(frame);
        if frame.keyframe {
            slot.join.clear();
            slot.join_bytes = 0;
        }
        if frame.keyframe || !slot.join.is_empty() {
            slot.join_bytes += message.len();
            slot.join.push_back(message.clone());
            if slot.join.len() > JOIN_FRAMES || slot.join_bytes > JOIN_BYTES {
                // Cutting the run anywhere also destroys it: every delta needs
                // the frames between it and the IDR at the head.
                slot.join.clear();
                slot.join_bytes = 0;
            }
        }
        if slot.channel.is_none() {
            return;
        }
        if slot.queue.len() >= FRAME_QUEUE && slot.replay.is_empty() {
            // The queue is full: drop the oldest, which is the one the worker
            // would have sent next, and remember that the frames still queued
            // now reference a hole. While a replay is still draining the queue
            // is allowed to grow: dropping here would cut a hole between the
            // replayed run and the live frames, and a receiver-side decoder
            // cannot ask the sender for another IDR.
            slot.queue.pop_front();
            slot.lost = true;
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
        }
        slot.queue.push_back(message);
        self.state.ready.notify_one();
    }

    fn drops(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for FrameHub {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Relaxed);
        self.state.ready.notify_all();
    }
}

fn frame_message(frame: &VideoFrame) -> Vec<u8> {
    let width = frame.width.min(u16::MAX as u32) as u16;
    let height = frame.height.min(u16::MAX as u32) as u16;
    let mut body = Vec::with_capacity(24 + frame.data.len());
    body.push(1);
    body.push(u8::from(frame.keyframe) | if frame.wallclock { 2 } else { 0 });
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&frame.pts_us.to_le_bytes());
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&(frame.data.len() as u32).to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&frame.data);
    body
}

struct DlnaRuntime {
    dmr: Dmr,
}

struct BrowserRuntime {
    receiver: BrowserMirrorReceiver,
    frames: Arc<FrameHub>,
    name: String,
    token: String,
    address: std::net::Ipv4Addr,
}

impl Receivers {
    pub fn start_airplay(&self, name: &str) -> Result<AirplayStatus, String> {
        let mut slot = self.lock_airplay()?;
        if let Some(runtime) = slot.as_ref() {
            return Ok(runtime.status());
        }

        let frames = Arc::new(FrameHub::new());
        let sink_frames = Arc::clone(&frames);
        let receiver = AirplayReceiver::bind(AirplayConfig {
            name: name.to_string(),
            frame_sink: Some(Arc::new(move |frame: VideoFrame| sink_frames.send(&frame))),
            ..AirplayConfig::default()
        })
        .map_err(|error| format!("无法启动 AirPlay 接收：{error}"))?;
        let advert = receiver.advert().clone();
        let stats = Arc::new(AirplayStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let last = Arc::new(Mutex::new(None));
        let receiver = Arc::new(receiver);

        let thread_receiver = Arc::clone(&receiver);
        let thread_stats = Arc::clone(&stats);
        let thread_stop = Arc::clone(&stop);
        let thread_last = Arc::clone(&last);
        let thread = thread::Builder::new()
            .name("airplay-receiver".into())
            .spawn(move || loop {
                match thread_receiver.accept_until(Arc::clone(&thread_stats), &thread_stop) {
                    Ok(Some(report)) => {
                        if let Ok(mut last) = thread_last.lock() {
                            *last = Some(report);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if thread_stop.load(Ordering::Relaxed) {
                            break;
                        }
                        eprintln!("airplay receiver: {error}");
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            })
            .map_err(|error| format!("无法启动 AirPlay 线程：{error}"))?;

        let runtime = AirplayRuntime {
            advert,
            stats,
            stop,
            receiver,
            thread: Some(thread),
            last,
            frames,
        };
        let status = runtime.status();
        *slot = Some(runtime);
        Ok(status)
    }

    pub fn attach_preview(&self, owner: String, channel: Channel<IpcResponse>) -> Result<(), String> {
        let slot = self.lock_airplay()?;
        match slot.as_ref() {
            Some(runtime) => {
                runtime.frames.attach(owner, channel);
                Ok(())
            }
            None => Err("请先开启 AirPlay 接收".to_string()),
        }
    }

    pub fn detach_preview(&self, owner: &str) {
        if let Ok(slot) = self.lock_airplay() {
            if let Some(runtime) = slot.as_ref() {
                runtime.frames.detach(owner);
            }
        }
    }

    pub fn stop_airplay(&self) -> Result<(), String> {
        let runtime = self.lock_airplay()?.take();
        if let Some(mut runtime) = runtime {
            runtime.stop.store(true, Ordering::Relaxed);
            runtime.receiver.disconnect();
            if let Some(thread) = runtime.thread.take() {
                let _ = thread.join();
            }
        }
        Ok(())
    }

    pub fn airplay_status(&self) -> Option<AirplayStatus> {
        self.airplay
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(AirplayRuntime::status))
    }

    /// Start the renderer over the stage's player. `drive` is the queue seen as a
    /// plain `Transport`: a controller's `SetAVTransportURI` becomes a one-item
    /// queue, so the standard control plane keeps working while the native API can
    /// still line up forty items in front of the same surface.
    pub fn start_dlna(&self, name: &str, drive: Arc<dyn Transport>) -> Result<DlnaStatus, String> {
        let mut slot = self.lock_dlna()?;
        if let Some(runtime) = slot.as_ref() {
            return Ok(runtime.status());
        }
        let mut config = DmrConfig {
            friendly_name: name.to_string(),
            // A stable port makes `LOCATION` — and therefore a controller's
            // bookmark of this renderer — survive a restart. SSDP still
            // re-announces, so nothing depends on the number matching.
            port: DMR_HTTP_PORT,
            ..DmrConfig::default()
        };
        let dmr = match Dmr::start_with(config.clone(), Arc::clone(&drive)) {
            Ok(dmr) => dmr,
            // Another renderer on this machine (or a leftover listener) owns the
            // port. Losing DLNA over a port clash is not worth the argument, so
            // take whatever the OS offers and report the address that resulted.
            Err(first) => {
                config.port = 0;
                Dmr::start_with(config, drive)
                    .map_err(|error| format!("无法启动 DLNA 接收：{error}（端口 {DMR_HTTP_PORT} 上已有一次失败：{first}）"))?
            }
        };
        let runtime = DlnaRuntime { dmr };
        let status = runtime.status();
        *slot = Some(runtime);
        Ok(status)
    }

    pub fn stop_dlna(&self) -> Result<(), String> {
        let runtime = self.lock_dlna()?.take();
        if let Some(mut runtime) = runtime {
            runtime.dmr.stop();
        }
        Ok(())
    }

    pub fn dlna_status(&self) -> Option<DlnaStatus> {
        self.dlna
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(DlnaRuntime::status))
    }

    pub fn start_browser(
        &self,
        name: &str,
        token: String,
        hub_port: Option<u16>,
    ) -> Result<BrowserStatus, String> {
        let mut slot = self.lock_browser()?;
        if let Some(runtime) = slot.as_ref() {
            return Ok(runtime.status());
        }

        let frames = Arc::new(FrameHub::new());
        let sink_frames = Arc::clone(&frames);
        let config = BrowserMirrorConfig {
            address: std::net::Ipv4Addr::UNSPECIFIED,
            port: BROWSER_MIRROR_PORT,
            name: name.to_string(),
            token: Some(token.clone()),
            hub_port,
        };
        let receiver = BrowserMirrorReceiver::start(
            config,
            Arc::new(move |frame: VideoFrame| sink_frames.send(&frame)),
        )
        .map_err(|error| format!("无法启动浏览器投屏接收：{error}"))?;

        let mut address = primary_interface();
        if address.is_unspecified() {
            address = std::net::Ipv4Addr::LOCALHOST;
        }
        let runtime = BrowserRuntime {
            receiver,
            frames,
            name: name.to_string(),
            token,
            address,
        };
        let status = runtime.status();
        *slot = Some(runtime);
        Ok(status)
    }

    pub fn stop_browser(&self) -> Result<(), String> {
        let runtime = self.lock_browser()?.take();
        if let Some(runtime) = runtime {
            runtime.receiver.stop();
        }
        Ok(())
    }

    pub fn browser_status(&self) -> Option<BrowserStatus> {
        self.browser
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(BrowserRuntime::status))
    }

    pub fn attach_browser_preview(
        &self,
        owner: String,
        channel: Channel<IpcResponse>,
    ) -> Result<(), String> {
        let slot = self.lock_browser()?;
        match slot.as_ref() {
            Some(runtime) => {
                runtime.frames.attach(owner, channel);
                Ok(())
            }
            None => Err("请先开启浏览器投屏接收".to_string()),
        }
    }

    pub fn detach_browser_preview(&self, owner: &str) {
        if let Ok(slot) = self.lock_browser() {
            if let Some(runtime) = slot.as_ref() {
                runtime.frames.detach(owner);
            }
        }
    }

    /// Asks the connected sender for an IDR. Used when a preview fell behind
    /// and would otherwise have to wait for the periodic keyframe.
    pub fn request_browser_keyframe(&self) -> Result<(), String> {
        let slot = self.lock_browser()?;
        match slot.as_ref() {
            Some(runtime) => {
                runtime.receiver.request_keyframe();
                Ok(())
            }
            None => Err("浏览器投屏接收未开启".to_string()),
        }
    }

    fn lock_airplay(&self) -> Result<std::sync::MutexGuard<'_, Option<AirplayRuntime>>, String> {
        self.airplay
            .lock()
            .map_err(|_| "投屏接收状态不可用".to_string())
    }

    fn lock_dlna(&self) -> Result<std::sync::MutexGuard<'_, Option<DlnaRuntime>>, String> {
        self.dlna
            .lock()
            .map_err(|_| "投屏接收状态不可用".to_string())
    }

    fn lock_browser(&self) -> Result<std::sync::MutexGuard<'_, Option<BrowserRuntime>>, String> {
        self.browser
            .lock()
            .map_err(|_| "投屏接收状态不可用".to_string())
    }
}

/// Short enough to paste, long enough that guessing it on a LAN is pointless.
/// The pairing code shown on the big screen. Six digits are easy to read across
/// a meeting room and type on a laptop, and they only ever guard a receiver that
/// listens inside one LAN on a fixed port — the same trade-off Miracast and the
/// conference panels everyone knows already make.
fn cast_code() -> String {
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    format!("{:06}", hasher.finish() % 1_000_000)
}

/// The browser token outlives a receiver restart, so the address saved inside
/// the extension keeps working after the app is closed or updated.
fn browser_token(app: &tauri::AppHandle) -> String {
    let Ok(dir) = app.path().app_config_dir() else {
        return cast_code();
    };
    let path = dir.join("browser-token");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let token = raw.trim();
        if token.len() == 6 && token.bytes().all(|byte| byte.is_ascii_digit()) {
            return token.to_string();
        }
    }
    let token = cast_code();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, &token);
    token
}

impl AirplayRuntime {
    fn status(&self) -> AirplayStatus {
        let snapshot = self.stats.snapshot();
        let last = self.last.lock().ok().and_then(|last| last.clone());
        AirplayStatus {
            name: self.advert.name.clone(),
            service: self.advert.service(),
            host: self.advert.host(),
            address: self.advert.address.to_string(),
            port: self.advert.port,
            connected: snapshot.in_session,
            packets: snapshot.packets,
            frames: snapshot.frames,
            seconds: snapshot.frames as f64 / 44_100.0,
            missing: snapshot.missing,
            resend_requests: snapshot.resend_requests,
            volume_db: snapshot.volume_milli as f32 / 1000.0,
            last_peer: last.as_ref().and_then(|report| report.peer.map(|peer| peer.to_string())),
            last_codec: last.map(|report| report.codec),
            paired: snapshot.paired,
            fairplay: snapshot.fairplay,
            mirroring: snapshot.mirroring,
            mirror_frames: snapshot.mirror_frames,
            mirror_keyframes: snapshot.mirror_keyframes,
            mirror_bytes: snapshot.mirror_bytes,
            mirror_width: snapshot.mirror_width,
            mirror_height: snapshot.mirror_height,
            mirror_latency_ms: micros_to_millis(snapshot.mirror_latency_us, snapshot.mirror_frames),
            mirror_latency_max_ms: snapshot.mirror_latency_max_us as f64 / 1_000.0,
            mirror_drift_ms: micros_to_millis(snapshot.mirror_drift_us, snapshot.mirror_frames),
            mirror_drift_max_ms: snapshot.mirror_drift_max_us as f64 / 1_000.0,
            mirror_gap_max_ms: snapshot.mirror_gap_max_us as f64 / 1_000.0,
            mirror_gap_avg_ms: micros_to_millis(snapshot.mirror_gap_us, snapshot.mirror_frames),
            mirror_pipeline_ms: micros_to_millis(
                snapshot.mirror_pipeline_us,
                snapshot.mirror_frames,
            ),
            mirror_drops: self.frames.drops(),
            preview: mirror_perf(),
        }
    }
}

fn micros_to_millis(sum_us: u64, samples: u64) -> f64 {
    if samples == 0 {
        0.0
    } else {
        sum_us as f64 / samples as f64 / 1_000.0
    }
}

impl DlnaRuntime {
    fn status(&self) -> DlnaStatus {
        let state = self.dmr.state();
        DlnaStatus {
            name: self.dmr.info().friendly_name.clone(),
            location: self.dmr.info().location.clone(),
            port: self.dmr.info().port,
            phase: phase_label(state.phase).to_string(),
            position: state.position,
            duration: state.duration,
            volume: state.volume,
            uri: state.uri,
            error: state.error.or_else(|| self.dmr.ssdp_error().map(str::to_string)),
        }
    }
}

impl BrowserRuntime {
    fn status(&self) -> BrowserStatus {
        let snapshot = self.receiver.status();
        BrowserStatus {
            name: self.name.clone(),
            listening: snapshot.listening,
            port: snapshot.port,
            address: format!("{}:{}", self.address, snapshot.port),
            code: self.token.clone(),
            url: format!(
                "ws://{}:{}/stream?token={}",
                self.address, snapshot.port, self.token
            ),
            connected: snapshot.connected,
            peer: snapshot.peer,
            frames: snapshot.frames,
            keyframes: snapshot.keyframes,
            video_bytes: snapshot.video_bytes,
            audio_bytes: snapshot.audio_bytes,
            width: snapshot.width,
            height: snapshot.height,
        }
    }
}

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Loading => "loading",
        Phase::Playing => "playing",
        Phase::Paused => "paused",
        Phase::Stopped => "stopped",
        Phase::Ended => "ended",
        Phase::Error => "error",
    }
}

// ------------------------------------------------------------- serialized

/// What the preview measures on its own side of the wall. The engine can see a
/// frame arrive, but not how long the webview took to turn it into pixels, and
/// that half is where a live view loses the most latency: every frame left
/// undrawn in the decoder queue is a frame the viewer is behind.
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MirrorPerf {
    /// Frames actually painted to the canvas.
    pub painted: u64,
    /// Frames decoded but thrown away because a newer one was already waiting.
    pub skipped: u64,
    /// Deepest decoder queue seen since the last report.
    pub queue_max: u32,
    /// Arrival to paint, per frame, in milliseconds.
    pub in_flight_avg_ms: f64,
    pub in_flight_max_ms: f64,
    /// Frames painted per second.
    pub fps: f64,
}

static MIRROR_PERF: Mutex<Option<MirrorPerf>> = Mutex::new(None);

#[tauri::command]
pub fn mirror_perf_report(perf: MirrorPerf) -> Result<(), String> {
    // The same file the engine traces to, so one log holds both halves of the
    // path and a stall can be read as arrival gap against paint rate.
    if let Some(path) = std::env::var_os("HORIZON_AIRPLAY_TRACE_FILE") {
        use std::io::Write;
        let line = format!(
            "preview window: paint {:.0} fps, skip {}, queue max {}, arrival to paint avg {:.1} ms max {:.1} ms\n",
            perf.fps, perf.skipped, perf.queue_max, perf.in_flight_avg_ms, perf.in_flight_max_ms
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
    let mut slot = MIRROR_PERF.lock().map_err(|_| "预览指标不可用".to_string())?;
    *slot = Some(perf);
    Ok(())
}

fn mirror_perf() -> Option<MirrorPerf> {
    MIRROR_PERF.lock().ok().and_then(|slot| *slot)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AirplayStatus {
    pub name: String,
    pub service: String,
    pub host: String,
    pub address: String,
    pub port: u16,
    pub connected: bool,
    pub packets: u64,
    pub frames: u64,
    pub seconds: f64,
    pub missing: u64,
    pub resend_requests: u64,
    pub volume_db: f32,
    pub last_peer: Option<String>,
    pub last_codec: Option<String>,
    /// Pair-verify and FairPlay completed for the current (or last) session.
    pub paired: bool,
    pub fairplay: bool,
    /// True while a sender holds the mirror TCP connection.
    pub mirroring: bool,
    pub mirror_frames: u64,
    pub mirror_keyframes: u64,
    pub mirror_bytes: u64,
    pub mirror_width: u32,
    pub mirror_height: u32,
    ///Milliseconds the mirrored picture trails reality, from the sender's own
    /// capture stamp: average and worst since the session started.
    pub mirror_latency_ms: f64,
    pub mirror_latency_max_ms: f64,
    /// Same delay measured against the session's first frame, which cancels any
    /// clock offset between the two machines. This is the number that shows a
    /// queue growing: it should stay flat, and it climbs when we fall behind.
    pub mirror_drift_ms: f64,
    pub mirror_drift_max_ms: f64,
    /// Worst gap between two arriving frames — a stutter measured in the
    /// stream itself, independent of how the preview draws it.
    pub mirror_gap_max_ms: f64,
    /// Average gap between two arriving frames, i.e. the sender's frame period.
    pub mirror_gap_avg_ms: f64,
    /// Microseconds this process spends per frame between arrival and hand-off.
    pub mirror_pipeline_ms: f64,
    /// Frames the forwarder threw away because the preview stopped draining.
    /// Every one of them is a hole in the reference chain.
    pub mirror_drops: u64,
    /// The preview's own numbers, absent until a preview has reported.
    pub preview: Option<MirrorPerf>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DlnaStatus {
    pub name: String,
    pub location: String,
    pub port: u16,
    /// `idle`, `loading`, `playing`, `paused`, `stopped`, `ended` or `error`.
    pub phase: String,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub uri: Option<String>,
    /// Empty unless SSDP could not bind, or the player reported a failure.
    pub error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatus {
    pub name: String,
    pub listening: bool,
    pub port: u16,
    /// `192.168.0.52:9407`, for operators who only need the host.
    pub address: String,
    /// The six digits shown on the big screen as the 投屏码; the extension's
    /// `?token=` must match it.
    pub code: String,
    /// The full address to paste into the HORIZON extension.
    pub url: String,
    pub connected: bool,
    pub peer: Option<String>,
    pub frames: u64,
    pub keyframes: u64,
    pub video_bytes: u64,
    pub audio_bytes: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverStatus {
    pub airplay: Option<AirplayStatus>,
    pub dlna: Option<DlnaStatus>,
    pub browser: Option<BrowserStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub id: String,
    pub name: String,
    /// `md` from a Cast device's TXT record, when it published one.
    pub model: Option<String>,
    pub protocol: String,
    pub address: Option<String>,
    pub location: Option<String>,
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn airplay_start(
    receivers: tauri::State<'_, Receivers>,
    name: String,
) -> Result<AirplayStatus, String> {
    receivers.start_airplay(&name)
}

#[tauri::command]
pub async fn airplay_stop(receivers: tauri::State<'_, Receivers>) -> Result<(), String> {
    receivers.stop_airplay()
}

/// Attaches the drawer's WebCodecs preview to the running receiver. The channel
/// outlives the command: frames are pushed from the mirror thread until
/// `airplay_preview_stop` or the receiver stops.
#[tauri::command]
pub async fn airplay_preview(
    receivers: tauri::State<'_, Receivers>,
    owner: String,
    channel: Channel<IpcResponse>,
) -> Result<(), String> {
    receivers.attach_preview(owner, channel)
}

#[tauri::command]
pub async fn airplay_preview_stop(
    receivers: tauri::State<'_, Receivers>,
    owner: String,
) -> Result<(), String> {
    receivers.detach_preview(&owner);
    Ok(())
}

#[tauri::command]
pub async fn dlna_start(
    app: tauri::AppHandle,
    receivers: tauri::State<'_, Receivers>,
    name: String,
) -> Result<DlnaStatus, String> {
    // The player belongs to the stage, not to this command: the renderer is handed
    // the queue as a `Transport` so a controller and the big screen drive one
    // surface instead of two decoders fighting over one window.
    let runtime = crate::media_runtime(&app);
    receivers.start_dlna(&name, runtime.stage.drive())
}

#[tauri::command]
pub async fn dlna_stop(receivers: tauri::State<'_, Receivers>) -> Result<(), String> {
    receivers.stop_dlna()
}

#[tauri::command]
pub async fn browser_start(
    app: tauri::AppHandle,
    receivers: tauri::State<'_, Receivers>,
    name: String,
) -> Result<BrowserStatus, String> {
    // The hub's port comes from the address the hub itself reported when it bound,
    // so a sender learns the real number instead of guessing one.
    let base = crate::media_runtime(&app).stage.base();
    receivers.start_browser(&name, browser_token(&app), hub_port(&base))
}

/// The port inside the hub's own base address, or `None` when the hub never
/// bound (`base` is empty) or named no port.
fn hub_port(base: &str) -> Option<u16> {
    let (_, rest) = base.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    authority.rsplit(':').next()?.parse().ok()
}

#[tauri::command]
pub async fn browser_stop(receivers: tauri::State<'_, Receivers>) -> Result<(), String> {
    receivers.stop_browser()
}

#[tauri::command]
pub async fn browser_preview(
    receivers: tauri::State<'_, Receivers>,
    owner: String,
    channel: Channel<IpcResponse>,
) -> Result<(), String> {
    receivers.attach_browser_preview(owner, channel)
}

#[tauri::command]
pub async fn browser_preview_stop(
    receivers: tauri::State<'_, Receivers>,
    owner: String,
) -> Result<(), String> {
    receivers.detach_browser_preview(&owner);
    Ok(())
}

#[tauri::command]
pub async fn browser_keyframe(receivers: tauri::State<'_, Receivers>) -> Result<(), String> {
    receivers.request_browser_keyframe()
}

#[tauri::command]
pub async fn receiver_status(
    receivers: tauri::State<'_, Receivers>,
) -> Result<ReceiverStatus, String> {
    Ok(ReceiverStatus {
        airplay: receivers.airplay_status(),
        dlna: receivers.dlna_status(),
        browser: receivers.browser_status(),
    })
}

/// Browse the LAN once and return what answered. The browse is intentionally
/// short: the panel is a snapshot, not a live monitor, and holding the two
/// multicast sockets longer would only duplicate what the receivers already do.
#[tauri::command]
pub async fn discover_peers(seconds: Option<f64>) -> Result<Vec<PeerView>, String> {
    let seconds = seconds.unwrap_or(3.0).clamp(1.0, 8.0);
    let mut discovery = Discovery::new().map_err(|error| format!("无法开始发现：{error}"))?;
    discovery
        .poll(Duration::from_secs_f64(seconds))
        .map_err(|error| format!("发现过程失败：{error}"))?;
    let mut peers: Vec<PeerView> = discovery
        .peers()
        .into_iter()
        .map(|peer| {
            // A Cast device's mDNS instance is a UUID; `fn` is the name humans
            // use. SSDP peers have no TXT and keep their USN label.
            let name = peer.txt_value("fn").unwrap_or(&peer.name).to_string();
            let model = peer.txt_value("md").map(str::to_string);
            PeerView {
                id: peer.id,
                name,
                model,
                protocol: protocol_label(peer.protocol).to_string(),
                address: peer.addr.map(|addr| addr.to_string()),
                location: peer.location,
            }
        })
        .collect();
    peers.sort_by(|a, b| a.protocol.cmp(&b.protocol).then(a.name.cmp(&b.name)));
    Ok(peers)
}

fn protocol_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Horizon => "HORIZON",
        Protocol::GoogleCast => "Cast",
        Protocol::AirPlay => "AirPlay",
        Protocol::Dlna => "DLNA",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_message_carries_the_header_the_webview_expects() {
        let frame = VideoFrame {
            data: vec![0, 0, 0, 1, 0x65, 9],
            pts_us: 77_250_000,
            width: 1920,
            height: 1080,
            keyframe: true,
            wallclock: false,
        };
        let body = frame_message(&frame);
        assert_eq!(body.len(), 24 + frame.data.len());
        assert_eq!(body[0], 1);
        assert_eq!(body[1], 1);
        assert_eq!(
            u64::from_le_bytes(body[4..12].try_into().unwrap()),
            frame.pts_us
        );
        assert_eq!(u16::from_le_bytes(body[12..14].try_into().unwrap()), 1920);
        assert_eq!(u16::from_le_bytes(body[14..16].try_into().unwrap()), 1080);
        assert_eq!(
            u32::from_le_bytes(body[16..20].try_into().unwrap()) as usize,
            frame.data.len()
        );
        assert_eq!(&body[24..], &frame.data);
    }

    #[test]
    fn a_sender_learns_the_hub_port_from_the_hubs_own_address() {
        assert_eq!(hub_port("http://192.168.0.27:7878"), Some(7878));
        assert_eq!(hub_port("http://192.168.0.27:7878/media/x"), Some(7878));
        assert_eq!(hub_port("http://[::1]:7878"), Some(7878));
        // A hub that never bound, or bound without naming a port, reports nothing
        // rather than a number a sender would then probe and blame.
        assert_eq!(hub_port(""), None);
        assert_eq!(hub_port("http://192.168.0.27"), None);
        assert_eq!(hub_port("192.168.0.27:7878"), None);
    }
}
