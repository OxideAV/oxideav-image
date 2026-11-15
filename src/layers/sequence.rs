//! An animation as one value: its frames with the canvas, background
//! colour and loop count they share.

use std::time::Duration;

use super::{LayerOptions, OptimizeOptions};
use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{BACKGROUND_COLOR_KEY, Page, format_background, parse_background};
use crate::open::ImageFile;
use crate::save::SaveOptions;
use oxideav_core::RuntimeContext;

/// A layer sequence: the frames of an animation — each with its
/// picture, [`Page`] geometry, delay, [`crate::Dispose`] and
/// [`crate::Blend`] — and what the whole animation states once: the
/// canvas size, the background colour and the loop count.
///
/// Built from a frame list ([`new`](Self::new)) or an opened file
/// ([`from_file`](Self::from_file)); every frame is put on the
/// sequence's canvas. The methods are the [`crate::layers`] operations
/// run with the sequence's background, returning a new sequence;
/// [`encode`](Self::encode) writes it through the registry (README,
/// *Layers*, for the metadata read and written).
#[derive(Clone, Debug)]
pub struct LayerSequence {
    frames: Vec<Image>,
    canvas: (u32, u32),
    background: [u16; 4],
    loop_count: Option<u32>,
}

impl LayerSequence {
    /// A sequence of `frames` (at least one). The canvas is the first
    /// frame's ([`Image::canvas_size`]); the loop count is the first
    /// frame's ([`Image::loop_count`]); the background is the
    /// `background_color` entry of the first frame's container
    /// metadata when it has one, else transparent black.
    pub fn new(frames: Vec<Image>) -> Result<Self> {
        let first = frames
            .first()
            .ok_or_else(|| Error::invalid("layer sequence: no frame"))?;
        let canvas = first.canvas_size();
        let background = first
            .container_metadata_value(BACKGROUND_COLOR_KEY)
            .and_then(parse_background)
            .unwrap_or([0; 4]);
        let loop_count = first.loop_count();
        Ok(Self {
            frames,
            canvas,
            background,
            loop_count,
        }
        .on_canvas())
    }

    /// The sequence an opened file holds: its frames, its loop count
    /// and the `background_color` entry of its metadata.
    pub fn from_file(file: &ImageFile) -> Result<Self> {
        let mut seq = Self::new(file.frames().to_vec())?;
        seq.loop_count = file.loop_count();
        if let Some(bg) = file
            .metadata()
            .iter()
            .find(|(k, _)| k == BACKGROUND_COLOR_KEY)
            .and_then(|(_, v)| parse_background(v))
        {
            seq.background = bg;
        }
        Ok(seq)
    }

    /// Every frame declared on the sequence's canvas, offsets kept.
    fn on_canvas(mut self) -> Self {
        let (w, h) = self.canvas;
        for f in &mut self.frames {
            let (x, y) = f.offset();
            f.set_page(Page::new(w, h).with_offset(x, y));
        }
        self
    }

    fn with_frames(&self, frames: Vec<Image>) -> Self {
        Self {
            frames,
            canvas: self.canvas,
            background: self.background,
            loop_count: self.loop_count,
        }
    }

    /// The frames, in display order.
    pub fn frames(&self) -> &[Image] {
        &self.frames
    }

    /// The frames, given away.
    pub fn into_frames(self) -> Vec<Image> {
        self.frames
    }

    /// Number of frames (at least one).
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Always `false`: a sequence holds at least one frame.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The canvas, `(width, height)`.
    pub fn canvas_size(&self) -> (u32, u32) {
        self.canvas
    }

    /// The background colour, straight 16-bit RGBA.
    pub fn background(&self) -> [u16; 4] {
        self.background
    }

    /// Plays (`0` = forever); `None` = unstated.
    pub fn loop_count(&self) -> Option<u32> {
        self.loop_count
    }

    /// The sum of the frames' delays.
    pub fn total_delay(&self) -> Duration {
        super::total_delay(&self.frames)
    }

    /// Another canvas size, the frames' offsets kept.
    pub fn with_canvas(mut self, width: u32, height: u32) -> Self {
        self.canvas = (width, height);
        self.on_canvas()
    }

    /// Another background colour (straight 16-bit RGBA).
    pub fn with_background_rgba16(mut self, rgba: [u16; 4]) -> Self {
        self.background = rgba;
        self
    }

    /// Another background colour, from 8-bit RGBA.
    pub fn with_background_rgba8(self, rgba: [u8; 4]) -> Self {
        self.with_background_rgba16(rgba.map(|c| u16::from(c) * 257))
    }

    /// Another loop count (`Some(0)` = forever).
    pub fn with_loop_count(mut self, plays: Option<u32>) -> Self {
        self.loop_count = plays;
        self
    }

    /// The [`LayerOptions`] the methods run with: this background, the
    /// default rule, serial.
    pub fn layer_options(&self) -> LayerOptions {
        LayerOptions::new().with_background_rgba16(self.background)
    }

    /// [`super::coalesce_with`]: every frame rendered to the full
    /// canvas.
    pub fn coalesce(&self) -> Result<Self> {
        Ok(self.with_frames(super::coalesce_with(&self.frames, &self.layer_options())?))
    }

    /// [`super::optimize_layers_with`] on this background (the
    /// background of `opts.layer` is replaced; its rule and thread
    /// budget are kept).
    pub fn optimize(&self, opts: &OptimizeOptions) -> Result<Self> {
        let layer = opts.layer.with_background_rgba16(self.background);
        let opts = opts.with_layer_options(layer);
        Ok(self.with_frames(super::optimize_layers_with(&self.frames, &opts)?))
    }

    /// [`super::deconstruct`]: each frame cut to what differs from the
    /// frames before it.
    pub fn deconstruct(&self) -> Result<Self> {
        Ok(self.with_frames(super::deconstruct(&self.frames)?))
    }

    /// [`super::flatten_with`]: one picture of the canvas.
    pub fn flatten(&self) -> Result<Image> {
        super::flatten_with(&self.frames, &self.layer_options())
    }

    /// [`super::mosaic_with`]: one picture, the canvas grown to fit.
    pub fn mosaic(&self) -> Result<Image> {
        super::mosaic_with(&self.frames, &self.layer_options())
    }

    /// [`super::merge_with`]: one picture of the frames' bounding box.
    pub fn merge(&self) -> Result<Image> {
        super::merge_with(&self.frames, &self.layer_options())
    }

    /// [`super::remove_dups`]: runs of identical frames folded, delays
    /// added.
    pub fn remove_dups(&self) -> Result<Self> {
        Ok(self.with_frames(super::remove_dups(&self.frames)?))
    }

    /// [`super::select`]: the frames at `indices`, in that order. An
    /// empty selection is `InvalidData`.
    pub fn select(&self, indices: &[usize]) -> Result<Self> {
        if indices.is_empty() {
            return Err(Error::invalid("layer sequence: empty selection"));
        }
        Ok(self.with_frames(super::select(&self.frames, indices)?))
    }

    /// The frames last to first.
    pub fn reverse(&self) -> Self {
        self.with_frames(super::reverse(&self.frames))
    }

    /// Every delay multiplied by `numerator / denominator`.
    pub fn scale_delays(&self, numerator: u32, denominator: u32) -> Result<Self> {
        Ok(self.with_frames(super::scale_delays(&self.frames, numerator, denominator)?))
    }

    /// Every frame given the delay `delay`.
    pub fn with_delay(&self, delay: Option<Duration>) -> Self {
        self.with_frames(super::set_delay(&self.frames, delay))
    }

    /// Write the sequence as `format` through the registry
    /// ([`crate::encode_frames`]): the loop count goes out as
    /// [`SaveOptions::loop_count`] (unless `opts` sets one) and a
    /// background other than transparent black as the
    /// `background_color` container metadata entry (unless `opts`
    /// carries one).
    pub fn encode(
        &self,
        ctx: &RuntimeContext,
        format: &str,
        opts: &SaveOptions,
    ) -> Result<Vec<u8>> {
        let mut opts = opts.clone();
        if opts.loop_count.is_none()
            && let Some(plays) = self.loop_count
        {
            opts = opts.with_loop_count(plays);
        }
        let stated = opts
            .container_metadata
            .iter()
            .any(|(k, _)| k == BACKGROUND_COLOR_KEY);
        if !stated && self.background != [0; 4] {
            opts = opts.with_container_metadata_entry(
                BACKGROUND_COLOR_KEY,
                format_background(self.background),
            );
        }
        crate::save::encode_frames(ctx, &self.frames, format, &opts)
    }
}
