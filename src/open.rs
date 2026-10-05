//! Opening: probe → demuxer → decoder through the registries.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use oxideav_core::{CodecParameters, Decoder, Frame, ReadSeek, RuntimeContext, StreamInfo};

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
        .probe_input(reader.as_mut(), opts.ext_hint.as_deref())?;
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

    let limit = opts.max_frames.unwrap_or(usize::MAX);
    let mut images = Vec::new();
    'pump: loop {
        if images.len() >= limit {
            break;
        }
        match demuxer.next_packet() {
            Ok(pkt) => {
                let idx = pkt.stream_index as usize;
                if let Some(Some(dec)) = decoders.get_mut(idx) {
                    dec.send_packet(&pkt)?;
                    drain(dec.as_mut(), &streams[idx].params, &mut images, limit)?;
                }
            }
            Err(oxideav_core::Error::Eof) => {
                for (i, slot) in decoders.iter_mut().enumerate() {
                    if let Some(dec) = slot {
                        let _ = dec.flush();
                        drain(dec.as_mut(), &streams[i].params, &mut images, limit)?;
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
    Ok(ImageFile {
        container,
        metadata,
        images,
    })
}

/// Pull every ready frame out of a decoder.
fn drain(
    dec: &mut dyn Decoder,
    stream: &CodecParameters,
    images: &mut Vec<Image>,
    limit: usize,
) -> Result<()> {
    loop {
        if images.len() >= limit {
            return Ok(());
        }
        match dec.receive_frame() {
            Ok(Frame::Video(vf)) => {
                let mut params = stream.clone();
                if params.pixel_format.is_none() {
                    params.pixel_format = dec.output_pixel_format();
                }
                let index = images.len();
                images.push(Image::from_video_frame(vf, &params)?.with_index(index));
            }
            Ok(_) => {}
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}
