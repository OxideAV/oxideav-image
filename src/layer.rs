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
