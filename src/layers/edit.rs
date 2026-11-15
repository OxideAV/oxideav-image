//! List edits that do not composite an animation: joining pictures
//! side by side, page geometry, trimming a layer to its content, delays
//! and loop counts, selecting and reordering frames.

use std::time::Duration;

use super::raster::{Base, Depth, Pred, Px, Raster, Rect, scan};
use super::{LayerOptions, frame_rect, need_frames, raster_image};
use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{Blend, Dispose, Page};

/// The axis [`append`] lays pictures along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Direction {
    /// Left to right.
    Horizontal,
    /// Top to bottom.
    Vertical,
}

/// Join the pictures of `frames` into one, side by side from the left
/// ([`Direction::Horizontal`]: the widths add up, the height is the
/// tallest) or stacked from the top ([`Direction::Vertical`]). Each
/// picture is copied as it is (page offsets, blending and disposal play
/// no part), aligned to the top / left edge; what no picture covers is
/// transparent black. The result is its own canvas and carries the
/// first picture's attributes.
pub fn append(frames: &[Image], direction: Direction) -> Result<Image> {
    append_with(frames, direction, &LayerOptions::default())
}

/// [`append`] with the uncovered part filled with `opts.background`.
pub fn append_with(frames: &[Image], direction: Direction, opts: &LayerOptions) -> Result<Image> {
    need_frames(frames, "append")?;
    let depth = Depth::of(frames);
    let (mut along, mut across) = (0u64, 0u64);
    for f in frames {
        let (w, h) = (u64::from(f.width()), u64::from(f.height()));
        let (a, c) = match direction {
            Direction::Horizontal => (w, h),
            Direction::Vertical => (h, w),
        };
        along += a;
        across = across.max(c);
    }
    let (w, h) = match direction {
        Direction::Horizontal => (along, across),
        Direction::Vertical => (across, along),
    };
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(Error::limit(format!(
            "append: {w}x{h} is over the layer limit"
        )));
    };
    let mut canvas = Raster::filled(w, h, depth, opts.px(depth))?;
    let mut at = 0i64;
    for f in frames {
        let (x, y, step) = match direction {
            Direction::Horizontal => (at, 0, f.width()),
            Direction::Vertical => (0, at, f.height()),
        };
        canvas.draw(&Raster::of(f, depth)?, x, y, Blend::Source);
        at += i64::from(step);
    }
    Ok(raster_image(&frames[0], canvas)?
        .with_page(Page::default())
        .with_dispose(Dispose::Unspecified)
        .with_blend(Blend::Over))
}

/// Forget every frame's page: each picture becomes its own canvas at
/// `+0+0` (page `0x0+0+0`). Pixels, blending, disposal and timing are
/// untouched.
pub fn reset_page(frames: &[Image]) -> Vec<Image> {
    frames
        .iter()
        .map(|f| f.clone().with_page(Page::default()))
        .collect()
}

/// Give every frame the page `page`: its canvas size (a side left at
/// `0` keeps the frame's own) and its offset.
pub fn repage(frames: &[Image], page: Page) -> Vec<Image> {
    frames
        .iter()
        .map(|f| {
            let own = f.page();
            let (w, h) = (
                if page.width == 0 {
                    own.width
                } else {
                    page.width
                },
                if page.height == 0 {
                    own.height
                } else {
                    page.height
                },
            );
            f.clone()
                .with_page(Page::new(w, h).with_offset(page.x, page.y))
        })
        .collect()
}

/// Put every frame on a `width × height` canvas, offsets kept.
pub fn set_canvas(frames: &[Image], width: u32, height: u32) -> Vec<Image> {
    frames
        .iter()
        .map(|f| {
            let (x, y) = f.offset();
            f.clone()
                .with_page(Page::new(width, height).with_offset(x, y))
        })
        .collect()
}

/// Move every frame by `(dx, dy)` on its canvas (the canvas, declared
/// at its present size, stays where it is). An offset that would leave
/// the `i32` range is `InvalidData`.
pub fn translate(frames: &[Image], dx: i32, dy: i32) -> Result<Vec<Image>> {
    frames
        .iter()
        .map(|f| {
            let (x, y) = f.offset();
            let (Some(x), Some(y)) = (x.checked_add(dx), y.checked_add(dy)) else {
                return Err(Error::invalid("translate: the offset overflows"));
            };
            let (cw, ch) = f.canvas_size();
            Ok(f.clone().with_page(Page::new(cw, ch).with_offset(x, y)))
        })
        .collect()
}

/// Cut `r` (canvas coordinates, inside the picture) out of `img`,
/// leaving it where it was on its canvas.
fn cut(img: &Image, r: Rect, depth: Depth) -> Result<Image> {
    let fr = frame_rect(img);
    let (cw, ch) = img.canvas_size();
    let page = Page::new(cw, ch).with_offset(r.x as i32, r.y as i32);
    let (lx, ly) = (r.x - fr.x, r.y - fr.y);
    if let Ok(c) = img.crop(lx as u32, ly as u32, r.w as u32, r.h as u32) {
        return Ok(c.with_page(page));
    }
    let window = Raster::of(img, depth)?.crop(Rect::new(lx, ly, r.w, r.h));
    Ok(raster_image(img, window)?.with_page(page))
}

fn trim_against(img: &Image, depth: Depth, border: Px) -> Result<Image> {
    let pixels = Raster::of(img, depth)?;
    let base = Base {
        src: None,
        uniform: border,
        fill: None,
    };
    let fr = frame_rect(img);
    let content = scan(&base, &pixels, None, Pred::Changed, 1).unwrap_or(Rect::new(0, 0, 1, 1));
    if (content.w, content.h) == (fr.w, fr.h) {
        return Ok(img.clone());
    }
    cut(
        img,
        Rect::new(fr.x + content.x, fr.y + content.y, content.w, content.h),
        depth,
    )
}

/// Trim a layer to its content: the smallest rectangle holding every
/// pixel that is not fully transparent. The picture stays where it was
/// on its canvas (the page keeps the canvas size and the offset moves
/// to the rectangle), in its own layout where [`Image::crop`] can cut
/// it; a picture with no content at all becomes its top-left pixel.
pub fn trim(image: &Image) -> Result<Image> {
    let depth = Depth::of(std::slice::from_ref(image));
    trim_against(image, depth, Px::clear(depth))
}

/// As [`trim`], the content being every pixel that differs from
/// `border` (straight 16-bit RGBA; `[0, 0, 0, 0]` trims transparency).
pub fn trim_to(image: &Image, border: [u16; 4]) -> Result<Image> {
    let depth = Depth::of(std::slice::from_ref(image));
    trim_against(image, depth, Px::from_rgba16(depth, border))
}

/// Give every frame the delay `delay` (`None` removes it).
pub fn set_delay(frames: &[Image], delay: Option<Duration>) -> Vec<Image> {
    frames.iter().map(|f| f.clone().with_delay(delay)).collect()
}

/// Give every frame a delay of `ticks` at `ticks_per_second`
/// ([`Image::with_delay_ticks`]).
pub fn set_delay_ticks(frames: &[Image], ticks: u64, ticks_per_second: u32) -> Vec<Image> {
    frames
        .iter()
        .map(|f| f.clone().with_delay_ticks(ticks, ticks_per_second))
        .collect()
}

/// Multiply every delay by `numerator / denominator` (rounded to the
/// nearest nanosecond): `1 / 2` plays twice as fast. A zero
/// denominator is `InvalidData`; a frame without a delay keeps none.
pub fn scale_delays(frames: &[Image], numerator: u32, denominator: u32) -> Result<Vec<Image>> {
    if denominator == 0 {
        return Err(Error::invalid("scale_delays: zero denominator"));
    }
    let (n, d) = (u128::from(numerator), u128::from(denominator));
    Ok(frames
        .iter()
        .map(|f| {
            let scaled = f.delay().map(|t| {
                let nanos = (t.as_nanos() * n + d / 2) / d;
                Duration::new(
                    (nanos / 1_000_000_000).min(u128::from(u64::MAX)) as u64,
                    (nanos % 1_000_000_000) as u32,
                )
            });
            f.clone().with_delay(scaled)
        })
        .collect())
}

/// The sum of the frames' delays.
pub fn total_delay(frames: &[Image]) -> Duration {
    frames.iter().filter_map(Image::delay).sum()
}

/// Set the loop count every frame carries (`Some(0)` = forever, `None`
/// = unstated); a multi-picture encode writes the first frame's.
pub fn set_loop_count(frames: &[Image], plays: Option<u32>) -> Vec<Image> {
    frames
        .iter()
        .map(|f| f.clone().with_loop_count(plays))
        .collect()
}

/// The frames at `indices`, in that order (an index may repeat). An
/// index past the end is `InvalidData`.
///
/// A layered list means what it means in its order — a frame is drawn
/// onto what the frames before it left — so select from, and reorder,
/// a coalesced list ([`super::coalesce`]) and optimise afterwards.
pub fn select(frames: &[Image], indices: &[usize]) -> Result<Vec<Image>> {
    indices
        .iter()
        .map(|&i| {
            frames.get(i).cloned().ok_or_else(|| {
                Error::invalid(format!(
                    "select: frame {i} of a {}-frame list",
                    frames.len()
                ))
            })
        })
        .collect()
}

/// The frames last to first (see [`select`] on layered lists).
pub fn reverse(frames: &[Image]) -> Vec<Image> {
    frames.iter().rev().cloned().collect()
}
