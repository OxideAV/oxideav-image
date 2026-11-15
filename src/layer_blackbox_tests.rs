//! The layer operations against a black-box reader: when the `magick`
//! executable of the version the pinned vectors were recorded with is
//! on the path, random 1-bit-alpha animations are played and optimised
//! by it and by the gateway and compared — page geometry, disposal and
//! pixels. Without the executable (or with another version) the test
//! does nothing.

use std::path::Path;
use std::process::Command;

use crate::layers;
use crate::{Blend, Dispose, Image, Page};

/// The version the expectations in `layer_tests` were recorded with.
const VERSION: &str = "ImageMagick 7.1.2-29";

fn validator() -> bool {
    Command::new("magick")
        .arg("-version")
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(VERSION))
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) % n
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
}

const COLOURS: [[u8; 4]; 5] = [
    [255, 0, 0, 255],
    [0, 255, 0, 255],
    [0, 0, 255, 255],
    [255, 255, 0, 255],
    [0, 0, 0, 0],
];

fn random_list(rng: &mut Rng) -> Vec<Image> {
    let (cw, ch) = (rng.range(6, 14), rng.range(6, 14));
    let n = rng.range(2, 5);
    (0..n)
        .map(|k| {
            // The first frame covers the canvas, as an animation's does.
            let (w, h, x, y) = if k == 0 {
                (cw, ch, 0, 0)
            } else {
                let (w, h) = (rng.range(1, cw), rng.range(1, ch));
                (w, h, rng.range(-2, cw - 1), rng.range(-2, ch - 1))
            };
            let base = COLOURS[rng.below(5) as usize];
            let mut px = Vec::with_capacity((w * h * 4) as usize);
            for _ in 0..w * h {
                let c = if rng.below(4) == 0 {
                    COLOURS[rng.below(5) as usize]
                } else {
                    base
                };
                px.extend_from_slice(&c);
            }
            let dispose =
                [Dispose::None, Dispose::Background, Dispose::Previous][rng.below(3) as usize];
            Image::from_rgba8(w as u32, h as u32, px)
                .unwrap()
                .with_page(Page::new(cw as u32, ch as u32).with_offset(x as i32, y as i32))
                .with_dispose(dispose)
                .with_blend(Blend::Over)
                .with_delay_ticks(10, 100)
        })
        .collect()
}

/// One frame as the validator reports it: page, size, disposal, RGBA.
#[derive(Debug, PartialEq)]
struct Seen {
    geometry: String,
    pixels: Vec<u8>,
}

fn seen(frames: &[Image]) -> Vec<Seen> {
    frames
        .iter()
        .map(|f| {
            let (cw, ch) = f.canvas_size();
            let (x, y) = f.offset();
            let mut pixels = f.to_rgba8().unwrap();
            // The colour under a zero alpha is not part of the picture.
            for p in pixels.as_chunks_mut::<4>().0 {
                if p[3] == 0 {
                    *p = [0; 4];
                }
            }
            Seen {
                geometry: format!(
                    "{cw} {ch} {x:+} {y:+} {} {} {}",
                    f.width(),
                    f.height(),
                    f.dispose().name()
                ),
                pixels,
            }
        })
        .collect()
}

/// Run the validator on `frames` with `operations`; `None` when it
/// refuses the list.
fn validate(dir: &Path, frames: &[Image], operations: &[&str]) -> Option<Vec<Seen>> {
    let mut cmd = Command::new("magick");
    cmd.current_dir(dir).args(["-background", "none"]);
    for (i, f) in frames.iter().enumerate() {
        let name = format!("in{i}.rgba");
        std::fs::write(dir.join(&name), f.to_rgba8().unwrap()).unwrap();
        let (cw, ch) = f.canvas_size();
        let (x, y) = f.offset();
        cmd.arg("(")
            .args(["-size", &format!("{}x{}", f.width(), f.height())])
            .args(["-depth", "8", &format!("rgba:{name}")])
            .args(["-repage", &format!("{cw}x{ch}{x:+}{y:+}")])
            .args(["-set", "dispose", f.dispose().name()])
            .arg(")");
    }
    cmd.args(operations)
        .args([
            "-format",
            "%W %H %X %Y %w %h %D\n",
            "-write",
            "info:meta.txt",
        ])
        .args(["+adjoin", "-depth", "8", "rgba:out%d.rgba"]);
    if !cmd.output().ok()?.status.success() {
        return None;
    }
    let meta = std::fs::read_to_string(dir.join("meta.txt")).ok()?;
    let mut out = Vec::new();
    for (i, line) in meta.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let mut pixels = std::fs::read(dir.join(format!("out{i}.rgba"))).ok()?;
        for p in pixels.as_chunks_mut::<4>().0 {
            if p[3] == 0 {
                *p = [0; 4];
            }
        }
        out.push(Seen {
            geometry: line.trim().to_string(),
            pixels,
        });
    }
    Some(out)
}

#[test]
fn coalesce_and_optimize_frame_match_the_black_box_reader() {
    if !validator() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("oxideav-image-layers-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = Rng(0x5EED_0F1A_7E55);
    let geometry =
        |frames: &[Seen]| -> Vec<String> { frames.iter().map(|f| f.geometry.clone()).collect() };
    let (mut played, mut optimised, mut compared) = (0, 0, 0);
    for case in 0..60 {
        let frames = random_list(&mut rng);
        let shown = layers::coalesce(&frames).unwrap();
        if let Some(theirs) = validate(&dir, &frames, &["-coalesce"]) {
            assert_eq!(seen(&shown), theirs, "case {case}: coalesce");
            played += 1;
        }
        let small = layers::optimize_frame(&shown).unwrap();
        if let Some(theirs) = validate(&dir, &frames, &["-coalesce", "-layers", "optimize-frame"]) {
            assert_eq!(seen(&small), theirs, "case {case}: optimize-frame");
            optimised += 1;
        }
        // The full optimisation: geometry and disposal (the reader's
        // transparency pass writes other pixels; README, *Layers*).
        let all = layers::optimize(&frames).unwrap();
        if let Some(theirs) = validate(&dir, &frames, &["-layers", "optimize"]) {
            assert_eq!(
                geometry(&seen(&all)),
                geometry(&theirs),
                "case {case}: optimize"
            );
        }
        let changed = layers::compare_any(&frames).unwrap();
        if let Some(theirs) = validate(&dir, &frames, &["-layers", "compare-any"]) {
            assert_eq!(seen(&changed), theirs, "case {case}: compare-any");
            compared += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        played >= 45 && optimised >= 45 && compared >= 45,
        "{played} played, {optimised} optimised, {compared} compared"
    );
}
