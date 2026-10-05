//! Saving: encoder + muxer through the registries, by format name or
//! file extension.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Frame, MediaType, Packet, PixelFormat, RuntimeContext, StreamInfo,
    TimeBase,
};
use oxideav_pixfmt::supports;

use crate::error::{Error, Result};
use crate::image::Image;

/// Knobs for [`encode`] / [`encode_frames`] / [`save`].
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct SaveOptions {
    /// Advisory quality `0..=100`, passed to encoders that read a
    /// `"quality"` option.
    pub quality: Option<u8>,
    /// Force the layout handed to the encoder. `None` tries the image's
    /// own layout first and then a ladder of common ones.
    pub pixel_format: Option<PixelFormat>,
    /// Force a codec id instead of the container's default.
    pub codec: Option<String>,
    /// Extra encoder options as `(name, value)` pairs.
    pub options: Vec<(String, String)>,
}

impl SaveOptions {
    /// Defaults everywhere.
    pub fn new() -> Self {
        Self::default()
    }

    /// Advisory quality `0..=100`.
    pub fn with_quality(mut self, q: u8) -> Self {
        self.quality = Some(q.min(100));
        self
    }

    /// Force the encoder's input layout.
    pub fn with_pixel_format(mut self, f: PixelFormat) -> Self {
        self.pixel_format = Some(f);
        self
    }

    /// Force a codec id.
    pub fn with_codec(mut self, codec: impl Into<String>) -> Self {
        self.codec = Some(codec.into());
        self
    }

    /// Add one encoder option.
    pub fn with_option(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.push((name.into(), value.into()));
        self
    }
}

/// Encode one picture as `format` (a registered container name or a
/// file extension such as `"png"`, `"jpg"`, `"heic"`).
pub fn encode(
    ctx: &RuntimeContext,
    image: &Image,
    format: &str,
    opts: &SaveOptions,
) -> Result<Vec<u8>> {
    encode_frames(ctx, std::slice::from_ref(image), format, opts)
}

/// Encode several pictures into one file (animation, burst, sequence or
/// pages, as the container defines). Picture delays become packet
/// timestamps in milliseconds.
pub fn encode_frames(
    ctx: &RuntimeContext,
    images: &[Image],
    format: &str,
    opts: &SaveOptions,
) -> Result<Vec<u8>> {
    let first = images
        .first()
        .ok_or_else(|| Error::invalid("encode_frames: no image"))?;
    let target = resolve_target(ctx, format)?;
    let codec_name = match (&opts.codec, &target) {
        (Some(c), _) => c.clone(),
        (None, Target::Container(c)) => default_codec_for_container(ctx, c),
        (None, Target::CodecOnly(id)) => id.clone(),
    };
    let codec_id = CodecId::new(codec_name);
    if !ctx.codecs.has_encoder(&codec_id) {
        return Err(Error::unsupported(format!(
            "no registered encoder for codec '{codec_id}' ({target})"
        )));
    }

    let candidates = candidates(first.format(), opts);
    let mut last_err: Option<Error> = None;
    for dst in candidates {
        match encode_attempt(ctx, images, &target, &codec_id, dst, opts) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| Error::unsupported("no pixel-format candidate to encode")))
}

/// Encode one picture and write it to `path`; the extension picks the
/// format.
pub fn save(
    ctx: &RuntimeContext,
    image: &Image,
    path: impl AsRef<Path>,
    opts: &SaveOptions,
) -> Result<()> {
    let path = path.as_ref();
    let ext = path.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        Error::UnknownFormat(format!(
            "'{}' has no extension to derive a format from",
            path.display()
        ))
    })?;
    let bytes = encode(ctx, image, ext, opts)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Where the encoded packets go.
#[derive(Clone, Debug)]
enum Target {
    /// A registered container with a muxer.
    Container(String),
    /// A codec registered without any container: its encoder writes the
    /// whole file as one packet (qoi, webp, gif, avif, exr, … register
    /// this way).
    CodecOnly(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Container(c) => write!(f, "container '{c}'"),
            Target::CodecOnly(c) => write!(f, "codec-only format '{c}'"),
        }
    }
}

/// Map a format name or extension onto a registered container that
/// has a muxer, or onto a codec that stands alone.
fn resolve_target(ctx: &RuntimeContext, format: &str) -> Result<Target> {
    let lower = format.trim_start_matches('.').to_ascii_lowercase();
    if ctx.containers.muxer_names().any(|n| n == lower) {
        return Ok(Target::Container(lower));
    }
    let by_ext = ctx.containers.container_for_extension(&lower);
    if let Some(name) = by_ext {
        if ctx.containers.muxer_names().any(|n| n == name) {
            return Ok(Target::Container(name.to_string()));
        }
    }
    // A codec registered without a container: the format's crate hands
    // whole files to its codec, so the encoder's packet is the file.
    for name in by_ext.into_iter().chain(std::iter::once(lower.as_str())) {
        let id = CodecId::new(name);
        if !ctx.containers.demuxer_names().any(|n| n == name) && ctx.codecs.has_encoder(&id) {
            return Ok(Target::CodecOnly(name.to_string()));
        }
    }
    if let Some(name) = by_ext {
        return Err(Error::UnknownFormat(format!(
            "container '{name}' (from '{format}') has no registered muxer"
        )));
    }
    Err(Error::UnknownFormat(format!(
        "no registered container matches '{format}'"
    )))
}

/// The payload codec a container carries by default: the encoder of
/// the same name when one is registered (png, bmp, tiff, heif, …), else
/// the few containers whose codec is named differently.
fn default_codec_for_container(ctx: &RuntimeContext, container: &str) -> String {
    if ctx.codecs.has_encoder(&CodecId::new(container)) {
        return container.to_string();
    }
    match container {
        "jpeg" | "jpg" | "mjpeg" | "mjpeg-raw" => "mjpeg",
        "dcx" => "pcx",
        "ani" | "cur" => "ico",
        "svgz" => "svg",
        "y4m" => "rawvideo",
        other if other.starts_with("iff_") => "ilbm",
        other => other,
    }
    .to_string()
}

/// Encoder input layouts to try, in order: the forced one, else the
/// image's own layout followed by the common packed and planar
/// fallbacks the converter can reach from it.
fn candidates(src: PixelFormat, opts: &SaveOptions) -> Vec<PixelFormat> {
    if let Some(f) = opts.pixel_format {
        return vec![f];
    }
    let mut out = vec![src];
    for f in [
        PixelFormat::Rgba,
        PixelFormat::Rgb24,
        PixelFormat::Gray8,
        PixelFormat::Yuv444P,
        PixelFormat::Yuv420P,
        PixelFormat::Rgba64Le,
    ] {
        if !out.contains(&f) && supports(src, f) {
            out.push(f);
        }
    }
    out
}

/// One attempt in a fixed layout: convert every picture, encode, mux
/// into memory.
fn encode_attempt(
    ctx: &RuntimeContext,
    images: &[Image],
    target: &Target,
    codec_id: &CodecId,
    dst: PixelFormat,
    opts: &SaveOptions,
) -> Result<Vec<u8>> {
    let first = &images[0];
    let mut params = CodecParameters::video(codec_id.clone());
    params.width = Some(first.width());
    params.height = Some(first.height());
    params.pixel_format = Some(dst);
    if let Some(sig) = first.color_signal() {
        params.color_signal = sig;
    }
    if let Some(q) = opts.quality {
        params.options = params.options.set("quality", q.to_string());
    }
    for (k, v) in &opts.options {
        params.options = params.options.set(k.clone(), v.clone());
    }

    let time_base = TimeBase::new(1, 1000);
    let mut encoder = ctx.codecs.first_encoder(&params)?;
    let mut packets: Vec<Packet> = Vec::new();
    let mut pts: i64 = 0;
    for img in images {
        if (img.width(), img.height()) != (first.width(), first.height()) {
            return Err(Error::invalid(format!(
                "frame {} is {}x{} but the sequence is {}x{}",
                img.index(),
                img.width(),
                img.height(),
                first.width(),
                first.height()
            )));
        }
        let converted = img.to_format(dst)?;
        let (mut frame, _) = converted.into_video_frame();
        frame.pts = Some(pts);
        pts += img
            .delay()
            .map(|d| d.as_millis() as i64)
            .unwrap_or(40)
            .max(1);
        encoder.send_frame(&Frame::Video(frame))?;
        drain_packets(encoder.as_mut(), &mut packets)?;
    }
    encoder.flush()?;
    drain_packets(encoder.as_mut(), &mut packets)?;
    if packets.is_empty() {
        return Err(Error::unsupported(format!(
            "encoder '{codec_id}' produced no packets for {dst:?} input"
        )));
    }

    let container = match target {
        Target::Container(c) => c.as_str(),
        Target::CodecOnly(id) => {
            if packets.len() != 1 {
                return Err(Error::unsupported(format!(
                    "codec-only format '{id}' produced {} packets for {} picture(s); \
                     only a single-packet file can be written without a muxer",
                    packets.len(),
                    images.len()
                )));
            }
            return Ok(packets.pop().map(|p| p.data).unwrap_or_default());
        }
    };
    let mut out_params = encoder.output_params().clone();
    out_params.media_type = MediaType::Video;
    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: Some(pts),
        start_time: Some(0),
        params: out_params,
    };
    let sink = SharedCursor::default();
    let mut muxer = ctx.containers.open_muxer(
        container,
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
    )?;
    muxer.write_header()?;
    for pkt in &packets {
        muxer.write_packet(pkt)?;
    }
    muxer.write_trailer()?;
    drop(muxer);
    let bytes = sink.into_bytes();
    if bytes.is_empty() {
        return Err(Error::unsupported(format!(
            "muxer '{container}' wrote no bytes"
        )));
    }
    Ok(bytes)
}

/// An in-memory sink the muxer can own while we keep a handle to read
/// the finished bytes back.
#[derive(Clone, Default)]
struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);

impl SharedCursor {
    fn into_bytes(self) -> Vec<u8> {
        match Arc::try_unwrap(self.0) {
            Ok(m) => m
                .into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .into_inner(),
            Err(shared) => shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_ref()
                .clone(),
        }
    }
}

impl Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).flush()
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).seek(pos)
    }
}

fn drain_packets(enc: &mut dyn oxideav_core::Encoder, out: &mut Vec<Packet>) -> Result<()> {
    loop {
        match enc.receive_packet() {
            Ok(p) => out.push(p),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}
