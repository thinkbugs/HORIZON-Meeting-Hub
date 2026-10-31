//! The stage: one library, one queue, one picture surface.
//!
//! Everything the room projects — a file uploaded from the extension, a URL a
//! controller flung, the forty-first slide export — ends up here, and everything
//! that plays it goes through the same two objects. That is the point of the
//! module: the protocols are interchangeable inputs, and the player is not.
//!
//! ## Who owns the MediaEngine
//!
//! `horizon_cast::player::Transport` is the seam the DLNA control plane was
//! already written against, so the queue sits *underneath* the protocols instead
//! of beside them:
//!
//! ```text
//! DLNA controller ─┐
//! launcher UI     ─┼─▶ Playlist ─▶ MediaEngineTransport ─▶ IMFMediaEngine
//! extension /api  ─┘        (one surface, one decoder)
//! ```
//!
//! A `QueueDrive` is what a protocol is handed. It implements `Transport`, and
//! `load(uri)` there means "this one item, now" — a playlist of one, which is
//! exactly the semantics of `SetAVTransportURI`. A standard controller cannot
//! express a playlist, so it gets a playlist of one; the native API can express
//! forty, and they share the same surface and the same state. Two decoders per
//! room would not be a design, it would be two pictures fighting for one window.
//!
//! When that address is one this hub printed (`<base>/media/<id>/<name>`), the
//! drive looks the id up first, so the queue carries the library's title and kind
//! and not a guess read off a path that ends in a hash.
//!
//! ## Why the ids are the API
//!
//! Callers name library entries (`{"ids":["8f3c1a2b0c4d"]}`), never URLs. The
//! stage turns an id into a playable URI at the last moment using the hub's own
//! base address, so an entry can be re-downloaded, renamed or moved and every
//! client keeps working; and a client cannot smuggle an arbitrary path, because
//! the only URIs this produces are `<base>/media/<id>/<file>` and the http(s)
//! address a user deliberately stored as a remote item.

use std::sync::{Arc, Mutex};

use horizon_cast::error::CastResult;
use horizon_cast::player::{MediaEngineTransport, PlaybackState, Phase, Transport};
use horizon_cast::queue::{Order, Playlist, QueueSnapshot, Repeat, Track};
use horizon_cast::CastError;
use serde_json::{json, Value};

use crate::media_library::{Library, MediaItem};

/// The projection surface, the queue in front of it, and the library behind that.
pub struct MediaStage {
    library: Arc<Library>,
    playlist: Arc<Playlist>,
    /// `http://192.168.0.24:7878`, filled in once the hub has a listener. Queue
    /// items are rebuilt from the library on every request, so a base that
    /// changes (new Wi-Fi, new port) does not strand anything already stored.
    /// Shared with every drive, because a controller hands back an address built
    /// from it and only the base says whether that address is ours.
    base: Arc<Mutex<String>>,
}

impl MediaStage {
    /// The production stage: a real MediaEngine, its video surface parented to
    /// the launcher's window so the picture is part of the big screen, and the
    /// background monitor that advances the queue by itself.
    ///
    /// A decoder that cannot be created is reported, not fatal. The library, the
    /// file hub and this API have nothing to do with Media Foundation, and taking
    /// them down with the picture would turn one broken feature into three.
    pub fn open(video_parent: Option<isize>, library: Arc<Library>) -> Self {
        let transport: Arc<dyn Transport> =
            match MediaEngineTransport::start_with(true, video_parent) {
                Ok(engine) => Arc::new(engine),
                Err(error) => {
                    let reason = format!("无法启动播放器：{error}");
                    eprintln!("[media] {reason}");
                    Arc::new(Unavailable::new(reason))
                }
            };
        Self::with_transport(transport, library, true)
    }

    /// A stage over any transport, with the background monitor switchable. Tests
    /// drive the queue themselves; the product always runs the monitor, because
    /// auto-advance is the feature.
    pub fn with_transport(
        transport: Arc<dyn Transport>,
        library: Arc<Library>,
        monitor: bool,
    ) -> Self {
        let playlist = if monitor {
            Arc::new(Playlist::new(transport))
        } else {
            Arc::new(Playlist::detached(transport, 7))
        };
        Self {
            library,
            playlist,
            base: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn set_base(&self, base: &str) {
        let mut slot = self.lock_base();
        *slot = base.trim_end_matches('/').to_string();
    }

    /// The hub's own root address, empty until the hub has bound.
    pub fn base(&self) -> String {
        self.lock_base().clone()
    }

    fn lock_base(&self) -> std::sync::MutexGuard<'_, String> {
        self.base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The handle a receiver protocol is given: DLNA's SOAP layer, and nothing
    /// else, sees the queue as a plain `Transport`.
    pub fn drive(&self) -> Arc<dyn Transport> {
        Arc::new(QueueDrive {
            playlist: Arc::clone(&self.playlist),
            library: Arc::clone(&self.library),
            base: Arc::clone(&self.base),
        })
    }

    fn track_of(&self, id: &str) -> Result<Track, String> {
        let item = self
            .library
            .get(id)
            .ok_or_else(|| format!("片库里没有 {id}"))?;
        Ok(self.track_for(&item))
    }

    fn track_for(&self, item: &MediaItem) -> Track {
        let base = self.lock_base().clone();
        Track {
            id: item.id.clone(),
            uri: self.library.play_url(item, &base),
            title: item.title.clone(),
            // The library's own classification, never a guess from the address:
            // a hub file address ends in an id, and reading a photo as a video
            // would let the queue stall on it forever, waiting for an ending
            // that still images do not report.
            kind: item.kind.to_cast(),
        }
    }

    /// Resolve either an `id` or an `index` from a request body to the position of
    /// that item in the current queue. Both are accepted because the big screen
    /// works in slots and a phone works in ids, and neither should have to ask.
    fn position(&self, args: &Value) -> Result<usize, String> {
        let snapshot = self.playlist.snapshot();
        if let Some(index) = args.get("index").and_then(Value::as_u64) {
            let index = index as usize;
            if index >= snapshot.items.len() {
                return Err("队列里没有这一项".to_string());
            }
            return Ok(index);
        }
        let id = string_field(args, "id").ok_or_else(|| "缺少 id 或 index".to_string())?;
        snapshot
            .items
            .iter()
            .position(|track| track.id == id)
            .ok_or_else(|| "队列里没有这一项".to_string())
    }

    // ---- the API -------------------------------------------------------

    /// The one entry point, shared by the HTTP routes and the Tauri commands.
    ///
    /// A dispatcher rather than twenty methods because there are two front doors
    /// and they must not be able to disagree: the extension posts
    /// `/api/media/setQueue`, the launcher sends `media_invoke("setQueue")`, and
    /// both land on identical code, identical JSON, identical errors.
    pub fn invoke(&self, action: &str, args: &Value) -> Result<Value, String> {
        match action {
            "library" => Ok(self.library_json()),
            "queue" => Ok(self.queue_json()),
            "status" => Ok(self.status_json()),
            "addUrl" => {
                let url = string_field(args, "url").ok_or_else(|| "缺少地址".to_string())?;
                let title = string_field(args, "title").unwrap_or_default();
                let source = string_field(args, "source").unwrap_or_else(|| "url".to_string());
                let item = self.library.add_url(&url, &title, &source)?;
                self.notify_library();
                Ok(json!({ "item": item_json(&item) }))
            }
            "rename" => {
                let id = string_field(args, "id").ok_or_else(|| "缺少 id".to_string())?;
                let title = string_field(args, "title").unwrap_or_default();
                let item = self.library.rename(&id, &title)?;
                self.notify_library();
                Ok(json!({ "item": item_json(&item) }))
            }
            "remove" => {
                let id = string_field(args, "id").ok_or_else(|| "缺少 id".to_string())?;
                let removed = self.library.remove(&id)?;
                // A queue may still hold the deleted entry's URI; drop it so the
                // player is never left pointing at a file that no longer exists.
                if let Ok(index) = self.position(&json!({ "id": id })) {
                    let _ = self.playlist.remove(index);
                }
                self.notify_library();
                Ok(json!({
                    "removed": removed.existed,
                    "fileError": removed.file_error,
                    "queue": self.queue_json(),
                }))
            }
            "setQueue" => {
                let tracks = self.tracks(args)?;
                let start = args.get("start").and_then(Value::as_u64).map(|v| v as usize);
                self.playlist
                    .set_items(tracks, start)
                    .map_err(describe)?;
                if args
                    .get("autoplay")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                {
                    self.playlist.play().map_err(describe)?;
                }
                Ok(self.queue_json())
            }
            "enqueue" => {
                let tracks = self.tracks(args)?;
                let autoplay = args
                    .get("autoplay")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.playlist.enqueue(tracks, autoplay).map_err(describe)?;
                Ok(self.queue_json())
            }
            "clearQueue" => {
                self.playlist.clear().map_err(describe)?;
                Ok(self.queue_json())
            }
            "queueRemove" => {
                let index = self.position(args)?;
                self.playlist.remove(index).map_err(describe)?;
                Ok(self.queue_json())
            }
            "queueMove" => {
                let from = self.position(args)?;
                let to = args
                    .get("to")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "缺少目标位置".to_string())? as usize;
                self.playlist.reorder(from, to).map_err(describe)?;
                Ok(self.queue_json())
            }
            "setNext" => {
                let id = string_field(args, "id").ok_or_else(|| "缺少 id".to_string())?;
                self.playlist.set_next(self.track_of(&id)?);
                Ok(self.queue_json())
            }
            "jump" => {
                let index = self.position(args)?;
                self.playlist.jump(index).map_err(describe)?;
                Ok(self.queue_json())
            }
            "next" => {
                self.playlist.next().map_err(describe)?;
                Ok(self.queue_json())
            }
            "previous" => {
                self.playlist.previous().map_err(describe)?;
                Ok(self.queue_json())
            }
            "hasNext" => Ok(json!({ "hasNext": self.playlist.has_next() })),
            "order" => {
                let name = string_field(args, "order").unwrap_or_default();
                self.playlist.set_order(match name.as_str() {
                    "shuffle" => Order::Shuffle,
                    "" => return Err("缺少播放顺序".to_string()),
                    _ => Order::Sequential,
                });
                Ok(self.queue_json())
            }
            "repeat" => {
                let name = string_field(args, "repeat").unwrap_or_default();
                self.playlist.set_repeat(match name.as_str() {
                    "all" => Repeat::All,
                    "one" => Repeat::One,
                    "" => return Err("缺少循环方式".to_string()),
                    _ => Repeat::Off,
                });
                Ok(self.queue_json())
            }
            "dwell" => {
                let seconds = number_field(args, "seconds").ok_or_else(|| "缺少停留时间".to_string())?;
                self.playlist.set_image_dwell(seconds);
                Ok(self.queue_json())
            }
            "transport" => {
                let verb = string_field(args, "action").unwrap_or_default();
                let value = number_field(args, "value");
                match verb.as_str() {
                    "play" => self.playlist.play(),
                    "pause" => self.playlist.pause(),
                    "stop" => self.playlist.stop(),
                    "seek" => self.playlist.seek(number_field(args, "at").unwrap_or_default()),
                    "volume" => self.playlist.set_volume(value.unwrap_or_default().clamp(0.0, 1.0)),
                    "mute" => self.playlist.set_mute(
                        args.get("value").and_then(Value::as_bool).unwrap_or_default(),
                    ),
                    other => return Err(format!("未知操作：{other}")),
                }
                .map_err(describe)?;
                Ok(self.status_json())
            }
            "present" => {
                let rect = match args.get("rect") {
                    Some(value) if value.is_null() => None,
                    Some(value) => Some(rect_of(value).ok_or_else(|| "画面矩形无效".to_string())?),
                    None => None,
                };
                self.playlist.present(rect).map_err(describe)?;
                Ok(self.status_json())
            }
            other => Err(format!("未知操作：{other}")),
        }
    }

    fn tracks(&self, args: &Value) -> Result<Vec<Track>, String> {
        let ids = args
            .get("ids")
            .and_then(Value::as_array)
            .ok_or_else(|| "缺少 ids".to_string())?;
        if ids.len() > 1000 {
            return Err("一次最多排入 1000 项".to_string());
        }
        let mut tracks = Vec::with_capacity(ids.len());
        for id in ids {
            let id = id.as_str().ok_or_else(|| "id 必须是字符串".to_string())?;
            tracks.push(self.track_of(id)?);
        }
        Ok(tracks)
    }

    pub(crate) fn library_json(&self) -> Value {
        let items: Vec<Value> = self.library.items().iter().map(item_json).collect();
        json!({
            "items": items,
            "root": self.library.root().to_string_lossy(),
            "base": self.lock_base().clone(),
        })
    }

    fn queue_json(&self) -> Value {
        queue_json(&self.playlist.snapshot(), &self.lock_base())
    }

    fn status_json(&self) -> Value {
        let snapshot = self.playlist.snapshot();
        let base = self.lock_base().clone();
        json!({
            "state": state_json(&snapshot.state),
            "playing": snapshot.playing,
            "current": snapshot
                .current
                .and_then(|index| snapshot.items.get(index))
                .map(|track| track_json(track, &base)),
            "queue": queue_json(&snapshot, &base),
        })
    }

    /// Lets the launcher redraw as soon as the library changes.
    fn notify_library(&self) {
        let listing = self.library_json();
        crate::snapdrop_server::emit_to_ui("media-library", listing);
    }
}

/// The library and the stage that plays it, created once and shared by the HTTP
/// hub, the Tauri commands and the DLNA renderer. They are one object because
/// splitting them lets a caller hold a library the player is not using.
pub struct MediaRuntime {
    pub library: Arc<Library>,
    pub stage: MediaStage,
}

impl MediaRuntime {
    /// The library at its default root, the stage over a real player. Infallible
    /// by design: this is created during startup and everything downstream —
    /// file transfer, the queue API, a controller's cast — reads one status page,
    /// so a decoder that refused to start has to be a reportable state rather
    /// than a missing object. See [`MediaStage::open`].
    pub fn start(video_parent: Option<isize>) -> Self {
        let library = Arc::new(Library::open(Library::default_root()));
        Self {
            stage: MediaStage::open(video_parent, Arc::clone(&library)),
            library,
        }
    }

    /// A runtime whose player is any transport, without the background monitor
    /// unless the caller wants it. The hub's HTTP tests use a player that answers
    /// `ok` and holds no decoder; the product always calls [`MediaStage::open`].
    #[cfg(test)]
    pub fn with_transport(
        transport: Arc<dyn Transport>,
        library: Arc<Library>,
        monitor: bool,
    ) -> Self {
        Self {
            stage: MediaStage::with_transport(transport, Arc::clone(&library), monitor),
            library,
        }
    }
}

/// What a receiver protocol holds instead of the player. See the module header.
#[derive(Clone)]
pub struct QueueDrive {
    playlist: Arc<Playlist>,
    library: Arc<Library>,
    base: Arc<Mutex<String>>,
}

impl QueueDrive {
    /// A controller hands over an address and nothing else. When that address is
    /// one this hub printed, the library knows far more than the URL spells out —
    /// the real title, and whether the item is a photo or a song — so the queue
    /// keeps the library's answer. An address from elsewhere keeps the URI's own
    /// tail, which is all anyone knows about it.
    fn track_for(&self, uri: &str) -> Track {
        match self.library_id(uri).and_then(|id| self.library.get(id)) {
            Some(item) => Track {
                id: item.id.clone(),
                uri: uri.to_string(),
                title: item.title.clone(),
                kind: item.kind.to_cast(),
            },
            None => Track::from_uri(uri),
        }
    }

    /// The library id inside a `<base>/media/<id>` address, if this is one. The
    /// base has to match: a phone casting from its own server with a `/media/`
    /// path is not an item of ours.
    fn library_id<'a>(&self, uri: &'a str) -> Option<&'a str> {
        let base = self
            .base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if base.is_empty() {
            return None;
        }
        let rest = uri.strip_prefix(&format!("{}/media/", base.trim_end_matches('/')))?;
        let id = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    }
}

impl Transport for QueueDrive {
    fn load(&self, uri: &str) -> CastResult<()> {
        self.playlist
            .set_items(vec![self.track_for(uri)], None)
    }
    fn play(&self) -> CastResult<()> {
        self.playlist.play()
    }
    fn pause(&self) -> CastResult<()> {
        self.playlist.pause()
    }
    fn stop(&self) -> CastResult<()> {
        self.playlist.stop()
    }
    fn seek(&self, seconds: f64) -> CastResult<()> {
        self.playlist.seek(seconds)
    }
    fn set_volume(&self, volume: f64) -> CastResult<()> {
        self.playlist.set_volume(volume)
    }
    fn set_mute(&self, muted: bool) -> CastResult<()> {
        self.playlist.set_mute(muted)
    }
    fn state(&self) -> PlaybackState {
        self.playlist.state()
    }
    fn set_next(&self, uri: &str) -> CastResult<()> {
        self.playlist.set_next(self.track_for(uri));
        Ok(())
    }
    fn next(&self) -> CastResult<()> {
        self.playlist.next()
    }
    fn previous(&self) -> CastResult<()> {
        self.playlist.previous()
    }
    fn has_next(&self) -> bool {
        self.playlist.has_next()
    }
    fn current(&self) -> Option<horizon_cast::queue::Track> {
        self.playlist.current()
    }
    fn present(&self, rect: Option<(i32, i32, i32, i32)>) -> CastResult<()> {
        self.playlist.present(rect)
    }
}

/// A player that could not be built, and says so in every language the UI reads.
///
/// `state()` keeps reporting [`Phase::Error`] with the original reason so the big
/// screen can show *why* there is no picture instead of showing a black rectangle,
/// and `load` refuses the same way. Everything around it — the library, uploads,
/// the queue's own order and repeat modes — is unaffected, which is the point: a
/// decoder failure should cost the picture, not the product.
struct Unavailable {
    reason: String,
    state: Mutex<PlaybackState>,
}

impl Unavailable {
    fn new(reason: String) -> Self {
        Self {
            state: Mutex::new(PlaybackState {
                phase: Phase::Error,
                volume: 1.0,
                error: Some(reason.clone()),
                ..Default::default()
            }),
            reason,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PlaybackState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Transport for Unavailable {
    fn load(&self, uri: &str) -> CastResult<()> {
        *self.lock() = PlaybackState {
            phase: Phase::Error,
            uri: Some(uri.to_string()),
            volume: 1.0,
            error: Some(self.reason.clone()),
            ..Default::default()
        };
        Err(CastError::Unsupported("播放器不可用"))
    }
    fn play(&self) -> CastResult<()> {
        Err(CastError::Unsupported("播放器不可用"))
    }
    fn pause(&self) -> CastResult<()> {
        Ok(())
    }
    fn stop(&self) -> CastResult<()> {
        Ok(())
    }
    fn seek(&self, _seconds: f64) -> CastResult<()> {
        Err(CastError::Unsupported("播放器不可用"))
    }
    fn set_volume(&self, volume: f64) -> CastResult<()> {
        self.lock().volume = volume;
        Ok(())
    }
    fn set_mute(&self, muted: bool) -> CastResult<()> {
        self.lock().muted = muted;
        Ok(())
    }
    fn state(&self) -> PlaybackState {
        self.lock().clone()
    }
}

fn string_field(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn number_field(args: &Value, key: &str) -> Option<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn rect_of(value: &Value) -> Option<(i32, i32, i32, i32)> {
    let values = value.as_array()?;
    if values.len() != 4 {
        return None;
    }
    let numbers: Vec<i32> = values
        .iter()
        .filter_map(|entry| entry.as_f64().filter(|number| number.is_finite()).map(|n| n.round() as i32))
        .collect();
    if numbers.len() != 4 {
        return None;
    }
    Some((numbers[0], numbers[1], numbers[2].max(0), numbers[3].max(0)))
}

fn item_json(item: &MediaItem) -> Value {
    json!({
        "id": item.id,
        "title": item.title,
        "kind": item.kind.as_str(),
        "file": item.file,
        "url": item.url,
        "bytes": item.bytes,
        "addedAt": item.added_at,
        "source": item.source,
    })
}

fn track_json(track: &Track, base: &str) -> Value {
    json!({
        "id": track.id,
        "uri": track.uri,
        "title": track.title,
        "kind": track.kind.as_str(),
        "local": is_ours(track, base),
    })
}

/// Whether the hub itself serves these bytes.
///
/// The alternative — `uri.contains("/media/")` — reads an address shape instead
/// of an origin, so any sender whose path happens to contain `media` gets
/// labelled as one of ours. That label is the big screen's promise that the
/// item keeps playing after the phone leaves the room, so it has to be the
/// hub's own prefix, which is the only thing that can actually be served.
fn is_ours(track: &Track, base: &str) -> bool {
    if base.is_empty() {
        return false;
    }
    let prefix = format!("{}/media/", base.trim_end_matches('/'));
    track.uri.starts_with(&prefix)
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

fn state_json(state: &PlaybackState) -> Value {
    json!({
        "phase": phase_label(state.phase),
        "uri": state.uri,
        "position": state.position,
        "duration": state.duration,
        "volume": state.volume,
        "muted": state.muted,
        "error": state.error,
    })
}

fn queue_json(snapshot: &QueueSnapshot, base: &str) -> Value {
    json!({
        "items": snapshot
            .items
            .iter()
            .map(|track| track_json(track, base))
            .collect::<Vec<_>>(),
        "current": snapshot.current,
        "order": match snapshot.order {
            Order::Sequential => "sequential",
            Order::Shuffle => "shuffle",
        },
        "repeat": match snapshot.repeat {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        },
        "imageDwell": snapshot.image_dwell,
        "playing": snapshot.playing,
        "halted": snapshot.halted,
        "state": state_json(&snapshot.state),
    })
}

fn describe(error: CastError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    /// A recorder, not a fake: it counts what the queue asked for and reports the
    /// phase the test tells it to, so auto-advance and errors are testable without
    /// a codec or a window.
    struct Scripted {
        calls: StdMutex<Vec<String>>,
        state: StdMutex<PlaybackState>,
    }

    impl Scripted {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: StdMutex::new(Vec::new()),
                state: StdMutex::new(PlaybackState {
                    phase: Phase::Playing,
                    ..Default::default()
                }),
            })
        }

        fn record(&self, call: &str) {
            self.calls.lock().unwrap().push(call.to_string());
        }

        fn set_phase(&self, phase: Phase) {
            self.state.lock().unwrap().phase = phase;
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Transport for Scripted {
        fn load(&self, uri: &str) -> CastResult<()> {
            self.record(&format!("load {uri}"));
            self.set_phase(Phase::Playing);
            Ok(())
        }
        fn play(&self) -> CastResult<()> {
            self.record("play");
            self.set_phase(Phase::Playing);
            Ok(())
        }
        fn pause(&self) -> CastResult<()> {
            self.record("pause");
            self.set_phase(Phase::Paused);
            Ok(())
        }
        fn stop(&self) -> CastResult<()> {
            self.record("stop");
            self.set_phase(Phase::Stopped);
            Ok(())
        }
        fn seek(&self, seconds: f64) -> CastResult<()> {
            self.record(&format!("seek {seconds}"));
            Ok(())
        }
        fn set_volume(&self, volume: f64) -> CastResult<()> {
            self.record(&format!("volume {volume}"));
            self.state.lock().unwrap().volume = volume;
            Ok(())
        }
        fn set_mute(&self, muted: bool) -> CastResult<()> {
            self.record(&format!("mute {muted}"));
            self.state.lock().unwrap().muted = muted;
            Ok(())
        }
        fn state(&self) -> PlaybackState {
            self.state.lock().unwrap().clone()
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "horizon-stage-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stage(label: &str) -> (TempDir, MediaStage) {
        let dir = TempDir::new(label);
        let library = Arc::new(Library::open(dir.0.clone()));
        let stage = MediaStage::with_transport(Scripted::new(), Arc::clone(&library), false);
        stage.set_base("http://192.168.0.24:7878");
        (dir, stage)
    }

    /// A file in the folder plus the entry that names it, the way an upload leaves
    /// both behind.
    fn put(stage: &MediaStage, name: &str) -> String {
        let library = &stage.library;
        let (stored, path) = library.stage(name);
        std::fs::write(&path, b"payload").expect("write");
        library
            .add_file(stored, name, 7, "test")
            .id
    }

    #[test]
    fn a_controllers_step_and_staged_next_move_the_same_queue_the_screen_reads() {
        let (_dir, stage) = stage("next");
        let drive = stage.drive();
        let first = put(&stage, "第一.mp4");
        let second = put(&stage, "第二.mp4");
        stage
            .invoke("setQueue", &json!({ "ids": [first, second], "autoplay": false }))
            .expect("setQueue");

        assert!(drive.has_next(), "两条队列从第一条往后还有得走");
        let current = drive.current().expect("current");
        assert_eq!(current.id, first);
        assert_eq!(
            current.title, "第一.mp4",
            "控制器读到的是片库里的标题，不是地址尾巴"
        );

        drive
            .set_next("http://192.168.0.9/media/控制器塞的.mp4")
            .expect("set_next");
        let queue = stage.invoke("queue", &json!({})).expect("queue");
        assert_eq!(
            queue["items"].as_array().unwrap().len(),
            2,
            "预排的一条在下一次走之前不进列表"
        );

        drive.next().expect("next");
        let staged = drive.current().expect("staged is current");
        assert_eq!(staged.uri, "http://192.168.0.9/media/控制器塞的.mp4");
        assert_eq!(staged.title, "控制器塞的.mp4");
        assert_eq!(
            stage
                .invoke("queue", &json!({}))
                .expect("queue")["items"]
                .as_array()
                .unwrap()
                .len(),
            3,
            "走到的那一条落成列表里的一项，大屏看得到"
        );

        assert!(!drive.has_next(), "队尾之后没有下一条");
        assert!(drive.next().is_ok(), "越过队尾不是错误，是停下");
        assert_eq!(
            stage.invoke("queue", &json!({})).expect("queue")["playing"],
            false
        );
    }

    #[test]
    fn an_address_of_ours_is_read_back_as_the_library_entry_it_points_at() {
        use horizon_cast::queue::MediaKind;
        let (_dir, stage) = stage("fling");
        let drive = stage.drive();
        let photo = put(&stage, "现场照片.jpg");
        let uri = format!("http://192.168.0.24:7878/media/{photo}");

        // The address itself says nothing about what is behind it, which is the
        // whole reason the id is looked up: read off the path alone, a still
        // photo becomes a video and the queue waits forever for an ending that
        // never comes.
        assert_eq!(
            Track::from_uri(&uri).kind,
            MediaKind::Video,
            "只看地址只能猜成视频——这正是必须回查片库的理由"
        );

        drive.load(&uri).expect("load");
        let current = drive.current().expect("current");
        assert_eq!(current.id, photo, "控制器投的是片库项，队列里就带着那条的 id");
        assert_eq!(current.title, "现场照片.jpg");
        assert_eq!(current.kind, MediaKind::Image);
        assert_eq!(
            stage.invoke("queue", &json!({})).expect("queue")["items"][0]["kind"],
            "image",
            "大屏读到的也是图片，而不是同一条地址的猜测"
        );

        // A `/media/` path that is not ours keeps the tail of the address: the
        // only thing anyone knows about it. Matching the base is what tells the
        // two apart, so a phone serving from its own box is never mistaken for
        // an entry of this hub.
        drive
            .load("http://10.0.0.5:9000/media/外部片段.mp4")
            .expect("load foreign");
        let foreign = drive.current().expect("foreign current");
        assert_eq!(foreign.title, "外部片段.mp4");
        assert_eq!(foreign.kind, MediaKind::Video);
        assert_ne!(foreign.id, photo);
    }

    #[test]
    fn the_api_names_library_entries_and_turns_them_into_addresses() {
        let (_dir, stage) = stage("ids");
        let photo = put(&stage, "照片.jpg");
        let film = put(&stage, "开幕.mp4");

        let listing = stage.invoke("library", &json!({})).expect("library");
        assert_eq!(listing["items"].as_array().unwrap().len(), 2);
        assert_eq!(listing["items"][0]["kind"], "image");
        assert_eq!(listing["base"], "http://192.168.0.24:7878");

        let queue = stage
            .invoke("setQueue", &json!({ "ids": [photo, film] }))
            .expect("setQueue");
        assert_eq!(queue["items"].as_array().unwrap().len(), 2);
        assert_eq!(
            queue["items"][0]["uri"],
            format!(
                "http://192.168.0.24:7878/media/{photo}/%E7%85%A7%E7%89%87.jpg",
                photo = photo
            ),
            "the stage, not the caller, decides how an entry is addressed — and the \
             address says what it is, because the player tells a photo from a film by \
             the end of it"
        );
        assert_eq!(queue["items"][1]["kind"], "video");
        assert_eq!(queue["current"], 0);
        assert!(queue["playing"].as_bool().unwrap(), "setQueue plays by default");
    }

    #[test]
    fn an_unknown_id_is_refused_before_it_reaches_the_player() {
        let (_dir, stage) = stage("unknown");
        let error = stage
            .invoke("setQueue", &json!({ "ids": ["nosuchid0000"] }))
            .expect_err("refused");
        assert!(error.contains("片库里没有"), "{error}");
    }

    #[test]
    fn a_remote_entry_keeps_its_own_address_and_is_marked_not_local() {
        let (_dir, stage) = stage("remote");
        let item = stage
            .invoke(
                "addUrl",
                &json!({ "url": "https://example.test/talk.mp4", "title": "讲座" }),
            )
            .expect("addUrl");
        let id = item["item"]["id"].as_str().unwrap().to_string();
        assert_eq!(item["item"]["kind"], "video");

        let queue = stage
            .invoke("setQueue", &json!({ "ids": [id], "autoplay": false }))
            .expect("setQueue");
        assert_eq!(queue["items"][0]["uri"], "https://example.test/talk.mp4");
        assert_eq!(
            queue["items"][0]["local"], false,
            "the UI needs to be able to say 这条依赖发送端保持可达"
        );

        let bad = stage
            .invoke("addUrl", &json!({ "url": "file:///C:/windows" }))
            .expect_err("refused");
        assert!(bad.contains("http"), "{bad}");
    }

    /// `local` is the big screen's promise that an item keeps playing after the
    /// sender walks away, so it is answered by the hub's own prefix rather than
    /// by a shape match: another machine's `/media/` path is not ours.
    #[test]
    fn only_an_address_under_our_own_hub_counts_as_local() {
        let (_dir, stage) = stage("locality");
        let photo = put(&stage, "现场照片.jpg");

        let borrowed = stage
            .invoke(
                "addUrl",
                &json!({ "url": "http://10.0.0.9:8000/media/别的机器.mp4", "title": "外站" }),
            )
            .expect("addUrl");
        let another = stage
            .invoke(
                "addUrl",
                &json!({ "url": format!("http://192.168.0.24:7878/media/{photo}"), "title": "本页地址" }),
            )
            .expect("addUrl");

        let queue = stage
            .invoke(
                "setQueue",
                &json!({
                    "ids": [photo, borrowed["item"]["id"].as_str().unwrap(), another["item"]["id"].as_str().unwrap()],
                    "autoplay": false,
                }),
            )
            .expect("setQueue");
        assert_eq!(queue["items"][0]["local"], true, "上传进片库的文件由主机自己出流");
        assert_eq!(
            queue["items"][1]["local"], false,
            "另一台机器的 /media/ 路径不是我们的"
        );
        assert_eq!(queue["items"][2]["local"], true, "指向本机 hub 的地址仍然是主机在出流");

        let status = stage.invoke("status", &Value::Null).expect("status");
        assert_eq!(
            status["queue"]["items"][1]["local"], false,
            "状态页读到的必须和队列页是同一个判断"
        );
    }

    #[test]
    fn queue_edits_by_id_and_by_slot_agree() {
        let (_dir, stage) = stage("edit");
        let a = put(&stage, "a.mp4");
        let b = put(&stage, "b.mp4");
        let c = put(&stage, "c.mp4");
        stage
            .invoke("setQueue", &json!({ "ids": [a, b, c], "autoplay": false }))
            .expect("set");

        // By id: move the last item to the front.
        let moved = stage
            .invoke("queueMove", &json!({ "id": c, "to": 0 }))
            .expect("move");
        assert_eq!(moved["items"][0]["id"], c);

        // By index: the list is now c, a, b, so slot 1 is a.
        let removed = stage
            .invoke("queueRemove", &json!({ "index": 1 }))
            .expect("remove");
        assert_eq!(removed["items"].as_array().unwrap().len(), 2);

        let jumped = stage.invoke("jump", &json!({ "id": b })).expect("jump");
        assert_eq!(jumped["current"], 1, "b is in slot 1 after the edits");

        stage.invoke("order", &json!({ "order": "shuffle" })).expect("order");
        stage.invoke("repeat", &json!({ "repeat": "one" })).expect("repeat");
        let modes = stage.invoke("queue", &json!({})).expect("queue");
        assert_eq!(modes["order"], "shuffle");
        assert_eq!(modes["repeat"], "one");

        let dwell = stage
            .invoke("dwell", &json!({ "seconds": 12.0 }))
            .expect("dwell");
        assert_eq!(dwell["imageDwell"], 12.0);
    }

    #[test]
    fn transport_verbs_reach_the_player_and_report_the_new_state() {
        let (_dir, stage) = stage("transport");
        let film = put(&stage, "开幕.mp4");
        stage
            .invoke("setQueue", &json!({ "ids": [film] }))
            .expect("set");

        let paused = stage
            .invoke("transport", &json!({ "action": "pause" }))
            .expect("pause");
        assert_eq!(paused["state"]["phase"], "paused");
        assert_eq!(paused["playing"], false);

        let resumed = stage
            .invoke("transport", &json!({ "action": "play" }))
            .expect("play");
        assert_eq!(resumed["state"]["phase"], "playing");

        stage
            .invoke("transport", &json!({ "action": "seek", "at": 42.0 }))
            .expect("seek");
        stage
            .invoke("transport", &json!({ "action": "volume", "value": 0.25 }))
            .expect("volume");
        let muted = stage
            .invoke("transport", &json!({ "action": "mute", "value": true }))
            .expect("mute");
        assert_eq!(muted["state"]["muted"], true);

        assert!(stage
            .invoke("transport", &json!({ "action": "flip" }))
            .expect_err("unknown verb")
            .contains("未知操作"));
    }

    #[test]
    fn deleting_an_entry_takes_it_out_of_the_queue_too() {
        let (_dir, stage) = stage("delete");
        let keep = put(&stage, "keep.mp4");
        let gone = put(&stage, "gone.mp4");
        stage
            .invoke("setQueue", &json!({ "ids": [keep, gone], "autoplay": false }))
            .expect("set");

        let after = stage.invoke("remove", &json!({ "id": gone })).expect("remove");
        assert_eq!(after["removed"], true);
        assert_eq!(after["queue"]["items"].as_array().unwrap().len(), 1);

        let listing = stage.invoke("library", &json!({})).expect("library");
        assert_eq!(listing["items"].as_array().unwrap().len(), 1);
        assert!(
            !stage
                .library
                .path_for_id(&gone)
                .map(|path| path.exists())
                .unwrap_or(false),
            "the file itself is gone with it"
        );
    }

    #[test]
    fn a_controller_loads_one_uri_and_the_stage_renders_it_as_a_one_item_queue() {
        let dir = TempDir::new("dlna");
        let scripted = Scripted::new();
        let library = Arc::new(Library::open(dir.0.clone()));
        let driven = MediaStage::with_transport(
            Arc::clone(&scripted) as Arc<dyn Transport>,
            library,
            false,
        );
        let drive = driven.drive();

        let uri = "http://192.168.0.9:8000/media/clip.mp4";
        drive.load(uri).expect("load");
        assert_eq!(
            scripted.calls(),
            vec![format!("load {uri}")],
            "SetAVTransportURI stages the item and stops there: the engine itself \
             refuses to autoplay a source, so the queue must not either"
        );
        drive.play().expect("play");
        assert_eq!(
            scripted.calls(),
            vec![format!("load {uri}"), "play".to_string()],
            "the controller's Play is what starts it"
        );
        drive.pause().expect("pause");
        assert_eq!(drive.state().phase, Phase::Paused);
        assert!(drive.present(Some((0, 0, 640, 360))).is_ok());

        let queue = driven.queue_json();
        assert_eq!(queue["items"].as_array().unwrap().len(), 1);
        assert_eq!(queue["items"][0]["title"], "clip.mp4");
        assert_eq!(
            queue["items"][0]["uri"], uri,
            "a flung address is kept verbatim, so a controller's own server stays \
             the origin and nothing is smuggled into the library"
        );
    }

    #[test]
    fn the_only_paths_in_this_api_are_the_ones_the_library_printed() {
        let (_dir, stage) = stage("paths");
        let film = put(&stage, "开幕.mp4");
        let queue = stage
            .invoke("setQueue", &json!({ "ids": [film], "autoplay": false }))
            .expect("set");
        let uri = queue["items"][0]["uri"].as_str().unwrap();
        assert!(uri.starts_with("http://192.168.0.24:7878/media/"));
        assert!(!uri.contains(".."), "{uri}");
        // `<base>/media/<id>/<name>`, and the name is percent-encoded so no
        // separator can hide inside it: the address stops at the file.
        let tail = uri
            .strip_prefix("http://192.168.0.24:7878/media/")
            .expect("our own prefix");
        let mut parts = tail.split('/');
        let id = parts.next().unwrap_or_default();
        assert!(!id.is_empty(), "{uri}");
        assert_eq!(parts.next(), Some("%E5%BC%80%E5%B9%95.mp4"), "{uri}");
        assert_eq!(parts.next(), None, "nothing after the file name: {uri}");

        for args in [
            json!({}),
            json!({ "id": "../..", "index": 99 }),
            json!({ "ids": "not-an-array" }),
            json!({ "ids": [""] }),
        ] {
            assert!(stage.invoke("setQueue", &args).is_err(), "{args}");
        }
        assert!(stage.invoke("nosuch", &json!({})).is_err());
    }
}
