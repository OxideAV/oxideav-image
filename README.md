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

if let Some(icc) = img.icc() { /* hand the profile to colour management */ }
let bare = encode(&ctx, img, "jpg", &SaveOptions::new().without_metadata())?;
let indexed = Image::from_indexed(2, 1, vec![0, 1], &[[255, 0, 0, 255], [0, 0, 0, 0]])?;
assert_eq!(indexed.to_rgba8()?[7], 0);           // palette alpha reaches the RGBA view
```

## API

| Item | Role |
|---|---|
| `open(&ctx, path)` / `open_with(.., &OpenOptions)` | Probe the file (extension as hint), open its demuxer, decode every video stream → `ImageFile`. |
| `decode_bytes(&ctx, &[u8])` / `decode_vec(&ctx, Vec<u8>)` / `decode_reader(&ctx, Box<dyn ReadSeek>, &OpenOptions)` | Same from memory (`decode_vec` without a copy) or any seekable reader. `*_with` variants take `OpenOptions`. |
| `ImageFile` | `container()`, `metadata()`, `primary()`, `frames()`, `frames_mut()`, `iter()`, `into_frames()`, `into_primary()`, `len()`; `IntoIterator` for `ImageFile` and `&ImageFile`. Metadata: `blobs()`, `stream_blobs(i)`, `blob(&kind)`, `icc()`, `exif()`, `xmp()`, `iptc()`, `has_metadata()`, `retain_blobs(f)`, `strip_metadata()` (see *Metadata*). |
| `Image` | A `VideoFrame` + `CodecParameters`: `width` / `height` / `format` / `color_signal` / `palette`; `palette_alpha`, `palette_rgba(i)`, `palette_entries`, `has_palette_alpha`, `has_alpha` (see *Palette alpha*), `is_opaque` / `has_transparency` (the alpha samples, see *Pixel-format ladder*); `delay`, `timestamp`, `stream`, `index`, `raw_timing`, `time_base`; `to_format(PixelFormat)`, `to_gray8`, `to_rgb8`, `to_rgba8`, `to_gray16`, `to_rgb16`, `to_rgba16`, `to_packed`, `as_packed`, `into_raw`, `crop`, `with_delay`, `with_color_signal`, `with_palette_rgba` / `with_palette` / `with_palette_alpha`, `frame`, `params`, `into_video_frame`. Metadata: `frame_blobs`, `stream_blobs`, `blobs`, `blob(&kind)`, `blobs_of(&kind)`, `icc`, `exif`, `xmp`, `iptc`, `has_metadata`, `with_blob(s)` / `set_blobs` / `push_blob` / `with_stream_blobs` / `take_blobs` / `retain_blobs` / `strip_metadata` / `without_metadata`. |
| `Image::from_rgb8` / `from_rgba8` / `from_indexed` / `from_raw` / `from_planes` / `from_video_frame` | Fallible constructors: geometry, plane count, stride and length are validated; `from_indexed` takes RGBA palette entries. |
| `encode(&ctx, &Image, "png", &SaveOptions)` / `encode_frames(&ctx, &[Image], ..)` / `save(&ctx, &Image, path, ..)` | Encoder + muxer by container name or extension; see *Format resolution*, *Pixel-format ladder* and *Geometry*. |
| `encoder_options(&ctx, format, &SaveOptions)` | The option schema of the encoder `encode` would use (names for `SaveOptions::with_option`). |
| `can_encode(&ctx, format)` / `resolve_target(&ctx, format, &SaveOptions)` | "Will this save?" before decoding: `can_encode` is a yes / no; `resolve_target` returns the `EncodeTarget` (`format()`, `container()` / `is_codec_only()`, `codec()`, `accepted_pixel_formats()`, `options()`, `ladder(src, &opts)` / `ladder_for(&image, &opts)`, `metadata_support()`) or the error `encode` would return before looking at a picture. |
| `OpenOptions` | `execution` (`with_execution_context(ctx)` / `with_threads(n)` / `serial()`, see *Execution budget*), `max_frames`, `ext_hint`, `decoder_options`, `max_pixels`, `allow_containers` / `deny_containers` / `allow_codecs` / `deny_codecs`, `strip_metadata` / `strip_blobs` (`without_metadata()`, `with_stripped_blobs(kinds)`). |
| `SaveOptions` | `execution` (`with_execution_context(ctx)` / `with_threads(n)` / `serial()`), `quality`, `pixel_format`, `codec`, `options`, `time_base`, `default_delay`, `blobs` (`with_blob(s)`), `strip_metadata` / `strip_blobs` (`without_metadata()`, `with_stripped_blobs(kinds)`). |
| `BlobKind` / `MetadataBlob` / `MetadataSupport` | The framework's blob types, re-exported; `MetadataSupport` is what `EncodeTarget::metadata_support()` answers. |
| `ImageError` (`Error`) | `#[non_exhaustive]`: see *Errors*. |

All conversions go through `oxideav-pixfmt`; `to_gray8` / `to_rgb8` /
`to_rgba8` / `to_gray16` / `to_rgb16` / `to_rgba16` return rows without
padding. The colour signal used for YCbCr ↔ RGB is the frame's own
record, else the stream-level one from the container, else pixfmt's
defaults.

**16-bit views (`to_gray16` / `to_rgb16` / `to_rgba16`)** are the
lossless path for texture consumers. The depth rule: a 16-bit source is
returned exactly (its samples are never requantised, whatever the
layout hop — grey ↔ RGB ↔ RGBA at 16 bits stage only through 16-bit
pivots, never through an 8-bit one); an 8-bit sample `s` widens to
`s × 257` (`0 → 0`, `255 → 65535`, every value exactly representable
back); 10- / 12-bit samples scale to the 16-bit full range the same
way; deep YCbCr goes through the colour matrix at 16-bit precision.
Grey of a colour source is pixfmt's grey derivation (the rounded mean
of R, G, B) at the view's depth. Alpha is opaque (`65535`) when the
source has none.

### Format resolution

- **Opening:** the registry's probe (magic bytes, extension hint as a
  tie-breaker) picks the container; every video stream gets the
  registry's first decoder; other streams are skipped. Pictures come
  back in packet order; `Image::stream()` says which stream each came
  from (a HEIF with a primary item and a sequence track yields the
  still from stream 0 and the track's frames from stream 1). **The
  decoder's word on the layout wins:** a picture is labelled with
  `Decoder::output_pixel_format()` when the decoder reports one (a
  hardware decoder's negotiated layout, a codec that reads its
  configuration record), else with the stream's `pixel_format`; the
  gateway never reports a layout the decoder did not emit, and a
  stream with no label opens when its decoder knows the layout. The
  relabelled parameters are shared by the stream's pictures as usual
  (`Image::params().pixel_format` says what the picture is).
- **Saving:** the format name or extension resolves to a container with
  a muxer. The payload codec is the container's default: the codec of
  the same name (png, bmp, tiff, heif, …) or the few that differ (`jpeg`
  → `mjpeg`, `dcx` → `pcx`, `ani` / `cur` → `ico`, `svgz` → `svg`,
  `iff_*` → `ilbm`); `SaveOptions::codec` overrides it (`"hevc"` or
  `"av1"` for a HEIF sequence track, for instance). **Ruling (round
  476):** the requested format name picks the codec when an encoder of
  that name exists on the resolved container — a muxer serving several
  brands through several extensions (`heic` / `avif` / `heif` on the
  heif muxer) receives the brand asked for: `encode(img, "avif", ..)`
  uses codec `avif`, `"heic"` / `"heif"` the container's default
  `heif`; `EncodeTarget::codec()` reports the chosen id. No encoder of
  the requested name → the table above, then the container's own name.
- **Asking first:** `can_encode(&ctx, "webp")` says whether a name or
  extension resolves to something writable at all; `resolve_target`
  tells which container and encoder, the layouts the encoder declares
  and the ladder a given source layout would walk — so a converter can
  refuse a target before decoding a large source, and a renderer can
  pick the output layout it will produce. Neither touches pixels.
- **Codec-only formats:** some image crates register a codec and an
  extension but no container (qoi, webp, gif, avif, openexr, pict, icer,
  jpeg2000, jpegxs, jpegxl). `encode` / `save` write the encoder's
  single packet as the file for them (prefixed encoder variants resolve
  too: `webp` → `webp_vp8l`, or `webp_vp8` when a quality is asked).
  Opening such a file through the registry is not possible until the
  crate grows a demuxer; `UnknownFormat` says so.

### Pixel-format ladder

`encode` hands the encoder the picture's own layout first and, when the
encoder or the muxer refuses it, steps through other layouts pixfmt
can reach from it. The order is a rule, not a table:

1. **colour-model distance** — the source's own model (RGB or YCbCr);
   then the other additive model; then CMYK (a four-ink layout for a
   three-channel picture is a last resort, never a tie-breaker on
   channel count: it is offered only when the source *is* CMYK or
   nothing closer is accepted); a palette rung (`Pal8`, a quantisation)
   after CMYK; grey **last** — only when the source is grey (then it
   comes first, followed by RGB, which is exact for grey, then YCbCr,
   then CMYK) or when nothing colour was accepted;
2. within a model, the source's bit tier first;
3. **transparency** as the pixels have it: a source with something
   transparent (an alpha sample below full, or a palette entry that is
   not opaque — `Image::has_transparency`) keeps alpha-carrying rungs
   ahead; a source in an alpha layout whose alpha is all full
   (`Image::is_opaque`) is offered the opaque layout of its model,
   tier, chroma fidelity and packing *before itself* (an opaque `Rgba`
   picture saves as `Rgb24`, an opaque `Yuva420P` as `Yuv420P`), then
   its own layout, then the rest; a source without alpha is never
   offered an alpha rung ahead of an opaque one;
4. YCbCr rungs at or below the source by decreasing chroma fidelity,
   then above it, closest first (a 4:2:0 source offers 4:2:2 before
   4:4:4);
5. packed before planar (`Rgb24` before `Gbrp8`).

An encoder that declares its accepted layouts
(`CodecCapabilities::accepted_pixel_formats`) is offered exactly those,
sorted by the same rule (the source itself only when it is declared).
One that declares nothing — a video codec taking 4:2:0 / 4:4:4 / 4:0:0,
say — gets the fixed rungs `Rgba`, `Rgb24`, `Rgba64Le`, `Rgb48Le`,
`Yuva444P`, `Yuva420P`, `Yuv444P`, `Yuv422P`, `Yuv420P`, `Yuv444P10Le`,
`Yuv420P10Le`, `Yuv444P16Le`, `Ya8`, `Gray8`, `Gray16Le`, sorted the same
way (no CMYK, no palette); an RGB picture therefore reaches such an
encoder as 4:4:4 YCbCr, never as grey. `SaveOptions::pixel_format`
replaces the ladder by a single layout. When every rung fails for the
same reason (an unknown option, a muxer refusing the stream) that
error is returned unchanged; otherwise `Unsupported` lists what each
rung hit.

`EncodeTarget::ladder(src, &opts)` is the rule applied to a layout
alone (transparency read from the layout); `ladder_for(&image, &opts)`
applies it to a picture, alpha samples and palette record read.

**Ruling (round 478).** An `Rgba` source saved as JPEG arrived as a
four-component CMYK JPEG: `Cmyk` and `Rgb24` tied as "RGB family, no
alpha" and sorted by name. Colour-model distance now decides before
anything else, an alpha source whose alpha is all `255` prefers the
opaque three-channel layout of its model, and CMYK is offered only
after every additive layout — pinned against a JPEG-like declared set
(full-range and studio YCbCr, grey 8 / 16, RGB, planar GBR, CMYK) in
the unit tests.

### Options

`SaveOptions::quality` is forwarded as the encoder's `"quality"` option
only when the encoder's declared schema has one (encoders parse their
options strictly; a lossless encoder would reject the unknown name).
`SaveOptions::with_option(name, value)` is forwarded verbatim; names
come from `encoder_options`. `OpenOptions::with_decoder_option` does the
same on the decoding side.

### Execution budget

Every decoder and encoder the gateway creates is handed an
`oxideav_core::ExecutionContext` through `set_execution_context`
(before its first packet or frame — the framework's threading
contract), so budgeted codecs (HEIF / AVIF / AV1 / H.265 tile- and
slice-parallel decodes) parallelise through the gateway. The budget
is `OpenOptions::execution` / `SaveOptions::execution`: `None` — the
default — is the host's parallelism (`ExecutionContext::auto()`,
`std::thread::available_parallelism`), `with_threads(n)` caps it,
`serial()` pins the codec to one worker, `with_execution_context(ctx)`
hands one through; `execution_context()` tells what the codecs will
get. Where a codec's declared option schema has a `threads` option the
caller did not set, the same budget is forwarded through that option
as well, so a codec that reads its budget from an option rather than
the context still gets it; a caller-set `threads` option wins on that
channel. A codec with no internal parallelism ignores the budget, as
the contract allows.

Measured on a 12 MP iPhone HEIC (`docs/image/heif/device-samples`,
HEVC still, 4:2:0) opened through the gateway on an 8-core host: see
*Timing* in the CHANGELOG entry for round 478 — the serial decode
versus the budgeted one.

### Metadata

Opaque payloads — ICC profiles, Exif, XMP, IPTC, anything a format
embeds as bytes it does not interpret — are framework `MetadataBlob`s
(`oxideav_core::blob`, core 0.1.40: a `BlobKind` name, an optional
media type and the bytes, with a format-neutral payload convention per
well-known kind so a blob lifted from one format drops into another).
They ride at two levels:

- **stream level** — `CodecParameters::blobs`, as the demuxer declared
  them for the stream: the file's profile, its Exif, its XMP.
  `ImageFile::blobs()` / `icc()` / `exif()` / `xmp()` / `iptc()` /
  `blob(&kind)` read the primary picture's stream; `stream_blobs(i)` any
  stream.
- **per picture** — the frame's blobs record (`VideoFrame::blobs`),
  lifted into `Image::frame_blobs()` when the picture is built: a
  multi-page TIFF's per-page Exif, a HEIF burst item's own profile.

**Precedence (ruling):** `Image::blobs()` / `blob(&kind)` /
`blobs_of(&kind)` / `icc()` and friends return the frame's blobs first,
then the stream blobs of kinds the frame does not carry — a frame blob
overrides every stream blob of the same kind. `Image::frame()` hands
back the frame without its blobs record; `into_video_frame` re-attaches
it, so a pipeline receives exactly what the decoder produced.

**Opening:** `OpenOptions::without_metadata()` (`strip_metadata`) drops
every blob at both levels before the decoder, the pictures or a later
`encode` see them; `with_stripped_blobs([BlobKind::EXIF])` drops kinds.
`ImageFile::retain_blobs(f)` / `strip_metadata()` do the same to an
open file. The container's string metadata (`ImageFile::metadata()`) is
not a blob and is never stripped. The pictures of a stream share its
parameters (copy on write), so a long animation or a many-page document
holds the stream's profile once.

**Saving:** `encode` / `encode_frames` hand blobs to the encoder at both
levels. The encoder's `CodecParameters::blobs` get the first picture's
stream blobs — and, for a single-picture encode, its frame blobs ahead
of them (a still-format encoder reads the stream parameters, and the
picture's own metadata is the file's) — and every frame carries its own
frame blobs on its record. `SaveOptions::with_blob(b)` writes `b` at the
stream level and replaces every source blob of its kind **at both
levels** — the stream's and every picture's frame record (set a
profile, swap the XMP packet): a blob set on the options is the one
that gets written, period, so a reopen cannot let a same-kind frame
blob override it (ruling, round 478; until then only the stream level
was replaced). `without_metadata()` / `with_stripped_blobs(kinds)`
withhold the source's. Whether the format embeds a kind is the
encoder's business: `EncodeTarget::metadata_support()` is
`MetadataSupport::PassThrough` for every target today — the framework
has no per-kind encoder declaration (`Declared(kinds)` is reserved for
one) — and the *Survey* below says what each crate carries.

### Palette alpha

An indexed (`Pal8`) picture carries its colours in the frame's palette
record and, since core 0.1.40, their transparency in a palette-alpha
record beside it (one byte per entry, possibly shorter than the
palette; uncovered entries are opaque): GIF's transparent index, PNG
`tRNS` on colour type 3, TGA 32-bit colour maps. The gateway honours it:

- `Image::palette_alpha()`, `palette_rgba(i)` (alpha `255` when absent),
  `palette_entries()`; `has_palette_alpha()` is true when any entry is
  not `255`, and `has_alpha()` is true for an alpha layout or such a
  palette.
- `to_rgba8` / `to_rgba16` — and any alpha layout through `to_format` —
  expand the picture through `palette_rgba`: a transparent entry yields
  alpha `0`, opaque and uncovered ones `255`; an index past the palette
  is opaque black (as pixfmt expands it).
- `to_rgb8` / `to_rgb16` / `to_gray8` / `to_gray16` **drop** the alpha
  (ruling): the entry's stored RGB, never composited over anything.
- The ladder ranks the alpha-carrying rungs first for a palette with a
  non-opaque entry (`Rgba` before `Rgb24`, `Yuva444P` before `Yuv444P`),
  so an RGB-only encoder receives the transparency rather than a
  flattened picture; `EncodeTarget::ladder_for(&image, &opts)` previews
  what `encode` tries (`ladder(PixelFormat, ..)` reads the layout alone,
  and `Pal8` has no alpha of its own).
- `crop` keeps both records; `from_indexed(w, h, indices, &[[r, g, b,
  a]])` builds one, attaching the alpha record only when an entry is
  not opaque (no record means opaque).

### Geometry

`encode_frames` never resizes. Pictures of one size share an output
stream; every distinct size, in first-appearance order, is a further
video stream of its own — the shape ICO entries and EXR parts take
through the framework. A container whose muxer takes several streams
(ico, exr) writes the set; one whose muxer takes a single geometry
refuses it, and the gateway reports that as `Unsupported` naming the
geometries (`"container 'png' refused 2 picture geometries (16x16,
32x32): …"`). A codec-only format (one packet, no muxer) refuses mixed
geometries before anything is encoded. `Image::stream()` on the decoded
side tells which stream each picture came from.

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

`OpenOptions::allow_containers` / `deny_containers` (`Option<Vec>` =
any when `None`; `Vec`, deny wins; names compare case-insensitively)
are checked on the probe result before the demuxer parses the header;
`allow_codecs` / `deny_codecs` are checked on the demuxer's stream table
— every video stream's codec id — before any decoder is created. A
refused file costs one probe (containers) or one header parse (codecs)
and fails with `Restricted`; streams the gateway never decodes (audio,
data) are not checked. `oxideav-io` enforces the same lists ahead of
the gateway today; the gateway now offers them so io can hand its lists
through (`OpenOptions::with_allow_containers(..)` and friends) instead
of parsing the header itself.

### Errors

| Variant | When |
|---|---|
| `Io` | File or stream I/O failed. |
| `UnknownFormat` | No container probed the input, the name / extension matches nothing with a muxer, or the extension belongs to a codec-only crate with no demuxer. |
| `NoImage` | A container matched but holds no video stream with a registered decoder, or decoded no picture. |
| `Unsupported` | No encoder for the codec, no layout of the ladder accepted, mixed geometries into a one-geometry container or a codec-only format, a planar layout asked for as packed bytes, a `crop` origin off the chroma grid or on a bit-packed layout. |
| `InvalidData` | Caller data inconsistent with its geometry (buffer length, plane count, stride), a crop window outside the picture, a stream without a pixel format whose decoder reports none. |
| `LimitExceeded` | `max_pixels` would be exceeded. |
| `Restricted` | The probed container or a video stream's codec is refused by the allow / deny lists; nothing was decoded. |
| `Core` | A registry, demuxer, decoder, muxer or encoder error, unchanged. |

### What the gateway deliberately does not do

- **Format-specific depth.** HEIF item graphs, PNG chunks, TIFF pages
  by tag, EXR channels and the like stay under the format crates' own
  names (`oxideav_heif::HeifFile`, `oxideav_png::PngMetadata`, …). The
  gateway is the common floor.
- **Parsing metadata.** ICC / Exif / XMP / IPTC are carried as opaque
  blobs (see *Metadata*) and routed by kind; the gateway never reads
  inside them (orientation, colour management and tag readers are the
  consumer's). `ImageFile::metadata()` is the container's string pairs
  as the demuxer reports them.
- **Guessing.** A stream without a declared pixel format whose decoder
  reports none either, a frame whose plane count disagrees with the
  label, or a probe miss is an error, not an inference.

## Testing

Unit tests run against a synthetic `OXIM` container + codec pair
registered exactly like a sibling crate (probe by magic and by extension
hint, 14 packed and planar layouts, `Pal8` with its palette and its
alpha record, stream and frame blobs through open / encode with every
precedence and strip rule, the legacy adaptations, exact
16-bit views, multi-frame
timing in every mode, allow / deny lists, the ladder against a layout-refusing codec, an encoder that declares
no layouts (colour before grey) and a layout-refusing muxer, mixed
geometries into a multi-stream container and into a one-geometry one, options, limits, every error variant). The
umbrella's `crates/oxideav-tests/tests/image_gateway.rs` runs the gateway
over the real format crates through `oxideav_meta::register_all`.

## Survey: metadata and palette alpha across the image crates (round 476)

Read-only survey of the 25 registered image crates on 2026-10-15, the
day core 0.1.40 (blobs + palette alpha) was released. Legend — **L1**:
the crate's standalone `Metadata { icc, exif, xmp }` / `Palette`
carries it; **field**: the Layer-1 field exists but the format has no
native carriage the crate reads; **—**: the format has no such thing;
**blob**: pushed through the registry on `CodecParameters::blobs` or the
frame record; **legacy**: reaches the registry through another public
field; **lost**: carried at Layer 1 but dropped on the registry path.
Every crate has the contract's `Metadata` struct; **none pushes a blob
through the registry yet** (0 of 25), so today the gateway surfaces
blobs only from the synthetic fixture — the per-crate asks below are
the adoption wave.

| Format | ICC | Exif | XMP | Palette alpha | Registry today | Ask (for that crate's seat) |
|---|---|---|---|---|---|---|
| png | L1 `iCCP` | L1 `eXIf` | L1 `iTXt XML:com.adobe.xmp` | L1 `tRNS` → **legacy** (demuxer `extradata` = `IHDR ‖ len ‖ PLTE ‖ len ‖ tRNS`), **adapted** by the gateway | no blobs; decoder attaches the palette only | demuxer pushes ICC / Exif / XMP on the stream; decoder attaches the alpha record on `Pal8` frames; encoder reads `params.blobs` + the alpha record instead of the split-less `PLTE ‖ tRNS` extradata |
| mjpeg | L1 `APP2` | L1 `APP1` | L1 `APP1` | — | none | demuxer / decoder push the three; encoder writes them from `params.blobs` |
| tiff | L1 tag 34675 | L1 Exif IFD | L1 tag 700 | — (colour maps are opaque) | none | per-page blobs on each frame's record (pages differ), the first page's on the stream; encoder writes from both |
| gif | L1 `ICCRGBG1012` | L1 app ext | L1 `XMP DataXMP` | L1 transparent index → **lost** for stills (`Pal8` without alpha; `Rgba` only when the 256-entry table has no free slot); animations are composited `Rgba` | none | stills attach the alpha record (transparent index → alpha `0`); encoder maps a `Pal8` input with an alpha record to the GCE transparent index; blobs on the stream |
| webp | L1 `ICCP` | L1 `EXIF` | L1 `XMP ` | — | none through the registry (the direct factory `make_encoder_with_metadata` embeds metadata; the registry encoder does not) | codec-only: decoder puts the three on the frame record; registry encoder reads `params.blobs` |
| heif | L1 `colr` `prof` | L1 `Exif` item | L1 `mime` item | — | none | primary item's three on stream 0; per-item blobs on burst / sequence frames; writer embeds from `params.blobs` |
| avif | L1 | L1 | L1 | — | none | rides heif: same ask |
| jpegxl | L1 codestream ICC | L1 `Exif` box | L1 `xml ` box | — | none | codec-only: frame record; encoder from `params.blobs` |
| jpeg2000 | L1 `jp2h` `colr` | field | field | — (`pclr` opaque) | **legacy, not adaptable**: demuxer string `icc = present` | push the profile bytes as a blob instead of the marker |
| jpegxs | field | L1 file-format box | field | — | **legacy, not adaptable**: demuxer string `exif = present` | push the Exif bytes as a blob instead of the marker |
| openexr | — | — | — | — | none | none (header attributes are not a well-known kind; a `custom("exr-attributes")` blob is the crate's call) |
| bmp | L1 V5 embedded profile | — | — | — (`RGBQUAD.rgbReserved` is reserved, not alpha; the crate decodes entries opaque) | none | ICC on the stream; no palette-alpha ask |
| tga | — | — | — | L1 32-bit colour map entries → **lost** (registry `to_rgb()`) | none | attach the alpha record for 32-bit colour maps; encoder writes it back |
| pcx | — | — | — | — | none | none |
| pbm | — | — | — | — | none | none |
| farbfeld | — | — | — | — | none | none |
| wbmp | — | — | — | — | none | none |
| hdr | — | — | — | — | none | none |
| ico | via PNG entries (L1 passes `Metadata` to the png member) | same | same | — (every entry decodes to `Rgba`; PNG `tRNS` and the BMP AND mask are applied) | none | per-entry frame blobs from PNG entries |
| dds | — | — | — | — | none | none |
| pict | — | — | — | — | none | none |
| icer | — | — | — | — | none | none |
| iff | — | — | — | L1 transparent colour key → **lost** (registry palette RGB only) | none | attach the alpha record from the key; encoder maps it back |
| qoi | — | — | — | — | none | none |
| svg | — | field | field | — | none | none |

Totals: ICC natively carried at Layer 1 by 11 formats, Exif by 10, XMP by
9, palette alpha by 4 (png, gif, tga, iff); as blobs through the
registry: 0 / 0 / 0 / 0; legacy through the registry: 1 (png palette
alpha, adapted) + 2 markers without bytes (jpeg2000, jpegxs).

**Adaptations the gateway applies (read side, withdrawn once the crate
adopts the record):**

- `png` streams: the demuxer's `extradata` layout above is public and
  unambiguous (colour type at `IHDR[9]`, both lengths always present),
  and for colour type 3 the `tRNS` data is one alpha byte per palette
  entry in entry order — the palette-alpha record exactly. When a `Pal8`
  picture arrives with a palette and no alpha record, the gateway lifts
  the `tRNS` tail into one (a tail longer than the palette, or an empty
  one, is left alone).

No other legacy field is adapted: the `present` markers carry no bytes,
and the encoder-side `PLTE ‖ tRNS` blob of png has no split point.

**Rulings recorded this round** (for `IMAGE_CRATE_API.md`): the
precedence rule (frame blob overrides stream blob of the same kind); the
single-picture promotion (a still's frame blobs are also the stream's
when it is encoded alone); `SaveOptions::blobs` replace by kind; RGB and
grey views drop palette alpha without compositing; a palette with a
non-opaque entry counts as an alpha source for the ladder; no record
means opaque.

## License

MIT — Copyright (c) 2026 Karpelès Lab Inc.
