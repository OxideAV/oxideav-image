//! The pixel-format ladder: which input layouts `encode` offers an
//! encoder, in which order.
//!
//! The rule (README, *Pixel-format ladder*): the source layout first;
//! then the colour layouts of the source's own family by decreasing
//! fidelity; then the other colour family; grey only when the source is
//! grey or nothing colour is accepted. An encoder that declares its
//! accepted layouts (`CodecCapabilities::accepted_pixel_formats`) gets
//! exactly those, in the same order; one that declares nothing gets the
//! fixed rung list below, ordered the same way.

use oxideav_core::PixelFormat;
use oxideav_pixfmt::supports;

/// Colour family of a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Rgb,
    Yuv,
    Gray,
}

fn family(f: PixelFormat) -> Family {
    if f.chroma_subsampling().is_some() {
        return Family::Yuv;
    }
    let name = format!("{f:?}");
    if name.starts_with("Gray") || name.starts_with("Mono") || name.starts_with("Ya") {
        Family::Gray
    } else {
        Family::Rgb
    }
}

/// More than eight bits per sample (integer deep tiers and float).
pub(crate) fn is_deep(f: PixelFormat) -> bool {
    if f.is_float() {
        return true;
    }
    let name = format!("{f:?}");
    ["10Le", "12Le", "14Le", "16Le", "48Le", "64Le"]
        .iter()
        .any(|t| name.ends_with(t))
}

/// Chroma fidelity of a YUV layout: 4:4:4 = 3, 4:2:2 / 4:4:0 = 2,
/// 4:2:0 = 1, 4:1:1 = 0. Non-YUV layouts rank as full colour (3).
fn fidelity(f: PixelFormat) -> u8 {
    match f.chroma_subsampling() {
        None | Some((0, 0)) => 3,
        Some((1, 0)) | Some((0, 1)) => 2,
        Some((1, 1)) => 1,
        _ => 0,
    }
}

/// Rungs offered to an encoder that declares no accepted layouts.
/// Order is irrelevant here: [`ladder`] sorts them for the source.
const RUNGS: &[PixelFormat] = &[
    PixelFormat::Rgba,
    PixelFormat::Rgb24,
    PixelFormat::Rgba64Le,
    PixelFormat::Rgb48Le,
    PixelFormat::Yuva444P,
    PixelFormat::Yuva420P,
    PixelFormat::Yuv444P,
    PixelFormat::Yuv422P,
    PixelFormat::Yuv420P,
    PixelFormat::Yuv444P10Le,
    PixelFormat::Yuv420P10Le,
    PixelFormat::Yuv444P16Le,
    PixelFormat::Ya8,
    PixelFormat::Gray8,
    PixelFormat::Gray16Le,
];

/// Sort key of a candidate layout `f` for a source `src`: lower first.
fn rank(src: PixelFormat, f: PixelFormat) -> (u8, u8, u8, u8, u8, String) {
    let sf = family(src);
    let ff = family(f);
    // Family: the source's own first; grey last unless the source is
    // grey (then grey first, RGB — exact for grey — before YUV).
    let fam = match (sf, ff) {
        (a, b) if a == b => 0,
        (Family::Gray, Family::Rgb) => 1,
        (Family::Gray, Family::Yuv) => 2,
        (_, Family::Gray) => 2,
        _ => 1,
    };
    // Tier: the source's own bit tier first.
    let tier = u8::from(is_deep(f) != is_deep(src));
    // Alpha: keep it when the source has it, do not invent it otherwise.
    let alpha = u8::from(f.has_alpha() != src.has_alpha());
    // Fidelity: at or below the source by decreasing fidelity, then
    // above it, closest first.
    let (s, c) = (fidelity(src), fidelity(f));
    let fid = if c <= s { s - c } else { 4 + (c - s) };
    // Packed before planar within RGB (Rgb24 before Gbrp8).
    let planar = u8::from(f.is_planar());
    (fam, tier, alpha, fid, planar, format!("{f:?}"))
}

/// The layouts to offer an encoder for a picture in `src`, in order.
/// `declared` is the encoder's accepted set (empty = undeclared, use
/// the fixed rungs). Every rung but the source itself must be reachable
/// through `oxideav-pixfmt`.
pub(crate) fn ladder(src: PixelFormat, declared: &[PixelFormat]) -> Vec<PixelFormat> {
    let mut rungs: Vec<PixelFormat> = if declared.is_empty() {
        RUNGS.to_vec()
    } else {
        declared.to_vec()
    };
    rungs.sort_by_cached_key(|f| rank(src, *f));
    rungs.dedup();
    let mut out = Vec::with_capacity(rungs.len() + 1);
    if declared.is_empty() || declared.contains(&src) {
        out.push(src);
    }
    for f in rungs {
        if !out.contains(&f) && supports(src, f) {
            out.push(f);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use PixelFormat as P;

    fn pos(l: &[P], f: P) -> usize {
        l.iter()
            .position(|x| *x == f)
            .unwrap_or_else(|| panic!("{f:?} not in {l:?}"))
    }

    #[test]
    fn rgb_source_offers_colour_before_grey() {
        let l = ladder(P::Rgb24, &[]);
        assert_eq!(l[0], P::Rgb24);
        assert_eq!(l[1], P::Rgba);
        assert!(pos(&l, P::Yuv444P) < pos(&l, P::Yuv422P));
        assert!(pos(&l, P::Yuv422P) < pos(&l, P::Yuv420P));
        assert!(pos(&l, P::Yuv420P) < pos(&l, P::Gray8));
        assert!(pos(&l, P::Rgb48Le) < pos(&l, P::Yuv444P));
        assert_eq!(*l.last().unwrap(), P::Gray16Le);
        // Alpha sources keep alpha-carrying rungs ahead.
        let l = ladder(P::Rgba, &[]);
        assert_eq!(&l[..2], &[P::Rgba, P::Rgb24]);
        assert!(pos(&l, P::Yuva444P) < pos(&l, P::Yuv444P));
        assert!(pos(&l, P::Yuv420P) < pos(&l, P::Gray8));
    }

    #[test]
    fn yuv_source_walks_down_then_up_then_rgb() {
        let l = ladder(P::Yuv422P, &[]);
        assert_eq!(&l[..3], &[P::Yuv422P, P::Yuv420P, P::Yuv444P]);
        assert!(pos(&l, P::Yuv444P) < pos(&l, P::Rgb24));
        assert!(pos(&l, P::Rgb24) < pos(&l, P::Gray8));
        let l = ladder(P::Yuv420P, &[]);
        assert_eq!(&l[..3], &[P::Yuv420P, P::Yuv422P, P::Yuv444P]);
    }

    #[test]
    fn grey_source_starts_grey_then_rgb() {
        let l = ladder(P::Gray8, &[]);
        assert_eq!(l[0], P::Gray8);
        assert!(pos(&l, P::Gray16Le) < pos(&l, P::Rgb24));
        assert!(pos(&l, P::Rgb24) < pos(&l, P::Yuv444P));
    }

    #[test]
    fn deep_source_prefers_its_tier() {
        let l = ladder(P::Rgb48Le, &[]);
        assert_eq!(&l[..3], &[P::Rgb48Le, P::Rgba64Le, P::Rgb24]);
        let l = ladder(P::Gray16Le, &[]);
        assert_eq!(&l[..2], &[P::Gray16Le, P::Gray8]);
    }

    #[test]
    fn declared_sets_are_ordered_the_same_way_and_skip_an_undeclared_source() {
        let declared = [P::Gray8, P::Yuv420P, P::Yuv444P];
        let l = ladder(P::Rgb24, &declared);
        assert_eq!(l, vec![P::Yuv444P, P::Yuv420P, P::Gray8]);
        let l = ladder(P::Yuv420P, &declared);
        assert_eq!(l, vec![P::Yuv420P, P::Yuv444P, P::Gray8]);
        let l = ladder(P::Gray8, &declared);
        assert_eq!(l, vec![P::Gray8, P::Yuv444P, P::Yuv420P]);
        assert_eq!(ladder(P::Rgba, &[P::Yuv420P]), vec![P::Yuv420P]);
    }

    #[test]
    fn families_and_tiers() {
        assert_eq!(family(P::YuvJ420P), Family::Yuv);
        assert_eq!(family(P::Nv12), Family::Yuv);
        assert_eq!(family(P::Gbrp8), Family::Rgb);
        assert_eq!(family(P::Pal8), Family::Rgb);
        assert_eq!(family(P::Ya16Le), Family::Gray);
        assert_eq!(family(P::MonoBlack), Family::Gray);
        assert!(is_deep(P::Yuv420P10Le));
        assert!(is_deep(P::RgbaF32Le));
        assert!(!is_deep(P::Bgra));
    }
}
