//! The per-picture layer record of a multi-frame image: where the
//! picture sits on the animation's virtual canvas, how it is drawn onto
//! it and what happens to its rectangle afterwards.
//!
//! The model is the one every animated image format shares, named
//! after the fields the format specifications define:
//!
//! | record | GIF89a | APNG (PNG 3rd ed. §11.3.6) | WebP (RFC 9649 §2.7.1.1) | JPEG XL (18181-1 Annex F) |
//! |---|---|---|---|---|
//! | [`Page`] size | Logical Screen Width/Height (§18) | IHDR width/height | VP8X canvas | image size |
//! | [`Page`] offset | Image Left/Top Position (§20) | fcTL `x_offset` / `y_offset` | ANMF Frame X/Y × 2 | `have_crop` x0 / y0 |
//! | [`Dispose`] | GCE Disposal Method (§23.c.iv) | fcTL `dispose_op` | ANMF D bit | — (each frame blends onto the previous canvas) |
//! | [`Blend`] | Transparency Index = over | fcTL `blend_op` | ANMF B bit | BlendingInfo `mode` (kReplace / kBlend) |
//! | delay | GCE Delay Time, 1/100 s | `delay_num` / `delay_den` | Frame Duration, ms | `duration` ticks at `tps_numerator / tps_denominator` |
//!
//! A picture whose record was never set is its own canvas: page
//! `0×0+0+0` (the canvas is the picture's size), [`Dispose::Unspecified`]
//! and [`Blend::Over`]. That is exactly what a decoder that composites
//! its animation (every format crate does, through the registry) hands
//! out, so [`crate::layers`] treats opened files and caller-built layer
//! lists alike.

/// The virtual canvas of an animation and a picture's place on it.
///
/// `width × height` is the canvas (`0` = not declared: the picture's
/// own size stands in, see [`crate::Image::canvas_size`]); `x`, `y` is
/// the picture's top-left corner on it, which may be negative or put the
/// picture partly outside the canvas (the parts outside are clipped when
/// the layers are composited).
///
/// `#[non_exhaustive]`: build it with [`new`](Self::new) and
/// [`with_offset`](Self::with_offset), read the fields directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Page {
    /// Canvas width in pixels; `0` = undeclared.
    pub width: u32,
    /// Canvas height in pixels; `0` = undeclared.
    pub height: u32,
    /// Column of the picture's left edge on the canvas.
    pub x: i32,
    /// Row of the picture's top edge on the canvas.
    pub y: i32,
}

impl Page {
    /// A `width × height` canvas with the picture at `+0+0`.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            x: 0,
            y: 0,
        }
    }

    /// The same canvas with the picture at `(x, y)`.
    pub fn with_offset(mut self, x: i32, y: i32) -> Self {
        self.x = x;
        self.y = y;
        self
    }

    /// The same offset on a `width × height` canvas.
    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }
}

impl std::fmt::Display for Page {
    /// `WxH+X+Y` (offsets signed: `40x30-2+5`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}{:+}{:+}", self.width, self.height, self.x, self.y)
    }
}

/// What happens to a picture's rectangle on the canvas once its delay
/// has elapsed, before the next picture is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Dispose {
    /// Not specified (GIF89a §23.c.iv value 0, "the decoder is not
    /// required to take any action"): handled as [`None`](Self::None).
    #[default]
    Unspecified,
    /// Leave the picture in place (GIF value 1, APNG
    /// `APNG_DISPOSE_OP_NONE`, WebP D = 0).
    None,
    /// Restore the rectangle to the background (GIF value 2, APNG
    /// `APNG_DISPOSE_OP_BACKGROUND` — fully transparent black, WebP
    /// D = 1). [`crate::layers::LayerOptions::background`] is the colour
    /// used; transparent black by default.
    Background,
    /// Restore the rectangle to what it held before the picture was
    /// drawn (GIF value 3, APNG `APNG_DISPOSE_OP_PREVIOUS`). On the
    /// first picture that is the initial canvas, which is the
    /// background — the APNG rule "treated as
    /// `APNG_DISPOSE_OP_BACKGROUND`".
    Previous,
}

impl Dispose {
    /// The GIF89a §23.c.iv Disposal Method code (0–3).
    pub fn gif_code(self) -> u8 {
        match self {
            Dispose::Unspecified => 0,
            Dispose::None => 1,
            Dispose::Background => 2,
            Dispose::Previous => 3,
        }
    }

    /// The disposal a GIF89a §23.c.iv code names; codes 4–7 ("to be
    /// defined") are [`Unspecified`](Self::Unspecified).
    pub fn from_gif_code(code: u8) -> Self {
        match code {
            1 => Dispose::None,
            2 => Dispose::Background,
            3 => Dispose::Previous,
            _ => Dispose::Unspecified,
        }
    }

    /// The name ImageMagick's `%D` / `-dispose` use (`Undefined`,
    /// `None`, `Background`, `Previous`).
    pub fn name(self) -> &'static str {
        match self {
            Dispose::Unspecified => "Undefined",
            Dispose::None => "None",
            Dispose::Background => "Background",
            Dispose::Previous => "Previous",
        }
    }
}

/// How a picture's pixels are combined with the canvas under its
/// rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Blend {
    /// Alpha-composite the picture over the canvas (APNG
    /// `APNG_BLEND_OP_OVER`, WebP B = 0, JPEG XL kBlend; a GIF
    /// Transparency Index leaves the canvas pixel as it is) with the
    /// non-premultiplied formula of RFC 9649 §2.7.1.1 / PNG §13.16.
    #[default]
    Over,
    /// Replace the rectangle, alpha included (APNG
    /// `APNG_BLEND_OP_SOURCE`, WebP B = 1, JPEG XL kReplace).
    Source,
}

/// The layer fields an [`crate::Image`] carries (kept together so the
/// image's constructors stay one line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Layer {
    pub(crate) page: Page,
    pub(crate) dispose: Dispose,
    pub(crate) blend: Blend,
    /// Ticks per second of [`crate::Image::delay_ticks`]; `None` = the
    /// stream time base when it is a whole number of ticks per second,
    /// else 100.
    pub(crate) ticks_per_second: Option<u32>,
}

/// `true` when `frames` (more than one) describe a layered animation
/// rather than a run of complete pictures: some frame is a sub-rectangle
/// of its canvas, or disposes its rectangle.
pub(crate) fn is_layered(frames: &[crate::Image]) -> bool {
    frames.len() > 1
        && frames.iter().any(|f| {
            !f.is_full_canvas() || matches!(f.dispose(), Dispose::Background | Dispose::Previous)
        })
}

/// The container string-metadata key an animation's background colour
/// travels under (`Demuxer::metadata` on the way in,
/// `Muxer::set_metadata` on the way out): straight RGBA in
/// hexadecimal, `#rrggbbaa` at 8 bits a channel or
/// `#rrrrggggbbbbaaaa` at 16.
pub const BACKGROUND_COLOR_KEY: &str = "background_color";

/// Read a [`BACKGROUND_COLOR_KEY`] value: `#rrggbb`, `#rrggbbaa` or
/// `#rrrrggggbbbbaaaa` (the `#` optional, either case), as straight
/// 16-bit RGBA. `None` for anything else.
pub fn parse_background(value: &str) -> Option<[u16; 4]> {
    let hex = value.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize, digits: usize| u16::from_str_radix(&hex[i * digits..][..digits], 16);
    match hex.len() {
        6 | 8 => {
            let mut out = [u16::MAX; 4];
            for (i, o) in out.iter_mut().enumerate().take(hex.len() / 2) {
                *o = channel(i, 2).ok()? * 257;
            }
            Some(out)
        }
        16 => {
            let mut out = [0u16; 4];
            for (i, o) in out.iter_mut().enumerate() {
                *o = channel(i, 4).ok()?;
            }
            Some(out)
        }
        _ => None,
    }
}

/// Write a [`BACKGROUND_COLOR_KEY`] value: `#rrggbbaa` when every
/// channel is an 8-bit value widened (`v × 257`), else
/// `#rrrrggggbbbbaaaa`.
pub fn format_background(rgba: [u16; 4]) -> String {
    if rgba.iter().all(|c| c % 257 == 0) {
        let [r, g, b, a] = rgba.map(|c| c / 257);
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    } else {
        let [r, g, b, a] = rgba;
        format!("#{r:04x}{g:04x}{b:04x}{a:04x}")
    }
}

/// A frame's layer fields as they travel through the registry: a
/// per-frame metadata blob of kind [`LayerRecord::KIND`] on the
/// `VideoFrame` a decoder hands out (a frame that is a layer of its
/// stream's canvas rather than the whole canvas) and on the frame an
/// encoder is handed (when it declares the kind among its
/// `CodecCapabilities::embedded_blob_kinds`).
///
/// The stream's `width × height` is the canvas; the record states the
/// picture's own size — a frame's planes hold `width × height` of the
/// record, not of the stream — its offset on the canvas, its disposal
/// and its blending. The delay is the packet's duration, as for any
/// frame.
///
/// Wire form ([`to_bytes`](Self::to_bytes), 28 bytes, little-endian):
///
/// | bytes | field |
/// |---|---|
/// | 0 | version, `1` |
/// | 1 | disposal: `0` unspecified, `1` none, `2` background, `3` previous |
/// | 2 | blending: `0` over, `1` source |
/// | 3 | reserved, `0` |
/// | 4..8, 8..12 | canvas width, height (`u32`; `0` = the stream's) |
/// | 12..16, 16..20 | offset x, y (`i32`) |
/// | 20..24, 24..28 | picture width, height (`u32`) |
///
/// `#[non_exhaustive]`: build it with [`new`](Self::new) /
/// [`of`](Self::of) and the `with_*` builders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct LayerRecord {
    /// The canvas and the picture's offset on it.
    pub page: Page,
    /// The picture's own width in pixels.
    pub width: u32,
    /// The picture's own height in pixels.
    pub height: u32,
    /// What happens to the rectangle after the delay.
    pub dispose: Dispose,
    /// How the picture is combined with the canvas.
    pub blend: Blend,
}

impl LayerRecord {
    /// The metadata-blob kind of the record.
    pub const KIND: &'static str = "image-layer";
    /// Length of the wire form.
    pub const WIRE_LEN: usize = 28;

    /// A `width × height` picture on `page`, [`Dispose::Unspecified`],
    /// [`Blend::Over`].
    pub fn new(page: Page, width: u32, height: u32) -> Self {
        Self {
            page,
            width,
            height,
            dispose: Dispose::Unspecified,
            blend: Blend::Over,
        }
    }

    /// The record of `image`: its size, its page with the canvas made
    /// explicit, its disposal and blending.
    pub fn of(image: &crate::Image) -> Self {
        let (cw, ch) = image.canvas_size();
        let (x, y) = image.offset();
        Self {
            page: Page::new(cw, ch).with_offset(x, y),
            width: image.width(),
            height: image.height(),
            dispose: image.dispose(),
            blend: image.blend(),
        }
    }

    /// The same record with another disposal.
    pub fn with_dispose(mut self, dispose: Dispose) -> Self {
        self.dispose = dispose;
        self
    }

    /// The same record with another blending.
    pub fn with_blend(mut self, blend: Blend) -> Self {
        self.blend = blend;
        self
    }

    /// The blob kind, as a `BlobKind`.
    pub fn kind() -> oxideav_core::BlobKind {
        oxideav_core::BlobKind::from_static(Self::KIND)
    }

    /// The wire form.
    pub fn to_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0] = 1;
        out[1] = self.dispose.gif_code();
        out[2] = u8::from(self.blend == Blend::Source);
        let words = [
            self.page.width.to_le_bytes(),
            self.page.height.to_le_bytes(),
            self.page.x.to_le_bytes(),
            self.page.y.to_le_bytes(),
            self.width.to_le_bytes(),
            self.height.to_le_bytes(),
        ];
        for (o, w) in out[4..].as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *o = w;
        }
        out
    }

    /// Read the wire form. `None` for another length or version, an
    /// unknown disposal or blending code, a non-zero reserved byte or a
    /// picture with a zero side.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::WIRE_LEN] = bytes.try_into().ok()?;
        if bytes[0] != 1 || bytes[1] > 3 || bytes[2] > 1 || bytes[3] != 0 {
            return None;
        }
        let words = bytes[4..].as_chunks::<4>().0;
        let unsigned = |i: usize| u32::from_le_bytes(words[i]);
        let signed = |i: usize| i32::from_le_bytes(words[i]);
        let (width, height) = (unsigned(4), unsigned(5));
        if width == 0 || height == 0 {
            return None;
        }
        Some(Self {
            page: Page::new(unsigned(0), unsigned(1)).with_offset(signed(2), signed(3)),
            width,
            height,
            dispose: Dispose::from_gif_code(bytes[1]),
            blend: if bytes[2] == 1 {
                Blend::Source
            } else {
                Blend::Over
            },
        })
    }

    /// The record as a metadata blob of kind [`KIND`](Self::KIND).
    pub fn to_blob(&self) -> oxideav_core::MetadataBlob {
        oxideav_core::MetadataBlob::new(Self::kind(), self.to_bytes().to_vec())
    }
}

/// `true` when every frame is on one canvas.
pub(crate) fn one_canvas(frames: &[crate::Image]) -> bool {
    frames
        .windows(2)
        .all(|w| w[0].canvas_size() == w[1].canvas_size())
}
