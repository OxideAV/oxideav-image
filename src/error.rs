//! Error type of the gateway.

use std::fmt;

/// Everything `oxideav-image` can fail with.
///
/// Framework errors (registry resolution, demuxing, decoding, muxing,
/// encoding) arrive as [`ImageError::Core`]; the other variants are the
/// gateway's own checks. The enum is `#[non_exhaustive]`: match with a
/// wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImageError {
    /// A file or stream operation failed.
    Io(std::io::Error),
    /// A framework registry, demuxer, decoder, muxer or encoder failed.
    Core(oxideav_core::Error),
    /// The input probed as a known container but produced no video frame.
    NoImage(String),
    /// No registered container matches the requested format name or
    /// file extension, or the container has no muxer.
    UnknownFormat(String),
    /// The request cannot be honoured by the registered codecs or by the
    /// pixel-format converter (no encoder, no conversion path, planar
    /// layout asked for as packed bytes, …).
    Unsupported(String),
    /// Caller-supplied data is inconsistent (buffer length vs geometry,
    /// stream parameters without dimensions, …).
    InvalidData(String),
}

/// Crate-local alias, so signatures read `Result<T, Error>`.
pub type Error = ImageError;

/// Crate-local result alias.
pub type Result<T> = std::result::Result<T, ImageError>;

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "io: {e}"),
            ImageError::Core(e) => write!(f, "{e}"),
            ImageError::NoImage(s) => write!(f, "no image: {s}"),
            ImageError::UnknownFormat(s) => write!(f, "unknown format: {s}"),
            ImageError::Unsupported(s) => write!(f, "unsupported: {s}"),
            ImageError::InvalidData(s) => write!(f, "invalid data: {s}"),
        }
    }
}

impl std::error::Error for ImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImageError::Io(e) => Some(e),
            ImageError::Core(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        ImageError::Io(e)
    }
}

impl From<oxideav_core::Error> for ImageError {
    fn from(e: oxideav_core::Error) -> Self {
        ImageError::Core(e)
    }
}

impl ImageError {
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        ImageError::Unsupported(msg.into())
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        ImageError::InvalidData(msg.into())
    }
}
