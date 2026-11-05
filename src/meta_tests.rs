//! Container string metadata through the gateway (open → save → open
//! over `Muxer::set_metadata` / `carries_metadata_key`) and native
//! packed layouts (core 0.1.41's 16-bit words and padded 32-bit orders)
//! kept end to end — against the synthetic OXIM pair, whose muxers
//! cover the three carriage shapes: a free-form tag block (`oxim`), a
//! fixed header with `title` / `comment` (`oximrgb`), per-language
//! `copyright` (`oximmulti`) and none at all (`oximmux`, plus every
//! codec-only target).

use oxideav_core::{BlobKind, MetadataBlob, PixelFormat};

use crate::fixture::{self, Fixture, PACKED_WORDS};
use crate::ladder::reachable as supports;
use crate::{
    Image, ImageError, OpenOptions, SaveOptions, decode_bytes, decode_bytes_with, encode,
    encode_detailed, encode_frames,
};

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

/// A still in `format` whose container reports `meta`.
fn tagged(format: PixelFormat, meta: &[(&str, &str)]) -> Vec<u8> {
    let mut fx = Fixture::new(4, 2, format);
    fx.push_gradient(0, 3);
    fx.metadata = pairs(meta);
    fx.encode()
}

/// Options naming the payload codec for the two containers whose codec
/// has another name (`oximrgb`, `oximmulti` carry codec `oxim`).
fn on(target: &str, opts: SaveOptions) -> SaveOptions {
    if matches!(target, "oximrgb" | "oximmulti") {
        opts.with_codec(fixture::CODEC)
    } else {
        opts
    }
}

fn value<'a>(meta: &'a [(String, String)], key: &str) -> Option<&'a str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

const SOURCE: &[(&str, &str)] = &[
    ("title", "Harbour at dusk"),
    ("comment", "shot on a tripod"),
    ("author", "M. K."),
    ("copyright", "(c) 2026 Example"),
    ("copyright:fr", "(c) 2026 Exemple"),
];

#[test]
fn pictures_carry_the_files_string_metadata() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(3, 3, PixelFormat::Rgba);
    fx.push_gradient(40, 1).push_gradient(40, 2);
    fx.metadata = pairs(SOURCE);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert_eq!(file.metadata(), pairs(SOURCE).as_slice());
    for img in &file {
        assert_eq!(img.container_metadata(), file.metadata());
        assert_eq!(img.container_metadata_value("author"), Some("M. K."));
    }
    // A caller-built picture has none.
    let img = Image::from_rgb8(1, 1, vec![0; 3]).unwrap();
    assert!(img.container_metadata().is_empty());
    assert_eq!(img.loop_count(), None);
    // Conversions and crops keep them.
    let p = file.primary();
    assert_eq!(
        p.to_format(PixelFormat::Rgb24)
            .unwrap()
            .container_metadata(),
        file.metadata()
    );
    assert_eq!(
        p.crop(0, 0, 2, 2).unwrap().container_metadata(),
        file.metadata()
    );
}

#[test]
fn free_form_target_round_trips_every_key() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
    let done = encode_detailed(&ctx, file.frames(), "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(
        done.metadata_written(),
        keys(&["title", "comment", "author", "copyright", "copyright:fr"]).as_slice()
    );
    assert!(done.metadata_dropped().is_empty() && done.metadata_complete());
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    for (k, v) in SOURCE {
        assert_eq!(value(back.metadata(), k), Some(*v), "{k}");
    }
    // And again: the reopened file's pairs go back out unchanged (the
    // muxer's own lines are not doubled).
    let again = encode_detailed(&ctx, back.frames(), "oxim", &SaveOptions::default()).unwrap();
    let third = decode_bytes(&ctx, again.bytes()).unwrap();
    assert_eq!(third.metadata(), back.metadata());
    // `encode` (bytes only) writes the same pairs.
    let bytes = encode(&ctx, file.primary(), "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(bytes, done.bytes());
}

#[test]
fn fixed_field_target_keeps_its_keys_and_reports_the_rest() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
    let done = encode_detailed(
        &ctx,
        file.frames(),
        "oximrgb",
        &on("oximrgb", SaveOptions::default()),
    )
    .unwrap();
    assert_eq!(
        done.metadata_written(),
        keys(&["title", "comment"]).as_slice()
    );
    assert_eq!(
        done.metadata_dropped(),
        keys(&["author", "copyright", "copyright:fr"]).as_slice()
    );
    assert!(!done.metadata_complete());
    let back = decode_bytes_with(
        &ctx,
        done.bytes(),
        &OpenOptions::new().with_ext_hint("oximrgb"),
    )
    .unwrap();
    assert_eq!(value(back.metadata(), "title"), Some("Harbour at dusk"));
    assert_eq!(value(back.metadata(), "comment"), Some("shot on a tripod"));
    for k in done.metadata_dropped() {
        assert_eq!(value(back.metadata(), k), None, "{k} was reported dropped");
    }
}

#[test]
fn per_language_copyright_target() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgba, SOURCE)).unwrap();
    let done = encode_detailed(
        &ctx,
        file.frames(),
        "oximmulti",
        &on("oximmulti", SaveOptions::default()),
    )
    .unwrap();
    assert_eq!(
        done.metadata_written(),
        keys(&["copyright", "copyright:fr"]).as_slice()
    );
    assert_eq!(
        done.metadata_dropped(),
        keys(&["title", "comment", "author"]).as_slice()
    );
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(
        value(back.metadata(), "copyright:fr"),
        Some("(c) 2026 Exemple")
    );
    assert_eq!(value(back.metadata(), "title"), None);
}

#[test]
fn targets_without_text_report_every_key() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
    let all = keys(&["title", "comment", "author", "copyright", "copyright:fr"]);
    // A muxer with the default `set_metadata` (Unsupported): written
    // anyway, every key reported.
    let done = encode_detailed(&ctx, file.frames(), "oximmux", &SaveOptions::default()).unwrap();
    assert!(done.metadata_written().is_empty());
    assert_eq!(done.metadata_dropped(), all.as_slice());
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(value(back.metadata(), "title"), None);
    // A codec-only format: no muxer, nothing can hold the text.
    let yuv = file.primary().to_format(PixelFormat::Yuv420P).unwrap();
    let done = encode_detailed(
        &ctx,
        std::slice::from_ref(&yuv),
        fixture::CODEC_YUV_ONLY,
        &SaveOptions::default(),
    )
    .unwrap();
    assert!(done.metadata_written().is_empty());
    assert_eq!(done.metadata_dropped(), all.as_slice());
    // Nothing requested, nothing reported.
    let bare = Image::from_rgb8(2, 2, vec![9; 12]).unwrap();
    let done = encode_detailed(
        &ctx,
        std::slice::from_ref(&bare),
        "oximmux",
        &SaveOptions::default(),
    )
    .unwrap();
    assert!(done.metadata_written().is_empty() && done.metadata_complete());
}

#[test]
fn strict_metadata_refuses_a_lossy_target() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
    let strict = SaveOptions::new().with_strict_metadata(true);
    for target in ["oximrgb", "oximmux"] {
        match encode_detailed(&ctx, file.frames(), target, &on(target, strict.clone())) {
            Err(ImageError::Unsupported(msg)) => {
                assert!(msg.contains("author"), "{target}: {msg}");
                assert!(!msg.contains("no input layout"), "{target}: {msg}");
            }
            other => panic!("{target}: expected Unsupported, got {other:?}"),
        }
    }
    let yuv = file.primary().to_format(PixelFormat::Yuv420P).unwrap();
    match encode(&ctx, &yuv, fixture::CODEC_YUV_ONLY, &strict) {
        Err(ImageError::Unsupported(msg)) => assert!(msg.contains("codec-only"), "{msg}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // A target that carries everything is fine under strict, and so is
    // a lossy target once the uncarried keys are withheld.
    encode_detailed(&ctx, file.frames(), "oxim", &strict).unwrap();
    let narrowed = strict.with_stripped_metadata_keys(["author", "copyright", "copyright:fr"]);
    let done = encode_detailed(&ctx, file.frames(), "oximrgb", &on("oximrgb", narrowed)).unwrap();
    assert_eq!(
        done.metadata_written(),
        keys(&["title", "comment"]).as_slice()
    );
}

#[test]
fn options_replace_by_key_strip_and_add() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
    let opts = SaveOptions::new()
        .with_container_metadata_entry("title", "Retitled")
        .with_container_metadata_entry("software", "oxideav-image")
        .with_stripped_metadata_keys(["author"]);
    let done = encode_detailed(&ctx, file.frames(), "oxim", &opts).unwrap();
    assert_eq!(
        done.metadata_written(),
        keys(&["comment", "copyright", "copyright:fr", "title", "software"]).as_slice()
    );
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(value(back.metadata(), "title"), Some("Retitled"));
    assert_eq!(value(back.metadata(), "software"), Some("oxideav-image"));
    assert_eq!(value(back.metadata(), "author"), None);
    assert_eq!(
        back.metadata().iter().filter(|(k, _)| k == "title").count(),
        1
    );
    // `without_metadata` withholds every source pair (and blob); the
    // options' own pairs are still written.
    let opts = SaveOptions::new()
        .without_metadata()
        .with_container_metadata(pairs(&[("comment", "fresh")]));
    let done = encode_detailed(&ctx, file.frames(), "oxim", &opts).unwrap();
    assert_eq!(done.metadata_written(), keys(&["comment"]).as_slice());
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(value(back.metadata(), "comment"), Some("fresh"));
    assert_eq!(value(back.metadata(), "title"), None);
    // A picture's own set can be replaced before the encode.
    let img = file
        .primary()
        .clone()
        .with_container_metadata(pairs(&[("title", "Mine")]));
    let done = encode_detailed(
        &ctx,
        std::slice::from_ref(&img),
        "oxim",
        &SaveOptions::default(),
    )
    .unwrap();
    assert_eq!(done.metadata_written(), keys(&["title"]).as_slice());
}

#[test]
fn a_refusing_muxer_fails_the_encode() {
    // `Muxer::set_metadata` answering something other than Unsupported
    // (here InvalidData: a key the free-form block cannot write) is the
    // encode's error, not a dropped key.
    let ctx = fixture::ctx();
    let img = Image::from_rgb8(2, 1, vec![1; 6]).unwrap();
    let opts = SaveOptions::new().with_container_metadata_entry("a=b", "x");
    match encode(&ctx, &img, "oxim", &opts) {
        Err(ImageError::Core(oxideav_core::Error::InvalidData(msg))) => {
            assert!(msg.contains("a=b"), "{msg}")
        }
        other => panic!("expected InvalidData, got {other:?}"),
    }
}

#[test]
fn blobs_keep_their_path_beside_the_text() {
    let ctx = fixture::ctx();
    let icc = MetadataBlob::new(BlobKind::ICC, vec![1, 2, 3, 4]);
    let mut fx = Fixture::new(2, 2, PixelFormat::Rgb24).with_blobs(vec![icc.clone()]);
    fx.push_gradient(0, 5);
    fx.metadata = pairs(&[("title", "T")]);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    let done = encode_detailed(&ctx, file.frames(), "oxim", &SaveOptions::default()).unwrap();
    // The text report is about text only: the profile is not a key.
    assert_eq!(done.metadata_written(), keys(&["title"]).as_slice());
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(back.icc(), Some(&[1u8, 2, 3, 4][..]));
    assert_eq!(value(back.metadata(), "title"), Some("T"));
    // A target that drops the text still writes the profile.
    let done = encode_detailed(&ctx, file.frames(), "oximmux", &SaveOptions::default()).unwrap();
    assert_eq!(done.metadata_dropped(), keys(&["title"]).as_slice());
    assert_eq!(
        decode_bytes(&ctx, done.bytes()).unwrap().icc(),
        Some(&[1u8, 2, 3, 4][..])
    );
}

#[test]
fn an_animation_keeps_its_loop_count() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(2, 2, PixelFormat::Rgba);
    fx.push_gradient(50, 1).push_gradient(50, 2);
    fx.metadata = pairs(&[("loop_count", "3"), ("title", "Loop")]);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert_eq!(file.loop_count(), Some(3));
    assert!(file.iter().all(|i| i.loop_count() == Some(3)));
    // Not text: neither written nor dropped as a key, and the count is
    // written through the loop option.
    let done = encode_detailed(&ctx, file.frames(), "oximmux", &SaveOptions::default()).unwrap();
    assert_eq!(done.metadata_dropped(), keys(&["title"]).as_slice());
    let back = decode_bytes(&ctx, done.bytes()).unwrap();
    assert_eq!(back.loop_count(), Some(3));
    // The caller's count wins.
    let bytes = encode_frames(
        &ctx,
        file.frames(),
        "oxim",
        &SaveOptions::new().with_loop_count(0),
    )
    .unwrap();
    assert_eq!(decode_bytes(&ctx, &bytes).unwrap().loop_count(), Some(0));
    // A single picture taken out of the animation is a still: no count.
    let bytes = encode(&ctx, file.primary(), "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(decode_bytes(&ctx, &bytes).unwrap().loop_count(), None);
}

/// Every registered muxer of the context: what was requested is either
/// written or reported, never both and never neither; what was written
/// reads back; what was reported does not.
#[test]
fn every_registered_target_accounts_for_every_key() {
    let ctx = fixture::ctx();
    let requested = keys(&["title", "comment", "author", "copyright", "copyright:fr"]);
    let mut names: Vec<String> = ctx.containers.muxer_names().map(str::to_string).collect();
    names.sort();
    assert!(names.len() >= 4, "{names:?}");
    for name in &names {
        // oximrgb takes Rgb24 streams only; the others take anything.
        let file = decode_bytes(&ctx, &tagged(PixelFormat::Rgb24, SOURCE)).unwrap();
        let done = encode_detailed(&ctx, file.frames(), name, &on(name, SaveOptions::default()))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut seen: Vec<String> = done
            .metadata_written()
            .iter()
            .chain(done.metadata_dropped())
            .cloned()
            .collect();
        seen.sort();
        let mut want = requested.clone();
        want.sort();
        assert_eq!(seen, want, "{name}");
        let back = decode_bytes_with(&ctx, done.bytes(), &OpenOptions::new().with_ext_hint(name))
            .unwrap_or_else(|e| panic!("{name}: reopen: {e}"));
        for k in done.metadata_written() {
            let v = SOURCE.iter().find(|(s, _)| s == k).unwrap().1;
            assert_eq!(value(back.metadata(), k), Some(v), "{name}: {k}");
        }
        for k in done.metadata_dropped() {
            assert_eq!(value(back.metadata(), k), None, "{name}: {k}");
        }
    }
}

// ---- native packed layouts -------------------------------------------------

/// Deterministic bytes for a `w × h` picture in a packed `f`.
fn packed_bytes(f: PixelFormat, w: u32, h: u32) -> Vec<u8> {
    let n = f.plane_row_bytes(0, w).unwrap() * h as usize;
    (0..n).map(|i| (i * 37 + 11) as u8).collect()
}

#[test]
fn packed_words_are_raw_layouts() {
    for &f in PACKED_WORDS {
        let bytes = packed_bytes(f, 5, 3);
        let bpp = if f.bits_per_pixel_approx() == 16 {
            2
        } else {
            4
        };
        assert_eq!(bytes.len(), 5 * 3 * bpp, "{f:?}");
        let img = Image::from_raw(5, 3, f, bytes.clone()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        assert_eq!(img.format(), f);
        assert_eq!(img.as_packed(), Some(bytes.as_slice()), "{f:?}");
        assert_eq!(img.to_packed(f).unwrap(), bytes, "{f:?}");
        // Padding is not alpha.
        assert!(!img.has_alpha() && img.is_opaque(), "{f:?}");
        // A conversion is pixfmt's to make: done when the linked release
        // describes the layout, `Unsupported` (never a panic) otherwise.
        match img.to_rgba8() {
            Ok(rgba) => {
                assert!(supports(f, PixelFormat::Rgba), "{f:?}");
                assert_eq!(rgba.len(), 5 * 3 * 4, "{f:?}");
                assert!(rgba.chunks(4).all(|p| p[3] == 255), "{f:?}");
            }
            Err(e) => {
                assert!(!supports(f, PixelFormat::Rgba), "{f:?}: {e}");
                assert!(matches!(e, ImageError::Unsupported(_)), "{f:?}: {e:?}");
            }
        }
        let c = img.crop(1, 1, 3, 2).unwrap();
        assert_eq!(c.as_packed().unwrap().len(), 3 * 2 * bpp, "{f:?}");
        assert_eq!(
            &c.as_packed().unwrap()[..bpp],
            &bytes[(5 + 1) * bpp..(5 + 2) * bpp],
            "{f:?}"
        );
    }
}

#[test]
fn a_declared_packed_layout_is_kept_end_to_end() {
    // The encoder declares the twelve labels: each picture reaches it
    // in its own layout (no conversion), and the reopened picture is
    // the same layout with the same bytes.
    let ctx = fixture::ctx();
    let opts = SaveOptions::new().with_codec(fixture::CODEC_PACKED);
    for &f in PACKED_WORDS {
        let bytes = packed_bytes(f, 4, 3);
        let img = Image::from_raw(4, 3, f, bytes.clone()).unwrap();
        let done = encode_detailed(&ctx, std::slice::from_ref(&img), "oxim", &opts)
            .unwrap_or_else(|e| panic!("{f:?}: {e}"));
        assert_eq!(done.pixel_format(), f);
        let back = decode_bytes(&ctx, done.bytes()).unwrap().into_primary();
        assert_eq!(back.format(), f, "opened in the decoder's layout");
        assert_eq!(back.as_packed(), Some(bytes.as_slice()), "{f:?}");
    }
}

#[test]
fn an_undeclared_packed_layout_converts_only_when_pixfmt_reaches_it() {
    // The `oxim` encoder declares RGB 24 / 32 / 48 / 64, YCbCr, grey,
    // palette, CMYK — not the packed words: a word picture goes through
    // pixfmt to the closest declared layout (`Rgb24`), or the encode
    // says no layout was reachable.
    let ctx = fixture::ctx();
    for &f in PACKED_WORDS {
        let img = Image::from_raw(4, 2, f, packed_bytes(f, 4, 2)).unwrap();
        let got = encode_detailed(
            &ctx,
            std::slice::from_ref(&img),
            "oxim",
            &SaveOptions::default(),
        );
        if supports(f, PixelFormat::Rgb24) {
            let done = got.unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert_eq!(done.pixel_format(), PixelFormat::Rgb24, "{f:?}");
            let back = decode_bytes(&ctx, done.bytes()).unwrap().into_primary();
            assert_eq!(back.to_rgb8().unwrap(), img.to_rgb8().unwrap(), "{f:?}");
        } else {
            assert!(
                matches!(got, Err(ImageError::Unsupported(_))),
                "{f:?}: {got:?}"
            );
        }
    }
    {
        // A declared target (`oxim_packed`) takes the RGB-family source
        // in a word layout when pixfmt reaches one, and never as grey.
        let rgb = Image::from_rgb8(4, 2, vec![200; 24]).unwrap();
        let got = encode_detailed(
            &ctx,
            std::slice::from_ref(&rgb),
            "oxim",
            &SaveOptions::new().with_codec(fixture::CODEC_PACKED),
        );
        if PACKED_WORDS
            .iter()
            .any(|w| supports(PixelFormat::Rgb24, *w))
        {
            let done = got.unwrap();
            // Same tier first: a padded 32-bit order, never a word.
            assert!(
                matches!(
                    done.pixel_format(),
                    PixelFormat::Rgbx | PixelFormat::Bgrx | PixelFormat::Xrgb | PixelFormat::Xbgr
                ),
                "{:?}",
                done.pixel_format()
            );
        } else {
            assert!(got.is_err());
        }
    }
}

#[test]
fn open_keeps_the_decoders_layout_unless_asked() {
    let ctx = fixture::ctx();
    // A YCbCr file opens as YCbCr — and as the asked layout on request.
    let mut fx = Fixture::new(4, 4, PixelFormat::Yuv420P);
    fx.push_gradient(0, 9);
    let bytes = fx.encode();
    assert_eq!(
        decode_bytes(&ctx, &bytes).unwrap().primary().format(),
        PixelFormat::Yuv420P
    );
    let asked = decode_bytes_with(
        &ctx,
        &bytes,
        &OpenOptions::new().with_pixel_format(PixelFormat::Rgba),
    )
    .unwrap();
    assert_eq!(asked.primary().format(), PixelFormat::Rgba);
    assert_eq!(
        asked.primary().as_packed().unwrap(),
        decode_bytes(&ctx, &bytes)
            .unwrap()
            .primary()
            .to_rgba8()
            .unwrap()
            .as_slice()
    );
    // A word layout: kept on open; converted on request when pixfmt
    // reaches the asked layout, else the open says so.
    let img = Image::from_raw(
        4,
        2,
        PixelFormat::Rgb565Le,
        packed_bytes(PixelFormat::Rgb565Le, 4, 2),
    )
    .unwrap();
    let file = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_codec(fixture::CODEC_PACKED),
    )
    .unwrap();
    assert_eq!(
        decode_bytes(&ctx, &file).unwrap().primary().format(),
        PixelFormat::Rgb565Le
    );
    let asked = decode_bytes_with(
        &ctx,
        &file,
        &OpenOptions::new().with_pixel_format(PixelFormat::Rgb24),
    );
    if supports(PixelFormat::Rgb565Le, PixelFormat::Rgb24) {
        assert_eq!(asked.unwrap().primary().format(), PixelFormat::Rgb24);
    } else {
        assert!(asked.is_err());
    }
    // Asking for the layout it already has is a no-op.
    let same = decode_bytes_with(
        &ctx,
        &file,
        &OpenOptions::new().with_pixel_format(PixelFormat::Rgb565Le),
    )
    .unwrap();
    assert_eq!(
        same.primary().as_packed(),
        Some(packed_bytes(PixelFormat::Rgb565Le, 4, 2).as_slice())
    );
}
