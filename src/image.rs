//! [`Image`]: a decoded picture in its native layout, with the raw
//! `Vec<u8>` views every Rust image consumer expects.

use std::time::Duration;

use oxideav_core::{
    CodecId, CodecParameters, ColorSignal, MediaType, PixelFormat, TimeBase, VideoFrame, VideoPlane,
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
    stream: usize,
    /// Presentation timestamp and duration in `time_base` ticks, as the
    /// container's packet carried them (`None` when it did not).
    pts: Option<i64>,
    duration: Option<i64>,
    time_base: TimeBase,
    /// The stream's `start_time`, subtracted from `pts` for
    /// [`timestamp`](Self::timestamp).
    start: i64,
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
            stream: 0,
            pts: None,
            duration: None,
            time_base: TimeBase::MILLIS,
            start: 0,
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
            stream: 0,
            pts: None,
            duration: None,
            time_base: TimeBase::MILLIS,
            start: 0,
        })
    }

    /// Build an image from planes in `format`'s plane order. Each plane
    /// must have `stride >= ` its tight row length and enough data for
    /// its rows (`PixelFormat::plane_dimensions` /
    /// `plane_row_bytes`); the plane count must match. Side-channel
    /// records (palette, colour signal) attached to the planes are kept.
    pub fn from_planes(
        width: u32,
        height: u32,
        format: PixelFormat,
        planes: Vec<VideoPlane>,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::invalid(format!(
                "image geometry {width}x{height} has a zero side"
            )));
        }
        let frame = VideoFrame { pts: None, planes };
        let n = frame.image_plane_count();
        if n != format.plane_count() {
            return Err(Error::invalid(format!(
                "{n} image plane(s) supplied but {format:?} has {}",
                format.plane_count()
            )));
        }
        for (p, plane) in frame.image_planes().iter().enumerate() {
            let row = format
                .plane_row_bytes(p, width)
                .ok_or_else(|| Error::invalid("plane geometry overflows usize"))?;
            let (_, rows) = format
                .plane_dimensions(p, width, height)
                .ok_or_else(|| Error::invalid("plane index out of range"))?;
            if plane.stride < row {
                return Err(Error::invalid(format!(
                    "plane {p}: stride {} is below the {row}-byte row of a {width}-pixel {format:?} row",
                    plane.stride
                )));
            }
            let need = (rows as usize - 1)
                .checked_mul(plane.stride)
                .and_then(|v| v.checked_add(row))
                .ok_or_else(|| Error::invalid("plane geometry overflows usize"))?;
            if plane.data.len() < need {
                return Err(Error::invalid(format!(
                    "plane {p}: {} bytes supplied, {need} needed for {rows} rows",
                    plane.data.len()
                )));
            }
        }
        let mut params = CodecParameters::video(CodecId::new("rawvideo"));
        params.width = Some(width);
        params.height = Some(height);
        params.pixel_format = Some(format);
        Self::from_video_frame(frame, &params)
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

    /// Display delay for this picture inside a sequence or animation.
    ///
    /// The rule, applied by [`crate::open`] and friends from the
    /// stream's timing (packet `duration` and `pts` in the stream
    /// `time_base`): the packet's own `duration` when the demuxer set
    /// one; else the gap to the next picture of the same stream; for the
    /// last picture without a duration, the previous picture's delay.
    /// A still (one picture, no duration) has `None`.
    pub fn delay(&self) -> Option<Duration> {
        self.delay
    }

    /// Presentation time of this picture from the start of its stream,
    /// when the container timestamped it.
    pub fn timestamp(&self) -> Option<Duration> {
        let pts = self.pts?;
        ticks_to_duration(pts.saturating_sub(self.start), self.time_base)
    }

    /// Index of the container stream this picture came from (0 for
    /// stills and for caller-built images).
    pub fn stream(&self) -> usize {
        self.stream
    }

    /// Raw timing as decoded: `(pts, duration)` in
    /// [`time_base`](Self::time_base) ticks.
    pub fn raw_timing(&self) -> (Option<i64>, Option<i64>) {
        (self.pts, self.duration)
    }

    /// Time base of [`raw_timing`](Self::raw_timing).
    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    pub(crate) fn set_timing(
        &mut self,
        stream: usize,
        time_base: TimeBase,
        start: i64,
        pts: Option<i64>,
        duration: Option<i64>,
    ) {
        self.stream = stream;
        self.time_base = time_base;
        self.start = start;
        self.pts = pts;
        self.duration = duration;
    }

    pub(crate) fn set_delay(&mut self, delay: Option<Duration>) {
        self.delay = delay;
    }

    /// Everything but the frame and its parameters.
    fn clone_timing(&self) -> Image {
        Image {
            frame: VideoFrame {
                pts: None,
                planes: Vec::new(),
            },
            params: CodecParameters::video(CodecId::new("rawvideo")),
            delay: self.delay,
            index: self.index,
            stream: self.stream,
            pts: self.pts,
            duration: self.duration,
            time_base: self.time_base,
            start: self.start,
        }
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
            ..self.clone_timing()
        })
    }

    /// Tightly packed `Gray8` bytes (`width × height`): luma of colour
    /// sources, palette expanded first.
    pub fn to_gray8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Gray8)
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

    /// The `w × h` window at `(x, y)` as a new image in the same layout,
    /// with the palette, colour signal and significant-bits records
    /// carried over. Works on every layout `PixelFormat` describes with
    /// whole bytes per sample position; for chroma-subsampled layouts
    /// `x` and `y` must sit on the chroma grid (even for 4:2:0), else
    /// `Unsupported`. Bit-packed mono and packed 4:2:2 macropixel
    /// layouts are `Unsupported` (convert first). A window outside the
    /// picture is `InvalidData`.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Image> {
        let (width, height, format) = (self.width(), self.height(), self.format());
        if w == 0 || h == 0 {
            return Err(Error::invalid("crop window has a zero side"));
        }
        let (Some(xe), Some(ye)) = (x.checked_add(w), y.checked_add(h)) else {
            return Err(Error::invalid("crop window overflows u32"));
        };
        if xe > width || ye > height {
            return Err(Error::invalid(format!(
                "crop {w}x{h}+{x}+{y} exceeds the {width}x{height} picture"
            )));
        }
        if format.bits_per_pixel_approx() < 8
            || matches!(format, PixelFormat::Yuyv422 | PixelFormat::Uyvy422)
        {
            return Err(Error::unsupported(format!(
                "crop of {format:?} (sub-byte samples or macropixels); convert first"
            )));
        }
        let sub = format.chroma_subsampling();
        if let Some((ssx, ssy)) = sub {
            if x % (1 << ssx) != 0 || y % (1 << ssy) != 0 {
                return Err(Error::unsupported(format!(
                    "crop origin ({x}, {y}) is not on the chroma grid of {format:?} \
                     ({}x{} pixels)",
                    1 << ssx,
                    1 << ssy
                )));
            }
        }
        let mut planes = Vec::with_capacity(format.plane_count());
        for (p, plane) in self.frame.image_planes().iter().enumerate() {
            let (ssx, ssy) = match (sub, p) {
                (Some(f), 1 | 2) => f,
                _ => (0, 0),
            };
            // Bytes per sample position: one position wide at this
            // plane's horizontal subsampling.
            let bpp = format
                .plane_row_bytes(p, 1 << ssx)
                .ok_or_else(|| Error::invalid("plane index out of range"))?;
            let px = (x >> ssx) as usize;
            let py = (y >> ssy) as usize;
            let pw = w.div_ceil(1 << ssx) as usize;
            let ph = h.div_ceil(1 << ssy) as usize;
            let row = pw * bpp;
            let mut data = Vec::with_capacity(row * ph);
            for r in 0..ph {
                let start = (py + r) * plane.stride + px * bpp;
                let src = plane.data.get(start..start + row).ok_or_else(|| {
                    Error::invalid(format!("plane {p} is too short for the crop window"))
                })?;
                data.extend_from_slice(src);
            }
            planes.push(VideoPlane { stride: row, data });
        }
        let mut frame = VideoFrame {
            pts: self.frame.pts,
            planes,
        };
        if let Some(pal) = self.frame.palette() {
            frame.set_palette(pal.to_vec());
        }
        if let Some(bits) = self.frame.significant_bits() {
            frame.set_significant_bits(bits.to_vec());
        }
        if let Some(sig) = self.frame.color_signal() {
            frame.set_color_signal(sig);
        }
        let mut params = self.params.clone();
        params.width = Some(w);
        params.height = Some(h);
        Ok(Image {
            frame,
            params,
            ..self.clone_timing()
        })
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

/// `ticks` of `tb` as a `Duration`; `None` for negative ticks or an
/// invalid time base.
pub(crate) fn ticks_to_duration(ticks: i64, tb: TimeBase) -> Option<Duration> {
    if ticks < 0 || tb.num() <= 0 || tb.den() <= 0 {
        return None;
    }
    let nanos = (ticks as i128) * (tb.num() as i128) * 1_000_000_000 / (tb.den() as i128);
    Some(Duration::from_nanos(nanos.try_into().ok()?))
}

/// `d` in `tb` ticks, rounded to nearest; at least 1 for a non-zero
/// duration so no picture collapses onto the next.
pub(crate) fn duration_to_ticks(d: Duration, tb: TimeBase) -> i64 {
    if tb.num() <= 0 || tb.den() <= 0 {
        return 0;
    }
    let num = tb.num() as i128 * 1_000_000_000;
    let ticks = (d.as_nanos() as i128 * tb.den() as i128 + num / 2) / num;
    let ticks = ticks.clamp(0, i64::MAX as i128) as i64;
    if ticks == 0 && !d.is_zero() {
        1
    } else {
        ticks
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
    fn tick_conversions_round_trip() {
        let ms = TimeBase::new(1, 1000);
        assert_eq!(
            ticks_to_duration(1500, ms),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(ticks_to_duration(-1, ms), None);
        assert_eq!(duration_to_ticks(Duration::from_millis(70), ms), 70);
        let cs = TimeBase::new(1, 100);
        assert_eq!(duration_to_ticks(Duration::from_millis(70), cs), 7);
        assert_eq!(duration_to_ticks(Duration::from_millis(74), cs), 7);
        assert_eq!(duration_to_ticks(Duration::from_millis(75), cs), 8);
        assert_eq!(duration_to_ticks(Duration::from_millis(1), cs), 1);
        assert_eq!(duration_to_ticks(Duration::ZERO, cs), 0);
        let ntsc = TimeBase::new(1001, 30000);
        assert_eq!(
            duration_to_ticks(ticks_to_duration(3, ntsc).unwrap(), ntsc),
            3
        );
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
