//! Single error type shared by every layer of the engine.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastError {
    /// A Win32/COM call returned a failure HRESULT. The code is kept verbatim;
    /// decoding it is what makes Media Foundation bugs findable.
    Hresult(i32),
    InvalidState(&'static str),
    Unsupported(&'static str),
    Io,
    Timeout,
    /// The peer sent bytes we could not parse.
    Malformed(&'static str),
    /// A delta arrived whose base frame is not what this decoder has applied.
    /// Not corruption: the honest answer is "send me a key frame", and a channel
    /// that silently painted over the gap would show a picture with stale tiles.
    NeedsKeyframe,
}

pub type CastResult<T> = Result<T, CastError>;

impl CastError {
    #[cfg(windows)]
    pub fn from_hresult(code: i32) -> Self {
        CastError::Hresult(code)
    }

    /// `0x80070057`-style rendering, which is how these codes appear in every
    /// Media Foundation reference table.
    pub fn hresult_hex(code: i32) -> String {
        format!("0x{:08X}", code as u32)
    }
}

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CastError::Hresult(c) => write!(f, "win32 failure {}", CastError::hresult_hex(*c)),
            CastError::InvalidState(s) => write!(f, "invalid state: {s}"),
            CastError::Unsupported(s) => write!(f, "unsupported: {s}"),
            CastError::Io => write!(f, "i/o error"),
            CastError::Timeout => write!(f, "timed out"),
            CastError::Malformed(s) => write!(f, "malformed data: {s}"),
            CastError::NeedsKeyframe => write!(f, "delta without its base frame"),
        }
    }
}

impl std::error::Error for CastError {}

#[cfg(windows)]
impl From<windows::core::Error> for CastError {
    fn from(err: windows::core::Error) -> Self {
        CastError::Hresult(err.code().0)
    }
}

#[cfg(windows)]
impl From<windows::core::HRESULT> for CastError {
    fn from(code: windows::core::HRESULT) -> Self {
        CastError::Hresult(code.0)
    }
}

impl From<std::io::Error> for CastError {
    fn from(_: std::io::Error) -> Self {
        CastError::Io
    }
}
