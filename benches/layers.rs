//! Layer-operation throughput on a synthetic 100-frame 1920×1080
//! animation: a full-canvas opaque first frame, then 320×240 sprites
//! (a disc with a transparent surround) moving across it, cycling
//! through the three disposals.
//!
//! `cargo bench --bench layers` prints frames per second for each
//! operation, the median of `LAYERS_BENCH_RUNS` runs (default 3);
//! `LAYERS_BENCH_FRAMES` / `LAYERS_BENCH_WIDTH` / `LAYERS_BENCH_HEIGHT`
//! / `LAYERS_BENCH_THREADS` scale it, and `LAYERS_BENCH_ONLY=<substring>`
//! runs only the operations whose label contains it (for a profiler).

use std::time::Instant;

use oxideav_image::layers::{self, LayerOptions, OptimizeOptions};
use oxideav_image::{Blend, Dispose, Image, Page};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn animation(w: u32, h: u32, n: usize) -> Vec<Image> {
    let mut first = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            first.extend_from_slice(&[
                (x & 255) as u8,
                (y & 255) as u8,
                ((x ^ y) & 255) as u8,
                255,
            ]);
        }
    }
    let (sw, sh) = (320u32.min(w), 240u32.min(h));
    let mut frames = vec![
        Image::from_rgba8(w, h, first)
            .unwrap()
            .with_page(Page::new(w, h))
            .with_dispose(Dispose::None)
            .with_blend(Blend::Source)
            .with_delay_ticks(4, 100),
    ];
    for k in 1..n {
        let mut px = Vec::with_capacity((sw * sh * 4) as usize);
        let (cx, cy, r) = (sw as i64 / 2, sh as i64 / 2, sh as i64 / 2 - 4);
        for y in 0..sh as i64 {
            for x in 0..sw as i64 {
                let inside = (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r;
                if inside {
                    px.extend_from_slice(&[
                        ((k * 37) & 255) as u8,
                        (x & 255) as u8,
                        (y & 255) as u8,
                        255,
                    ]);
                } else {
                    px.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        let x = (k as u32 * 53) % (w - sw / 2);
        let y = (k as u32 * 29) % (h - sh / 2);
        let dispose = match k % 3 {
            0 => Dispose::None,
            1 => Dispose::Background,
            _ => Dispose::Previous,
        };
        frames.push(
            Image::from_rgba8(sw, sh, px)
                .unwrap()
                .with_page(Page::new(w, h).with_offset(x as i32, y as i32))
                .with_dispose(dispose)
                .with_delay_ticks(4, 100),
        );
    }
    frames
}

/// Time `op` when `label` is selected (always run it once for its
/// result).
fn report<T>(label: &str, runs: usize, frames: usize, op: impl FnMut() -> T) -> T {
    let only = std::env::var("LAYERS_BENCH_ONLY").unwrap_or_default();
    let runs = if label.contains(&only) { runs } else { 1 };
    let (fps, out) = median_fps(runs, frames, op);
    if label.contains(&only) {
        println!("{label:<32}{fps:9.1} frames/s");
    }
    out
}

fn median_fps<T>(runs: usize, frames: usize, mut op: impl FnMut() -> T) -> (f64, T) {
    let mut secs = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        drop(last.take());
        let t = Instant::now();
        last = Some(op());
        secs.push(t.elapsed().as_secs_f64());
    }
    secs.sort_by(f64::total_cmp);
    (frames as f64 / secs[secs.len() / 2], last.unwrap())
}

fn main() {
    let (w, h) = (
        env("LAYERS_BENCH_WIDTH", 1920) as u32,
        env("LAYERS_BENCH_HEIGHT", 1080) as u32,
    );
    let n = env("LAYERS_BENCH_FRAMES", 100);
    let runs = env("LAYERS_BENCH_RUNS", 3);
    let layered = animation(w, h, n);
    let threads = env("LAYERS_BENCH_THREADS", 1);
    let layer = LayerOptions::new().with_threads(threads);
    println!("{n} frames {w}x{h}, median of {runs}, {threads} thread(s)");

    let coalesced = report("coalesce", runs, n, || {
        layers::coalesce_with(&layered, &layer).unwrap()
    });
    let framed = report("optimize_frame (coalesced in)", runs, n, || {
        layers::layers(&coalesced, layers::LayerMethod::OptimizeFrame, &layer).unwrap()
    });
    report("optimize_transparency", runs, n, || {
        layers::optimize_transparency_with(&framed, &layer).unwrap()
    });
    drop(framed);
    let exact = OptimizeOptions::new()
        .with_layer_options(layer)
        .with_transparency(true);
    let small = report("optimize_layers (coalesced in)", runs, n, || {
        layers::optimize_layers_with(&coalesced, &exact).unwrap()
    });
    let area: u64 = small
        .iter()
        .map(|f| u64::from(f.width()) * u64::from(f.height()))
        .sum();
    println!(
        "  optimised area {:.2}% of {n} canvases",
        100.0 * area as f64 / (n as f64 * f64::from(w) * f64::from(h))
    );
    drop(small);
    drop(coalesced);
    report("optimize (layered in)", runs, n, || {
        layers::optimize_with(&layered, &layer).unwrap()
    });
    report("optimize_layers (layered in)", runs, n, || {
        layers::optimize_layers_with(&layered, &exact).unwrap()
    });
}
