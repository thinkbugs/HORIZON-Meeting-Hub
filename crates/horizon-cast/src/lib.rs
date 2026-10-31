//! HORIZON casting engine.
//!
//! Layering, from the wire up:
//! - [`error`] — one error type shared by every layer.
//! - [`metrics`] — the measurement system every optimisation is judged by.
//! - [`codec`] — Media Foundation H.264 encode/decode, the interop path.
//! - `capture` — screen acquisition.
//! - [`wire`] — the tile codec the self-built channel speaks: one frame in, one
//!   frame out, lossless where the content is text.
//! - [`transport`] — UDP send/receive, pacing, socket tuning.
//! - [`session`] — sender/receiver state machines and the sockets they run on.
//! - [`discovery`] — mDNS and SSDP.
//! - [`dlna`] — the UPnP MediaRenderer a television or controller drives.
//! - [`queue`] — the playlist above one transport: order, repeat, auto-advance.
//! - [`cast`] — the Google Cast v2 sender for Chromecast and Cast-enabled TVs.
//! - [`cast_recv`] — the Google Cast v2 receiver (this machine as a Chromecast).
//! - [`airplay`] — the AirPlay 1 (RAOP) audio receiver.

#[cfg(windows)]
pub mod capture;
pub mod discovery;
pub mod dlna;
pub mod error;
pub mod metrics;
pub mod session;
pub mod transport;
pub mod wire;

#[cfg(windows)]
pub mod airplay;
#[cfg(windows)]
pub mod cast;
#[cfg(windows)]
pub mod cast_recv;
#[cfg(windows)]
pub mod browser;
#[cfg(windows)]
pub mod codec;
#[cfg(windows)]
pub mod player;
#[cfg(windows)]
pub mod queue;

pub use error::{CastError, CastResult};
