//! [`Image`]: a decoded picture in its native layout, with the raw
//! `Vec<u8>` views every Rust image consumer expects.

use std::time::Duration;

use oxideav_core::{
    BlobKind, CodecId, CodecParameters, ColorSignal, MediaType, MetadataBlob, PixelFormat,
    TimeBase, VideoFrame, VideoPlane,
};
use oxideav_pixfmt::{convert_with, supports_direct, ConvertContext, ConvertOptions, FrameInfo};

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
///
/// Metadata rides along at two levels (see *Metadata* in the README):
/// stream-level blobs on the parameters ([`stream_blobs`](Self::stream_blobs),
/// the file's ICC profile / Exif / XMP as the demuxer reported them) and
/// per-picture blobs lifted off the frame's side-channel record
/// ([`frame_blobs`](Self::frame_blobs)). The typed accessors
/// ([`icc`](Self::icc), [`exif`](Self::exif), [`xmp`](Self::xmp),
/// [`iptc`](Self::iptc), [`blob`](Self::blob)) apply the precedence
/// rule: a frame blob overrides a stream blob of the same kind.
#[derive(Clone, Debug)]
pub struct Image {
    frame: VideoFrame,
    params: CodecParameters,
    /// Per-picture blobs, lifted off `frame`'s side-channel record at
    /// construction and re-attached by `into_video_frame`.
    blobs: Vec<MetadataBlob>,
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
    /// implies (side-channel records excluded). A metadata-blobs record
    /// on the frame is lifted into [`frame_blobs`](Self::frame_blobs);
    /// `params.blobs` become the [`stream_blobs`](Self::stream_blobs).
    pub fn from_video_frame(mut frame: VideoFrame, params: &CodecParameters) -> Result<Self> {
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
        let blobs = frame.take_blobs();
        Ok(Self {
            frame,
            params: params.clone(),
            blobs,
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
            blobs: Vec::new(),
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

    /// An indexed (`Pal8`) picture: one byte per pixel into `palette`
    /// (at most 256 RGBA entries; alpha `255` = opaque). The palette
    /// is attached as the frame's side-channel record, plus the
    /// palette-alpha record when any entry is not opaque, so
    /// [`palette_rgba`](Self::palette_rgba) and the RGBA views honour
    /// the entries' alpha.
    pub fn from_indexed(
        width: u32,
        height: u32,
        indices: Vec<u8>,
        palette: &[[u8; 4]],
    ) -> Result<Self> {
        if palette.is_empty() || palette.len() > 256 {
            return Err(Error::invalid(format!(
                "a Pal8 palette has 1..=256 entries, {} given",
                palette.len()
            )));
        }
        let mut img = Self::from_raw(width, height, PixelFormat::Pal8, indices)?;
        let rgb = palette.iter().flat_map(|e| [e[0], e[1], e[2]]).collect();
        img.frame.set_palette(rgb);
        // No record means opaque: attach one only when an entry is not.
        if palette.iter().any(|e| e[3] != u8::MAX) {
            img.frame
                .set_palette_alpha(palette.iter().map(|e| e[3]).collect());
        }
        Ok(img)
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

    /// The palette-alpha record next to the palette: byte `i` is the
    /// alpha of entry `i` (`0` transparent, `255` opaque); it may be
    /// shorter than the palette, and every entry it does not cover is
    /// opaque. `None` without a palette or without the record.
    pub fn palette_alpha(&self) -> Option<&[u8]> {
        self.frame.palette_alpha()
    }

    /// Palette entry `index` as RGBA — alpha `255` when no alpha record
    /// covers it; `None` when the palette does not reach `index`.
    pub fn palette_rgba(&self, index: u8) -> Option<[u8; 4]> {
        self.frame.palette_rgba(index)
    }

    /// Every palette entry as RGBA (alpha filled in per
    /// [`palette_rgba`](Self::palette_rgba)); `None` without a palette.
    pub fn palette_entries(&self) -> Option<Vec<[u8; 4]>> {
        let n = self.frame.palette()?.len() / 3;
        Some(
            (0..n.min(256))
                .filter_map(|i| self.frame.palette_rgba(i as u8))
                .collect(),
        )
    }

    /// Replace the palette with RGBA entries (both side-channel
    /// records). Meaningful for `Pal8` pictures only; harmless elsewhere.
    pub fn with_palette_rgba(mut self, palette: &[[u8; 4]]) -> Self {
        self.frame.set_palette_rgba(palette);
        self
    }

    /// Replace the palette (packed RGB triplets), leaving any
    /// palette-alpha record in place.
    pub fn with_palette(mut self, rgb: Vec<u8>) -> Self {
        self.frame.set_palette(rgb);
        self
    }

    /// Attach (or replace) the palette-alpha record: one byte per
    /// entry, in entry order, possibly shorter than the palette (the
    /// rest is opaque). Empty removes the record.
    pub fn set_palette_alpha(&mut self, alpha: Vec<u8>) {
        self.frame.set_palette_alpha(alpha);
    }

    /// Builder form of [`set_palette_alpha`](Self::set_palette_alpha).
    pub fn with_palette_alpha(mut self, alpha: Vec<u8>) -> Self {
        self.frame.set_palette_alpha(alpha);
        self
    }

    /// True when any palette entry is not fully opaque — the picture
    /// has transparency even though `Pal8` itself has no alpha channel.
    pub fn has_palette_alpha(&self) -> bool {
        self.frame
            .palette_alpha()
            .is_some_and(|a| a.iter().any(|&v| v != u8::MAX))
    }

    /// True when the picture carries transparency: an alpha channel in
    /// its layout, or a palette with a non-opaque entry
    /// ([`has_palette_alpha`](Self::has_palette_alpha)).
    pub fn has_alpha(&self) -> bool {
        self.format().has_alpha() || self.has_palette_alpha()
    }

    // ---- metadata ---------------------------------------------------------

    /// Per-picture metadata blobs (the frame's own record: a page's
    /// Exif, a burst item's profile). Empty for most stills, whose
    /// metadata is stream-level ([`stream_blobs`](Self::stream_blobs)).
    pub fn frame_blobs(&self) -> &[MetadataBlob] {
        &self.blobs
    }

    /// Stream-level metadata blobs: the file's ICC profile / Exif / XMP
    /// as the demuxer put them on the stream's `CodecParameters`.
    pub fn stream_blobs(&self) -> &[MetadataBlob] {
        self.params.blobs()
    }

    /// The effective blobs of this picture: every frame blob, then the
    /// stream blobs of kinds no frame blob carries — a frame blob
    /// overrides the stream blob(s) of the same kind.
    pub fn blobs(&self) -> Vec<&MetadataBlob> {
        let mut out: Vec<&MetadataBlob> = self.blobs.iter().collect();
        for b in self.params.blobs() {
            if !self.blobs.iter().any(|f| f.kind == b.kind) {
                out.push(b);
            }
        }
        out
    }

    /// The first effective blob of `kind` (frame level first, then
    /// stream level).
    pub fn blob(&self, kind: &BlobKind) -> Option<&MetadataBlob> {
        self.blobs
            .iter()
            .find(|b| b.is(kind))
            .or_else(|| self.params.blob(kind))
    }

    /// Every effective blob of `kind`: the frame's when it has any of
    /// that kind, else the stream's.
    pub fn blobs_of(&self, kind: &BlobKind) -> Vec<&MetadataBlob> {
        let frame: Vec<&MetadataBlob> = self.blobs.iter().filter(|b| b.is(kind)).collect();
        if !frame.is_empty() {
            return frame;
        }
        self.params.blobs().iter().filter(|b| b.is(kind)).collect()
    }

    /// The ICC profile (`BlobKind::ICC` payload: a complete profile
    /// file), per the precedence of [`blob`](Self::blob).
    pub fn icc(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::ICC).map(|b| b.data.as_slice())
    }

    /// The Exif block (`BlobKind::EXIF` payload: TIFF-structured, from
    /// the byte-order mark).
    pub fn exif(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::EXIF).map(|b| b.data.as_slice())
    }

    /// The XMP packet (`BlobKind::XMP` payload: UTF-8 RDF/XML).
    pub fn xmp(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::XMP).map(|b| b.data.as_slice())
    }

    /// The IPTC-IIM datasets (`BlobKind::IPTC`).
    pub fn iptc(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::IPTC).map(|b| b.data.as_slice())
    }

    /// True when any blob is attached at either level.
    pub fn has_metadata(&self) -> bool {
        !self.blobs.is_empty() || !self.params.blobs().is_empty()
    }

    /// Append a per-picture blob (frame level; it then overrides a
    /// stream blob of the same kind and travels with the frame into an
    /// encoder).
    pub fn push_blob(&mut self, blob: MetadataBlob) {
        self.blobs.push(blob);
    }

    /// Builder form of [`push_blob`](Self::push_blob).
    pub fn with_blob(mut self, blob: MetadataBlob) -> Self {
        self.blobs.push(blob);
        self
    }

    /// Replace the per-picture blobs.
    pub fn set_blobs(&mut self, blobs: Vec<MetadataBlob>) {
        self.blobs = blobs;
    }

    /// Builder form of [`set_blobs`](Self::set_blobs).
    pub fn with_blobs(mut self, blobs: Vec<MetadataBlob>) -> Self {
        self.blobs = blobs;
        self
    }

    /// Replace the stream-level blobs (what a demuxer would have put on
    /// the stream; [`crate::encode`] hands them to the encoder's
    /// parameters).
    pub fn with_stream_blobs(mut self, blobs: Vec<MetadataBlob>) -> Self {
        self.params.blobs = blobs;
        self
    }

    /// Detach and return the per-picture blobs.
    pub fn take_blobs(&mut self) -> Vec<MetadataBlob> {
        std::mem::take(&mut self.blobs)
    }

    /// Keep only the blobs (both levels) `keep` accepts.
    pub fn retain_blobs(&mut self, mut keep: impl FnMut(&MetadataBlob) -> bool) {
        self.blobs.retain(|b| keep(b));
        self.params.blobs.retain(|b| keep(b));
    }

    /// Drop every blob at both levels (the container's string metadata
    /// on [`crate::ImageFile`] is untouched).
    pub fn strip_metadata(&mut self) {
        self.blobs.clear();
        self.params.blobs.clear();
    }

    /// Builder form of [`strip_metadata`](Self::strip_metadata).
    pub fn without_metadata(mut self) -> Self {
        self.strip_metadata();
        self
    }

    /// Display delay for this picture inside a sequence or animation.
    ///
    /// The rule, applied by [`crate::open`] and friends from the
    /// stream's timing (packet `duration` and `pts` in the stream
    /// `time_base`): the packet's own `duration` when the demuxer set
    /// one; else the gap to the next picture of the same stream; for the
    /// last picture without a duration, the previous picture's delay.
    /// A stream with a single picture is a still and has `None`, even
    /// when its demuxer stamped a nominal duration.
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

    /// Everything but the frame and its parameters (the per-picture
    /// blobs included: they describe the picture, whatever its layout).
    fn clone_timing(&self) -> Image {
        Image {
            frame: VideoFrame {
                pts: None,
                planes: Vec::new(),
            },
            params: CodecParameters::video(CodecId::new("rawvideo")),
            blobs: self.blobs.clone(),
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

    /// The underlying frame: pixel planes plus the palette, palette
    /// alpha, colour-signal and significant-bits records. Its
    /// metadata-blobs record was lifted into
    /// [`frame_blobs`](Self::frame_blobs) and is re-attached by
    /// [`into_video_frame`](Self::into_video_frame).
    pub fn frame(&self) -> &VideoFrame {
        &self.frame
    }

    /// The stream parameters describing the frame (their `blobs` are the
    /// [`stream_blobs`](Self::stream_blobs)).
    pub fn params(&self) -> &CodecParameters {
        &self.params
    }

    /// Hand the frame back to the framework (pipeline, encoder, filter),
    /// with the per-picture blobs re-attached as its side-channel record.
    pub fn into_video_frame(self) -> (VideoFrame, CodecParameters) {
        (self.frame.with_blobs(self.blobs), self.params)
    }

    /// Convert to another layout through `oxideav-pixfmt`. Same format
    /// returns a clone. The colour signal used for YCbCr ↔ RGB is the
    /// frame's own record, else the stream-level one from the container
    /// (what [`color_signal`](Self::color_signal) reports), else
    /// pixfmt's defaults. The per-picture blobs are carried over.
    ///
    /// Palette alpha: a `Pal8` source with a palette-alpha record asked
    /// for a layout with alpha is expanded here through
    /// [`palette_rgba`](Self::palette_rgba) (transparent entries yield
    /// alpha `0`, uncovered ones `255`) and converted on from `Rgba`; a
    /// target without alpha drops it (no compositing — the entry's RGB
    /// is used as is), as [`to_rgb8`](Self::to_rgb8) documents.
    pub fn to_format(&self, dst: PixelFormat) -> Result<Image> {
        let src = self.format();
        if src == dst {
            return Ok(self.clone());
        }
        let mut cx = ConvertContext::new();
        if self.frame.color_signal().is_none() {
            if let Some(sig) = self.color_signal() {
                cx = cx.with_signal(sig);
            }
        }
        let (w, h) = (self.width(), self.height());
        let mut cur = src;
        let mut converted: Option<VideoFrame> = None;
        if src == PixelFormat::Pal8 && dst.has_alpha() && self.frame.palette_alpha().is_some() {
            converted = Some(self.expand_palette_rgba()?);
            cur = PixelFormat::Rgba;
        }
        // A deep source asked for a 16-bit view walks the deep pivots
        // when pixfmt has no single-step route, so no hop narrows to
        // eight bits (pixfmt's own staging would pivot through Gray8 for
        // Gray16Le → Rgb48Le, say).
        let hops = deep_route(cur, dst).unwrap_or_default();
        for next in hops.into_iter().chain(std::iter::once(dst)) {
            if next == cur {
                continue;
            }
            let info = FrameInfo::new(cur, w, h);
            let from = converted.as_ref().unwrap_or(&self.frame);
            converted = Some(convert_with(
                from,
                info,
                next,
                &ConvertOptions::default(),
                &cx,
            )?);
            cur = next;
        }
        let converted = converted.ok_or_else(|| Error::invalid("conversion produced no frame"))?;
        let mut params = self.params.clone();
        params.pixel_format = Some(dst);
        params.media_type = MediaType::Video;
        Ok(Image {
            frame: converted,
            params,
            ..self.clone_timing()
        })
    }

    /// `Pal8` → tight `Rgba` through the frame's palette and
    /// palette-alpha records. An index past the palette is opaque black.
    fn expand_palette_rgba(&self) -> Result<VideoFrame> {
        let (w, h) = (self.width() as usize, self.height() as usize);
        let plane = self
            .frame
            .image_planes()
            .first()
            .ok_or_else(|| Error::invalid("Pal8 frame has no plane"))?;
        if self.frame.palette().is_none() {
            return Err(Error::invalid("Pal8 frame carries no palette"));
        }
        let table: Vec<[u8; 4]> = (0..=255u8)
            .map(|i| self.frame.palette_rgba(i).unwrap_or([0, 0, 0, 255]))
            .collect();
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let row = plane
                .data
                .get(y * plane.stride..y * plane.stride + w)
                .ok_or_else(|| Error::invalid("Pal8 plane is too short for its geometry"))?;
            for &i in row {
                data.extend_from_slice(&table[usize::from(i)]);
            }
        }
        let mut frame = VideoFrame {
            pts: self.frame.pts,
            planes: vec![VideoPlane {
                stride: w * 4,
                data,
            }],
        };
        if let Some(sig) = self.frame.color_signal() {
            frame.set_color_signal(sig);
        }
        Ok(frame)
    }

    /// Tightly packed `Gray8` bytes (`width × height`): luma of colour
    /// sources, palette expanded first (palette alpha dropped).
    pub fn to_gray8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Gray8)
    }

    /// Tightly packed `Rgb24` bytes (`width × height × 3`). Alpha —
    /// a channel or a palette entry's alpha — is dropped, never
    /// composited: a transparent pixel yields its stored RGB.
    pub fn to_rgb8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgb24)
    }

    /// Tightly packed `Rgba` bytes (`width × height × 4`); alpha is
    /// opaque when the source has none. A `Pal8` source with a
    /// palette-alpha record yields each entry's alpha (`0` for a
    /// transparent entry, `255` for opaque and uncovered ones).
    pub fn to_rgba8(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgba)
    }

    /// Tightly packed `Rgba64Le` bytes (`width × height × 8`,
    /// little-endian 16-bit samples); see [`to_rgb16`](Self::to_rgb16)
    /// for the depth rule. Alpha is opaque (`65535`) when the source has
    /// none; palette alpha widens like any 8-bit sample.
    pub fn to_rgba16(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgba64Le)
    }

    /// Tightly packed `Rgb48Le` bytes (`width × height × 6`,
    /// little-endian 16-bit samples): the lossless 16-bit view for
    /// texture consumers. Alpha is dropped as in [`to_rgb8`](Self::to_rgb8).
    ///
    /// Depth rule (shared by [`to_rgba16`](Self::to_rgba16) and
    /// [`to_gray16`](Self::to_gray16)): a 16-bit source is returned
    /// exactly (same samples, converted between layouts without
    /// requantising); an 8-bit sample `s` widens to `s × 257` (`0 → 0`,
    /// `255 → 65535`, every value exactly representable back); 10- and
    /// 12-bit sources scale to the 16-bit full range the same way
    /// (`s × 65535 / max`). YCbCr sources go through the colour matrix
    /// at 16-bit precision. Nothing here is ever narrowed.
    pub fn to_rgb16(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Rgb48Le)
    }

    /// Tightly packed `Gray16Le` bytes (`width × height × 2`): luma of
    /// colour sources, palette expanded first; depth rule as
    /// [`to_rgb16`](Self::to_rgb16).
    pub fn to_gray16(&self) -> Result<Vec<u8>> {
        self.packed_bytes(PixelFormat::Gray16Le)
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
    /// with the palette, palette alpha, colour signal and
    /// significant-bits records and the blobs at both levels carried
    /// over. Works on every layout `PixelFormat` describes with
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
        if let Some(alpha) = self.frame.palette_alpha() {
            frame.set_palette_alpha(alpha.to_vec());
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

/// The 16-bit packed views and their alpha / grey relatives: the pivots
/// a deep-to-deep conversion may stage through without narrowing.
const DEEP_PIVOTS: [PixelFormat; 4] = [
    PixelFormat::Gray16Le,
    PixelFormat::Ya16Le,
    PixelFormat::Rgba64Le,
    PixelFormat::Rgb48Le,
];

/// Intermediate hops for `src → dst` when `dst` is a 16-bit packed view,
/// `src` is deep and pixfmt has no direct route: the shortest chain of
/// direct conversions through [`DEEP_PIVOTS`] (breadth-first, at most
/// three hops). `None` leaves the conversion to pixfmt's own staging.
fn deep_route(src: PixelFormat, dst: PixelFormat) -> Option<Vec<PixelFormat>> {
    if src == dst
        || !DEEP_PIVOTS.contains(&dst)
        || !crate::ladder::is_deep(src)
        || supports_direct(src, dst)
    {
        return None;
    }
    let mut paths: Vec<Vec<PixelFormat>> = vec![vec![]];
    for _ in 0..3 {
        let mut next = Vec::new();
        for path in &paths {
            let last = path.last().copied().unwrap_or(src);
            for &p in &DEEP_PIVOTS {
                if p == dst || path.contains(&p) || !supports_direct(last, p) {
                    continue;
                }
                let mut ext = path.clone();
                ext.push(p);
                if supports_direct(p, dst) {
                    return Some(ext);
                }
                next.push(ext);
            }
        }
        if next.is_empty() {
            return None;
        }
        paths = next;
    }
    None
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
    fn sixteen_bit_views_are_exact_or_widen_by_257() {
        // 8-bit → 16-bit: s × 257, so 128 → 32896 = 0x8080.
        let img = Image::from_rgb8(1, 1, vec![255, 0, 128]).unwrap();
        assert_eq!(img.to_rgb16().unwrap(), vec![255, 255, 0, 0, 0x80, 0x80]);
        assert_eq!(
            img.to_rgba16().unwrap(),
            vec![255, 255, 0, 0, 0x80, 0x80, 255, 255]
        );
        let grey = Image::from_raw(2, 1, PixelFormat::Gray8, vec![0, 200]).unwrap();
        assert_eq!(grey.to_gray16().unwrap(), vec![0, 0, 200, 200]);
        // 16-bit sources come back exactly, whatever the layout hop.
        let rgb16: Vec<u8> = vec![0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a];
        let deep = Image::from_raw(1, 1, PixelFormat::Rgb48Le, rgb16.clone()).unwrap();
        assert_eq!(deep.to_rgb16().unwrap(), rgb16);
        assert_eq!(
            deep.to_rgba16().unwrap(),
            vec![0x34, 0x12, 0x78, 0x56, 0xbc, 0x9a, 0xff, 0xff]
        );
        let g16 = Image::from_raw(1, 1, PixelFormat::Gray16Le, vec![0xcd, 0xab]).unwrap();
        assert_eq!(g16.to_gray16().unwrap(), vec![0xcd, 0xab]);
        assert_eq!(
            g16.to_rgb16().unwrap(),
            vec![0xcd, 0xab, 0xcd, 0xab, 0xcd, 0xab]
        );
        let rgba16 =
            Image::from_raw(1, 1, PixelFormat::Rgba64Le, vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(rgba16.to_rgb16().unwrap(), vec![1, 2, 3, 4, 5, 6]);
        // Widening then narrowing is the identity on 8-bit data.
        let wide = Image::from_raw(1, 1, PixelFormat::Rgb48Le, img.to_rgb16().unwrap()).unwrap();
        assert_eq!(wide.to_rgb8().unwrap(), vec![255, 0, 128]);
    }

    #[test]
    fn deep_routes_only_where_pixfmt_has_no_direct_step() {
        use PixelFormat as P;
        // Which deep pairs pixfmt converts directly is pixfmt's to
        // grow (release by release), so the rule is checked against
        // `supports_direct` rather than a pinned pair: no route where a
        // direct step exists, else a chain of direct steps through the
        // pivots, at most three hops.
        let check = |a: P, b: P| {
            let r = deep_route(a, b);
            if a == b || supports_direct(a, b) {
                assert_eq!(r, None, "{a:?} → {b:?}");
                return;
            }
            let r = r.unwrap_or_else(|| panic!("{a:?} → {b:?} has no route"));
            assert!(!r.is_empty() && r.len() <= 3);
            assert!(r.iter().all(|f| DEEP_PIVOTS.contains(f)));
            let mut cur = a;
            for &hop in &r {
                assert!(supports_direct(cur, hop), "{cur:?} → {hop:?}");
                cur = hop;
            }
            assert!(supports_direct(cur, b), "{cur:?} → {b:?}");
        };
        for &a in &DEEP_PIVOTS {
            for &b in &DEEP_PIVOTS {
                check(a, b);
            }
        }
        check(P::Yuv444P16Le, P::Gray16Le);
        check(P::Yuv420P10Le, P::Rgb48Le);
        // Shallow sources and non-view targets never route.
        assert_eq!(deep_route(P::Rgb24, P::Rgb48Le), None);
        assert_eq!(deep_route(P::Rgb48Le, P::Gray8), None);
        assert_eq!(deep_route(P::Rgba, P::Rgba64Le), None);
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
