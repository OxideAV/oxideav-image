//! Read-side adaptations of legacy carriages: a format crate that still
//! carries a record the framework now has a side-channel for, through
//! a public and unambiguous field, is adapted here so the gateway's
//! view is uniform until the crate adopts the side-channel itself.
//! Each adaptation is keyed on the stream's codec id, documented in the
//! README (*Survey*), and withdrawn once the crate adopts core 0.1.40.

use oxideav_core::{CodecParameters, PixelFormat};

use crate::image::Image;

/// Apply every adaptation that matches `stream` to a freshly decoded
/// picture.
pub(crate) fn adapt(img: &mut Image, stream: &CodecParameters) {
    if stream.codec_id.as_str() == "png" {
        png_palette_alpha(img, &stream.extradata);
    }
}

/// `png`: the demuxer's stream `extradata` is `IHDR (13 bytes) ||
/// u32-BE length || PLTE data || u32-BE length || tRNS data` (both
/// lengths present, `0` for an absent chunk). For colour type 3
/// (`IHDR[9]`) the `tRNS` data is one alpha byte per palette entry, in
/// entry order, possibly shorter than the palette — exactly the
/// framework's palette-alpha record. The decoder attaches the palette
/// only, so the alpha is lifted here when the picture is `Pal8`, has a
/// palette and no alpha record yet.
fn png_palette_alpha(img: &mut Image, extradata: &[u8]) {
    if img.format() != PixelFormat::Pal8 || img.palette().is_none() || img.palette_alpha().is_some()
    {
        return;
    }
    let Some(trns) = png_trns(extradata) else {
        return;
    };
    let entries = img.palette().map_or(0, |p| p.len() / 3);
    if trns.is_empty() || trns.len() > entries {
        return;
    }
    img.set_palette_alpha(trns.to_vec());
}

/// The `tRNS` data of a palettised stream from the layout above;
/// `None` when the bytes do not have that shape or the colour type is
/// not 3.
fn png_trns(extradata: &[u8]) -> Option<&[u8]> {
    const IHDR: usize = 13;
    if extradata.len() < IHDR + 8 || extradata[9] != 3 {
        return None;
    }
    let len = |at: usize| -> Option<usize> {
        let b = extradata.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let plte_len = len(IHDR)?;
    let trns_at = IHDR + 4 + plte_len;
    let trns_len = len(trns_at)?;
    extradata.get(trns_at + 4..trns_at + 4 + trns_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CodecId;

    fn extradata(colour_type: u8, plte: &[u8], trns: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 13];
        v[9] = colour_type;
        v.extend_from_slice(&(plte.len() as u32).to_be_bytes());
        v.extend_from_slice(plte);
        v.extend_from_slice(&(trns.len() as u32).to_be_bytes());
        v.extend_from_slice(trns);
        v
    }

    fn pal8() -> Image {
        Image::from_indexed(2, 1, vec![0, 1], &[[1, 2, 3, 255], [4, 5, 6, 255]])
            .unwrap()
            .with_palette(vec![1, 2, 3, 4, 5, 6])
    }

    fn png_stream(extradata: Vec<u8>) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new("png"));
        p.extradata = extradata;
        p
    }

    #[test]
    fn png_trns_is_lifted_into_the_palette_alpha_record() {
        let mut img = pal8();
        assert_eq!(img.palette_alpha(), None);
        adapt(
            &mut img,
            &png_stream(extradata(3, &[1, 2, 3, 4, 5, 6], &[0])),
        );
        assert_eq!(img.palette_alpha(), Some(&[0][..]));
        assert!(img.has_alpha());
        assert_eq!(img.to_rgba8().unwrap(), vec![1, 2, 3, 0, 4, 5, 6, 255]);
    }

    #[test]
    fn png_trns_is_left_alone_when_it_does_not_apply() {
        // Not colour type 3.
        let mut img = pal8();
        adapt(&mut img, &png_stream(extradata(2, &[], &[0])));
        assert_eq!(img.palette_alpha(), None);
        // Empty tRNS, or longer than the palette.
        let mut img = pal8();
        adapt(
            &mut img,
            &png_stream(extradata(3, &[1, 2, 3, 4, 5, 6], &[])),
        );
        assert_eq!(img.palette_alpha(), None);
        let mut img = pal8();
        adapt(
            &mut img,
            &png_stream(extradata(3, &[1, 2, 3, 4, 5, 6], &[0, 0, 0])),
        );
        assert_eq!(img.palette_alpha(), None);
        // Truncated or foreign extradata.
        let mut img = pal8();
        adapt(&mut img, &png_stream(vec![0; 13]));
        assert_eq!(img.palette_alpha(), None);
        let mut img = pal8();
        let mut short = extradata(3, &[1, 2, 3, 4, 5, 6], &[0]);
        short.truncate(20);
        adapt(&mut img, &png_stream(short));
        assert_eq!(img.palette_alpha(), None);
        // Another codec is never touched.
        let mut img = pal8();
        let mut p = CodecParameters::video(CodecId::new("gif"));
        p.extradata = extradata(3, &[1, 2, 3, 4, 5, 6], &[0]);
        adapt(&mut img, &p);
        assert_eq!(img.palette_alpha(), None);
        // An existing record wins over the legacy field.
        let mut img = pal8().with_palette_rgba(&[[1, 2, 3, 7], [4, 5, 6, 255]]);
        adapt(
            &mut img,
            &png_stream(extradata(3, &[1, 2, 3, 4, 5, 6], &[0])),
        );
        assert_eq!(img.palette_alpha(), Some(&[7, 255][..]));
        // Not Pal8.
        let mut img = Image::from_rgb8(1, 1, vec![0; 3]).unwrap();
        adapt(&mut img, &png_stream(extradata(3, &[1, 2, 3], &[0])));
        assert_eq!(img.palette_alpha(), None);
    }
}
