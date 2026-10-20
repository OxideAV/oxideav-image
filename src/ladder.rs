//! The pixel-format ladder: which input layouts `encode` offers an
//! encoder, in which order.
//!
//! The rule (README, *Pixel-format ladder*): colour-model distance
//! first — the source's own model, then the other additive colour
//! model (RGB ↔ YCbCr), then CMYK, grey last (a grey source walks
//! grey → RGB → YCbCr → CMYK); within a model the source's bit tier
//! first, the source's transparency kept (an alpha layout whose
//! samples are all opaque prefers the opaque layout of the same model,
//! ahead of itself), YCbCr rungs at or below the source by decreasing
//! chroma fidelity, then above it closest first, packed before planar.
//! An encoder that declares its accepted layouts
//! (`CodecCapabilities::accepted_pixel_formats`) gets exactly those,
//! in that order; one that declares nothing gets the fixed rung list
//! below, ordered the same way.

use oxideav_core::PixelFormat;
use oxideav_pixfmt::supports;

/// Colour model of a layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Rgb,
    Yuv,
    Cmyk,
    Gray,
}

fn family(f: PixelFormat) -> Family {
    if matches!(f, PixelFormat::Cmyk | PixelFormat::CmykInverted) {
        return Family::Cmyk;
    }
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

/// Colour-model distance from a source model to a candidate's: the
/// source's own model is 0; the additive models are each other's
/// nearest neighbour; CMYK is reachable from RGB / YCbCr only after
/// both (a four-ink layout for a three-channel picture is a last
/// resort, never a tie-breaker on channel count); grey is last for
/// every colour source, and a grey source walks RGB (exact for grey)
/// before YCbCr before CMYK.
fn model_distance(src: Family, f: Family) -> u8 {
    use Family::*;
    if src == f {
        return 0;
    }
    match (src, f) {
        (Rgb, Yuv) | (Yuv, Rgb) | (Cmyk, Rgb) | (Gray, Rgb) => 1,
        (Rgb, Cmyk) | (Yuv, Cmyk) | (Cmyk, Yuv) | (Gray, Yuv) => 2,
        (_, Gray) | (Gray, Cmyk) => 3,
        _ => 3,
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

/// Sort key of a candidate layout `f` for a source `src`
/// (`transparent`: the source carries transparency — an alpha sample
/// below full, or a palette entry that is not opaque): lower first.
fn rank(
    src: PixelFormat,
    transparent: bool,
    f: PixelFormat,
) -> (u8, u8, u8, u8, u8, u8, u8, String) {
    // Colour model: distance from the source's, doubled to leave room
    // for a palette rung — quantising to 256 entries loses more than
    // any model change, so `Pal8` ranks after CMYK and before grey for
    // every source that is not itself indexed.
    let fam = if f.is_palette() && f != src {
        5
    } else {
        2 * model_distance(family(src), family(f))
    };
    // Tier: the source's own bit tier first.
    let tier = u8::from(is_deep(f) != is_deep(src));
    // Fidelity: at or below the source by decreasing fidelity, then
    // above it, closest first.
    let (s, c) = (fidelity(src), fidelity(f));
    let fid = if c <= s { s - c } else { 4 + (c - s) };
    // An opaque picture in an alpha layout: the opaque layouts of its
    // model, tier, fidelity and packing come ahead of the source itself
    // (`Rgb24` before an all-opaque `Rgba`, `Yuv420P` before `Yuva420P`).
    let opaque_first = u8::from(
        !(src.has_alpha()
            && !transparent
            && !f.has_alpha()
            && fid == 0
            && f.is_planar() == src.is_planar()),
    );
    // The source itself leads the rest.
    let own = u8::from(f != src);
    // Alpha: keep it when the source is transparent, do not invent it
    // otherwise.
    let alpha = u8::from(f.has_alpha() != transparent);
    // Packed before planar within RGB (Rgb24 before Gbrp8).
    let planar = u8::from(f.is_planar());
    (
        fam,
        tier,
        opaque_first,
        own,
        alpha,
        fid,
        planar,
        format!("{f:?}"),
    )
}

/// The layouts to offer an encoder for a picture in `src`, in order.
/// `declared` is the encoder's accepted set (empty = undeclared, use
/// the fixed rungs). Every rung but the source itself must be reachable
/// through `oxideav-pixfmt`. The source's transparency is read from its
/// layout; [`ladder_with_alpha`] takes it explicitly (palette alpha,
/// an alpha channel that is fully opaque).
pub(crate) fn ladder(src: PixelFormat, declared: &[PixelFormat]) -> Vec<PixelFormat> {
    ladder_with_alpha(src, src.has_alpha(), declared)
}

/// [`ladder`] for a source whose transparency is known independently of
/// its layout: a `Pal8` picture with a transparent palette entry ranks
/// the alpha-carrying rungs first (`Rgba` before `Rgb24`); an `Rgba`
/// picture whose alpha samples are all full (`transparent = false`)
/// ranks `Rgb24` ahead of `Rgba` itself.
pub(crate) fn ladder_with_alpha(
    src: PixelFormat,
    transparent: bool,
    declared: &[PixelFormat],
) -> Vec<PixelFormat> {
    let mut rungs: Vec<PixelFormat> = if declared.is_empty() {
        RUNGS.to_vec()
    } else {
        declared.to_vec()
    };
    if declared.is_empty() || declared.contains(&src) {
        rungs.push(src);
    }
    rungs.sort_by_cached_key(|f| rank(src, transparent, *f));
    rungs.dedup();
    let mut out = Vec::with_capacity(rungs.len());
    for f in rungs {
        if !out.contains(&f) && (f == src || supports(src, f)) {
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
    fn palette_alpha_ranks_alpha_rungs_first() {
        // Pal8 itself has no alpha channel: without palette alpha Rgb24
        // leads, with it Rgba does — in the fixed rungs and in a
        // declared set alike.
        let l = ladder(P::Pal8, &[]);
        assert!(pos(&l, P::Rgb24) < pos(&l, P::Rgba));
        let l = ladder_with_alpha(P::Pal8, true, &[]);
        assert_eq!(&l[..2], &[P::Pal8, P::Rgba]);
        assert!(pos(&l, P::Rgba) < pos(&l, P::Rgb24));
        assert!(pos(&l, P::Rgba64Le) < pos(&l, P::Rgb48Le));
        assert!(pos(&l, P::Yuva444P) < pos(&l, P::Yuv444P));
        let declared = [P::Rgb24, P::Rgba, P::Gray8];
        assert_eq!(
            ladder_with_alpha(P::Pal8, true, &declared),
            vec![P::Rgba, P::Rgb24, P::Gray8]
        );
        assert_eq!(
            ladder_with_alpha(P::Pal8, false, &declared),
            vec![P::Rgb24, P::Rgba, P::Gray8]
        );
    }

    /// The layouts a JPEG encoder declares (8 / 12-bit YCbCr, grey
    /// 8–16, RGB-coded, planar GBR, CMYK) — the set that exposed the
    /// defect: an `Rgba` source ranked `Cmyk` before `Rgb24` because
    /// both are "RGB family, no alpha" and `C` sorts before `R`.
    const JPEG_LIKE: &[P] = &[
        P::YuvJ444P,
        P::YuvJ422P,
        P::YuvJ420P,
        P::Yuv444P,
        P::Yuv422P,
        P::Yuv420P,
        P::Yuv440P,
        P::Yuv444P12Le,
        P::Yuv422P12Le,
        P::Yuv420P12Le,
        P::Gray8,
        P::Gray12Le,
        P::Gray16Le,
        P::Rgb24,
        P::Rgb48Le,
        P::Gbrp8,
        P::Gbrp12Le,
        P::Cmyk,
        P::CmykInverted,
    ];

    #[test]
    fn cmyk_is_a_last_resort_for_three_channel_sources() {
        // Rgba (transparent or not) → Rgb24 first, YCbCr next, CMYK after
        // every additive rung, grey last.
        for transparent in [true, false] {
            let l = ladder_with_alpha(P::Rgba, transparent, JPEG_LIKE);
            assert_eq!(l[0], P::Rgb24, "{l:?}");
            assert_eq!(l[1], P::Gbrp8, "{l:?}");
            assert!(pos(&l, P::YuvJ444P) < pos(&l, P::Cmyk), "{l:?}");
            assert!(pos(&l, P::Yuv420P12Le) < pos(&l, P::Cmyk), "{l:?}");
            assert!(pos(&l, P::Cmyk) < pos(&l, P::Gray8), "{l:?}");
            assert!(pos(&l, P::CmykInverted) < pos(&l, P::Gray8), "{l:?}");
        }
        // The undeclared rungs never contain CMYK at all.
        assert!(!ladder(P::Rgba, &[]).contains(&P::Cmyk));
        // A YCbCr source: its own model, RGB, then CMYK, then grey.
        let l = ladder(P::Yuv420P, JPEG_LIKE);
        assert_eq!(&l[..2], &[P::Yuv420P, P::YuvJ420P]);
        assert!(pos(&l, P::Rgb24) < pos(&l, P::Cmyk));
        assert!(pos(&l, P::Cmyk) < pos(&l, P::Gray8));
        // A grey source: grey, RGB, YCbCr, CMYK.
        let l = ladder(P::Gray8, JPEG_LIKE);
        assert_eq!(&l[..3], &[P::Gray8, P::Gray12Le, P::Gray16Le]);
        assert!(pos(&l, P::Rgb24) < pos(&l, P::YuvJ444P));
        assert!(pos(&l, P::YuvJ444P) < pos(&l, P::Cmyk));
        assert_eq!(*l.last().unwrap(), P::CmykInverted);
        // CMYK is first only for a CMYK source, then RGB, YCbCr, grey.
        let l = ladder(P::Cmyk, JPEG_LIKE);
        assert_eq!(&l[..2], &[P::Cmyk, P::CmykInverted]);
        assert!(pos(&l, P::Rgb24) < pos(&l, P::YuvJ444P));
        assert!(pos(&l, P::YuvJ444P) < pos(&l, P::Gray8));
        // Nothing closer accepted: CMYK is taken rather than grey.
        assert_eq!(
            ladder(P::Rgb24, &[P::Gray8, P::Cmyk]),
            vec![P::Cmyk, P::Gray8]
        );
        // A palette rung is a quantisation: after every true-colour
        // layout, CMYK included, before grey — unless the source is
        // indexed itself.
        assert_eq!(
            ladder(P::Rgb24, &[P::Pal8, P::Gray8, P::Cmyk, P::Yuv420P, P::Rgba]),
            vec![P::Rgba, P::Yuv420P, P::Cmyk, P::Pal8, P::Gray8]
        );
        assert_eq!(ladder(P::Pal8, &[P::Pal8, P::Rgb24])[0], P::Pal8);
    }

    #[test]
    fn opaque_alpha_source_prefers_the_opaque_layout_of_its_model() {
        // Every alpha sample full: Rgb24 leads, the source's own layout
        // follows (still ahead of every other alpha layout).
        let l = ladder_with_alpha(P::Rgba, false, &[]);
        assert_eq!(&l[..3], &[P::Rgb24, P::Rgba, P::Rgb48Le]);
        assert!(pos(&l, P::Yuv444P) < pos(&l, P::Yuva444P));
        let l = ladder_with_alpha(P::Rgba64Le, false, &[]);
        assert_eq!(&l[..2], &[P::Rgb48Le, P::Rgba64Le]);
        let l = ladder_with_alpha(P::Yuva420P, false, &[]);
        assert_eq!(&l[..2], &[P::Yuv420P, P::Yuva420P]);
        // Transparent: the source leads as before.
        let l = ladder_with_alpha(P::Rgba, true, &[]);
        assert_eq!(&l[..2], &[P::Rgba, P::Rgb24]);
        // Declared sets: the same, and the source only when declared.
        let declared = [P::Gray8, P::Rgba, P::Rgb24, P::Bgra];
        assert_eq!(
            ladder_with_alpha(P::Rgba, false, &declared),
            vec![P::Rgb24, P::Rgba, P::Bgra, P::Gray8]
        );
        assert_eq!(
            ladder_with_alpha(P::Rgba, true, &declared),
            vec![P::Rgba, P::Bgra, P::Rgb24, P::Gray8]
        );
        assert_eq!(
            ladder_with_alpha(P::Rgba, false, &[P::Bgra, P::Rgb24]),
            vec![P::Rgb24, P::Bgra]
        );
    }

    #[test]
    fn families_and_tiers() {
        assert_eq!(family(P::YuvJ420P), Family::Yuv);
        assert_eq!(family(P::Nv12), Family::Yuv);
        assert_eq!(family(P::Gbrp8), Family::Rgb);
        assert_eq!(family(P::Pal8), Family::Rgb);
        assert_eq!(family(P::Ya16Le), Family::Gray);
        assert_eq!(family(P::MonoBlack), Family::Gray);
        assert_eq!(family(P::Cmyk), Family::Cmyk);
        assert_eq!(family(P::CmykInverted), Family::Cmyk);
        assert!(is_deep(P::Yuv420P10Le));
        assert!(is_deep(P::RgbaF32Le));
        assert!(!is_deep(P::Bgra));
        assert_eq!(model_distance(Family::Rgb, Family::Rgb), 0);
        assert!(
            model_distance(Family::Rgb, Family::Yuv) < model_distance(Family::Rgb, Family::Cmyk)
        );
        assert!(
            model_distance(Family::Rgb, Family::Cmyk) < model_distance(Family::Rgb, Family::Gray)
        );
        assert!(
            model_distance(Family::Gray, Family::Rgb) < model_distance(Family::Gray, Family::Yuv)
        );
        assert!(
            model_distance(Family::Gray, Family::Yuv) < model_distance(Family::Gray, Family::Cmyk)
        );
        assert!(
            model_distance(Family::Cmyk, Family::Rgb) < model_distance(Family::Cmyk, Family::Yuv)
        );
    }
}
