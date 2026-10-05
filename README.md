# oxideav-image

Image gateway for the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework — open, decode, convert and save still images and image
sequences through the registered format crates, with raw RGB / RGBA
`Vec<u8>` in and out.

This is **Layer 2** of the image-crate API described in the workspace's
[`IMAGE_CRATE_API.md`](https://github.com/OxideAV/oxideav-workspace/blob/master/IMAGE_CRATE_API.md).
Layer 1 is each format crate's own standalone surface
(`oxideav_png::decode_rgba8` and friends, no `oxideav-core` needed).
Layer 2 is one entry point over *every* registered format, for
applications that already run the framework: the same probe → demuxer →
decoder resolution every framework path uses, and the same encoder →
muxer path back.

The crate depends on `oxideav-core` and `oxideav-pixfmt` only. The
application registers the formats it wants (normally
`oxideav_meta::register_all`); `oxideav-meta` resolves only inside the
workspace, which is what lets this crate publish on its own.

## Usage

```rust,ignore
use oxideav_image::{open, save, encode, encode_frames, Image, OpenOptions, SaveOptions};

let mut ctx = oxideav_core::RuntimeContext::new();
oxideav_meta::register_all(&mut ctx);          // or register the few crates you need

let file = open(&ctx, "photo.heic")?;          // probe → demuxer → decoder, via the registry
let img = file.primary();                      // native layout, e.g. Yuv420P
let rgba: Vec<u8> = img.to_rgba8()?;           // tightly packed, 4 bytes per pixel
println!("{}x{} {:?} {:?}", img.width(), img.height(), img.format(), img.color_signal());

for frame in &file {                           // animations, sequences, pages
    println!("{:?} at {:?}", frame.delay(), frame.timestamp());
}

save(&ctx, img, "out.png", &SaveOptions::default())?;
let heic = encode(&ctx, img, "heic", &SaveOptions::new().with_option("qp", "20"))?;

let thumb = img.crop(0, 0, 256, 256)?;
let apng = encode_frames(&ctx, file.frames(), "png", &SaveOptions::default())?;
```

## API

| Item | Role |
|---|---|
| `open(&ctx, path)` / `open_with(.., &OpenOptions)` | Probe the file (extension as hint), open its demuxer, decode every video stream → `ImageFile`. |
| `decode_bytes(&ctx, &[u8])` / `decode_vec(&ctx, Vec<u8>)` / `decode_reader(&ctx, Box<dyn ReadSeek>, &OpenOptions)` | Same from memory (`decode_vec` without a copy) or any seekable reader. `*_with` variants take `OpenOptions`. |
| `ImageFile` | `container()`, `metadata()`, `primary()`, `frames()`, `frames_mut()`, `iter()`, `into_frames()`, `into_primary()`, `len()`; `IntoIterator` for `ImageFile` and `&ImageFile`. |
| `Image` | A `VideoFrame` + `CodecParameters`: `width` / `height` / `format` / `color_signal` / `palette`; `delay`, `timestamp`, `stream`, `index`, `raw_timing`, `time_base`; `to_format(PixelFormat)`, `to_gray8`, `to_rgb8`, `to_rgba8`, `to_rgba16`, `to_packed`, `as_packed`, `into_raw`, `crop`, `with_delay`, `with_color_signal`, `frame`, `params`, `into_video_frame`. |
| `Image::from_rgb8` / `from_rgba8` / `from_raw` / `from_planes` / `from_video_frame` | Fallible constructors: geometry, plane count, stride and length are validated. |
| `encode(&ctx, &Image, "png", &SaveOptions)` / `encode_frames(&ctx, &[Image], ..)` / `save(&ctx, &Image, path, ..)` | Encoder + muxer by container name or extension; see *Format resolution* and *Pixel-format ladder*. |
| `encoder_options(&ctx, format, &SaveOptions)` | The option schema of the encoder `encode` would use (names for `SaveOptions::with_option`). |
| `OpenOptions` | `max_frames`, `ext_hint`, `decoder_options`, `max_pixels`. |
| `SaveOptions` | `quality`, `pixel_format`, `codec`, `options`, `time_base`, `default_delay`. |
| `ImageError` (`Error`) | `#[non_exhaustive]`: see *Errors*. |

All conversions go through `oxideav-pixfmt`; `to_gray8` / `to_rgb8` /
`to_rgba8` / `to_rgba16` return rows without padding. The colour signal
used for YCbCr ↔ RGB is the frame's own record, else the stream-level
one from the container, else pixfmt's defaults.

### Format resolution

- **Opening:** the registry's probe (magic bytes, extension hint as a
  tie-breaker) picks the container; every video stream gets the
  registry's first decoder; other streams are skipped. Pictures come
  back in packet order; `Image::stream()` says which stream each came
  from (a HEIF with a primary item and a sequence track yields the
  still from stream 0 and the track's frames from stream 1).
- **Saving:** the format name or extension resolves to a container with
  a muxer. The payload codec is the container's default: the codec of
  the same name (png, bmp, tiff, heif, …) or the few that differ (`jpeg`
  → `mjpeg`, `dcx` → `pcx`, `ani` / `cur` → `ico`, `svgz` → `svg`,
  `iff_*` → `ilbm`); `SaveOptions::codec` overrides it (`"hevc"` or
  `"av1"` for a HEIF sequence track, for instance).
- **Codec-only formats:** some image crates register a codec and an
  extension but no container (qoi, webp, gif, avif, openexr, pict, icer,
  jpeg2000, jpegxs, jpegxl). `encode` / `save` write the encoder's
  single packet as the file for them (prefixed encoder variants resolve
  too: `webp` → `webp_vp8l`, or `webp_vp8` when a quality is asked).
  Opening such a file through the registry is not possible until the
  crate grows a demuxer; `UnknownFormat` says so.

### Pixel-format ladder

`encode` hands the encoder the picture's own layout first, then — when
the encoder or the muxer refuses it — the common layouts pixfmt can
reach from it, in order: `Rgba`, `Rgb24`, `Gray8`, `Yuv444P`,
`Yuv420P`, `Rgba64Le`. `SaveOptions::pixel_format` replaces the ladder
by a single layout. When every rung fails for the same reason (an
unknown option, a muxer refusing the stream) that error is returned
unchanged; otherwise `Unsupported` lists what each rung hit.

### Options

`SaveOptions::quality` is forwarded as the encoder's `"quality"` option
only when the encoder's declared schema has one (encoders parse their
options strictly; a lossless encoder would reject the unknown name).
`SaveOptions::with_option(name, value)` is forwarded verbatim; names
come from `encoder_options`. `OpenOptions::with_decoder_option` does the
same on the decoding side.

### Timing

Delays are read from the stream, never from a format table:

- `Image::delay()` is the packet's `duration` in the stream time base
  when the demuxer set one; else the gap to the next picture of the
  same stream; the last picture without a duration repeats the previous
  delay. A stream with a single picture is a still and has no delay,
  whatever nominal duration its demuxer stamped. A stream on a `1/1`
  time base is untimed by convention (HEIF bursts, EXR parts, ICER
  bands, TIFF pages): its pictures are indexed, not scheduled, and
  report no delay. `Image::timestamp()` is the presentation time from
  the stream start.
- `encode_frames` rescales every picture's delay (or
  `SaveOptions::default_delay`, 100 ms) to `SaveOptions::time_base`
  (milliseconds by default; 1/100 s for `png`, the APNG delay unit),
  gives frames cumulative `pts`, and — when the encoder returns one
  packet per picture — stamps `pts` / `dts` / `duration` / `time_base`
  on the packets so `decode(encode_frames(..))` reports the same
  delays. An encoder that folds every picture into one packet (png
  writes an APNG itself) is re-run once per picture when the container
  has a muxer, so the muxer assembles the file with these delays. A
  single still is written without a duration.

### Limits

`OpenOptions::max_pixels` is a budget over the decoded pixels of the
whole file: checked from the stream geometry before a decoder exists
and again before each picture is kept, so an oversized file fails with
`LimitExceeded` before its pixels are decoded. `max_frames` bounds what
the budget has to cover. `decode_bytes(&[u8])` copies the slice once
(the demuxer needs an owned seekable reader); `decode_vec` does not.

### Errors

| Variant | When |
|---|---|
| `Io` | File or stream I/O failed. |
| `UnknownFormat` | No container probed the input, the name / extension matches nothing with a muxer, or the extension belongs to a codec-only crate with no demuxer. |
| `NoImage` | A container matched but holds no video stream with a registered decoder, or decoded no picture. |
| `Unsupported` | No encoder for the codec, no layout of the ladder accepted, a planar layout asked for as packed bytes, a `crop` origin off the chroma grid or on a bit-packed layout. |
| `InvalidData` | Caller data inconsistent with its geometry (buffer length, plane count, stride), a crop window outside the picture, mixed geometries in `encode_frames`, a stream without a pixel format. |
| `LimitExceeded` | `max_pixels` would be exceeded. |
| `Core` | A registry, demuxer, decoder, muxer or encoder error, unchanged. |

### What the gateway deliberately does not do

- **Format-specific depth.** HEIF item graphs, PNG chunks, TIFF pages
  by tag, EXR channels and the like stay under the format crates' own
  names (`oxideav_heif::HeifFile`, `oxideav_png::PngMetadata`, …). The
  gateway is the common floor.
- **ICC / Exif / XMP.** The framework's `VideoFrame` and
  `CodecParameters` carry a colour signal (H.273 code points + range),
  a palette and significant bits, but no embedded profile or metadata
  blobs; until a core side-channel or stream field exists, those are
  Layer 1's (`XxxImage::metadata`). `ImageFile::metadata()` is the
  container's string pairs as the demuxer reports them.
- **Guessing.** A stream without a declared pixel format, a frame whose
  plane count disagrees with it, or a probe miss is an error, not an
  inference.

## Testing

Unit tests run against a synthetic `OXIM` container + codec pair
registered exactly like a sibling crate (probe by magic and by extension
hint, 14 packed and planar layouts, `Pal8` with its palette, multi-frame
timing in every mode, the ladder against a layout-refusing codec and a
layout-refusing muxer, options, limits, every error variant). The
umbrella's `crates/oxideav-tests/tests/image_gateway.rs` runs the gateway
over the real format crates through `oxideav_meta::register_all`.

## License

MIT — Copyright (c) 2026 Karpelès Lab Inc.
