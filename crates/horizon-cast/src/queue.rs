//! The queue that turns one transport into a playlist.
//!
//! A receiver that can only ever play one item is a receiver whose cast menu is
//! a replace button. Meeting rooms do not work that way: somebody brings five
//! photos and a two-minute clip and wants them in an order they control. The
//! protocol cannot say that — `SetAVTransportURI` carries one URI, and a sender
//! that sleeps takes its HTTP server with it — so the list lives here, on the
//! side that stays awake.
//!
//! ## Why it wraps [`Transport`] instead of replacing it
//!
//! [`crate::player`] owns one source and one state machine, and the DLNA control
//! plane talks only to the trait. This layer sits above it: it owns the list, the
//! order, and the decision to load the next item, and every command still reaches
//! the engine through the same trait a controller uses. Tests drive it with a
//! scripted transport, so playlist behaviour is measured without a codec, a sound
//! card or a window.
//!
//! ## Auto-advance
//!
//! A monitor thread samples the transport snapshot on a fixed tick. When it sees
//! `Ended` — or an error it cannot recover from — it loads the next item. There is
//! no MediaEngine event to wait for, and adding one would only create a second
//! path to reason about.
//!
//! Still images are the exception that needs its own clock. `IMFMediaEngine`
//! refuses them outright, so the launcher paints them itself and the transport
//! reports `Playing` forever: nothing ever ends. The queue holds a dwell timer for
//! image items and advances on elapsed time, which is also what makes a slideshow
//! work.
//!
//! ## One lock, deliberately
//!
//! Transport commands are issued while holding the queue lock. The monitor and the
//! callers — a controller's SOAP request, the big screen's own buttons, the native
//! HTTP API — all want to change what is playing, and the interesting question is
//! always which one wins, never what happens if both act at once. The player
//! thread does not take this lock, so holding it across a channel send cannot
//! deadlock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{CastError, CastResult};
use crate::player::{PlaybackState, Phase, Transport};

/// How long a photo stays on screen before the queue moves on, seconds.
const DEFAULT_IMAGE_DWELL: f64 = 6.0;
/// The monitor's sampling interval. Coarse enough to be free, fine enough that a
/// slideshow never visibly overshoots its dwell.
const TICK: Duration = Duration::from_millis(150);

/// What kind of item a track is. Drives the big screen's renderer choice and the
/// image dwell, nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    Image,
}

impl MediaKind {
    /// Classified from the URI, because a DLNA controller hands over nothing else.
    ///
    /// Ambiguous containers (`mp4`, `m4a`) answer `Video`: a wrong guess there is
    /// harmless, since timed items advance on the engine's own `Ended`. A wrong
    /// guess the other way would strand the queue on something that never ends.
    pub fn from_uri(uri: &str) -> Self {
        let lower = uri.to_ascii_lowercase();
        let path = lower.split(['?', '#']).next().unwrap_or(&lower);
        if [
            ".jpg", ".jpeg", ".png", ".webp", ".gif", ".bmp", ".heic", ".heif", ".avif",
        ]
        .iter()
        .any(|ext| path.ends_with(ext))
        {
            return MediaKind::Image;
        }
        if [
            ".mp3", ".wav", ".aac", ".flac", ".ogg", ".oga", ".wma", ".m4a", ".mka", ".opus",
        ]
        .iter()
        .any(|ext| path.ends_with(ext))
        {
            return MediaKind::Audio;
        }
        MediaKind::Video
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Image => "image",
        }
    }
}

/// One entry in the queue.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// Stable handle for the native API. A track that arrived from a controller
    /// has no id of its own, so it derives one from its URI.
    pub id: String,
    pub uri: String,
    pub title: String,
    pub kind: MediaKind,
}

impl Track {
    pub fn new(id: impl Into<String>, uri: impl Into<String>, title: impl Into<String>) -> Self {
        let uri = uri.into();
        let kind = MediaKind::from_uri(&uri);
        Self {
            id: id.into(),
            uri,
            title: title.into(),
            kind,
        }
    }

    /// A track from a URI alone, the way a DLNA controller hands it over. The
    /// title falls back to the last path segment, which is what a controller with
    /// no metadata still expects to see on the big screen.
    pub fn from_uri(uri: impl Into<String>) -> Self {
        let uri = uri.into();
        let title = uri
            .split(['?', '#'])
            .next()
            .and_then(|path| path.rsplit('/').next().map(str::to_string))
            .unwrap_or_default();
        let id = format!("uri:{uri}");
        Self::new(id, uri, title)
    }
}

/// The order items play in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Sequential,
    Shuffle,
}

/// What happens at the end of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    /// Play the whole list again.
    All,
    /// Play the current item again.
    One,
}

/// What the queue looks like right now, for the big screen and the native API.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueSnapshot {
    pub items: Vec<Track>,
    /// Index into `items`, or `None` before the first load.
    pub current: Option<usize>,
    pub order: Order,
    pub repeat: Repeat,
    pub image_dwell: f64,
    /// The transport state of the current item, kept separate from the queue's own
    /// fields so a caller never has to guess which layer a `Paused` came from.
    pub state: PlaybackState,
    /// Whether the queue means to be running, as distinct from what the engine
    /// reports: a paused item and a finished list both say `STOPPED`.
    pub playing: bool,
    /// Why the queue stopped by itself. An item that cannot be decoded is
    /// reported, not skipped forever.
    pub halted: Option<String>,
}

struct Inner {
    items: Vec<Track>,
    /// The playing order: a permutation of the indices into `items`. Kept as a
    /// permutation rather than a flag so advancing is one lookup in either mode.
    sequence: Vec<usize>,
    current: Option<usize>,
    /// The URI the transport holds, so resuming a paused item is not a reload.
    loaded: Option<String>,
    playing: bool,
    order: Order,
    repeat: Repeat,
    image_dwell: f64,
    /// Seconds the current image has been on screen while playing.
    shown: f64,
    /// The item a controller asked to follow this one (`SetNextAVTransportURI`).
    next_override: Option<Track>,
    /// Consecutive decode failures. Without a bound, a list of unreadable files
    /// spins the monitor as fast as it can tick.
    failures: usize,
    halted: Option<String>,
    seed: u64,
}

impl Inner {
    fn new(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            sequence: Vec::new(),
            current: None,
            loaded: None,
            playing: false,
            order: Order::Sequential,
            repeat: Repeat::Off,
            image_dwell: DEFAULT_IMAGE_DWELL,
            shown: 0.0,
            next_override: None,
            failures: 0,
            halted: None,
            seed,
        }
    }

    /// Rebuild the order after any change to the list or the mode.
    fn resequence(&mut self) {
        self.sequence = (0..self.items.len()).collect();
        if self.order == Order::Shuffle {
            shuffle(&mut self.sequence, &mut self.seed);
        }
        self.anchor();
    }

    /// Put the item in play at the front of the order. Without this, switching a
    /// running queue to shuffle would move the photo people are looking to into
    /// the middle of the list, and `next` would then walk past it to whatever
    /// followed — losing the item rather than skipping it.
    fn anchor(&mut self) {
        let Some(current) = self.current else {
            return;
        };
        if self.order != Order::Shuffle {
            return;
        }
        let Some(position) = self.sequence.iter().position(|index| *index == current) else {
            return;
        };
        if position > 0 {
            self.sequence.remove(position);
            self.sequence.insert(0, current);
        }
    }

    /// Where the current item sits in the order, if it is still in the list.
    fn position(&self) -> Option<usize> {
        let current = self.current?;
        self.sequence.iter().position(|index| *index == current)
    }

    fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|index| self.items.get(index))
    }

    /// The next item, wrapping only when the list repeats.
    fn upcoming(&self) -> Option<usize> {
        let position = self.position()?;
        if position + 1 < self.sequence.len() {
            Some(self.sequence[position + 1])
        } else if self.repeat == Repeat::All {
            Some(self.sequence[0])
        } else {
            None
        }
    }

    fn previous(&self) -> Option<usize> {
        let position = self.position()?;
        if position > 0 {
            Some(self.sequence[position - 1])
        } else if self.repeat == Repeat::All {
            self.sequence.last().copied()
        } else {
            // At the front of a list that does not repeat: replay it rather than
            // reporting that nothing comes before this.
            self.sequence.first().copied()
        }
    }

    /// The item after the current one, re-rolling a shuffled list when a pass
    /// ends. `None` means the list is finished, which is what lets the DLNA layer
    /// answer `Next` with UPnP error 701 instead of silently stopping.
    fn advance_to(&mut self) -> Option<usize> {
        let position = self.position()?;
        if position + 1 < self.sequence.len() {
            return Some(self.sequence[position + 1]);
        }
        if self.repeat != Repeat::All {
            return None;
        }
        if self.order == Order::Shuffle {
            // A fresh roll for the next pass, so a listener never hears the same
            // sequence twice in a row. The item that just ended is skipped at the
            // front of it: an immediate repeat looks like a stuck queue.
            let finished = self.current;
            shuffle(&mut self.sequence, &mut self.seed);
            return self
                .sequence
                .iter()
                .copied()
                .find(|index| Some(*index) != finished)
                .or(self.sequence.first().copied());
        }
        self.sequence.first().copied()
    }

    /// Hand the current item to the transport, loading only if the transport is
    /// not already holding it. Every mutation ends with this call, so the engine
    /// and the queue can never disagree about what should be on screen.
    fn render(&mut self, transport: &dyn Transport, autoplay: bool) -> CastResult<()> {
        let Some(index) = self.current else {
            // Nothing to play: the run is over, even if the list still has the
            // items a controller queued before it was emptied.
            self.loaded = None;
            self.playing = false;
            return transport.stop();
        };
        let uri = self.items[index].uri.clone();
        if self.loaded.as_deref() != Some(uri.as_str()) {
            transport.load(&uri)?;
            self.loaded = Some(uri);
            self.shown = 0.0;
        }
        if autoplay {
            return transport.play();
        }
        Ok(())
    }
}

/// The queue. Whoever owns the `Arc` shares one playlist: the DLNA control plane,
/// the big screen, and the native HTTP API all drive the same list.
pub struct Playlist {
    inner: Arc<Mutex<Inner>>,
    transport: Arc<dyn Transport>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Playlist {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            // The monitor sleeps for one tick, so this is bounded by `TICK`.
            let _ = thread.join();
        }
    }
}

impl Playlist {
    /// A playlist that watches its transport and advances by itself.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        let this = Self::detached(transport, seed_from_clock());
        this.start_monitor();
        this
    }

    /// The same, with a fixed shuffle seed so a test can assert the order.
    pub fn with_seed(transport: Arc<dyn Transport>, seed: u64) -> Self {
        let this = Self::detached(transport, seed);
        this.start_monitor();
        this
    }

    /// A playlist with no monitor thread. The caller advances it by hand, which
    /// is how the tests assert a dwell or an `Ended` transition without sleeping.
    pub fn detached(transport: Arc<dyn Transport>, seed: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(seed))),
            transport,
            stop: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }

    fn start_monitor(&self) {
        let inner = Arc::clone(&self.inner);
        let transport = Arc::clone(&self.transport);
        let stop = Arc::clone(&self.stop);
        let handle = std::thread::Builder::new()
            .name("horizon-playlist".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(TICK);
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let state = transport.state();
                    let mut shared = lock(&inner);
                    on_tick(&mut shared, transport.as_ref(), &state, TICK);
                }
            });
        if let Ok(handle) = handle {
            let mut slot = self
                .thread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(handle);
        }
        // If no thread could be spawned the list still plays the item it was
        // handed; only auto-advance is lost, and the renderer's own status page
        // reports the transport state either way.
    }

    /// One tick of the monitor, exposed so a test can drive time directly.
    #[cfg(test)]
    fn advance_now(&self, state: &PlaybackState, elapsed: Duration) {
        let mut shared = self.shared();
        on_tick(&mut shared, self.transport.as_ref(), state, elapsed);
    }

    fn shared(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    // ---- building the list ----

    /// Replace the whole list and put one item on screen without playing it. This
    /// is what a controller's `SetAVTransportURI` means, widened to a list.
    pub fn set_items(&self, items: Vec<Track>, start: Option<usize>) -> CastResult<()> {
        let mut shared = self.shared();
        shared.items = items;
        shared.current = if shared.items.is_empty() {
            None
        } else {
            Some(start.unwrap_or(0).min(shared.items.len() - 1))
        };
        shared.playing = false;
        shared.shown = 0.0;
        shared.failures = 0;
        shared.halted = None;
        shared.next_override = None;
        shared.resequence();
        shared.render(self.transport.as_ref(), false)
    }

    /// A one-item list, which is all a standard DLNA controller can send.
    pub fn replace_one(&self, uri: &str) -> CastResult<()> {
        self.set_items(vec![Track::from_uri(uri)], None)
    }

    /// Append items. On an empty queue the first appended item becomes the one on
    /// screen, because "cast these" with nothing loaded means play them, not
    /// stage them.
    pub fn enqueue(&self, items: Vec<Track>, autoplay: bool) -> CastResult<()> {
        let mut shared = self.shared();
        let was_empty = shared.items.is_empty();
        shared.items.extend(items);
        shared.resequence();
        if was_empty && !shared.items.is_empty() {
            shared.current = Some(0);
            shared.shown = 0.0;
            shared.failures = 0;
            shared.halted = None;
        }
        if autoplay && was_empty {
            shared.playing = true;
            return shared.render(self.transport.as_ref(), true);
        }
        Ok(())
    }

    /// The item a controller asked to follow the current one. Kept as an override
    /// rather than an insert: UPnP does not say where it lands if the list changes
    /// in between, and playing it next is the plainest reading.
    pub fn set_next(&self, track: Track) {
        self.shared().next_override = Some(track);
    }

    pub fn clear(&self) -> CastResult<()> {
        let mut shared = self.shared();
        shared.items.clear();
        shared.sequence.clear();
        shared.current = None;
        shared.playing = false;
        shared.shown = 0.0;
        shared.next_override = None;
        shared.halted = None;
        shared.render(self.transport.as_ref(), false)
    }

    /// Remove one item. Removing what is on screen moves to whatever now sits in
    /// that slot, because leaving a deleted file up is worse than advancing.
    pub fn remove(&self, index: usize) -> CastResult<()> {
        let mut shared = self.shared();
        if index >= shared.items.len() {
            return Err(CastError::InvalidState("队列里没有这一项"));
        }
        let was_current = shared.current == Some(index);
        shared.items.remove(index);
        shared.current = match shared.current {
            Some(current) if current > index => Some(current - 1),
            Some(current) if current == index => {
                if shared.items.is_empty() {
                    None
                } else {
                    Some(current.min(shared.items.len() - 1))
                }
            }
            other => other,
        };
        shared.resequence();
        if was_current {
            shared.shown = 0.0;
        }
        let autoplay = shared.playing;
        shared.render(self.transport.as_ref(), autoplay)
    }

    /// Reorder within the list. The playing position follows the item, not the
    /// slot, so dragging the current track does not restart it.
    pub fn reorder(&self, from: usize, to: usize) -> CastResult<()> {
        let mut shared = self.shared();
        if from >= shared.items.len() || to >= shared.items.len() {
            return Err(CastError::InvalidState("队列里没有这一项"));
        }
        if from == to {
            return Ok(());
        }
        let track = shared.items.remove(from);
        shared.items.insert(to, track);
        shared.current = match shared.current {
            Some(current) if current == from => Some(to),
            Some(current) if from < current && current <= to => Some(current - 1),
            Some(current) if to <= current && current < from => Some(current + 1),
            other => other,
        };
        shared.resequence();
        // The transport still holds the same URI, so `render` reloads nothing and
        // the item in play is unaffected by the shuffle of the list.
        shared.render(self.transport.as_ref(), false)
    }

    pub fn set_order(&self, order: Order) {
        let mut shared = self.shared();
        shared.order = order;
        shared.resequence();
    }

    pub fn set_repeat(&self, repeat: Repeat) {
        self.shared().repeat = repeat;
    }

    pub fn set_image_dwell(&self, seconds: f64) {
        self.shared().image_dwell = seconds.max(0.5);
    }

    // ---- transport, through the queue ----

    pub fn play(&self) -> CastResult<()> {
        let mut shared = self.shared();
        if shared.current.is_none() {
            shared.current = shared.sequence.first().copied();
            shared.shown = 0.0;
            shared.failures = 0;
            shared.halted = None;
        }
        shared.playing = true;
        shared.render(self.transport.as_ref(), true)
    }

    pub fn pause(&self) -> CastResult<()> {
        self.shared().playing = false;
        self.transport.pause()
    }

    /// Stop the item but keep the list. A controller that pressed Stop still owns
    /// the queue it built.
    pub fn stop(&self) -> CastResult<()> {
        self.shared().playing = false;
        self.transport.stop()
    }

    pub fn seek(&self, seconds: f64) -> CastResult<()> {
        self.transport.seek(seconds)
    }

    pub fn set_volume(&self, volume: f64) -> CastResult<()> {
        self.transport.set_volume(volume)
    }

    pub fn set_mute(&self, muted: bool) -> CastResult<()> {
        self.transport.set_mute(muted)
    }

    pub fn present(&self, rect: Option<(i32, i32, i32, i32)>) -> CastResult<()> {
        self.transport.present(rect)
    }

    /// Skip forward, whether or not the queue is playing.
    pub fn next(&self) -> CastResult<()> {
        self.step(true)
    }

    pub fn previous(&self) -> CastResult<()> {
        self.step(false)
    }

    /// Whether there is anything after the current item. The DLNA layer needs
    /// this to answer `Next` with UPnP error 701 at the end of a list, which is
    /// what a controller expects instead of a silent stop.
    pub fn has_next(&self) -> bool {
        let shared = self.shared();
        // A staged `SetNextAVTransportURI` is a next item even at the end of a
        // list: `step` plays the override before it looks at the sequence, and a
        // controller that staged one would read `701` as "the staging failed".
        shared.next_override.is_some() || shared.upcoming().is_some()
    }

    /// Jump to a list index — the native API's "play this one now".
    pub fn jump(&self, index: usize) -> CastResult<()> {
        let mut shared = self.shared();
        if index >= shared.items.len() {
            return Err(CastError::InvalidState("队列里没有这一项"));
        }
        shared.current = Some(index);
        shared.shown = 0.0;
        shared.failures = 0;
        shared.halted = None;
        let autoplay = shared.playing;
        shared.render(self.transport.as_ref(), autoplay)
    }

    fn step(&self, forward: bool) -> CastResult<()> {
        let mut shared = self.shared();
        let autoplay = shared.playing;
        let override_track = if forward {
            shared.next_override.take()
        } else {
            None
        };
        if let Some(track) = override_track {
            let index = shared.items.len();
            shared.items.push(track);
            shared.resequence();
            shared.current = Some(index);
        } else {
            let target = if forward {
                shared.advance_to()
            } else {
                shared.previous()
            };
            match target {
                Some(index) => shared.current = Some(index),
                // Past the end of a list that does not repeat.
                None => {
                    shared.playing = false;
                    return shared.render(self.transport.as_ref(), false);
                }
            }
        }
        shared.shown = 0.0;
        shared.failures = 0;
        shared.render(self.transport.as_ref(), autoplay)
    }

    // ---- reading ----

    pub fn state(&self) -> PlaybackState {
        self.transport.state()
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        let shared = self.shared();
        QueueSnapshot {
            items: shared.items.clone(),
            current: shared.current,
            order: shared.order,
            repeat: shared.repeat,
            image_dwell: shared.image_dwell,
            state: self.transport.state(),
            playing: shared.playing,
            halted: shared.halted.clone(),
        }
    }

    pub fn is_playing(&self) -> bool {
        self.shared().playing
    }

    pub fn current(&self) -> Option<Track> {
        self.shared().current_track().cloned()
    }

    /// The image dwell in force, exposed for the big screen's countdown.
    pub fn image_dwell(&self) -> f64 {
        self.shared().image_dwell
    }
}

/// One tick: is the current item finished, by the engine's account or by the
/// clock?
fn on_tick(shared: &mut Inner, transport: &dyn Transport, state: &PlaybackState, elapsed: Duration) {
    if !shared.playing {
        return;
    }
    match state.phase {
        // A decode failure is an ending too. Counting them and halting once every
        // item has failed is what keeps a list of broken files from cycling.
        Phase::Error => {
            shared.failures += 1;
            if halt(shared) {
                return;
            }
            advance(shared, transport);
        }
        Phase::Ended => {
            shared.failures = 0;
            advance(shared, transport);
        }
        Phase::Playing => {
            shared.failures = 0;
            if shared.current_track().map(|track| track.kind) == Some(MediaKind::Image) {
                shared.shown += elapsed.as_secs_f64();
                if shared.shown >= shared.image_dwell {
                    advance(shared, transport);
                }
            }
        }
        _ => {}
    }
}

/// True once the queue has given up, so the caller must not advance again.
fn halt(shared: &mut Inner) -> bool {
    let total = shared.items.len();
    if total == 0 || shared.failures < total {
        return false;
    }
    shared.halted = Some(format!("队列里 {total} 个条目都无法解码，已停止"));
    shared.playing = false;
    shared.failures = 0;
    true
}

fn advance(shared: &mut Inner, transport: &dyn Transport) {
    if shared.repeat == Repeat::One {
        // The same item again. Reload rather than seek to zero, because a still
        // image has no timeline to seek and the engine is not even involved.
        shared.loaded = None;
        shared.shown = 0.0;
        let _ = shared.render(transport, true);
        return;
    }
    let override_track = shared.next_override.take();
    if let Some(track) = override_track {
        let index = shared.items.len();
        shared.items.push(track);
        shared.resequence();
        shared.current = Some(index);
    } else {
        match shared.advance_to() {
            Some(index) => shared.current = Some(index),
            None => {
                // The end of a list that does not repeat. This is a normal
                // finish, not a fault, so nothing goes into `halted`.
                shared.playing = false;
                shared.shown = 0.0;
                let _ = transport.stop();
                return;
            }
        }
    }
    shared.shown = 0.0;
    let _ = shared.render(transport, true);
}

fn lock(inner: &Arc<Mutex<Inner>>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fisher-Yates over a splitmix64 stream. `rand` is not a dependency of this
/// crate and a shuffle does not need a cryptographic permutation.
fn shuffle(order: &mut [usize], seed: &mut u64) {
    for index in (1..order.len()).rev() {
        let swap = (next_random(seed) % (index as u64 + 1)) as usize;
        order.swap(index, swap);
    }
}

fn next_random(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut value = *seed;
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_8B3F);
    value ^ (value >> 31)
}

fn seed_from_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x5EED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A transport that records what the queue asked for and lets the test decide
    /// when an item has ended.
    struct Scripted {
        inner: Mutex<ScriptedInner>,
    }

    struct ScriptedInner {
        loads: Vec<String>,
        state: PlaybackState,
        stops: usize,
    }

    impl Default for ScriptedInner {
        fn default() -> Self {
            Self {
                loads: Vec::new(),
                state: PlaybackState::default(),
                stops: 0,
            }
        }
    }

    impl Scripted {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(ScriptedInner::default()),
            })
        }

        fn shared(&self) -> MutexGuard<'_, ScriptedInner> {
            self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn loads(&self) -> Vec<String> {
            self.shared().loads.clone()
        }

        fn stops(&self) -> usize {
            self.shared().stops
        }

        fn set_phase(&self, phase: Phase) {
            self.shared().state.phase = phase;
        }
    }

    impl Transport for Scripted {
        fn load(&self, uri: &str) -> CastResult<()> {
            let mut inner = self.shared();
            inner.loads.push(uri.to_string());
            inner.state.uri = Some(uri.to_string());
            inner.state.phase = Phase::Stopped;
            inner.state.position = 0.0;
            Ok(())
        }

        fn play(&self) -> CastResult<()> {
            self.shared().state.phase = Phase::Playing;
            Ok(())
        }

        fn pause(&self) -> CastResult<()> {
            self.shared().state.phase = Phase::Paused;
            Ok(())
        }

        fn stop(&self) -> CastResult<()> {
            let mut inner = self.shared();
            inner.stops += 1;
            inner.state.phase = Phase::Stopped;
            inner.state.position = 0.0;
            Ok(())
        }

        fn seek(&self, _seconds: f64) -> CastResult<()> {
            Ok(())
        }

        fn set_volume(&self, volume: f64) -> CastResult<()> {
            self.shared().state.volume = volume;
            Ok(())
        }

        fn set_mute(&self, muted: bool) -> CastResult<()> {
            self.shared().state.muted = muted;
            Ok(())
        }

        fn state(&self) -> PlaybackState {
            self.shared().state.clone()
        }
    }

    fn tracks(names: &[&str]) -> Vec<Track> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| Track::new(format!("t{index}"), format!("http://h/{name}"), *name))
            .collect()
    }

    /// A queue with no monitor thread, plus the transport it drives.
    fn rig() -> (Arc<Scripted>, Playlist) {
        let scripted = Scripted::new();
        let queue = Playlist::detached(Arc::clone(&scripted) as Arc<dyn Transport>, 7);
        (scripted, queue)
    }

    /// The monitor's job, performed once by the test.
    fn tick(scripted: &Scripted, queue: &Playlist) {
        queue.advance_now(&scripted.state(), Duration::from_millis(150));
    }

    #[test]
    fn a_controller_uri_becomes_a_one_item_queue() {
        let (scripted, queue) = rig();
        queue
            .replace_one("http://192.168.0.9:8000/media/clip.mp4")
            .expect("load");
        assert_eq!(
            scripted.loads(),
            vec!["http://192.168.0.9:8000/media/clip.mp4".to_string()]
        );
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.current, Some(0));
        assert_eq!(snapshot.items[0].title, "clip.mp4");
        assert_eq!(snapshot.items[0].kind, MediaKind::Video);
        assert!(!snapshot.playing, "a controller still has to say Play");
    }

    #[test]
    fn kinds_come_from_the_uri_a_controller_sent() {
        assert_eq!(MediaKind::from_uri("http://h/a.JPG"), MediaKind::Image);
        assert_eq!(
            MediaKind::from_uri("http://h/a.png?size=200"),
            MediaKind::Image
        );
        assert_eq!(MediaKind::from_uri("http://h/song.mp3"), MediaKind::Audio);
        assert_eq!(MediaKind::from_uri("http://h/film.mkv"), MediaKind::Video);
    }

    #[test]
    fn an_ended_item_advances_and_a_finished_list_does_not_loop() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), None).expect("set");
        queue.play().expect("play");
        assert_eq!(scripted.loads(), vec!["http://h/a.mp4".to_string()]);

        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(1));

        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(2));
        assert_eq!(scripted.loads().len(), 3);

        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.current, Some(2), "stays on the last item");
        assert!(!queue.is_playing(), "a finished list is not a running one");
        assert!(snapshot.halted.is_none(), "finishing is not a fault");
        assert!(scripted.stops() >= 1, "the transport is told to stop");
    }

    #[test]
    fn repeat_all_wraps_to_the_front() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.set_repeat(Repeat::All);
        queue.play().expect("play");
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(1));
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(0));
        assert!(queue.is_playing());
    }

    #[test]
    fn repeat_one_reloads_the_same_item() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.set_repeat(Repeat::One);
        queue.play().expect("play");
        let before = scripted.loads().len();
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(0));
        assert_eq!(scripted.loads()[before], "http://h/a.mp4", "reloaded, not skipped");
        assert!(queue.is_playing());
    }

    #[test]
    fn a_photo_advances_on_its_dwell_because_the_engine_never_ends_it() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["photo.jpg", "next.jpg"]), None).expect("set");
        queue.set_image_dwell(0.02);
        assert_eq!(
            queue.image_dwell(),
            0.5,
            "a flash of a photo is not a slideshow"
        );
        queue.set_image_dwell(0.6);
        queue.play().expect("play");
        assert_eq!(queue.snapshot().items[0].kind, MediaKind::Image);
        // MediaEngine is never asked about a photo, so the phase just sits at
        // Playing and only the clock can move the slideshow on.
        tick(&scripted, &queue);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(0), "0.3s of a 0.6s dwell");
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(0), "0.45s of a 0.6s dwell");
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(1));
    }

    #[test]
    fn a_paused_photo_does_not_count_down() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["photo.jpg", "next.jpg"]), None).expect("set");
        queue.set_image_dwell(0.3);
        queue.play().expect("play");
        queue.pause().expect("pause");
        for _ in 0..10 {
            tick(&scripted, &queue);
        }
        assert_eq!(queue.snapshot().current, Some(0), "a paused slideshow holds its frame");
    }

    /// The titles in the order the queue holds them.
    fn titles(queue: &Playlist) -> Vec<String> {
        queue.snapshot().items.iter().map(|t| t.title.clone()).collect()
    }

    fn current_title(queue: &Playlist) -> String {
        let snapshot = queue.snapshot();
        snapshot.items[snapshot.current.expect("a current item")].title.clone()
    }

    #[test]
    fn shuffle_is_a_true_permutation_and_plays_every_item_once_per_pass() {
        let (scripted, queue) = rig();
        queue
            .set_items(tracks(&["1.mp4", "2.mp4", "3.mp4", "4.mp4", "5.mp4"]), None)
            .expect("set");
        queue.set_order(Order::Shuffle);
        queue.play().expect("play");

        // The item in play sits at the front of the roll, so switching a running
        // queue to shuffle never skips past what is already on screen.
        let head = {
            let shared = queue.shared();
            shared.sequence[0]
        };
        assert_eq!(queue.snapshot().current, Some(head));

        let mut pass = vec![current_title(&queue)];
        for _ in 0..4 {
            scripted.set_phase(Phase::Ended);
            tick(&scripted, &queue);
            pass.push(current_title(&queue));
        }
        let mut sorted = pass.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["1.mp4", "2.mp4", "3.mp4", "4.mp4", "5.mp4"]);
        assert_eq!(pass.len(), 5);
        assert_eq!(titles(&queue).len(), 5, "the list itself is untouched");
    }

    #[test]
    fn a_shuffled_pass_re_rolls_without_repeating_the_item_that_just_ended() {
        let (scripted, queue) = rig();
        queue
            .set_items(tracks(&["1.mp4", "2.mp4", "3.mp4", "4.mp4", "5.mp4"]), None)
            .expect("set");
        queue.set_order(Order::Shuffle);
        queue.set_repeat(Repeat::All);
        queue.play().expect("play");
        for _ in 0..4 {
            scripted.set_phase(Phase::Ended);
            tick(&scripted, &queue);
        }
        let last = current_title(&queue);
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_ne!(current_title(&queue), last, "an immediate replay looks stuck");
        assert!(queue.is_playing(), "the second pass started");
    }

    #[test]
    fn a_broken_list_halts_instead_of_cycling_forever() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.play().expect("play");
        for _ in 0..10 {
            scripted.set_phase(Phase::Error);
            tick(&scripted, &queue);
        }
        let snapshot = queue.snapshot();
        assert!(snapshot.halted.is_some(), "the queue says why it gave up");
        assert!(!queue.is_playing());
        assert_eq!(
            scripted.loads().len(),
            2,
            "one load per item, not one per tick: {:?}",
            scripted.loads()
        );
    }

    #[test]
    fn next_override_from_a_controller_is_played_after_the_current_item() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.play().expect("play");
        queue.set_next(Track::from_uri("http://h/next.mp4"));
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.items.len(), 3, "the override joins the list");
        assert_eq!(snapshot.items[snapshot.current.unwrap()].title, "next.mp4");
    }

    #[test]
    fn manual_next_and_previous_walk_the_list() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), None).expect("set");
        queue.next().expect("next");
        assert_eq!(queue.snapshot().current, Some(1));
        queue.next().expect("next");
        assert_eq!(queue.snapshot().current, Some(2));
        queue.previous().expect("previous");
        assert_eq!(queue.snapshot().current, Some(1));
        assert_eq!(scripted.loads().last().unwrap(), "http://h/b.mp4");
    }

    #[test]
    fn the_end_of_a_list_reports_that_there_is_nothing_next() {
        let (_scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        assert!(queue.has_next());
        queue.jump(1).expect("jump");
        assert!(!queue.has_next(), "a controller must get 701, not silence");
        // A staged `SetNextAVTransportURI` is a next item even here: `step` plays
        // the override before it looks at the sequence, so answering "nowhere to
        // go" while one is staged would reject a staging the controller just made.
        queue.set_next(Track::from_uri("http://h/staged.mp4"));
        assert!(queue.has_next(), "预排的那条在队尾也算有下一条");
        queue.set_repeat(Repeat::All);
        assert!(queue.has_next());
    }

    #[test]
    fn removing_the_current_item_moves_to_the_next_one() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), None).expect("set");
        queue.play().expect("play");
        queue.remove(0).expect("remove");
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.items.len(), 2);
        assert_eq!(snapshot.current, Some(0));
        assert_eq!(snapshot.items[0].title, "b.mp4");
        assert_eq!(scripted.loads().last().unwrap(), "http://h/b.mp4");
        assert!(queue.is_playing(), "a slideshow does not stop because a photo died");
    }

    #[test]
    fn removing_a_queued_item_leaves_playback_alone() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), None).expect("set");
        queue.play().expect("play");
        let before = scripted.loads();
        queue.remove(2).expect("remove");
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.items.len(), 2);
        assert_eq!(snapshot.current, Some(0));
        assert_eq!(scripted.loads(), before, "nothing was reloaded");
    }

    #[test]
    fn removing_the_last_item_stops_the_queue() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4"]), None).expect("set");
        queue.play().expect("play");
        queue.remove(0).expect("remove");
        let snapshot = queue.snapshot();
        assert!(snapshot.items.is_empty());
        assert_eq!(snapshot.current, None);
        assert!(!queue.is_playing());
        assert!(scripted.stops() >= 1);
    }

    #[test]
    fn reorder_follows_the_item_not_the_slot() {
        let (scripted, queue) = rig();
        queue
            .set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), Some(2))
            .expect("set");
        queue.play().expect("play");
        let before = scripted.loads().len();
        // Move the last item to the front: the item in play stays in play.
        queue.reorder(2, 0).expect("reorder");
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.current, Some(0));
        assert_eq!(snapshot.items[0].title, "c.mp4");
        assert_eq!(scripted.loads().len(), before, "no reload for a reorder");
    }

    #[test]
    fn reorder_moves_the_rest_of_the_list_by_one() {
        let (_scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4", "c.mp4"]), Some(1)).expect("set");
        queue.reorder(0, 2).expect("reorder");
        let snapshot = queue.snapshot();
        assert_eq!(
            snapshot.items.iter().map(|t| t.title.clone()).collect::<Vec<_>>(),
            vec!["b.mp4", "c.mp4", "a.mp4"]
        );
        assert_eq!(snapshot.current, Some(0), "b.mp4 is still the item in play");
    }

    #[test]
    fn enqueuing_onto_an_empty_queue_starts_the_batch() {
        let (scripted, queue) = rig();
        queue.enqueue(tracks(&["a.mp4", "b.mp4"]), true).expect("enqueue");
        assert!(queue.is_playing());
        assert_eq!(scripted.loads(), vec!["http://h/a.mp4".to_string()]);
    }

    #[test]
    fn enqueuing_behind_a_running_queue_does_not_interrupt_it() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4"]), None).expect("set");
        queue.play().expect("play");
        let before = scripted.loads().len();
        queue.enqueue(tracks(&["b.mp4", "c.mp4"]), false).expect("enqueue");
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.items.len(), 3);
        assert_eq!(snapshot.current, Some(0), "still on the first item");
        assert_eq!(scripted.loads().len(), before, "nothing was reloaded");
        assert!(queue.has_next());
    }

    #[test]
    fn stopping_keeps_the_list_but_ends_the_run() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.play().expect("play");
        queue.stop().expect("stop");
        assert_eq!(queue.snapshot().items.len(), 2, "the list survives");
        assert!(!queue.is_playing());
        scripted.set_phase(Phase::Ended);
        tick(&scripted, &queue);
        assert_eq!(queue.snapshot().current, Some(0), "a stopped queue does not advance");
    }

    #[test]
    fn resuming_a_stopped_item_does_not_reload_it() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4", "b.mp4"]), None).expect("set");
        queue.play().expect("play");
        let before = scripted.loads().len();
        queue.pause().expect("pause");
        queue.play().expect("play again");
        assert_eq!(scripted.loads().len(), before, "resume, not restart");
    }

    #[test]
    fn clearing_empties_the_list_and_the_screen() {
        let (scripted, queue) = rig();
        queue.set_items(tracks(&["a.mp4"]), None).expect("set");
        queue.play().expect("play");
        queue.clear().expect("clear");
        let snapshot = queue.snapshot();
        assert!(snapshot.items.is_empty());
        assert_eq!(snapshot.current, None);
        assert!(scripted.stops() >= 1);
    }

    /// The monitor thread is the part a pure-function test cannot cover: without
    /// this, a queue that advances only when a test calls it would be a queue that
    /// never advances in the app.
    #[test]
    fn the_monitor_thread_advances_a_photo_on_its_own() {
        let scripted = Scripted::new();
        let queue = Playlist::with_seed(Arc::clone(&scripted) as Arc<dyn Transport>, 11);
        queue.set_items(tracks(&["a.jpg", "b.jpg"]), None).expect("set");
        queue.set_image_dwell(0.5);
        queue.play().expect("play");
        assert_eq!(queue.snapshot().current, Some(0));
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(queue.snapshot().current, Some(1), "the thread advanced it");
    }

    #[test]
    fn a_playlist_drops_its_monitor_cleanly() {
        let scripted = Scripted::new();
        let queue = Playlist::new(Arc::clone(&scripted) as Arc<dyn Transport>);
        queue.enqueue(tracks(&["a.mp4"]), true).expect("enqueue");
        drop(queue);
    }
}
