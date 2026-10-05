//! Opening: probe → demuxer → decoder through the registries.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use oxideav_core::{Decoder, Frame, ReadSeek, RuntimeContext, StreamInfo};

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
    images: Vec<Image>,
}

impl ImageFile {
    /// Registered name of the container that decoded the input
    /// (`"png"`, `"heif"`, `"tiff"`, …).
    pub fn container(&self) -> &str {
        &self.container
    }

    /// Container-level metadata as the demuxer reported it.
    pub fn metadata(&self) -> &[(String, String)] {
        &self.metadata
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

    /// Number of decoded pictures (always at least one).
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Never true: an `ImageFile` always holds at least one picture.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
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

/// Decode an in-memory file.
pub fn decode_bytes(ctx: &RuntimeContext, bytes: &[u8]) -> Result<ImageFile> {
    decode_bytes_with(ctx, bytes, &OpenOptions::default())
}

/// [`decode_bytes`] with options.
pub fn decode_bytes_with(
    ctx: &RuntimeContext,
    bytes: &[u8],
    opts: &OpenOptions,
) -> Result<ImageFile> {
    decode_reader(ctx, Box::new(Cursor::new(bytes.to_vec())), opts)
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
    let mut demuxer = ctx
        .containers
        .open_demuxer(&container, reader, &ctx.codecs)?;
    let streams: Vec<StreamInfo> = demuxer.streams().to_vec();
    let metadata = demuxer.metadata().to_vec();

    // One decoder per video stream; other streams are skipped.
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = Vec::with_capacity(streams.len());
    for s in &streams {
        let dec = if s.params.media_type == oxideav_core::MediaType::Video {
            ctx.codecs.first_decoder(&s.params).ok()
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
                        &streams[idx],
                        idx,
                        &mut timing[idx],
                        &mut images,
                        limit,
                    )?;
                }
            }
            Err(oxideav_core::Error::Eof) => {
                for (i, slot) in decoders.iter_mut().enumerate() {
                    if let Some(dec) = slot {
                        let _ = dec.flush();
                        drain(
                            dec.as_mut(),
                            &streams[i],
                            i,
                            &mut timing[i],
                            &mut images,
                            limit,
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
        images,
    })
}

/// Pull every ready frame out of a decoder.
fn drain(
    dec: &mut dyn Decoder,
    info: &StreamInfo,
    stream_index: usize,
    timing: &mut VecDeque<(Option<i64>, Option<i64>)>,
    images: &mut Vec<Image>,
    limit: usize,
) -> Result<()> {
    let stream = &info.params;
    loop {
        if images.len() >= limit {
            return Ok(());
        }
        match dec.receive_frame() {
            Ok(Frame::Video(vf)) => {
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
                let mut img = Image::from_video_frame(vf, stream)?.with_index(index);
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
    let Some(name) = ctx.containers.container_for_extension(ext) else {
        return msg;
    };
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
/// picture, else (last picture) the previous delay. A lone picture with
/// no duration stays `None`.
fn resolve_delays(images: &mut [Image], streams: usize) {
    for s in 0..streams {
        let idx: Vec<usize> = (0..images.len())
            .filter(|&i| images[i].stream() == s)
            .collect();
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
