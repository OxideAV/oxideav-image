//! The layer model and [`crate::layers`]: every expected geometry,
//! disposal and pixel below was recorded from `magick` 7.1.2-29
//! Q16-HDRI run as a black box on the same frames (`-coalesce`,
//! `-layers <method>`, `-format '%g %w %h %D %T'`), except where a test
//! says the gateway departs from it.

use std::time::Duration;

use crate::layers::{self, LayerMethod, LayerOptions};
use crate::{Blend, Dispose, Image, Page};

type C = [u8; 4];
const R: C = [255, 0, 0, 255];
const G: C = [0, 255, 0, 255];
const B: C = [0, 0, 255, 255];
const Y: C = [255, 255, 0, 255];
const W: C = [255, 255, 255, 255];
const K: C = [0, 0, 0, 255];
const T: C = [0, 0, 0, 0];
const HALF_GREEN: C = [0, 255, 0, 128];

/// A `w × h` picture of `bg` with `rects` (`x, y, w, h, colour`) painted
/// on it, clipped.
fn paint(w: u32, h: u32, bg: C, rects: &[(i32, i32, u32, u32, C)]) -> Image {
    let mut px = vec![bg; (w * h) as usize];
    for &(x, y, rw, rh, c) in rects {
        for yy in y..y + rh as i32 {
            for xx in x..x + rw as i32 {
                if xx >= 0 && yy >= 0 && (xx as u32) < w && (yy as u32) < h {
                    px[(yy as u32 * w + xx as u32) as usize] = c;
                }
            }
        }
    }
    Image::from_rgba8(w, h, px.concat()).unwrap()
}

/// A full-canvas frame with dispose None (ImageMagick's view of a
/// frame list read from raw pixels) and `cs` centiseconds of delay.
fn full(w: u32, h: u32, bg: C, rects: &[(i32, i32, u32, u32, C)], cs: u64) -> Image {
    paint(w, h, bg, rects)
        .with_page(Page::new(w, h))
        .with_dispose(Dispose::None)
        .with_delay_ticks(cs, 100)
}

/// A solid `w × h` layer at `(x, y)` on a `cw × ch` canvas.
#[allow(clippy::too_many_arguments)]
fn layer(w: u32, h: u32, c: C, x: i32, y: i32, cw: u32, ch: u32, d: Dispose) -> Image {
    paint(w, h, c, &[])
        .with_page(Page::new(cw, ch).with_offset(x, y))
        .with_dispose(d)
        .with_delay_ticks(10, 100)
}

fn geometry(frames: &[Image]) -> Vec<String> {
    frames
        .iter()
        .map(|f| {
            let (cw, ch) = f.canvas_size();
            let (x, y) = f.offset();
            format!(
                "{cw}x{ch}{x:+}{y:+} {} {} {}",
                f.width(),
                f.height(),
                f.dispose().name()
            )
        })
        .collect()
}

fn expect(frames: &[Image], want: &str) {
    let want: Vec<&str> = want
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(geometry(frames), want);
}

fn px(img: &Image, x: u32, y: u32) -> C {
    let b = img.to_rgba8().unwrap();
    let o = ((y * img.width() + x) * 4) as usize;
    [b[o], b[o + 1], b[o + 2], b[o + 3]]
}

fn delays_cs(frames: &[Image]) -> Vec<u64> {
    frames.iter().map(|f| f.delay_ticks()).collect()
}

/// Background red canvas, a blue layer disposed to background, a
/// half-green layer restored to previous, a yellow layer.
fn c1() -> Vec<Image> {
    vec![
        layer(40, 30, R, 0, 0, 40, 30, Dispose::None),
        layer(10, 8, B, 5, 4, 40, 30, Dispose::Background).with_delay_ticks(20, 100),
        layer(12, 6, HALF_GREEN, 20, 10, 40, 30, Dispose::Previous).with_delay_ticks(30, 100),
        layer(6, 6, Y, 2, 20, 40, 30, Dispose::None).with_delay_ticks(40, 100),
    ]
}

/// Opaque: a red square moving on white, a small square blinking blue
/// / green.
fn c2() -> Vec<Image> {
    (0..5)
        .map(|k| {
            let blink = if k % 2 == 1 { B } else { G };
            full(
                32,
                24,
                W,
                &[(2 + 5 * k, 3 + 2 * k, 6, 5, R), (20, 2, 3, 3, blink)],
                10,
            )
        })
        .collect()
}

/// A red square moving on transparency.
fn c3() -> Vec<Image> {
    (0..5)
        .map(|k| full(32, 24, T, &[(2 + 5 * k, 3 + 2 * k, 6, 5, R)], 10))
        .collect()
}

/// A static green bar, a blue sprite walking right, a yellow overlay
/// shown on frames 1 and 3, on transparency.
fn c4() -> Vec<Image> {
    (0..6)
        .map(|k| {
            let mut r = vec![(0, 20, 40, 10, G), (3 + 4 * k, 8, 5, 5, B)];
            if k == 1 || k == 3 {
                r.push((25, 2, 8, 4, Y));
            }
            full(40, 30, T, &r, 8)
        })
        .collect()
}

/// Runs of identical frames and zero delays.
fn c6() -> Vec<Image> {
    [(R, 10), (R, 20), (G, 0), (G, 10), (B, 0), (B, 0), (R, 30)]
        .iter()
        .map(|&(c, d)| full(8, 6, W, &[(1, 1, 4, 3, c)], d))
        .collect()
}

/// Semi-transparent content on a semi-transparent background.
fn c7() -> Vec<Image> {
    (0..4)
        .map(|k| {
            let a = if k % 2 == 1 { 255 } else { 100 };
            full(
                20,
                16,
                [0, 0, 255, 64],
                &[
                    (1 + 3 * k, 2, 5, 5, [255, 0, 0, 200]),
                    (10, 10, 4, 4, [0, 255, 0, a]),
                ],
                10,
            )
        })
        .collect()
}

/// Layers with negative offsets and layers leaving the canvas.
fn c8() -> Vec<Image> {
    vec![
        layer(10, 10, R, 3, 2, 20, 14, Dispose::None),
        layer(8, 4, B, -2, 6, 20, 14, Dispose::Previous),
        layer(5, 5, HALF_GREEN, 12, 9, 20, 14, Dispose::Background),
        layer(6, 3, Y, 15, 1, 20, 14, Dispose::Unspecified),
    ]
}

/// A black bar, a red square stepping right, a blue block shown every
/// third frame, on transparency.
fn c9() -> Vec<Image> {
    (0..6)
        .map(|k| {
            let mut r = vec![(0, 0, 30, 4, K), (4 * k, 6, 4, 4, R)];
            if k % 3 == 0 {
                r.push((10, 12, 10, 6, B));
            }
            full(30, 20, T, &r, 10)
        })
        .collect()
}

// ---- the record --------------------------------------------------------

#[test]
fn layer_record_defaults_and_builders() {
    let img = paint(4, 3, R, &[]);
    assert_eq!(img.page(), Page::default());
    assert_eq!(img.canvas_size(), (4, 3));
    assert!(img.is_full_canvas());
    assert_eq!(img.dispose(), Dispose::Unspecified);
    assert_eq!(img.blend(), Blend::Over);
    assert_eq!(img.ticks_per_second(), 100);
    assert_eq!(img.delay_ticks(), 0);

    let img = img
        .with_page(Page::new(40, 30).with_offset(-2, 5))
        .with_dispose(Dispose::Previous)
        .with_blend(Blend::Source)
        .with_delay_ticks(7, 1000);
    assert_eq!(img.page().to_string(), "40x30-2+5");
    assert_eq!(img.offset(), (-2, 5));
    assert_eq!(img.canvas_size(), (40, 30));
    assert!(!img.is_full_canvas());
    assert_eq!(img.delay(), Some(Duration::from_millis(7)));
    assert_eq!((img.ticks_per_second(), img.delay_ticks()), (1000, 7));
    // A converted or cropped picture keeps the record.
    let conv = img.to_format(oxideav_core::PixelFormat::Rgb24).unwrap();
    assert_eq!(
        (conv.page(), conv.dispose(), conv.blend()),
        (img.page(), img.dispose(), img.blend())
    );
    assert_eq!(conv.ticks_per_second(), 1000);
    let crop = img.crop(1, 1, 2, 2).unwrap();
    assert_eq!(crop.dispose(), Dispose::Previous);
}

#[test]
fn dispose_codes_follow_gif89a() {
    for (code, d) in [
        (0, Dispose::Unspecified),
        (1, Dispose::None),
        (2, Dispose::Background),
        (3, Dispose::Previous),
    ] {
        assert_eq!(Dispose::from_gif_code(code), d);
        assert_eq!(d.gif_code(), code);
    }
    // §23.c.iv values 4–7 are "to be defined".
    assert_eq!(Dispose::from_gif_code(5), Dispose::Unspecified);
    assert_eq!(Dispose::Background.name(), "Background");
}

// ---- coalesce ----------------------------------------------------------

#[test]
fn coalesce_renders_layers_with_their_disposal() {
    let out = layers::coalesce(&c1()).unwrap();
    expect(
        &out,
        "40x30+0+0 40 30 None; 40x30+0+0 40 30 Background; 40x30+0+0 40 30 None; 40x30+0+0 40 30 None",
    );
    assert_eq!(delays_cs(&out), [10, 20, 30, 40]);
    // Frame 1's rectangle is cleared to transparency for frame 2; the
    // half-green layer is composited over red (magick: 127,128,0,255).
    assert_eq!(px(&out[1], 6, 5), B);
    assert_eq!(px(&out[2], 6, 5)[3], 0);
    assert_eq!(px(&out[2], 21, 11), [127, 128, 0, 255]);
    // Restored to previous: the half-green layer is gone again.
    assert_eq!(px(&out[3], 21, 11), R);
    assert_eq!(px(&out[3], 3, 21), Y);
    assert!(
        out.iter()
            .all(|f| f.format() == oxideav_core::PixelFormat::Rgba)
    );
}

#[test]
fn coalesce_clips_layers_and_previous_restores() {
    let out = layers::coalesce(&c8()).unwrap();
    expect(
        &out,
        "20x14+0+0 20 14 None; 20x14+0+0 20 14 Background; 20x14+0+0 20 14 Background; 20x14+0+0 20 14 Background",
    );
    let at = |i: usize| [(4, 3), (1, 7), (13, 10), (16, 2)].map(|(x, y)| px(&out[i], x, y));
    assert_eq!(at(0), [R, T, T, T]);
    assert_eq!(at(1), [R, B, T, T]);
    assert_eq!(at(2), [R, T, HALF_GREEN, T]);
    assert_eq!(at(3).map(|p| p[3]), [255, 0, 0, 255]);
    assert_eq!(at(3)[3], Y);
}

#[test]
fn coalesce_output_disposal_matrix() {
    // Red full canvas, a blue layer, a yellow layer: the 27 disposal
    // combinations as magick reports them (frames with an alpha channel,
    // `-background none`, so the initial canvas a first-frame Previous
    // restores is transparent). The last frame repeats the
    // previous frame's choice; a frame is Background exactly when the
    // next one clears one of its visible pixels.
    let ds = [Dispose::None, Dispose::Background, Dispose::Previous];
    let n = "None";
    let b = "Background";
    let table: [[&str; 3]; 9] = [
        [n, n, n],
        [n, b, b],
        [n, n, n],
        [b, n, n],
        [b, b, b],
        [b, b, b],
        [b, n, n],
        [b, b, b],
        [b, b, b],
    ];
    for (i, a) in ds.iter().enumerate() {
        for (j, bd) in ds.iter().enumerate() {
            for c in ds {
                let frames = [
                    layer(40, 30, R, 0, 0, 40, 30, *a),
                    layer(10, 8, B, 5, 4, 40, 30, *bd),
                    layer(6, 6, Y, 2, 20, 40, 30, c),
                ];
                let got: Vec<&str> = layers::coalesce(&frames)
                    .unwrap()
                    .iter()
                    .map(|f| f.dispose().name())
                    .collect();
                assert_eq!(got, table[i * 3 + j], "{a:?} {bd:?} {c:?}");
            }
        }
    }
}

#[test]
fn coalesce_counts_half_alpha_as_the_visibility_line() {
    // Frame 0 restored to previous (the empty canvas), then a frame
    // whose one pixel drops from alpha `a` to `b`: magick marks the
    // earlier frame Background only when the pixel crosses one half.
    for (a, b, want) in [
        (255, 200, Dispose::None),
        (255, 128, Dispose::None),
        (255, 127, Dispose::Background),
        (200, 100, Dispose::Background),
        (100, 0, Dispose::None),
    ] {
        let base = paint(4, 4, [255, 0, 0, a], &[]);
        let dropped = paint(4, 4, [255, 0, 0, a], &[(1, 1, 1, 1, [255, 0, 0, b])]);
        let frames = [
            base.clone().with_dispose(Dispose::Previous),
            base.with_dispose(Dispose::Previous),
            dropped.with_dispose(Dispose::None),
        ];
        let out = layers::coalesce(&frames).unwrap();
        assert_eq!(out[1].dispose(), want, "{a} -> {b}");
    }
}

#[test]
fn coalesce_of_decoded_style_frames_is_the_identity() {
    // Full-canvas frames that replace the canvas (what `open` hands
    // out) come back pixel for pixel.
    let frames: Vec<Image> = c3()
        .into_iter()
        .map(|f| f.with_blend(Blend::Source))
        .collect();
    let out = layers::coalesce(&frames).unwrap();
    for (a, b) in frames.iter().zip(&out) {
        assert_eq!(a.to_rgba8().unwrap(), b.to_rgba8().unwrap());
    }
}

#[test]
fn background_option_fills_the_initial_canvas_and_disposal() {
    let opts = LayerOptions::new().with_background_rgba8([10, 20, 30, 255]);
    let frames = [
        layer(4, 4, B, 2, 2, 8, 8, Dispose::Background),
        layer(1, 1, Y, 0, 0, 8, 8, Dispose::None),
    ];
    let out = layers::coalesce_with(&frames, &opts).unwrap();
    assert_eq!(px(&out[0], 0, 0), [10, 20, 30, 255]);
    assert_eq!(px(&out[1], 3, 3), [10, 20, 30, 255]);
    assert_eq!(px(&out[1], 0, 0), Y);
}

#[test]
fn apply_disposal_shows_the_canvas_each_frame_leaves() {
    let out = layers::apply_disposal(&c1()).unwrap();
    assert_eq!(out.len(), 4);
    assert!(
        out.iter()
            .all(|f| f.dispose() == Dispose::None && f.is_full_canvas())
    );
    assert_eq!(px(&out[0], 6, 5), R);
    assert_eq!(px(&out[1], 6, 5)[3], 0, "blue cleared");
    assert_eq!(px(&out[2], 21, 11), R, "half green restored away");
    assert_eq!(px(&out[3], 3, 21), Y);
}

// ---- compare -----------------------------------------------------------

#[test]
fn compare_methods_match_magick() {
    type Case = (fn() -> Vec<Image>, [&'static str; 3]);
    let cases: [Case; 4] = [
        (
            c3,
            [
                "32x24+0+0 32 24;32x24+2+3 11 7;32x24+7+5 11 7;32x24+12+7 11 7;32x24+17+9 11 7",
                "32x24+0+0 32 24;32x24+2+3 6 5;32x24+7+5 6 5;32x24+12+7 6 5;32x24+17+9 6 5",
                "32x24+0+0 32 24;32x24+7+5 6 5;32x24+12+7 6 5;32x24+17+9 6 5;32x24+22+11 6 5",
            ],
        ),
        (
            c4,
            [
                "40x30+0+0 40 30;40x30+3+2 30 11;40x30+7+2 26 11;40x30+11+2 22 11;40x30+15+2 18 11;40x30+19+8 9 5",
                "40x30+0+0 40 30;40x30+3+8 4 5;40x30+7+2 26 11;40x30+11+8 4 5;40x30+15+2 18 11;40x30+19+8 4 5",
                "40x30+0+0 40 30;40x30+8+2 25 11;40x30+12+8 4 5;40x30+16+2 17 11;40x30+20+8 4 5;40x30+24+8 4 5",
            ],
        ),
        (
            c7,
            [
                "20x16+0+0 20 16;20x16+1+2 13 12;20x16+4+2 10 12;20x16+7+2 8 12",
                "20x16+0+0 20 16;20x16+1+2 3 5;20x16+4+2 10 12;20x16+7+2 3 5",
                "20x16+0+0 20 16;20x16+6+2 8 12;20x16+9+2 3 5;20x16+10+2 5 12",
            ],
        ),
        (
            c9,
            [
                "30x20+0+0 30 20;30x20+0+6 20 12;30x20+4+6 8 4;30x20+8+6 12 12;30x20+10+6 10 12;30x20+16+6 8 4",
                "30x20+0+0 30 20;30x20+0+6 20 12;30x20+4+6 4 4;30x20+8+6 4 4;30x20+10+6 10 12;30x20+16+6 4 4",
                "30x20+0+0 30 20;30x20+4+6 4 4;30x20+8+6 4 4;30x20+10+6 10 12;30x20+16+6 4 4;30x20+20+6 4 4",
            ],
        ),
    ];
    let short = |frames: &[Image]| -> String {
        geometry(frames)
            .iter()
            .map(|g| g.rsplit_once(' ').unwrap().0.to_string())
            .collect::<Vec<_>>()
            .join(";")
    };
    for (make, [any, clear, overlay]) in cases {
        let frames = make();
        assert_eq!(short(&layers::compare_any(&frames).unwrap()), any);
        assert_eq!(short(&layers::compare_clear(&frames).unwrap()), clear);
        assert_eq!(short(&layers::compare_overlay(&frames).unwrap()), overlay);
    }
}

#[test]
fn compare_works_on_layers_and_drops_unchanged_frames() {
    // Layered input: each layer against the one before it on the canvas.
    expect(
        &layers::compare_any(&c8()).unwrap(),
        "20x14+3+2 10 10 None; 20x14+0+6 6 4 Previous; 20x14+12+9 5 5 Background; 20x14+15+1 5 3 Undefined",
    );
    // Nothing turns transparent between overlaid layers.
    assert_eq!(layers::compare_clear(&c1()).unwrap().len(), 1);
    // Identical neighbours have no difference and are dropped.
    let any = layers::deconstruct(&c6()).unwrap();
    assert_eq!(delays_cs(&any), [10, 0, 0, 30]);
    // The cropped window keeps the frame's own pixels.
    let any = layers::compare_any(&c1()).unwrap();
    assert_eq!(px(&any[2], 0, 0), HALF_GREEN);
    // Colour under alpha 0 is not a difference.
    let frames = [
        paint(8, 6, T, &[]),
        paint(8, 6, T, &[(2, 2, 2, 2, [255, 255, 255, 0])]),
        paint(8, 6, T, &[(5, 1, 1, 1, R)]),
    ];
    expect(
        &layers::compare_any(&frames).unwrap(),
        "8x6+0+0 8 6 Undefined; 8x6+5+1 1 1 Undefined",
    );
}

// ---- optimize ----------------------------------------------------------

#[test]
fn optimize_frame_matches_magick() {
    expect(
        &layers::optimize_frame(&c2()).unwrap(),
        "32x24+0+0 32 24 None;32x24+2+2 21 8 Previous;32x24+2+3 16 9 None;32x24+12+2 11 12 Previous;32x24+12+7 16 9 Previous",
    );
    expect(
        &layers::optimize_frame(&c3()).unwrap(),
        "32x24+2+3 6 5 Previous;32x24+7+5 6 5 Previous;32x24+12+7 6 5 Previous;32x24+17+9 6 5 Previous;32x24+22+11 6 5 Previous",
    );
    expect(
        &layers::optimize_frame(&c4()).unwrap(),
        "40x30+0+8 40 22 Previous;40x30+0+2 40 28 Previous;40x30+0+8 40 22 Previous;40x30+0+2 40 28 Previous;40x30+0+8 40 22 Previous;40x30+0+8 40 22 Previous",
    );
    expect(
        &layers::optimize_frame(&c9()).unwrap(),
        "30x20+0+0 30 18 Previous;30x20+0+0 30 10 Previous;30x20+0+0 30 10 Previous;30x20+0+0 30 18 Previous;30x20+0+0 30 10 Previous;30x20+0+0 30 10 Previous",
    );
    // An unchanged frame is magick's 1×1 transparent pixel at -1-1.
    let out = layers::optimize_frame(&c6()).unwrap();
    expect(
        &out,
        "8x6+0+0 8 6 None;8x6-1-1 1 1 None;8x6+1+1 4 3 None;8x6-1-1 1 1 None;8x6+1+1 4 3 None;8x6-1-1 1 1 None;8x6+1+1 4 3 None",
    );
    assert_eq!(px(&out[1], 0, 0), T);
    assert_eq!(delays_cs(&out), [10, 20, 0, 10, 0, 0, 30]);
}

#[test]
fn optimize_frame_grows_a_background_window_over_what_must_clear() {
    // A yellow block that grows, then moves: the frame before the move
    // is disposed to background over the union of its own window and
    // the pixels the next frame erases (magick: 4,0 3x5 Background).
    let frames = [
        full(7, 5, T, &[(4, 0, 2, 3, Y)], 10),
        full(7, 5, T, &[(4, 0, 2, 3, Y), (6, 2, 1, 3, Y)], 10),
        full(7, 5, T, &[(1, 0, 2, 2, Y)], 10),
    ];
    expect(
        &layers::optimize_frame(&frames).unwrap(),
        "7x5+4+0 2 3 None;7x5+4+0 3 5 Background;7x5+1+0 2 2 Background",
    );
}

#[test]
fn optimize_frame_wants_a_coalesced_list() {
    let err = layers::optimize_frame(&c1()).unwrap_err();
    assert!(err.to_string().contains("coalesce first"), "{err}");
    let mixed = [paint(4, 4, R, &[]), paint(5, 4, R, &[])];
    assert!(layers::optimize_frame(&mixed).is_err());
    assert!(layers::optimize_frame(&[]).is_err());
}

#[test]
fn optimize_plus_matches_magick_geometry_and_keeps_the_timing() {
    let out = layers::optimize_plus(&c4()).unwrap();
    expect(
        &out,
        "40x30+0+8 40 22 None;40x30+3+8 4 5 Background;40x30+7+2 26 11 Background;40x30+11+8 5 5 Previous;40x30+15+2 18 11 Previous;40x30+19+8 5 5 Previous;40x30+23+8 5 5 Previous",
    );
    assert_eq!(delays_cs(&out), [0, 8, 8, 8, 8, 8, 8]);
    let out = layers::optimize_plus(&c9()).unwrap();
    expect(
        &out,
        "30x20+0+0 30 18 None;30x20+0+6 20 12 Background;30x20+4+6 4 4 Previous;30x20+8+6 4 4 Previous;30x20+10+6 10 12 Previous;30x20+16+6 4 4 Previous;30x20+20+6 4 4 Previous",
    );
    // Identical frames after the second fold into the frame before them;
    // magick drops the folded delays when a split follows, the gateway
    // keeps the total.
    let frames = [
        full(6, 4, T, &[(0, 0, 3, 3, R)], 5),
        full(6, 4, T, &[(2, 0, 3, 3, R)], 10),
        full(6, 4, T, &[(2, 0, 3, 3, R)], 20),
        full(6, 4, T, &[(3, 1, 3, 3, R)], 40),
    ];
    let out = layers::optimize_plus(&frames).unwrap();
    assert_eq!(delays_cs(&out).iter().sum::<u64>(), 75);
    assert!(out.len() < frames.len() + 2);
}

#[test]
fn optimize_is_coalesce_frame_and_transparency() {
    expect(
        &layers::optimize(&c2()).unwrap(),
        "32x24+0+0 32 24 None;32x24+2+2 21 8 Previous;32x24+2+3 16 9 None;32x24+12+2 11 12 Previous;32x24+12+7 16 9 Previous",
    );
    // Dispose-None frames with transparency stack (a trail), so magick
    // optimises the trail.
    expect(
        &layers::optimize(&c3()).unwrap(),
        "32x24+2+3 6 5 None;32x24+7+5 6 5 None;32x24+12+7 6 5 None;32x24+17+9 6 5 None;32x24+22+11 6 5 None",
    );
    let transparent = |frames: &[Image]| -> Vec<usize> {
        frames
            .iter()
            .map(|f| {
                f.to_rgba8()
                    .unwrap()
                    .iter()
                    .skip(3)
                    .step_by(4)
                    .filter(|a| **a == 0)
                    .count()
            })
            .collect()
    };
    // Per-frame transparent pixels in magick's output.
    assert_eq!(
        transparent(&layers::optimize(&c2()).unwrap()),
        [0, 105, 84, 69, 84]
    );
    let c4o = layers::optimize(&c4()).unwrap();
    expect(
        &c4o,
        "40x30+0+8 40 22 None;40x30+8+2 25 11 None;40x30+12+8 4 5 None;40x30+16+8 4 5 None;40x30+20+8 4 5 None;40x30+24+8 4 5 None",
    );
    assert_eq!(transparent(&c4o), [455, 223, 0, 0, 0, 0]);
    let c9o = layers::optimize(&c9()).unwrap();
    assert_eq!(transparent(&c9o), [344, 0, 0, 0, 0, 0]);
}

/// Displayed pictures, invisible pixels normalised to transparent black.
fn shown(frames: &[Image]) -> Vec<Vec<u8>> {
    layers::coalesce(frames)
        .unwrap()
        .iter()
        .map(|f| {
            let mut b = f.to_rgba8().unwrap();
            for p in b.chunks_mut(4) {
                if p[3] == 0 {
                    p.fill(0);
                }
            }
            b
        })
        .collect()
}

#[test]
fn optimized_lists_replay_to_the_same_animation() {
    for make in [c1 as fn() -> Vec<Image>, c2, c3, c4, c6, c8, c9] {
        let frames = make();
        let want = shown(&frames);
        assert_eq!(shown(&layers::optimize(&frames).unwrap()), want);
        assert_eq!(
            shown(&layers::optimize_frame(&layers::coalesce(&frames).unwrap()).unwrap()),
            want
        );
        // optimize-plus folds identical frames and splits transitions
        // into two pictures that look alike: compare the distinct
        // pictures in order.
        let mut got = shown(&layers::optimize_plus(&layers::coalesce(&frames).unwrap()).unwrap());
        got.dedup();
        let mut want = want;
        want.dedup();
        assert_eq!(got, want);
    }
}

#[test]
fn optimize_transparency_skips_semi_transparent_matches_and_source_frames() {
    // A semi-transparent pixel drawn over its own value is not a no-op
    // (the alpha grows), so it stays — magick drops it.
    let frames = [
        paint(2, 1, [30, 60, 90, 200], &[]),
        paint(2, 1, [30, 60, 90, 200], &[(0, 0, 1, 1, R)]),
    ];
    let out = layers::optimize_transparency(&frames).unwrap();
    assert_eq!(px(&out[1], 1, 0), [30, 60, 90, 200]);
    // An opaque pixel equal to the canvas goes.
    let frames = [paint(2, 1, R, &[]), paint(2, 1, R, &[(0, 0, 1, 1, B)])];
    let out = layers::optimize_transparency(&frames).unwrap();
    assert_eq!((px(&out[1], 0, 0), px(&out[1], 1, 0)[3]), (B, 0));
    // A frame that replaces its rectangle cannot leave holes.
    let frames: Vec<Image> = frames
        .into_iter()
        .map(|f| f.with_blend(Blend::Source))
        .collect();
    let out = layers::optimize_transparency(&frames).unwrap();
    assert_eq!(px(&out[1], 1, 0), R);
}

// ---- clean-up and flattening -------------------------------------------

#[test]
fn remove_dups_and_remove_zero_match_magick() {
    let out = layers::remove_dups(&c6()).unwrap();
    assert_eq!(delays_cs(&out), [30, 10, 0, 30]);
    assert_eq!(out.len(), 4);
    let out = layers::remove_zero(&c6());
    assert_eq!(delays_cs(&out), [10, 20, 10, 30]);
    // All zero: unchanged.
    let zeros: Vec<Image> = c6().into_iter().map(|f| f.with_delay(None)).collect();
    assert_eq!(layers::remove_zero(&zeros).len(), 7);
    // The last frame of a run is the one kept (its disposal, its page).
    let frames = [
        full(8, 6, R, &[], 3),
        full(8, 6, R, &[], 5).with_dispose(Dispose::Background),
        full(8, 6, R, &[], 7).with_dispose(Dispose::Previous),
    ];
    let out = layers::remove_dups(&frames).unwrap();
    assert_eq!(
        (out.len(), out[0].dispose(), out[0].delay_ticks()),
        (1, Dispose::Previous, 15)
    );
    // A different offset is not a duplicate.
    let frames = [
        full(8, 6, R, &[], 3),
        full(8, 6, R, &[], 5).with_offset(1, 0),
    ];
    assert_eq!(layers::remove_dups(&frames).unwrap().len(), 2);
}

#[test]
fn flatten_mosaic_merge_and_trim_bounds_match_magick() {
    let f = layers::flatten(&c8()).unwrap();
    expect(std::slice::from_ref(&f), "20x14+0+0 20 14 Undefined");
    assert_eq!(
        [(0, 7), (4, 3), (13, 10), (16, 2), (19, 13), (1, 1)].map(|(x, y)| px(&f, x, y)),
        [B, R, HALF_GREEN, Y, T, T]
    );
    let m = layers::mosaic(&c8()).unwrap();
    expect(std::slice::from_ref(&m), "21x14+0+0 21 14 Undefined");
    assert_eq!((px(&m, 20, 2), px(&m, 0, 7)), (Y, B));
    let m = layers::merge(&c8()).unwrap();
    assert_eq!(m.page().to_string(), "20x14-2+1");
    assert_eq!((m.width(), m.height()), (23, 13));
    assert_eq!((px(&m, 0, 6), px(&m, 5, 1)), (B, R));
    expect(
        &layers::trim_bounds(&c8()),
        "23x13+5+1 10 10 None;23x13+0+5 8 4 Previous;23x13+14+8 5 5 Background;23x13+17+0 6 3 Undefined",
    );
    let f = layers::flatten(&c1()).unwrap();
    expect(std::slice::from_ref(&f), "40x30+0+0 40 30 Undefined");
    assert_eq!(f.delay_ticks(), 10);
    // Opaque background option.
    let f = layers::flatten_with(&c8(), &LayerOptions::new().with_background_rgba8(W)).unwrap();
    assert_eq!(px(&f, 19, 13), W);
}

#[test]
fn composite_layers_pairs_lists_like_magick() {
    let dst = [
        layer(8, 6, R, 0, 0, 20, 20, Dispose::None),
        layer(8, 6, G, 1, 1, 20, 20, Dispose::None),
    ];
    let src = [layer(2, 2, B, 3, 2, 20, 20, Dispose::None)];
    let out = layers::composite_layers(&dst, &src, 0, 0).unwrap();
    expect(&out, "20x20+0+0 8 6 None; 20x20+1+1 8 6 None");
    // Source placed relative to each destination frame's offset.
    assert_eq!((px(&out[0], 3, 2), px(&out[0], 2, 2)), (B, R));
    assert_eq!((px(&out[1], 2, 1), px(&out[1], 4, 3)), (B, G));
    // One destination repeated per source frame; the offset moves the
    // source (here mostly off the frame).
    let dst = [layer(8, 6, R, 5, 0, 20, 20, Dispose::None)];
    let src = [
        layer(2, 2, B, 3, 2, 20, 20, Dispose::None),
        layer(2, 2, Y, 0, 0, 20, 20, Dispose::None),
    ];
    let out = layers::composite_layers(&dst, &src, 1, 0).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!((px(&out[0], 0, 2), px(&out[0], 1, 2)), (B, R));
    assert!(out[1].to_rgba8().unwrap().chunks(4).all(|p| p == R));
    // Longer destination: the rest is kept.
    let dst = [c2()[0].clone(), c2()[1].clone(), c2()[2].clone()];
    let src = [c3()[0].clone(), c3()[1].clone()];
    assert_eq!(layers::composite_layers(&dst, &src, 0, 0).unwrap().len(), 3);
}

#[test]
fn layer_methods_dispatch() {
    let frames = c2();
    for (m, n) in [
        (LayerMethod::Coalesce, 5),
        (LayerMethod::OptimizeFrame, 5),
        (LayerMethod::Flatten, 1),
        (LayerMethod::Mosaic, 1),
        (LayerMethod::Merge, 1),
        (LayerMethod::TrimBounds, 5),
        (LayerMethod::RemoveDups, 5),
        (LayerMethod::CompareAny, 5),
    ] {
        assert_eq!(
            layers::layers(&frames, m, &LayerOptions::new())
                .unwrap()
                .len(),
            n,
            "{m:?}"
        );
    }
}

#[test]
fn deep_frames_come_out_sixteen_bit() {
    let deep = Image::from_raw(
        2,
        1,
        oxideav_core::PixelFormat::Rgba64Le,
        [[0x1234u16, 0, 0, 0xffff], [0, 0, 0, 0]]
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
    )
    .unwrap()
    .with_page(Page::new(2, 1));
    let out = layers::coalesce(&[deep.clone(), deep]).unwrap();
    assert_eq!(out[0].format(), oxideav_core::PixelFormat::Rgba64Le);
    assert_eq!(&out[0].to_rgba16().unwrap()[0..2], &0x1234u16.to_le_bytes());
}

// ---- through the gateway (OXIM fixture) ---------------------------------

mod gateway {
    use super::*;
    use crate::fixture::{self, Fixture};
    use crate::{SaveOptions, decode_bytes, encode_frames};
    use oxideav_core::{CodecId, PixelFormat};

    #[test]
    fn loop_count_reads_the_demuxer_pair() {
        let ctx = fixture::ctx();
        let mut fx = Fixture::new(2, 2, PixelFormat::Rgba);
        fx.push_gradient(10, 1).push_gradient(10, 2);
        assert_eq!(decode_bytes(&ctx, &fx.encode()).unwrap().loop_count(), None);
        fx.metadata = vec![("loop_count".into(), "3".into())];
        assert_eq!(
            decode_bytes(&ctx, &fx.encode()).unwrap().loop_count(),
            Some(3)
        );
        fx.metadata = vec![("loop_count".into(), "0".into())];
        assert_eq!(
            decode_bytes(&ctx, &fx.encode()).unwrap().loop_count(),
            Some(0)
        );
    }

    #[test]
    fn loop_count_is_written_through_the_muxer_stream() {
        let ctx = fixture::ctx();
        let frames = c3();
        let bytes = encode_frames(
            &ctx,
            &frames,
            "oxim",
            &SaveOptions::new().with_loop_count(4),
        )
        .unwrap();
        let back = decode_bytes(&ctx, &bytes).unwrap();
        assert_eq!(back.loop_count(), Some(4));
        // A caller-set option wins.
        let opts = SaveOptions::new()
            .with_loop_count(4)
            .with_option("loop_count", "9");
        assert_eq!(opts.loop_option(&CodecId::new("oxim"), Some("oxim")), None);
    }

    #[test]
    fn loop_count_speaks_each_format_unit() {
        let gif = CodecId::new("gif");
        let webp = CodecId::new("webp_vp8l");
        let o = |n| SaveOptions::new().with_loop_count(n);
        // NETSCAPE2.0 counts the repeats after the first pass; one play
        // is no looping block (`-1` in the gif schema).
        assert_eq!(o(0).loop_option(&gif, Some("gif")).as_deref(), Some("0"));
        assert_eq!(o(1).loop_option(&gif, Some("gif")).as_deref(), Some("-1"));
        assert_eq!(o(3).loop_option(&gif, None).as_deref(), Some("2"));
        assert_eq!(o(3).loop_option(&webp, Some("webp")).as_deref(), Some("3"));
        assert_eq!(SaveOptions::new().loop_option(&webp, Some("webp")), None);
    }

    #[test]
    fn layered_lists_are_written_as_displayed() {
        let ctx = fixture::ctx();
        let frames = c1();
        let bytes = encode_frames(&ctx, &frames, "oxim", &SaveOptions::default()).unwrap();
        let back = decode_bytes(&ctx, &bytes).unwrap();
        // One stream at the canvas size, not one per layer geometry.
        assert_eq!(back.len(), 4);
        assert!(
            back.frames()
                .iter()
                .all(|f| (f.width(), f.height(), f.stream()) == (40, 30, 0))
        );
        let want = shown(&frames);
        let got: Vec<Vec<u8>> = back
            .frames()
            .iter()
            .map(|f| {
                let mut b = f.to_rgba8().unwrap();
                for p in b.chunks_mut(4) {
                    if p[3] == 0 {
                        p.fill(0);
                    }
                }
                b
            })
            .collect();
        assert_eq!(got, want);
        // coalesce → optimize → write gives the same file content.
        let optimized = layers::optimize(&frames).unwrap();
        let again = encode_frames(&ctx, &optimized, "oxim", &SaveOptions::default()).unwrap();
        let back2 = decode_bytes(&ctx, &again).unwrap();
        for (a, b) in back.frames().iter().zip(back2.frames()) {
            assert_eq!(a.to_rgba8().unwrap(), b.to_rgba8().unwrap());
        }
    }

    #[test]
    fn a_later_frame_s_transparency_keeps_the_alpha_layout() {
        let ctx = fixture::ctx();
        let frames = [
            paint(3, 2, R, &[]).with_delay_ticks(10, 100),
            paint(3, 2, R, &[(0, 0, 1, 1, T)]).with_delay_ticks(10, 100),
        ];
        let bytes = encode_frames(&ctx, &frames, "oxim", &SaveOptions::default()).unwrap();
        let back = decode_bytes(&ctx, &bytes).unwrap();
        assert!(back.frames()[1].has_transparency());
        assert_eq!(px(&back.frames()[1], 0, 0)[3], 0);
    }

    #[test]
    fn zero_delays_survive_and_decoded_frames_replace_the_canvas() {
        let ctx = fixture::ctx();
        let mut fx = Fixture::new(2, 2, PixelFormat::Rgba);
        fx.push_gradient(100, 1)
            .push_gradient(0, 2)
            .push_gradient(0, 3);
        let file = decode_bytes(&ctx, &fx.encode()).unwrap();
        let delays: Vec<Option<Duration>> = file.frames().iter().map(Image::delay).collect();
        assert_eq!(
            delays,
            [
                Some(Duration::from_millis(100)),
                Some(Duration::ZERO),
                Some(Duration::ZERO)
            ]
        );
        assert_eq!(file.frames()[0].ticks_per_second(), 1000);
        assert!(
            file.frames()
                .iter()
                .all(|f| f.blend() == Blend::Source && f.is_full_canvas())
        );
        // remove-zero then sees the intermediate frames.
        assert_eq!(layers::remove_zero(file.frames()).len(), 1);
    }
}
