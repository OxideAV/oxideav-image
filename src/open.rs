//! Opening: probe → demuxer → decoder through the registries.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use std::sync::Arc;

use oxideav_core::{
    BlobKind, CodecParameters, Decoder, Frame, MetadataBlob, ReadSeek, RuntimeContext, StreamInfo,
};

use crate::image::ticks_to_duration;

use crate::error::{Error, Result};
use crate::image::Image;

/// What a file decoded to: the container that matched, its metadata and
/// every picture (one for a still; the frames of an animation, burst,
/// sequence or multi-page document in file order).
#[derive(Debug)]
pub struct ImageFile {
    container: String,
    metadata: Vec<(String, String)>,
    /// The container streams' parameters (after the open options'
    /// strip rules), indexed like `Image::stream()` and shared with the
    /// pictures of each stream.
    streams: Vec<Arc<CodecParameters>>,
    images: Vec<Image>,
}

impl ImageFile {
    /// Registered name of the container that decoded the input
    /// (`"png"`, `"heif"`, `"tiff"`, …).
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Container-level metadata as the demuxer reported it: string
    /// pairs. Opaque payloads (ICC, Exif, XMP, …) are blobs instead —
    /// see [`blobs`](Self::blobs).
    pub fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    /// The file's metadata blobs: the stream-level blobs of the stream
    /// the primary picture came from (what the demuxer put on that
    /// stream's `CodecParameters` — the ICC profile, Exif, XMP of the
    /// file as a whole). Per-picture blobs are on each [`Image`].
    pub fn blobs(&self) -> &[MetadataBlob] {
        self.stream_blobs(self.images[0].stream())
    }

    /// The stream-level blobs of container stream `index` (empty for a
    /// stream the gateway did not decode or an index out of range).
    pub fn stream_blobs(&self, index: usize) -> &[MetadataBlob] {
        self.streams
            .get(index)
            .map(|p| p.blobs())
            .unwrap_or_default()
    }

    /// The first file-level blob of `kind` (see [`blobs`](Self::blobs)).
    pub fn blob(&self, kind: &BlobKind) -> Option<&MetadataBlob> {
        self.blobs().iter().find(|b| b.is(kind))
    }

    /// The file's ICC profile (`BlobKind::ICC`), stream level. The
    /// primary picture's [`Image::icc`] also consults its own frame
    /// record first.
    pub fn icc(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::ICC).map(|b| b.data.as_slice())
    }

    /// The file's Exif block (`BlobKind::EXIF`), stream level.
    pub fn exif(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::EXIF).map(|b| b.data.as_slice())
    }

    /// The file's XMP packet (`BlobKind::XMP`), stream level.
    pub fn xmp(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::XMP).map(|b| b.data.as_slice())
    }

    /// The file's IPTC datasets (`BlobKind::IPTC`), stream level.
    pub fn iptc(&self) -> Option<&[u8]> {
        self.blob(&BlobKind::IPTC).map(|b| b.data.as_slice())
    }

    /// True when any stream or any picture carries a blob.
    pub fn has_metadata(&self) -> bool {
        self.streams.iter().any(|p| !p.blobs().is_empty())
            || self.images.iter().any(|i| !i.frame_blobs().is_empty())
    }

    /// Keep only the blobs `keep` accepts, on every stream and every
    /// picture (both levels) — the post-open counterpart of
    /// [`OpenOptions::strip_blobs`].
    pub fn retain_blobs(&mut self, mut keep: impl FnMut(&MetadataBlob) -> bool) {
        // Pictures of one stream share its parameters: filter each
        // distinct set once and re-point the pictures that held it, so
        // the sharing survives (a picture given its own parameters by
        // the caller is filtered on its own).
        type Shared = Arc<CodecParameters>;
        fn filter(
            seen: &mut Vec<(Shared, Shared)>,
            keep: &mut dyn FnMut(&MetadataBlob) -> bool,
            p: &Shared,
        ) -> Shared {
            if let Some((_, f)) = seen.iter().find(|(o, _)| Arc::ptr_eq(o, p)) {
                return f.clone();
            }
            let f = if p.blobs().iter().all(&mut *keep) {
                p.clone()
            } else {
                let mut c = (**p).clone();
                c.blobs.retain(|b| keep(b));
                Arc::new(c)
            };
            seen.push((p.clone(), f.clone()));
            f
        }
        let mut seen: Vec<(Shared, Shared)> = Vec::new();
        for p in &mut self.streams {
            *p = filter(&mut seen, &mut keep, p);
        }
        for img in &mut self.images {
            let shared = filter(&mut seen, &mut keep, img.params_arc());
            img.set_params_arc(shared);
            img.retain_frame_blobs(&mut keep);
        }
    }

    /// Drop every blob on every stream and every picture; the string
    /// metadata stays. The post-open counterpart of
    /// [`OpenOptions::strip_metadata`].
    pub fn strip_metadata(&mut self) {
        self.retain_blobs(|_| false);
    }

    /// The primary picture: the first decoded frame.
    pub fn primary(&self) -> &Image {
        &self.images[0]
    }

    /// Every decoded picture in file order.
    pub fn frames(&self) -> &[Image] {
        &self.images
    }

    /// Consume the file, keeping the pictures.
    pub fn into_frames(self) -> Vec<Image> {
        self.images
    }

    /// Consume the file, keeping the primary picture.
    pub fn into_primary(mut self) -> Image {
        self.images.swap_remove(0)
    }

    /// Every decoded picture, mutably (set delays before re-encoding,
    /// replace a frame, …).
    pub fn frames_mut(&mut self) -> &mut [Image] {
        &mut self.images
    }

    /// Iterate the pictures in file order.
    pub fn iter(&self) -> std::slice::Iter<'_, Image> {
        self.images.iter()
    }

    /// Number of decoded pictures (always at least one).
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Never true: an `ImageFile` always holds at least one picture.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

impl IntoIterator for ImageFile {
    type Item = Image;
    type IntoIter = std::vec::IntoIter<Image>;
    fn into_iter(self) -> Self::IntoIter {
        self.images.into_iter()
    }
}

impl<'a> IntoIterator for &'a ImageFile {
    type Item = &'a Image;
    type IntoIter = std::slice::Iter<'a, Image>;
    fn into_iter(self) -> Self::IntoIter {
        self.images.iter()
    }
}

/// Knobs for [`open_with`] / [`decode_bytes_with`] / [`decode_reader`].
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct OpenOptions {
    /// Stop after this many pictures (`Some(1)` decodes only the
    /// primary image of an animation); `None` decodes everything.
    /// `Some(0)` is treated as `Some(1)`: a file always yields its
    /// primary picture.
    pub max_frames: Option<usize>,
    /// File-extension hint for the container probe (without the dot).
    /// [`open`] fills it from the path.
    pub ext_hint: Option<String>,
    /// Options handed to the decoder through `CodecParameters::options`
    /// (names from the codec's declared decoder schema; unknown names
    /// fail inside the decoder).
    pub decoder_options: Vec<(String, String)>,
    /// Budget of decoded pixels over the whole file (every kept picture's
    /// `width × height` summed). Checked from the stream geometry before
    /// a decoder is created and again before each picture is kept, so an
    /// oversized file fails with [`crate::ImageError::LimitExceeded`]
    /// before its pixels are decoded. `None` = unlimited.
    pub max_pixels: Option<u64>,
    /// Container names the probe may resolve to; `None` = any. Checked
    /// right after the probe, before the demuxer parses the header, and
    /// refused with [`crate::ImageError::Restricted`]. Names compare
    /// case-insensitively.
    pub allow_containers: Option<Vec<String>>,
    /// Container names refused after the probe (wins over the allow
    /// list).
    pub deny_containers: Vec<String>,
    /// Codec ids the file's video streams may carry; `None` = any.
    /// Checked from the demuxer's stream table before any decoder is
    /// created — one header parse — and refused with
    /// [`crate::ImageError::Restricted`]. Streams the gateway never
    /// decodes (audio, data) are not checked.
    pub allow_codecs: Option<Vec<String>>,
    /// Codec ids refused on any video stream (wins over the allow list).
    pub deny_codecs: Vec<String>,
    /// Drop every metadata blob — stream level and per picture — as the
    /// file is opened, so nothing embedded (ICC, Exif, XMP, …) reaches
    /// the pictures or a later `encode`. The container's string
    /// metadata is unaffected. Default `false`: everything is kept.
    pub strip_metadata: bool,
    /// Blob kinds dropped at both levels when `strip_metadata` is off
    /// (`BlobKind::EXIF` to shed location data, say).
    pub strip_blobs: Vec<BlobKind>,
}

impl OpenOptions {
    /// Defaults: every frame, no extension hint.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode at most `n` pictures.
    pub fn with_max_frames(mut self, n: usize) -> Self {
        self.max_frames = Some(n);
        self
    }

    /// Hint the container probe with a file extension.
    pub fn with_ext_hint(mut self, ext: impl Into<String>) -> Self {
        self.ext_hint = Some(ext.into());
        self
    }

    /// Add one decoder option.
    pub fn with_decoder_option(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.decoder_options.push((name.into(), value.into()));
        self
    }

    /// Replace the decoder options.
    pub fn with_decoder_options(mut self, options: Vec<(String, String)>) -> Self {
        self.decoder_options = options;
        self
    }

    /// Cap the decoded pixels (see the field).
    pub fn with_max_pixels(mut self, n: u64) -> Self {
        self.max_pixels = Some(n);
        self
    }

    /// Only these containers may open (see the field).
    pub fn with_allow_containers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_containers = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Refuse these containers (see the field).
    pub fn with_deny_containers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_containers = names.into_iter().map(Into::into).collect();
        self
    }

    /// Only these codecs may be decoded (see the field).
    pub fn with_allow_codecs<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allow_codecs = Some(ids.into_iter().map(Into::into).collect());
        self
    }

    /// Refuse these codecs (see the field).
    pub fn with_deny_codecs<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.deny_codecs = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Keep (`true`, the default) or drop every metadata blob while
    /// opening (see the `strip_metadata` field).
    pub fn with_metadata(mut self, keep: bool) -> Self {
        self.strip_metadata = !keep;
        self
    }

    /// Drop every metadata blob while opening.
    pub fn without_metadata(self) -> Self {
        self.with_metadata(false)
    }

    /// Drop these blob kinds while opening (see the field).
    pub fn with_stripped_blobs<I, K>(mut self, kinds: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<BlobKind>,
    {
        self.strip_blobs = kinds.into_iter().map(Into::into).collect();
        self
    }

    /// Whether a blob survives these options.
    fn keeps(&self, blob: &MetadataBlob) -> bool {
        !self.strip_metadata && !self.strip_blobs.iter().any(|k| blob.is(k))
    }

    /// Apply the strip rules to a stream's parameters.
    fn filter_params(&self, params: &mut CodecParameters) {
        if self.strip_metadata || !self.strip_blobs.is_empty() {
            params.blobs.retain(|b| self.keeps(b));
        }
    }
}

/// The allow / deny rule: a denied name loses; otherwise an allow list,
/// when present, must contain it. Case-insensitive.
fn permitted(name: &str, allow: Option<&[String]>, deny: &[String]) -> bool {
    if deny.iter().any(|d| d.eq_ignore_ascii_case(name)) {
        return false;
    }
    match allow {
        Some(list) => list.iter().any(|a| a.eq_ignore_ascii_case(name)),
        None => true,
    }
}

/// Open and decode an image file through the registries in `ctx`.
pub fn open(ctx: &RuntimeContext, path: impl AsRef<Path>) -> Result<ImageFile> {
    open_with(ctx, path, &OpenOptions::default())
}

/// [`open`] with options. The path's extension becomes the probe hint
/// unless the options carry one.
pub fn open_with(
    ctx: &RuntimeContext,
    path: impl AsRef<Path>,
    opts: &OpenOptions,
) -> Result<ImageFile> {
    let path = path.as_ref();
    let mut opts = opts.clone();
    if opts.ext_hint.is_none() {
        opts.ext_hint = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
    }
    let file = File::open(path)?;
    decode_reader(ctx, Box::new(BufReader::new(file)), &opts)
}

/// Decode an in-memory file. The demuxer needs an owned seekable
/// reader, so the slice is copied once; [`decode_vec`] avoids that.
pub fn decode_bytes(ctx: &RuntimeContext, bytes: &[u8]) -> Result<ImageFile> {
    decode_bytes_with(ctx, bytes, &OpenOptions::default())
}

/// [`decode_bytes`] with options.
pub fn decode_bytes_with(
    ctx: &RuntimeContext,
    bytes: &[u8],
    opts: &OpenOptions,
) -> Result<ImageFile> {
    decode_vec_with(ctx, bytes.to_vec(), opts)
}

/// Decode an in-memory file without copying it.
pub fn decode_vec(ctx: &RuntimeContext, bytes: Vec<u8>) -> Result<ImageFile> {
    decode_vec_with(ctx, bytes, &OpenOptions::default())
}

/// [`decode_vec`] with options.
pub fn decode_vec_with(
    ctx: &RuntimeContext,
    bytes: Vec<u8>,
    opts: &OpenOptions,
) -> Result<ImageFile> {
    decode_reader(ctx, Box::new(Cursor::new(bytes)), opts)
}

/// Decode from any seekable reader: probe the container, open its
/// demuxer, decode every video stream with the registry's first
/// decoder, and collect the pictures.
pub fn decode_reader(
    ctx: &RuntimeContext,
    mut reader: Box<dyn ReadSeek>,
    opts: &OpenOptions,
) -> Result<ImageFile> {
    let container = ctx
        .containers
        .probe_input(reader.as_mut(), opts.ext_hint.as_deref())
        .map_err(|e| match e {
            oxideav_core::Error::FormatNotFound(msg) => {
                Error::UnknownFormat(codec_only_hint(ctx, opts.ext_hint.as_deref(), msg))
            }
            other => Error::Core(other),
        })?;
    if !permitted(
        &container,
        opts.allow_containers.as_deref(),
        &opts.deny_containers,
    ) {
        return Err(Error::restricted(format!(
            "container '{container}' is not permitted by the open options"
        )));
    }
    let mut demuxer = ctx
        .containers
        .open_demuxer(&container, reader, &ctx.codecs)
        .map_err(|e| match e {
            // An extension registered by a codec-only crate probes to a
            // container name that has no demuxer.
            oxideav_core::Error::FormatNotFound(msg) => {
                Error::UnknownFormat(codec_only_hint(ctx, Some(&container), msg))
            }
            other => Error::Core(other),
        })?;
    let mut streams: Vec<StreamInfo> = demuxer.streams().to_vec();
    // The strip rules apply before anything downstream sees the
    // parameters: the decoder, the pictures, a later encode.
    for s in &mut streams {
        opts.filter_params(&mut s.params);
    }
    let metadata = demuxer.metadata().to_vec();
    // The codec lists are checked from the stream table alone: no
    // decoder exists yet, so a refused file costs one header parse.
    for s in &streams {
        if s.params.media_type != oxideav_core::MediaType::Video {
            continue;
        }
        let id = s.params.codec_id.as_str();
        if !permitted(id, opts.allow_codecs.as_deref(), &opts.deny_codecs) {
            return Err(Error::restricted(format!(
                "codec '{id}' (stream {} of container '{container}') is not permitted by the \
                 open options",
                s.index
            )));
        }
    }

    // The pictures of a stream share its parameters (one copy of the
    // stream's blobs, however many pages or frames).
    let shared: Vec<Arc<CodecParameters>> =
        streams.iter().map(|s| Arc::new(s.params.clone())).collect();
    // One decoder per video stream; other streams are skipped.
    let budget = opts.max_pixels.unwrap_or(u64::MAX);
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = Vec::with_capacity(streams.len());
    for s in &streams {
        let dec = if s.params.media_type == oxideav_core::MediaType::Video {
            let px = pixels_of(&s.params);
            if px > budget {
                return Err(Error::limit(format!(
                    "stream {} is {}x{} = {px} pixels, over the {budget}-pixel budget",
                    s.index,
                    s.params.width.unwrap_or(0),
                    s.params.height.unwrap_or(0)
                )));
            }
            let mut params = s.params.clone();
            for (k, v) in &opts.decoder_options {
                params.options = params.options.set(k.clone(), v.clone());
            }
            ctx.codecs.first_decoder(&params).ok()
        } else {
            None
        };
        decoders.push(dec);
    }
    if decoders.iter().all(Option::is_none) {
        return Err(Error::NoImage(format!(
            "container '{container}' has no video stream with a registered decoder"
        )));
    }

    // At least the primary picture: `Some(0)` behaves like `Some(1)`.
    let limit = opts.max_frames.unwrap_or(usize::MAX).max(1);
    let mut images = Vec::new();
    let mut spent: u64 = 0;
    // Packet timing per stream, consumed as frames come out: the
    // decoders of image codecs are one-in/one-out, and a frame that
    // carries its own `pts` is matched to the packet with that `pts`.
    let mut timing: Vec<VecDeque<(Option<i64>, Option<i64>)>> =
        (0..streams.len()).map(|_| VecDeque::new()).collect();
    'pump: loop {
        if images.len() >= limit {
            break;
        }
        match demuxer.next_packet() {
            Ok(pkt) => {
                let idx = pkt.stream_index as usize;
                if let Some(Some(dec)) = decoders.get_mut(idx) {
                    timing[idx].push_back((pkt.pts, pkt.duration));
                    dec.send_packet(&pkt)?;
                    drain(
                        dec.as_mut(),
                        (&streams[idx], &shared[idx]),
                        idx,
                        &mut timing[idx],
                        &mut images,
                        limit,
                        (&mut spent, budget),
                        opts,
                    )?;
                }
            }
            Err(oxideav_core::Error::Eof) => {
                for (i, slot) in decoders.iter_mut().enumerate() {
                    if let Some(dec) = slot {
                        let _ = dec.flush();
                        drain(
                            dec.as_mut(),
                            (&streams[i], &shared[i]),
                            i,
                            &mut timing[i],
                            &mut images,
                            limit,
                            (&mut spent, budget),
                            opts,
                        )?;
                    }
                }
                break 'pump;
            }
            Err(e) => return Err(e.into()),
        }
    }
    if images.is_empty() {
        return Err(Error::NoImage(format!(
            "container '{container}' decoded no picture"
        )));
    }
    resolve_delays(&mut images, streams.len());
    Ok(ImageFile {
        container,
        metadata,
        streams: shared,
        images,
    })
}

/// Pull every ready frame out of a decoder.
#[allow(clippy::too_many_arguments)]
fn drain(
    dec: &mut dyn Decoder,
    (info, shared): (&StreamInfo, &Arc<CodecParameters>),
    stream_index: usize,
    timing: &mut VecDeque<(Option<i64>, Option<i64>)>,
    images: &mut Vec<Image>,
    limit: usize,
    (spent, budget): (&mut u64, u64),
    opts: &OpenOptions,
) -> Result<()> {
    let stream = &info.params;
    loop {
        if images.len() >= limit {
            return Ok(());
        }
        match dec.receive_frame() {
            Ok(Frame::Video(vf)) => {
                let px = pixels_of(stream);
                if spent.saturating_add(px) > budget {
                    return Err(Error::limit(format!(
                        "picture {} would bring the decoded total to {} pixels, over the \
                         {budget}-pixel budget",
                        images.len(),
                        spent.saturating_add(px)
                    )));
                }
                *spent += px;
                // The packet this frame came from: by pts when the
                // decoder preserved it, else the oldest unmatched one.
                let matched = vf
                    .pts
                    .and_then(|p| timing.iter().position(|(q, _)| *q == Some(p)))
                    .and_then(|i| timing.remove(i))
                    .or_else(|| timing.pop_front());
                let (pts, duration) = match matched {
                    Some((p, d)) => (p.or(vf.pts), d),
                    None => (vf.pts, None),
                };
                // The stream's parameters are authoritative: every image
                // demuxer declares its native layout on the stream
                // (IMAGE_CRATE_API fleet-sweep ruling), so a missing
                // pixel format is a demuxer defect, not something to
                // guess from the plane count (Rgba vs Bgra vs Yuv444P are
                // indistinguishable by geometry).
                if stream.pixel_format.is_none() {
                    return Err(Error::invalid(format!(
                        "stream of codec '{}' declares no pixel format",
                        stream.codec_id
                    )));
                }
                let index = images.len();
                let mut img = Image::from_video_frame_shared(vf, shared.clone())?.with_index(index);
                crate::adapt::adapt(&mut img, stream);
                // The stream level was filtered before decoding; only
                // the frame's own blobs are left to check.
                if opts.strip_metadata || !opts.strip_blobs.is_empty() {
                    img.retain_frame_blobs(|b| opts.keeps(b));
                }
                img.set_timing(
                    stream_index,
                    info.time_base,
                    info.start_time.unwrap_or(0),
                    pts,
                    duration,
                );
                images.push(img);
            }
            Ok(_) => {}
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Some image crates register a codec and an extension but no
/// container (their decoder takes the whole file as one packet). The
/// gateway cannot open those through the registry yet — say so instead
/// of a bare "no format matched".
fn codec_only_hint(ctx: &RuntimeContext, ext: Option<&str>, msg: String) -> String {
    let Some(ext) = ext else { return msg };
    let name = ctx.containers.container_for_extension(ext).unwrap_or(ext);
    let id = oxideav_core::CodecId::new(name);
    if ctx.containers.demuxer_names().any(|n| n == name) || !ctx.codecs.has_decoder(&id) {
        return msg;
    }
    format!(
        "{msg}; '{ext}' is registered as codec '{name}' without a container demuxer, \
         which the gateway cannot open through the registry"
    )
}

/// Apply the delay rule documented on [`Image::delay`] to the pictures
/// of each stream, in decode order: own duration, else gap to the next
/// picture, else (last picture) the previous delay. A stream with a
/// single picture is a still: no delay, whatever duration the demuxer
/// stamped (JPEG's frame-rate tick, HEIF's `1`, …).
fn resolve_delays(images: &mut [Image], streams: usize) {
    for s in 0..streams {
        let idx: Vec<usize> = (0..images.len())
            .filter(|&i| images[i].stream() == s)
            .collect();
        if idx.len() < 2 {
            continue;
        }
        // A 1/1 time base is the convention for untimed multi-image
        // streams (HEIF bursts, EXR parts, ICER bands, TIFF pages): the
        // pictures are indexed, not scheduled, so they carry no delay —
        // a tick of one second would be an invention.
        let tb = images[idx[0]].time_base();
        if tb.0.num == tb.0.den {
            for &i in &idx {
                images[i].set_delay(None);
            }
            continue;
        }
        let mut prev: Option<std::time::Duration> = None;
        for (k, &i) in idx.iter().enumerate() {
            let tb = images[i].time_base();
            let (pts, dur) = images[i].raw_timing();
            let next_pts = idx.get(k + 1).and_then(|&j| images[j].raw_timing().0);
            // A non-positive duration says nothing (stills are often
            // stamped 0); fall through to the pts gap.
            let ticks = dur.filter(|d| *d > 0).or_else(|| match (pts, next_pts) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                _ => None,
            });
            let delay = ticks.and_then(|t| ticks_to_duration(t, tb)).or(prev);
            images[i].set_delay(delay);
            prev = delay;
        }
    }
}

/// `width × height` of a stream, saturating.
fn pixels_of(p: &oxideav_core::CodecParameters) -> u64 {
    u64::from(p.width.unwrap_or(0)) * u64::from(p.height.unwrap_or(0))
}

#[cfg(test)]
mod delay_tests {
    use super::*;
    use oxideav_core::TimeBase;

    fn pic(stream: usize, tb: TimeBase, pts: i64, dur: Option<i64>) -> Image {
        let mut img = Image::from_rgb8(1, 1, vec![0, 0, 0]).unwrap();
        img.set_timing(stream, tb, 0, Some(pts), dur);
        img
    }

    #[test]
    fn untimed_one_over_one_streams_have_no_delay() {
        let tb = TimeBase::new(1, 1);
        let mut imgs = vec![
            pic(0, tb, 0, Some(1)),
            pic(0, tb, 1, Some(1)),
            pic(0, tb, 2, Some(1)),
        ];
        resolve_delays(&mut imgs, 1);
        assert!(imgs.iter().all(|i| i.delay().is_none()));
    }

    #[test]
    fn timed_streams_keep_their_durations() {
        let tb = TimeBase::new(1, 1000);
        let mut imgs = vec![pic(0, tb, 0, Some(100)), pic(0, tb, 100, Some(250))];
        resolve_delays(&mut imgs, 1);
        assert_eq!(imgs[0].delay(), Some(std::time::Duration::from_millis(100)));
        assert_eq!(imgs[1].delay(), Some(std::time::Duration::from_millis(250)));
    }
}
