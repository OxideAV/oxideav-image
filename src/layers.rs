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
//! Every operation reads its frames through the picture's own layout
//! (any layout [`Image::to_rgba16`] reaches). Frames it builds from a
//! canvas are `Rgba` (8-bit) — `Rgba64Le` when any input is deeper than
//! eight bits; frames it only crops keep their layout (palette
//! included) where [`Image::crop`] can.
//!
//! Disposal, blending and canvas semantics follow the animation
//! specifications: the canvas starts as the background (transparent
//! black by default — PNG 3rd edition §11.3.6.2 "fully transparent
//! black", RFC 9649 §2.7.2 "background color … or an
//! application-defined color"), a picture is drawn with its [`Blend`]
//! (the non-premultiplied OVER of RFC 9649 §2.7.1.1), then disposed
//! (GIF89a §23.c.iv / fcTL `dispose_op` / ANMF D bit).
//!
//! Two of the comparisons use a *visibility threshold*: a pixel counts
//! as transparent when its alpha is below one half (`< 32768` at
//! 16 bits, `< 128` at 8), which is what ImageMagick does, and is exact
//! for GIF-style 0 / 255 alpha. *Cleared* pixels are visible in the
//! earlier picture and transparent in the later one; *overlaid* pixels
//! changed and are visible in the later one.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{Blend, Dispose, Page};
use oxideav_core::PixelFormat;

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
        M::OptimizeFrame => optimize_frame(frames),
        M::OptimizePlus => optimize_plus(frames),
        M::OptimizeTransparency => optimize_transparency_with(frames, opts),
        M::RemoveDups => remove_dups(frames),
        M::RemoveZero => Ok(remove_zero(frames)),
        M::Flatten => flatten_with(frames, opts).map(|i| vec![i]),
        M::Mosaic => mosaic_with(frames, opts).map(|i| vec![i]),
        M::Merge => merge_with(frames, opts).map(|i| vec![i]),
        M::TrimBounds => Ok(trim_bounds(frames)),
    }
}

// ---- pixels ---------------------------------------------------------------

/// Straight RGBA, 16 bits per channel.
type Px = [u16; 4];

const CLEAR: Px = [0; 4];

/// Visible at the half-alpha threshold.
fn visible(p: Px) -> bool {
    p[3] >= 0x8000
}

/// Equal pixels: identical, or both fully transparent (the colour of an
/// invisible pixel does not count).
fn same(a: Px, b: Px) -> bool {
    (a[3] == 0 && b[3] == 0) || a == b
}

/// `s` OVER `d`, non-premultiplied (RFC 9649 §2.7.1.1; PNG §13.16).
fn over(s: Px, d: Px) -> Px {
    if s[3] == u16::MAX || d[3] == 0 {
        return s;
    }
    if s[3] == 0 {
        return d;
    }
    let sa = f64::from(s[3]) / 65535.0;
    let da = f64::from(d[3]) / 65535.0;
    let oa = sa + da * (1.0 - sa);
    let mut out = [0u16; 4];
    for c in 0..3 {
        let v = (f64::from(s[c]) * sa + f64::from(d[c]) * da * (1.0 - sa)) / oa;
        out[c] = v.round().clamp(0.0, 65535.0) as u16;
    }
    out[3] = (oa * 65535.0).round().clamp(0.0, 65535.0) as u16;
    out
}

fn blend_px(blend: Blend, s: Px, d: Px) -> Px {
    match blend {
        Blend::Source => s,
        _ => over(s, d),
    }
}

/// An axis-aligned rectangle in canvas coordinates (`w`, `h` ≥ 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Rect {
    fn area(r: Option<Rect>) -> u64 {
        // An empty difference is ImageMagick's 1×1 at -1-1: it costs one
        // pixel like any 1×1 frame.
        r.map_or(1, |r| (r.w * r.h) as u64)
    }

    fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
        match (a, b) {
            (None, r) | (r, None) => r,
            (Some(a), Some(b)) => {
                let x = a.x.min(b.x);
                let y = a.y.min(b.y);
                let x2 = (a.x + a.w).max(b.x + b.w);
                let y2 = (a.y + a.h).max(b.y + b.h);
                Some(Rect {
                    x,
                    y,
                    w: x2 - x,
                    h: y2 - y,
                })
            }
        }
    }

    fn intersect(self, w: i64, h: i64) -> Option<Rect> {
        let x = self.x.max(0);
        let y = self.y.max(0);
        let x2 = (self.x + self.w).min(w);
        let y2 = (self.y + self.h).min(h);
        (x2 > x && y2 > y).then_some(Rect {
            x,
            y,
            w: x2 - x,
            h: y2 - y,
        })
    }
}

/// Growing bounding box.
#[derive(Default)]
struct Bounds(Option<(i64, i64, i64, i64)>);

impl Bounds {
    fn add(&mut self, x: i64, y: i64) {
        self.0 = Some(match self.0 {
            None => (x, y, x, y),
            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x), d.max(y)),
        });
    }

    fn rect(&self) -> Option<Rect> {
        self.0.map(|(x0, y0, x1, y1)| Rect {
            x: x0,
            y: y0,
            w: x1 - x0 + 1,
            h: y1 - y0 + 1,
        })
    }
}

/// A canvas or a picture's pixels, 16-bit straight RGBA.
#[derive(Clone, PartialEq)]
struct Raster {
    w: i64,
    h: i64,
    px: Vec<Px>,
}

/// Canvas pixels the layer operations allocate at most (a 16-bit RGBA
/// canvas is eight bytes a pixel).
const MAX_CANVAS_PIXELS: u64 = 1 << 30;

impl Raster {
    fn filled(w: u32, h: u32, fill: Px) -> Result<Self> {
        let n = u64::from(w) * u64::from(h);
        if n == 0 {
            return Err(Error::invalid(format!("canvas {w}x{h} has a zero side")));
        }
        if n > MAX_CANVAS_PIXELS {
            return Err(Error::limit(format!(
                "canvas {w}x{h} is over the {MAX_CANVAS_PIXELS}-pixel layer limit"
            )));
        }
        Ok(Self {
            w: i64::from(w),
            h: i64::from(h),
            px: vec![fill; n as usize],
        })
    }

    fn of(img: &Image) -> Result<Self> {
        let bytes = img.to_rgba16()?;
        let px = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                [
                    u16::from_le_bytes([c[0], c[1]]),
                    u16::from_le_bytes([c[2], c[3]]),
                    u16::from_le_bytes([c[4], c[5]]),
                    u16::from_le_bytes([c[6], c[7]]),
                ]
            })
            .collect();
        Ok(Self {
            w: i64::from(img.width()),
            h: i64::from(img.height()),
            px,
        })
    }

    fn at(&self, x: i64, y: i64) -> Px {
        self.px[(y * self.w + x) as usize]
    }

    fn put(&mut self, x: i64, y: i64, p: Px) {
        let w = self.w;
        self.px[(y * w + x) as usize] = p;
    }

    /// Draw `src` with its top-left corner at `(x, y)`, clipped.
    fn draw(&mut self, src: &Raster, x: i64, y: i64, blend: Blend) {
        let Some(r) = (Rect {
            x,
            y,
            w: src.w,
            h: src.h,
        })
        .intersect(self.w, self.h) else {
            return;
        };
        for cy in r.y..r.y + r.h {
            for cx in r.x..r.x + r.w {
                let s = src.at(cx - x, cy - y);
                let d = self.at(cx, cy);
                self.put(cx, cy, blend_px(blend, s, d));
            }
        }
    }

    fn fill(&mut self, r: Option<Rect>, p: Px) {
        let Some(r) = r.and_then(|r| r.intersect(self.w, self.h)) else {
            return;
        };
        for cy in r.y..r.y + r.h {
            for cx in r.x..r.x + r.w {
                self.put(cx, cy, p);
            }
        }
    }

    fn crop(&self, r: Rect) -> Raster {
        let mut px = Vec::with_capacity((r.w * r.h) as usize);
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let inside = x >= 0 && y >= 0 && x < self.w && y < self.h;
                px.push(if inside { self.at(x, y) } else { CLEAR });
            }
        }
        Raster { w: r.w, h: r.h, px }
    }

    /// Bounding box of the pixels `keep` selects, `a` against `b` (same
    /// geometry).
    fn diff(&self, b: &Raster, keep: impl Fn(Px, Px) -> bool) -> Option<Rect> {
        let mut bb = Bounds::default();
        for y in 0..self.h {
            for x in 0..self.w {
                if keep(self.at(x, y), b.at(x, y)) {
                    bb.add(x, y);
                }
            }
        }
        bb.rect()
    }

    /// `Rgba` (or `Rgba64Le` when `deep`) bytes.
    fn bytes(&self, deep: bool) -> Vec<u8> {
        if deep {
            self.px
                .iter()
                .flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes()))
                .collect()
        } else {
            self.px
                .iter()
                .flat_map(|p| p.map(|c| ((u32::from(c) * 255 + 32767) / 65535) as u8))
                .collect()
        }
    }

    /// A picture of these pixels carrying `template`'s timing, blobs,
    /// layer record and stream parameters.
    fn image(&self, template: &Image, deep: bool) -> Result<Image> {
        let format = if deep {
            PixelFormat::Rgba64Le
        } else {
            PixelFormat::Rgba
        };
        template.with_pixels(self.w as u32, self.h as u32, format, self.bytes(deep))
    }
}

fn any_changed(a: Px, b: Px) -> bool {
    !same(a, b)
}

fn cleared(a: Px, b: Px) -> bool {
    visible(a) && !visible(b)
}

fn overlaid(a: Px, b: Px) -> bool {
    !same(a, b) && visible(b)
}

fn is_deep(frames: &[Image]) -> bool {
    frames.iter().any(|f| crate::ladder::is_deep(f.format()))
}

fn need_frames(frames: &[Image], what: &str) -> Result<()> {
    if frames.is_empty() {
        return Err(Error::invalid(format!("{what}: no frame")));
    }
    Ok(())
}

/// The picture's rectangle on its canvas.
fn frame_rect(img: &Image) -> Rect {
    let (x, y) = img.offset();
    Rect {
        x: i64::from(x),
        y: i64::from(y),
        w: i64::from(img.width()),
        h: i64::from(img.height()),
    }
}

/// Window `r` (canvas coordinates) of `img` as its own picture, in the
/// picture's layout when [`Image::crop`] handles it, else as RGBA; the
/// page keeps the canvas and moves the offset to `r`, and the window is
/// an overlay ([`Blend::Over`]).
fn crop_frame(img: &Image, r: Rect, deep: bool) -> Result<Image> {
    let fr = frame_rect(img);
    let (lx, ly) = (r.x - fr.x, r.y - fr.y);
    let (cw, ch) = img.canvas_size();
    let page = Page::new(cw, ch).with_offset(r.x as i32, r.y as i32);
    let inside = lx >= 0 && ly >= 0 && lx + r.w <= fr.w && ly + r.h <= fr.h;
    if inside && let Ok(c) = img.crop(lx as u32, ly as u32, r.w as u32, r.h as u32) {
        return Ok(c.with_page(page).with_blend(Blend::Over));
    }
    let raster = Raster::of(img)?.crop(Rect {
        x: lx,
        y: ly,
        w: r.w,
        h: r.h,
    });
    Ok(raster
        .image(img, deep)?
        .with_page(page)
        .with_blend(Blend::Over))
}

/// ImageMagick's picture for "no change": one transparent pixel at
/// `-1-1`.
fn empty_frame(template: &Image, canvas: (u32, u32), deep: bool) -> Result<Image> {
    let r = Raster {
        w: 1,
        h: 1,
        px: vec![CLEAR],
    };
    Ok(r.image(template, deep)?
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

/// Play `frames` on a canvas the size of the first frame's canvas,
/// starting from the background: for each frame, `visit(index, canvas
/// as displayed, canvas after the frame's disposal)`.
fn play(
    frames: &[Image],
    opts: &LayerOptions,
    mut visit: impl FnMut(usize, &Raster, &Raster) -> Result<()>,
) -> Result<(u32, u32)> {
    let (cw, ch) = frames[0].canvas_size();
    let bg = opts.background;
    let mut canvas = Raster::filled(cw, ch, bg)?;
    for (i, f) in frames.iter().enumerate() {
        let saved = (f.dispose() == Dispose::Previous).then(|| canvas.clone());
        let src = Raster::of(f)?;
        let r = frame_rect(f);
        canvas.draw(&src, r.x, r.y, f.blend());
        let shown = canvas.clone();
        match f.dispose() {
            Dispose::Background => canvas.fill(Some(r), bg),
            // On the first frame the state before it is the initial
            // background (APNG: "treated as APNG_DISPOSE_OP_BACKGROUND").
            Dispose::Previous => canvas = saved.unwrap_or_else(|| canvas.clone()),
            _ => {}
        }
        visit(i, &shown, &canvas)?;
    }
    Ok((cw, ch))
}

/// Full-canvas `pixels` built on `template`'s attributes.
fn canvas_frame(template: &Image, pixels: &Raster, deep: bool) -> Result<Image> {
    Ok(pixels
        .image(template, deep)?
        .with_page(Page::new(pixels.w as u32, pixels.h as u32))
        .with_blend(Blend::Over))
}

// ---- coalesce / dispose -----------------------------------------------------

/// Render every frame onto the full canvas, honouring each frame's
/// offset, blending and disposal: the animation as it is displayed,
/// one full-canvas picture per frame (page `WxH+0+0`). Transparent-black
/// background; [`coalesce_with`] sets another.
///
/// The output frames are disposed so that drawing them in order with
/// [`Blend::Over`] shows the same animation: [`Dispose::Background`]
/// where the next frame clears a visible pixel, else [`Dispose::None`]
/// (the last frame repeats the previous frame's choice).
pub fn coalesce(frames: &[Image]) -> Result<Vec<Image>> {
    coalesce_with(frames, &LayerOptions::default())
}

/// [`coalesce`] on `opts.background`.
pub fn coalesce_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    need_frames(frames, "coalesce")?;
    let deep = is_deep(frames);
    let mut out: Vec<Image> = Vec::with_capacity(frames.len());
    let mut last: Option<Raster> = None;
    play(frames, opts, |i, shown, _| {
        if let Some(prev) = &last {
            let clears = prev.diff(shown, cleared).is_some();
            if let Some(p) = out.last_mut() {
                p.set_dispose(if clears {
                    Dispose::Background
                } else {
                    Dispose::None
                });
            }
        }
        out.push(canvas_frame(&frames[i], shown, deep)?.with_dispose(Dispose::None));
        last = Some(shown.clone());
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
    let deep = is_deep(frames);
    let mut out = Vec::with_capacity(frames.len());
    play(frames, opts, |i, _, disposed| {
        out.push(canvas_frame(&frames[i], disposed, deep)?.with_dispose(Dispose::None));
        Ok(())
    })?;
    Ok(out)
}

// ---- compare ----------------------------------------------------------------

#[derive(Clone, Copy)]
enum Compare {
    Any,
    Clear,
    Overlay,
}

/// Each frame after the first cropped to the pixels that differ from
/// the frame before it, both placed on the canvas at their offsets (no
/// disposal applied): ImageMagick's `CompareAny`, `deconstructImages`.
/// The first frame is kept as is; a frame identical to its predecessor
/// has nothing to keep and is dropped, as ImageMagick drops it.
pub fn compare_any(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Compare::Any)
}

/// `Imagick::deconstructImages`: [`compare_any`].
pub fn deconstruct(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Compare::Any)
}

/// As [`compare_any`], cropped to the pixels that are visible in the
/// earlier frame and transparent in the later one (what must be erased
/// to go from one to the next).
pub fn compare_clear(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Compare::Clear)
}

/// As [`compare_any`], cropped to the changed pixels that are visible
/// in the later frame (what must be painted over the earlier one).
pub fn compare_overlay(frames: &[Image]) -> Result<Vec<Image>> {
    compare(frames, Compare::Overlay)
}

fn compare(frames: &[Image], how: Compare) -> Result<Vec<Image>> {
    need_frames(frames, "compare")?;
    let deep = is_deep(frames);
    let mut out = vec![frames[0].clone()];
    for pair in frames.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let (cw, ch) = cur.canvas_size();
        let mut before = Raster::filled(cw, ch, CLEAR)?;
        let pr = frame_rect(prev);
        before.draw(&Raster::of(prev)?, pr.x, pr.y, Blend::Source);
        let src = Raster::of(cur)?;
        let r = frame_rect(cur);
        let mut bb = Bounds::default();
        if let Some(win) = r.intersect(before.w, before.h) {
            for y in win.y..win.y + win.h {
                for x in win.x..win.x + win.w {
                    let (a, b) = (before.at(x, y), src.at(x - r.x, y - r.y));
                    let hit = match how {
                        Compare::Any => any_changed(a, b),
                        Compare::Clear => cleared(a, b),
                        Compare::Overlay => overlaid(a, b),
                    };
                    if hit {
                        bb.add(x, y);
                    }
                }
            }
        }
        if let Some(b) = bb.rect() {
            out.push(crop_frame(cur, b, deep)?);
        }
    }
    Ok(out)
}

// ---- optimize ---------------------------------------------------------------

/// One output frame of the optimisers, before it is cut.
struct Planned {
    /// Input frame the pixels come from.
    src: usize,
    /// Canvas window (`None` = ImageMagick's empty 1×1 at -1-1).
    rect: Option<Rect>,
    /// Decided disposal (`None` until the next frame decides it).
    dispose: Option<Dispose>,
    delay: Option<Duration>,
}

/// The disposal choice for the transition from coalesced frame `prev`
/// to `cur`: what to do with the last planned frame and the window the
/// next one needs.
struct Choice {
    kind: ChoiceKind,
    rect: Option<Rect>,
    /// The canvas the next frame is drawn onto.
    state: Raster,
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

fn choose(
    prev: &Raster,
    cur: &Raster,
    before_last: &Raster,
    last_rect: Option<Rect>,
    plus: bool,
) -> Choice {
    let clear = prev.diff(cur, cleared);
    let mut best: Option<(u64, Choice)> = None;
    let mut offer = |cost: u64, c: Choice| {
        if best.as_ref().is_none_or(|(b, _)| cost < *b) {
            best = Some((cost, c));
        }
    };
    if clear.is_none() {
        let rect = prev.diff(cur, any_changed);
        offer(
            Rect::area(rect),
            Choice {
                kind: ChoiceKind::None,
                rect,
                state: prev.clone(),
            },
        );
    }
    if before_last.diff(cur, cleared).is_none() {
        let rect = before_last.diff(cur, any_changed);
        offer(
            Rect::area(rect),
            Choice {
                kind: ChoiceKind::Previous,
                rect,
                state: before_last.clone(),
            },
        );
    }
    if plus && let Some(cb) = clear {
        let mut state = prev.clone();
        state.fill(Some(cb), CLEAR);
        let rect = state.diff(cur, any_changed);
        offer(
            Rect::area(Some(cb)) + Rect::area(rect),
            Choice {
                kind: ChoiceKind::Split(cb),
                rect,
                state,
            },
        );
    }
    let grown = Rect::union(last_rect, clear);
    let mut state = prev.clone();
    state.fill(grown, CLEAR);
    let rect = state.diff(cur, any_changed);
    let growth = Rect::area(grown).saturating_sub(Rect::area(last_rect));
    offer(
        Rect::area(rect) + growth,
        Choice {
            kind: ChoiceKind::Background(grown),
            rect,
            state,
        },
    );
    best.map(|(_, c)| c).expect("Background is always offered")
}

fn check_coalesced(frames: &[Image], what: &str) -> Result<()> {
    need_frames(frames, what)?;
    let (w, h) = (frames[0].width(), frames[0].height());
    for (i, f) in frames.iter().enumerate() {
        if (f.width(), f.height()) != (w, h) {
            return Err(Error::invalid(format!(
                "{what}: frame {i} is {}x{}, frame 0 is {w}x{h} (coalesce first)",
                f.width(),
                f.height()
            )));
        }
        if !f.is_full_canvas() {
            return Err(Error::invalid(format!(
                "{what}: frame {i} has page {}, not a coalesced full-canvas frame (coalesce first)",
                f.page()
            )));
        }
    }
    Ok(())
}

/// Turn a coalesced animation (every frame full canvas, same size —
/// [`coalesce`] output) into the smallest frames it can, choosing each
/// frame's disposal: for every transition the candidates are
/// [`Dispose::None`] (when nothing visible has to be erased),
/// [`Dispose::Previous`] and [`Dispose::Background`] (the previous
/// window grown over what has to be erased), and the one giving the
/// smallest next window wins (ties in that order). A frame identical to
/// its predecessor becomes ImageMagick's 1×1 transparent frame at
/// `-1-1`. Output geometry and disposal match `magick -layers
/// optimize-frame` (README, *Layer operations*).
pub fn optimize_frame(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_frames(frames, false, "optimize-frame")
}

/// [`optimize_frame`] that may also split a transition in two — the
/// frame kept with no delay, then a clearing frame over just the pixels
/// to erase (with the delay and [`Dispose::Background`]) — when that
/// costs no more than the best single disposal, and that folds a frame
/// identical to its predecessor into it (delays added). ImageMagick's
/// `OptimizePlus`; the delays here always add up to the input's
/// (README, *Layer operations*).
pub fn optimize_plus(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_frames(frames, true, "optimize-plus")
}

fn optimize_frames(frames: &[Image], plus: bool, what: &str) -> Result<Vec<Image>> {
    check_coalesced(frames, what)?;
    let deep = is_deep(frames);
    let (w, h) = (frames[0].width(), frames[0].height());
    let mut prev = Raster::of(&frames[0])?;
    let mut before_last = Raster::filled(w, h, CLEAR)?;
    let empty = before_last.clone();
    let mut plan = vec![Planned {
        src: 0,
        rect: empty.diff(&prev, |_, b| b[3] != 0),
        dispose: None,
        delay: frames[0].delay(),
    }];
    for (i, f) in frames.iter().enumerate().skip(1) {
        let cur = Raster::of(f)?;
        let last = plan.len() - 1;
        if plus && i >= 2 && prev.diff(&cur, any_changed).is_none() {
            plan[last].delay = add_delay(plan[last].delay, f.delay());
            plan[last].dispose = Some(Dispose::None);
            continue;
        }
        let c = choose(&prev, &cur, &before_last, plan[last].rect, plus);
        match c.kind {
            ChoiceKind::None => plan[last].dispose = Some(Dispose::None),
            ChoiceKind::Previous => plan[last].dispose = Some(Dispose::Previous),
            ChoiceKind::Background(grown) => {
                plan[last].dispose = Some(Dispose::Background);
                plan[last].rect = grown;
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
            }
        }
        plan.push(Planned {
            src: i,
            rect: c.rect,
            dispose: None,
            delay: f.delay(),
        });
        before_last = c.state;
        prev = cur;
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
                Some(r) => crop_frame(f, r, deep)?,
                None => empty_frame(f, (w, h), deep)?,
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
/// `Rgba` (`Rgba64Le` for deep input).
pub fn optimize_transparency(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_transparency_with(frames, &LayerOptions::default())
}

/// [`optimize_transparency`] on `opts.background`.
pub fn optimize_transparency_with(frames: &[Image], opts: &LayerOptions) -> Result<Vec<Image>> {
    need_frames(frames, "optimize-transparency")?;
    let deep = is_deep(frames);
    let (cw, ch) = frames[0].canvas_size();
    let bg = opts.background;
    let mut canvas = Raster::filled(cw, ch, bg)?;
    let mut out = Vec::with_capacity(frames.len());
    for f in frames {
        let saved = (f.dispose() == Dispose::Previous).then(|| canvas.clone());
        let src = Raster::of(f)?;
        let r = frame_rect(f);
        let mut kept = src.clone();
        // Only an overlay can leave a pixel alone: a `Source` frame's
        // transparent pixel would erase the canvas.
        if f.blend() == Blend::Over
            && let Some(win) = r.intersect(canvas.w, canvas.h)
        {
            for y in win.y..win.y + win.h {
                for x in win.x..win.x + win.w {
                    let (s, d) = (src.at(x - r.x, y - r.y), canvas.at(x, y));
                    if s[3] != 0 && same(over(s, d), d) {
                        kept.put(x - r.x, y - r.y, CLEAR);
                    }
                }
            }
        }
        canvas.draw(&src, r.x, r.y, f.blend());
        match f.dispose() {
            Dispose::Background => canvas.fill(Some(r), bg),
            Dispose::Previous => canvas = saved.unwrap_or_else(|| canvas.clone()),
            _ => {}
        }
        out.push(kept.image(f, deep)?);
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
    let framed = optimize_frame(&coalesced)?;
    optimize_transparency_with(&framed, opts)
}

// ---- list clean-up ----------------------------------------------------------

/// Fold every run of consecutive identical frames (same size, offset
/// and pixels) into its last frame, the delays added: ImageMagick's
/// `RemoveDups`.
pub fn remove_dups(frames: &[Image]) -> Result<Vec<Image>> {
    let mut out: Vec<Image> = Vec::with_capacity(frames.len());
    let mut last: Option<Raster> = None;
    for f in frames {
        let r = Raster::of(f)?;
        if let (Some(prev), Some(lr)) = (out.last(), &last)
            && prev.width() == f.width()
            && prev.height() == f.height()
            && prev.offset() == f.offset()
            && lr.px.iter().zip(&r.px).all(|(a, b)| same(*a, *b))
        {
            let delay = add_delay(prev.delay(), f.delay());
            out.pop();
            out.push(f.clone().with_delay(delay));
        } else {
            out.push(f.clone());
        }
        last = Some(r);
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
    let deep = is_deep(frames);
    let mut canvas = Raster::filled(w, h, opts.background)?;
    for f in frames {
        let r = frame_rect(f);
        canvas.draw(&Raster::of(f)?, r.x - ox, r.y - oy, f.blend());
    }
    Ok(canvas
        .image(&frames[0], deep)?
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
    let deep = is_deep(destination) || is_deep(source);
    let pairs: Vec<(&Image, &Image)> = if source.len() == 1 {
        destination.iter().map(|d| (d, &source[0])).collect()
    } else if destination.len() == 1 {
        source.iter().map(|s| (&destination[0], s)).collect()
    } else {
        destination.iter().zip(source).collect()
    };
    let mut out: Vec<Image> = Vec::new();
    for (d, s) in &pairs {
        let mut canvas = Raster::of(d)?;
        let (dr, sr) = (frame_rect(d), frame_rect(s));
        canvas.draw(
            &Raster::of(s)?,
            sr.x - dr.x + i64::from(dx),
            sr.y - dr.y + i64::from(dy),
            s.blend(),
        );
        out.push(canvas.image(d, deep)?);
    }
    if source.len() > 1 && destination.len() > source.len() {
        out.extend(destination[source.len()..].iter().cloned());
    }
    Ok(out)
}
