//! Saving: encoder + muxer through the registries, by format name or
//! file extension.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxideav_core::{
    CodecId, CodecParameters, Frame, MediaType, OptionField, Packet, PixelFormat, RuntimeContext,
    StreamInfo, TimeBase,
};
use oxideav_pixfmt::supports;

use crate::error::{Error, Result};
use crate::image::{duration_to_ticks, Image};

/// Knobs for [`encode`] / [`encode_frames`] / [`save`].
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct SaveOptions {
    /// Advisory quality `0..=100`, handed to the encoder as its
    /// `"quality"` option when its declared schema has one
    /// ([`encoder_options`]) and ignored otherwise — lossless encoders
    /// have no such knob and would reject an unknown option.
    pub quality: Option<u8>,
    /// Force the layout handed to the encoder. `None` tries the image's
    /// own layout first and then a ladder of common ones.
    pub pixel_format: Option<PixelFormat>,
    /// Force a codec id instead of the container's default.
    pub codec: Option<String>,
    /// Extra encoder options as `(name, value)` pairs, forwarded
    /// verbatim; names come from [`encoder_options`], and an encoder
    /// rejects names it does not declare.
    pub options: Vec<(String, String)>,
    /// Time base of the written stream and its packets. `None` picks
    /// milliseconds, except for containers whose muxer defines its own
    /// tick (APNG: 1/100 s).
    pub time_base: Option<TimeBase>,
    /// Delay used for a picture that carries none in a multi-picture
    /// encode (`None` = 100 ms).
    pub default_delay: Option<Duration>,
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

    /// Time base for the written stream (see the field).
    pub fn with_time_base(mut self, tb: TimeBase) -> Self {
        self.time_base = Some(tb);
        self
    }

    /// Delay for pictures that carry none (see the field).
    pub fn with_default_delay(mut self, d: Duration) -> Self {
        self.default_delay = Some(d);
        self
    }
}

/// Default delay for a picture without one in a multi-picture encode.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(100);

/// The option schema of the encoder [`encode`] would use for `format`
/// (with `SaveOptions::codec` honoured): the names
/// [`SaveOptions::with_option`] accepts, with kinds, defaults and help
/// text. Empty when the encoder declares none.
pub fn encoder_options(
    ctx: &RuntimeContext,
    format: &str,
    opts: &SaveOptions,
) -> Result<&'static [OptionField]> {
    let target = resolve_target(ctx, format)?;
    let codec_id = codec_for(ctx, &target, opts);
    if !ctx.codecs.has_encoder(&codec_id) {
        return Err(Error::unsupported(format!(
            "no registered encoder for codec '{codec_id}' ({target})"
        )));
    }
    Ok(ctx.codecs.encoder_options_schema(&codec_id).unwrap_or(&[]))
}

/// The codec [`encode`] uses for a resolved target.
fn codec_for(ctx: &RuntimeContext, target: &Target, opts: &SaveOptions) -> CodecId {
    CodecId::new(match (&opts.codec, target) {
        (Some(c), _) => c.clone(),
        (None, Target::Container(c)) => default_codec_for_container(ctx, c, opts),
        (None, Target::CodecOnly(id)) => {
            if ctx.codecs.has_encoder(&CodecId::new(id)) {
                id.clone()
            } else {
                pick_prefixed_encoder(ctx, id, opts).unwrap_or_else(|| id.clone())
            }
        }
    })
}

/// Encoder ids of the form `<name>_<variant>`, sorted.
fn prefixed_encoders(ctx: &RuntimeContext, name: &str) -> Vec<String> {
    let prefix = format!("{name}_");
    let mut v: Vec<String> = ctx
        .codecs
        .encoder_ids()
        .filter(|id| id.as_str().starts_with(&prefix))
        .map(|id| id.as_str().to_string())
        .collect();
    v.sort();
    v
}

/// Among `<name>_*` encoders: the lossless one unless a quality was
/// asked for (then a lossy one), else the first by name.
fn pick_prefixed_encoder(ctx: &RuntimeContext, name: &str, opts: &SaveOptions) -> Option<String> {
    let ids = prefixed_encoders(ctx, name);
    let want_lossy = opts.quality.is_some();
    let flagged = ids.iter().find(|id| {
        ctx.codecs
            .implementations(&CodecId::new(id.as_str()))
            .iter()
            .any(|i| {
                i.make_encoder.is_some()
                    && if want_lossy {
                        i.caps.lossy && !i.caps.lossless
                    } else {
                        i.caps.lossless
                    }
            })
    });
    flagged.or(ids.first()).cloned()
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
/// pages, as the container defines).
///
/// Timing: every picture's [`Image::delay`] (or
/// [`SaveOptions::default_delay`]) is rescaled to the stream time base
/// ([`SaveOptions::time_base`], default milliseconds; 1/100 s for
/// `png`, the APNG delay unit); frame `pts` are the cumulative delays.
/// When the encoder returns exactly one packet per picture, the gateway
/// stamps `pts` / `dts` / `duration` / `time_base` on the packets so
/// that `decode(encode_frames(..))` reports the same delays regardless
/// of what the codec did with the frame `pts`; otherwise the encoder's
/// packet timing is kept.
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
    let codec_id = codec_for(ctx, &target, opts);
    if !ctx.codecs.has_encoder(&codec_id) {
        return Err(Error::unsupported(format!(
            "no registered encoder for codec '{codec_id}' ({target})"
        )));
    }

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
    }

    let candidates = candidates(first.format(), opts);
    let mut first_err: Option<Error> = None;
    let mut same = true;
    let mut attempts: Vec<String> = Vec::new();
    for dst in candidates {
        match encode_attempt(ctx, images, &target, &codec_id, dst, opts) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                let text = e.to_string();
                if let Some(f) = &first_err {
                    same &= f.to_string() == text;
                }
                attempts.push(format!("{dst:?}: {text}"));
                first_err.get_or_insert(e);
            }
        }
    }
    let first_err = first_err.unwrap_or_else(|| Error::unsupported("no input layout to try"));
    if same {
        // One cause, whatever the layout (unknown option, muxer refusing
        // the stream, …): keep it as is.
        return Err(first_err);
    }
    // Every layout of the ladder failed differently: say what each hit.
    Err(Error::unsupported(format!(
        "no input layout was accepted by encoder '{codec_id}' + {target} — {}",
        attempts.join("; ")
    )))
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
    // The encoder may be the codec itself (`qoi`) or a prefixed
    // variant (`webp` decodes, `webp_vp8l` / `webp_vp8` encode).
    for name in by_ext.into_iter().chain(std::iter::once(lower.as_str())) {
        if ctx.containers.demuxer_names().any(|n| n == name) {
            continue;
        }
        if ctx.codecs.has_encoder(&CodecId::new(name)) || !prefixed_encoders(ctx, name).is_empty() {
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

/// The payload codec a container carries by default: the few containers
/// whose codec is named differently (when that encoder is registered),
/// else the codec of the same name (png, bmp, tiff, heif, …).
fn default_codec_for_container(
    ctx: &RuntimeContext,
    container: &str,
    opts: &SaveOptions,
) -> String {
    let known = match container {
        "jpeg" | "jpg" | "mjpeg" | "mjpeg-raw" => Some("mjpeg"),
        "jp2" | "jph" => Some("jpeg2000"),
        "jxs" => Some("jpegxs"),
        "dcx" => Some("pcx"),
        "ani" | "cur" => Some("ico"),
        "svgz" => Some("svg"),
        "y4m" => Some("rawvideo"),
        other if other.starts_with("iff_") => Some("ilbm"),
        _ => None,
    };
    match known {
        // A tag-only `jpeg` id from another crate must not shadow mjpeg.
        Some(c) if ctx.codecs.has_encoder(&CodecId::new(c)) => c.to_string(),
        // No encoder named like the container: a `<container>_<variant>`
        // family (webp → webp_vp8l / webp_vp8) picks lossless unless a
        // quality was asked for.
        _ if !ctx.codecs.has_encoder(&CodecId::new(container)) => {
            pick_prefixed_encoder(ctx, container, opts).unwrap_or_else(|| container.to_string())
        }
        _ => container.to_string(),
    }
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
        let declared = ctx
            .codecs
            .encoder_options_schema(codec_id)
            .is_some_and(|schema| schema.iter().any(|f| f.name == "quality"));
        if declared {
            params.options = params.options.set("quality", q.to_string());
        }
    }
    for (k, v) in &opts.options {
        params.options = params.options.set(k.clone(), v.clone());
    }

    let time_base = opts.time_base.unwrap_or_else(|| match target {
        // The APNG muxer reads `duration` in its 1/100 s delay unit.
        Target::Container(c) if c == "png" || c == "apng" => TimeBase::new(1, 100),
        _ => TimeBase::MILLIS,
    });
    let default_delay = opts.default_delay.unwrap_or(DEFAULT_DELAY);
    let mut encoder = ctx.codecs.first_encoder(&params)?;
    let mut packets: Vec<Packet> = Vec::new();
    let mut pts: i64 = 0;
    let mut ticks: Vec<(i64, i64)> = Vec::with_capacity(images.len());
    for img in images {
        let converted = img.to_format(dst)?;
        let (mut frame, _) = converted.into_video_frame();
        frame.pts = Some(pts);
        let delay = if images.len() == 1 {
            img.delay().unwrap_or(Duration::ZERO)
        } else {
            img.delay().unwrap_or(default_delay)
        };
        let d = duration_to_ticks(delay, time_base);
        ticks.push((pts, d));
        pts += d;
        encoder.send_frame(&Frame::Video(frame))?;
        drain_packets_between_frames(encoder.as_mut(), &mut packets);
    }
    encoder.flush()?;
    drain_packets(encoder.as_mut(), &mut packets)?;
    if packets.is_empty() {
        return Err(Error::unsupported(format!(
            "encoder '{codec_id}' produced no packets for {dst:?} input"
        )));
    }
    if packets.len() == 1 && images.len() > 1 && matches!(target, Target::Container(_)) {
        // The encoder folded every picture into one packet (png writes
        // an APNG itself with a uniform delay). Containers whose muxer
        // assembles the multi-picture file from one packet per picture
        // need exactly that, so encode each picture on its own encoder
        // and let the muxer do the assembly with our timing.
        packets.clear();
        for img in images {
            let mut enc = ctx.codecs.first_encoder(&params)?;
            let (mut frame, _) = img.to_format(dst)?.into_video_frame();
            frame.pts = Some(0);
            enc.send_frame(&Frame::Video(frame))?;
            enc.flush()?;
            let before = packets.len();
            drain_packets(enc.as_mut(), &mut packets)?;
            if packets.len() != before + 1 {
                return Err(Error::unsupported(format!(
                    "encoder '{codec_id}' produced {} packet(s) for one picture",
                    packets.len() - before
                )));
            }
        }
    }
    if packets.len() == images.len() {
        for (pkt, (p, d)) in packets.iter_mut().zip(&ticks) {
            pkt.time_base = time_base;
            pkt.pts = Some(*p);
            pkt.dts = Some(*p);
            pkt.duration = (*d > 0 || images.len() > 1).then_some(*d);
        }
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

/// Collect what an encoder has ready between two frames. Errors here
/// are not fatal: some image encoders answer `receive_packet` before
/// `flush` with an error other than `NeedMore` (they queue nothing
/// until the end), and anything real surfaces again after `flush`.
fn drain_packets_between_frames(enc: &mut dyn oxideav_core::Encoder, out: &mut Vec<Packet>) {
    while let Ok(p) = enc.receive_packet() {
        out.push(p);
    }
}

/// Collect everything a flushed encoder has. `NeedMore` / `Eof` end
/// the stream; any other error ends it too once the encode has
/// delivered at least one packet (an encoder that errors instead of
/// saying `Eof` after its last packet has still produced its output),
/// and is fatal when nothing came out at all.
fn drain_packets(enc: &mut dyn oxideav_core::Encoder, out: &mut Vec<Packet>) -> Result<()> {
    loop {
        match enc.receive_packet() {
            Ok(p) => out.push(p),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(_) if !out.is_empty() => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}
