//! MediaEngine playback, shared by every receiver protocol.
//!
//! The DLNA renderer needs to play an arbitrary HTTP URL the way a television
//! does: MP4, MKV, MP3, WAV, HLS — whatever the controller points at. Writing a
//! demuxer for each is the wrong project. `IMFMediaEngine` is the same engine
//! behind Edge's `<video>` element: it owns the source resolver, the decoders and
//! the audio endpoint, and it renders video into a window handle we give it.
//!
//! ## Threading
//!
//! MediaEngine is a browser component: it is created, driven and destroyed on one
//! UI thread with a message pump, and it is *not* agile. So the engine lives on
//! its own thread and everything else talks to it through two things:
//!
//! - a command channel ([`Transport`] methods), and
//! - a shared [`PlaybackState`] snapshot, written by the player thread on a
//!   100 ms timer and read by the HTTP control plane.
//!
//! Every command is followed by a posted `WM_APP` so a `Play` does not wait for
//! the next timer tick; the timer exists for position and state sampling.
//!
//! ## What this deliberately does not do
//!
//! No MediaEngine event callbacks. They would arrive on the player thread anyway
//! and the control plane only needs a polled snapshot; one polling path is easier
//! to reason about than two, and the DLNA actions that care (position, state)
//! are pull-based.
//!
//! The window is created hidden and shown on the first load when `show_video` is
//! true. Audio-only playback therefore costs no window, and closing the video
//! window hides it instead of ending the session — a controller keeps owning the
//! transport until it says Stop.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{BSTR, HSTRING, IUnknown, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RPC_E_CHANGED_MODE, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory, IMFMediaEngineNotify,
    IMFMediaEngineNotify_Impl, MFCreateAttributes, CLSID_MFMediaEngineClassFactory,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_PLAYBACK_HWND,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    LoadCursorW, PostMessageW, RegisterClassW, SetTimer, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, MSG, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

use crate::codec;
use crate::error::{CastError, CastResult};

const PLAYER_CLASS: &str = "HorizonCastPlayer";
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No source has been set.
    Idle,
    /// A source has been set and is not playing yet.
    Loading,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}

/// The snapshot the control plane answers SOAP from.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackState {
    pub phase: Phase,
    pub uri: Option<String>,
    /// Seconds. Zero when the source has no known duration.
    pub position: f64,
    pub duration: f64,
    /// 0.0–1.0, as MediaEngine takes it.
    pub volume: f64,
    pub muted: bool,
    pub error: Option<String>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            uri: None,
            position: 0.0,
            duration: 0.0,
            volume: 0.5,
            muted: false,
            error: None,
        }
    }
}

/// What every receiver protocol can ask its player to do.
///
/// A trait rather than a concrete type so the DLNA control plane can be driven
/// in tests by a recorder, without a codec, a sound card or a window.
pub trait Transport: Send + Sync {
    fn load(&self, uri: &str) -> CastResult<()>;
    fn play(&self) -> CastResult<()>;
    fn pause(&self) -> CastResult<()>;
    fn stop(&self) -> CastResult<()>;
    fn seek(&self, seconds: f64) -> CastResult<()>;
    fn set_volume(&self, volume: f64) -> CastResult<()>;
    fn set_mute(&self, muted: bool) -> CastResult<()>;
    fn state(&self) -> PlaybackState;
}

enum Command {
    Load(String),
    Play,
    Pause,
    Stop,
    Seek(f64),
    Volume(f64),
    Mute(bool),
    Quit,
}

/// `CreateInstance` refuses an attribute store without a callback, even though
/// this player learns everything it needs by polling. The events are consumed
/// and discarded; implementing the interface is the price of admission.
#[windows::core::implement(IMFMediaEngineNotify)]
struct Notify;

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> windows::core::Result<()> {
        Ok(())
    }
}

/// MediaEngine-backed [`Transport`].
pub struct MediaEngineTransport {
    tx: Sender<Command>,
    /// `HWND` is a raw pointer and not `Send`; the window outlives this struct on
    /// the player thread, and posting to a dead window only fails.
    hwnd: isize,
    state: Arc<Mutex<PlaybackState>>,
    thread: Option<JoinHandle<()>>,
}

impl MediaEngineTransport {
    /// Start the player thread and wait until Media Foundation, the window and the
    /// engine exist. `show_video` only decides whether the window is shown when a
    /// source is loaded; audio plays either way.
    pub fn start(show_video: bool) -> CastResult<Self> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let state = Arc::new(Mutex::new(PlaybackState::default()));
        let thread_state = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("horizon-player".into())
            .spawn(move || player_thread(show_video, rx, thread_state, ready_tx))
            .map_err(|_| CastError::Io)?;
        let hwnd = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(hwnd)) => hwnd,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(CastError::Timeout),
        };
        Ok(Self {
            tx,
            hwnd,
            state,
            thread: Some(thread),
        })
    }

    fn send(&self, command: Command) -> CastResult<()> {
        self.tx
            .send(command)
            .map_err(|_| CastError::InvalidState("player thread is gone"))?;
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_APP,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .map_err(|_| CastError::InvalidState("player window is gone"))
    }
}

impl Transport for MediaEngineTransport {
    fn load(&self, uri: &str) -> CastResult<()> {
        self.send(Command::Load(uri.to_string()))
    }

    fn play(&self) -> CastResult<()> {
        self.send(Command::Play)
    }

    fn pause(&self) -> CastResult<()> {
        self.send(Command::Pause)
    }

    fn stop(&self) -> CastResult<()> {
        self.send(Command::Stop)
    }

    fn seek(&self, seconds: f64) -> CastResult<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(CastError::Unsupported("seek outside the timeline"));
        }
        self.send(Command::Seek(seconds))
    }

    fn set_volume(&self, volume: f64) -> CastResult<()> {
        if !volume.is_finite() {
            return Err(CastError::Unsupported("volume is not a number"));
        }
        self.send(Command::Volume(volume.clamp(0.0, 1.0)))
    }

    fn set_mute(&self, muted: bool) -> CastResult<()> {
        self.send(Command::Mute(muted))
    }

    fn state(&self) -> PlaybackState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for MediaEngineTransport {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_APP,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn player_thread(
    show_video: bool,
    rx: Receiver<Command>,
    state: Arc<Mutex<PlaybackState>>,
    ready: Sender<CastResult<isize>>,
) {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            let _ = ready.send(Err(CastError::from(hr)));
            return;
        }
        // MFStartup is process-wide and idempotent; MediaEngine still needs this
        // thread's COM apartment, which is what CoInitializeEx above established.
        if let Err(e) = codec::startup() {
            let _ = ready.send(Err(e));
            if hr.is_ok() {
                CoUninitialize();
            }
            return;
        }
        if let Err(e) = pump(show_video, rx, state, &ready) {
            let _ = ready.send(Err(e));
        }
        if hr.is_ok() {
            CoUninitialize();
        }
    }
}

/// Why the phase the engine reports is not trusted on its own: `IsPaused` is true
/// both before Play and after Stop, and "loaded, never played" must read STOPPED
/// to a DLNA controller, not PAUSED_PLAYBACK.
enum Intent {
    Idle,
    Loaded,
    Playing,
    Paused,
    Stopped,
}

fn pump(
    show_video: bool,
    rx: Receiver<Command>,
    shared: Arc<Mutex<PlaybackState>>,
    ready: &Sender<CastResult<isize>>,
) -> CastResult<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let instance: windows::Win32::Foundation::HINSTANCE = hinstance.into();
        let class = HSTRING::from(PLAYER_CLASS);
        let wndclass = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        // Zero means "failed or already registered". Both are survivable: the
        // class registration is process-wide and a second player reuses it.
        RegisterClassW(&wndclass);

        let title = HSTRING::from("HORIZON Cast Receiver");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            &class,
            &title,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            960,
            540,
            None,
            None,
            Some(instance),
            None,
        )?;

        let mut attrs: Option<IMFAttributes> = None;
        if let Err(e) = MFCreateAttributes(&mut attrs, 2) {
            let _ = DestroyWindow(hwnd);
            return Err(e.into());
        }
        let Some(attrs) = attrs else {
            let _ = DestroyWindow(hwnd);
            return Err(CastError::InvalidState("MF attributes unavailable"));
        };
        if let Err(e) = attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, hwnd.0 as u64) {
            let _ = DestroyWindow(hwnd);
            return Err(e.into());
        }
        let notify: IMFMediaEngineNotify =
            windows::core::ComObject::new(Notify).into_interface();
        if let Err(e) = attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify) {
            let _ = DestroyWindow(hwnd);
            return Err(e.into());
        }
        let factory: IMFMediaEngineClassFactory =
            match CoCreateInstance::<Option<&IUnknown>, _>(
                &CLSID_MFMediaEngineClassFactory,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                Ok(factory) => factory,
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    return Err(e.into());
                }
            };
        let engine: IMFMediaEngine = match factory.CreateInstance(0, &attrs) {
            Ok(engine) => engine,
            Err(e) => {
                let _ = DestroyWindow(hwnd);
                return Err(e.into());
            }
        };

        SetTimer(Some(hwnd), TIMER_ID, TIMER_MS, None);
        let _ = ready.send(Ok(hwnd.0 as isize));

        let mut intent = Intent::Idle;
        let mut uri: Option<String> = None;
        let mut position = 0.0f64;
        let mut duration = 0.0f64;
        let mut volume = 0.5f64;
        let mut muted = false;
        let mut problem: Option<String> = None;
        let mut quit = false;

        let mut msg = MSG::default();
        while !quit {
            let got = GetMessageW(&mut msg, None, 0, 0);
            if got.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if msg.message != WM_APP && msg.message != WM_TIMER {
                continue;
            }

            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::Load(source) => {
                        problem = None;
                        position = 0.0;
                        duration = 0.0;
                        if let Err(e) = engine.SetSource(&BSTR::from(source.as_str())) {
                            problem = Some(format!("set source: {e}"));
                            intent = Intent::Idle;
                        } else {
                            // Do not autoplay: SetSource starts the source resolver
                            // and, for some media, playback; a DMR must wait for
                            // the controller's Play.
                            let _ = engine.SetVolume(volume);
                            let _ = engine.SetMuted(muted);
                            let _ = engine.Pause();
                            uri = Some(source);
                            intent = Intent::Loaded;
                            if show_video {
                                let _ = ShowWindow(hwnd, SW_SHOW);
                            }
                        }
                    }
                    Command::Play => {
                        if let Err(e) = engine.Play() {
                            problem = Some(format!("play: {e}"));
                        } else {
                            intent = Intent::Playing;
                        }
                    }
                    Command::Pause => {
                        if let Err(e) = engine.Pause() {
                            problem = Some(format!("pause: {e}"));
                        } else {
                            intent = Intent::Paused;
                        }
                    }
                    Command::Stop => {
                        let _ = engine.Pause();
                        let _ = engine.SetCurrentTime(0.0);
                        position = 0.0;
                        intent = Intent::Stopped;
                    }
                    Command::Seek(seconds) => {
                        if let Err(e) = engine.SetCurrentTime(seconds) {
                            problem = Some(format!("seek: {e}"));
                        } else {
                            position = seconds;
                        }
                    }
                    Command::Volume(v) => {
                        volume = v;
                        let _ = engine.SetVolume(v);
                    }
                    Command::Mute(m) => {
                        muted = m;
                        let _ = engine.SetMuted(m);
                    }
                    Command::Quit => quit = true,
                }
            }
            if quit {
                break;
            }

            let ready_state = engine.GetReadyState();
            let ended = engine.IsEnded().as_bool();
            let reported = engine.GetError().ok().map(|error| error.GetErrorCode());
            if let Some(code) = reported.filter(|code| *code != 0) {
                if problem.is_none() {
                    problem = Some(format!("media engine error {code}"));
                }
            }
            let engine_position = engine.GetCurrentTime();
            if engine_position.is_finite() && engine_position >= 0.0 {
                position = engine_position;
            }
            let engine_duration = engine.GetDuration();
            if engine_duration.is_finite() && engine_duration >= 0.0 {
                duration = engine_duration;
            }

            let phase = if problem.is_some() {
                Phase::Error
            } else if ended && matches!(intent, Intent::Playing) {
                Phase::Ended
            } else {
                match intent {
                    Intent::Idle => Phase::Idle,
                    Intent::Loaded | Intent::Stopped => Phase::Stopped,
                    // A controller that pressed Play is TRANSITIONING until the
                    // engine has data; saying PLAYING during the buffer gap makes
                    // some controllers show a frozen first frame as live.
                    Intent::Playing if ready_state == 0 => Phase::Loading,
                    Intent::Playing => Phase::Playing,
                    Intent::Paused if ready_state == 0 => Phase::Loading,
                    Intent::Paused => Phase::Paused,
                }
            };

            *shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = PlaybackState {
                phase,
                uri: uri.clone(),
                position,
                duration,
                volume,
                muted,
                error: problem.clone(),
            };
        }

        let _ = KillTimer(Some(hwnd), TIMER_ID);
        let _ = DestroyWindow(hwnd);
        Ok(())
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // Closing the video window must not end the transport: a controller
            // still owns it and may be playing audio.
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_starts_idle_and_stops_cleanly() {
        let transport = MediaEngineTransport::start(false).expect("MediaEngine");
        assert_eq!(transport.state().phase, Phase::Idle);
        std::thread::sleep(Duration::from_millis(250));
        let state = transport.state();
        assert_eq!(state.phase, Phase::Idle);
        assert!(state.uri.is_none());
        drop(transport);
    }

    #[test]
    fn seek_rejects_values_outside_the_timeline() {
        let transport = MediaEngineTransport::start(false).expect("MediaEngine");
        assert!(transport.seek(-1.0).is_err());
        assert!(transport.seek(f64::NAN).is_err());
    }
}
