//! The pixel engine of the layer operations: straight
//! (non-premultiplied) RGBA rasters held as the bytes of the packed
//! layout the frames come in — `Rgba` (8 bits a sample), `Rgba64Le`
//! (16) or `RgbaF32Le` (float) — so an `Rgba` frame is read where it
//! lies and a canvas is handed out without a conversion.
//!
//! Everything works on rectangles: a draw touches the drawn rectangle,
//! a disposal the disposed one, and a comparison skips equal row
//! segments with a slice comparison before it looks at a pixel.

use std::borrow::Cow;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::Blend;
use oxideav_core::PixelFormat;

/// Sample depth of an operation: the deepest among its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Depth {
    U8,
    U16,
    F32,
}

impl Depth {
    /// Float when any frame is float, else 16-bit when any is deeper
    /// than eight bits, else 8-bit.
    pub(crate) fn of(frames: &[Image]) -> Depth {
        let mut depth = Depth::U8;
        for f in frames {
            let format = f.format();
            if format.is_float() {
                return Depth::F32;
            }
            if crate::ladder::is_deep(format) {
                depth = Depth::U16;
            }
        }
        depth
    }

    /// Bytes per pixel.
    pub(crate) fn bpp(self) -> usize {
        match self {
            Depth::U8 => 4,
            Depth::U16 => 8,
            Depth::F32 => 16,
        }
    }

    /// The packed RGBA layout of this depth.
    pub(crate) fn format(self) -> PixelFormat {
        match self {
            Depth::U8 => PixelFormat::Rgba,
            Depth::U16 => PixelFormat::Rgba64Le,
            Depth::F32 => PixelFormat::RgbaF32Le,
        }
    }
}

/// One pixel in the bytes of a [`Depth`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Px {
    bytes: [u8; 16],
    len: usize,
}

impl Px {
    /// Fully transparent, colour zero.
    pub(crate) fn clear(depth: Depth) -> Px {
        Px {
            bytes: [0; 16],
            len: depth.bpp(),
        }
    }

    /// 16-bit straight RGBA at `depth`: as is at 16 bits, rounded to
    /// the nearest 8-bit value, or `c / 65535` in float.
    pub(crate) fn from_rgba16(depth: Depth, c: [u16; 4]) -> Px {
        let mut px = Px::clear(depth);
        match depth {
            Depth::U8 => {
                for (o, v) in px.bytes.iter_mut().zip(c) {
                    *o = narrow(v);
                }
            }
            Depth::U16 => {
                for (o, v) in px.bytes.as_chunks_mut::<2>().0.iter_mut().zip(c) {
                    *o = v.to_le_bytes();
                }
            }
            Depth::F32 => {
                return Px::from_f32(c.map(|v| f32::from(v) / 65535.0));
            }
        }
        px
    }

    /// A float pixel.
    pub(crate) fn from_f32(c: [f32; 4]) -> Px {
        let mut px = Px::clear(Depth::F32);
        for (o, v) in px.bytes.as_chunks_mut::<4>().0.iter_mut().zip(c) {
            *o = v.to_le_bytes();
        }
        px
    }

    /// The same colour, fully transparent / fully opaque.
    pub(crate) fn with_alpha(mut self, depth: Depth, opaque: bool) -> Px {
        match depth {
            Depth::U8 => self.bytes[3] = if opaque { 255 } else { 0 },
            Depth::U16 => {
                let a: u16 = if opaque { u16::MAX } else { 0 };
                self.bytes[6..8].copy_from_slice(&a.to_le_bytes());
            }
            Depth::F32 => {
                let a: f32 = if opaque { 1.0 } else { 0.0 };
                self.bytes[12..16].copy_from_slice(&a.to_le_bytes());
            }
        }
        self
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// A 16-bit sample rounded to the nearest 8-bit one.
fn narrow(v: u16) -> u8 {
    ((u32::from(v) * 255 + 32767) / 65535) as u8
}

/// `s` OVER `d` on straight 16-bit RGBA, for `0 < s.alpha < 65535` and
/// `d.alpha > 0`, in exact integer arithmetic. With `M = 65535` and
/// `A = s.a·M + d.a·(M − s.a)`: alpha is `A / M` and each colour
/// `(s.c·s.a·M + d.c·d.a·(M − s.a)) / A`, both rounded to nearest (a
/// colour tie rounds up; alpha has no tie, `M` being odd).
fn over16(s: [u16; 4], d: [u16; 4]) -> [u16; 4] {
    const M: u64 = 65535;
    let (sa, da) = (u64::from(s[3]), u64::from(d[3]));
    let rest = da * (M - sa);
    let a = sa * M + rest;
    let mut out = [0u16; 4];
    for ((o, sc), dc) in out.iter_mut().zip(s).zip(d).take(3) {
        let num = u64::from(sc) * sa * M + u64::from(dc) * rest;
        *o = ((2 * num + a) / (2 * a)) as u16;
    }
    out[3] = ((a + M / 2) / M) as u16;
    out
}

/// The per-depth pixel rules, `N` bytes a pixel.
pub(crate) trait Samples<const N: usize> {
    /// Alpha is zero (float: not above zero).
    fn transparent(p: &[u8; N]) -> bool;
    /// Alpha is at least one half.
    fn visible(p: &[u8; N]) -> bool;
    /// `d ← s OVER d`.
    fn over(s: &[u8; N], d: &mut [u8; N]);
}

pub(crate) struct P8;
pub(crate) struct P16;
pub(crate) struct PF;

impl Samples<4> for P8 {
    fn transparent(p: &[u8; 4]) -> bool {
        p[3] == 0
    }

    fn visible(p: &[u8; 4]) -> bool {
        p[3] >= 128
    }

    /// An 8-bit sample `v` stands for the 16-bit value `257·v`; the
    /// 16-bit result is rounded to the nearest 8-bit value.
    fn over(s: &[u8; 4], d: &mut [u8; 4]) {
        if s[3] == 255 || d[3] == 0 {
            *d = *s;
        } else if s[3] != 0 {
            let wide = |p: &[u8; 4]| p.map(|v| u16::from(v) * 257);
            *d = over16(wide(s), wide(d)).map(narrow);
        }
    }
}

fn px16(p: &[u8; 8]) -> [u16; 4] {
    [
        u16::from_le_bytes([p[0], p[1]]),
        u16::from_le_bytes([p[2], p[3]]),
        u16::from_le_bytes([p[4], p[5]]),
        u16::from_le_bytes([p[6], p[7]]),
    ]
}

impl Samples<8> for P16 {
    fn transparent(p: &[u8; 8]) -> bool {
        p[6] == 0 && p[7] == 0
    }

    fn visible(p: &[u8; 8]) -> bool {
        p[7] >= 0x80
    }

    fn over(s: &[u8; 8], d: &mut [u8; 8]) {
        let (sp, dp) = (px16(s), px16(d));
        if sp[3] == u16::MAX || dp[3] == 0 {
            *d = *s;
        } else if sp[3] != 0 {
            let o = over16(sp, dp);
            for (d, o) in d.as_chunks_mut::<2>().0.iter_mut().zip(o) {
                *d = o.to_le_bytes();
            }
        }
    }
}

fn pxf(p: &[u8; 16]) -> [f32; 4] {
    let c = p.as_chunks::<4>().0;
    [
        f32::from_le_bytes(c[0]),
        f32::from_le_bytes(c[1]),
        f32::from_le_bytes(c[2]),
        f32::from_le_bytes(c[3]),
    ]
}

impl Samples<16> for PF {
    fn transparent(p: &[u8; 16]) -> bool {
        // NaN counts as zero.
        pxf(p)[3].partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
    }

    fn visible(p: &[u8; 16]) -> bool {
        pxf(p)[3] >= 0.5
    }

    /// Alpha is clamped to `[0, 1]` (NaN = 0); with `a = s.a + d.a·(1 −
    /// s.a)` each colour is `(s.c·s.a + d.c·d.a·(1 − s.a)) / a`,
    /// evaluated in `f64` and rounded to `f32`.
    fn over(s: &[u8; 16], d: &mut [u8; 16]) {
        let (sp, dp) = (pxf(s), pxf(d));
        let unit = |a: f32| {
            if a > 0.0 { f64::from(a.min(1.0)) } else { 0.0 }
        };
        let (sa, da) = (unit(sp[3]), unit(dp[3]));
        if sa >= 1.0 || da <= 0.0 {
            *d = *s;
        } else if sa > 0.0 {
            let rest = da * (1.0 - sa);
            let a = sa + rest;
            let chunks = d.as_chunks_mut::<4>().0;
            for ((o, sc), dc) in chunks.iter_mut().zip(sp).zip(dp).take(3) {
                let v = (f64::from(sc) * sa + f64::from(dc) * rest) / a;
                *o = (v as f32).to_le_bytes();
            }
            chunks[3] = (a as f32).to_le_bytes();
        }
    }
}

/// Run `$f::<P, N>($args)` with the [`Samples`] of `$depth`.
macro_rules! by_depth {
    ($depth:expr, $f:ident($($arg:expr),* $(,)?)) => {
        match $depth {
            Depth::U8 => $f::<P8, 4>($($arg),*),
            Depth::U16 => $f::<P16, 8>($($arg),*),
            Depth::F32 => $f::<PF, 16>($($arg),*),
        }
    };
}

/// The first pixel of `bytes`.
fn first<const N: usize>(bytes: &[u8]) -> &[u8; N] {
    &bytes.as_chunks::<N>().0[0]
}

fn same_arr<P: Samples<N>, const N: usize>(a: &[u8; N], b: &[u8; N]) -> bool {
    a == b || (P::transparent(a) && P::transparent(b))
}

fn drop_unchanged_row<P: Samples<N>, const N: usize>(line: &mut [u8], under: &[u8]) {
    let pairs = line.as_chunks_mut::<N>().0.iter_mut();
    for (s, d) in pairs.zip(under.as_chunks::<N>().0) {
        if P::transparent(s) {
            continue;
        }
        let mut shown = *d;
        P::over(s, &mut shown);
        if same_arr::<P, N>(&shown, d) {
            *s = [0; N];
        }
    }
}

/// Make fully transparent every pixel of `line` that, drawn over the
/// pixel of `under` at its place, leaves it as it is.
pub(crate) fn drop_unchanged(depth: Depth, line: &mut [u8], under: &[u8]) {
    by_depth!(depth, drop_unchanged_row(line, under));
}

fn settle_row<P: Samples<N>, const N: usize>(line: &mut [u8], under: Option<&[u8]>, all: bool) {
    // A pixel drawn over itself stays itself: opaque, or transparent.
    let stays = |t: &[u8; N]| {
        let mut shown = *t;
        P::over(t, &mut shown);
        same_arr::<P, N>(&shown, t)
    };
    let pixels = line.as_chunks_mut::<N>().0.iter_mut();
    match under {
        Some(under) => {
            for (t, b) in pixels.zip(under.as_chunks::<N>().0) {
                if same_arr::<P, N>(t, b) && (all || !stays(t)) {
                    *t = [0; N];
                }
            }
        }
        None => {
            for t in pixels {
                if all || !stays(t) {
                    *t = [0; N];
                }
            }
        }
    }
}

/// Settle the pixels of an overlay frame's row: `line` holds what the
/// frame must show and `under` the canvas it is drawn onto (`None`:
/// the canvas already shows `line`). A pixel equal to the canvas pixel
/// becomes fully transparent when drawing it would alter the canvas —
/// or always, when `all` is set.
pub(crate) fn settle_overlay(depth: Depth, line: &mut [u8], under: Option<&[u8]>, all: bool) {
    by_depth!(depth, settle_row(line, under, all));
}

/// What a comparison of a base pixel `a` with a later pixel `b` looks
/// for. Every rule is false for two identical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pred {
    /// The pixels differ.
    Changed,
    /// Visible in `a`, not in `b`.
    Cleared,
    /// The pixels differ and `b` is visible.
    Overlaid,
    /// The pixels differ and `b` drawn over `a` does not show `b`.
    OverFails,
}

/// `pred` on two pixels whose bytes differ.
fn hit<P: Samples<N>, const N: usize>(pred: Pred, a: &[u8; N], b: &[u8; N]) -> bool {
    let changed = !(P::transparent(a) && P::transparent(b));
    match pred {
        Pred::Changed => changed,
        Pred::Cleared => P::visible(a) && !P::visible(b),
        Pred::Overlaid => changed && P::visible(b),
        Pred::OverFails => {
            changed && {
                let mut out = *a;
                P::over(b, &mut out);
                !same_arr::<P, N>(&out, b)
            }
        }
    }
}

/// An axis-aligned rectangle in canvas coordinates (`w`, `h` ≥ 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: i64,
    pub(crate) y: i64,
    pub(crate) w: i64,
    pub(crate) h: i64,
}

impl Rect {
    pub(crate) fn new(x: i64, y: i64, w: i64, h: i64) -> Rect {
        Rect { x, y, w, h }
    }

    /// Pixels of `r`; an empty rectangle costs one pixel, like the 1×1
    /// frame that stands for it.
    pub(crate) fn area(r: Option<Rect>) -> u64 {
        r.map_or(1, |r| (r.w * r.h) as u64)
    }

    pub(crate) fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
        match (a, b) {
            (None, r) | (r, None) => r,
            (Some(a), Some(b)) => {
                let x = a.x.min(b.x);
                let y = a.y.min(b.y);
                let x2 = (a.x + a.w).max(b.x + b.w);
                let y2 = (a.y + a.h).max(b.y + b.h);
                Some(Rect::new(x, y, x2 - x, y2 - y))
            }
        }
    }

    /// The part inside a `w × h` canvas.
    pub(crate) fn clip(self, w: usize, h: usize) -> Option<Rect> {
        let x = self.x.max(0);
        let y = self.y.max(0);
        let x2 = (self.x + self.w).min(w as i64);
        let y2 = (self.y + self.h).min(h as i64);
        (x2 > x && y2 > y).then_some(Rect::new(x, y, x2 - x, y2 - y))
    }

    fn cols(self) -> Range<usize> {
        self.x as usize..(self.x + self.w) as usize
    }

    fn rows(self) -> Range<usize> {
        self.y as usize..(self.y + self.h) as usize
    }
}

/// Canvas pixels the layer operations allocate at most.
const MAX_CANVAS_PIXELS: u64 = 1 << 30;

/// A canvas or a picture's pixels: rows of straight RGBA at one depth,
/// borrowed from the picture when it already has that layout.
#[derive(Clone)]
pub(crate) struct Raster<'a> {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) depth: Depth,
    data: Cow<'a, [u8]>,
}

impl<'a> Raster<'a> {
    /// A `w × h` canvas of `fill`.
    pub(crate) fn filled(w: u32, h: u32, depth: Depth, fill: Px) -> Result<Raster<'static>> {
        let n = u64::from(w) * u64::from(h);
        if n == 0 {
            return Err(Error::invalid(format!("canvas {w}x{h} has a zero side")));
        }
        if n > MAX_CANVAS_PIXELS {
            return Err(Error::limit(format!(
                "canvas {w}x{h} is over the {MAX_CANVAS_PIXELS}-pixel layer limit"
            )));
        }
        let px = fill.as_slice();
        let data = if px.iter().all(|&b| b == 0) {
            vec![0u8; n as usize * px.len()]
        } else {
            px.repeat(n as usize)
        };
        Ok(Raster {
            w: w as usize,
            h: h as usize,
            depth,
            data: Cow::Owned(data),
        })
    }

    /// The pixels of `img` at `depth`: its own bytes when it is the
    /// packed RGBA layout of that depth without row padding, else a
    /// conversion.
    pub(crate) fn of(img: &'a Image, depth: Depth) -> Result<Raster<'a>> {
        let format = depth.format();
        let data = match img.as_packed().filter(|_| img.format() == format) {
            Some(bytes) => Cow::Borrowed(bytes),
            None => Cow::Owned(img.to_packed(format)?),
        };
        let (w, h) = (img.width() as usize, img.height() as usize);
        if data.len() != w * h * depth.bpp() {
            return Err(Error::invalid(format!(
                "{} bytes for a {w}x{h} {format:?} picture",
                data.len()
            )));
        }
        Ok(Raster { w, h, depth, data })
    }

    /// Pixels from bytes already in the layout of `depth`.
    pub(crate) fn from_bytes(w: usize, h: usize, depth: Depth, data: Vec<u8>) -> Raster<'static> {
        debug_assert_eq!(data.len(), w * h * depth.bpp());
        Raster {
            w,
            h,
            depth,
            data: Cow::Owned(data),
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.data.into_owned()
    }

    fn row_bytes(&self) -> usize {
        self.w * self.depth.bpp()
    }

    /// Columns `cols` of row `y`.
    pub(crate) fn span(&self, y: usize, cols: Range<usize>) -> &[u8] {
        let bpp = self.depth.bpp();
        let at = y * self.row_bytes();
        &self.data[at + cols.start * bpp..at + cols.end * bpp]
    }

    fn span_mut(&mut self, y: usize, cols: Range<usize>) -> &mut [u8] {
        let bpp = self.depth.bpp();
        let at = y * self.row_bytes();
        &mut self.data.to_mut()[at + cols.start * bpp..at + cols.end * bpp]
    }

    /// Draw `src` with its top-left corner at `(x, y)`, clipped.
    pub(crate) fn draw(&mut self, src: &Raster, x: i64, y: i64, blend: Blend) {
        let Some(r) = Rect::new(x, y, src.w as i64, src.h as i64).clip(self.w, self.h) else {
            return;
        };
        let depth = self.depth;
        let from = (r.x - x) as usize;
        let source_cols = from..from + r.w as usize;
        for cy in r.rows() {
            let s = src.span((cy as i64 - y) as usize, source_cols.clone());
            let d = self.span_mut(cy, r.cols());
            match blend {
                Blend::Source => d.copy_from_slice(s),
                _ => by_depth!(depth, over_row(s, d)),
            }
        }
    }

    /// `p` over `r` (clipped).
    pub(crate) fn fill(&mut self, r: Rect, p: Px) {
        let Some(r) = r.clip(self.w, self.h) else {
            return;
        };
        let p = p.as_slice();
        for cy in r.rows() {
            for d in self.span_mut(cy, r.cols()).chunks_exact_mut(p.len()) {
                d.copy_from_slice(p);
            }
        }
    }

    /// Alpha zero over `r` (clipped), colours kept.
    pub(crate) fn clear_alpha(&mut self, r: Rect) {
        let Some(r) = r.clip(self.w, self.h) else {
            return;
        };
        let bpp = self.depth.bpp();
        let alpha = bpp / 4 * 3;
        for cy in r.rows() {
            for d in self.span_mut(cy, r.cols()).chunks_exact_mut(bpp) {
                d[alpha..].fill(0);
            }
        }
    }

    /// The bytes of `r` (which lies inside), row after row.
    pub(crate) fn save(&self, r: Rect) -> Vec<u8> {
        let mut out = Vec::with_capacity((r.w * r.h) as usize * self.depth.bpp());
        for cy in r.rows() {
            out.extend_from_slice(self.span(cy, r.cols()));
        }
        out
    }

    /// Put back what [`save`](Self::save) took from `r`.
    pub(crate) fn restore(&mut self, r: Rect, saved: &[u8]) {
        let n = r.w as usize * self.depth.bpp();
        for (cy, s) in r.rows().zip(saved.chunks_exact(n)) {
            self.span_mut(cy, r.cols()).copy_from_slice(s);
        }
    }

    /// Take the pixels of `r` (clipped) from `other`, a raster of the
    /// same size.
    pub(crate) fn copy_from(&mut self, other: &Raster, r: Rect) {
        let Some(r) = r.clip(self.w, self.h) else {
            return;
        };
        for cy in r.rows() {
            self.span_mut(cy, r.cols())
                .copy_from_slice(other.span(cy, r.cols()));
        }
    }

    /// Window `r` as its own raster; what lies outside is transparent.
    pub(crate) fn crop(&self, r: Rect) -> Raster<'static> {
        let bpp = self.depth.bpp();
        let (w, h) = (r.w as usize, r.h as usize);
        let mut data = vec![0u8; w * h * bpp];
        if let Some(c) = r.clip(self.w, self.h) {
            let n = c.w as usize * bpp;
            let left = (c.x - r.x) as usize * bpp;
            for cy in c.rows() {
                let at = (cy as i64 - r.y) as usize * w * bpp + left;
                data[at..at + n].copy_from_slice(self.span(cy, c.cols()));
            }
        }
        Raster::from_bytes(w, h, self.depth, data)
    }
}

fn over_row<P: Samples<N>, const N: usize>(s: &[u8], d: &mut [u8]) {
    for (s, d) in s.as_chunks::<N>().0.iter().zip(d.as_chunks_mut::<N>().0) {
        P::over(s, d);
    }
}

/// What replaces a rectangle of a [`Base`].
#[derive(Clone, Copy)]
pub(crate) enum Patch<'r> {
    /// One colour (a rectangle disposed to the background).
    Colour(Px),
    /// A raster the size of the rectangle (what it held before a frame
    /// was drawn on it).
    Pixels(&'r Raster<'r>),
}

/// What a later raster is compared against: a raster (or one colour
/// everywhere when there is none) with an optional rectangle of it
/// replaced — the canvas a frame leaves once it has been disposed,
/// without building it.
#[derive(Clone, Copy)]
pub(crate) struct Base<'r> {
    /// The raster; `None` = `uniform` everywhere.
    pub(crate) src: Option<&'r Raster<'r>>,
    pub(crate) uniform: Px,
    /// A rectangle inside the canvas holding something else.
    pub(crate) fill: Option<(Rect, Patch<'r>)>,
}

impl<'r> Base<'r> {
    pub(crate) fn of(src: &'r Raster<'r>) -> Base<'r> {
        Base {
            src: Some(src),
            uniform: Px::clear(src.depth),
            fill: None,
        }
    }

    /// Window `r` (inside the canvas) of the base as its own raster.
    pub(crate) fn crop(&self, r: Rect, depth: Depth) -> Raster<'static> {
        let bpp = depth.bpp();
        let (w, h) = (r.w as usize, r.h as usize);
        let mut data = vec![0u8; w * h * bpp];
        for (line, y) in data.chunks_exact_mut(w * bpp).zip(r.rows()) {
            match self.src {
                Some(src) => line.copy_from_slice(src.span(y, r.cols())),
                None => {
                    for d in line.chunks_exact_mut(bpp) {
                        d.copy_from_slice(self.uniform.as_slice());
                    }
                }
            }
            let Some((f, patch)) = &self.fill else {
                continue;
            };
            if !f.rows().contains(&y) {
                continue;
            }
            let x0 = f.cols().start.clamp(r.cols().start, r.cols().end);
            let x1 = f.cols().end.clamp(r.cols().start, r.cols().end);
            if x0 >= x1 {
                continue;
            }
            let part = &mut line[(x0 - r.cols().start) * bpp..(x1 - r.cols().start) * bpp];
            match patch {
                Patch::Colour(p) => {
                    for d in part.chunks_exact_mut(bpp) {
                        d.copy_from_slice(p.as_slice());
                    }
                }
                Patch::Pixels(px) => part.copy_from_slice(
                    px.span(y - f.rows().start, x0 - f.cols().start..x1 - f.cols().start),
                ),
            }
        }
        Raster::from_bytes(w, h, depth, data)
    }
}

type Bounds = Option<[usize; 4]>;

fn merge(a: Bounds, b: Bounds) -> Bounds {
    match (a, b) {
        (None, r) | (r, None) => r,
        (Some(a), Some(b)) => Some([
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]),
    }
}

/// First and last pixel of `b` that `pred` selects against the pixels
/// of `a`.
fn extent<P: Samples<N>, const N: usize>(pred: Pred, a: &[u8], b: &[u8]) -> Option<(usize, usize)> {
    if a == b {
        return None;
    }
    let pairs = || a.as_chunks::<N>().0.iter().zip(b.as_chunks::<N>().0);
    let test = |(a, b): (&[u8; N], &[u8; N])| a != b && hit::<P, N>(pred, a, b);
    let first = pairs().position(test)?;
    let last = pairs().rposition(test).unwrap_or(first);
    Some((first, last))
}

/// [`extent`] against one colour.
fn extent_uniform<P: Samples<N>, const N: usize>(
    pred: Pred,
    a: &[u8],
    b: &[u8],
) -> Option<(usize, usize)> {
    let a = first::<N>(a);
    let pixels = b.as_chunks::<N>().0;
    let test = |b: &[u8; N]| a != b && hit::<P, N>(pred, a, b);
    let first = pixels.iter().position(test)?;
    let last = pixels.iter().rposition(test).unwrap_or(first);
    Some((first, last))
}

fn scan_rows<P: Samples<N>, const N: usize>(
    base: &Base,
    cur: &Raster,
    cols: Range<usize>,
    rows: Range<usize>,
    pred: Pred,
) -> Bounds {
    let mut bounds: Bounds = None;
    for y in rows {
        // The row in up to three segments: before, inside and after the
        // base's filled rectangle.
        let (f0, f1, fill) = match &base.fill {
            Some((r, p)) if r.rows().contains(&y) => (
                r.cols().start.clamp(cols.start, cols.end),
                r.cols().end.clamp(cols.start, cols.end),
                Some(p),
            ),
            _ => (cols.start, cols.start, None),
        };
        let mut row: Option<(usize, usize)> = None;
        let segments = [(cols.start..f0, None), (f0..f1, fill), (f1..cols.end, None)];
        for (seg, patch) in segments {
            if seg.is_empty() {
                continue;
            }
            let b = cur.span(y, seg.clone());
            let found = match (patch, base.src) {
                (Some(Patch::Colour(p)), _) => extent_uniform::<P, N>(pred, p.as_slice(), b),
                (Some(Patch::Pixels(px)), _) => {
                    let (fx, fy) = match &base.fill {
                        Some((r, _)) => (r.cols().start, r.rows().start),
                        None => (0, 0),
                    };
                    extent::<P, N>(pred, px.span(y - fy, seg.start - fx..seg.end - fx), b)
                }
                (None, Some(src)) => extent::<P, N>(pred, src.span(y, seg.clone()), b),
                (None, None) => extent_uniform::<P, N>(pred, base.uniform.as_slice(), b),
            };
            if let Some((first, last)) = found {
                let (first, last) = (seg.start + first, seg.start + last);
                row = Some(match row {
                    None => (first, last),
                    Some((lo, hi)) => (lo.min(first), hi.max(last)),
                });
            }
        }
        if let Some((lo, hi)) = row {
            bounds = merge(bounds, Some([lo, y, hi, y]));
        }
    }
    bounds
}

/// Rows a band must hold before a comparison is split across threads.
const MIN_BAND_PIXELS: usize = 1 << 16;

/// The bounding box of the pixels of `cur` that `pred` selects against
/// `base`, inside `window` (the whole canvas when `None`), on up to
/// `threads` row bands.
pub(crate) fn scan(
    base: &Base,
    cur: &Raster,
    window: Option<Rect>,
    pred: Pred,
    threads: usize,
) -> Option<Rect> {
    let window = match window {
        Some(r) => r.clip(cur.w, cur.h)?,
        None => Rect::new(0, 0, cur.w as i64, cur.h as i64),
    };
    let (cols, rows) = (window.cols(), window.rows());
    let depth = cur.depth;
    let bands = threads
        .min(rows.len() * cols.len() / MIN_BAND_PIXELS)
        .min(rows.len())
        .max(1);
    let bounds = if bands == 1 {
        by_depth!(depth, scan_rows(base, cur, cols, rows, pred))
    } else {
        let per = rows.len().div_ceil(bands);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..bands)
                .map(|k| {
                    let band = (rows.start + k * per).min(rows.end)
                        ..(rows.start + (k + 1) * per).min(rows.end);
                    let cols = cols.clone();
                    scope.spawn(move || by_depth!(depth, scan_rows(base, cur, cols, band, pred)))
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("a comparison band panicked"))
                .fold(None, merge)
        })
    };
    bounds.map(|[x0, y0, x1, y1]| {
        Rect::new(
            x0 as i64,
            y0 as i64,
            (x1 - x0 + 1) as i64,
            (y1 - y0 + 1) as i64,
        )
    })
}
