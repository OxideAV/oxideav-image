# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Metadata blobs surfaced** (core 0.1.40): `ImageFile::blobs()` /
  `stream_blobs(i)` / `blob(&kind)` / `icc()` / `exif()` / `xmp()` /
  `iptc()` / `has_metadata()` report the stream-level blobs the demuxer
  put on the stream's `CodecParameters`; `Image::frame_blobs()` (the
  frame's own record, lifted at construction) / `stream_blobs()` /
  `blobs()` / `blob(&kind)` / `blobs_of(&kind)` / `icc()` / `exif()` /
  `xmp()` / `iptc()` / `has_metadata()` apply the precedence rule — a
  frame blob overrides the stream blob(s) of the same kind. Builders
  `with_blob` / `with_blobs` / `set_blobs` / `push_blob` /
  `with_stream_blobs` / `take_blobs` / `retain_blobs` / `strip_metadata`
  / `without_metadata`; `into_video_frame` re-attaches the record;
  `to_format` and `crop` carry the blobs. `OpenOptions::strip_metadata`
  / `strip_blobs` (`with_metadata(bool)` / `without_metadata()` /
  `with_stripped_blobs(kinds)`) drop blobs at both levels while opening;
  `ImageFile::retain_blobs` / `strip_metadata` do the same after the
  fact. `BlobKind` / `MetadataBlob` are re-exported.
- **Metadata written**: `encode` / `encode_frames` hand the blobs to the
  encoder — stream-level ones on its `CodecParameters::blobs` (the first
  picture's stream blobs; for a single-picture encode its frame blobs
  too, frame first), per-picture ones on each frame's record.
  `SaveOptions::blobs` (`with_blob` / `with_blobs`) replace the source
  blobs of the same kind; `strip_metadata` / `strip_blobs`
  (`with_metadata(bool)` / `without_metadata()` /
  `with_stripped_blobs(kinds)`) withhold source blobs.
  `EncodeTarget::metadata_support()` → `MetadataSupport::PassThrough`
  (the framework has no per-kind encoder declaration yet; `Declared` is
  reserved for one).
- **Palette alpha honoured** (core 0.1.40): `Image::palette_alpha()` /
  `palette_rgba(i)` / `palette_entries()` / `has_palette_alpha()` /
  `has_alpha()`; `from_indexed(w, h, indices, &[[r, g, b, a]])`,
  `with_palette_rgba` / `with_palette` / `set_palette_alpha` /
  `with_palette_alpha`. `to_rgba8` / `to_rgba16` (and every alpha
  layout through `to_format`) expand a `Pal8` source through
  `palette_rgba`: a transparent entry yields alpha `0`, opaque and
  uncovered ones `255`; `to_rgb8` / `to_rgb16` / grey drop the alpha
  (no compositing). The ladder ranks alpha-carrying rungs first for a
  palette with a non-opaque entry (`EncodeTarget::ladder_for(&image,
  &opts)`); `crop` keeps the alpha record.
- **Legacy adaptation** (read side, withdrawn once the crate adopts the
  record): a `png` stream's `extradata` (`IHDR || len || PLTE || len ||
  tRNS`) tRNS tail becomes the palette-alpha record of a `Pal8` picture.
- README: *Metadata* and *Palette alpha* sections, the round-476 survey
  of the 25 image crates (ICC / Exif / XMP / palette alpha: blob, legacy
  or none, with the per-crate ask).

### Changed

- The pictures of one stream share that stream's `CodecParameters`
  (copy on write): a 40-page document with a 64 KiB profile holds it
  once, not once per page; `ImageFile::retain_blobs` filters each shared
  set once and keeps the sharing.

- The synthetic OXIM fixture carries stream blobs, frame blobs and the
  palette-alpha record, the way a sibling crate does after adopting core
  0.1.40. The deep-route unit test checks pixfmt's direct-step table
  instead of a pinned pair.

## [0.0.3](https://github.com/OxideAV/oxideav-image/compare/v0.0.2...v0.0.3) - 2026-10-14

### Other

- one stream per picture geometry — multi-stream muxers write the set, single-geometry ones refuse by type, never a resize
- :to_rgb16 / to_gray16: lossless 16-bit views; deep sources stage through 16-bit pivots, never an 8-bit one
- allow / deny lists for containers and codecs on OpenOptions, checked between probe and first decoder; ImageError::Restricted
- can_encode / resolve_target: answer "will this save?" through the registries without touching pixels
- colour before grey — source family by fidelity, other colour family, grey last; declared sets sorted the same way

### Added

- `can_encode(&ctx, format)` and `resolve_target(&ctx, format, &opts) ->
  EncodeTarget` answer "will this save?" without decoding: the resolved
  container (or codec-only), the encoder id, its declared accepted
  layouts, its option schema and the pixel-format ladder for a given
  source layout (`EncodeTarget::ladder`). `encoder_options` now rides
  `resolve_target`.
- `OpenOptions::allow_containers` / `deny_containers` / `allow_codecs` /
  `deny_codecs` (round-473 ruling), with `with_*` setters: containers are
  checked on the probe result before the header is parsed, codecs on the
  demuxer's stream table before any decoder exists, so a refused file
  costs one header parse at most. Refusals are the new
  `ImageError::Restricted`. The field shapes mirror `oxideav-io`'s
  (`Option<Vec<String>>` allow = any when `None`, `Vec<String>` deny,
  case-insensitive) so io can hand its lists through.
- `Image::to_rgb16` (`Rgb48Le`) and `Image::to_gray16` (`Gray16Le`)
  beside `to_rgba16`: the lossless 16-bit views for texture consumers.
  Rule: 16-bit sources come back exactly, 8-bit samples widen as
  `s × 257`, 10- / 12-bit scale to full range, deep YCbCr goes through
  the 16-bit matrix. `Image::to_format` now stages a deep source asked
  for a 16-bit view through 16-bit pivots when pixfmt has no direct
  step (pixfmt's own staging pivots `Gray16Le → Rgb48Le` and
  `Rgb48Le → Gray16Le` through `Gray8`, which narrowed).
- `encode_frames` with per-picture geometry: pictures of one size share
  an output stream, every further size is a stream of its own (the ICO /
  EXR shape), so a multi-stream muxer writes the set; a one-geometry
  muxer's refusal and a codec-only target become `Unsupported` naming
  the geometries — never a resize, never a panic.

### Changed

- Mixed geometries in `encode_frames` are `Unsupported` (from the muxer
  or the codec-only rule) instead of an up-front `InvalidData`.

### Fixed

- Pixel-format ladder: colour before grey (`IMAGE_CRATE_API.md` round-474
  ruling). An encoder that declares no accepted layouts is offered the
  source's own colour family, then the other colour family (`Yuv444P` /
  `Yuv422P` / `Yuv420P` for RGB sources), and `Gray8` only last; an RGB
  picture no longer arrives grey at a YCbCr-or-grey video encoder. An
  encoder that declares its layouts is offered exactly those, sorted by
  the same rule (source layout, own family by decreasing fidelity, other
  colour family, grey). The rule is stated in the README.

## [0.0.2](https://github.com/OxideAV/oxideav-image/compare/v0.0.1...v0.0.2) - 2026-10-05

### Other

- container default codec follows prefixed encoder families and wrapper names
- pictures on a 1/1 time base are untimed and carry no delay
- README in the contract's spirit: usage, API table, format resolution, ladder, options, timing rule, limits, error table, deliberate non-goals
- Fixes surfaced by the umbrella suite: stream colour signal into pixfmt, lone-picture delay, prefixed codec-only encoders, per-picture APNG retry, tolerant drains, UnknownFormat on demuxer-less extensions
- :quality is schema-gated; encoder_options() exposes the encoder's declared options
- Image API completion: crop on every byte-granular layout, to_gray8, from_planes, ImageFile iteration, decoder options, max_pixels budget, decode_vec
- Frame timing: delays from packet duration / pts deltas in the stream time base; encode_frames stamps pts, dts, duration back
- Synthetic OXIM registry fixture + 17 gateway tests; codec-only encode path; default-codec rule from the registry

### Fixed

- `encode` / `save` to a container whose encoder family is prefixed (`webp` → `webp_vp8l` / `webp_vp8`) or named differently (`jp2` / `jph` → `jpeg2000`, `jxs` → `jpegxs`) no longer needs `SaveOptions::codec`.
- Pictures on a `1/1` time base (the untimed-stream convention used by HEIF bursts, EXR parts, ICER bands, TIFF pages) report `delay() == None` instead of a one-second delay.

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
- `Image::to_format` (and every `to_*`) hands the stream-level colour
  signal to `oxideav-pixfmt` when the frame carries no record of its
  own, so YCbCr frames from a video codec inside a container (HEIF
  sequence tracks) convert with the container's matrix and range.
- Codec-only formats whose encoders are prefixed variants (`webp` →
  `webp_vp8l` / `webp_vp8`) resolve: the lossless one by default, a
  lossy one when a quality is asked for.
- When every layout of the ladder fails for the same reason the original
  error is returned unchanged; otherwise `Unsupported` lists what each
  attempt hit. Packets are drained leniently between frames (some image
  encoders answer `receive_packet` before `flush` with an error other
  than `NeedMore`); after `flush` an error following at least one
  delivered packet ends the stream, an error before any packet is fatal.
- Multi-picture encode re-encodes one picture per encoder instance when
  the encoder folded them into a single packet and the container has a
  muxer (png → APNG with per-picture delays through the muxer).
- `SaveOptions::quality` reaches the encoder only when its declared
  option schema has a `quality` field (encoders parse options strictly,
  so a lossless encoder would otherwise fail on the unknown name);
  `encoder_options(&ctx, format, &opts)` returns the schema of the
  encoder `encode` would use, for discovering `with_option` names.
- `Image::crop(x, y, w, h)` on every layout `PixelFormat` describes with
  whole bytes per sample position (chroma-subsampled layouts need the
  origin on the chroma grid; bit-packed mono and packed 4:2:2 are
  `Unsupported`), carrying the palette, colour-signal and
  significant-bits records over; `Image::to_gray8`;
  `Image::from_planes(width, height, format, planes)` validating plane
  count, stride and length against the format's geometry.
- `ImageFile::frames_mut` / `iter` and `IntoIterator` for `ImageFile`
  and `&ImageFile`.
- `OpenOptions::with_decoder_option(s)` (forwarded through
  `CodecParameters::options`) and `OpenOptions::with_max_pixels`, a
  budget over the decoded pixels of the whole file checked from the
  stream geometry before a decoder exists and again before each picture
  is kept; `ImageError::LimitExceeded` reports it.
- `decode_vec` / `decode_vec_with` take the buffer without copying;
  `decode_bytes(&[u8])` copies once because the demuxer needs an owned
  seekable reader.
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
- A stream with a single picture never reports a delay, whatever
  nominal duration its demuxer stamped.
