//! [`Image`]: a decoded picture in its native layout, with the raw
//! `Vec<u8>` views every Rust image consumer expects.

use std::time::Duration;

use oxideav_core::{
    CodecId, CodecParameters, ColorSignal, MediaType, PixelFormat, VideoFrame, VideoPlane,
};
use oxideav_pixfmt::{convert, ConvertOptions, FrameInfo};

use crate::error::{Error, Result};

/// A decoded picture: a framework [`VideoFrame`] in its native layout
/// plus the [`CodecParameters`] that describe it (dimensions, pixel
/// format, stream-level colour signal).
///
/// Decoders produce these through [`crate::open`] / [`crate::decode_bytes`];
/// callers assemble them with [`Image::from_rgb8`] / [`Image::from_rgba8`] /
/// [`Image::from_raw`]. Conversions go through `oxideav-pixfmt`, so every
/// layout the framework knows is reachable with [`Image::to_format`], and
/// the one-call raw paths ([`to_rgb8`](Image::to_rgb8),
/// [`to_rgba8`](Image::to_rgba8), [`to_rgba16`](Image::to_rgba16))
/// return tightly packed row-major bytes.
#[derive(Clone, Debug)]
pub struct Image {
    frame: VideoFrame,
    params: CodecParameters,
    delay: Option<Duration>,
    index: usize,
}

impl Image {
    /// Wrap a decoded frame. `params` must carry `width`, `height` and
    /// `pixel_format`; the frame must have the plane count the format
    /// implies (side-channel records excluded).
    pub fn from_video_frame(frame: VideoFrame, params: &CodecParameters) -> Result<Self> {
        let width = params
            .width
            .ok_or_else(|| Error::invalid("image stream has no width"))?;
        let height = params
            .height
            .ok_or_else(|| Error::invalid("image stream has no height"))?;
        let format = params
            .pixel_format
            .ok_or_else(|| Error::invalid("image stream has no pixel format"))?;
        if width == 0 || height == 0 {
            return Err(Error::invalid(format!(
                "image geometry {width}x{height} has a zero side"
            )));
        }
        let planes = frame.image_planes().len();
        let expected = format.plane_count();
        if planes != expected {
            return Err(Error::invalid(format!(
                "frame carries {planes} image plane(s) but {format:?} has {expected}"
            )));
        }
        Ok(Self {
            frame,
            params: params.clone(),
            delay: None,
            index: 0,
        })
    }

    /// Build an image from one tightly packed plane of `format` samples
    /// (`width × height × bytes-per-pixel` bytes). Planar layouts are
    /// `Unsupported` here — assemble a [`VideoFrame`] and use
    /// [`from_video_frame`](Self::from_video_frame) for those.
    pub fn from_raw(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::invalid(format!(
                "image geometry {width}x{height} has a zero side"
            )));
        }
        let row = packed_row_bytes(format, width).ok_or_else(|| {
            Error::unsupported(format!(
                "{format:?} is not a packed layout; build a VideoFrame instead"
            ))
        })?;
        let expected = row
            .checked_mul(height as usize)
            .ok_or_else(|| Error::invalid("image geometry overflows usize"))?;
        if data.len() != expected {
            return Err(Error::invalid(format!(
                "{} bytes supplied for a {width}x{height} {format:?} image ({expected} expected)",
                data.len()
            )));
        }
        let mut params = CodecParameters::video(CodecId::new("rawvideo"));
        params.width = Some(width);
        params.height = Some(height);
        params.pixel_format = Some(format);
        Ok(Self {
            frame: VideoFrame {
                pts: Some(0),
                planes: vec![VideoPlane { stride: row, data }],
            },
            params,
            delay: None,
            index: 0,
        })
    }

    /// Packed `Rgb24`, three bytes per pixel, row-major.
    pub fn from_rgb8(width: u32, height: u32, rgb: Vec<u8>) -> Result<Self> {
        Self::from_raw(width, height, PixelFormat::Rgb24, rgb)
    }

    /// Packed `Rgba`, four bytes per pixel, row-major.
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        Self::from_raw(width, height, PixelFormat::Rgba, rgba)
    }

    /// Picture width in pixels.
    pub fn width(&self) -> u32 {
        self.params.width.unwrap_or(0)
    }

    /// Picture height in pixels.
    pub fn height(&self) -> u32 {
        self.params.height.unwrap_or(0)
    }

    /// Native pixel layout.
    pub fn format(&self) -> PixelFormat {
        self.params.pixel_format.unwrap_or(PixelFormat::Rgba)
    }

    /// Colour description: the frame's own signal when attached, else
    /// the stream-level one when it says anything, else `None`.
    pub fn color_signal(&self) -> Option<ColorSignal> {
        self.frame.color_signal().or_else(|| {
            let stream = self.params.color_signal;
            (stream != ColorSignal::default()).then_some(stream)
        })
    }

    /// Attach (or replace) the frame-level colour description, which
    /// [`color_signal`](Self::color_signal) then reports and encoders
    /// receive.
    pub fn with_color_signal(mut self, signal: ColorSignal) -> Self {
        self.frame.set_color_signal(signal);
        self.params.color_signal = signal;
        self
    }

    /// Attached palette (packed RGB triplets) for indexed layouts.
    pub fn palette(&self) -> Option<&[u8]> {
        self.frame.palette()
    }

    /// Display delay for this picture inside a sequence or animation,
    /// when the container said so.
    pub fn delay(&self) -> Option<Duration> {
        self.delay
    }

    /// Set the display delay (used by [`crate::encode_frames`]).
    pub fn with_delay(mut self, delay: Option<Duration>) -> Self {
        self.delay = delay;
        self
    }

    /// Position of this picture in the file it came from (0 for stills).
    pub fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn with_index(mut self, index: usize) -> Self {
        self.index = index;
        self
    }

    /// The underlying frame.
    pub fn frame(&self) -> &VideoFrame {
        &self.frame
    }

    /// The stream parameters describing the frame.
    pub fn params(&self) -> &CodecParameters {
        &self.params
    }

    /// Hand the frame back to the framework (pipeline, encoder, filter).
    pub fn into_video_frame(self) -> (VideoFrame, CodecParameters) {
        (self.frame, self.params)
    }

    /// Convert to another layout through `oxideav-pixfmt`. Same format
    /// returns a clone.
    pub fn to_format(&self, dst: PixelFormat) -> Result<Image> {
        let src = self.format();
        if src == dst {
            return Ok(self.clone());
        }
        let info = FrameInfo::new(src, self.width(), self.height());
        let converted = convert(&self.frame, info, dst, &ConvertOptions::default())?;
        let mut params = self.params.clone();
        params.pixel_format = Some(dst);
        params.media_type = MediaType::Video;
        Ok(Image {
            frame: converted,
            params,
            delay: self.delay,
            index: self.index,
        })
    }

    /// Tightly packed `Rgb24` bytes (`width × height × 3`).
    pub fn to_rgb8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgb24)
    }

    /// Tightly packed `Rgba` bytes (`width × height × 4`); alpha is
    /// opaque when the source has none.
    pub fn to_rgba8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgba)
    }

    /// Tightly packed `Rgba64Le` bytes (`width × height × 8`,
    /// little-endian 16-bit samples).
    pub fn to_rgba16(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgba64Le)
    }

    /// Convert to a packed layout and return its bytes without row
    /// padding.
    pub fn to_packed(&self, dst: PixelFormat) -> Result<Vec<u8>> {
        self.packed_bytes(dst)
    }

    fn packed_bytes(&self, dst: PixelFormat) -> Result<Vec<u8>> {
        let row = packed_row_bytes(dst, self.width()).ok_or_else(|| {
            Error::unsupported(format!("{dst:?} is not a packed layout; use to_format"))
        })?;
        let img = self.to_format(dst)?;
        let plane = img
            .frame
            .image_planes()
            .first()
            .ok_or_else(|| Error::invalid("converted frame has no plane"))?;
        Ok(tight_rows(plane, row, img.height() as usize))
    }

    /// The single plane's bytes when the layout is packed and the rows
    /// carry no padding; `None` for planar or padded frames (use
    /// [`to_packed`](Self::to_packed) / [`into_raw`](Self::into_raw)).
    pub fn as_packed(&self) -> Option<&[u8]> {
        let row = packed_row_bytes(self.format(), self.width())?;
        let planes = self.frame.image_planes();
        if planes.len() != 1 || planes[0].stride != row {
            return None;
        }
        let expected = row.checked_mul(self.height() as usize)?;
        (planes[0].data.len() == expected).then_some(planes[0].data.as_slice())
    }

    /// Every image plane's bytes concatenated in plane order, rows as
    /// stored (strides as [`frame`](Self::frame) reports). Packed layouts
    /// yield the one plane.
    pub fn into_raw(self) -> Vec<u8> {
        let n = self.frame.image_plane_count();
        let mut planes = self.frame.planes;
        planes.truncate(n);
        if planes.len() == 1 {
            return planes.pop().map(|p| p.data).unwrap_or_default();
        }
        let total: usize = planes.iter().map(|p| p.data.len()).sum();
        let mut out = Vec::with_capacity(total);
        for p in planes {
            out.extend_from_slice(&p.data);
        }
        out
    }
}

/// Bytes per row of a packed (single-plane) layout, or `None` for
/// planar / bit-packed layouts the gateway does not hand out as raw
/// bytes.
pub(crate) fn packed_row_bytes(format: PixelFormat, width: u32) -> Option<usize> {
    let bpp: usize = match format {
        PixelFormat::Gray8 | PixelFormat::Pal8 => 1,
        PixelFormat::Gray16Le | PixelFormat::Ya8 => 2,
        PixelFormat::Rgb24 | PixelFormat::Bgr24 => 3,
        PixelFormat::Rgba
        | PixelFormat::Bgra
        | PixelFormat::Argb
        | PixelFormat::Abgr
        | PixelFormat::Ya16Le
        | PixelFormat::GrayF32Le => 4,
        PixelFormat::Rgb48Le => 6,
        PixelFormat::Rgba64Le => 8,
        PixelFormat::RgbF32Le => 12,
        PixelFormat::RgbaF32Le => 16,
        _ => return None,
    };
    (width as usize).checked_mul(bpp)
}

/// Copy `height` rows of `row` bytes out of a plane, dropping any stride
/// padding. A plane that is already tight is copied in one go.
fn tight_rows(plane: &VideoPlane, row: usize, height: usize) -> Vec<u8> {
    if plane.stride == row && plane.data.len() == row * height {
        return plane.data.clone();
    }
    let mut out = Vec::with_capacity(row * height);
    for y in 0..height {
        let start = y * plane.stride;
        let end = (start + row).min(plane.data.len());
        if start >= plane.data.len() {
            break;
        }
        out.extend_from_slice(&plane.data[start..end]);
    }
    out.resize(row * height, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_rgb8_validates_length() {
        assert!(Image::from_rgb8(2, 2, vec![0; 11]).is_err());
        assert!(Image::from_rgb8(0, 2, vec![]).is_err());
        let img = Image::from_rgb8(2, 2, vec![7; 12]).unwrap();
        assert_eq!((img.width(), img.height()), (2, 2));
        assert_eq!(img.format(), PixelFormat::Rgb24);
        assert_eq!(img.as_packed().map(<[u8]>::len), Some(12));
    }

    #[test]
    fn rgb_to_rgba_and_back_is_lossless() {
        let rgb: Vec<u8> = (0..12u8).collect();
        let img = Image::from_rgb8(2, 2, rgb.clone()).unwrap();
        let rgba = img.to_rgba8().unwrap();
        assert_eq!(rgba.len(), 16);
        assert_eq!(&rgba[..4], &[0, 1, 2, 255]);
        let back = Image::from_rgba8(2, 2, rgba).unwrap().to_rgb8().unwrap();
        assert_eq!(back, rgb);
    }

    #[test]
    fn rgba16_widens_samples() {
        let img = Image::from_rgba8(1, 1, vec![255, 0, 128, 255]).unwrap();
        let wide = img.to_rgba16().unwrap();
        assert_eq!(wide.len(), 8);
        assert_eq!(&wide[..2], &[255, 255]);
        assert_eq!(&wide[2..4], &[0, 0]);
    }

    #[test]
    fn into_raw_returns_the_plane() {
        let img = Image::from_rgba8(1, 2, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(img.into_raw(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn tight_rows_drops_padding() {
        let plane = VideoPlane {
            stride: 4,
            data: vec![1, 2, 3, 9, 4, 5, 6, 9],
        };
        assert_eq!(tight_rows(&plane, 3, 2), vec![1, 2, 3, 4, 5, 6]);
    }
}
