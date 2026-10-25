# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.6](https://github.com/OxideAV/oxideav-image/compare/v0.0.5...v0.0.6) - 2026-10-25

### Other

- Label pictures from the watched decoder report (Decoder::output_params), not a per-picture query
- Multi-frame layer model: page / dispose / blend / tick-rate record on Image, loop counts both ways, and the layers module (coalesce, compare-any/clear/overlay, optimize / optimize-frame / optimize-plus / optimize-transparency, remove-dups / remove-zero, flatten / mosaic / merge / trim-bounds, composite) pinned against magick 7.1.2 as a black box; layered lists are written coalesced
- edition 2024, MSRV 1.95

- edition 2024, MSRV 1.95

### Added

- Pictures are labelled from the stream parameters merged with the
  decoder's full report (oxideav-core 0.1.42 `Decoder::output_params`:
  layout, emitted width / height, colour signal) through one
  `OutputParamsWatch` per stream — primed before the first picture,
  re-read only when `output_params_generation` moves — instead of
  asking `output_pixel_format()` on every picture; a mid-stream change
  relabels the pictures from that one on (earlier pictures keep their
  parameter set). The picture budget counts the decoder-reported size.

- **Layer record on every `Image`** (`Page`, `Dispose`, `Blend`, all
  `#[non_exhaustive]`): `page` / `with_page` / `with_offset` /
  `offset` / `canvas_size` / `is_full_canvas`, `dispose` /
  `with_dispose`, `blend` / `with_blend`, `ticks_per_second` /
  `delay_ticks` / `with_delay_ticks`. Decoded pictures are full
  canvases with `Blend::Source`. `ImageFile::loop_count()` (plays,
  `0` = forever; a GIF's NETSCAPE2.0 repeat count reads `n + 1`) and
  `SaveOptions::with_loop_count(plays)` (the encoder's and the muxer
  stream's `loop_count` option, in each format's unit).
- **`layers` module**: `coalesce`, `apply_disposal`, `compare_any` /
  `deconstruct`, `compare_clear`, `compare_overlay`, `optimize`,
  `optimize_frame`, `optimize_plus`, `optimize_transparency`,
  `remove_dups`, `remove_zero`, `flatten`, `mosaic`, `merge`,
  `trim_bounds`, `composite_layers`, `layers(frames, LayerMethod, ..)`,
  `LayerOptions` (background). Geometry, disposal and pixels match
  `magick` 7.1.2 (black box) on 519 generated animations and 18 GIFs;
  the deliberate departures are listed in the README (*Layers*).
- `encode_frames` writes a layered list (sub-rectangle frames or
  frames disposing their rectangle) coalesced, as one animation at the
  canvas size, instead of one stream per frame geometry.

### Fixed

- A multi-picture encode picks its layout from every picture's
  transparency, not the first one's: an animation whose first frame is
  opaque no longer loses the alpha of later frames to an `Rgb24` rung.
- A zero packet duration is a zero delay when the timestamps agree
  (GIF Delay Time 0, APNG `delay_num` 0) instead of repeating the
  previous delay.

## [0.0.5](https://github.com/OxideAV/oxideav-image/compare/v0.0.4...v0.0.5) - 2026-10-21

### Other

- A caller-set threads option is the budget when no explicit one is given (one number on both channels); PNG-like and WebP-like declared sets pinned on the ladder
- support / can_open / codec_only_formats: the registry's answer for a name or extension; README codec-only list replaced by the registry survey (none of the 25 image crates is codec-only), budget timings recorded
- Execution budget on OpenOptions / SaveOptions (set_execution_context on every decoder and encoder, threads option forwarded where declared; default = host parallelism) and decoder output labels (Decoder::output_pixel_format wins over the stream label; unlabelled streams open when the decoder knows)
- :blobs replace by kind at both levels: the frame records handed to the encoder drop every kind the options set, so a same-kind frame blob cannot win on reopen
- colour-model distance before channel count, opaque alpha prefers the opaque layout, CMYK and palette rungs last — an opaque Rgba source saved as JPEG no longer becomes four-ink CMYK

### Added

- **Execution budget.** `OpenOptions::execution` / `SaveOptions::execution`
  (`with_execution_context(ctx)`, `with_threads(n)`, `serial()`,
  `execution_context()`): every decoder and encoder the gateway creates
  receives an `oxideav_core::ExecutionContext` through
  `set_execution_context` before its first packet / frame, so budgeted
  codecs (heif, avif, av1, h265) parallelise through the gateway. The
  default is the host's parallelism (`ExecutionContext::auto()`). Where
  the codec's declared option schema has a `threads` option the caller
  did not set, the budget is forwarded through it too (a caller-set
  option wins on that channel, and with no explicit budget it *is* the
  budget, so a codec reading both channels hears one number).
  `ExecutionContext` is re-exported.
  *Timing* (gateway `open`, best of 3, 18-logical-core host,
  `docs/image/heif/device-samples`): the 12 MP `iphone_13_pro_max.HEIC`
  (48 HEVC tiles) 0.481 s serial → 0.271 s at 2 workers → 0.174 s at 4
  → 0.103 s at 8 → 0.077 s at the host default; the 8.5 MP
  `IMG_5195.HEIC` (42 tiles) 0.274 s → 0.146 s → 0.086 s → 0.050 s →
  0.036 s. Before this release the gateway never set a budget, so every
  open ran serial.
- **Decoder output labels.** A picture is labelled with the decoder's
  `output_pixel_format()` when it reports one, else the stream's
  `pixel_format`: the gateway never reports a layout the decoder did
  not emit (hardware decoders negotiate their own; some codecs read
  theirs from the configuration record). A stream without a label
  opens when its decoder knows the layout — it was `InvalidData`; it
  still is when neither knows. The relabelled parameters stay shared by
  the stream's pictures.
- `support(&ctx, format)` → `FormatSupport` (`container()`,
  `has_demuxer()` / `has_muxer()` / `has_decoder()` / `has_encoder()`,
  `can_open()` / `can_save()`, `is_codec_only()` / `is_unknown()`),
  `can_open(&ctx, format)` and `codec_only_formats(&ctx, names)`: the
  registry's answer for a name or extension without touching a file, so
  consumers and the umbrella tests survey which formats are codec-only
  instead of trusting a hand-maintained list.
- `Image::is_opaque()` / `has_transparency()`: the picture's real
  transparency — every alpha sample at full scale (8 / 16-bit, the
  layout's maximum code for 10–14-bit planar, `≥ 1.0` float) and every
  palette entry opaque, or not. `has_alpha()` keeps reading the layout.

### Fixed

- README: the "codec-only formats" list (qoi, webp, gif, avif, openexr,
  pict, icer, jpeg2000, jpegxs, jpegxl) was stale — every one of the 25
  image crates registers a demuxer and a muxer today (gif since 0.0.14;
  `avif` is a codec on the `heif` container); the paragraph now points
  at `support` and records the round-478 registry survey. The webp /
  jpegxl survey cells said "codec-only" too. The round-476 survey
  carries a round-478 update for the six crates that adopted blobs.
- **Ladder: colour-model distance before channel count.** An `Rgba`
  source saved to an encoder that declares CMYK among its layouts (a
  JPEG encoder) was offered `Cmyk` before `Rgb24` — both ranked "RGB
  family, no alpha" and sorted by name — so an opaque RGBA picture
  became a four-component CMYK JPEG. The rank is now: the source's
  model, the other additive model, CMYK (only for a CMYK source or when
  nothing closer is accepted), a palette rung (a quantisation, after
  CMYK), grey last; within a model the source's bit tier, then
  transparency, then chroma fidelity, then packing. A palette rung used
  to tie with `Rgb24` by name as well.
- **Ladder: an alpha source whose alpha is all full prefers the opaque
  layout of its model** (`EncodeTarget::ladder_for`, `encode`): an
  opaque `Rgba` picture is offered `Rgb24` before `Rgba`, an opaque
  `Yuva420P` picture `Yuv420P` first; a picture with any translucent
  sample keeps its alpha layout first. `EncodeTarget::ladder` (layout
  only, no pixels) is unchanged.
- `Image::as_packed` / `to_packed` serve `Cmyk` / `CmykInverted` (four
  bytes per pixel) — they answered `None` / `Unsupported`.
- **`SaveOptions::blobs` replace by kind at both levels.** A blob set on
  the options replaced the source blob of its kind on the encoder's
  stream parameters only; a picture's frame record kept its own blob of
  that kind, which won on reopen (frame over stream). The frame records
  handed to the encoder now drop every kind the options set, so the
  options' blob is the one written, period.

## [0.0.4](https://github.com/OxideAV/oxideav-image/compare/v0.0.3...v0.0.4) - 2026-10-17

### Other

- the requested format name picks the codec on a multi-brand container — "avif" on the heif muxer selects the avif encoder instead of the container's default
- Pictures of one stream share its CodecParameters (copy on write): a stream's blobs are held once however many pages or frames; ImageFile::retain_blobs filters each shared set once
- :retain_blobs / strip_metadata: post-open stripping on every stream and every picture, the counterpart of the OpenOptions rules
- Metadata and Palette alpha sections, the round-476 survey of the 25 image crates (ICC / Exif / XMP / palette alpha: Layer 1, legacy or none; 0 of 25 push blobs yet) with the per-crate ask, the png tRNS adaptation and the rulings
- core 0.1.40 adoption: metadata blobs surfaced and written through the gateway, palette alpha honoured in every RGBA view and in the ladder

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

### Fixed

- `encode(img, "avif", ..)` wrote the heif container's default codec:
  the requested name was lost once the extension resolved to the
  container. The requested (lower-cased, dot-stripped) name now rides
  with the resolved container and picks a registered encoder of that
  name first (`avif` → codec `avif`; `heic` / `heif` → `heif`), before
  the known-codec table and the container's own name;
  `EncodeTarget::codec()` reports it and `format()` is the requested
  name.

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
