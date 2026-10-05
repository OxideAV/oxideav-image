//! # oxideav-image
//!
//! The image gateway of the oxideav framework — Layer 2 of the
//! image-crate API (`IMAGE_CRATE_API.md` in the `oxideav-workspace`
//! repository).
//!
//! Every oxideav image-format crate (`oxideav-png`, `oxideav-mjpeg`,
//! `oxideav-heif`, …) can be used on its own with its small standalone
//! API. This crate is the other half: one entry point that opens **any**
//! registered format through the framework's registries, hands back the
//! picture in its native layout as an [`Image`], and converts it to the
//! raw `Vec<u8>` views Rust image consumers expect — plus the reverse
//! direction, [`encode`] / [`save`] by format name or file extension.
//!
//! The application registers the formats it wants; the gateway only
//! consumes the registry and depends on no format crate:
//!
//! ```ignore
//! let mut ctx = oxideav_core::RuntimeContext::new();
//! oxideav_meta::register_all(&mut ctx);           // or just the crates you need
//!
//! let file = oxideav_image::open(&ctx, "photo.heic")?;
//! let img = file.primary();
//! let rgba: Vec<u8> = img.to_rgba8()?;             // tightly packed, 4 bytes/pixel
//! let (w, h, fmt) = (img.width(), img.height(), img.format());
//!
//! for frame in file.frames() { /* animations, bursts, pages */ }
//!
//! oxideav_image::save(&ctx, img, "out.png", &SaveOptions::default())?;
//! let avif = oxideav_image::encode(&ctx, img, "avif", &SaveOptions::new().with_quality(80))?;
//! ```
//!
//! `oxideav-meta` resolves only inside the workspace, which is why the
//! registration step is the caller's: this crate stays publishable and
//! lean (`oxideav-core` + `oxideav-pixfmt`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod image;
mod open;
mod save;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;

pub use error::{Error, ImageError, Result};
pub use image::Image;
pub use open::{
    decode_bytes, decode_bytes_with, decode_reader, open, open_with, ImageFile, OpenOptions,
};
pub use save::{encode, encode_frames, save, SaveOptions, DEFAULT_DELAY};

/// Re-exported for convenience: the framework's pixel layouts and colour
/// description, which [`Image`] reports.
pub use oxideav_core::{ColorSignal, PixelFormat, RuntimeContext};
