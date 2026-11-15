//! The exact frame optimiser: the smallest rectangle per frame, with
//! the disposal of the frame before it and its own blending chosen so
//! that the optimised list plays back to the very pixels of the
//! original, at every alpha value and every depth.

use super::raster::{self, Base, Depth, Patch, Pred, Px, Raster, Rect, scan};
use super::{BackgroundRule, LayerOptions, Stage, disposed_rect, frame_rect, need_frames, play};
use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{Blend, Dispose, Page};

/// What [`optimize_layers_with`] may use and do.
///
/// `#[non_exhaustive]`: start from [`new`](Self::new) / [`Default`]
/// (every disposal and both blends allowed, no transparency
/// optimisation, serial) and the `with_*` builders. Restrict it to what
/// the target format can store: a format without a "previous" disposal
/// takes `with_dispose_previous(false)`, one whose frames can only be
/// drawn over the canvas takes `with_blend_source(false)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OptimizeOptions {
    /// Background, background rule and thread budget.
    pub layer: LayerOptions,
    /// Make every pixel an overlay frame leaves unchanged fully
    /// transparent (long runs of one value for the encoder). Default
    /// `false`: such a pixel keeps its colour when drawing it changes
    /// nothing.
    pub transparency: bool,
    /// Allow [`Dispose::Background`]. Default `true`.
    pub dispose_background: bool,
    /// Allow [`Dispose::Previous`]. Default `true`.
    pub dispose_previous: bool,
    /// Allow [`Blend::Source`]. Default `true`.
    pub blend_source: bool,
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        Self {
            layer: LayerOptions::default(),
            transparency: false,
            dispose_background: true,
            dispose_previous: true,
            blend_source: true,
        }
    }
}

impl OptimizeOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Background, background rule and thread budget.
    pub fn with_layer_options(mut self, layer: LayerOptions) -> Self {
        self.layer = layer;
        self
    }

    /// Thread budget (see [`LayerOptions::with_threads`]).
    pub fn with_threads(mut self, n: usize) -> Self {
        self.layer = self.layer.with_threads(n);
        self
    }

    /// The transparency optimisation on or off.
    pub fn with_transparency(mut self, on: bool) -> Self {
        self.transparency = on;
        self
    }

    /// Allow or forbid [`Dispose::Background`].
    pub fn with_dispose_background(mut self, allow: bool) -> Self {
        self.dispose_background = allow;
        self
    }

    /// Allow or forbid [`Dispose::Previous`].
    pub fn with_dispose_previous(mut self, allow: bool) -> Self {
        self.dispose_previous = allow;
        self
    }

    /// Allow or forbid [`Blend::Source`].
    pub fn with_blend_source(mut self, allow: bool) -> Self {
        self.blend_source = allow;
        self
    }
}

/// [`optimize_layers_with`] with the default options.
pub fn optimize_layers(frames: &[Image]) -> Result<Vec<Image>> {
    optimize_layers_with(frames, &OptimizeOptions::default())
}

/// Rewrite an animation — any list, layered or coalesced — as the
/// smallest frames that play back to exactly the same pictures:
/// `coalesce_with(optimize_layers_with(frames, o), o.layer)` shows, frame
/// for frame, the pixels of `coalesce_with(frames, o.layer)` (equal in
/// the module's sense: identical samples, or both fully transparent).
/// One output frame per input frame, timing kept.
///
/// With `T` the picture a frame must show and `B` the canvas it is
/// drawn onto, the frame's rectangle is the bounding box of the pixels
/// where `T ≠ B` (one pixel at `+0+0` when there is none). `B` is one
/// of three canvases, depending on the disposal given to the frame
/// before it: the picture that frame showed ([`Dispose::None`]), that
/// picture with the frame's rectangle set to the background
/// ([`Dispose::Background`]), or the canvas that frame was itself drawn
/// onto ([`Dispose::Previous`]). The disposal giving the smallest
/// rectangle wins; ties go to `None`, then `Background`, then
/// `Previous`.
///
/// The frame is an overlay ([`Blend::Over`]) when every pixel of its
/// rectangle is either unchanged or shows itself when drawn over `B`
/// (`T OVER B = T`: `T` opaque, or `B` fully transparent); unchanged
/// pixels then become fully transparent where keeping them would alter
/// the canvas (a semi-transparent pixel drawn over itself) — and
/// everywhere under [`OptimizeOptions::transparency`]. Otherwise the
/// frame replaces its rectangle ([`Blend::Source`]) with the pixels of
/// `T`.
///
/// Without [`Blend::Source`] (`with_blend_source(false)`) a pixel that
/// must lose opacity can only be erased by a background disposal: the
/// rectangle of the frame before is grown over such pixels (the growth
/// counts in the comparison of the candidates). An animation that
/// cannot be written that way — pixels to erase on an opaque
/// background, or with [`Dispose::Background`] forbidden too — is
/// `Unsupported`.
///
/// Only the rectangles a frame draws or disposes are compared when the
/// input is a layered list; a coalesced input costs one comparison of
/// the canvas per candidate, in row bands on the thread budget of
/// [`LayerOptions::with_threads`].
pub fn optimize_layers_with(frames: &[Image], opts: &OptimizeOptions) -> Result<Vec<Image>> {
    need_frames(frames, "optimize-layers")?;
    let depth = Depth::of(frames);
    // The colour a disposed rectangle holds and an output canvas starts
    // as: every output frame carries alpha, so under the first-frame
    // rule both are the transparent background.
    let background = match opts.layer.background_rule {
        BackgroundRule::FirstFrameAlpha => opts.layer.px(depth).with_alpha(depth, false),
        _ => opts.layer.px(depth),
    };
    let mut out: Vec<Image> = Vec::with_capacity(frames.len());
    // The output canvas as displayed after the pending frame.
    let mut shown: Option<Raster<'static>> = None;
    let mut pending: Option<Pending> = None;
    play(
        frames,
        &opts.layer,
        depth,
        false,
        |i, stage, canvas, fresh| {
            if stage != Stage::Shown {
                return Ok(());
            }
            let whole = Rect::new(0, 0, canvas.w as i64, canvas.h as i64);
            let first = i == 0 || fresh;
            if first {
                if let (Some(p), Some(t)) = (pending.take(), shown.as_ref()) {
                    out.push(p.build(&frames[p.index], t, None, Dispose::None, opts)?);
                }
                shown = Some(Raster::filled(
                    canvas.w as u32,
                    canvas.h as u32,
                    depth,
                    background,
                )?);
            }
            let before = shown.as_mut().expect("set on the first frame");
            // Where the picture differs from the one before it: inside
            // what the frame before disposed and this one drew — a
            // rectangle the pixels then narrow, so that a full-canvas
            // frame costs one comparison of the canvas and every
            // candidate below a comparison of the changed box.
            let dirty = if first {
                Some(whole)
            } else {
                Rect::union(disposed_rect(&frames[i - 1]), Some(frame_rect(&frames[i]))).and_then(
                    |r| {
                        let threads = opts.layer.threads();
                        scan(&Base::of(before), canvas, Some(r), Pred::Changed, threads)
                    },
                )
            };
            let plan = plan(before, canvas, pending.as_ref(), dirty, background, opts)
                .map_err(|e| Error::unsupported(format!("optimize-layers: frame {i} {e}")))?;
            if let Some(p) = pending.take() {
                out.push(p.build(&frames[p.index], before, plan.grown, plan.dispose, opts)?);
            }
            pending = Some(Pending {
                index: i,
                rect: plan.rect,
                blend: plan.blend,
                under: plan.under,
            });
            if let Some(d) = dirty {
                before.copy_from(canvas, d);
            }
            Ok(())
        },
    )?;
    if let (Some(p), Some(t)) = (pending.take(), shown.as_ref()) {
        out.push(p.build(&frames[p.index], t, None, Dispose::None, opts)?);
    }
    Ok(out)
}

/// A planned frame whose disposal the next frame decides.
struct Pending {
    /// Input frame it stands for.
    index: usize,
    rect: Rect,
    blend: Blend,
    /// The canvas the frame is drawn onto, under `rect`.
    under: Raster<'static>,
}

impl Pending {
    /// The frame's picture: `shown` is the canvas it must display,
    /// `grown` its rectangle when a background disposal enlarged it.
    fn build(
        &self,
        template: &Image,
        shown: &Raster,
        grown: Option<Rect>,
        dispose: Dispose,
        opts: &OptimizeOptions,
    ) -> Result<Image> {
        let depth = shown.depth;
        let bpp = depth.bpp();
        let rect = grown.unwrap_or(self.rect);
        let mut pixels = shown.crop(rect).into_bytes();
        if self.blend == Blend::Over {
            let width = rect.w as usize * bpp;
            // Outside the planned rectangle (a rectangle grown by a
            // background disposal) the canvas under the frame is the
            // picture itself.
            let x0 = (self.rect.x - rect.x) as usize * bpp;
            let x1 = x0 + self.rect.w as usize * bpp;
            let all = opts.transparency;
            for (line, y) in pixels.chunks_exact_mut(width).zip(rect.y..) {
                let row = y - self.rect.y;
                if row < 0 || row >= self.rect.h {
                    raster::settle_overlay(depth, line, None, all);
                    continue;
                }
                let under = self.under.span(row as usize, 0..self.rect.w as usize);
                raster::settle_overlay(depth, &mut line[..x0], None, all);
                raster::settle_overlay(depth, &mut line[x0..x1], Some(under), all);
                raster::settle_overlay(depth, &mut line[x1..], None, all);
            }
        }
        let raster = Raster::from_bytes(rect.w as usize, rect.h as usize, depth, pixels);
        Ok(super::raster_image(template, raster)?
            .with_page(
                Page::new(shown.w as u32, shown.h as u32).with_offset(rect.x as i32, rect.y as i32),
            )
            .with_blend(self.blend)
            .with_dispose(dispose))
    }
}

/// The plan for one frame.
struct Plan {
    /// Disposal of the frame before.
    dispose: Dispose,
    /// That frame's rectangle, when the disposal enlarged it.
    grown: Option<Rect>,
    rect: Rect,
    blend: Blend,
    under: Raster<'static>,
}

/// Choose the disposal of `pending` and the rectangle and blending of
/// the frame that must show `target`, `before` being the picture
/// `pending` shows and `dirty` where `target` can differ from it.
fn plan(
    before: &Raster,
    target: &Raster,
    pending: Option<&Pending>,
    dirty: Option<Rect>,
    background: Px,
    opts: &OptimizeOptions,
) -> std::result::Result<Plan, String> {
    let depth = target.depth;
    let threads = opts.layer.threads();
    let plain = Base::of(before);
    let mut candidates: Vec<(Dispose, Base)> = vec![(Dispose::None, plain)];
    if let Some(p) = pending {
        if opts.dispose_background {
            candidates.push((
                Dispose::Background,
                Base {
                    fill: Some((p.rect, Patch::Colour(background))),
                    ..plain
                },
            ));
        }
        if opts.dispose_previous {
            candidates.push((
                Dispose::Previous,
                Base {
                    fill: Some((p.rect, Patch::Pixels(&p.under))),
                    ..plain
                },
            ));
        }
    }
    // Cost, disposal, grown rectangle, rectangle, blending and base
    // of the best candidate so far.
    let mut best: Option<(u64, Dispose, Option<Rect>, Rect, Blend, Base)> = None;
    for (dispose, mut base) in candidates {
        // The base differs from the target inside the dirty window and
        // wherever the base differs from the picture before.
        let mut window = match dispose {
            Dispose::None => dirty,
            _ => Rect::union(dirty, pending.map(|p| p.rect)),
        };
        let look = |base: &Base, window: Option<Rect>, pred| match window {
            Some(w) => scan(base, target, Some(w), pred, threads),
            None => None,
        };
        let mut changed = look(&base, window, Pred::Changed);
        let mut failing = look(&base, window, Pred::OverFails);
        let mut grown = None;
        let mut growth = 0;
        if failing.is_some()
            && !opts.blend_source
            && dispose == Dispose::Background
            && let Some(p) = pending
        {
            // Erase what an overlay cannot: the disposed rectangle
            // grows over those pixels.
            let wider = Rect::union(Some(p.rect), failing);
            growth = Rect::area(wider) - Rect::area(Some(p.rect));
            grown = wider;
            base.fill = wider.map(|r| (r, Patch::Colour(background)));
            window = Rect::union(window, wider);
            changed = look(&base, window, Pred::Changed);
            failing = look(&base, window, Pred::OverFails);
        }
        if failing.is_some() && !opts.blend_source {
            continue;
        }
        let rect = changed.unwrap_or(Rect::new(0, 0, 1, 1));
        let cost = Rect::area(Some(rect)) + growth;
        if best.as_ref().is_none_or(|(b, ..)| cost < *b) {
            let blend = if failing.is_none() {
                Blend::Over
            } else {
                Blend::Source
            };
            best = Some((cost, dispose, grown, rect, blend, base));
        }
    }
    best.map(|(_, dispose, grown, rect, blend, base)| Plan {
        dispose,
        grown,
        rect,
        blend,
        under: base.crop(rect, depth),
    })
    .ok_or_else(|| {
        "has pixels that lose opacity, which no allowed disposal or blending can produce \
         (allow Blend::Source, or Dispose::Background on a transparent background)"
            .to_string()
    })
}
