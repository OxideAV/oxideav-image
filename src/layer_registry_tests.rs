//! Layer sequences through the registry, against a mock animated
//! format registered the way a format crate registers ("OXLY": a
//! container and two codecs). The mock stores what the animated image
//! formats store — a canvas, a loop count, a background colour, and per
//! frame a rectangle, a delay, a disposal and a blending — and speaks
//! to the gateway only through the framework: stream parameters,
//! `Demuxer::metadata` / `Muxer::set_metadata`, packet durations and
//! the per-frame `image-layer` record.
//!
//! File layout (little-endian): `"OXLY" | canvas width u32 | height u32
//! | loop count u32 (`u32::MAX` = none) | background present u8 |
//! background RGBA 4 × u16 | frame count u32`, then per frame `delay ms
//! u32 | payload length u32 | payload`; a payload is the 28-byte layer
//! record followed by the picture's `Rgba` bytes.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecResolver, Decoder, Demuxer,
    Encoder, Error as CoreError, Frame, MAX_PROBE_SCORE, MediaType, Muxer, Packet, PixelFormat,
    ProbeData, ProbeScore, ReadSeek, Result as CoreResult, RuntimeContext, StreamInfo, TimeBase,
    VideoFrame, VideoPlane, WriteSeek,
};

use crate::layers::{self, LayerSequence, OptimizeOptions};
use crate::{
    BACKGROUND_COLOR_KEY, Blend, Dispose, Image, LayerRecord, Page, SaveOptions, decode_vec,
    encode_detailed, encode_frames, encodes_layers, format_background, parse_background,
};

const MAGIC: &[u8; 4] = b"OXLY";
const CONTAINER: &str = "oxly";
/// Writes layers: declares the record among its embedded blob kinds.
const CODEC: &str = "oxly";
/// The same coder without the declaration: it is handed full canvases.
const CODEC_FLAT: &str = "oxly_flat";
const MILLIS: TimeBase = TimeBase::new(1, 1000);

fn bad(what: &str) -> CoreError {
    CoreError::invalid(format!("oxly: {what}"))
}

struct OxlyDecoder {
    id: CodecId,
    ready: Vec<VideoFrame>,
}

impl Decoder for OxlyDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        let (record, pixels) = packet
            .data
            .split_at_checked(LayerRecord::WIRE_LEN)
            .ok_or_else(|| bad("short payload"))?;
        let layer = LayerRecord::from_bytes(record).ok_or_else(|| bad("layer record"))?;
        if pixels.len() != layer.width as usize * layer.height as usize * 4 {
            return Err(bad("pixel count"));
        }
        let frame = VideoFrame {
            pts: packet.pts,
            planes: vec![VideoPlane {
                stride: layer.width as usize * 4,
                data: pixels.to_vec(),
            }],
        };
        // A whole-canvas replacement is a plain frame; anything else is
        // a layer and says so.
        self.ready.push(frame.with_blob(layer.to_blob()));
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        if self.ready.is_empty() {
            return Err(CoreError::NeedMore);
        }
        Ok(Frame::Video(self.ready.remove(0)))
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.ready.clear();
        Ok(())
    }
}

fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    Ok(Box::new(OxlyDecoder {
        id: params.codec_id.clone(),
        ready: Vec::new(),
    }))
}

struct OxlyEncoder {
    out: CodecParameters,
    pending: Vec<Packet>,
    done: bool,
}

impl Encoder for OxlyEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.out.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out
    }

    fn send_frame(&mut self, frame: &Frame) -> CoreResult<()> {
        let Frame::Video(vf) = frame else {
            return Err(bad("video only"));
        };
        let (cw, ch) = (self.out.width.unwrap_or(0), self.out.height.unwrap_or(0));
        // Without a record the frame is the canvas itself.
        let layer = match vf.blob(&LayerRecord::kind()) {
            Some(blob) => LayerRecord::from_bytes(&blob.data).ok_or_else(|| bad("layer record"))?,
            None => LayerRecord::new(Page::new(cw, ch), cw, ch).with_blend(Blend::Source),
        };
        // What a real format's writer checks: the rectangle lies on
        // the canvas.
        let (x, y) = (i64::from(layer.page.x), i64::from(layer.page.y));
        if x < 0
            || y < 0
            || x + i64::from(layer.width) > i64::from(cw)
            || y + i64::from(layer.height) > i64::from(ch)
        {
            return Err(bad("frame outside the canvas"));
        }
        let plane = vf.image_planes().first().ok_or_else(|| bad("no plane"))?;
        let row = layer.width as usize * 4;
        let mut data = layer.to_bytes().to_vec();
        for r in 0..layer.height as usize {
            let at = r * plane.stride;
            data.extend_from_slice(
                plane
                    .data
                    .get(at..at + row)
                    .ok_or_else(|| bad("short plane"))?,
            );
        }
        let mut pkt = Packet::new(0, MILLIS, data);
        pkt.pts = vf.pts;
        pkt.flags.keyframe = true;
        self.pending.push(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> CoreResult<Packet> {
        if !self.pending.is_empty() {
            return Ok(self.pending.remove(0));
        }
        Err(if self.done {
            CoreError::Eof
        } else {
            CoreError::NeedMore
        })
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.done = true;
        Ok(())
    }
}

fn make_encoder(params: &CodecParameters) -> CoreResult<Box<dyn Encoder>> {
    if params.pixel_format != Some(PixelFormat::Rgba) {
        return Err(CoreError::unsupported("oxly encoder: Rgba only"));
    }
    let mut out = params.clone();
    out.media_type = MediaType::Video;
    Ok(Box::new(OxlyEncoder {
        out,
        pending: Vec::new(),
        done: false,
    }))
}

struct OxlyDemuxer {
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    frames: std::collections::VecDeque<(u32, Vec<u8>)>,
    pts: i64,
}

fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> CoreResult<Box<dyn Demuxer>> {
    input.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    input.read_to_end(&mut buf)?;
    let word = |at: usize| -> CoreResult<u32> {
        let b = buf.get(at..at + 4).ok_or_else(|| bad("truncated"))?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if buf.get(..4) != Some(MAGIC.as_slice()) {
        return Err(bad("magic"));
    }
    let (w, h, plays) = (word(4)?, word(8)?, word(12)?);
    let has_background = *buf.get(16).ok_or_else(|| bad("truncated"))? != 0;
    let colour = buf.get(17..25).ok_or_else(|| bad("truncated"))?;
    let count = word(25)?;
    let mut metadata = Vec::new();
    if plays != u32::MAX {
        metadata.push(("loop_count".to_string(), plays.to_string()));
    }
    if has_background {
        let c: Vec<u16> = colour
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        metadata.push((
            BACKGROUND_COLOR_KEY.to_string(),
            format_background([c[0], c[1], c[2], c[3]]),
        ));
    }
    let mut frames = std::collections::VecDeque::new();
    let mut at = 29;
    let mut total = 0i64;
    for _ in 0..count {
        let (delay, len) = (word(at)?, word(at + 4)? as usize);
        let payload = buf
            .get(at + 8..at + 8 + len)
            .ok_or_else(|| bad("truncated"))?;
        frames.push_back((delay, payload.to_vec()));
        total += i64::from(delay);
        at += 8 + len;
    }
    let mut params = CodecParameters::video(CodecId::new(CODEC));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(PixelFormat::Rgba);
    Ok(Box::new(OxlyDemuxer {
        streams: vec![StreamInfo {
            index: 0,
            time_base: MILLIS,
            duration: Some(total),
            start_time: Some(0),
            params,
        }],
        metadata,
        frames,
        pts: 0,
    }))
}

impl Demuxer for OxlyDemuxer {
    fn format_name(&self) -> &str {
        CONTAINER
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        let (delay, payload) = self.frames.pop_front().ok_or(CoreError::Eof)?;
        let mut pkt = Packet::new(0, MILLIS, payload);
        pkt.pts = Some(self.pts);
        pkt.dts = Some(self.pts);
        pkt.duration = Some(i64::from(delay));
        pkt.flags.keyframe = true;
        self.pts += i64::from(delay);
        Ok(pkt)
    }
}

struct OxlyMuxer {
    output: Box<dyn WriteSeek>,
    stream: StreamInfo,
    background: Option<[u16; 4]>,
    packets: Vec<Packet>,
}

fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> CoreResult<Box<dyn Muxer>> {
    let [stream] = streams else {
        return Err(bad("exactly one stream"));
    };
    Ok(Box::new(OxlyMuxer {
        output,
        stream: stream.clone(),
        background: None,
        packets: Vec::new(),
    }))
}

impl Muxer for OxlyMuxer {
    fn format_name(&self) -> &str {
        CONTAINER
    }

    fn write_header(&mut self) -> CoreResult<()> {
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        self.packets.push(packet.clone());
        Ok(())
    }

    fn set_metadata(&mut self, metadata: &[(String, String)]) -> CoreResult<()> {
        self.background = None;
        for (key, value) in metadata {
            if key == BACKGROUND_COLOR_KEY {
                self.background = Some(parse_background(value).ok_or_else(|| bad("background"))?);
            }
        }
        Ok(())
    }

    fn carries_metadata_key(&self, key: &str) -> bool {
        key == BACKGROUND_COLOR_KEY
    }

    fn write_trailer(&mut self) -> CoreResult<()> {
        let p = &self.stream.params;
        let plays = match p.options.get("loop_count") {
            Some(v) => v.parse::<u32>().map_err(|_| bad("loop count"))?,
            None => u32::MAX,
        };
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&p.width.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&p.height.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&plays.to_le_bytes());
        out.push(u8::from(self.background.is_some()));
        for c in self.background.unwrap_or_default() {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(&(self.packets.len() as u32).to_le_bytes());
        for pkt in &self.packets {
            let delay = pkt.duration.unwrap_or(0) as u32;
            out.extend_from_slice(&delay.to_le_bytes());
            out.extend_from_slice(&(pkt.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&pkt.data);
        }
        self.output.write_all(&out)?;
        Ok(())
    }
}

fn probe(data: &ProbeData) -> ProbeScore {
    if data.buf.len() >= 4 && &data.buf[..4] == MAGIC {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    let caps = |name: &str| {
        CodecCapabilities::video(name)
            .with_intra_only(true)
            .with_lossless(true)
            .with_pixel_formats(vec![PixelFormat::Rgba])
    };
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC))
            .capabilities(caps("oxly_sw").with_embedded_blob_kind(LayerRecord::kind()))
            .decoder(make_decoder)
            .encoder(make_encoder),
    );
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC_FLAT))
            .capabilities(caps("oxly_flat_sw"))
            .decoder(make_decoder)
            .encoder(make_encoder),
    );
    ctx.containers.register_demuxer(CONTAINER, open_demuxer);
    ctx.containers.register_muxer(CONTAINER, open_muxer);
    ctx.containers.register_extension("oxly", CONTAINER);
    ctx.containers.register_probe(CONTAINER, probe);
    ctx
}

fn solid(w: u32, h: u32, c: [u8; 4]) -> Image {
    Image::from_rgba8(w, h, c.repeat((w * h) as usize)).unwrap()
}

/// A 12×9 animation using every disposal and both blends, with a frame
/// that runs off the canvas and one wholly outside it.
fn animation() -> Vec<Image> {
    let on = |img: Image, x, y| img.with_page(Page::new(12, 9).with_offset(x, y));
    vec![
        on(solid(12, 9, [200, 200, 200, 255]), 0, 0)
            .with_blend(Blend::Source)
            .with_dispose(Dispose::None)
            .with_delay_ticks(40, 1000),
        on(solid(4, 3, [255, 0, 0, 255]), 2, 1)
            .with_dispose(Dispose::Background)
            .with_delay_ticks(70, 1000),
        on(solid(5, 5, [0, 0, 255, 96]), 9, 6)
            .with_dispose(Dispose::Previous)
            .with_delay_ticks(0, 1000),
        on(solid(3, 3, [0, 255, 0, 128]), -1, -2)
            .with_blend(Blend::Source)
            .with_delay_ticks(30, 1000),
        on(solid(2, 2, [1, 2, 3, 255]), 40, 40)
            .with_dispose(Dispose::Background)
            .with_delay_ticks(55, 1000),
        on(solid(6, 2, [255, 255, 0, 255]), 3, 7).with_delay_ticks(20, 1000),
    ]
}

fn pictures(frames: &[Image]) -> Vec<(Page, u32, u32, Vec<u8>)> {
    frames
        .iter()
        .map(|f| (f.page(), f.width(), f.height(), f.to_rgba8().unwrap()))
        .collect()
}

/// The frames show the same pictures: same geometry, and every pixel
/// identical or fully transparent in both.
fn assert_shows_same(a: &[Image], b: &[Image]) {
    assert_eq!(a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        assert_eq!(
            (x.page(), x.width(), x.height()),
            (y.page(), y.width(), y.height())
        );
        let (px, py) = (x.to_rgba8().unwrap(), y.to_rgba8().unwrap());
        let pairs = px.as_chunks::<4>().0.iter().zip(py.as_chunks::<4>().0);
        for (k, (p, q)) in pairs.enumerate() {
            assert!(
                p == q || (p[3] == 0 && q[3] == 0),
                "frame {i} pixel {k}: {p:?} {q:?}"
            );
        }
    }
}

fn delays(frames: &[Image]) -> Vec<Option<Duration>> {
    frames.iter().map(Image::delay).collect()
}

#[test]
fn the_layer_record_round_trips_and_refuses_bad_bytes() {
    let record = LayerRecord::new(Page::new(640, 480).with_offset(-3, 70_000), 17, 9)
        .with_dispose(Dispose::Previous)
        .with_blend(Blend::Source);
    let bytes = record.to_bytes();
    assert_eq!(bytes.len(), LayerRecord::WIRE_LEN);
    assert_eq!(bytes[..4], [1, 3, 1, 0]);
    assert_eq!(bytes[4..8], 640u32.to_le_bytes());
    assert_eq!(bytes[12..16], (-3i32).to_le_bytes());
    assert_eq!(bytes[20..24], 17u32.to_le_bytes());
    assert_eq!(LayerRecord::from_bytes(&bytes), Some(record));
    let blob = record.to_blob();
    assert_eq!(blob.kind, LayerRecord::kind());
    assert_eq!(blob.kind.as_str(), "image-layer");
    assert!(blob.kind.follows_naming_rule());
    for (at, value) in [(0, 2), (1, 4), (2, 2), (3, 1)] {
        let mut b = bytes;
        b[at] = value;
        assert_eq!(LayerRecord::from_bytes(&b), None, "byte {at}");
    }
    let mut zero = bytes;
    zero[20..24].fill(0);
    assert_eq!(LayerRecord::from_bytes(&zero), None);
    assert_eq!(LayerRecord::from_bytes(&bytes[..27]), None);
    for dispose in [
        Dispose::Unspecified,
        Dispose::None,
        Dispose::Background,
        Dispose::Previous,
    ] {
        for blend in [Blend::Over, Blend::Source] {
            let r = record.with_dispose(dispose).with_blend(blend);
            assert_eq!(LayerRecord::from_bytes(&r.to_bytes()), Some(r));
        }
    }
    let img = solid(3, 2, [0; 4])
        .with_offset(5, 6)
        .with_dispose(Dispose::Background);
    let of = LayerRecord::of(&img);
    assert_eq!(
        (of.page, of.width, of.height),
        (Page::new(3, 2).with_offset(5, 6), 3, 2)
    );
    assert_eq!((of.dispose, of.blend), (Dispose::Background, Blend::Over));
}

#[test]
fn a_layered_list_is_written_as_layers_and_read_back_as_layers() {
    let ctx = ctx();
    let frames = animation();
    assert!(encodes_layers(&ctx, "oxly", &SaveOptions::new()));
    let done = encode_detailed(
        &ctx,
        &frames,
        "oxly",
        &SaveOptions::new().with_loop_count(3),
    )
    .unwrap();
    assert!(done.layered());
    let file = decode_vec(&ctx, done.into_bytes()).unwrap();
    assert_eq!((file.len(), file.loop_count()), (frames.len(), Some(3)));
    let back = file.frames();
    // Geometry: the frames inside the canvas come back as they went;
    // the one running off it is cut to the canvas and the one outside
    // is a single transparent pixel.
    let want = layers::clip_to_canvas(&frames).unwrap();
    assert_eq!(pictures(back), pictures(&want));
    assert_eq!(back[3].page(), Page::new(12, 9));
    assert_eq!((back[3].width(), back[3].height()), (2, 1));
    assert_eq!(
        (back[4].width(), back[4].to_rgba8().unwrap()),
        (1, vec![0, 0, 0, 0])
    );
    for (b, w) in back.iter().zip(&want) {
        assert_eq!((b.dispose(), b.blend()), (w.dispose(), w.blend()));
        // The record became the picture's layer fields; it is not left
        // behind as a blob.
        assert!(b.frame_blobs().is_empty());
    }
    assert_eq!(back[1].dispose(), Dispose::Background);
    assert_eq!(back[2].dispose(), Dispose::Previous);
    assert_eq!(back[3].blend(), Blend::Source);
    assert_eq!(delays(back), delays(&frames));
    // And it plays the same.
    assert_eq!(
        pictures(&layers::coalesce(back).unwrap()),
        pictures(&layers::coalesce(&frames).unwrap())
    );
    // Written again, the file is byte for byte the same.
    let again = encode_frames(&ctx, back, "oxly", &SaveOptions::new()).unwrap();
    let first = encode_frames(
        &ctx,
        &frames,
        "oxly",
        &SaveOptions::new().with_loop_count(3),
    )
    .unwrap();
    assert_eq!(again, first);
}

#[test]
fn an_encoder_without_the_record_gets_full_canvases() {
    let ctx = ctx();
    let frames = animation();
    let flat = SaveOptions::new().with_codec(CODEC_FLAT);
    assert!(!encodes_layers(&ctx, "oxly", &flat));
    assert!(!encodes_layers(
        &ctx,
        "nothing-registered",
        &SaveOptions::new()
    ));
    let done = encode_detailed(&ctx, &frames, "oxly", &flat).unwrap();
    assert!(!done.layered());
    let file = decode_vec(&ctx, done.into_bytes()).unwrap();
    assert!(file.frames().iter().all(Image::is_full_canvas));
    assert_eq!(
        pictures(file.frames()),
        pictures(&layers::coalesce(&frames).unwrap())
    );
    // A list on two canvases cannot be one layered stream either.
    let mut two = frames.clone();
    two.push(solid(5, 5, [9, 9, 9, 255]).with_delay_ticks(10, 1000));
    assert!(encode_detailed(&ctx, &two, "oxly", &SaveOptions::new()).is_err());
    // A list that is not layered is written as it is.
    let plain = vec![solid(4, 4, [1, 1, 1, 255]), solid(4, 4, [2, 2, 2, 255])];
    assert!(
        !encode_detailed(&ctx, &plain, "oxly", &SaveOptions::new())
            .unwrap()
            .layered()
    );
}

#[test]
fn coalesce_optimise_write_keeps_the_rectangles_small() {
    let ctx = ctx();
    let seq = LayerSequence::new(animation())
        .unwrap()
        .with_loop_count(Some(0))
        .with_background_rgba8([0, 0, 64, 0]);
    let shown = seq.coalesce().unwrap();
    let small = shown
        .optimize(&OptimizeOptions::new().with_transparency(true))
        .unwrap();
    let bytes = small.encode(&ctx, "oxly", &SaveOptions::new()).unwrap();
    let file = decode_vec(&ctx, bytes.clone()).unwrap();
    let back = LayerSequence::from_file(&file).unwrap();
    // The sequence fields survive: canvas, loop count, background.
    assert_eq!(back.canvas_size(), (12, 9));
    assert_eq!(back.loop_count(), Some(0));
    assert_eq!(back.background(), [0, 0, 64 * 257, 0]);
    assert_eq!(
        file.metadata()
            .iter()
            .filter(|(k, _)| k == BACKGROUND_COLOR_KEY)
            .count(),
        1
    );
    // The frames on disk are the optimiser's rectangles.
    assert_eq!(pictures(back.frames()), pictures(small.frames()));
    let area = |frames: &[Image]| -> u32 { frames.iter().map(|f| f.width() * f.height()).sum() };
    assert!(area(back.frames()) * 2 < area(shown.frames()));
    assert_eq!(
        bytes.len(),
        29 + back.len() * (8 + 28) + area(back.frames()) as usize * 4
    );
    // And the file plays as the original animation.
    assert_shows_same(back.coalesce().unwrap().frames(), shown.frames());
    // A coalesced list replays to itself: its frames replace the canvas.
    assert!(shown.frames().iter().all(|f| f.blend() == Blend::Source));
    assert_eq!(
        pictures(shown.coalesce().unwrap().frames()),
        pictures(shown.frames())
    );
    assert_eq!(delays(back.frames()), delays(seq.frames()));
    assert_eq!(back.total_delay(), seq.total_delay());
    // A caller-set background entry wins over the sequence's.
    let opts = SaveOptions::new().with_container_metadata_entry(BACKGROUND_COLOR_KEY, "#ffffffff");
    let file = decode_vec(&ctx, small.encode(&ctx, "oxly", &opts).unwrap()).unwrap();
    assert_eq!(
        LayerSequence::from_file(&file).unwrap().background(),
        [0xffff; 4]
    );
    // No loop count, transparent-black background: neither is written.
    let bare = LayerSequence::new(animation()).unwrap();
    let file = decode_vec(
        &ctx,
        bare.encode(&ctx, "oxly", &SaveOptions::new()).unwrap(),
    )
    .unwrap();
    assert_eq!((file.loop_count(), file.metadata().len()), (None, 0));
}

#[test]
fn a_malformed_record_and_the_pixel_budget() {
    let ctx = ctx();
    let frames = animation();
    let bytes = encode_frames(&ctx, &frames, "oxly", &SaveOptions::new()).unwrap();
    // Layers are budgeted at their own size: 108 + 12 + 9 + 2 + 1 + 12
    // pixels, not six canvases.
    let own: u64 = layers::clip_to_canvas(&frames)
        .unwrap()
        .iter()
        .map(|f| u64::from(f.width() * f.height()))
        .sum();
    assert_eq!(own, 144);
    let exact = crate::OpenOptions::new().with_max_pixels(own);
    assert_eq!(
        crate::decode_vec_with(&ctx, bytes.clone(), &exact)
            .unwrap()
            .len(),
        6
    );
    let tight = crate::OpenOptions::new().with_max_pixels(own - 1);
    assert!(matches!(
        crate::decode_vec_with(&ctx, bytes.clone(), &tight),
        Err(crate::Error::LimitExceeded(_))
    ));
    // A decoder handing out a record the gateway cannot read is an
    // error, not a guess.
    struct Broken;
    impl Decoder for Broken {
        fn codec_id(&self) -> &CodecId {
            static ID: std::sync::OnceLock<CodecId> = std::sync::OnceLock::new();
            ID.get_or_init(|| CodecId::new(CODEC))
        }
        fn send_packet(&mut self, _: &Packet) -> CoreResult<()> {
            Ok(())
        }
        fn receive_frame(&mut self) -> CoreResult<Frame> {
            let frame = VideoFrame {
                pts: Some(0),
                planes: vec![VideoPlane {
                    stride: 48,
                    data: vec![0; 48 * 9],
                }],
            };
            let blob = oxideav_core::MetadataBlob::new(LayerRecord::kind(), vec![7u8; 5]);
            Ok(Frame::Video(frame.with_blob(blob)))
        }
        fn flush(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }
    let mut ctx = RuntimeContext::new();
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC))
            .capabilities(CodecCapabilities::video("oxly_broken"))
            .decoder(|_| Ok(Box::new(Broken))),
    );
    ctx.containers.register_demuxer(CONTAINER, open_demuxer);
    ctx.containers.register_probe(CONTAINER, probe);
    match decode_vec(&ctx, bytes) {
        Err(crate::Error::InvalidData(msg)) => assert!(msg.contains("image-layer"), "{msg}"),
        other => panic!("{:?}", other.map(|f| f.len())),
    }
}
