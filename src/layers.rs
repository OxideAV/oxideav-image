//! Format-independent operations on a list of layered pictures: the
//! frames of an animation (or the layers of a composition) with their
//! [`Page`] geometry, [`Dispose`] and [`Blend`] records.
//!
//! The vocabulary is ImageMagick's `-layers` / `-coalesce` family,
//! which the `Imagick` API exposes (`coalesceImages`,
//! `optimizeImageLayers`, `deconstructImages`, `compareImageLayers`,
//! `mergeImageLayers`, `flattenImages`, `mosaicImages`). The results
//! were pinned against the `magick` binary (7.1.2 Q16-HDRI) used as a
//! black box — page geometry, disposal and pixels of each frame; see
//! *Layer operations* in the README for the rules and the few places
//! the gateway departs from it on purpose.
//!
//! **Depth.** An operation works at the depth of its deepest frame:
//! 8-bit straight RGBA (`Rgba`) when every frame has eight bits a
//! sample or fewer, 16-bit (`Rgba64Le`) when one is deeper, float
//! (`RgbaF32Le`) when one is float. Frames it builds from a canvas come
//! out in that layout; frames it only crops keep their own layout
//! (palette included) where [`Image::crop`] can. The canvas is kept at
//! that depth too, so a list played frame by frame and the same list
//! played from its coalesced frames show the same pixels.
//!
//! **Alpha** is straight (non-premultiplied) everywhere. `s` OVER `d`
//! is defined as follows, with `M` the full-scale value (65535):
//! `s` when `s.a = M` or `d.a = 0`; `d` when `s.a = 0`; otherwise, with
//! `A = s.a·M + d.a·(M − s.a)`, alpha `A / M` and each colour
//! `(s.c·s.a·M + d.c·d.a·(M − s.a)) / A`, in exact integer arithmetic
//! rounded to nearest. An 8-bit sample `v` stands for the 16-bit value
//! `257·v` and the result is rounded to the nearest 8-bit value. In
//! float the same expressions are evaluated on `[0, 1]` alpha (clamped)
//! in `f64` and rounded to `f32`; colours are not clamped. Two pixels
//! are *equal* when their samples are identical or both are fully
//! transparent — the colour under a zero alpha is never shown and never
//! reaches another pixel through OVER.
//!
//! Disposal, blending and canvas semantics follow the animation
//! specifications: the canvas starts as the background (transparent
//! black by default — PNG 3rd edition §11.3.6.2 "fully transparent
//! black", RFC 9649 §2.7.2 "background color … or an
//! application-defined color"), a picture is drawn with its [`Blend`]
//! (the non-premultiplied OVER of RFC 9649 §2.7.1.1), then disposed
//! (GIF89a §23.c.iv / fcTL `dispose_op` / ANMF D bit).
//! [`BackgroundRule::FirstFrameAlpha`] swaps the background part for
//! the rule the `magick` binary applies.
//!
//! Two of the comparisons use a *visibility threshold*: a pixel counts
//! as transparent when its alpha is below one half (`< 32768` at
//! 16 bits, `< 128` at 8), which is what ImageMagick does, and is exact
//! for GIF-style 0 / 255 alpha. *Cleared* pixels are visible in the
//! earlier picture and transparent in the later one; *overlaid* pixels
//! changed and are visible in the later one.
//!
//! **A change of canvas** — a frame whose canvas (its [`Page`] size, or
//! its own size when it declares none) differs from the frame before
//! it, as in a picture sequence whose size changed mid-stream — starts
//! a new canvas: the frames before it and after it are never composited
//! or compared across the change. [`coalesce`], [`apply_disposal`] and
//! [`optimize_transparency`] restart from the background on the new
//! canvas; [`compare_any`] / [`compare_clear`] / [`compare_overlay`]
//! keep the first frame of the new canvas whole, as they keep frame 0;
//! [`optimize_frame`] / [`optimize_plus`] optimise each run of one
//! canvas on its own. [`flatten`], [`mosaic`], [`merge`] and
//! [`composite_layers`] produce one canvas by definition and clip as
//! documented on each.

use std::rc::Rc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{Blend, Dispose, Page};

mod edit;
mod exact;
mod raster;
mod sequence;

pub use edit::{
    Direction, append, append_with, clip_to_canvas, repage, reset_page, reverse, scale_delays,
    select, set_canvas, set_delay, set_delay_ticks, set_loop_count, total_delay, translate, trim,
    trim_to,
};
pub use exact::{OptimizeOptions, optimize_layers, optimize_layers_with};
pub use sequence::LayerSequence;

use raster::{Base, Depth, Patch, Pred, Px, Raster, Rect, scan};

/// Settings shared by the layer operations.
///
/// `#[non_exhaustive]`: start from [`new`](Self::new) /
/// [`Default`] and the `with_*` builders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LayerOptions {
    /// The background colour, straight (non-premultiplied) RGBA at
    /// 16 bits per channel: the initial canvas of [`coalesce`] /
    /// [`flatten`] / [`mosaic`] / [`merge`] and what
    /// [`Dispose::Background`] restores. Default transparent black.
    pub background: [u16; 4],
    /// How [`background`](Self::background) seeds a played canvas and
    /// what [`Dispose::Background`] restores; see [`BackgroundRule`].
    pub background_rule: BackgroundRule,
    /// The background for float canvases, as `f32` bits (kept as bits
    /// so the options stay `Eq`); `None` = `background / 65535`.
    background_float: Option<[u32; 4]>,
    /// Threads an operation may use; `0` and `1` are serial.
    threads: usize,
}

/// How the layer operations that play a list ([`coalesce`],
/// [`apply_disposal`], [`optimize_transparency`], [`optimize`]) use
/// [`LayerOptions::background`]. [`flatten`], [`mosaic`], [`merge`]
/// fill their canvas with the background as given under every rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackgroundRule {
    /// The canvas starts as the background colour exactly as given, and
    /// [`Dispose::Background`] restores it (the animation
    /// specifications' model: PNG 3rd edition §11.3.6.2, RFC 9649
    /// §2.7.2). The default.
    #[default]
    Fill,
    /// The rule the `magick` binary (7.1.2) applies, for callers that
    /// must reproduce `Imagick` results: [`coalesce`] /
    /// [`apply_disposal`] start each canvas as the background colour,
    /// *opaque* when the first frame of the canvas has no alpha
    /// ([`Image::has_alpha`] false) and *fully transparent* (alpha `0`,
    /// colour kept) when it has; [`optimize_transparency`] starts from
    /// fully transparent whatever the background; and
    /// [`Dispose::Background`] clears the rectangle to fully
    /// transparent (each pixel's colour kept, alpha `0`).
    FirstFrameAlpha,
}

impl LayerOptions {
    /// Transparent-black background.
    pub fn new() -> Self {
        Self::default()
    }

    /// Background from 8-bit RGBA (`[255, 255, 255, 255]` = opaque
    /// white).
    pub fn with_background_rgba8(mut self, rgba: [u8; 4]) -> Self {
        self.background = rgba.map(|c| u16::from(c) * 257);
        self
    }

    /// Background from 16-bit RGBA.
    pub fn with_background_rgba16(mut self, rgba: [u16; 4]) -> Self {
        self.background = rgba;
        self
    }

    /// The [`BackgroundRule`] of the played canvases.
    pub fn with_background_rule(mut self, rule: BackgroundRule) -> Self {
        self.background_rule = rule;
        self
    }

    /// Background for float canvases (`RgbaF32Le` lists), straight
    /// RGBA: values the 16-bit field cannot hold (above `1.0`, or finer
    /// than 1 / 65535). Integer canvases keep using
    /// [`background`](Self::background).
    pub fn with_background_rgba_f32(mut self, rgba: [f32; 4]) -> Self {
        self.background_float = Some(rgba.map(f32::to_bits));
        self
    }

    /// The float background set by
    /// [`with_background_rgba_f32`](Self::with_background_rgba_f32).
    pub fn background_rgba_f32(&self) -> Option<[f32; 4]> {
        self.background_float.map(|b| b.map(f32::from_bits))
    }

    /// Let the operations that compare whole canvases split them into
    /// row bands on up to `n` threads. `0` and `1` (the default) are
    /// serial; the result is the same at every budget.
    pub fn with_threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }

    /// [`with_threads`](Self::with_threads) from the framework's
    /// execution budget.
    pub fn with_execution_context(self, ctx: &oxideav_core::ExecutionContext) -> Self {
        self.with_threads(ctx.threads)
    }

    /// The thread budget (at least 1).
    pub fn threads(&self) -> usize {
        self.threads.max(1)
    }

    /// The background at `depth`.
    fn px(&self, depth: Depth) -> Px {
        match (depth, self.background_rgba_f32()) {
            (Depth::F32, Some(c)) => Px::from_f32(c),
            _ => Px::from_rgba16(depth, self.background),
        }
    }

    /// The colour a played canvas whose first frame is `first` starts
    /// as. `transparent_start` = the operation starts from fully
    /// transparent under [`BackgroundRule::FirstFrameAlpha`].
    fn canvas_start(&self, first: &Image, transparent_start: bool, depth: Depth) -> Px {
        let bg = self.px(depth);
        match self.background_rule {
            BackgroundRule::FirstFrameAlpha => {
                bg.with_alpha(depth, !(transparent_start || first.has_alpha()))
            }
            _ => bg,
        }
    }

    /// [`Dispose::Background`] of rectangle `r` on `canvas`.
    fn dispose_background(&self, canvas: &mut Raster, r: Rect) {
        match self.background_rule {
            BackgroundRule::FirstFrameAlpha => canvas.clear_alpha(r),
            _ => canvas.fill(r, self.px(canvas.depth)),
        }
    }
}

/// The `-layers` methods that take one frame list, for callers that
/// dispatch on a method value (`Imagick::LAYERMETHOD_*`); see
/// [`layers`]. [`composite_layers`] takes two lists and is called
/// directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LayerMethod {
    /// [`coalesce`].
    Coalesce,
    /// [`apply_disposal`].
    Dispose,
    /// [`compare_any`].
    CompareAny,
    /// [`compare_clear`].
    CompareClear,
    /// [`compare_overlay`].
    CompareOverlay,
    /// [`optimize`].
    Optimize,
    /// [`optimize_frame`].
    OptimizeFrame,
    /// [`optimize_plus`].
    OptimizePlus,
    /// [`optimize_transparency`].
    OptimizeTransparency,
    /// [`remove_dups`].
    RemoveDups,
    /// [`remove_zero`].
    RemoveZero,
    /// [`flatten`] (one picture).
    Flatten,
    /// [`mosaic`] (one picture).
    Mosaic,
    /// [`merge`] (one picture).
    Merge,
    /// [`trim_bounds`].
    TrimBounds,
}

/// Run `method` on `frames`. The flattening methods return a
/// one-picture list.
pub fn layers(frames: &[Image], method: LayerMethod, opts: &LayerOptions) -> Result<Vec<Image>> {
    use LayerMethod as M;
    match method {
        M::Coalesce => coalesce_with(frames, opts),
        M::Dispose => apply_disposal_with(frames, opts),
        M::CompareAny => compare_any(frames),
        M::CompareClear => compare_clear(frames),
        M::CompareOverlay => compare_overlay(frames),
        M::Optimize => optimize_with(frames, opts),
        M::OptimizeFrame => optimize_frames(frames, false, "optimize-frame", opts.threads()),
        M::OptimizePlus => optimize_frames(frames, true, "optimize-plus", opts.threads()),
        M::OptimizeTransparency => optimize_transparency_with(frames, opts),
        M::RemoveDups => remove_dups(frames),
        M::RemoveZero => Ok(remove_zero(frames)),
        M::Flatten => flatten_with(frames, opts).map(|i| vec![i]),
        M::Mosaic => mosaic_with(frames, opts).map(|i| vec![i]),
        M::Merge => merge_with(frames, opts).map(|i| vec![i]),
        M::TrimBounds => Ok(trim_bounds(frames)),
    }
}

// ---- helpers ----------------------------------------------------------------

fn need_frames(frames: &[Image], what: &str) -> Result<()> {
    if frames.is_empty() {
        return Err(Error::invalid(format!("{what}: no frame")));
    }
    Ok(())
}

/// The picture's rectangle on its canvas.
fn frame_rect(img: &Image) -> Rect {
    let (x, y) = img.offset();
    Rect::new(
        i64::from(x),
        i64::from(y),
        i64::from(img.width()),
        i64::from(img.height()),
    )
}

/// A picture of `pixels` carrying `template`'s timing, blobs, layer
/// record and stream parameters.
fn raster_image(template: &Image, pixels: Raster) -> Result<Image> {
    let (w, h, format) = (pixels.w as u32, pixels.h as u32, pixels.depth.format());
    template.with_pixels(w, h, format, pixels.into_bytes())
}

/// Window `r` (canvas coordinates) of `img` as its own picture, in the
/// picture's layout when [`Image::crop`] handles it, else as RGBA; the
/// page keeps the canvas and moves the offset to `r`, and the window is
/// an overlay ([`Blend::Over`]).
fn crop_frame(img: &Image, r: Rect, depth: Depth) -> Result<Image> {
    let fr = frame_rect(img);
    let (lx, ly) = (r.x - fr.x, r.y - fr.y);
    let (cw, ch) = img.canvas_size();
    let page = Page::new(cw, ch).with_offset(r.x as i32, r.y as i32);
    let inside = lx >= 0 && ly >= 0 && lx + r.w <= fr.w && ly + r.h <= fr.h;
    if inside && let Ok(c) = img.crop(lx as u32, ly as u32, r.w as u32, r.h as u32) {
        return Ok(c.with_page(page).with_blend(Blend::Over));
    }
    let window = Raster::of(img, depth)?.crop(Rect::new(lx, ly, r.w, r.h));
    Ok(raster_image(img, window)?
        .with_page(page)
        .with_blend(Blend::Over))
}

/// The picture that stands for "no change": one transparent pixel at
/// `-1-1`.
fn empty_frame(template: &Image, canvas: (u32, u32), depth: Depth) -> Result<Image> {
    let pixel = Raster::filled(1, 1, depth, Px::clear(depth))?;
    Ok(raster_image(template, pixel)?
        .with_page(Page::new(canvas.0, canvas.1).with_offset(-1, -1))
        .with_blend(Blend::Over))
}

fn add_delay(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or_default() + b.unwrap_or_default()),
    }
}

// ---- playback ---------------------------------------------------------------

/// `true` when frame `i` starts a new canvas: its canvas size is not
/// the one of the frame before it (module docs, *a change of canvas*).
fn new_canvas(frames: &[Image], i: usize) -> bool {
    i > 0 && frames[i].canvas_size() != frames[i - 1].canvas_size()
}

/// The two moments of a frame a player reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The frame has been drawn: the canvas as displayed.
    Shown,
    /// The frame has been disposed: the canvas the next frame is drawn
    /// onto.
    Disposed,
}

/// Play `frames` on a canvas the size of the first frame's canvas,
/// starting from the background — and on a fresh background canvas at
/// every change of canvas: for each frame, `visit(index, stage, canvas,
/// whether the frame started a new canvas)` once it is drawn and once
/// it is disposed. Only the frame's rectangle is touched at each step.
fn play(
    frames: &[Image],
    opts: &LayerOptions,
    depth: Depth,
    transparent_start: bool,
    mut visit: impl FnMut(usize, Stage, &Raster<'static>, bool) -> Result<()>,
) -> Result<()> {
    let start = |f: &Image| -> Result<Raster<'static>> {
        let (cw, ch) = f.canvas_size();
        Raster::filled(
            cw,
            ch,
            depth,
            opts.canvas_start(f, transparent_start, depth),
        )
    };
    let mut canvas = start(&frames[0])?;
    for (i, f) in frames.iter().enumerate() {
        let fresh = new_canvas(frames, i);
        if fresh {
            canvas = start(f)?;
        }
        let r = frame_rect(f);
        // On the first frame the state before it is the initial
        // background (APNG: "treated as APNG_DISPOSE_OP_BACKGROUND").
        let saved = match (f.dispose(), r.clip(canvas.w, canvas.h)) {
            (Dispose::Previous, Some(inside)) => Some((inside, canvas.save(inside))),
            _ => None,
        };
        canvas.draw(&Raster::of(f, depth)?, r.x, r.y, f.blend());
        visit(i, Stage::Shown, &canvas, fresh)?;
        match f.dispose() {
            Dispose::Background => opts.dispose_background(&mut canvas, r),
            Dispose::Previous => {
                if let Some((inside, bytes)) = &saved {
                    canvas.restore(*inside, bytes);
                }
            }
            _ => {}
        }
        visit(i, Stage::Disposed, &canvas, fresh)?;
    }
    Ok(())
}

/// The canvas pixels a frame's disposal changes.
fn disposed_rect(f: &Image) -> Option<Rect> {
    matches!(f.dispose(), Dispose::Background | Dispose::Previous).then(|| frame_rect(f))
}

/// Full-canvas `pixels` built on `template`'s attributes: a frame that
/// replaces the canvas ([`Blend::Source`]), as a decoded frame does.
fn canvas_frame(template: &Image, pixels: &Raster, threads: usize) -> Result<Image> {
    let copy = Raster::from_bytes(pixels.w, pixels.h, pixels.depth, pixels.copy_bytes(threads));
    Ok(raster_image(template, copy)?
        .with_page(Page::new(pixels.w as u32, pixels.h as u32))
        .with_blend(Blend::Source))
}

// ---- coalesce / dispose -----------------------------------------------------

/// Render every frame onto the full canvas, honouring each frame's
/// offset, blending and disposal: the animation as it is displayed,
/// one full-canvas picture per frame (page `WxH+0+0`). Transparent-black
/// background; [`coalesce_with`] sets another.
///
/// Every output frame replaces the canvas ([`Blend::Source`]), so a
/// coalesced list plays back to itself at every alpha value. The
/// frames are also labelled with the disposal under which drawing them
/// as overlays would show the same animation when alpha is all or
/// nothing: [`Dispose::Background`] where the next frame clears a
/// visible pixel, else [`Dispose::None`] (the last frame repeats the
/// previous frame's choice).
pub fn coalesce(frames: &[Image]) -> Result<Vec<Image>> {
    coalesce_with(frames, &LayerOptions::default())
}

/// [`coalesce`] on `opts.background`.
pub fn coalesce_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    need_frames(frames, "coalesce")?;
    let depth = Depth::of(frames);
    let mut out: Vec<Image> = Vec::with_capacity(frames.len());
    play(frames, opts, depth, false, |i, stage, canvas, fresh| {
        if stage != Stage::Shown {
            return Ok(());
        }
        // Across a change of canvas nothing carries over: the last
        // frame of the old canvas keeps `None`. Otherwise the canvas
        // differs from the one shown before only where the frame before
        // was disposed and where this one was drawn.
        if !fresh && i > 0 {
            let windows = [disposed_rect(&frames[i - 1]), Some(frame_rect(&frames[i]))];
            let clears = {
                let before = Raster::of(&out[i - 1], depth)?;
                windows
                    .into_iter()
                    .flatten()
                    .any(|w| scan(&Base::of(&before), canvas, Some(w), Pred::Cleared, 1).is_some())
            };
            out[i - 1].set_dispose(if clears {
                Dispose::Background
            } else {
                Dispose::None
            });
        }
        out.push(canvas_frame(&frames[i], canvas, opts.threads())?.with_dispose(Dispose::None));
        Ok(())
    })?;
    let n = out.len();
    if n > 1 {
        let d = out[n - 2].dispose();
        out[n - 1].set_dispose(d);
    }
    Ok(out)
}

/// Every frame as the canvas looks once that frame has been disposed —
/// the state the next frame is drawn onto — full canvas,
/// [`Dispose::None`].
///
/// ImageMagick 7.1.2's `-layers dispose` returns the first frame's
/// canvas for every frame; this follows the disposal model instead
/// (README, *Layer operations*).
pub fn apply_disposal(frames: &[Image]) -> Result<Vec<Image>> {
    apply_disposal_with(frames, &LayerOptions::default())
}

/// [`apply_disposal`] on `opts.background`.
pub fn apply_disposal_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    need_frames(frames, "dispose")?;
    let depth = Depth::of(frames);
    let mut out = Vec::with_capacity(frames.len());
    play(frames, opts, depth, false, |i, stage, canvas, _| {
        if stage == Stage::Disposed {
            out.push(canvas_frame(&frames[i], canvas, opts.threads())?.with_dispose(Dispose::None));
        }
        Ok(())
    })?;
    Ok(out)
}

// ---- compare ----------------------------------------------------------------

/// Each frame after the first cropped to the pixels of its rectangle
/// that differ from the canvas the frames before it leave — every
/// earlier frame drawn at its offset replacing what it covers, on a
/// transparent canvas, no disposal applied: ImageMagick's
/// `CompareAny`, `deconstructImages`.
/// The first frame is kept as is; a frame identical to its predecessor
/// has nothing to keep and is dropped, as ImageMagick drops it.
pub fn compare_any(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Pred::Changed)
}

/// `Imagick::deconstructImages`: [`compare_any`].
pub fn deconstruct(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Pred::Changed)
}

/// As [`compare_any`], cropped to the pixels that are visible in the
/// earlier frame and transparent in the later one (what must be erased
/// to go from one to the next).
pub fn compare_clear(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Pred::Cleared)
}

/// As [`compare_any`], cropped to the changed pixels that are visible
/// in the later frame (what must be painted over the earlier one).
pub fn compare_overlay(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Pred::Overlaid)
}

fn compare(frames: &[Image], how: Pred) -> Result<Vec<Image>> {
    need_frames(frames, "compare")?;
    let depth = Depth::of(frames);
    let mut out = vec![frames[0].clone()];
    // Every earlier frame of the canvas, each replacing its rectangle
    // (no disposal, no blending): what the current frame is compared
    // against.
    let (cw, ch) = frames[0].canvas_size();
    let mut before = Raster::filled(cw, ch, depth, Px::clear(depth))?;
    for pair in frames.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let (cw, ch) = cur.canvas_size();
        if prev.canvas_size() != (cw, ch) {
            // A new canvas: nothing to compare against, kept whole.
            before = Raster::filled(cw, ch, depth, Px::clear(depth))?;
            out.push(cur.clone());
            continue;
        }
        let pr = frame_rect(prev);
        before.draw(&Raster::of(prev, depth)?, pr.x, pr.y, Blend::Source);
        let r = frame_rect(cur);
        let Some(win) = r.clip(before.w, before.h) else {
            continue;
        };
        // The window of the canvas and of the frame, side by side.
        let under = before.crop(win);
        let over = Raster::of(cur, depth)?.crop(Rect::new(win.x - r.x, win.y - r.y, win.w, win.h));
        if let Some(b) = scan(&Base::of(&under), &over, None, how, 1) {
            out.push(crop_frame(
                cur,
                Rect::new(win.x + b.x, win.y + b.y, b.w, b.h),
                depth,
            )?);
        }
    }
    Ok(out)
}

// ---- optimize ---------------------------------------------------------------

/// One output frame of the optimisers, before it is cut.
struct Planned {
    /// Input frame the pixels come from.
    src: usize,
    /// Canvas window (`None` = the empty 1×1 at -1-1).
    rect: Option<Rect>,
    /// Decided disposal (`None` until the next frame decides it).
    dispose: Option<Dispose>,
    delay: Option<Duration>,
}

/// A canvas the optimiser reasons about without building it: a
/// coalesced input frame (or the empty canvas) with a rectangle of it
/// cleared.
#[derive(Clone, Copy)]
struct State {
    /// Input frame; `None` = the empty canvas.
    src: Option<usize>,
    /// Cleared rectangle.
    fill: Option<Rect>,
}

/// The rasters of the frames an optimiser has in hand: the current
/// frame, the one before and the one a pending `Previous` goes back to.
/// An `Rgba` frame is borrowed; any other is converted once.
struct Views<'a> {
    frames: &'a [Image],
    depth: Depth,
    held: Vec<(usize, Rc<Raster<'a>>)>,
}

impl<'a> Views<'a> {
    fn get(&mut self, i: usize) -> Result<Rc<Raster<'a>>> {
        if let Some((_, r)) = self.held.iter().find(|(k, _)| *k == i) {
            return Ok(r.clone());
        }
        let r = Rc::new(Raster::of(&self.frames[i], self.depth)?);
        self.held.push((i, r.clone()));
        Ok(r)
    }

    fn keep(&mut self, live: &[Option<usize>]) {
        self.held.retain(|(k, _)| live.contains(&Some(*k)));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChoiceKind {
    None,
    Previous,
    /// OptimizePlus only: keep the last frame (`None`, no delay) and
    /// add a clearing frame over the window with `Background`.
    Split(Rect),
    /// Background, the last frame's window grown to `Option<Rect>`.
    Background(Option<Rect>),
}

/// The disposal choice for the transition from coalesced frame `prev`
/// to `cur`: what to do with the last planned frame, and the window the
/// next one needs. `before_last` is the canvas the last planned frame
/// was drawn onto and `dirty` the box of the pixels where `cur` differs
/// from `prev`.
///
/// Every canvas compared here is `prev` or `before_last`, possibly
/// with a rectangle cleared; `before_last` differs from `prev` only
/// inside `last_rect` and the cleared rectangles lie inside `dirty` or
/// `last_rect`, so the comparisons look at those two boxes only.
fn choose(
    prev: &Raster,
    cur: &Raster,
    before_last: &Base,
    last_rect: Option<Rect>,
    dirty: Option<Rect>,
    plus: bool,
    threads: usize,
) -> (ChoiceKind, Option<Rect>) {
    let clear_px = Px::clear(cur.depth);
    let after = |fill: Option<Rect>| Base {
        src: Some(prev),
        uniform: clear_px,
        fill: fill.map(|r| (r, Patch::Colour(clear_px))),
    };
    let window = Rect::union(dirty, last_rect);
    let diff = |base: &Base, pred| window.and_then(|w| scan(base, cur, Some(w), pred, threads));
    let clear = diff(&after(None), Pred::Cleared);
    let mut best: Option<(u64, ChoiceKind, Option<Rect>)> = None;
    let mut offer = |cost: u64, kind: ChoiceKind, rect: Option<Rect>| {
        if best.as_ref().is_none_or(|(b, ..)| cost < *b) {
            best = Some((cost, kind, rect));
        }
    };
    if clear.is_none() {
        let rect = diff(&after(None), Pred::Changed);
        offer(Rect::area(rect), ChoiceKind::None, rect);
    }
    if diff(before_last, Pred::Cleared).is_none() {
        let rect = diff(before_last, Pred::Changed);
        offer(Rect::area(rect), ChoiceKind::Previous, rect);
    }
    if plus && let Some(cb) = clear {
        let rect = diff(&after(Some(cb)), Pred::Changed);
        offer(
            Rect::area(Some(cb)) + Rect::area(rect),
            ChoiceKind::Split(cb),
            rect,
        );
    }
    let grown = Rect::union(last_rect, clear);
    let rect = diff(&after(grown), Pred::Changed);
    let growth = Rect::area(grown).saturating_sub(Rect::area(last_rect));
    offer(
        Rect::area(rect) + growth,
        ChoiceKind::Background(grown),
        rect,
    );
    let (_, kind, rect) = best.expect("Background is always offered");
    (kind, rect)
}

fn check_coalesced(frames: &[Image], what: &str) -> Result<()> {
    need_frames(frames, what)?;
    for (i, f) in frames.iter().enumerate() {
        if !f.is_full_canvas() {
            return Err(Error::invalid(format!(
                "{what}: frame {i} has page {}, not a coalesced full-canvas frame (coalesce first)",
                f.page()
            )));
        }
    }
    Ok(())
}

/// Turn a coalesced animation (every frame full canvas — [`coalesce`]
/// output; a change of size starts a run optimised on its own) into the smallest frames it can, choosing each
/// frame's disposal: for every transition the candidates are
/// [`Dispose::None`] (when nothing visible has to be erased),
/// [`Dispose::Previous`] and [`Dispose::Background`] (the previous
/// window grown over what has to be erased), and the one giving the
/// smallest next window wins (ties in that order). A frame identical to
/// its predecessor becomes ImageMagick's 1×1 transparent frame at
/// `-1-1`. Output geometry and disposal match `magick -layers
/// optimize-frame` (README, *Layer operations*).
///
/// This optimiser takes the half-alpha view of transparency described
/// in the module docs; [`optimize_layers`] is the one that replays
/// exactly at every alpha.
pub fn optimize_frame(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_frames(frames, false, "optimize-frame", 1)
}

/// [`optimize_frame`] that may also split a transition in two — the
/// frame kept with no delay, then a clearing frame over just the pixels
/// to erase (with the delay and [`Dispose::Background`]) — when that
/// costs no more than the best single disposal, and that folds a frame
/// identical to its predecessor into it (delays added). ImageMagick's
/// `OptimizePlus`; the delays here always add up to the input's
/// (README, *Layer operations*).
pub fn optimize_plus(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_frames(frames, true, "optimize-plus", 1)
}

fn optimize_frames(frames: &[Image], plus: bool, what: &str, threads: usize) -> Result<Vec<Image>> {
    check_coalesced(frames, what)?;
    // Each run of one canvas on its own (module docs, *a change of
    // canvas*); the layout choice is the whole list's.
    let depth = Depth::of(frames);
    let mut out = Vec::with_capacity(frames.len());
    let mut start = 0;
    for i in 1..=frames.len() {
        if i == frames.len() || new_canvas(frames, i) {
            out.extend(optimize_run(&frames[start..i], plus, depth, threads)?);
            start = i;
        }
    }
    Ok(out)
}

/// [`optimize_frames`] on frames of one canvas.
fn optimize_run(frames: &[Image], plus: bool, depth: Depth, threads: usize) -> Result<Vec<Image>> {
    let (w, h) = (frames[0].width(), frames[0].height());
    let clear_px = Px::clear(depth);
    let mut views = Views {
        frames,
        depth,
        held: Vec::new(),
    };
    let empty = Base {
        src: None,
        uniform: clear_px,
        fill: None,
    };
    let mut prev = views.get(0)?;
    // The canvas the last planned frame was drawn onto.
    let mut before_last = State {
        src: None,
        fill: None,
    };
    let mut plan = vec![Planned {
        src: 0,
        rect: scan(&empty, &prev, None, Pred::Changed, threads),
        dispose: None,
        delay: frames[0].delay(),
    }];
    for (i, f) in frames.iter().enumerate().skip(1) {
        let cur = views.get(i)?;
        let last = plan.len() - 1;
        let dirty = scan(&Base::of(&prev), &cur, None, Pred::Changed, threads);
        if plus && i >= 2 && dirty.is_none() {
            plan[last].delay = add_delay(plan[last].delay, f.delay());
            plan[last].dispose = Some(Dispose::None);
            continue;
        }
        let under = match before_last.src {
            Some(k) => Some(views.get(k)?),
            None => None,
        };
        let base = Base {
            src: under.as_deref(),
            uniform: clear_px,
            fill: before_last.fill.map(|r| (r, Patch::Colour(clear_px))),
        };
        let (kind, rect) = choose(&prev, &cur, &base, plan[last].rect, dirty, plus, threads);
        match kind {
            ChoiceKind::None => {
                plan[last].dispose = Some(Dispose::None);
                before_last = State {
                    src: Some(i - 1),
                    fill: None,
                };
            }
            ChoiceKind::Previous => plan[last].dispose = Some(Dispose::Previous),
            ChoiceKind::Background(grown) => {
                plan[last].dispose = Some(Dispose::Background);
                plan[last].rect = grown;
                before_last = State {
                    src: Some(i - 1),
                    fill: grown,
                };
            }
            ChoiceKind::Split(cb) => {
                let delay = plan[last].delay;
                plan[last].dispose = Some(Dispose::None);
                plan[last].delay = Some(Duration::ZERO);
                plan.push(Planned {
                    src: i - 1,
                    rect: Some(cb),
                    dispose: Some(Dispose::Background),
                    delay,
                });
                before_last = State {
                    src: Some(i - 1),
                    fill: Some(cb),
                };
            }
        }
        plan.push(Planned {
            src: i,
            rect,
            dispose: None,
            delay: f.delay(),
        });
        prev = cur;
        views.keep(&[Some(i), before_last.src]);
    }
    let n = plan.len();
    if plan[n - 1].dispose.is_none() {
        plan[n - 1].dispose = Some(if n > 1 {
            plan[n - 2].dispose.unwrap_or(Dispose::None)
        } else {
            Dispose::None
        });
    }
    plan.into_iter()
        .map(|p| {
            let f = &frames[p.src];
            let img = match p.rect {
                Some(r) => crop_frame(f, r, depth)?,
                None => empty_frame(f, (w, h), depth)?,
            };
            Ok(img
                .with_dispose(p.dispose.unwrap_or(Dispose::None))
                .with_delay(p.delay))
        })
        .collect()
}

/// Make transparent every pixel of every overlay frame
/// ([`Blend::Over`]) that leaves the canvas pixel under it unchanged
/// (the frames played in order with their offsets, blending and
/// disposal): the animation looks the same and a GIF / palette encoder
/// sees long runs of one transparent index. ImageMagick's
/// `OptimizeTransparency`; for opaque and fully transparent pixels the
/// two agree, for semi-transparent ones ImageMagick also drops a pixel
/// equal to the canvas pixel, which changes the picture, and the
/// gateway does not (README, *Layer operations*). Frames come out
/// in the RGBA layout of the list's depth.
pub fn optimize_transparency(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_transparency_with(frames, &LayerOptions::default())
}

/// [`optimize_transparency`] on `opts.background`.
pub fn optimize_transparency_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    need_frames(frames, "optimize-transparency")?;
    let depth = Depth::of(frames);
    let bpp = depth.bpp();
    let (cw, ch) = frames[0].canvas_size();
    let mut canvas = Raster::filled(cw, ch, depth, opts.canvas_start(&frames[0], true, depth))?;
    let mut out = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        if new_canvas(frames, i) {
            let (cw, ch) = f.canvas_size();
            canvas = Raster::filled(cw, ch, depth, opts.canvas_start(f, true, depth))?;
        }
        let src = Raster::of(f, depth)?;
        let r = frame_rect(f);
        let inside = r.clip(canvas.w, canvas.h);
        let saved = match (f.dispose(), inside) {
            (Dispose::Previous, Some(win)) => Some((win, canvas.save(win))),
            _ => None,
        };
        let mut kept = src.bytes().to_vec();
        // Only an overlay can leave a pixel alone: a `Source` frame's
        // transparent pixel would erase the canvas.
        if f.blend() == Blend::Over
            && let Some(win) = inside
        {
            let (x0, y0) = ((win.x - r.x) as usize, (win.y - r.y) as usize);
            let (ww, wh) = (win.w as usize, win.h as usize);
            for row in 0..wh {
                let under = canvas.span(win.y as usize + row, win.x as usize..win.x as usize + ww);
                let at = ((y0 + row) * src.w + x0) * bpp;
                raster::drop_unchanged(depth, &mut kept[at..at + ww * bpp], under);
            }
        }
        canvas.draw(&src, r.x, r.y, f.blend());
        match f.dispose() {
            Dispose::Background => opts.dispose_background(&mut canvas, r),
            Dispose::Previous => {
                if let Some((win, bytes)) = &saved {
                    canvas.restore(*win, bytes);
                }
            }
            _ => {}
        }
        out.push(raster_image(
            f,
            Raster::from_bytes(src.w, src.h, depth, kept),
        )?);
    }
    Ok(out)
}

/// The general-purpose GIF optimisation: [`coalesce`], then
/// [`optimize_frame`], then [`optimize_transparency`] — ImageMagick's
/// `-layers optimize`.
pub fn optimize(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_with(frames, &LayerOptions::default())
}

/// [`optimize`] on `opts.background`.
pub fn optimize_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    let coalesced = coalesce_with(frames, opts)?;
    let framed = optimize_frames(&coalesced, false, "optimize-frame", opts.threads())?;
    drop(coalesced);
    optimize_transparency_with(&framed, opts)
}

// ---- list clean-up ----------------------------------------------------------

/// `true` when the two pictures have the same size and equal pixels.
fn same_pixels(a: &Image, b: &Image, depth: Depth) -> Result<bool> {
    if (a.width(), a.height()) != (b.width(), b.height()) {
        return Ok(false);
    }
    let (ra, rb) = (Raster::of(a, depth)?, Raster::of(b, depth)?);
    Ok(scan(&Base::of(&ra), &rb, None, Pred::Changed, 1).is_none())
}

/// Fold every run of consecutive identical frames (same size, offset
/// and pixels) into its last frame, the delays added: ImageMagick's
/// `RemoveDups`.
pub fn remove_dups(frames: &[Image]) -> Result<Vec<Image>> {
    let depth = Depth::of(frames);
    let mut out: Vec<Image> = Vec::with_capacity(frames.len());
    for f in frames {
        match out.last() {
            Some(prev) if prev.offset() == f.offset() && same_pixels(prev, f, depth)? => {
                let delay = add_delay(prev.delay(), f.delay());
                out.pop();
                out.push(f.clone().with_delay(delay));
            }
            _ => out.push(f.clone()),
        }
    }
    Ok(out)
}

/// Drop every frame without a delay (zero or none) — the intermediate
/// frames an optimiser adds — unless no frame has one, when the list is
/// returned unchanged: ImageMagick's `RemoveZero`.
pub fn remove_zero(frames: &[Image]) -> Vec<Image> {
    let timed = |f: &&Image| f.delay().is_some_and(|d| !d.is_zero());
    if !frames.iter().any(|f| timed(&f)) {
        return frames.to_vec();
    }
    frames.iter().filter(timed).cloned().collect()
}

// ---- flatten / mosaic / merge / trim ---------------------------------------

/// Composite every frame, at its offset with its blending, onto one
/// canvas the size of the first frame's canvas filled with the
/// background; frames outside it are clipped. One full-canvas picture
/// at `+0+0` (the first frame's attributes, [`Dispose::Unspecified`]).
pub fn flatten(frames: &[Image]) -> Result<Image> {
    flatten_with(frames, &LayerOptions::default())
}

/// [`flatten`] on `opts.background`.
pub fn flatten_with(frames: &[Image], opts: &LayerOptions) -> Result<Image> {
    need_frames(frames, "flatten")?;
    let (w, h) = frames[0].canvas_size();
    compose_onto(frames, opts, w, h, 0, 0, Page::new(w, h))
}

/// As [`flatten`], the canvas first grown right and down until every
/// frame fits (negative offsets still clip at the top-left).
pub fn mosaic(frames: &[Image]) -> Result<Image> {
    mosaic_with(frames, &LayerOptions::default())
}

/// [`mosaic`] on `opts.background`.
pub fn mosaic_with(frames: &[Image], opts: &LayerOptions) -> Result<Image> {
    need_frames(frames, "mosaic")?;
    let (mut w, mut h) = frames[0].canvas_size();
    for f in frames {
        let r = frame_rect(f);
        w = w.max(u32::try_from(r.x + r.w).unwrap_or(0));
        h = h.max(u32::try_from(r.y + r.h).unwrap_or(0));
    }
    compose_onto(frames, opts, w, h, 0, 0, Page::new(w, h))
}

/// As [`flatten`] on a canvas just large enough for every frame,
/// negative offsets included: the result keeps the first frame's canvas
/// size on its page and sits at the frames' top-left corner (which may
/// be negative).
pub fn merge(frames: &[Image]) -> Result<Image> {
    merge_with(frames, &LayerOptions::default())
}

/// [`merge`] on `opts.background`.
pub fn merge_with(frames: &[Image], opts: &LayerOptions) -> Result<Image> {
    need_frames(frames, "merge")?;
    let b = frames
        .iter()
        .map(|f| Some(frame_rect(f)))
        .fold(None, Rect::union)
        .expect("one frame at least");
    let (cw, ch) = frames[0].canvas_size();
    let page = Page::new(cw, ch).with_offset(b.x as i32, b.y as i32);
    compose_onto(frames, opts, b.w as u32, b.h as u32, b.x, b.y, page)
}

fn compose_onto(
    frames: &[Image],
    opts: &LayerOptions,
    w: u32,
    h: u32,
    ox: i64,
    oy: i64,
    page: Page,
) -> Result<Image> {
    let depth = Depth::of(frames);
    let mut canvas = Raster::filled(w, h, depth, opts.px(depth))?;
    for f in frames {
        let r = frame_rect(f);
        canvas.draw(&Raster::of(f, depth)?, r.x - ox, r.y - oy, f.blend());
    }
    Ok(raster_image(&frames[0], canvas)?
        .with_page(page)
        .with_dispose(Dispose::Unspecified)
        .with_blend(Blend::Over))
}

/// Shift every frame so the frames' common bounding box starts at
/// `+0+0` and make the canvas exactly that box (pixels untouched).
pub fn trim_bounds(frames: &[Image]) -> Vec<Image> {
    let Some(b) = frames
        .iter()
        .map(|f| Some(frame_rect(f)))
        .fold(None, Rect::union)
    else {
        return Vec::new();
    };
    frames
        .iter()
        .map(|f| {
            let r = frame_rect(f);
            f.clone().with_page(
                Page::new(b.w as u32, b.h as u32)
                    .with_offset((r.x - b.x) as i32, (r.y - b.y) as i32),
            )
        })
        .collect()
}

/// Composite the frames of `source` onto those of `destination`, pair by
/// pair until either list ends (the rest of `destination` is kept);
/// a one-frame list pairs with every frame of the other (a lone
/// destination frame is repeated per source frame). Each source frame
/// lands at its offset relative to the destination frame's, moved by
/// `(dx, dy)`, with its own blending; the destination's page is kept.
/// ImageMagick's `-layers composite`.
pub fn composite_layers(
    destination: &[Image],
    source: &[Image],
    dx: i32,
    dy: i32,
) -> Result<Vec<Image>> {
    need_frames(destination, "composite (destination)")?;
    need_frames(source, "composite (source)")?;
    let depth = match (Depth::of(destination), Depth::of(source)) {
        (Depth::F32, _) | (_, Depth::F32) => Depth::F32,
        (Depth::U16, _) | (_, Depth::U16) => Depth::U16,
        _ => Depth::U8,
    };
    let pairs: Vec<(&Image, &Image)> = if source.len() == 1 {
        destination.iter().map(|d| (d, &source[0])).collect()
    } else if destination.len() == 1 {
        source.iter().map(|s| (&destination[0], s)).collect()
    } else {
        destination.iter().zip(source).collect()
    };
    let mut out: Vec<Image> = Vec::new();
    for (d, s) in &pairs {
        let mut canvas = Raster::of(d, depth)?;
        let (dr, sr) = (frame_rect(d), frame_rect(s));
        canvas.draw(
            &Raster::of(s, depth)?,
            sr.x - dr.x + i64::from(dx),
            sr.y - dr.y + i64::from(dy),
            s.blend(),
        );
        out.push(raster_image(d, canvas)?);
    }
    if source.len() > 1 && destination.len() > source.len() {
        out.extend(destination[source.len()..].iter().cloned());
    }
    Ok(out)
}
