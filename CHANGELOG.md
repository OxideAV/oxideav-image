# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Layer 2 of the image-crate API (`IMAGE_CRATE_API.md` in the workspace):
  `open` / `open_with` / `decode_bytes` / `decode_reader` resolve the
  demuxer and decoder through the `oxideav-core` registries and return an
  `ImageFile` (container name, metadata, every decoded `Image`).
- `Image` wraps a framework `VideoFrame` + `CodecParameters`: `width` /
  `height` / `format` / `color_signal` / `palette`, `to_format` through
  `oxideav-pixfmt`, `to_rgb8` / `to_rgba8` / `to_rgba16` tightly packed,
  `into_raw`, `as_packed`, `into_video_frame`; constructors
  `from_video_frame` / `from_raw` / `from_rgb8` / `from_rgba8` validate
  geometry and return `Result`.
- `encode` / `encode_frames` / `save` pick the muxer by format name or
  extension and the encoder by the container's default codec (or
  `SaveOptions::codec`), stepping a pixel-format ladder when an encoder
  or muxer rejects a layout.
- Codec-only formats: when a format name or extension resolves to a
  codec that registers an encoder but no container (qoi, webp, gif,
  avif, exr, … hand whole files to their codec), `encode` / `save` write
  the encoder's single packet as the file. Opening such a file still
  needs a container demuxer; `UnknownFormat` now says so.
- Default codec of a container: the encoder of the same name when one is
  registered, else a short exception table (`jpeg` → `mjpeg`, `dcx` →
  `pcx`, `ani` / `cur` → `ico`, `svgz` → `svg`, `iff_*` → `ilbm`).
- `Image::with_color_signal`.
- Frame timing from the stream: `Image::delay` follows a documented rule
  (the packet's `duration` in the stream time base; else the gap to the
  next picture of the same stream; the last picture without a duration
  repeats the previous delay; a lone still has none), plus
  `Image::timestamp` (presentation time from the stream start),
  `Image::stream`, `raw_timing` and `time_base`. `encode_frames` maps
  delays back: pictures get cumulative `pts` in `SaveOptions::time_base`
  (default milliseconds; 1/100 s for `png`, the APNG delay unit) and,
  when the encoder returns one packet per picture, the packets are
  stamped with `pts` / `dts` / `duration` / `time_base`, so
  `decode(encode_frames(..))` reports the same delays. Pictures without a
  delay use `SaveOptions::default_delay` (`DEFAULT_DELAY` = 100 ms); a
  single still is written without a duration.
- A synthetic `OXIM` container + codec pair under `#[cfg(test)]`,
  registered like a sibling crate, exercising probe by magic and by
  extension hint, every packed and planar layout, `Pal8` + palette,
  multi-frame files, `max_frames`, metadata, lossless `encode` round
  trips for every layout, the pixel-format ladder against a codec that
  refuses layouts and against a muxer that refuses them, and the error
  variants of every failure path.

### Changed

- A probe miss is `UnknownFormat` (was `Core(FormatNotFound)`);
  `OpenOptions::max_frames(0)` behaves like `1`.
