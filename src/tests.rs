//! Gateway tests against the synthetic OXIM container + codec pair in
//! [`crate::fixture`]. No file I/O except where marked `not(miri)`.

use std::time::Duration;

use oxideav_core::{BlobKind, ExecutionContext, MetadataBlob, PixelFormat, TimeBase, VideoPlane};

use crate::fixture::{self, Fixture, FORMATS};
use crate::{
    can_encode, can_open, codec_only_formats, decode_bytes, decode_bytes_with, decode_vec, encode,
    encode_frames, encoder_options, resolve_target, support, Image, ImageError, MetadataSupport,
    OpenOptions, SaveOptions,
};

fn still(format: PixelFormat) -> Fixture {
    let mut fx = Fixture::new(5, 3, format);
    fx.push_gradient(0, 1);
    fx
}

fn planes_of(img: &Image) -> Vec<Vec<u8>> {
    img.frame()
        .image_planes()
        .iter()
        .map(|p| p.data.clone())
        .collect()
}

fn is_packed(f: PixelFormat) -> bool {
    f.plane_count() == 1
}

#[test]
fn probe_by_magic_without_hint() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &still(PixelFormat::Rgba).encode()).unwrap();
    assert_eq!(file.container(), fixture::CONTAINER);
    assert_eq!(file.len(), 1);
    assert_eq!(file.primary().format(), PixelFormat::Rgba);
    assert_eq!((file.primary().width(), file.primary().height()), (5, 3));
}

#[test]
fn extension_hint_selects_a_sibling_container() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgb24).encode();
    let file =
        decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_ext_hint("oximrgb")).unwrap();
    assert_eq!(file.container(), fixture::CONTAINER_RGB_ONLY);
    // An unrelated hint does not stop the magic from winning.
    let file = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_ext_hint("txt")).unwrap();
    assert_eq!(file.container(), fixture::CONTAINER);
}

#[test]
fn garbage_is_unknown_format() {
    let ctx = fixture::ctx();
    match decode_bytes(&ctx, b"not an image at all, really not") {
        Err(ImageError::UnknownFormat(_)) => {}
        other => panic!("expected UnknownFormat, got {other:?}"),
    }
}

#[test]
fn every_packed_layout_decodes_to_its_tight_plane() {
    let ctx = fixture::ctx();
    for &f in FORMATS.iter().filter(|f| is_packed(**f)) {
        let fx = still(f);
        let file = decode_bytes(&ctx, &fx.encode()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        let img = file.primary();
        assert_eq!(img.format(), f, "{f:?}");
        assert_eq!(
            img.as_packed(),
            Some(fx.frames[0].planes[0].data.as_slice()),
            "{f:?}: as_packed"
        );
        let rgba = img
            .to_rgba8()
            .unwrap_or_else(|e| panic!("{f:?}: to_rgba8: {e}"));
        assert_eq!(rgba.len(), 5 * 3 * 4, "{f:?}");
        let rgb = img.to_rgb8().unwrap();
        assert_eq!(rgb.len(), 5 * 3 * 3, "{f:?}");
        assert_eq!(img.to_rgb16().unwrap().len(), 5 * 3 * 6, "{f:?}: rgb16");
        assert_eq!(img.to_gray16().unwrap().len(), 5 * 3 * 2, "{f:?}: gray16");
    }
}

#[test]
fn planar_layouts_decode_and_convert() {
    let ctx = fixture::ctx();
    for &f in FORMATS.iter().filter(|f| !is_packed(**f)) {
        let fx = still(f);
        let file = decode_bytes(&ctx, &fx.encode()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        let img = file.primary().clone();
        assert_eq!(img.format(), f);
        assert!(img.as_packed().is_none(), "{f:?} is planar");
        let expected: Vec<u8> = fx.frames[0]
            .planes
            .iter()
            .flat_map(|p| p.data.iter().copied())
            .collect();
        assert_eq!(img.to_rgba8().unwrap().len(), 5 * 3 * 4, "{f:?}");
        assert_eq!(img.to_rgb16().unwrap().len(), 5 * 3 * 6, "{f:?}: rgb16");
        assert_eq!(img.into_raw(), expected, "{f:?}: into_raw");
    }
}

#[test]
fn pal8_carries_its_palette_and_expands_through_it() {
    let ctx = fixture::ctx();
    let fx = still(PixelFormat::Pal8);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    let img = file.primary();
    let pal = fx.frames[0].palette.as_deref().unwrap();
    assert_eq!(img.palette(), Some(pal));
    let rgb = img.to_rgb8().unwrap();
    let idx = fx.frames[0].planes[0].data[0] as usize;
    assert_eq!(&rgb[..3], &pal[idx * 3..idx * 3 + 3]);
}

#[test]
fn multi_frame_files_and_max_frames() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(4, 4, PixelFormat::Rgb24);
    fx.push_gradient(40, 1)
        .push_gradient(80, 2)
        .push_gradient(120, 3);
    let bytes = fx.encode();
    let file = decode_bytes(&ctx, &bytes).unwrap();
    assert_eq!(file.len(), 3);
    for (i, img) in file.frames().iter().enumerate() {
        assert_eq!(img.index(), i);
        assert_eq!(
            img.as_packed(),
            Some(fx.frames[i].planes[0].data.as_slice())
        );
    }
    let one = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_max_frames(1)).unwrap();
    assert_eq!(one.len(), 1);
    // Zero is clamped: a file always yields its primary picture.
    let zero = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_max_frames(0)).unwrap();
    assert_eq!(zero.len(), 1);
    let two = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_max_frames(2)).unwrap();
    assert_eq!(two.into_frames().len(), 2);
}

#[test]
fn metadata_passes_through() {
    let ctx = fixture::ctx();
    let mut fx = still(PixelFormat::Gray8);
    fx.metadata = vec![
        ("title".into(), "gateway".into()),
        ("author".into(), "oxim".into()),
    ];
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert_eq!(file.metadata(), fx.metadata.as_slice());
}

#[test]
fn encode_round_trips_every_layout() {
    let ctx = fixture::ctx();
    for &f in FORMATS {
        let fx = still(f);
        let img = decode_bytes(&ctx, &fx.encode()).unwrap().into_primary();
        let bytes = encode(&ctx, &img, "oxim", &SaveOptions::default())
            .unwrap_or_else(|e| panic!("{f:?}: encode: {e}"));
        let back = decode_bytes(&ctx, &bytes).unwrap().into_primary();
        assert_eq!(back.format(), f, "{f:?}: format kept");
        assert_eq!(planes_of(&back), planes_of(&img), "{f:?}: planes");
        assert_eq!(back.palette(), img.palette(), "{f:?}: palette");
    }
}

#[test]
fn encode_by_extension_and_dotted_name() {
    let ctx = fixture::ctx();
    let img = Image::from_rgb8(2, 2, vec![9; 12]).unwrap();
    for name in ["oxim", ".oxim", "OXIM"] {
        let bytes = encode(&ctx, &img, name, &SaveOptions::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            decode_bytes(&ctx, &bytes).unwrap().primary().as_packed(),
            Some(&[9u8; 12][..])
        );
    }
}

#[test]
fn ladder_steps_to_a_layout_the_codec_accepts() {
    let ctx = fixture::ctx();
    let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| (i * 3) as u8).collect();
    let img = Image::from_rgba8(4, 4, rgba).unwrap();
    let bytes = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_codec(fixture::CODEC_YUV_ONLY),
    )
    .unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap().into_primary();
    assert_eq!(back.format(), PixelFormat::Yuv420P);
    assert_eq!(back.to_rgba8().unwrap().len(), 64);
}

#[test]
fn ladder_offers_colour_before_grey_to_an_undeclared_encoder() {
    // r474 ruling: an RGB picture sent to an encoder that declares no
    // accepted layouts (and takes YCbCr or grey) must not arrive grey.
    let ctx = fixture::ctx();
    let rgb: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 7) as u8).collect();
    let img = Image::from_rgb8(4, 4, rgb).unwrap();
    let opts = SaveOptions::new().with_codec(fixture::CODEC_UNDECLARED);
    let bytes = encode(&ctx, &img, "oxim", &opts).unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap().into_primary();
    assert_eq!(
        back.format(),
        PixelFormat::Yuv444P,
        "highest-fidelity colour rung"
    );
    // The same with alpha: still colour, still 4:4:4.
    let rgba = Image::from_rgba8(4, 4, (0..64).collect()).unwrap();
    let bytes = encode(&ctx, &rgba, "oxim", &opts).unwrap();
    assert_eq!(
        decode_bytes(&ctx, &bytes).unwrap().primary().format(),
        PixelFormat::Yuv444P
    );
    // A 4:2:0 source stays 4:2:0; a grey source stays grey.
    let yuv = decode_bytes(&ctx, &still(PixelFormat::Yuv420P).encode())
        .unwrap()
        .into_primary();
    let bytes = encode(&ctx, &yuv, "oxim", &opts).unwrap();
    assert_eq!(
        decode_bytes(&ctx, &bytes).unwrap().primary().format(),
        PixelFormat::Yuv420P
    );
    let grey = Image::from_raw(4, 4, PixelFormat::Gray8, vec![9; 16]).unwrap();
    let bytes = encode(&ctx, &grey, "oxim", &opts).unwrap();
    assert_eq!(
        decode_bytes(&ctx, &bytes).unwrap().primary().format(),
        PixelFormat::Gray8
    );
}

#[test]
fn ladder_steps_when_the_muxer_refuses_a_layout() {
    let ctx = fixture::ctx();
    let rgba: Vec<u8> = (0..3 * 2 * 4).map(|i| (i * 5) as u8).collect();
    let img = Image::from_rgba8(3, 2, rgba.clone()).unwrap();
    // The container's payload codec has another name, so it is named.
    let bytes = encode(
        &ctx,
        &img,
        "oximrgb",
        &SaveOptions::new().with_codec(fixture::CODEC),
    )
    .unwrap();
    let back = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_ext_hint("oximrgb"))
        .unwrap()
        .into_primary();
    assert_eq!(back.format(), PixelFormat::Rgb24);
    let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
    assert_eq!(back.as_packed(), Some(rgb.as_slice()));
}

#[test]
fn forced_pixel_format_is_the_only_candidate() {
    let ctx = fixture::ctx();
    let img = Image::from_rgba8(2, 2, vec![200; 16]).unwrap();
    let bytes = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_pixel_format(PixelFormat::Gray8),
    )
    .unwrap();
    assert_eq!(
        decode_bytes(&ctx, &bytes).unwrap().primary().format(),
        PixelFormat::Gray8
    );
    // Forcing a layout the codec refuses fails instead of stepping.
    let err = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new()
            .with_codec(fixture::CODEC_YUV_ONLY)
            .with_pixel_format(PixelFormat::Rgba),
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::Core(_)), "{err:?}");
}

#[test]
fn error_paths_have_their_variants() {
    let ctx = fixture::ctx();
    let img = Image::from_rgb8(2, 2, vec![0; 12]).unwrap();
    assert!(matches!(
        encode(&ctx, &img, "nope", &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
    // A container with a demuxer but no muxer.
    assert!(matches!(
        encode(&ctx, &img, "oximnodec", &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
    assert!(matches!(
        encode(
            &ctx,
            &img,
            "oxim",
            &SaveOptions::new().with_codec("no_such_codec")
        ),
        Err(ImageError::Unsupported(_))
    ));
    assert!(matches!(
        encode_frames(&ctx, &[], "oxim", &SaveOptions::default()),
        Err(ImageError::InvalidData(_))
    ));
    // Mixed geometries into a one-geometry muxer: a typed refusal.
    let other = Image::from_rgb8(3, 2, vec![0; 18]).unwrap();
    match encode_frames(&ctx, &[img.clone(), other], "oxim", &SaveOptions::default()) {
        Err(ImageError::Unsupported(msg)) => assert!(msg.contains("2x2, 3x2"), "{msg}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // A stream whose codec nobody decodes.
    let bytes = still(PixelFormat::Rgb24).encode();
    assert!(matches!(
        decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_ext_hint("oximnodec")),
        Err(ImageError::NoImage(_))
    ));
    // A file with no pictures at all.
    let empty = Fixture::new(2, 2, PixelFormat::Rgb24).encode();
    assert!(matches!(
        decode_bytes(&ctx, &empty),
        Err(ImageError::NoImage(_))
    ));
    // Truncated payload surfaces the decoder's error.
    let mut cut = still(PixelFormat::Rgb24).encode();
    cut.truncate(cut.len() - 4);
    assert!(matches!(decode_bytes(&ctx, &cut), Err(ImageError::Core(_))));
}

#[test]
fn encode_frames_keeps_every_picture() {
    let ctx = fixture::ctx();
    let frames: Vec<Image> = (0..3u8)
        .map(|i| Image::from_rgb8(2, 2, vec![i * 40; 12]).unwrap())
        .collect();
    let bytes = encode_frames(&ctx, &frames, "oxim", &SaveOptions::default()).unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap();
    assert_eq!(back.len(), 3);
    for (a, b) in back.frames().iter().zip(&frames) {
        assert_eq!(a.as_packed(), b.as_packed());
    }
}

#[test]
fn color_signal_prefers_the_frame_then_the_stream() {
    use oxideav_core::{
        ColorPrimaries, ColorRange, ColorSignal, MatrixCoefficients, TransferCharacteristics,
    };
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Yuv420P).encode();
    let plain = decode_bytes(&ctx, &bytes).unwrap().into_primary();
    assert_eq!(plain.color_signal(), None);
    let sig = ColorSignal::new(
        ColorRange::Full,
        ColorPrimaries(1),
        TransferCharacteristics(13),
        MatrixCoefficients(5),
    );
    let img = plain.clone().with_color_signal(sig);
    assert_eq!(img.color_signal(), Some(sig));
    assert_eq!(img.frame().color_signal(), Some(sig));
}

#[cfg(not(miri))]
#[test]
fn save_and_open_round_trip_through_files() {
    let ctx = fixture::ctx();
    let dir = std::env::temp_dir().join(format!(
        "oxideav-image-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("picture.OXIM");
    let img = Image::from_rgba8(3, 3, (0..36).collect()).unwrap();
    crate::save(&ctx, &img, &path, &SaveOptions::default()).unwrap();
    let file = crate::open(&ctx, &path).unwrap();
    assert_eq!(file.container(), fixture::CONTAINER);
    assert_eq!(file.primary().as_packed(), img.as_packed());
    // No extension → UnknownFormat before anything is written.
    let bare = dir.join("picture");
    assert!(matches!(
        crate::save(&ctx, &img, &bare, &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
    assert!(!bare.exists());
    assert!(matches!(
        crate::open(&ctx, dir.join("missing.oxim")),
        Err(ImageError::Io(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

fn anim(flags: u8) -> Fixture {
    let mut fx = Fixture::new(2, 2, PixelFormat::Rgb24);
    fx.flags = flags;
    fx.push_gradient(30, 1)
        .push_gradient(70, 2)
        .push_gradient(20, 3);
    fx
}

fn ms(v: u64) -> Option<Duration> {
    Some(Duration::from_millis(v))
}

#[test]
fn delays_come_from_packet_durations() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &anim(fixture::FLAG_DURATIONS).encode()).unwrap();
    let delays: Vec<_> = file.frames().iter().map(Image::delay).collect();
    assert_eq!(delays, vec![ms(30), ms(70), ms(20)]);
    let stamps: Vec<_> = file.frames().iter().map(Image::timestamp).collect();
    assert_eq!(stamps, vec![ms(0), ms(30), ms(100)]);
    assert!(file.frames().iter().all(|i| i.stream() == 0));
    assert_eq!(file.primary().time_base(), fixture::TIME_BASE);
    assert_eq!(file.primary().raw_timing(), (Some(0), Some(30)));
}

#[test]
fn delays_fall_back_to_pts_deltas_and_the_last_repeats() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &anim(0).encode()).unwrap();
    let delays: Vec<_> = file.frames().iter().map(Image::delay).collect();
    // No durations: 30 → 100 is a 70 ms gap... the first gap is 30,
    // the second 70, and the last picture repeats the previous delay.
    assert_eq!(delays, vec![ms(30), ms(70), ms(70)]);
}

#[test]
fn no_timing_at_all_means_no_delay() {
    let ctx = fixture::ctx();
    let file = decode_bytes(&ctx, &anim(fixture::FLAG_NO_PTS).encode()).unwrap();
    assert!(file.frames().iter().all(|i| i.delay().is_none()));
    assert!(file.frames().iter().all(|i| i.timestamp().is_none()));
    // A still never has a delay, with or without a stamped duration.
    let still = decode_bytes(&ctx, &still(PixelFormat::Gray8).encode()).unwrap();
    assert_eq!(still.primary().delay(), None);
    let mut fx = Fixture::new(2, 2, PixelFormat::Gray8);
    fx.push_gradient(500, 1);
    let timed = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert_eq!(timed.primary().delay(), None);
    assert_eq!(timed.primary().raw_timing(), (Some(0), Some(500)));
}

#[test]
fn encode_frames_round_trips_delays() {
    let ctx = fixture::ctx();
    let frames: Vec<Image> = [30u64, 70, 20]
        .iter()
        .enumerate()
        .map(|(i, d)| {
            Image::from_rgb8(2, 2, vec![i as u8 * 40; 12])
                .unwrap()
                .with_delay(ms(*d))
        })
        .collect();
    for tb in [
        None,
        Some(TimeBase::new(1, 100)),
        Some(TimeBase::new(1, 90_000)),
    ] {
        let mut opts = SaveOptions::default();
        if let Some(tb) = tb {
            opts = opts.with_time_base(tb);
        }
        let bytes = encode_frames(&ctx, &frames, "oxim", &opts).unwrap();
        let back = decode_bytes(&ctx, &bytes).unwrap();
        let delays: Vec<_> = back.frames().iter().map(Image::delay).collect();
        assert_eq!(delays, vec![ms(30), ms(70), ms(20)], "{tb:?}");
    }
    // Pictures without a delay take the default (or the caller's).
    let plain: Vec<Image> = frames.iter().map(|f| f.clone().with_delay(None)).collect();
    let bytes = encode_frames(&ctx, &plain, "oxim", &SaveOptions::default()).unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap();
    assert!(back
        .frames()
        .iter()
        .all(|i| i.delay() == Some(crate::DEFAULT_DELAY)));
    let bytes = encode_frames(
        &ctx,
        &plain,
        "oxim",
        &SaveOptions::new().with_default_delay(Duration::from_millis(16)),
    )
    .unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap();
    assert!(back.frames().iter().all(|i| i.delay() == ms(16)));
    // A single still is written without a duration.
    let one = encode(&ctx, &plain[0], "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(decode_bytes(&ctx, &one).unwrap().primary().delay(), None);
}

#[test]
fn crop_packed_slices_rows() {
    // 4x3 Rgb24 with pixel (x, y) = [x, y, 7].
    let rgb: Vec<u8> = (0..3u8)
        .flat_map(|y| (0..4u8).flat_map(move |x| [x, y, 7]))
        .collect();
    let img = Image::from_rgb8(4, 3, rgb).unwrap();
    let c = img.crop(1, 1, 2, 2).unwrap();
    assert_eq!(
        (c.width(), c.height(), c.format()),
        (2, 2, PixelFormat::Rgb24)
    );
    assert_eq!(
        c.as_packed(),
        Some(&[1, 1, 7, 2, 1, 7, 1, 2, 7, 2, 2, 7][..])
    );
    assert_eq!(c.index(), img.index());
    assert!(matches!(
        img.crop(3, 0, 2, 1),
        Err(ImageError::InvalidData(_))
    ));
    assert!(matches!(
        img.crop(0, 0, 0, 1),
        Err(ImageError::InvalidData(_))
    ));
    assert!(matches!(
        img.crop(u32::MAX, 0, 2, 1),
        Err(ImageError::InvalidData(_))
    ));
}

#[test]
fn crop_planar_respects_the_chroma_grid() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(6, 4, PixelFormat::Yuv420P);
    fx.push_gradient(0, 9);
    let img = decode_bytes(&ctx, &fx.encode()).unwrap().into_primary();
    let c = img.crop(2, 2, 3, 2).unwrap();
    assert_eq!((c.width(), c.height()), (3, 2));
    let planes = c.frame().image_planes();
    assert_eq!(planes.len(), 3);
    let src = &fx.frames[0].planes;
    // Luma: rows 2..4, columns 2..5.
    assert_eq!(planes[0].stride, 3);
    assert_eq!(&planes[0].data[..3], &src[0].data[2 * 6 + 2..2 * 6 + 5]);
    assert_eq!(&planes[0].data[3..], &src[0].data[3 * 6 + 2..3 * 6 + 5]);
    // Chroma: 3x3 planes, row 1, columns 1..3 (ceil(3/2) = 2 wide).
    assert_eq!(planes[1].stride, 2);
    assert_eq!(planes[1].data, &src[1].data[3 + 1..3 + 3]);
    assert_eq!(planes[2].data, &src[2].data[3 + 1..3 + 3]);
    assert!(matches!(
        img.crop(1, 0, 2, 2),
        Err(ImageError::Unsupported(_))
    ));
    assert!(matches!(
        img.crop(0, 1, 2, 2),
        Err(ImageError::Unsupported(_))
    ));
    assert_eq!(c.to_rgba8().unwrap().len(), 3 * 2 * 4);
    // 4:4:4 and planar RGB have no grid.
    for f in [
        PixelFormat::Yuv444P,
        PixelFormat::Gbrp8,
        PixelFormat::Yuva420P,
    ] {
        let mut fx = Fixture::new(6, 4, f);
        fx.push_gradient(0, 1);
        let img = decode_bytes(&ctx, &fx.encode()).unwrap().into_primary();
        let (x, y) = if f == PixelFormat::Yuva420P {
            (2, 2)
        } else {
            (1, 1)
        };
        let c = img.crop(x, y, 3, 2).unwrap();
        assert_eq!(c.frame().image_planes().len(), f.plane_count(), "{f:?}");
        assert_eq!(
            c.into_raw().len(),
            f.frame_size_bytes(3, 2).unwrap(),
            "{f:?}"
        );
    }
}

#[test]
fn crop_keeps_palette_and_colour_signal() {
    let ctx = fixture::ctx();
    let fx = still(PixelFormat::Pal8);
    let img = decode_bytes(&ctx, &fx.encode()).unwrap().into_primary();
    let c = img.crop(1, 1, 2, 2).unwrap();
    assert_eq!(c.palette(), img.palette());
    assert_eq!(c.format(), PixelFormat::Pal8);
    let idx = fx.frames[0].planes[0].data[5 + 1] as usize; // row 1, column 1
    let pal = img.palette().unwrap();
    assert_eq!(&c.to_rgb8().unwrap()[..3], &pal[idx * 3..idx * 3 + 3]);
    let sig = oxideav_core::ColorSignal::new(
        oxideav_core::ColorRange::Full,
        oxideav_core::ColorPrimaries(9),
        oxideav_core::TransferCharacteristics(16),
        oxideav_core::MatrixCoefficients(9),
    );
    let tagged = Image::from_rgba8(2, 2, vec![1; 16])
        .unwrap()
        .with_color_signal(sig);
    assert_eq!(tagged.crop(0, 0, 1, 1).unwrap().color_signal(), Some(sig));
}

#[test]
fn to_gray8_and_from_planes() {
    let img = Image::from_rgb8(1, 2, vec![255, 255, 255, 0, 0, 0]).unwrap();
    assert_eq!(img.to_gray8().unwrap(), vec![255, 0]);
    // Padded planar input is accepted and converts.
    let y = VideoPlane {
        stride: 8,
        data: vec![128; 8 * 2],
    };
    let u = VideoPlane {
        stride: 4,
        data: vec![128; 4],
    };
    let v = u.clone();
    let img = Image::from_planes(
        4,
        2,
        PixelFormat::Yuv420P,
        vec![y.clone(), u.clone(), v.clone()],
    )
    .unwrap();
    assert_eq!(img.to_rgba8().unwrap().len(), 4 * 2 * 4);
    assert!(matches!(
        Image::from_planes(4, 2, PixelFormat::Yuv420P, vec![y.clone(), u.clone()]),
        Err(ImageError::InvalidData(_))
    ));
    let short = VideoPlane {
        stride: 4,
        data: vec![0; 7],
    };
    assert!(matches!(
        Image::from_planes(
            4,
            2,
            PixelFormat::Yuv420P,
            vec![short, u.clone(), v.clone()]
        ),
        Err(ImageError::InvalidData(_))
    ));
    let narrow = VideoPlane {
        stride: 3,
        data: vec![0; 8],
    };
    assert!(matches!(
        Image::from_planes(4, 2, PixelFormat::Yuv420P, vec![narrow, u, v]),
        Err(ImageError::InvalidData(_))
    ));
    assert!(matches!(
        Image::from_planes(0, 2, PixelFormat::Gray8, vec![]),
        Err(ImageError::InvalidData(_))
    ));
    // A palette side-channel rides along.
    let pal = fixture::test_palette(3);
    let indices = VideoPlane {
        stride: 2,
        data: vec![5, 6, 7, 8],
    };
    let frame = oxideav_core::VideoFrame {
        pts: None,
        planes: vec![indices],
    }
    .with_palette(pal.clone());
    let img = Image::from_planes(2, 2, PixelFormat::Pal8, frame.planes).unwrap();
    assert_eq!(img.palette(), Some(pal.as_slice()));
    assert_eq!(&img.to_rgb8().unwrap()[..3], &pal[15..18]);
}

#[test]
fn image_file_iteration_and_mutation() {
    let ctx = fixture::ctx();
    let mut file = decode_bytes(&ctx, &anim(fixture::FLAG_DURATIONS).encode()).unwrap();
    assert_eq!(file.iter().count(), 3);
    assert_eq!((&file).into_iter().map(Image::index).sum::<usize>(), 3);
    for img in file.frames_mut() {
        *img = img.clone().with_delay(ms(5));
    }
    let delays: Vec<_> = file.into_iter().map(|i| i.delay()).collect();
    assert_eq!(delays, vec![ms(5); 3]);
}

#[test]
fn decoder_options_reach_the_decoder() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgb24).encode();
    let plain = decode_bytes(&ctx, &bytes).unwrap().into_primary();
    assert_eq!(plain.frame().color_signal(), None);
    let opts = OpenOptions::new().with_decoder_option("stamp_color", "1");
    let stamped = decode_bytes_with(&ctx, &bytes, &opts)
        .unwrap()
        .into_primary();
    assert!(stamped.frame().color_signal().is_some());
    let opts = OpenOptions::new().with_decoder_options(vec![("stamp_color".into(), "1".into())]);
    assert!(decode_bytes_with(&ctx, &bytes, &opts)
        .unwrap()
        .primary()
        .frame()
        .color_signal()
        .is_some());
}

#[test]
fn max_pixels_is_a_budget_over_the_file() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgb24).encode(); // 5x3 = 15 px
    assert!(decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_max_pixels(15)).is_ok());
    assert!(matches!(
        decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_max_pixels(14)),
        Err(ImageError::LimitExceeded(_))
    ));
    let anim = anim(fixture::FLAG_DURATIONS).encode(); // 3 × 2x2
    assert_eq!(
        decode_bytes_with(&ctx, &anim, &OpenOptions::new().with_max_pixels(12))
            .unwrap()
            .len(),
        3
    );
    assert!(matches!(
        decode_bytes_with(&ctx, &anim, &OpenOptions::new().with_max_pixels(11)),
        Err(ImageError::LimitExceeded(_))
    ));
    // max_frames bounds what the budget has to cover.
    assert_eq!(
        decode_bytes_with(
            &ctx,
            &anim,
            &OpenOptions::new().with_max_pixels(8).with_max_frames(2)
        )
        .unwrap()
        .len(),
        2
    );
}

#[test]
fn decode_vec_matches_decode_bytes() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgba64Le).encode();
    let a = decode_bytes(&ctx, &bytes).unwrap().into_primary();
    let b = decode_vec(&ctx, bytes).unwrap().into_primary();
    assert_eq!(a.as_packed(), b.as_packed());
    assert_eq!(a.format(), b.format());
}

#[test]
fn quality_reaches_only_encoders_that_declare_it() {
    let ctx = fixture::ctx();
    let img = Image::from_rgba8(2, 2, vec![3; 16]).unwrap();
    let meta_of = |bytes: &[u8]| decode_bytes(&ctx, bytes).unwrap().metadata().to_vec();
    // `oxim` declares "quality".
    let bytes = encode(&ctx, &img, "oxim", &SaveOptions::new().with_quality(80)).unwrap();
    assert!(meta_of(&bytes).contains(&("quality".to_string(), "80".to_string())));
    assert!(meta_of(&bytes).contains(&("muxer".to_string(), "oxim".to_string())));
    // Clamped to 100.
    let bytes = encode(&ctx, &img, "oxim", &SaveOptions::new().with_quality(250)).unwrap();
    assert!(meta_of(&bytes).contains(&("quality".to_string(), "100".to_string())));
    // No quality asked: nothing forwarded.
    let bytes = encode(&ctx, &img, "oxim", &SaveOptions::default()).unwrap();
    assert!(!meta_of(&bytes).iter().any(|(k, _)| k == "quality"));
    // `oxim_yuv` declares nothing: quality is dropped, not an error.
    let bytes = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new()
            .with_codec(fixture::CODEC_YUV_ONLY)
            .with_quality(80),
    )
    .unwrap();
    assert!(!meta_of(&bytes).iter().any(|(k, _)| k == "quality"));
    // Explicit options are verbatim and the encoder's strictness shows.
    let err = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_option("bogus", "1"),
    )
    .unwrap_err();
    assert!(matches!(err, ImageError::Core(_)), "{err:?}");
    let bytes = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_option("quality", "42"),
    )
    .unwrap();
    assert!(meta_of(&bytes).contains(&("quality".to_string(), "42".to_string())));
    // Discovery.
    let schema = encoder_options(&ctx, "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(schema.len(), 2);
    assert_eq!(schema[0].name, "quality");
    assert_eq!(schema[1].name, "threads");
    let none = encoder_options(
        &ctx,
        "oxim",
        &SaveOptions::new().with_codec(fixture::CODEC_YUV_ONLY),
    )
    .unwrap();
    assert!(none.is_empty());
    assert!(matches!(
        encoder_options(&ctx, "nope", &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
}

#[test]
fn can_encode_and_resolve_target_answer_without_pixels() {
    let ctx = fixture::ctx();
    assert!(can_encode(&ctx, "oxim"));
    assert!(can_encode(&ctx, ".OXIM"));
    // `oximrgb` names no encoder of its own: not writable by default,
    // writable once the payload codec is named.
    assert!(!can_encode(&ctx, "oximrgb"));
    assert_eq!(
        resolve_target(
            &ctx,
            "oximrgb",
            &SaveOptions::new().with_codec(fixture::CODEC)
        )
        .unwrap()
        .container(),
        Some("oximrgb")
    );
    assert!(!can_encode(&ctx, "nope"));
    // A demuxer without a muxer cannot be written.
    assert!(!can_encode(&ctx, "oximnodec"));
    // A codec registered without any container is codec-only.
    assert!(can_encode(&ctx, fixture::CODEC_YUV_ONLY));

    let t = resolve_target(&ctx, "OXIM", &SaveOptions::default()).unwrap();
    assert_eq!(t.format(), "oxim");
    assert_eq!(t.container(), Some("oxim"));
    assert!(!t.is_codec_only());
    assert_eq!(t.codec(), fixture::CODEC);
    assert_eq!(t.accepted_pixel_formats(), FORMATS);
    assert_eq!(t.options().len(), 2);
    assert_eq!(t.options()[0].name, "quality");
    // The ladder a declared set yields: only declared layouts.
    let l = t.ladder(PixelFormat::Rgb24, &SaveOptions::default());
    assert_eq!(l[0], PixelFormat::Rgb24);
    assert!(l.iter().all(|f| FORMATS.contains(f)));
    assert_eq!(
        t.ladder(
            PixelFormat::Rgb24,
            &SaveOptions::new().with_pixel_format(PixelFormat::Gray8)
        ),
        vec![PixelFormat::Gray8]
    );

    let opts = SaveOptions::new().with_codec(fixture::CODEC_UNDECLARED);
    let t = resolve_target(&ctx, "oxim", &opts).unwrap();
    assert_eq!(t.codec(), fixture::CODEC_UNDECLARED);
    assert!(t.accepted_pixel_formats().is_empty());
    assert!(t.options().is_empty());
    let l = t.ladder(PixelFormat::Rgb24, &opts);
    assert_eq!(l[0], PixelFormat::Rgb24);
    assert!(l.contains(&PixelFormat::Yuv444P));
    assert_eq!(l.last(), Some(&PixelFormat::Gray16Le));

    let t = resolve_target(&ctx, fixture::CODEC_YUV_ONLY, &SaveOptions::default()).unwrap();
    assert!(t.is_codec_only());
    assert_eq!(t.container(), None);
    assert_eq!(t.format(), fixture::CODEC_YUV_ONLY);
    assert_eq!(t.accepted_pixel_formats(), &[PixelFormat::Yuv420P]);

    assert!(matches!(
        resolve_target(&ctx, "nope", &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
    assert!(matches!(
        resolve_target(&ctx, "oximnodec", &SaveOptions::default()),
        Err(ImageError::UnknownFormat(_))
    ));
    assert!(matches!(
        resolve_target(
            &ctx,
            "oxim",
            &SaveOptions::new().with_codec("no_such_codec")
        ),
        Err(ImageError::Unsupported(_))
    ));
}

#[test]
fn allow_and_deny_lists_refuse_before_decoding() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgb24).encode();
    let restricted = |opts: &OpenOptions| match decode_bytes_with(&ctx, &bytes, opts) {
        Err(ImageError::Restricted(msg)) => msg,
        other => panic!("expected Restricted, got {other:?}"),
    };
    // Containers: deny wins, allow must contain, both case-insensitive.
    let msg = restricted(&OpenOptions::new().with_deny_containers(["OXIM"]));
    assert!(msg.contains("container 'oxim'"), "{msg}");
    restricted(&OpenOptions::new().with_allow_containers(["png", "oximrgb"]));
    assert!(decode_bytes_with(
        &ctx,
        &bytes,
        &OpenOptions::new().with_allow_containers(["Oxim"])
    )
    .is_ok());
    restricted(
        &OpenOptions::new()
            .with_allow_containers(["oxim"])
            .with_deny_containers(["oxim"]),
    );
    // An empty allow list allows nothing; a cleared one allows all.
    restricted(&OpenOptions::new().with_allow_containers(Vec::<String>::new()));
    let mut opts = OpenOptions::new().with_allow_containers(["nothing"]);
    opts.allow_containers = None;
    assert!(decode_bytes_with(&ctx, &bytes, &opts).is_ok());
    // Codecs: from the stream table, before any decoder exists.
    let msg = restricted(&OpenOptions::new().with_deny_codecs([fixture::CODEC]));
    assert!(
        msg.contains("codec 'oxim'") && msg.contains("stream 0"),
        "{msg}"
    );
    restricted(&OpenOptions::new().with_allow_codecs(["png"]));
    assert!(decode_bytes_with(
        &ctx,
        &bytes,
        &OpenOptions::new().with_allow_codecs(["OXIM"])
    )
    .is_ok());
    // A stream whose codec nobody decodes is still refused by its id
    // (Restricted outranks NoImage: the list is checked first).
    let opts = OpenOptions::new()
        .with_ext_hint("oximnodec")
        .with_deny_codecs([fixture::CODEC_NO_DECODER]);
    assert!(matches!(
        decode_bytes_with(&ctx, &bytes, &opts),
        Err(ImageError::Restricted(_))
    ));
    // The hint selects the container before the lists see it.
    let opts = OpenOptions::new()
        .with_ext_hint("oximrgb")
        .with_allow_containers(["oximrgb"]);
    assert_eq!(
        decode_bytes_with(&ctx, &bytes, &opts).unwrap().container(),
        fixture::CONTAINER_RGB_ONLY
    );
}

#[test]
fn encode_frames_with_mixed_geometry_goes_to_a_multi_stream_container() {
    let ctx = fixture::ctx();
    let frames = vec![
        Image::from_rgb8(2, 2, vec![10; 12]).unwrap(),
        Image::from_rgb8(3, 3, vec![20; 27]).unwrap(),
        Image::from_rgb8(2, 2, vec![30; 12]).unwrap(),
        Image::from_rgba8(1, 4, vec![40; 16]).unwrap(),
    ];
    // Writable? The muxer decides, not the gateway: `oximmulti` yes.
    let multi = SaveOptions::new().with_codec(fixture::CODEC);
    let bytes = encode_frames(&ctx, &frames, fixture::CONTAINER_MULTI, &multi).unwrap();
    let hint = OpenOptions::new().with_ext_hint("oximmulti");
    let back = decode_bytes_with(&ctx, &bytes, &hint).unwrap();
    assert_eq!(back.container(), fixture::CONTAINER_MULTI);
    assert_eq!(back.len(), 4);
    // One stream per distinct geometry in first-appearance order; the
    // 2x2 pictures share stream 0.
    let mut seen: Vec<(usize, u32, u32, Vec<u8>)> = back
        .frames()
        .iter()
        .map(|i| (i.stream(), i.width(), i.height(), i.to_rgb8().unwrap()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            (0, 2, 2, vec![10; 12]),
            (0, 2, 2, vec![30; 12]),
            (1, 3, 3, vec![20; 27]),
            (2, 1, 4, vec![40; 12]),
        ]
    );
    // The same set into a one-geometry container is refused by type,
    // naming the geometries — never resized, never a panic.
    match encode_frames(&ctx, &frames, "oxim", &SaveOptions::default()) {
        Err(ImageError::Unsupported(msg)) => {
            assert!(msg.contains("3 picture geometries"), "{msg}");
            assert!(msg.contains("2x2, 3x3, 1x4"), "{msg}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // A codec-only target refuses before encoding anything.
    match encode_frames(
        &ctx,
        &frames,
        fixture::CODEC_YUV_ONLY,
        &SaveOptions::default(),
    ) {
        Err(ImageError::Unsupported(msg)) => assert!(msg.contains("one picture geometry"), "{msg}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // Uniform geometry through the multi-stream muxer is one stream.
    let same: Vec<Image> = (0..3u8)
        .map(|i| {
            Image::from_rgb8(2, 2, vec![i; 12])
                .unwrap()
                .with_delay(ms(25))
        })
        .collect();
    let bytes = encode_frames(&ctx, &same, fixture::CONTAINER_MULTI, &multi).unwrap();
    let back = decode_bytes_with(&ctx, &bytes, &hint).unwrap();
    assert!(back.frames().iter().all(|i| i.stream() == 0));
    assert!(back.frames().iter().all(|i| i.delay() == ms(25)));
}

// ---- metadata blobs (core 0.1.40) -------------------------------------------

fn blob(kind: BlobKind, data: &[u8]) -> MetadataBlob {
    MetadataBlob::new(kind, data.to_vec())
}

#[test]
fn stream_blobs_surface_on_the_file_and_on_every_picture() {
    let ctx = fixture::ctx();
    let fx = still(PixelFormat::Rgb24).with_blobs(vec![
        blob(BlobKind::ICC, b"acsp-profile"),
        blob(BlobKind::XMP, b"<x:xmpmeta/>"),
        MetadataBlob::new(BlobKind::custom("oxim-notes"), b"n".to_vec()).with_mime("text/plain"),
    ]);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert!(file.has_metadata());
    assert_eq!(file.blobs().len(), 3);
    assert_eq!(file.icc(), Some(&b"acsp-profile"[..]));
    assert_eq!(file.xmp(), Some(&b"<x:xmpmeta/>"[..]));
    assert_eq!(file.exif(), None);
    assert_eq!(file.iptc(), None);
    assert_eq!(
        file.blob(&BlobKind::custom("oxim-notes"))
            .and_then(|b| b.mime.as_deref()),
        Some("text/plain")
    );
    assert_eq!(file.stream_blobs(0).len(), 3);
    assert!(file.stream_blobs(7).is_empty());
    // The picture sees the stream's blobs through the precedence view.
    let img = file.primary();
    assert!(img.frame_blobs().is_empty());
    assert_eq!(img.stream_blobs().len(), 3);
    assert_eq!(img.blobs().len(), 3);
    assert_eq!(img.icc(), Some(&b"acsp-profile"[..]));
    assert_eq!(img.xmp(), Some(&b"<x:xmpmeta/>"[..]));
    assert!(img.has_metadata());
    // The frame handed back carries no blobs record (they are
    // stream-level); the parameters do.
    assert!(img.frame().blobs().is_empty());
    assert_eq!(img.params().blobs().len(), 3);
    // Views keep the stream blobs with the parameters.
    let rgba = img.to_format(PixelFormat::Rgba).unwrap();
    assert_eq!(rgba.icc(), Some(&b"acsp-profile"[..]));
}

#[test]
fn frame_blobs_override_stream_blobs_of_the_same_kind() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(4, 4, PixelFormat::Rgb24);
    fx.push_gradient(40, 1).push_gradient(40, 2);
    fx.last_frame().blobs = vec![
        blob(BlobKind::EXIF, b"II*\0page-2"),
        blob(BlobKind::EXIF, b"II*\0page-2-bis"),
    ];
    let fx = fx.with_blobs(vec![
        blob(BlobKind::EXIF, b"II*\0file"),
        blob(BlobKind::ICC, b"acsp"),
    ]);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    let [a, b] = file.frames() else {
        panic!("two pictures")
    };
    // Picture 0 has no frame blobs: the stream's Exif and ICC.
    assert_eq!(a.exif(), Some(&b"II*\0file"[..]));
    assert_eq!(a.icc(), Some(&b"acsp"[..]));
    assert_eq!(a.blobs().len(), 2);
    // Picture 1 carries its own Exif: it wins, every one of that kind;
    // the ICC still comes from the stream.
    assert_eq!(b.frame_blobs().len(), 2);
    assert_eq!(b.exif(), Some(&b"II*\0page-2"[..]));
    assert_eq!(b.blobs_of(&BlobKind::EXIF).len(), 2);
    assert_eq!(a.blobs_of(&BlobKind::EXIF).len(), 1);
    assert_eq!(b.icc(), Some(&b"acsp"[..]));
    let all = b.blobs();
    assert_eq!(all.len(), 3);
    assert!(all[..2].iter().all(|x| x.is(&BlobKind::EXIF)));
    assert!(all[2].is(&BlobKind::ICC));
    // The file-level accessors are stream level only.
    assert_eq!(file.exif(), Some(&b"II*\0file"[..]));
    // Frame blobs are lifted off the frame and re-attached on the way
    // out; crop and conversion keep them.
    assert!(b.frame().blobs().is_empty());
    assert_eq!(
        b.crop(1, 1, 2, 2).unwrap().exif(),
        Some(&b"II*\0page-2"[..])
    );
    assert_eq!(
        b.to_format(PixelFormat::Rgba).unwrap().frame_blobs().len(),
        2
    );
    let (frame, params) = b.clone().into_video_frame();
    assert_eq!(frame.blobs().len(), 2);
    assert_eq!(params.blobs().len(), 2);
    // Round trip through the constructor lifts them again.
    let again = Image::from_video_frame(frame, &params).unwrap();
    assert_eq!(again.frame_blobs().len(), 2);
    assert!(again.frame().blobs().is_empty());
}

#[test]
fn open_options_strip_metadata_or_kinds() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(4, 4, PixelFormat::Rgb24);
    fx.push_gradient(40, 1);
    fx.last_frame().blobs = vec![blob(BlobKind::EXIF, b"II*\0page")];
    fx.metadata = vec![("title".into(), "kept".into())];
    let fx = fx.with_blobs(vec![
        blob(BlobKind::ICC, b"acsp"),
        blob(BlobKind::XMP, b"<x/>"),
    ]);
    let bytes = fx.encode();
    let all = decode_bytes(&ctx, &bytes).unwrap();
    assert!(all.has_metadata());
    assert_eq!(all.primary().blobs().len(), 3);

    let none = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().without_metadata()).unwrap();
    assert!(!none.has_metadata());
    assert!(none.blobs().is_empty());
    assert!(none.primary().blobs().is_empty());
    assert!(none.primary().frame_blobs().is_empty());
    assert!(none.primary().params().blobs().is_empty());
    // String metadata is not a blob.
    assert_eq!(none.metadata(), fx.metadata.as_slice());
    let kept = decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_metadata(true)).unwrap();
    assert_eq!(kept.primary().blobs().len(), 3);

    let no_exif = decode_bytes_with(
        &ctx,
        &bytes,
        &OpenOptions::new().with_stripped_blobs([BlobKind::EXIF, BlobKind::XMP]),
    )
    .unwrap();
    let img = no_exif.primary();
    assert_eq!(img.exif(), None);
    assert_eq!(img.xmp(), None);
    assert_eq!(img.icc(), Some(&b"acsp"[..]));
    assert_eq!(no_exif.blobs().len(), 1);
    assert!(img.frame_blobs().is_empty());
}

#[test]
fn pictures_of_a_stream_share_its_parameters() {
    // A 40-frame file with a 64 KiB profile: the stream's blobs are
    // held once, not once per picture (40 × 64 KiB → 64 KiB), and the
    // sharing survives a post-open filter that drops nothing from the
    // stream level. A filter that does drop something copies once per
    // stream, not per picture.
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(2, 2, PixelFormat::Rgb24);
    for i in 0..40 {
        fx.push_gradient(10, i);
    }
    fx.last_frame().blobs = vec![blob(BlobKind::EXIF, b"II*\0last")];
    let fx = fx.with_blobs(vec![
        blob(BlobKind::ICC, &vec![7u8; 64 * 1024]),
        blob(BlobKind::XMP, b"<x/>"),
    ]);
    let mut file = decode_bytes(&ctx, &fx.encode()).unwrap();
    assert_eq!(file.len(), 40);
    let first = file.primary().params() as *const _;
    assert!(file
        .frames()
        .iter()
        .all(|i| std::ptr::eq(i.params(), first)));
    file.retain_blobs(|b| !b.is(&BlobKind::EXIF));
    let first = file.primary().params() as *const _;
    assert!(file
        .frames()
        .iter()
        .all(|i| std::ptr::eq(i.params(), first)));
    assert!(file.frames()[39].frame_blobs().is_empty());
    assert_eq!(file.frames()[39].icc().map(<[u8]>::len), Some(64 * 1024));
    file.retain_blobs(|b| !b.is(&BlobKind::XMP));
    let first = file.primary().params() as *const _;
    assert!(file
        .frames()
        .iter()
        .all(|i| std::ptr::eq(i.params(), first)));
    assert_eq!(file.blobs().len(), 1);
    assert_eq!(file.frames()[5].stream_blobs().len(), 1);
    // A picture given its own stream blobs stops sharing, alone.
    let own = file.frames()[3]
        .clone()
        .with_stream_blobs(vec![blob(BlobKind::XMP, b"<own/>")]);
    assert!(!std::ptr::eq(own.params(), first));
    assert!(std::ptr::eq(file.frames()[4].params(), first));
    assert_eq!(own.xmp(), Some(&b"<own/>"[..]));
    assert_eq!(file.frames()[3].xmp(), None);
    // into_video_frame hands a shared stream's parameters back by copy.
    let (_, params) = file.frames()[2].clone().into_video_frame();
    assert_eq!(params.blobs().len(), 1);
}

#[test]
fn image_file_strips_metadata_after_the_fact() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(4, 4, PixelFormat::Rgb24);
    fx.push_gradient(40, 1);
    fx.last_frame().blobs = vec![blob(BlobKind::EXIF, b"II*\0page")];
    fx.metadata = vec![("title".into(), "kept".into())];
    let fx = fx.with_blobs(vec![
        blob(BlobKind::ICC, b"acsp"),
        blob(BlobKind::XMP, b"<x/>"),
    ]);
    let mut file = decode_bytes(&ctx, &fx.encode()).unwrap();
    file.retain_blobs(|b| !b.is(&BlobKind::EXIF));
    assert_eq!(file.blobs().len(), 2);
    assert!(file.primary().frame_blobs().is_empty());
    assert_eq!(file.primary().exif(), None);
    assert_eq!(file.primary().icc(), Some(&b"acsp"[..]));
    file.strip_metadata();
    assert!(!file.has_metadata());
    assert!(file.blobs().is_empty());
    assert!(file.primary().stream_blobs().is_empty());
    assert_eq!(file.metadata(), fx.metadata.as_slice());
    // What the pictures now carry is what an encode hands on.
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, file.primary(), "oxim", &SaveOptions::default()).unwrap(),
    )
    .unwrap();
    assert!(!back.has_metadata());
}

#[test]
fn encode_hands_blobs_to_the_encoder_at_both_levels() {
    let ctx = fixture::ctx();
    let mut fx = Fixture::new(4, 4, PixelFormat::Rgb24);
    fx.push_gradient(40, 1).push_gradient(40, 2);
    fx.last_frame().blobs = vec![blob(BlobKind::EXIF, b"II*\0page-2")];
    let bytes = fx
        .with_blobs(vec![
            blob(BlobKind::ICC, b"acsp"),
            blob(BlobKind::XMP, b"<x/>"),
        ])
        .encode();
    let file = decode_bytes(&ctx, &bytes).unwrap();

    // Multi-picture: the stream gets the first picture's stream blobs,
    // each frame its own — the fixture muxer writes the stream blobs
    // and its encoder the frame blobs, so a decode shows the same split.
    let out = encode_frames(&ctx, file.frames(), "oxim", &SaveOptions::default()).unwrap();
    let back = decode_bytes(&ctx, &out).unwrap();
    assert_eq!(back.icc(), Some(&b"acsp"[..]));
    assert_eq!(back.xmp(), Some(&b"<x/>"[..]));
    assert_eq!(back.exif(), None);
    assert!(back.frames()[0].frame_blobs().is_empty());
    assert_eq!(back.frames()[1].exif(), Some(&b"II*\0page-2"[..]));
    assert_eq!(back.frames()[1].frame_blobs().len(), 1);

    // Single picture: its frame blobs are promoted to the stream as
    // well (a still encoder reads the parameters), frame first.
    let one = encode(&ctx, &file.frames()[1], "oxim", &SaveOptions::default()).unwrap();
    let back = decode_bytes(&ctx, &one).unwrap();
    assert_eq!(back.exif(), Some(&b"II*\0page-2"[..]));
    assert_eq!(back.icc(), Some(&b"acsp"[..]));
    assert_eq!(back.blobs()[0].kind, BlobKind::EXIF);
    assert_eq!(back.primary().frame_blobs().len(), 1);
    // The ladder path converts first; blobs still travel.
    let yuv = encode(
        &ctx,
        &file.frames()[1],
        "oxim",
        &SaveOptions::new().with_codec("oxim_yuv"),
    )
    .unwrap();
    let back = decode_bytes(&ctx, &yuv).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Yuv420P);
    assert_eq!(back.primary().exif(), Some(&b"II*\0page-2"[..]));
    assert_eq!(back.icc(), Some(&b"acsp"[..]));

    // SaveOptions blobs replace the source's of the same kind, and
    // nothing else; strip rules withhold the source's.
    let opts = SaveOptions::new()
        .with_blob(blob(BlobKind::ICC, b"acsp-new"))
        .with_blob(blob(BlobKind::IPTC, b"\x1c\x02"));
    let back = decode_bytes(&ctx, &encode(&ctx, file.primary(), "oxim", &opts).unwrap()).unwrap();
    assert_eq!(back.icc(), Some(&b"acsp-new"[..]));
    assert_eq!(back.iptc(), Some(&b"\x1c\x02"[..]));
    assert_eq!(back.xmp(), Some(&b"<x/>"[..]));
    assert_eq!(back.blobs().len(), 3);
    // Ruling (round 478): a blob set on the options is the one that
    // gets written, at both levels — the frame record of the same kind
    // is replaced too, so a reopen cannot let the old frame blob win.
    let opts = SaveOptions::new().with_blob(blob(BlobKind::EXIF, b"II*\0set"));
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, &file.frames()[1], "oxim", &opts).unwrap(),
    )
    .unwrap();
    assert_eq!(back.exif(), Some(&b"II*\0set"[..]));
    assert_eq!(back.primary().exif(), Some(&b"II*\0set"[..]));
    assert!(back.primary().frame_blobs().is_empty());
    assert_eq!(back.primary().blobs_of(&BlobKind::EXIF).len(), 1);
    let back = decode_bytes(
        &ctx,
        &encode_frames(&ctx, file.frames(), "oxim", &opts).unwrap(),
    )
    .unwrap();
    assert_eq!(back.exif(), Some(&b"II*\0set"[..]));
    assert!(back.frames()[1].frame_blobs().is_empty());
    assert_eq!(back.frames()[1].exif(), Some(&b"II*\0set"[..]));
    assert_eq!(back.icc(), Some(&b"acsp"[..]), "other kinds pass through");
    let opts = SaveOptions::new()
        .without_metadata()
        .with_blob(blob(BlobKind::ICC, b"only"));
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, &file.frames()[1], "oxim", &opts).unwrap(),
    )
    .unwrap();
    assert_eq!(back.blobs().len(), 1);
    assert_eq!(back.icc(), Some(&b"only"[..]));
    assert!(back.primary().frame_blobs().is_empty());
    let opts = SaveOptions::new().with_stripped_blobs([BlobKind::EXIF]);
    let back = decode_bytes(
        &ctx,
        &encode_frames(&ctx, file.frames(), "oxim", &opts).unwrap(),
    )
    .unwrap();
    assert_eq!(back.blobs().len(), 2);
    assert!(back.frames()[1].frame_blobs().is_empty());
    assert_eq!(back.frames()[1].exif(), None);

    // Caller-built pictures: blobs attached on the Image reach the
    // encoder the same way.
    let img = Image::from_rgb8(2, 2, vec![0; 12])
        .unwrap()
        .with_blob(blob(BlobKind::EXIF, b"II*\0built"))
        .with_stream_blobs(vec![blob(BlobKind::ICC, b"acsp-built")]);
    assert_eq!(img.exif(), Some(&b"II*\0built"[..]));
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, &img, "oxim", &SaveOptions::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(back.exif(), Some(&b"II*\0built"[..]));
    assert_eq!(back.icc(), Some(&b"acsp-built"[..]));
    assert_eq!(back.primary().exif(), Some(&b"II*\0built"[..]));
    let stripped = img.clone().without_metadata();
    assert!(!stripped.has_metadata());
    let mut retained = img.clone();
    retained.retain_blobs(|b| b.is(&BlobKind::ICC));
    assert_eq!(retained.exif(), None);
    assert_eq!(retained.icc(), Some(&b"acsp-built"[..]));
    let mut taken = img;
    assert_eq!(taken.take_blobs().len(), 1);
    assert!(taken.frame_blobs().is_empty());
}

#[test]
fn requested_brand_picks_the_codec_on_a_multi_codec_container() {
    // `oximmux` serves `.oximmux` and `.oximalt`; encoders `oximmux` and
    // `oximalt` are both registered. The requested name picks the
    // codec; the container's own name keeps its default; an explicit
    // codec wins over both.
    let ctx = fixture::ctx();
    let opts = SaveOptions::default();
    let t = resolve_target(&ctx, "oximalt", &opts).unwrap();
    assert_eq!(t.container(), Some("oximmux"));
    assert_eq!(t.format(), "oximalt");
    assert_eq!(t.codec(), "oximalt");
    let t = resolve_target(&ctx, ".OXIMALT", &opts).unwrap();
    assert_eq!(t.codec(), "oximalt");
    let t = resolve_target(&ctx, "oximmux", &opts).unwrap();
    assert_eq!(t.codec(), "oximmux");
    assert_eq!(t.format(), "oximmux");
    let t = resolve_target(&ctx, "oximalt", &SaveOptions::new().with_codec("oximmux")).unwrap();
    assert_eq!(t.codec(), "oximmux");
    // A name without an encoder of its own falls through unchanged (the
    // `oximrgb` container has no `oximrgb` codec: still `Unsupported`
    // without an explicit codec).
    assert!(matches!(
        resolve_target(&ctx, "oximrgb", &opts),
        Err(ImageError::Unsupported(_))
    ));
    // The encoder actually used is the one the muxer recorded.
    let img = Image::from_rgb8(2, 2, vec![0; 12]).unwrap();
    let codec_of = |format: &str, opts: &SaveOptions| {
        let bytes = encode(&ctx, &img, format, opts).unwrap();
        decode_bytes(&ctx, &bytes)
            .unwrap()
            .metadata()
            .iter()
            .find(|(k, _)| k == "codec")
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(codec_of("oximalt", &opts), "oximalt");
    assert_eq!(codec_of("oximmux", &opts), "oximmux");
    assert_eq!(
        codec_of("oximalt", &SaveOptions::new().with_codec("oximmux")),
        "oximmux"
    );
    assert_eq!(
        codec_of("oximmux", &SaveOptions::new().with_codec("oximalt")),
        "oximalt"
    );
}

#[test]
fn metadata_support_is_pass_through() {
    let ctx = fixture::ctx();
    let target = resolve_target(&ctx, "oxim", &SaveOptions::default()).unwrap();
    assert_eq!(target.metadata_support(), MetadataSupport::PassThrough);
}

// ---- palette alpha (core 0.1.40) --------------------------------------------

/// A 2×2 Pal8 picture over a four-entry palette whose entry 1 is
/// transparent and entry 2 half-transparent; entry 3 is uncovered by
/// the alpha record (opaque).
fn indexed_with_alpha() -> Image {
    let img = Image::from_indexed(
        2,
        2,
        vec![0, 1, 2, 3],
        &[
            [10, 20, 30, 255],
            [40, 50, 60, 0],
            [70, 80, 90, 128],
            [1, 2, 3, 255],
        ],
    )
    .unwrap();
    // Shorten the record: entry 3 is uncovered.
    let (mut frame, params) = img.into_video_frame();
    frame.set_palette_alpha(vec![255, 0, 128]);
    Image::from_video_frame(frame, &params).unwrap()
}

#[test]
fn palette_alpha_reaches_the_rgba_views_and_not_the_rgb_ones() {
    let img = indexed_with_alpha();
    assert_eq!(img.format(), PixelFormat::Pal8);
    assert_eq!(img.palette_alpha(), Some(&[255, 0, 128][..]));
    assert_eq!(img.palette_rgba(1), Some([40, 50, 60, 0]));
    assert_eq!(img.palette_rgba(3), Some([1, 2, 3, 255]));
    assert_eq!(img.palette_rgba(4), None);
    assert_eq!(
        img.palette_entries().unwrap(),
        vec![
            [10, 20, 30, 255],
            [40, 50, 60, 0],
            [70, 80, 90, 128],
            [1, 2, 3, 255]
        ]
    );
    assert!(img.has_palette_alpha());
    assert!(img.has_alpha());
    assert_eq!(
        img.to_rgba8().unwrap(),
        vec![10, 20, 30, 255, 40, 50, 60, 0, 70, 80, 90, 128, 1, 2, 3, 255]
    );
    // Alpha widens like any sample (0 → 0, 128 → 0x8080, 255 → 0xffff).
    let wide = img.to_rgba16().unwrap();
    assert_eq!(&wide[6..8], &[0xff, 0xff]);
    assert_eq!(&wide[14..16], &[0, 0]);
    assert_eq!(&wide[22..24], &[0x80, 0x80]);
    assert_eq!(&wide[..6], &[10, 10, 20, 20, 30, 30]);
    // RGB views drop the alpha: the stored RGB of a transparent entry.
    assert_eq!(
        img.to_rgb8().unwrap(),
        vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 1, 2, 3]
    );
    assert_eq!(&img.to_rgb16().unwrap()[6..12], &[40, 40, 50, 50, 60, 60]);
    assert_eq!(img.to_gray8().unwrap().len(), 4);
    // Other alpha layouts go through the expander too.
    let bgra = img.to_packed(PixelFormat::Bgra).unwrap();
    assert_eq!(&bgra[4..8], &[60, 50, 40, 0]);
    // Crop keeps both palette records.
    let c = img.crop(1, 0, 1, 2).unwrap();
    assert_eq!(c.palette_alpha(), Some(&[255, 0, 128][..]));
    assert_eq!(c.to_rgba8().unwrap(), vec![40, 50, 60, 0, 1, 2, 3, 255]);
    // An opaque palette is not alpha, with or without a record.
    let opaque = Image::from_indexed(1, 1, vec![0], &[[1, 2, 3, 255]]).unwrap();
    assert!(!opaque.has_palette_alpha());
    assert!(!opaque.has_alpha());
    assert_eq!(opaque.to_rgba8().unwrap(), vec![1, 2, 3, 255]);
    let plain = Image::from_raw(1, 1, PixelFormat::Pal8, vec![0]).unwrap();
    assert_eq!(plain.palette_alpha(), None);
    assert_eq!(plain.palette_entries(), None);
    assert!(!plain.has_alpha());
    assert!(Image::from_indexed(1, 1, vec![0], &[]).is_err());
    // An index past the palette is opaque black, as pixfmt expands it.
    let short = Image::from_indexed(1, 1, vec![5], &[[9, 9, 9, 0]]).unwrap();
    assert_eq!(short.to_rgba8().unwrap(), vec![0, 0, 0, 255]);
}

#[test]
fn palette_alpha_survives_the_registry_and_ranks_alpha_rungs_first() {
    let ctx = fixture::ctx();
    let img = indexed_with_alpha();
    // Encoded natively (oxim accepts Pal8): both records round-trip.
    let bytes = encode(&ctx, &img, "oxim", &SaveOptions::default()).unwrap();
    let back = decode_bytes(&ctx, &bytes).unwrap();
    let b = back.primary();
    assert_eq!(b.format(), PixelFormat::Pal8);
    assert_eq!(b.palette(), img.palette());
    assert_eq!(b.palette_alpha(), Some(&[255, 0, 128][..]));
    assert_eq!(b.to_rgba8().unwrap(), img.to_rgba8().unwrap());
    // Through a fixture file with the record shorter than the palette.
    let mut fx = Fixture::new(5, 3, PixelFormat::Pal8);
    fx.push_gradient(0, 1);
    fx.last_frame().palette_alpha = Some(vec![0]);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    let p = file.primary();
    assert!(p.has_alpha());
    let rgba = p.to_rgba8().unwrap();
    for (i, &idx) in fx.frames[0].planes[0].data.iter().enumerate() {
        assert_eq!(rgba[i * 4 + 3], if idx == 0 { 0 } else { 255 });
    }
    // The ladder: a transparent palette ranks Rgba ahead of Rgb24
    // against an encoder that declares both, and the picture reaches an
    // RGB-only encoder as Rgba with its alpha intact.
    let target = resolve_target(&ctx, "oxim", &SaveOptions::default()).unwrap();
    let l = target.ladder_for(&img, &SaveOptions::default());
    assert_eq!(l[0], PixelFormat::Pal8);
    assert!(l[1].has_alpha(), "{l:?}");
    assert!(
        l.iter().position(|f| *f == PixelFormat::Rgba)
            < l.iter().position(|f| *f == PixelFormat::Rgb24)
    );
    let l = target.ladder(PixelFormat::Pal8, &SaveOptions::default());
    assert_eq!(l[0], PixelFormat::Pal8);
    assert!(
        l.iter().position(|f| *f == PixelFormat::Rgb24)
            < l.iter().position(|f| *f == PixelFormat::Rgba)
    );
    let opaque = Image::from_indexed(2, 2, vec![0; 4], &[[1, 2, 3, 255]]).unwrap();
    let l = target.ladder_for(&opaque, &SaveOptions::default());
    assert!(
        l.iter().position(|f| *f == PixelFormat::Rgb24)
            < l.iter().position(|f| *f == PixelFormat::Rgba)
    );
    // oxim_undeclared takes Yuv420P / Yuv444P / Gray8 only: the
    // alpha-carrying rungs come first but none is accepted, so the
    // picture lands as 4:4:4 (alpha dropped, never composited).
    let yuv = encode(
        &ctx,
        &img,
        "oxim",
        &SaveOptions::new().with_codec(fixture::CODEC_UNDECLARED),
    )
    .unwrap();
    let back = decode_bytes(&ctx, &yuv).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Yuv444P);
    assert!(!back.primary().has_alpha());
}

#[test]
fn is_opaque_reads_the_alpha_samples() {
    use PixelFormat as P;
    // No alpha at all.
    assert!(Image::from_rgb8(2, 1, vec![0; 6]).unwrap().is_opaque());
    // Packed 8-bit: every alpha byte full, or one short.
    let full = Image::from_rgba8(2, 2, [7, 8, 9, 255].repeat(4)).unwrap();
    assert!(full.is_opaque());
    assert!(!full.has_transparency());
    assert!(full.has_alpha(), "the layout still has an alpha channel");
    let mut px = [7, 8, 9, 255].repeat(4);
    px[15] = 254;
    let one = Image::from_rgba8(2, 2, px).unwrap();
    assert!(!one.is_opaque());
    assert!(one.has_transparency());
    for f in [P::Bgra, P::Argb, P::Abgr] {
        let i = Image::from_raw(2, 1, f, vec![255; 8]).unwrap();
        assert!(i.is_opaque(), "{f:?}");
        let mut px = vec![255; 8];
        px[if f == P::Bgra { 3 } else { 4 }] = 0;
        assert!(!Image::from_raw(2, 1, f, px).unwrap().is_opaque(), "{f:?}");
    }
    assert!(Image::from_raw(2, 1, P::Ya8, vec![1, 255, 2, 255])
        .unwrap()
        .is_opaque());
    assert!(!Image::from_raw(2, 1, P::Ya8, vec![1, 255, 2, 0])
        .unwrap()
        .is_opaque());
    // 16-bit and float packed.
    let mut w = Vec::new();
    for _ in 0..2 {
        for v in [1u16, 2, 3, u16::MAX] {
            w.extend_from_slice(&v.to_le_bytes());
        }
    }
    assert!(Image::from_raw(2, 1, P::Rgba64Le, w.clone())
        .unwrap()
        .is_opaque());
    w[14] = 0xFE;
    assert!(!Image::from_raw(2, 1, P::Rgba64Le, w).unwrap().is_opaque());
    let mut fl = Vec::new();
    for v in [0.1f32, 0.2, 0.3, 1.0] {
        fl.extend_from_slice(&v.to_le_bytes());
    }
    assert!(Image::from_raw(1, 1, P::RgbaF32Le, fl.clone())
        .unwrap()
        .is_opaque());
    fl[12..16].copy_from_slice(&0.999f32.to_le_bytes());
    assert!(!Image::from_raw(1, 1, P::RgbaF32Le, fl).unwrap().is_opaque());
    // Planar: the fourth plane.
    let mut planes = fixture::gradient_planes(4, 2, P::Yuva420P, 0);
    planes[3].data.fill(255);
    let img = Image::from_planes(4, 2, P::Yuva420P, planes.clone()).unwrap();
    assert!(img.is_opaque());
    planes[3].data[5] = 1;
    assert!(!Image::from_planes(4, 2, P::Yuva420P, planes)
        .unwrap()
        .is_opaque());
    // Palette: the record decides.
    assert!(!indexed_with_alpha().is_opaque());
    let opaque_pal = Image::from_indexed(1, 1, vec![0], &[[1, 2, 3, 255]]).unwrap();
    assert!(opaque_pal.is_opaque());
}

#[test]
fn opaque_alpha_saves_as_the_opaque_layout_of_its_model() {
    // Ruling (round 478): an alpha source whose alpha is all full is
    // offered the opaque layout of the same model first; a transparent
    // one keeps its own layout first.
    let ctx = fixture::ctx();
    let rgb: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 5) as u8).collect();
    let opaque: Vec<u8> = rgb
        .chunks(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect();
    let img = Image::from_rgba8(4, 4, opaque.clone()).unwrap();
    let t = resolve_target(&ctx, "oxim", &SaveOptions::default()).unwrap();
    let l = t.ladder_for(&img, &SaveOptions::default());
    assert_eq!(&l[..2], &[PixelFormat::Rgb24, PixelFormat::Rgba], "{l:?}");
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, &img, "oxim", &SaveOptions::default()).unwrap(),
    )
    .unwrap()
    .into_primary();
    assert_eq!(back.format(), PixelFormat::Rgb24);
    assert_eq!(back.as_packed(), Some(rgb.as_slice()));
    // One translucent sample: the alpha layout stays first.
    let mut px = opaque;
    px[3] = 200;
    let img = Image::from_rgba8(4, 4, px.clone()).unwrap();
    let l = t.ladder_for(&img, &SaveOptions::default());
    assert_eq!(l[0], PixelFormat::Rgba, "{l:?}");
    let at = |f: PixelFormat| l.iter().position(|x| *x == f).unwrap();
    assert!(at(PixelFormat::Bgra) < at(PixelFormat::Rgb24), "{l:?}");
    assert!(at(PixelFormat::Rgb24) < at(PixelFormat::Gbrp8), "{l:?}");
    let back = decode_bytes(
        &ctx,
        &encode(&ctx, &img, "oxim", &SaveOptions::default()).unwrap(),
    )
    .unwrap()
    .into_primary();
    assert_eq!(back.format(), PixelFormat::Rgba);
    assert_eq!(back.as_packed(), Some(px.as_slice()));
    // The layout-only ladder (no pixels) keeps the layout's word.
    assert_eq!(
        t.ladder(PixelFormat::Rgba, &SaveOptions::default())[0],
        PixelFormat::Rgba
    );
    // A forced layout is still the only candidate.
    let forced = SaveOptions::new().with_pixel_format(PixelFormat::Rgba);
    let back = decode_bytes(&ctx, &encode(&ctx, &back, "oxim", &forced).unwrap())
        .unwrap()
        .into_primary();
    assert_eq!(back.format(), PixelFormat::Rgba);
}

#[test]
fn cmyk_is_offered_only_after_every_additive_layout() {
    // The round-477 defect: an `Rgba` source saved to a JPEG encoder
    // (which declares CMYK among its layouts) arrived as four-ink CMYK
    // because `Cmyk` and `Rgb24` tied as "RGB-family, no alpha" and
    // sorted by name. Colour-model distance now decides first.
    let ctx = fixture::ctx();
    let opts = SaveOptions::new().with_codec(fixture::CODEC_JPEG_LIKE);
    let t = resolve_target(&ctx, "oxim", &opts).unwrap();
    assert_eq!(t.accepted_pixel_formats(), fixture::JPEG_LIKE_FORMATS);
    let rgb: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 5) as u8).collect();
    for alpha in [255u8, 10] {
        let px: Vec<u8> = rgb
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], alpha])
            .collect();
        let img = Image::from_rgba8(4, 4, px).unwrap();
        let l = t.ladder_for(&img, &opts);
        assert_eq!(l[0], PixelFormat::Rgb24, "alpha {alpha}: {l:?}");
        let cmyk = l.iter().position(|f| *f == PixelFormat::Cmyk).unwrap();
        for f in [
            PixelFormat::Gbrp8,
            PixelFormat::Rgb48Le,
            PixelFormat::YuvJ420P,
            PixelFormat::Yuv444P,
            PixelFormat::Yuv420P,
        ] {
            let at = l.iter().position(|x| *x == f).unwrap();
            assert!(at < cmyk, "alpha {alpha}: {f:?} after Cmyk in {l:?}");
        }
        let grey = l.iter().position(|f| *f == PixelFormat::Gray8).unwrap();
        assert!(cmyk < grey, "alpha {alpha}: {l:?}");
        let back = decode_bytes(&ctx, &encode(&ctx, &img, "oxim", &opts).unwrap())
            .unwrap()
            .into_primary();
        assert_eq!(back.format(), PixelFormat::Rgb24, "alpha {alpha}");
        assert_eq!(back.as_packed(), Some(rgb.as_slice()), "alpha {alpha}");
    }
    // A YCbCr source stays in its model; grey stays grey; CMYK stays
    // CMYK — and a CMYK source offered no CMYK rung goes to RGB, not
    // grey.
    let yuv = decode_bytes(&ctx, &still(PixelFormat::Yuv420P).encode())
        .unwrap()
        .into_primary();
    let l = t.ladder_for(&yuv, &opts);
    assert_eq!(&l[..2], &[PixelFormat::Yuv420P, PixelFormat::YuvJ420P]);
    let back = decode_bytes(&ctx, &encode(&ctx, &yuv, "oxim", &opts).unwrap()).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Yuv420P);
    let grey = Image::from_raw(4, 4, PixelFormat::Gray8, vec![9; 16]).unwrap();
    let back = decode_bytes(&ctx, &encode(&ctx, &grey, "oxim", &opts).unwrap()).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Gray8);
    let cmyk = decode_bytes(&ctx, &still(PixelFormat::Cmyk).encode())
        .unwrap()
        .into_primary();
    assert_eq!(t.ladder_for(&cmyk, &opts)[0], PixelFormat::Cmyk);
    let back = decode_bytes(&ctx, &encode(&ctx, &cmyk, "oxim", &opts).unwrap()).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Cmyk);
    let rgb_only = SaveOptions::new().with_codec(fixture::CODEC_UNDECLARED);
    let back = decode_bytes(&ctx, &encode(&ctx, &cmyk, "oxim", &rgb_only).unwrap()).unwrap();
    assert_eq!(back.primary().format(), PixelFormat::Yuv444P);
    // Through the undeclared rungs CMYK is never offered.
    let u = resolve_target(&ctx, "oxim", &rgb_only).unwrap();
    assert!(!u
        .ladder(PixelFormat::Rgba, &rgb_only)
        .contains(&PixelFormat::Cmyk));
}

/// The value of a custom blob the fixture decoder attaches.
fn report(img: &Image, kind: &str) -> Option<usize> {
    img.blob(&BlobKind::custom(kind))
        .map(|b| String::from_utf8_lossy(&b.data).parse().unwrap())
}

#[test]
fn execution_budget_reaches_every_decoder_and_encoder() {
    let ctx = fixture::ctx();
    let bytes = still(PixelFormat::Rgb24).encode();
    let host = ExecutionContext::auto().threads;
    // Default: the host's parallelism, through `set_execution_context`
    // and through the decoder's declared `threads` option alike.
    let opts = OpenOptions::new().with_decoder_option("report_threads", "1");
    let img = decode_bytes_with(&ctx, &bytes, &opts)
        .unwrap()
        .into_primary();
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(host));
    assert_eq!(report(&img, fixture::BLOB_THREADS_OPTION), Some(host));
    assert_eq!(opts.execution_context().threads, host);
    // Explicit budgets.
    let img = decode_bytes_with(&ctx, &bytes, &opts.clone().with_threads(3))
        .unwrap()
        .into_primary();
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(3));
    assert_eq!(report(&img, fixture::BLOB_THREADS_OPTION), Some(3));
    let img = decode_bytes_with(&ctx, &bytes, &opts.clone().serial())
        .unwrap()
        .into_primary();
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(1));
    assert_eq!(report(&img, fixture::BLOB_THREADS_OPTION), Some(1));
    let img = decode_bytes_with(
        &ctx,
        &bytes,
        &opts
            .clone()
            .with_execution_context(ExecutionContext::with_threads(0)),
    )
    .unwrap()
    .into_primary();
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(1), "clamped up");
    // A caller-set `threads` option wins over the forwarded budget on
    // that channel; the context channel still carries the budget.
    let img = decode_bytes_with(
        &ctx,
        &bytes,
        &opts
            .clone()
            .with_threads(6)
            .with_decoder_option("threads", "2"),
    )
    .unwrap()
    .into_primary();
    assert_eq!(report(&img, fixture::BLOB_THREADS_OPTION), Some(2));
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(6));
    // A decoder that declares no `threads` option gets the context only
    // (no unknown option is ever set): the `oximmux` container's streams
    // name its own codec, which declares no decoder options.
    assert!(ctx
        .codecs
        .decoder_options_schema(&oxideav_core::CodecId::new(fixture::CONTAINER_MUX))
        .is_none());
    let img = decode_bytes_with(
        &ctx,
        &bytes,
        &opts.clone().with_threads(5).with_ext_hint("oximmux"),
    )
    .unwrap()
    .into_primary();
    assert_eq!(report(&img, fixture::BLOB_BUDGET), Some(5));
    assert_eq!(report(&img, fixture::BLOB_THREADS_OPTION), None);

    // Encoders: the budget is echoed as `budget=N` (context) and
    // `threads=N` (declared option) in the file metadata.
    let meta = |bytes: &[u8]| -> Vec<(String, String)> {
        decode_bytes(&ctx, bytes).unwrap().metadata().to_vec()
    };
    let src = Image::from_rgb8(2, 2, vec![1; 12]).unwrap();
    let m = meta(&encode(&ctx, &src, "oxim", &SaveOptions::default()).unwrap());
    assert!(m.contains(&("budget".into(), host.to_string())), "{m:?}");
    assert!(m.contains(&("threads".into(), host.to_string())), "{m:?}");
    let m = meta(&encode(&ctx, &src, "oxim", &SaveOptions::new().with_threads(4)).unwrap());
    assert!(m.contains(&("budget".into(), "4".into())), "{m:?}");
    assert!(m.contains(&("threads".into(), "4".into())), "{m:?}");
    let m = meta(&encode(&ctx, &src, "oxim", &SaveOptions::new().serial()).unwrap());
    assert!(m.contains(&("budget".into(), "1".into())), "{m:?}");
    let m = meta(
        &encode(
            &ctx,
            &src,
            "oxim",
            &SaveOptions::new()
                .with_threads(4)
                .with_option("threads", "2"),
        )
        .unwrap(),
    );
    assert!(m.contains(&("threads".into(), "2".into())), "{m:?}");
    assert!(m.contains(&("budget".into(), "4".into())), "{m:?}");
    // An encoder without a `threads` option: context only, no option
    // (it would refuse an unknown name).
    let m = meta(
        &encode(
            &ctx,
            &src,
            "oxim",
            &SaveOptions::new()
                .with_codec(fixture::CODEC_YUV_ONLY)
                .with_threads(7),
        )
        .unwrap(),
    );
    assert!(m.contains(&("budget".into(), "7".into())), "{m:?}");
    assert!(!m.iter().any(|(k, _)| k == "threads"), "{m:?}");
    // Multi-picture encodes reach every encoder the gateway creates.
    let mut fx = Fixture::new(2, 2, PixelFormat::Rgb24);
    fx.push_gradient(40, 1).push_gradient(40, 2);
    let file = decode_bytes(&ctx, &fx.encode()).unwrap();
    let m = meta(
        &encode_frames(
            &ctx,
            file.frames(),
            "oxim",
            &SaveOptions::new().with_threads(2),
        )
        .unwrap(),
    );
    assert!(m.contains(&("budget".into(), "2".into())), "{m:?}");
}

#[test]
fn decoder_output_label_wins_over_the_stream_label() {
    let ctx = fixture::ctx();
    for (f, wrong) in [
        (PixelFormat::Rgb24, PixelFormat::Bgr24),
        (PixelFormat::Rgba, PixelFormat::Bgra),
        (PixelFormat::Gray8, PixelFormat::Pal8),
    ] {
        let fx = still(f);
        let bytes = fx.encode();
        // Through the honest container the stream label stands.
        let plain = decode_bytes(&ctx, &bytes).unwrap();
        assert_eq!(plain.primary().format(), f);
        assert_eq!(plain.primary().params().pixel_format, Some(f));
        // The mislabelling container says `wrong`; the decoder knows
        // better and the picture is reported as what it emitted.
        let opts = OpenOptions::new().with_ext_hint("oximlabel");
        let file = decode_bytes_with(&ctx, &bytes, &opts).unwrap();
        assert_eq!(file.container(), fixture::CONTAINER_MISLABELLED);
        let img = file.primary();
        assert_eq!(img.format(), f, "{f:?} labelled {wrong:?}");
        assert_eq!(img.params().pixel_format, Some(f));
        assert_eq!(
            img.as_packed(),
            Some(fx.frames[0].planes[0].data.as_slice())
        );
        assert_ne!(img.format(), wrong);
        // A stream with no label at all opens when the decoder reports
        // one (it was `InvalidData` before).
        let opts = OpenOptions::new().with_ext_hint("oximnolabel");
        let file = decode_bytes_with(&ctx, &bytes, &opts).unwrap();
        assert_eq!(file.container(), fixture::CONTAINER_UNLABELLED);
        assert_eq!(file.primary().format(), f);
        assert_eq!(file.stream_blobs(0).len(), 0);
    }
    // Pictures of one stream keep sharing one parameter set after the
    // relabel; stream blobs survive it.
    let mut fx = Fixture::new(3, 2, PixelFormat::Rgb24);
    fx.push_gradient(40, 1).push_gradient(40, 2);
    let bytes = fx.with_blobs(vec![blob(BlobKind::ICC, b"acsp")]).encode();
    let opts = OpenOptions::new().with_ext_hint("oximlabel");
    let file = decode_bytes_with(&ctx, &bytes, &opts).unwrap();
    assert_eq!(file.len(), 2);
    assert!(file.iter().all(|i| i.format() == PixelFormat::Rgb24));
    assert!(std::ptr::eq(
        file.frames()[0].params(),
        file.frames()[1].params()
    ));
    assert_eq!(file.icc(), Some(&b"acsp"[..]));
    assert_eq!(file.frames()[1].icc(), Some(&b"acsp"[..]));
    // A label the frame cannot be (wrong plane count) is still refused.
    // (The honest container with a planar source: the decoder's label
    // and the stream's agree, nothing to refuse.)
    let planar = decode_bytes(&ctx, &still(PixelFormat::Yuv420P).encode()).unwrap();
    assert_eq!(planar.primary().format(), PixelFormat::Yuv420P);
}

#[test]
fn support_answers_the_registry_questions_without_a_file() {
    let ctx = fixture::ctx();
    let full = support(&ctx, ".OXIM");
    assert_eq!(full.format(), "oxim");
    assert_eq!(full.container(), Some("oxim"));
    assert!(full.has_demuxer() && full.has_muxer());
    assert!(full.has_decoder() && full.has_encoder());
    assert!(full.can_open() && full.can_save());
    assert!(!full.is_codec_only() && !full.is_unknown());
    assert!(can_open(&ctx, "oxim") && can_encode(&ctx, "oxim"));
    // Demuxer only, and its stream codec has no decoder.
    let nodec = support(&ctx, "oximnodec");
    assert_eq!(nodec.container(), Some("oximnodec"));
    assert!(nodec.has_demuxer() && !nodec.has_muxer());
    assert!(!nodec.has_decoder() && !nodec.has_encoder());
    assert!(nodec.can_open() && !nodec.can_save());
    assert!(!nodec.is_codec_only());
    assert!(can_open(&ctx, "oximnodec") && !can_encode(&ctx, "oximnodec"));
    // A codec registered without a container.
    let yuv = support(&ctx, fixture::CODEC_YUV_ONLY);
    assert_eq!(yuv.container(), None);
    assert!(yuv.has_decoder() && yuv.has_encoder());
    assert!(yuv.is_codec_only());
    assert!(!yuv.can_open() && yuv.can_save());
    assert!(!can_open(&ctx, fixture::CODEC_YUV_ONLY));
    assert!(can_encode(&ctx, fixture::CODEC_YUV_ONLY));
    // A brand of a multi-codec container resolves through the
    // requested-name rule.
    let alt = support(&ctx, "oximalt");
    assert_eq!(alt.container(), Some("oximmux"));
    assert!(alt.has_encoder() && alt.can_save() && alt.can_open());
    // Nothing at all.
    let none = support(&ctx, "nope");
    assert!(none.is_unknown());
    assert!(!none.can_open() && !none.can_save() && !none.is_codec_only());
    assert_eq!(
        codec_only_formats(
            &ctx,
            [
                "oxim",
                fixture::CODEC_YUV_ONLY,
                "oximnodec",
                "nope",
                ".OXIM_YUV"
            ]
        ),
        vec!["oxim_yuv".to_string(), "oxim_yuv".to_string()]
    );
    // `can_open` and `can_encode` agree with what open / encode do.
    let img = Image::from_rgb8(1, 1, vec![0; 3]).unwrap();
    for name in [
        "oxim",
        "oximrgb",
        "oximnodec",
        fixture::CODEC_YUV_ONLY,
        "nope",
    ] {
        let s = support(&ctx, name);
        let saved = encode(&ctx, &img, name, &SaveOptions::default());
        assert_eq!(s.can_save(), saved.is_ok(), "{name}: {saved:?}");
        assert_eq!(s.can_save(), can_encode(&ctx, name), "{name}");
    }
}
