//! Exactness of the layer model: the OVER operator against its
//! definition, and `coalesce ∘ optimize_layers = coalesce` on random
//! animations covering every disposal × blending pair, frames partly
//! or wholly outside the canvas, every alpha value, the three depths,
//! both background rules and every restriction of the optimiser.

use std::time::Duration;

use crate::layers::{
    self, BackgroundRule, Direction, LayerOptions, LayerSequence, OptimizeOptions,
};
use crate::{Blend, Dispose, Error, Image, Page, PixelFormat};

/// xorshift64*: a deterministic stream for the property tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        (self.next() >> 11) % n
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Depth {
    U8,
    U16,
    F32,
}

impl Depth {
    fn format(self) -> PixelFormat {
        match self {
            Depth::U8 => PixelFormat::Rgba,
            Depth::U16 => PixelFormat::Rgba64Le,
            Depth::F32 => PixelFormat::RgbaF32Le,
        }
    }

    fn bpp(self) -> usize {
        match self {
            Depth::U8 => 4,
            Depth::U16 => 8,
            Depth::F32 => 16,
        }
    }

    /// One sample from a 16-bit value.
    fn sample(self, v: u16, out: &mut Vec<u8>) {
        match self {
            Depth::U8 => out.push((v >> 8) as u8),
            Depth::U16 => out.extend_from_slice(&v.to_le_bytes()),
            Depth::F32 => out.extend_from_slice(&(f32::from(v) / 65535.0).to_le_bytes()),
        }
    }

    fn transparent(self, p: &[u8]) -> bool {
        match self {
            Depth::U8 => p[3] == 0,
            Depth::U16 => p[6] == 0 && p[7] == 0,
            Depth::F32 => f32::from_le_bytes([p[12], p[13], p[14], p[15]]) <= 0.0,
        }
    }
}

/// A random pixel: a small palette of colours (so frames repeat
/// values), alpha zero, full or anything in between.
fn random_px(rng: &mut Rng, depth: Depth, out: &mut Vec<u8>) {
    let levels = [0u16, 0x5555, 0xaaaa, 0xffff];
    for _ in 0..3 {
        depth.sample(rng.pick(&levels), out);
    }
    let alpha = match rng.below(8) {
        0 | 1 => 0,
        2..=4 => 0xffff,
        5 => 0x7fff,
        6 => 0x8000,
        _ => rng.below(0x10000) as u16,
    };
    depth.sample(alpha, out);
}

fn random_frame(rng: &mut Rng, depth: Depth, canvas: (u32, u32)) -> Image {
    let (cw, ch) = (i64::from(canvas.0), i64::from(canvas.1));
    let (w, h) = (rng.range(1, cw + 2), rng.range(1, ch + 2));
    let mut data = Vec::new();
    // A frame is one colour, or noise, or noise on one colour.
    let mut solid = Vec::new();
    random_px(rng, depth, &mut solid);
    let noise = rng.below(3);
    for _ in 0..w * h {
        if noise == 0 || (noise == 1 && rng.below(3) == 0) {
            random_px(rng, depth, &mut data);
        } else {
            data.extend_from_slice(&solid);
        }
    }
    // Offsets from wholly outside (no intersection) to inside.
    let (x, y) = (rng.range(-w - 1, cw + 1), rng.range(-h - 1, ch + 1));
    Image::from_raw(w as u32, h as u32, depth.format(), data)
        .unwrap()
        .with_page(Page::new(canvas.0, canvas.1).with_offset(x as i32, y as i32))
        .with_dispose(rng.pick(&[
            Dispose::Unspecified,
            Dispose::None,
            Dispose::Background,
            Dispose::Previous,
        ]))
        .with_blend(rng.pick(&[Blend::Over, Blend::Source]))
        .with_delay_ticks(rng.below(20), 100)
}

fn random_animation(rng: &mut Rng, depth: Depth) -> Vec<Image> {
    let canvas = (rng.range(1, 9) as u32, rng.range(1, 9) as u32);
    let n = rng.range(1, 7);
    (0..n).map(|_| random_frame(rng, depth, canvas)).collect()
}

/// Every disposal × blending pair on consecutive frames, twice over.
fn all_pairs_animation(rng: &mut Rng, depth: Depth) -> Vec<Image> {
    let canvas = (7, 6);
    let mut frames = Vec::new();
    for dispose in [
        Dispose::Unspecified,
        Dispose::None,
        Dispose::Background,
        Dispose::Previous,
    ] {
        for blend in [Blend::Over, Blend::Source] {
            for _ in 0..2 {
                frames.push(
                    random_frame(rng, depth, canvas)
                        .with_dispose(dispose)
                        .with_blend(blend),
                );
            }
        }
    }
    frames
}

/// The pictures are equal: same size, and every pixel identical or
/// fully transparent in both.
fn assert_same_pictures(a: &[Image], b: &[Image], depth: Depth, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: frame count");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert_eq!(
            (x.width(), x.height(), x.offset(), x.canvas_size()),
            (y.width(), y.height(), y.offset(), y.canvas_size()),
            "{what}: frame {i} geometry"
        );
        let (px, py) = (
            x.to_packed(depth.format()).unwrap(),
            y.to_packed(depth.format()).unwrap(),
        );
        for (k, (p, q)) in px
            .chunks_exact(depth.bpp())
            .zip(py.chunks_exact(depth.bpp()))
            .enumerate()
        {
            assert!(
                p == q || (depth.transparent(p) && depth.transparent(q)),
                "{what}: frame {i} pixel {k}: {p:?} != {q:?}"
            );
        }
    }
}

fn cases(full: usize) -> usize {
    if cfg!(miri) { 3 } else { full }
}

/// Every combination of the optimiser's switches that keeps
/// `Blend::Source`.
fn option_grid() -> Vec<OptimizeOptions> {
    let mut grid = Vec::new();
    for transparency in [false, true] {
        for background in [true, false] {
            for previous in [true, false] {
                grid.push(
                    OptimizeOptions::new()
                        .with_transparency(transparency)
                        .with_dispose_background(background)
                        .with_dispose_previous(previous),
                );
            }
        }
    }
    grid
}

fn check_identity(frames: &[Image], depth: Depth, layer: LayerOptions, seed: u64) {
    let want = layers::coalesce_with(frames, &layer).unwrap();
    // A coalesced list plays back to itself.
    let replay = layers::coalesce_with(&want, &layer).unwrap();
    assert_same_pictures(
        &replay,
        &want,
        depth,
        &format!("seed {seed} {depth:?} coalesce twice"),
    );
    for opts in option_grid() {
        let opts = opts.with_layer_options(layer);
        let small = layers::optimize_layers_with(frames, &opts).unwrap();
        let what = format!("seed {seed} {depth:?} {opts:?}");
        assert_eq!(small.len(), frames.len(), "{what}");
        for (f, s) in frames.iter().zip(&small) {
            let (cw, ch) = f.canvas_size();
            assert_eq!(s.delay(), f.delay(), "{what}: delay");
            // Inside the canvas, never larger than it.
            let (x, y) = s.offset();
            assert!(x >= 0 && y >= 0, "{what}: offset {x},{y}");
            assert!(
                x as u32 + s.width() <= cw && y as u32 + s.height() <= ch,
                "{what}"
            );
            if !opts.dispose_previous {
                assert_ne!(s.dispose(), Dispose::Previous, "{what}");
            }
            if !opts.dispose_background {
                assert_ne!(s.dispose(), Dispose::Background, "{what}");
            }
        }
        let got = layers::coalesce_with(&small, &layer).unwrap();
        assert_same_pictures(&got, &want, depth, &what);
        // Optimising what is already optimised changes no picture.
        let again = layers::optimize_layers_with(&small, &opts).unwrap();
        let replay = layers::coalesce_with(&again, &layer).unwrap();
        assert_same_pictures(&replay, &want, depth, &format!("{what} (twice)"));
    }
}

#[test]
fn optimised_lists_coalesce_back_to_the_same_pictures() {
    for depth in [Depth::U8, Depth::U16, Depth::F32] {
        for seed in 1..=cases(120) as u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) + depth.bpp() as u64);
            let frames = random_animation(&mut rng, depth);
            check_identity(&frames, depth, LayerOptions::new(), seed);
        }
    }
}

#[test]
fn every_disposal_and_blending_pair_replays_exactly() {
    for depth in [Depth::U8, Depth::U16, Depth::F32] {
        for seed in 1..=cases(12) as u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) + depth.bpp() as u64);
            let frames = all_pairs_animation(&mut rng, depth);
            check_identity(&frames, depth, LayerOptions::new(), seed);
        }
    }
}

#[test]
fn identity_holds_on_coloured_backgrounds_and_under_both_rules() {
    let backgrounds: [[u16; 4]; 3] = [
        [0xffff, 0xffff, 0xffff, 0xffff],
        [0x1234, 0xabcd, 0x0f0f, 0x8000],
        [0xffff, 0, 0, 0],
    ];
    for depth in [Depth::U8, Depth::U16, Depth::F32] {
        for seed in 1..=cases(40) as u64 {
            let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) + depth.bpp() as u64);
            let frames = random_animation(&mut rng, depth);
            let bg = backgrounds[(seed % 3) as usize];
            for rule in [BackgroundRule::Fill, BackgroundRule::FirstFrameAlpha] {
                let layer = LayerOptions::new()
                    .with_background_rgba16(bg)
                    .with_background_rule(rule);
                check_identity(&frames, depth, layer, seed);
            }
        }
    }
}

#[test]
fn overlay_only_lists_replay_exactly_or_are_refused() {
    let (mut written, mut refused) = (0, 0);
    for depth in [Depth::U8, Depth::U16, Depth::F32] {
        for seed in 1..=cases(120) as u64 {
            let mut rng = Rng(seed.wrapping_mul(0x94D0_49BB_1331_11EB) + depth.bpp() as u64);
            let frames = random_animation(&mut rng, depth);
            let want = layers::coalesce(&frames).unwrap();
            for transparency in [false, true] {
                let opts = OptimizeOptions::new()
                    .with_blend_source(false)
                    .with_transparency(transparency);
                // A transparent background can always be reached by
                // disposing to it.
                let small = layers::optimize_layers_with(&frames, &opts).unwrap();
                assert!(small.iter().all(|f| f.blend() == Blend::Over));
                let got = layers::coalesce(&small).unwrap();
                assert_same_pictures(&got, &want, depth, &format!("seed {seed} over-only"));
                written += 1;
                // Without that disposal, or on an opaque background,
                // some animations cannot be written; none is written
                // wrongly.
                let white = LayerOptions::new().with_background_rgba8([255; 4]);
                for opts in [
                    opts.with_dispose_background(false),
                    opts.with_layer_options(white),
                ] {
                    let want = layers::coalesce_with(&frames, &opts.layer).unwrap();
                    match layers::optimize_layers_with(&frames, &opts) {
                        Ok(small) => {
                            assert!(small.iter().all(|f| f.blend() == Blend::Over));
                            let got = layers::coalesce_with(&small, &opts.layer).unwrap();
                            assert_same_pictures(&got, &want, depth, &format!("seed {seed}"));
                            written += 1;
                        }
                        Err(Error::Unsupported(msg)) => {
                            assert!(msg.contains("lose opacity"), "{msg}");
                            refused += 1;
                        }
                        Err(e) => panic!("seed {seed}: {e}"),
                    }
                }
            }
        }
    }
    assert!(written > 0);
    if !cfg!(miri) {
        assert!(refused > 0, "the restricted cases were never refused");
    }
}

#[test]
fn legacy_optimisers_replay_exactly_on_one_bit_alpha() {
    // The half-alpha optimisers are exact when alpha is 0 or full.
    for seed in 1..=cases(150) as u64 {
        let mut rng = Rng(seed.wrapping_mul(0xBF58_476D_1CE4_E5B9));
        let mut frames = random_animation(&mut rng, Depth::U8);
        for f in &mut frames {
            let mut px = f.to_rgba8().unwrap();
            for p in px.as_chunks_mut::<4>().0 {
                p[3] = if p[3] >= 128 { 255 } else { 0 };
            }
            *f = f
                .with_pixels(f.width(), f.height(), PixelFormat::Rgba, px)
                .unwrap();
        }
        let want = layers::coalesce(&frames).unwrap();
        for small in [
            layers::optimize(&frames).unwrap(),
            layers::optimize_frame(&want).unwrap(),
            layers::optimize_plus(&want).unwrap(),
            layers::optimize_transparency(&frames).unwrap(),
        ] {
            let got = layers::remove_zero(&layers::coalesce(&small).unwrap());
            let timed = layers::remove_zero(&want);
            if got.len() == timed.len() {
                assert_same_pictures(&got, &timed, Depth::U8, &format!("seed {seed}"));
            }
        }
    }
}

#[test]
fn transparency_optimisation_plays_the_same_at_every_alpha() {
    // Under the default rule the transparency optimisation is exact
    // too, replacements included: converted to overlays where that
    // plays the same, left alone where it does not.
    let mut converted = 0;
    for depth in [Depth::U8, Depth::U16, Depth::F32] {
        for seed in 1..=cases(120) as u64 {
            let mut rng = Rng(seed.wrapping_mul(0xE703_7ED1_A0B4_28DB) + depth.bpp() as u64);
            let mut frames = random_animation(&mut rng, depth);
            if seed % 2 == 0 {
                // Authored overlays of semi-transparent pixels are the
                // documented exception; replacements have none.
                for f in &mut frames {
                    f.set_blend(Blend::Source);
                }
            }
            let small = layers::optimize_transparency(&frames).unwrap();
            converted += frames
                .iter()
                .zip(&small)
                .filter(|(f, s)| f.blend() != s.blend())
                .count();
            if seed % 2 == 0 {
                let what = format!("seed {seed} {depth:?} transparency");
                let (got, want) = (
                    layers::coalesce(&small).unwrap(),
                    layers::coalesce(&frames).unwrap(),
                );
                assert_same_pictures(&got, &want, depth, &what);
            }
        }
    }
    if !cfg!(miri) {
        assert!(converted > 0);
    }
    // A coalesced list — replacements — is optimised as its overlays
    // would be.
    let frames: Vec<Image> = (0..3u8)
        .map(|k| {
            let mut px = [[7u8, 7, 7, 255]; 6];
            px[usize::from(k)] = [200, 0, 0, 255];
            Image::from_rgba8(3, 2, px.concat()).unwrap()
        })
        .collect();
    let shown = layers::coalesce(&frames).unwrap();
    let small = layers::optimize_transparency(&shown).unwrap();
    assert!(small.iter().all(|f| f.blend() == Blend::Over));
    let holes = |f: &Image| {
        let px = f.to_rgba8().unwrap();
        px.as_chunks::<4>().0.iter().filter(|p| p[3] == 0).count()
    };
    assert_eq!(small.iter().map(holes).collect::<Vec<_>>(), [0, 4, 4]);
    assert_same_pictures(
        &layers::coalesce(&small).unwrap(),
        &shown,
        Depth::U8,
        "coalesced",
    );
}

#[test]
fn a_moving_sprite_costs_its_two_positions() {
    // Opaque 40×30 background, an opaque 4×4 sprite moving 5 pixels a
    // frame: each later frame is the box around both positions.
    let (w, h) = (40u32, 30u32);
    let frame = |k: u32| {
        let mut px = vec![[10u8, 20, 30, 255]; (w * h) as usize];
        for y in 8..12 {
            for x in 2 + 5 * k..6 + 5 * k {
                px[(y * w + x) as usize] = [200, 0, 0, 255];
            }
        }
        Image::from_rgba8(w, h, px.concat())
            .unwrap()
            .with_delay_ticks(5, 100)
    };
    let frames: Vec<Image> = (0..5).map(frame).collect();
    let small = layers::optimize_layers(&frames).unwrap();
    assert_eq!((small[0].width(), small[0].height()), (w, h));
    for (k, f) in small.iter().enumerate().skip(1) {
        assert_eq!((f.width(), f.height()), (9, 4), "frame {k}");
        assert_eq!(f.offset(), (2 + 5 * (k as i32 - 1), 8));
        assert_eq!(f.blend(), Blend::Over);
    }
    assert!(small[..4].iter().all(|f| f.dispose() == Dispose::None));
    // With the transparency optimisation the unchanged pixels inside a
    // window are transparent: here the window is two 4×4 squares and a
    // one-pixel gap, all of it changed but the gap.
    let clear =
        layers::optimize_layers_with(&frames, &OptimizeOptions::new().with_transparency(true))
            .unwrap();
    let px = clear[1].to_rgba8().unwrap();
    assert_eq!(
        px.as_chunks::<4>().0.iter().filter(|p| p[3] == 0).count(),
        4
    );
    assert_same_pictures(
        &layers::coalesce(&clear).unwrap(),
        &layers::coalesce(&frames).unwrap(),
        Depth::U8,
        "sprite",
    );
    // An unchanged frame is one pixel.
    let twice = vec![frames[0].clone(), frames[0].clone()];
    let small = layers::optimize_layers(&twice).unwrap();
    assert_eq!((small[1].width(), small[1].height()), (1, 1));
    assert_eq!(small[1].offset(), (0, 0));
}

#[test]
fn a_fading_pixel_needs_source_or_a_cleared_canvas() {
    // One pixel going from opaque to half transparent.
    let px = |a: u8| {
        Image::from_rgba8(1, 1, vec![9, 9, 9, a])
            .unwrap()
            .with_blend(Blend::Source)
    };
    let frames = vec![px(255), px(100)];
    let small = layers::optimize_layers(&frames).unwrap();
    assert_eq!(small[1].blend(), Blend::Source);
    assert_eq!(small[0].dispose(), Dispose::None);
    let over = OptimizeOptions::new().with_blend_source(false);
    let small = layers::optimize_layers_with(&frames, &over).unwrap();
    assert_eq!(small[0].dispose(), Dispose::Background);
    assert_eq!(small[1].blend(), Blend::Over);
    assert_eq!(
        layers::coalesce(&small).unwrap()[1].to_rgba8().unwrap(),
        [9, 9, 9, 100]
    );
    // Restoring the canvas under the first frame — transparent — works
    // too; with neither, nothing can erase.
    let small =
        layers::optimize_layers_with(&frames, &over.with_dispose_background(false)).unwrap();
    assert_eq!(small[0].dispose(), Dispose::Previous);
    let stuck = over
        .with_dispose_background(false)
        .with_dispose_previous(false);
    assert!(matches!(
        layers::optimize_layers_with(&frames, &stuck),
        Err(Error::Unsupported(_))
    ));
    // A semi-transparent pixel that stays is not drawn twice.
    let frames = vec![px(100), px(100)];
    let small = layers::optimize_layers_with(&frames, &over).unwrap();
    assert_eq!(
        layers::coalesce(&small).unwrap()[1].to_rgba8().unwrap(),
        [9, 9, 9, 100]
    );
}

#[test]
fn over_matches_its_definition_at_sixteen_bits() {
    // Exact rational arithmetic, rounded to nearest, against the
    // operator on 1×1 frames.
    const M: u128 = 65535;
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let edge = [0u16, 1, 2, 0x7fff, 0x8000, 0xfffe, 0xffff];
    for round in 0..cases(4000) {
        let mut px = [[0u16; 4]; 2];
        for p in &mut px {
            for c in p.iter_mut() {
                *c = if round % 3 == 0 {
                    rng.pick(&edge)
                } else {
                    rng.below(0x10000) as u16
                };
            }
        }
        let [s, d] = px;
        let (sa, da) = (u128::from(s[3]), u128::from(d[3]));
        let want = if sa == M || da == 0 {
            s
        } else if sa == 0 {
            d
        } else {
            let a = sa * M + da * (M - sa);
            let mut o = [0u16; 4];
            for c in 0..3 {
                let num = u128::from(s[c]) * sa * M + u128::from(d[c]) * da * (M - sa);
                o[c] = ((2 * num + a) / (2 * a)) as u16;
            }
            o[3] = ((2 * a + M) / (2 * M)) as u16;
            o
        };
        let image = |p: [u16; 4]| {
            let bytes: Vec<u8> = p.iter().flat_map(|c| c.to_le_bytes()).collect();
            Image::from_raw(1, 1, PixelFormat::Rgba64Le, bytes).unwrap()
        };
        let frames = [image(d).with_blend(Blend::Source), image(s)];
        let got = layers::flatten(&frames).unwrap().to_rgba16().unwrap();
        let got: Vec<u16> = got
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(got, want, "{s:?} over {d:?}");
    }
}

#[test]
fn eight_bit_over_is_the_sixteen_bit_result_rounded() {
    let mut rng = Rng(0x0BAD_5EED_0BAD_5EED);
    for _ in 0..cases(4000) {
        let s: [u8; 4] = std::array::from_fn(|_| rng.below(256) as u8);
        let d: [u8; 4] = std::array::from_fn(|_| rng.below(256) as u8);
        let wide = |p: [u8; 4]| {
            let bytes: Vec<u8> = p
                .iter()
                .flat_map(|&c| (u16::from(c) * 257).to_le_bytes())
                .collect();
            Image::from_raw(1, 1, PixelFormat::Rgba64Le, bytes).unwrap()
        };
        let narrow = |p: [u8; 4]| Image::from_rgba8(1, 1, p.to_vec()).unwrap();
        let deep = layers::flatten(&[wide(d).with_blend(Blend::Source), wide(s)]).unwrap();
        let want: Vec<u8> = deep
            .to_rgba16()
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| ((u32::from(u16::from_le_bytes(*c)) * 255 + 32767) / 65535) as u8)
            .collect();
        let got = layers::flatten(&[narrow(d).with_blend(Blend::Source), narrow(s)]).unwrap();
        assert_eq!(got.format(), PixelFormat::Rgba);
        assert_eq!(got.to_rgba8().unwrap(), want, "{s:?} over {d:?}");
    }
}

fn float_px(p: [f32; 4]) -> Image {
    let bytes: Vec<u8> = p.iter().flat_map(|c| c.to_le_bytes()).collect();
    Image::from_raw(1, 1, PixelFormat::RgbaF32Le, bytes).unwrap()
}

fn floats(img: &Image) -> Vec<f32> {
    img.to_packed(PixelFormat::RgbaF32Le)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[test]
fn float_over_keeps_scene_values_and_clamps_alpha_only() {
    let over = |s: [f32; 4], d: [f32; 4]| {
        floats(&layers::flatten(&[float_px(d).with_blend(Blend::Source), float_px(s)]).unwrap())
    };
    // Opaque source, transparent destination, transparent source.
    assert_eq!(over([4.0, -1.0, 0.5, 1.0], [1.0; 4]), [4.0, -1.0, 0.5, 1.0]);
    assert_eq!(
        over([4.0, 2.0, 0.5, 0.25], [9.0, 9.0, 9.0, 0.0]),
        [4.0, 2.0, 0.5, 0.25]
    );
    assert_eq!(
        over([4.0, 2.0, 0.5, 0.0], [1.0, 2.0, 3.0, 0.5]),
        [1.0, 2.0, 3.0, 0.5]
    );
    // The general case, in f64: a = 0.25 + 0.5·0.75 = 0.625.
    let got = over([8.0, 0.0, 1.0, 0.25], [0.0, 4.0, 1.0, 0.5]);
    let want = [
        (8.0f64 * 0.25 / 0.625) as f32,
        (4.0f64 * 0.5 * 0.75 / 0.625) as f32,
        1.0,
        0.625,
    ];
    assert_eq!(got, want);
    // Alpha above one is opaque, below zero or NaN transparent.
    assert_eq!(over([2.0, 2.0, 2.0, 1.5], [1.0; 4]), [2.0, 2.0, 2.0, 1.5]);
    assert_eq!(over([2.0, 2.0, 2.0, -0.5], [1.0; 4]), [1.0; 4]);
    assert_eq!(over([2.0, 2.0, 2.0, f32::NAN], [1.0; 4]), [1.0; 4]);
    // A float background the 16-bit field cannot hold.
    let opts = LayerOptions::new().with_background_rgba_f32([3.5, 0.0, 0.0, 1.0]);
    assert_eq!(opts.background_rgba_f32(), Some([3.5, 0.0, 0.0, 1.0]));
    let frame = float_px([0.0, 0.0, 0.0, 0.0]).with_page(Page::new(2, 1));
    let flat = layers::flatten_with(&[frame], &opts).unwrap();
    assert_eq!(flat.format(), PixelFormat::RgbaF32Le);
    assert_eq!(floats(&flat), [3.5, 0.0, 0.0, 1.0, 3.5, 0.0, 0.0, 1.0]);
}

#[test]
fn depth_follows_the_deepest_frame() {
    let shallow = Image::from_rgba8(1, 1, vec![255, 0, 0, 255]).unwrap();
    let deep = Image::from_raw(
        1,
        1,
        PixelFormat::Rgba64Le,
        vec![0, 0, 0xff, 0x7f, 0, 0, 0xff, 0xff],
    )
    .unwrap();
    let float = float_px([0.0, 0.0, 2.0, 1.0]);
    let fmt = |frames: &[Image]| layers::coalesce(frames).unwrap()[0].format();
    assert_eq!(fmt(std::slice::from_ref(&shallow)), PixelFormat::Rgba);
    assert_eq!(fmt(&[shallow.clone(), deep.clone()]), PixelFormat::Rgba64Le);
    assert_eq!(
        fmt(&[shallow.clone(), deep, float.clone()]),
        PixelFormat::RgbaF32Le
    );
    // An 8-bit frame in a float list is its value over 255.
    let both = layers::coalesce(&[shallow, float]).unwrap();
    assert_eq!(floats(&both[0]), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(floats(&both[1]), [0.0, 0.0, 2.0, 1.0]);
}

#[test]
fn thread_budgets_give_the_same_frames() {
    // Large enough for the comparisons to be split into row bands.
    let (w, h) = if cfg!(miri) {
        (64u32, 40u32)
    } else {
        (512u32, 400u32)
    };
    let mut rng = Rng(77);
    let frame = |rng: &mut Rng, k: u32| {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let moving = (x + 7 * k) % 64 < 20 && (y + 3 * k) % 50 < 20;
                let a = if (x / 16 + y / 16) % 5 == 0 { 120 } else { 255 };
                px.extend_from_slice(&[if moving { 200 } else { 30 }, (y & 255) as u8, 7, a]);
            }
        }
        if k % 2 == 1 {
            let at = rng.below(u64::from(w * h)) as usize * 4;
            px[at] ^= 0x55;
        }
        Image::from_rgba8(w, h, px).unwrap()
    };
    let frames: Vec<Image> = (0..4).map(|k| frame(&mut rng, k)).collect();
    let run = |threads: usize| {
        let opts = OptimizeOptions::new()
            .with_threads(threads)
            .with_transparency(true);
        let exact = layers::optimize_layers_with(&frames, &opts).unwrap();
        let legacy = layers::optimize_with(&frames, &opts.layer).unwrap();
        (exact, legacy)
    };
    let (exact1, legacy1) = run(1);
    for threads in [2, 3, 8] {
        let (exact, legacy) = run(threads);
        for (a, b) in exact.iter().zip(&exact1).chain(legacy.iter().zip(&legacy1)) {
            assert_eq!(
                (a.page(), a.dispose(), a.blend()),
                (b.page(), b.dispose(), b.blend())
            );
            assert_eq!(
                a.to_rgba8().unwrap(),
                b.to_rgba8().unwrap(),
                "{threads} threads"
            );
        }
    }
    assert_same_pictures(
        &layers::coalesce(&exact1).unwrap(),
        &layers::coalesce(&frames).unwrap(),
        Depth::U8,
        "bands",
    );
    let ctx = oxideav_core::ExecutionContext::with_threads(6);
    assert_eq!(
        LayerOptions::new().with_execution_context(&ctx).threads(),
        6
    );
    assert_eq!(LayerOptions::new().threads(), 1);
}

#[test]
fn a_change_of_canvas_restarts_the_optimiser() {
    let mut rng = Rng(5);
    let mut frames: Vec<Image> = (0..3)
        .map(|_| random_frame(&mut rng, Depth::U8, (6, 5)))
        .collect();
    frames.extend((0..3).map(|_| random_frame(&mut rng, Depth::U8, (4, 9))));
    check_identity(&frames, Depth::U8, LayerOptions::new(), 5);
    let small = layers::optimize_layers(&frames).unwrap();
    assert_eq!(small[2].canvas_size(), (6, 5));
    assert_eq!(small[3].canvas_size(), (4, 9));
}

fn solid(w: u32, h: u32, c: [u8; 4]) -> Image {
    Image::from_rgba8(w, h, c.repeat((w * h) as usize)).unwrap()
}

#[test]
fn append_joins_pictures_along_an_axis() {
    let (r, g) = ([255, 0, 0, 255], [0, 255, 0, 128]);
    let frames = [
        solid(2, 3, r).with_page(Page::new(9, 9).with_offset(4, 4)),
        solid(3, 1, g).with_delay_ticks(3, 100),
    ];
    let row = layers::append(&frames, Direction::Horizontal).unwrap();
    assert_eq!((row.width(), row.height()), (5, 3));
    assert_eq!(row.page(), Page::default());
    let px = row.to_rgba8().unwrap();
    let at = |x: usize, y: usize| px[(y * 5 + x) * 4..][..4].to_vec();
    assert_eq!(at(1, 2), r);
    // Copied, not blended: the half-transparent green keeps its alpha.
    assert_eq!(at(2, 0), g);
    assert_eq!(at(4, 1), [0, 0, 0, 0]);
    let column = layers::append_with(
        &frames,
        Direction::Vertical,
        &LayerOptions::new().with_background_rgba8([1, 2, 3, 255]),
    )
    .unwrap();
    assert_eq!((column.width(), column.height()), (3, 4));
    let px = column.to_rgba8().unwrap();
    assert_eq!(px[2 * 4..3 * 4], [1, 2, 3, 255]);
    assert_eq!(px[(3 * 3) * 4..][..4], g);
    assert!(layers::append(&[], Direction::Horizontal).is_err());
}

#[test]
fn pages_are_reset_set_and_moved() {
    let frames = [
        solid(2, 2, [1, 1, 1, 255]).with_page(Page::new(8, 6).with_offset(3, -1)),
        solid(4, 1, [2, 2, 2, 255]),
    ];
    let reset = layers::reset_page(&frames);
    assert!(reset.iter().all(|f| f.page() == Page::default()));
    assert_eq!(reset[0].canvas_size(), (2, 2));
    let paged = layers::repage(&frames, Page::new(0, 20).with_offset(1, 2));
    assert_eq!(paged[0].page(), Page::new(8, 20).with_offset(1, 2));
    assert_eq!(paged[1].page(), Page::new(0, 20).with_offset(1, 2));
    let canvas = layers::set_canvas(&frames, 10, 10);
    assert_eq!(canvas[0].page(), Page::new(10, 10).with_offset(3, -1));
    assert_eq!(canvas[1].page(), Page::new(10, 10));
    let moved = layers::translate(&frames, -5, 2).unwrap();
    assert_eq!(moved[0].page(), Page::new(8, 6).with_offset(-2, 1));
    assert_eq!(moved[1].page(), Page::new(4, 1).with_offset(-5, 2));
    assert!(layers::translate(&frames, i32::MAX, 0).is_err());
}

#[test]
fn trim_cuts_a_layer_to_its_content_in_place() {
    // 6×5, content in columns 2..=3, rows 1..=3.
    let mut px = [[0u8, 0, 0, 0]; 30];
    for (x, y) in [(2, 1), (3, 3)] {
        px[y * 6 + x] = [5, 6, 7, 255];
    }
    let layer = Image::from_rgba8(6, 5, px.concat())
        .unwrap()
        .with_offset(10, 20)
        .with_blend(Blend::Source)
        .with_dispose(Dispose::Background)
        .with_delay_ticks(7, 100);
    let cut = layers::trim(&layer).unwrap();
    assert_eq!((cut.width(), cut.height()), (2, 3));
    assert_eq!(cut.page(), Page::new(6, 5).with_offset(12, 21));
    assert_eq!(
        (cut.blend(), cut.dispose(), cut.delay_ticks()),
        (Blend::Source, Dispose::Background, 7)
    );
    assert_eq!(cut.to_rgba8().unwrap()[..4], [5, 6, 7, 255]);
    // Already tight: unchanged. Empty: one pixel.
    assert_eq!(layers::trim(&cut).unwrap().page(), cut.page());
    let empty = layers::trim(&solid(4, 4, [9, 9, 9, 0]).with_offset(2, 2)).unwrap();
    assert_eq!(
        (empty.width(), empty.height(), empty.offset()),
        (1, 1, (2, 2))
    );
    // Against a border colour; a palette picture keeps its layout.
    let mut px = [[255u8, 255, 255, 255]; 12];
    px[5] = [0, 0, 0, 255];
    let framed = Image::from_rgba8(4, 3, px.concat()).unwrap();
    let cut = layers::trim_to(&framed, [0xffff; 4]).unwrap();
    assert_eq!((cut.width(), cut.height(), cut.offset()), (1, 1, (1, 1)));
    let indexed =
        Image::from_indexed(3, 1, vec![0, 1, 0], &[[0, 0, 0, 0], [8, 8, 8, 255]]).unwrap();
    let cut = layers::trim(&indexed).unwrap();
    assert_eq!(
        (cut.format(), cut.width(), cut.offset()),
        (PixelFormat::Pal8, 1, (1, 0))
    );
}

#[test]
fn delays_loops_selection_and_order() {
    let frames: Vec<Image> = (0..4u8)
        .map(|k| solid(1, 1, [k, 0, 0, 255]).with_delay_ticks(u64::from(k) * 3, 100))
        .collect();
    assert_eq!(layers::total_delay(&frames), Duration::from_millis(180));
    let fast = layers::scale_delays(&frames, 1, 3).unwrap();
    assert_eq!(
        fast.iter()
            .map(|f| f.delay().unwrap().as_millis())
            .collect::<Vec<_>>(),
        [0, 10, 20, 30]
    );
    assert!(layers::scale_delays(&frames, 1, 0).is_err());
    let even = layers::set_delay(&frames, Some(Duration::from_millis(40)));
    assert_eq!(layers::total_delay(&even), Duration::from_millis(160));
    assert!(
        layers::set_delay(&frames, None)
            .iter()
            .all(|f| f.delay().is_none())
    );
    let ticks = layers::set_delay_ticks(&frames, 5, 1000);
    assert_eq!(
        (ticks[0].delay_ticks(), ticks[0].ticks_per_second()),
        (5, 1000)
    );
    let looped = layers::set_loop_count(&frames, Some(3));
    assert!(looped.iter().all(|f| f.loop_count() == Some(3)));
    let first = |list: &[Image]| {
        list.iter()
            .map(|f| f.to_rgba8().unwrap()[0])
            .collect::<Vec<_>>()
    };
    assert_eq!(
        first(&layers::select(&frames, &[3, 0, 0, 2]).unwrap()),
        [3, 0, 0, 2]
    );
    assert!(layers::select(&frames, &[4]).is_err());
    assert_eq!(first(&layers::reverse(&frames)), [3, 2, 1, 0]);
    // Duplicates fold with their delays added.
    let dups = layers::select(&frames, &[1, 1, 2, 2, 2, 3]).unwrap();
    let folded = layers::remove_dups(&dups).unwrap();
    assert_eq!(first(&folded), [1, 2, 3]);
    assert_eq!(
        folded.iter().map(Image::delay_ticks).collect::<Vec<_>>(),
        [6, 18, 9]
    );
}

#[test]
fn a_sequence_carries_canvas_background_and_loops() {
    let frames = vec![
        solid(4, 3, [255, 0, 0, 255])
            .with_delay_ticks(10, 100)
            .with_loop_count(Some(2))
            .with_container_metadata(vec![("background_color".into(), "#00ff0080".into())]),
        solid(2, 2, [0, 0, 255, 255])
            .with_offset(3, 2)
            .with_dispose(Dispose::Background)
            .with_delay_ticks(20, 100),
    ];
    let seq = LayerSequence::new(frames).unwrap();
    assert_eq!(
        (seq.len(), seq.canvas_size(), seq.loop_count()),
        (2, (4, 3), Some(2))
    );
    assert_eq!(seq.background(), [0, 0xffff, 0, 0x8080]);
    assert_eq!(seq.frames()[1].page(), Page::new(4, 3).with_offset(3, 2));
    assert_eq!(seq.total_delay(), Duration::from_millis(300));
    let shown = seq.coalesce().unwrap();
    assert!(shown.frames().iter().all(Image::is_full_canvas));
    // The blue square is clipped to the canvas: one pixel of it.
    assert_eq!(
        shown.frames()[1].to_rgba8().unwrap()[(2 * 4 + 3) * 4..][..4],
        [0, 0, 255, 255]
    );
    let small = shown.optimize(&OptimizeOptions::new()).unwrap();
    assert_eq!(
        (small.frames()[1].width(), small.frames()[1].offset()),
        (1, (3, 2))
    );
    assert_same_pictures(
        small.coalesce().unwrap().frames(),
        shown.frames(),
        Depth::U8,
        "sequence",
    );
    // Flatten paints on the sequence's background.
    let seq = seq
        .with_background_rgba8([0, 0, 0, 255])
        .with_canvas(5, 4)
        .with_loop_count(Some(0));
    assert_eq!((seq.canvas_size(), seq.loop_count()), ((5, 4), Some(0)));
    let flat = seq.flatten().unwrap();
    assert_eq!((flat.width(), flat.height()), (5, 4));
    assert_eq!(
        flat.to_rgba8().unwrap()[(3 * 5 + 4) * 4..][..4],
        [0, 0, 255, 255]
    );
    assert_eq!(flat.to_rgba8().unwrap()[(3 * 5) * 4..][..4], [0, 0, 0, 255]);
    assert_eq!(seq.reverse().frames()[0].offset(), (3, 2));
    assert_eq!(seq.select(&[1]).unwrap().len(), 1);
    assert!(seq.select(&[]).is_err());
    assert_eq!(
        seq.scale_delays(2, 1).unwrap().total_delay(),
        Duration::from_millis(600)
    );
    assert!(LayerSequence::new(Vec::new()).is_err());
}

#[test]
fn background_colour_values_parse_and_print() {
    use crate::{format_background, parse_background};
    assert_eq!(parse_background("#ff000080"), Some([0xffff, 0, 0, 0x8080]));
    assert_eq!(parse_background("00FF00"), Some([0, 0xffff, 0, 0xffff]));
    assert_eq!(parse_background(" #0001000200030004 "), Some([1, 2, 3, 4]));
    for bad in ["", "#12345", "#gg0000", "red", "#+1+2+3+4"] {
        assert_eq!(parse_background(bad), None, "{bad}");
    }
    for c in [[0u16; 4], [0xffff, 0x0101, 0, 0x8080], [1, 2, 3, 4]] {
        assert_eq!(parse_background(&format_background(c)), Some(c));
    }
    assert_eq!(format_background([0xffff, 0, 0, 0x8080]), "#ff000080");
    assert_eq!(format_background([1, 0, 0, 0]), "#0001000000000000");
}

#[test]
fn offsets_at_the_ends_of_the_range_never_panic() {
    // Frames placed anywhere an `i32` offset can put them: every
    // operation either answers or refuses with an error.
    let ends = [i32::MIN, i32::MIN + 1, -1, 0, 1, 3, i32::MAX - 1, i32::MAX];
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    for _ in 0..cases(60) {
        let frames: Vec<Image> = (0..rng.range(1, 4))
            .map(|_| {
                random_frame(&mut rng, Depth::U8, (5, 4))
                    .with_page(Page::new(5, 4).with_offset(rng.pick(&ends), rng.pick(&ends)))
            })
            .collect();
        let shown = layers::coalesce(&frames).unwrap();
        assert!(shown.iter().all(Image::is_full_canvas));
        let small = layers::optimize_layers(&frames).unwrap();
        assert_same_pictures(
            &layers::coalesce(&small).unwrap(),
            &shown,
            Depth::U8,
            "ends",
        );
        layers::optimize(&frames).unwrap();
        layers::optimize_transparency(&frames).unwrap();
        layers::apply_disposal(&frames).unwrap();
        layers::compare_any(&frames).unwrap();
        layers::compare_clear(&frames).unwrap();
        layers::compare_overlay(&frames).unwrap();
        layers::flatten(&frames).unwrap();
        layers::remove_dups(&frames).unwrap();
        let inside = layers::clip_to_canvas(&frames).unwrap();
        assert_same_pictures(
            &layers::coalesce(&inside).unwrap(),
            &shown,
            Depth::U8,
            "clipped",
        );
        for f in &inside {
            let (x, y) = f.offset();
            assert!(x >= 0 && y >= 0 && x as u32 + f.width() <= 5 && y as u32 + f.height() <= 4);
        }
        layers::composite_layers(&frames, &frames, i32::MAX, i32::MIN).unwrap();
        assert_eq!(layers::trim_bounds(&frames).len(), frames.len());
        // A canvas that would have to span the offsets is refused, not
        // allocated.
        for big in [layers::mosaic(&frames), layers::merge(&frames)] {
            if let Err(e) = big {
                assert!(matches!(e, Error::LimitExceeded(_)), "{e}");
            }
        }
        let _ = layers::translate(&frames, i32::MAX, i32::MIN);
    }
}
