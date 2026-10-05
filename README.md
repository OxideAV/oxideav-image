# oxideav-image

Image gateway for the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework — open, decode, convert and save still images and image
sequences through the registered format crates, with raw RGB / RGBA
`Vec<u8>` in and out.

This is **Layer 2** of the image-crate API described in the workspace's
[`IMAGE_CRATE_API.md`](https://github.com/OxideAV/oxideav-workspace/blob/master/IMAGE_CRATE_API.md).
Layer 1 is each format crate's own standalone surface (`oxideav_png::decode_rgba8`
and friends, no `oxideav-core` needed). Layer 2 is one entry point over
*every* registered format, for applications that already run the
framework.

## Usage

```rust,ignore
use oxideav_image::{open, save, SaveOptions};

let mut ctx = oxideav_core::RuntimeContext::new();
oxideav_meta::register_all(&mut ctx);          // or register the few crates you need

let file = open(&ctx, "photo.heic")?;          // probe → demuxer → decoder, via the registry
let img = file.primary();                      // native layout, e.g. Yuv420P
let rgba: Vec<u8> = img.to_rgba8()?;           // tightly packed, 4 bytes per pixel
println!("{}x{} {:?} {:?}", img.width(), img.height(), img.format(), img.color_signal());

for frame in file.frames() {                   // animations, bursts, sequences, pages
    let _ = frame.delay();
}

save(&ctx, img, "out.png", &SaveOptions::default())?;
let avif = oxideav_image::encode(&ctx, img, "avif", &SaveOptions::new().with_quality(80))?;
```

The gateway depends on `oxideav-core` and `oxideav-pixfmt` only. The
application registers formats (normally `oxideav_meta::register_all`);
`oxideav-meta` resolves only inside the workspace, so keeping registration
on the caller's side is what lets this crate publish on its own.

## API

| Item | Role |
|---|---|
| `open(&ctx, path)` / `open_with(.., &OpenOptions)` | Probe the file, open its demuxer, decode every video stream → `ImageFile`. |
| `decode_bytes(&ctx, &[u8])` / `decode_reader(&ctx, Box<dyn ReadSeek>, &OpenOptions)` | Same from memory or any seekable reader. |
| `ImageFile` | `container()`, `metadata()`, `primary()`, `frames()`, `into_frames()`. |
| `Image` | A `VideoFrame` + `CodecParameters`: `width` / `height` / `format` / `color_signal` / `palette` / `delay`; `to_format(PixelFormat)`, `to_rgb8`, `to_rgba8`, `to_rgba16`, `to_packed`, `as_packed`, `into_raw`, `into_video_frame`. |
| `Image::from_rgb8` / `from_rgba8` / `from_raw` / `from_video_frame` | Fallible constructors (geometry and plane count validated). |
| `encode(&ctx, &Image, "png", &SaveOptions)` / `encode_frames` / `save(.., path, ..)` | Muxer by container name or extension, encoder by the container's default codec (or `SaveOptions::codec`); a layout the encoder or muxer refuses steps to the next candidate. |
| `OpenOptions` | `max_frames`, `ext_hint`. |
| `SaveOptions` | `quality`, `pixel_format`, `codec`, `options`. |

All conversions go through `oxideav-pixfmt`; `to_rgb8` / `to_rgba8` /
`to_rgba16` return rows without padding.

## Status

First cut: open / decode / convert / encode / save over the registry.
Planned: `crop`, metadata accessors for ICC / Exif / XMP once the
framework carries them on the frame, `oxideav-io`'s image half and the
private `RgbaImage` copies in `oxideav-cli-convert` / `oxideav-render`
moving onto this crate.

## License

MIT — Copyright (c) 2026 Karpelès Lab Inc.
