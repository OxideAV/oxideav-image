//! Saving: encoder + muxer through the registries, by format name or
//! file extension.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::image::{duration_to_ticks, Image};
use crate::ladder;
use oxideav_core::{
    BlobKind, CodecId, CodecParameters, Frame, MediaType, MetadataBlob, OptionField, Packet,
    PixelFormat, RuntimeContext, StreamInfo, TimeBase,
};

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
    /// Blobs written into the encoder's stream parameters on top of
    /// the pictures' own (README, *Metadata*): a blob here replaces
    /// every source blob of the same kind at both levels — the stream's
    /// and every picture's frame record (set an ICC profile, swap the
    /// XMP packet); other kinds pass through untouched.
    pub blobs: Vec<MetadataBlob>,
    /// Hand no source blob to the encoder — neither the pictures'
    /// stream-level ones nor their per-picture ones. `blobs` above are
    /// still written. Default `false`.
    pub strip_metadata: bool,
    /// Source blob kinds withheld from the encoder when
    /// `strip_metadata` is off.
    pub strip_blobs: Vec<BlobKind>,
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

    /// Add a blob for the encoder's stream parameters (see the field).
    pub fn with_blob(mut self, blob: MetadataBlob) -> Self {
        self.blobs.push(blob);
        self
    }

    /// Replace the blobs for the encoder's stream parameters.
    pub fn with_blobs(mut self, blobs: Vec<MetadataBlob>) -> Self {
        self.blobs = blobs;
        self
    }

    /// Keep (`true`, the default) or withhold every source blob.
    pub fn with_metadata(mut self, keep: bool) -> Self {
        self.strip_metadata = !keep;
        self
    }

    /// Withhold every source blob from the encoder.
    pub fn without_metadata(self) -> Self {
        self.with_metadata(false)
    }

    /// Withhold these source blob kinds from the encoder.
    pub fn with_stripped_blobs<I, K>(mut self, kinds: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<BlobKind>,
    {
        self.strip_blobs = kinds.into_iter().map(Into::into).collect();
        self
    }

    /// Whether a source blob reaches the encoder.
    fn keeps(&self, blob: &MetadataBlob) -> bool {
        !self.strip_metadata && !self.strip_blobs.iter().any(|k| blob.is(k))
    }

    /// The per-picture blobs handed to the encoder with `image`'s frame:
    /// the frame's own, minus the strip rules, minus every kind
    /// `self.blobs` sets — a blob set on the options is the one that
    /// gets written, at both levels, so no frame record carries a
    /// competing blob of its kind (a frame blob would override the
    /// stream blob on reopen).
    fn frame_blobs(&self, image: &Image) -> Vec<MetadataBlob> {
        image
            .frame_blobs()
            .iter()
            .filter(|b| self.keeps(b) && !self.blobs.iter().any(|o| o.kind == b.kind))
            .cloned()
            .collect()
    }

    /// The stream-level blobs of an encoder whose first picture is
    /// `first`: the picture's stream blobs — and, for a single-picture
    /// encode (`single`), its per-picture blobs ahead of them (frame
    /// wins by kind), since a still-format encoder reads the stream
    /// parameters — filtered by the strip rules, then `self.blobs`,
    /// each replacing the source blobs of its kind.
    fn stream_blobs(&self, first: &Image, single: bool) -> Vec<MetadataBlob> {
        let mut out: Vec<MetadataBlob> = if single {
            first.blobs().into_iter().cloned().collect()
        } else {
            first.stream_blobs().to_vec()
        };
        out.retain(|b| self.keeps(b));
        for b in &self.blobs {
            out.retain(|s| s.kind != b.kind);
        }
        out.extend(self.blobs.iter().cloned());
        out
    }
}

/// How a target carries metadata blobs, from
/// [`EncodeTarget::metadata_support`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataSupport {
    /// The encoder declares nothing about blobs (no codec does today —
    /// the framework has no capability field for it). The gateway hands
    /// every blob through regardless: stream-level ones on the encoder's
    /// `CodecParameters::blobs`, per-picture ones on each frame's
    /// record. Whether the format embeds them is the encoder's.
    PassThrough,
    /// The encoder declares the kinds it writes; others are handed
    /// through but will not be embedded. Reserved for a framework
    /// capability declaration — never produced today.
    Declared(Vec<BlobKind>),
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
    Ok(resolve_target(ctx, format, opts)?.options())
}

/// Where [`encode`] / [`save`] would send a picture for `format` (a
/// registered container name or a file extension), resolved through the
/// registries without touching any pixels: the container (or that the
/// format is codec-only), the encoder, the layouts it declares and its
/// option schema. Consumers ask this before decoding a source they may
/// not be able to save.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EncodeTarget {
    format: String,
    container: Option<String>,
    codec: CodecId,
    accepted: Vec<PixelFormat>,
    options: &'static [OptionField],
}

impl EncodeTarget {
    /// The format name as resolved (lower-case, no leading dot).
    pub fn format(&self) -> &str {
        &self.format
    }

    /// Registered name of the container whose muxer writes the file;
    /// `None` for a codec-only format (the encoder's single packet is
    /// the file).
    pub fn container(&self) -> Option<&str> {
        self.container.as_deref()
    }

    /// True when no muxer is involved (see [`container`](Self::container)).
    pub fn is_codec_only(&self) -> bool {
        self.container.is_none()
    }

    /// Id of the encoder [`encode`] would create.
    pub fn codec(&self) -> &str {
        self.codec.as_str()
    }

    /// The input layouts the encoder declares
    /// (`CodecCapabilities::accepted_pixel_formats`); empty when it
    /// declares none, in which case the ladder's fixed rungs apply.
    pub fn accepted_pixel_formats(&self) -> &[PixelFormat] {
        &self.accepted
    }

    /// The encoder's declared option schema (see [`encoder_options`]).
    pub fn options(&self) -> &'static [OptionField] {
        self.options
    }

    /// The layouts [`encode`] would offer the encoder for a picture in
    /// `src`, in order (README, *Pixel-format ladder*);
    /// `SaveOptions::pixel_format` reduces it to that one layout. The
    /// source's transparency is read from its layout; for a `Pal8`
    /// picture with a transparent palette entry use
    /// [`ladder_for`](Self::ladder_for), which ranks alpha rungs first.
    pub fn ladder(&self, src: PixelFormat, opts: &SaveOptions) -> Vec<PixelFormat> {
        match opts.pixel_format {
            Some(f) => vec![f],
            None => ladder::ladder(src, &self.accepted),
        }
    }

    /// [`ladder`](Self::ladder) for an actual picture: what [`encode`]
    /// tries for it, the picture's real transparency accounted for
    /// ([`Image::is_opaque`]: palette alpha counts, an alpha channel
    /// whose samples are all full does not — such a picture is offered
    /// the opaque layout of its model first).
    pub fn ladder_for(&self, image: &Image, opts: &SaveOptions) -> Vec<PixelFormat> {
        match opts.pixel_format {
            Some(f) => vec![f],
            None => {
                ladder::ladder_with_alpha(image.format(), image.has_transparency(), &self.accepted)
            }
        }
    }

    /// How the target carries metadata blobs. Always
    /// [`MetadataSupport::PassThrough`] today: every blob the pictures
    /// or [`SaveOptions::blobs`] carry reaches the encoder; the
    /// framework has no declaration of which kinds a format embeds.
    pub fn metadata_support(&self) -> MetadataSupport {
        MetadataSupport::PassThrough
    }
}

/// Resolve `format` the way [`encode`] does, without encoding: the
/// errors are the ones `encode` would return before looking at a
/// picture (`UnknownFormat` for a name or extension no muxer serves,
/// `Unsupported` when the container's codec — or
/// `SaveOptions::codec` — has no registered encoder).
pub fn resolve_target(
    ctx: &RuntimeContext,
    format: &str,
    opts: &SaveOptions,
) -> Result<EncodeTarget> {
    let target = resolve_format(ctx, format)?;
    let codec = codec_for(ctx, &target, opts);
    if !ctx.codecs.has_encoder(&codec) {
        return Err(Error::unsupported(format!(
            "no registered encoder for codec '{codec}' ({target})"
        )));
    }
    let accepted = declared_formats(ctx, &codec);
    let options = ctx.codecs.encoder_options_schema(&codec).unwrap_or(&[]);
    let (format, container) = match target {
        Target::Container { name, requested } => (requested, Some(name)),
        Target::CodecOnly(c) => (c, None),
    };
    Ok(EncodeTarget {
        format,
        container,
        codec,
        accepted,
        options,
    })
}

/// Whether [`encode`] / [`save`] can write `format` at all (a muxer or
/// codec-only encoder is registered for the name or extension). The
/// layout question is answered when encoding: see
/// [`EncodeTarget::ladder`].
pub fn can_encode(ctx: &RuntimeContext, format: &str) -> bool {
    resolve_target(ctx, format, &SaveOptions::default()).is_ok()
}

/// The codec [`encode`] uses for a resolved target.
fn codec_for(ctx: &RuntimeContext, target: &Target, opts: &SaveOptions) -> CodecId {
    CodecId::new(match (&opts.codec, target) {
        (Some(c), _) => c.clone(),
        (None, Target::Container { name, requested }) => {
            default_codec_for_container(ctx, name, requested, opts)
        }
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
/// Geometry: pictures of one size share an output stream; every
/// distinct size (in first-appearance order) is a further stream of
/// its own, which a multi-geometry container (ICO entries, EXR parts)
/// accepts and a single-geometry one refuses — the refusal is
/// `Unsupported` naming the geometries, never a resize. A codec-only
/// format (one packet, no muxer) refuses mixed geometries the same way.
///
/// Timing: every picture's [`Image::delay`] (or
/// [`SaveOptions::default_delay`]) is rescaled to the stream time base
/// ([`SaveOptions::time_base`], default milliseconds; 1/100 s for
/// `png`, the APNG delay unit); frame `pts` are the cumulative delays
/// within each stream. When the encoder returns exactly one packet per
/// picture, the gateway stamps `pts` / `dts` / `duration` / `time_base`
/// on the packets so that `decode(encode_frames(..))` reports the same
/// delays regardless of what the codec did with the frame `pts`;
/// otherwise the encoder's packet timing is kept.
pub fn encode_frames(
    ctx: &RuntimeContext,
    images: &[Image],
    format: &str,
    opts: &SaveOptions,
) -> Result<Vec<u8>> {
    let first = images
        .first()
        .ok_or_else(|| Error::invalid("encode_frames: no image"))?;
    let target = resolve_format(ctx, format)?;
    let codec_id = codec_for(ctx, &target, opts);
    if !ctx.codecs.has_encoder(&codec_id) {
        return Err(Error::unsupported(format!(
            "no registered encoder for codec '{codec_id}' ({target})"
        )));
    }

    let groups = group_by_geometry(images);
    if groups.len() > 1 && matches!(target, Target::CodecOnly(_)) {
        return Err(Error::unsupported(format!(
            "{target} writes one picture geometry per file; {} were given ({})",
            groups.len(),
            geometries(&groups)
        )));
    }

    let candidates = candidates(ctx, &codec_id, first, opts);
    let mut first_err: Option<Error> = None;
    let mut same = true;
    let mut attempts: Vec<String> = Vec::new();
    for dst in candidates {
        match encode_attempt(ctx, images, &groups, &target, &codec_id, dst, opts) {
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

/// Pictures sharing one geometry: one output stream each.
struct Group {
    width: u32,
    height: u32,
    /// Indices into the picture slice, in order.
    members: Vec<usize>,
}

/// One group per distinct `(width, height)`, in first-appearance order.
fn group_by_geometry(images: &[Image]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (i, img) in images.iter().enumerate() {
        let (w, h) = (img.width(), img.height());
        match groups.iter_mut().find(|g| g.width == w && g.height == h) {
            Some(g) => g.members.push(i),
            None => groups.push(Group {
                width: w,
                height: h,
                members: vec![i],
            }),
        }
    }
    groups
}

fn geometries(groups: &[Group]) -> String {
    groups
        .iter()
        .map(|g| format!("{}x{}", g.width, g.height))
        .collect::<Vec<_>>()
        .join(", ")
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
    /// A registered container with a muxer. `requested` is the format
    /// name as asked (lower-cased, dot-stripped): a container serving
    /// several brands through several extensions (`heic` / `avif` /
    /// `heif` on one muxer) picks its codec from it.
    Container { name: String, requested: String },
    /// A codec registered without any container: its encoder writes the
    /// whole file as one packet (qoi, webp, gif, avif, exr, … register
    /// this way).
    CodecOnly(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Container { name, requested } if name == requested => {
                write!(f, "container '{name}'")
            }
            Target::Container { name, requested } => {
                write!(f, "container '{name}' (from '{requested}')")
            }
            Target::CodecOnly(c) => write!(f, "codec-only format '{c}'"),
        }
    }
}

/// Map a format name or extension onto a registered container that
/// has a muxer, or onto a codec that stands alone.
fn resolve_format(ctx: &RuntimeContext, format: &str) -> Result<Target> {
    let lower = format.trim_start_matches('.').to_ascii_lowercase();
    if ctx.containers.muxer_names().any(|n| n == lower) {
        return Ok(Target::Container {
            name: lower.clone(),
            requested: lower,
        });
    }
    let by_ext = ctx.containers.container_for_extension(&lower);
    if let Some(name) = by_ext {
        if ctx.containers.muxer_names().any(|n| n == name) {
            return Ok(Target::Container {
                name: name.to_string(),
                requested: lower,
            });
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

/// The payload codec a container carries by default: a registered
/// encoder named like the requested format when the request was a
/// brand or extension of a multi-codec container (`avif` on the heif
/// muxer → codec `avif`; `heic` / `heif` → `heif`); else the few
/// containers whose codec is named differently (when that encoder is
/// registered); else the codec of the same name (png, bmp, tiff, heif,
/// …).
fn default_codec_for_container(
    ctx: &RuntimeContext,
    container: &str,
    requested: &str,
    opts: &SaveOptions,
) -> String {
    // The requested name picks the codec when an encoder of that name
    // exists on the resolved container (ruling, round 476): the brand
    // asked for must not be lost to the container's default codec.
    if requested != container && ctx.codecs.has_encoder(&CodecId::new(requested)) {
        return requested.to_string();
    }
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
/// ladder of [`crate::ladder`] for the picture's own layout (and its
/// real transparency: palette alpha included, an all-opaque alpha
/// channel excluded) and the encoder's declared accepted set.
fn candidates(
    ctx: &RuntimeContext,
    codec_id: &CodecId,
    first: &Image,
    opts: &SaveOptions,
) -> Vec<PixelFormat> {
    if let Some(f) = opts.pixel_format {
        return vec![f];
    }
    ladder::ladder_with_alpha(
        first.format(),
        first.has_transparency(),
        &declared_formats(ctx, codec_id),
    )
}

/// The accepted input layouts the encoder `first_encoder` picks for
/// `codec_id` declares (empty = undeclared).
fn declared_formats(ctx: &RuntimeContext, codec_id: &CodecId) -> Vec<PixelFormat> {
    ctx.codecs
        .implementations(codec_id)
        .iter()
        .find(|i| i.make_encoder.is_some())
        .map(|i| i.caps.accepted_pixel_formats.clone())
        .unwrap_or_default()
}

/// One attempt in a fixed layout: convert every picture, encode each
/// geometry group on its own encoder, mux into memory.
fn encode_attempt(
    ctx: &RuntimeContext,
    images: &[Image],
    groups: &[Group],
    target: &Target,
    codec_id: &CodecId,
    dst: PixelFormat,
    opts: &SaveOptions,
) -> Result<Vec<u8>> {
    let time_base = opts.time_base.unwrap_or_else(|| match target {
        // The APNG muxer reads `duration` in its 1/100 s delay unit.
        Target::Container { name, .. } if name == "png" || name == "apng" => TimeBase::new(1, 100),
        _ => TimeBase::MILLIS,
    });
    let default_delay = opts.default_delay.unwrap_or(DEFAULT_DELAY);

    let mut streams: Vec<StreamInfo> = Vec::with_capacity(groups.len());
    let mut per_group: Vec<Vec<Packet>> = Vec::with_capacity(groups.len());
    for (gi, g) in groups.iter().enumerate() {
        let params = encoder_params(
            ctx,
            codec_id,
            g,
            &images[g.members[0]],
            images.len() == 1,
            dst,
            opts,
        );
        let (packets, out_params, total) = encode_group(
            ctx,
            images,
            g,
            gi as u32,
            &params,
            matches!(target, Target::Container { .. }),
            time_base,
            default_delay,
            opts,
        )?;
        streams.push(StreamInfo {
            index: gi as u32,
            time_base,
            duration: Some(total),
            start_time: Some(0),
            params: out_params,
        });
        per_group.push(packets);
    }

    let container = match target {
        Target::Container { name, .. } => name.as_str(),
        Target::CodecOnly(id) => {
            // `encode_frames` admits one group here.
            let mut packets = per_group.pop().unwrap_or_default();
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

    // Packet order: the pictures' own order when every group produced
    // one packet per picture, else group by group.
    let one_each = groups
        .iter()
        .zip(&per_group)
        .all(|(g, p)| g.members.len() == p.len());
    let packets: Vec<Packet> = if one_each {
        let mut indexed: Vec<(usize, Packet)> = groups
            .iter()
            .zip(per_group)
            .flat_map(|(g, p)| g.members.iter().copied().zip(p))
            .collect();
        indexed.sort_by_key(|(i, _)| *i);
        indexed.into_iter().map(|(_, p)| p).collect()
    } else {
        per_group.into_iter().flatten().collect()
    };

    // A muxer that takes one geometry refuses the stream set: say which
    // geometries were asked for instead of handing its error through.
    let multi = |e: oxideav_core::Error| {
        if groups.len() > 1 {
            Error::unsupported(format!(
                "container '{container}' refused {} picture geometries ({}): {e}",
                groups.len(),
                geometries(groups)
            ))
        } else {
            Error::Core(e)
        }
    };
    let sink = SharedCursor::default();
    let mut muxer = ctx
        .containers
        .open_muxer(container, Box::new(sink.clone()), &streams)
        .map_err(multi)?;
    muxer.write_header().map_err(multi)?;
    for pkt in &packets {
        muxer.write_packet(pkt).map_err(multi)?;
    }
    muxer.write_trailer().map_err(multi)?;
    drop(muxer);
    let bytes = sink.into_bytes();
    if bytes.is_empty() {
        return Err(Error::unsupported(format!(
            "muxer '{container}' wrote no bytes"
        )));
    }
    Ok(bytes)
}

/// Encoder parameters for one geometry group in layout `dst`
/// (`single`: the whole encode is one picture).
fn encoder_params(
    ctx: &RuntimeContext,
    codec_id: &CodecId,
    group: &Group,
    first: &Image,
    single: bool,
    dst: PixelFormat,
    opts: &SaveOptions,
) -> CodecParameters {
    let mut params = CodecParameters::video(codec_id.clone());
    params.width = Some(group.width);
    params.height = Some(group.height);
    params.pixel_format = Some(dst);
    if let Some(sig) = first.color_signal() {
        params.color_signal = sig;
    }
    params.blobs = opts.stream_blobs(first, single);
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
    params
}

/// Encode the pictures of one group on one encoder: the packets (stamped
/// with `stream_index` and, when there is one per picture, with this
/// group's timing), the encoder's output parameters and the group's
/// total duration in ticks.
#[allow(clippy::too_many_arguments)]
fn encode_group(
    ctx: &RuntimeContext,
    images: &[Image],
    group: &Group,
    stream_index: u32,
    params: &CodecParameters,
    has_muxer: bool,
    time_base: TimeBase,
    default_delay: Duration,
    opts: &SaveOptions,
) -> Result<(Vec<Packet>, CodecParameters, i64)> {
    let codec_id = &params.codec_id;
    let dst = params.pixel_format.unwrap_or(PixelFormat::Rgba);
    let n = group.members.len();
    let mut encoder = ctx.codecs.first_encoder(params)?;
    let mut packets: Vec<Packet> = Vec::new();
    let mut pts: i64 = 0;
    let mut ticks: Vec<(i64, i64)> = Vec::with_capacity(n);
    for &i in &group.members {
        let img = &images[i];
        let converted = img.to_format(dst)?;
        let (mut frame, _) = converted.into_video_frame();
        frame.set_blobs(opts.frame_blobs(img));
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
    if packets.len() == 1 && n > 1 && has_muxer {
        // The encoder folded every picture into one packet (png writes
        // an APNG itself with a uniform delay). Containers whose muxer
        // assembles the multi-picture file from one packet per picture
        // need exactly that, so encode each picture on its own encoder
        // and let the muxer do the assembly with our timing.
        packets.clear();
        for &i in &group.members {
            let mut enc = ctx.codecs.first_encoder(params)?;
            let (mut frame, _) = images[i].to_format(dst)?.into_video_frame();
            frame.set_blobs(opts.frame_blobs(&images[i]));
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
    if packets.len() == n {
        for (pkt, (p, d)) in packets.iter_mut().zip(&ticks) {
            pkt.time_base = time_base;
            pkt.pts = Some(*p);
            pkt.dts = Some(*p);
            pkt.duration = (*d > 0 || images.len() > 1).then_some(*d);
        }
    }
    for pkt in &mut packets {
        pkt.stream_index = stream_index;
    }
    let mut out_params = encoder.output_params().clone();
    out_params.media_type = MediaType::Video;
    Ok((packets, out_params, pts))
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
