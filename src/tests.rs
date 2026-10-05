//! Gateway tests against the synthetic OXIM container + codec pair in
//! [`crate::fixture`]. No file I/O except where marked `not(miri)`.

use std::time::Duration;

use oxideav_core::{PixelFormat, TimeBase, VideoPlane};

use crate::fixture::{self, Fixture, FORMATS};
use crate::{
    decode_bytes, decode_bytes_with, decode_vec, encode, encode_frames, encoder_options, Image,
    ImageError, OpenOptions, SaveOptions,
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
    let other = Image::from_rgb8(3, 2, vec![0; 18]).unwrap();
    assert!(matches!(
        encode_frames(&ctx, &[img.clone(), other], "oxim", &SaveOptions::default()),
        Err(ImageError::InvalidData(_))
    ));
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
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name, "quality");
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
