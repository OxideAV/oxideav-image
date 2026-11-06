//! Picture sequences whose shape changes mid-stream: the gateway takes
//! each picture's shape from the stream parameters kept current by the
//! decoder's in-band `Frame::StreamParams` event or, for a decoder that
//! does not emit it, by its `output_params_generation` — never from a
//! guess — and never counts the event as a picture. The layer
//! operations re-plan at the change, and an encode never feeds an
//! encoder a frame of another shape.

use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecResolver, Decoder, Demuxer,
    Encoder, Error, Frame, Packet, PixelFormat, ReadSeek, Result, RuntimeContext, StreamInfo,
    TimeBase, VideoFrame, VideoPlane,
};

use crate::{
    Image, ImageError, OpenOptions, SaveOptions, decode_bytes_with, encode_frames, layers,
};

const CODEC: &str = "oxim_shift";
const STRICT: &str = "oxim_strict";

/// How the stub decoder announces a change of shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `Frame::StreamParams` before the first picture of the new
    /// shape, generation bumped with it.
    Event = 0,
    /// Generation bump only (a decoder that does not emit the event).
    Generation = 1,
    /// Neither: a decoder breaking the contract.
    Silent = 2,
    /// As `Event`, every picture held until `flush` — the event comes
    /// out while the gateway drains the flushed decoder.
    EventAtFlush = 3,
    /// The event without a generation bump — what a stage that only
    /// forwards frames hands on: the event alone must relabel.
    EventOnly = 4,
}

impl Mode {
    fn of(b: u8) -> Mode {
        match b {
            0 => Mode::Event,
            1 => Mode::Generation,
            2 => Mode::Silent,
            3 => Mode::EventAtFlush,
            _ => Mode::EventOnly,
        }
    }
}

/// One picture of the script: `(width, height, layout, seed)`.
type Pic = (u8, u8, PixelFormat, u8);

fn tag(f: PixelFormat) -> u8 {
    match f {
        PixelFormat::Rgba => 0,
        PixelFormat::Gray8 => 1,
        PixelFormat::Rgb24 => 2,
        _ => 3,
    }
}

fn layout(t: u8) -> PixelFormat {
    match t {
        0 => PixelFormat::Rgba,
        1 => PixelFormat::Gray8,
        2 => PixelFormat::Rgb24,
        _ => PixelFormat::Yuv420P,
    }
}

/// The file: mode byte, then four bytes per picture.
fn file(mode: Mode, pics: &[Pic]) -> Vec<u8> {
    let mut b = vec![mode as u8];
    for &(w, h, f, seed) in pics {
        b.extend_from_slice(&[w, h, tag(f), seed]);
    }
    b
}

/// The tight planes of a scripted picture.
fn planes(&(w, h, f, seed): &Pic) -> Vec<VideoPlane> {
    (0..f.plane_count())
        .map(|p| {
            let row = f.plane_row_bytes(p, u32::from(w)).unwrap();
            let (_, rows) = f.plane_dimensions(p, u32::from(w), u32::from(h)).unwrap();
            let data = (0..row * rows as usize)
                .map(|i| seed.wrapping_add((i * 7 + p * 31) as u8))
                .collect();
            VideoPlane { stride: row, data }
        })
        .collect()
}

fn pic_of(b: &[u8]) -> Pic {
    (b[0], b[1], layout(b[2]), b[3])
}

fn params_of(&(w, h, f, _): &Pic) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(CODEC));
    p.width = Some(u32::from(w));
    p.height = Some(u32::from(h));
    p.pixel_format = Some(f);
    p
}

struct Demux {
    streams: Vec<StreamInfo>,
    pics: VecDeque<Pic>,
    next: i64,
}

impl Demuxer for Demux {
    fn format_name(&self) -> &str {
        CODEC
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        let (w, h, f, seed) = self.pics.pop_front().ok_or(Error::Eof)?;
        let pts = self.next;
        self.next += 1;
        let mut pkt = Packet::new(0, TimeBase::new(1, 10), vec![w, h, tag(f), seed]).with_pts(pts);
        pkt.duration = Some(1);
        Ok(pkt)
    }
}

fn open_demuxer(mut input: Box<dyn ReadSeek>, _: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut b = Vec::new();
    input.read_to_end(&mut b)?;
    let pics: VecDeque<Pic> = b[1..].chunks(4).map(pic_of).collect();
    // The container states the first picture's shape (what a config
    // record would) and hands the decoder its mode in `extradata`.
    let mut params = params_of(&pics[0]);
    params.extradata = vec![b[0]];
    Ok(Box::new(Demux {
        streams: vec![StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 10),
            duration: None,
            start_time: Some(0),
            params,
        }],
        pics,
        next: 0,
    }))
}

struct Shift {
    mode: Mode,
    input: VecDeque<(Pic, Option<i64>)>,
    /// The shape `output_params` reports.
    current: Pic,
    generation: u64,
    eof: bool,
}

impl Decoder for Shift {
    fn codec_id(&self) -> &CodecId {
        static ID: std::sync::OnceLock<CodecId> = std::sync::OnceLock::new();
        ID.get_or_init(|| CodecId::new(CODEC))
    }
    fn send_packet(&mut self, p: &Packet) -> Result<()> {
        self.input.push_back((pic_of(&p.data), p.pts));
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        if self.mode == Mode::EventAtFlush && !self.eof {
            return Err(Error::NeedMore);
        }
        let Some(&(next, pts)) = self.input.front() else {
            return Err(if self.eof {
                Error::Eof
            } else {
                Error::NeedMore
            });
        };
        let shape = |p: &Pic| (p.0, p.1, p.2);
        if shape(&next) != shape(&self.current) {
            self.current = next;
            if !matches!(self.mode, Mode::Silent | Mode::EventOnly) {
                self.generation += 1;
            }
            if matches!(
                self.mode,
                Mode::Event | Mode::EventAtFlush | Mode::EventOnly
            ) {
                // Moved before the event is returned; the payload is
                // what `output_params` now answers.
                return Ok(Frame::StreamParams(Box::new(
                    self.output_params().expect("known"),
                )));
            }
        }
        self.input.pop_front();
        self.current = next;
        Ok(Frame::Video(VideoFrame {
            pts,
            planes: planes(&next),
        }))
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
    fn output_params(&self) -> Option<CodecParameters> {
        Some(params_of(&self.current))
    }
    fn output_params_generation(&self) -> u64 {
        self.generation
    }
    fn output_pixel_format(&self) -> Option<PixelFormat> {
        Some(self.current.2)
    }
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let current = (
        params.width.unwrap_or(0) as u8,
        params.height.unwrap_or(0) as u8,
        params.pixel_format.unwrap_or(PixelFormat::Rgba),
        0,
    );
    Ok(Box::new(Shift {
        mode: Mode::of(params.extradata[0]),
        input: VecDeque::new(),
        current,
        generation: 0,
        eof: false,
    }))
}

/// An encoder that takes exactly the shape it was opened with: it
/// refuses `Frame::StreamParams` (it cannot re-configure) and any frame
/// whose planes are not its shape's, and writes one packet holding the
/// number of frames it accepted.
struct Strict {
    id: CodecId,
    params: CodecParameters,
    frames: u8,
    done: bool,
}

impl Encoder for Strict {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }
    fn output_params(&self) -> &CodecParameters {
        &self.params
    }
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::StreamParams(_) => {
                return Err(Error::unsupported("strict encoder: no mid-stream change"));
            }
            _ => return Err(Error::invalid("strict encoder: video only")),
        };
        let (w, h, f) = (
            self.params.width.unwrap(),
            self.params.height.unwrap(),
            self.params.pixel_format.unwrap(),
        );
        let planes = vf.image_planes();
        if planes.len() != f.plane_count() {
            return Err(Error::invalid("strict encoder: plane count"));
        }
        for (p, plane) in planes.iter().enumerate() {
            let row = f.plane_row_bytes(p, w).unwrap();
            let (_, rows) = f.plane_dimensions(p, w, h).unwrap();
            if plane.stride < row || plane.data.len() < row * rows as usize {
                return Err(Error::invalid("strict encoder: frame of another shape"));
            }
        }
        self.frames += 1;
        Ok(())
    }
    fn receive_packet(&mut self) -> Result<Packet> {
        if self.done {
            self.done = false;
            return Ok(Packet::new(0, TimeBase::MILLIS, vec![self.frames]));
        }
        Err(Error::NeedMore)
    }
    fn flush(&mut self) -> Result<()> {
        self.done = true;
        Ok(())
    }
}

fn make_strict(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(Strict {
        id: CodecId::new(STRICT),
        params: params.clone(),
        frames: 0,
        done: false,
    }))
}

/// The fixture formats plus the shifting decoder and the strict
/// encoder (codec-only: one packet, no muxer).
fn ctx() -> RuntimeContext {
    let mut ctx = crate::fixture::ctx();
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC))
            .capabilities(CodecCapabilities::video("oxim_shift_sw").with_intra_only(true))
            .decoder(make_decoder),
    );
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(STRICT))
            .capabilities(
                CodecCapabilities::video("oxim_strict_sw")
                    .with_intra_only(true)
                    .with_pixel_formats(vec![PixelFormat::Rgba, PixelFormat::Gray8]),
            )
            .encoder(make_strict),
    );
    ctx.containers.register_demuxer(CODEC, open_demuxer);
    ctx.containers.register_extension(CODEC, CODEC);
    ctx
}

fn opts() -> OpenOptions {
    OpenOptions::new().with_ext_hint(CODEC)
}

/// Size and layout change twice, then the first shape comes back.
fn script() -> Vec<Pic> {
    vec![
        (4, 4, PixelFormat::Rgba, 10),
        (4, 4, PixelFormat::Rgba, 20),
        (6, 3, PixelFormat::Gray8, 30),
        (6, 3, PixelFormat::Gray8, 40),
        (6, 3, PixelFormat::Rgb24, 50),
        (4, 4, PixelFormat::Rgba, 60),
    ]
}

/// Every picture has its own true shape, its own pixels, its own index
/// and the timing of its own packet.
fn assert_true_shapes(pictures: &[Image], pics: &[Pic]) {
    assert_eq!(pictures.len(), pics.len(), "the event is not a picture");
    for (i, (img, pic)) in pictures.iter().zip(pics).enumerate() {
        let (w, h, f, _) = *pic;
        assert_eq!(
            (img.width(), img.height(), img.format()),
            (u32::from(w), u32::from(h), f),
            "picture {i}"
        );
        assert_eq!(img.params().width, Some(u32::from(w)), "picture {i}");
        assert_eq!(img.index(), i);
        assert_eq!(img.raw_timing().0, Some(i as i64), "picture {i} timing");
        let want = planes(pic);
        for (p, plane) in img.frame().image_planes().iter().enumerate() {
            assert_eq!(plane.data, want[p].data, "picture {i} plane {p}");
        }
    }
}

#[test]
fn the_event_relabels_the_pictures_after_it_and_is_not_a_picture() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::Event, &script()), &opts()).unwrap();
    assert_true_shapes(f.frames(), &script());
    // One parameter set per shape run: shared within a run, a new one
    // at each change.
    let p = f.frames();
    assert!(std::ptr::eq(p[0].params(), p[1].params()));
    assert!(!std::ptr::eq(p[1].params(), p[2].params()));
    assert!(std::ptr::eq(p[2].params(), p[3].params()));
    assert!(!std::ptr::eq(p[3].params(), p[4].params()));
    // Delays come from the pictures' own packets (0.1 s each).
    assert!(
        p.iter()
            .all(|i| i.delay() == Some(std::time::Duration::from_millis(100)))
    );
}

#[test]
fn the_event_alone_relabels_without_a_generation_bump() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::EventOnly, &script()), &opts()).unwrap();
    assert_true_shapes(f.frames(), &script());
}

#[test]
fn a_generation_bump_alone_relabels_the_same_way() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::Generation, &script()), &opts()).unwrap();
    assert_true_shapes(f.frames(), &script());
}

#[test]
fn events_while_draining_a_flushed_decoder_are_honoured() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::EventAtFlush, &script()), &opts()).unwrap();
    assert_true_shapes(f.frames(), &script());
}

#[test]
fn the_frame_limit_counts_pictures_not_events() {
    let ctx = ctx();
    for mode in [
        Mode::Event,
        Mode::Generation,
        Mode::EventAtFlush,
        Mode::EventOnly,
    ] {
        let f =
            decode_bytes_with(&ctx, &file(mode, &script()), &opts().with_max_frames(3)).unwrap();
        assert_true_shapes(f.frames(), &script()[..3]);
    }
}

#[test]
fn the_pixel_budget_charges_each_picture_its_own_size() {
    let ctx = ctx();
    let total: u64 = script()
        .iter()
        .map(|&(w, h, ..)| u64::from(w) * u64::from(h))
        .sum();
    for mode in [Mode::Event, Mode::Generation, Mode::EventOnly] {
        let data = file(mode, &script());
        decode_bytes_with(&ctx, &data, &opts().with_max_pixels(total)).unwrap();
        match decode_bytes_with(&ctx, &data, &opts().with_max_pixels(total - 1)) {
            Err(ImageError::LimitExceeded(msg)) => assert!(msg.contains("picture 5"), "{msg}"),
            other => panic!("{mode:?}: {other:?}"),
        }
    }
}

#[test]
fn a_change_without_event_or_generation_is_reported_not_mislabelled() {
    // 8×8 then 2×2: under the old label the small picture's planes are
    // too short — a clear error naming the picture, not a picture
    // labelled with a shape it does not have.
    let pics = [
        (8, 8, PixelFormat::Yuv420P, 1),
        (2, 2, PixelFormat::Yuv420P, 2),
    ];
    match decode_bytes_with(&ctx(), &file(Mode::Silent, &pics), &opts()) {
        Err(ImageError::InvalidData(msg)) => {
            assert!(msg.contains("picture 1"), "{msg}");
            assert!(msg.contains("does not match the shape"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    // The same change announced either way opens.
    for mode in [Mode::Event, Mode::Generation] {
        let f = decode_bytes_with(&ctx(), &file(mode, &pics), &opts()).unwrap();
        assert_true_shapes(f.frames(), &pics);
    }
}

#[test]
fn an_explicit_layout_converts_every_picture_at_its_own_size() {
    let ctx = ctx();
    let f = decode_bytes_with(
        &ctx,
        &file(Mode::Event, &script()),
        &opts().with_pixel_format(PixelFormat::Rgba),
    )
    .unwrap();
    for (img, &(w, h, ..)) in f.iter().zip(&script()) {
        assert_eq!(
            (img.width(), img.height(), img.format()),
            (u32::from(w), u32::from(h), PixelFormat::Rgba)
        );
    }
}

#[test]
fn layer_operations_re_plan_at_the_change_of_canvas() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::Event, &script()), &opts()).unwrap();
    let frames = f.frames();
    let sizes = |list: &[Image]| -> Vec<(u32, u32)> {
        list.iter().map(|i| (i.width(), i.height())).collect()
    };
    let want: Vec<(u32, u32)> = sizes(frames);

    // Coalesce: each picture on a canvas of its own size, pixel for
    // pixel (decoded pictures replace the canvas) — not clipped to the
    // first picture's 4×4.
    let c = layers::coalesce(frames).unwrap();
    assert_eq!(sizes(&c), want);
    for (a, b) in frames.iter().zip(&c) {
        assert_eq!(a.to_rgba8().unwrap(), b.to_rgba8().unwrap());
        assert!(b.is_full_canvas());
    }
    let d = layers::apply_disposal(frames).unwrap();
    assert_eq!(sizes(&d), want);

    // Compare: the first picture of each new canvas is kept whole.
    let cmp = layers::compare_any(frames).unwrap();
    let kept: Vec<(u32, u32, (u32, u32))> = cmp
        .iter()
        .map(|i| (i.width(), i.height(), i.canvas_size()))
        .collect();
    assert!(kept.contains(&(6, 3, (6, 3))), "{kept:?}");
    assert!(cmp.iter().all(|i| {
        let (cw, ch) = i.canvas_size();
        i.width() <= cw && i.height() <= ch
    }));

    // Optimise: runs per canvas; replaying the result per run shows the
    // same pictures.
    let o = layers::optimize(frames).unwrap();
    let replay = layers::coalesce(&o).unwrap();
    assert_eq!(sizes(&replay), want);
    for (a, b) in frames.iter().zip(&replay) {
        assert_eq!(a.to_rgba8().unwrap(), b.to_rgba8().unwrap());
    }
    let t = layers::optimize_transparency(frames).unwrap();
    assert_eq!(sizes(&t), want);
}

#[test]
fn a_shape_changing_sequence_saves_one_stream_per_geometry() {
    let ctx = ctx();
    let f = decode_bytes_with(&ctx, &file(Mode::Event, &script()), &opts()).unwrap();
    let frames = f.frames();

    // A multi-geometry container: two streams (4×4, 6×3), each in one
    // layout; reading the file back gives every picture its own size
    // and the same pixels.
    let multi = SaveOptions::new().with_codec(crate::fixture::CODEC);
    let bytes = encode_frames(&ctx, frames, crate::fixture::CONTAINER_MULTI, &multi).unwrap();
    let back =
        decode_bytes_with(&ctx, &bytes, &OpenOptions::new().with_ext_hint("oximmulti")).unwrap();
    assert_eq!(back.len(), frames.len());
    let by_stream = |s: usize| -> Vec<&Image> { back.iter().filter(|i| i.stream() == s).collect() };
    for (s, members) in [(0, vec![0, 1, 5]), (1, vec![2, 3, 4])] {
        let got = by_stream(s);
        assert_eq!(got.len(), members.len());
        for (g, &m) in got.iter().zip(&members) {
            let src = &frames[m];
            assert_eq!((g.width(), g.height()), (src.width(), src.height()));
            assert_eq!(
                g.to_rgba8().unwrap(),
                src.to_rgba8().unwrap(),
                "picture {m}"
            );
        }
        // One layout per stream.
        assert!(got.iter().all(|g| g.format() == got[0].format()));
    }

    // A one-geometry container refuses by type, naming the geometries.
    match encode_frames(&ctx, frames, "oxim", &SaveOptions::default()) {
        Err(ImageError::Unsupported(msg)) => {
            assert!(msg.contains("4x4") && msg.contains("6x3"), "{msg}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_encoder_never_sees_its_input_shape_change() {
    let ctx = ctx();
    // One geometry, the layout changing (Rgba → Gray8 → Rgb24): every
    // picture reaches the strict encoder in the one layout it was
    // opened with, and no event is sent to it.
    let pics = [
        (4, 4, PixelFormat::Gray8, 1),
        (4, 4, PixelFormat::Rgba, 2),
        (4, 4, PixelFormat::Rgb24, 3),
    ];
    let f = decode_bytes_with(&ctx, &file(Mode::Event, &pics), &opts()).unwrap();
    assert_true_shapes(f.frames(), &pics);
    let bytes = encode_frames(&ctx, f.frames(), STRICT, &SaveOptions::default()).unwrap();
    assert_eq!(bytes, vec![3], "three frames accepted");

    // Codec-only: one geometry per file, refused by type otherwise.
    let f = decode_bytes_with(&ctx, &file(Mode::Event, &script()), &opts()).unwrap();
    match encode_frames(&ctx, f.frames(), STRICT, &SaveOptions::default()) {
        Err(ImageError::Unsupported(msg)) => assert!(msg.contains("geometr"), "{msg}"),
        other => panic!("{other:?}"),
    }
}
