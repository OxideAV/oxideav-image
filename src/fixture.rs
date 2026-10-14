//! A synthetic container + codec pair ("OXIM") registered the way a real
//! sibling crate registers, so the gateway's registry paths can be
//! tested without a format crate.
//!
//! File layout (little-endian):
//!
//! ```text
//! magic "OXIM" | width u32 | height u32 | format tag u8 | flags u8
//! | frame count u32 | metadata length u16 | metadata ("k=v\n" …)
//! then per frame: delay_ms u32 | payload length u32 | payload
//! ```
//!
//! `flags` bit 0: the demuxer stamps `Packet::duration`; bit 1: the
//! demuxer leaves packet `pts` unset. The codec payload is
//! `palette length u16 | palette RGB bytes | planes, tight rows, in
//! plane order`, which the decoder splits with the stream geometry.
//!
//! Registered names: container `oxim` (extension `.oxim`, codec `oxim`
//! accepting every layout in [`FORMATS`]), container `oximrgb` (same
//! files, muxer accepts `Rgb24` streams only), codec `oxim_yuv`
//! (encoder accepts `Yuv420P` only) and container `oximnodec` whose
//! stream names a codec nobody decodes.

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_core::{
    parse_options, CodecCapabilities, CodecId, CodecInfo, CodecOptionsStruct, CodecParameters,
    CodecResolver, Decoder, Demuxer, Encoder, Error, Frame, MediaType, Muxer, OptionField,
    OptionKind, OptionValue, Packet, PixelFormat, ProbeData, ProbeScore, ReadSeek, Result,
    RuntimeContext, StreamInfo, TimeBase, VideoFrame, VideoPlane, WriteSeek, MAX_PROBE_SCORE,
    PROBE_SCORE_EXTENSION,
};

pub(crate) const CONTAINER: &str = "oxim";
pub(crate) const CONTAINER_RGB_ONLY: &str = "oximrgb";
pub(crate) const CONTAINER_NO_DECODER: &str = "oximnodec";
/// Several OXIM blocks back to back: one video stream per block, so the
/// muxer takes any number of geometries (the ICO / EXR shape).
pub(crate) const CONTAINER_MULTI: &str = "oximmulti";
pub(crate) const CODEC: &str = "oxim";
pub(crate) const CODEC_YUV_ONLY: &str = "oxim_yuv";
/// Declares no accepted layouts (like a video codec) but takes only
/// 4:2:0 / 4:4:4 YCbCr and 4:0:0 grey.
pub(crate) const CODEC_UNDECLARED: &str = "oxim_undeclared";
pub(crate) const CODEC_NO_DECODER: &str = "oxim_nodec";
const MAGIC: &[u8; 4] = b"OXIM";
pub(crate) const FLAG_DURATIONS: u8 = 1;
pub(crate) const FLAG_NO_PTS: u8 = 2;
pub(crate) const TIME_BASE: TimeBase = TimeBase::new(1, 1000);

/// Every layout the synthetic codec carries (tag = index).
pub(crate) const FORMATS: &[PixelFormat] = &[
    PixelFormat::Gray8,
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Yuv420P,
    PixelFormat::Pal8,
    PixelFormat::Rgba64Le,
    PixelFormat::Rgb48Le,
    PixelFormat::Bgra,
    PixelFormat::Gray16Le,
    PixelFormat::Ya8,
    PixelFormat::Yuv444P,
    PixelFormat::RgbaF32Le,
    PixelFormat::Yuva420P,
    PixelFormat::Gbrp8,
];

fn tag_of(f: PixelFormat) -> Option<u8> {
    FORMATS.iter().position(|x| *x == f).map(|i| i as u8)
}

fn format_of(tag: u8) -> Option<PixelFormat> {
    FORMATS.get(tag as usize).copied()
}

/// One picture of a fixture file.
#[derive(Clone, Debug)]
pub(crate) struct FixtureFrame {
    pub delay_ms: u32,
    /// Tight planes in plane order.
    pub planes: Vec<VideoPlane>,
    pub palette: Option<Vec<u8>>,
}

/// A fixture file before serialisation.
#[derive(Clone, Debug)]
pub(crate) struct Fixture {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub flags: u8,
    pub metadata: Vec<(String, String)>,
    pub frames: Vec<FixtureFrame>,
}

impl Fixture {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            width,
            height,
            format,
            flags: FLAG_DURATIONS,
            metadata: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Append a deterministic gradient frame (`seed` perturbs the bytes).
    pub fn push_gradient(&mut self, delay_ms: u32, seed: u8) -> &mut Self {
        let planes = gradient_planes(self.width, self.height, self.format, seed);
        let palette = (self.format == PixelFormat::Pal8).then(|| test_palette(seed));
        self.frames.push(FixtureFrame {
            delay_ms,
            planes,
            palette,
        });
        self
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.push(tag_of(self.format).expect("fixture format has a tag"));
        out.push(self.flags);
        out.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        let mut meta = Vec::new();
        for (k, v) in &self.metadata {
            meta.extend_from_slice(k.as_bytes());
            meta.push(b'=');
            meta.extend_from_slice(v.as_bytes());
            meta.push(b'\n');
        }
        out.extend_from_slice(&(meta.len() as u16).to_le_bytes());
        out.extend_from_slice(&meta);
        for f in &self.frames {
            let payload = encode_payload(&f.planes, f.palette.as_deref());
            out.extend_from_slice(&f.delay_ms.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&payload);
        }
        out
    }
}

/// Deterministic tight planes for `format` at `width × height`.
pub(crate) fn gradient_planes(
    width: u32,
    height: u32,
    format: PixelFormat,
    seed: u8,
) -> Vec<VideoPlane> {
    (0..format.plane_count())
        .map(|p| {
            let stride = format.plane_row_bytes(p, width).expect("row bytes");
            let (_, rows) = format.plane_dimensions(p, width, height).expect("dims");
            let data = (0..stride * rows as usize)
                .map(|i| {
                    (i as u32)
                        .wrapping_mul(7 + p as u32)
                        .wrapping_add(seed as u32 * 13)
                        .wrapping_add(p as u32 * 50) as u8
                })
                .collect();
            VideoPlane { stride, data }
        })
        .collect()
}

/// A 256-entry RGB palette (768 bytes).
pub(crate) fn test_palette(seed: u8) -> Vec<u8> {
    (0..256u32)
        .flat_map(|i| {
            [
                i as u8,
                (255 - i) as u8,
                (i as u8).wrapping_mul(3).wrapping_add(seed),
            ]
        })
        .collect()
}

fn encode_payload(planes: &[VideoPlane], palette: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    let pal = palette.unwrap_or(&[]);
    out.extend_from_slice(&(pal.len() as u16).to_le_bytes());
    out.extend_from_slice(pal);
    for p in planes {
        out.extend_from_slice(&p.data);
    }
    out
}

fn rd_u32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| Error::invalid("oxim: truncated"))
}

// ---- codec -----------------------------------------------------------------

struct OximDecoder {
    id: CodecId,
    params: CodecParameters,
    pending: Option<VideoFrame>,
    eof: bool,
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(OximDecoder {
        id: CodecId::new(CODEC),
        params: params.clone(),
        pending: None,
        eof: false,
    }))
}

impl Decoder for OximDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let (w, h, f) = match (
            self.params.width,
            self.params.height,
            self.params.pixel_format,
        ) {
            (Some(w), Some(h), Some(f)) => (w, h, f),
            _ => return Err(Error::invalid("oxim decoder: stream geometry missing")),
        };
        let d = &packet.data;
        let pal_len = d
            .get(0..2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
            .ok_or_else(|| Error::invalid("oxim: truncated payload"))?;
        let pal = d
            .get(2..2 + pal_len)
            .ok_or_else(|| Error::invalid("oxim: truncated palette"))?
            .to_vec();
        let mut at = 2 + pal_len;
        let mut planes = Vec::new();
        for p in 0..f.plane_count() {
            let stride = f
                .plane_row_bytes(p, w)
                .ok_or_else(|| Error::invalid("oxim: row"))?;
            let (_, rows) = f
                .plane_dimensions(p, w, h)
                .ok_or_else(|| Error::invalid("oxim: dims"))?;
            let len = stride * rows as usize;
            let data = d
                .get(at..at + len)
                .ok_or_else(|| Error::invalid("oxim: truncated plane"))?
                .to_vec();
            at += len;
            planes.push(VideoPlane { stride, data });
        }
        let mut frame = VideoFrame {
            pts: packet.pts,
            planes,
        };
        if !pal.is_empty() {
            frame.set_palette(pal);
        }
        if self.params.options.get("stamp_color").is_some() {
            frame.set_color_signal(self.params.color_signal);
        }
        self.pending = Some(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Video(f)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending = None;
        self.eof = false;
        Ok(())
    }
}

/// Declared encoder options of codec `oxim`, parsed strictly the way
/// real encoders do (`parse_options` rejects unknown names).
#[derive(Default)]
pub(crate) struct OximEncOpts {
    pub quality: u32,
}

impl CodecOptionsStruct for OximEncOpts {
    const SCHEMA: &'static [OptionField] = &[OptionField {
        name: "quality",
        kind: OptionKind::U32,
        default: OptionValue::U32(0),
        help: "advisory quality 0..=100 (recorded, never applied)",
    }];

    fn apply(&mut self, key: &str, value: &OptionValue) -> Result<()> {
        if key == "quality" {
            self.quality = value.as_u32()?;
        }
        Ok(())
    }
}

struct OximEncoder {
    id: CodecId,
    out_params: CodecParameters,
    pending: Vec<Packet>,
    eof: bool,
}

fn make_encoder_for(
    id: &str,
    accepted: &[PixelFormat],
    params: &CodecParameters,
) -> Result<Box<dyn Encoder>> {
    let f = params
        .pixel_format
        .ok_or_else(|| Error::invalid("oxim encoder: no pixel format"))?;
    if !accepted.contains(&f) {
        return Err(Error::unsupported(format!(
            "oxim encoder '{id}' does not accept {f:?}"
        )));
    }
    let mut out_params = params.clone();
    out_params.codec_id = CodecId::new(id);
    out_params.media_type = MediaType::Video;
    // Options: `oxim` declares a schema and parses strictly; `oxim_yuv`
    // declares none and refuses every option, like an encoder without
    // an options struct. What was understood is echoed on the output
    // parameters (the muxer writes them as file metadata).
    out_params.options = oxideav_core::CodecOptions::new();
    if id == CODEC {
        let parsed: OximEncOpts = parse_options(&params.options)?;
        if params.options.get("quality").is_some() {
            out_params.options = out_params
                .options
                .set("quality", parsed.quality.to_string());
        }
    } else if let Some((k, _)) = params.options.iter().next() {
        return Err(Error::invalid(format!("unknown option '{k}'")));
    }
    Ok(Box::new(OximEncoder {
        id: CodecId::new(id),
        out_params,
        pending: Vec::new(),
        eof: false,
    }))
}

fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(CODEC, FORMATS, params)
}

fn make_encoder_yuv(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(CODEC_YUV_ONLY, &[PixelFormat::Yuv420P], params)
}

fn make_encoder_undeclared(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_for(
        CODEC_UNDECLARED,
        &[
            PixelFormat::Yuv420P,
            PixelFormat::Yuv444P,
            PixelFormat::Gray8,
        ],
        params,
    )
}

impl Encoder for OximEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            _ => return Err(Error::invalid("oxim encoder: video only")),
        };
        if self.out_params.options.get("reject_at_send").is_some() {
            return Err(Error::unsupported("oxim encoder: rejected at send"));
        }
        let (w, h, f) = (
            self.out_params.width.unwrap_or(0),
            self.out_params.height.unwrap_or(0),
            self.out_params.pixel_format.unwrap_or(PixelFormat::Rgba),
        );
        let mut tight = Vec::new();
        for (p, plane) in vf.image_planes().iter().enumerate() {
            let row = f
                .plane_row_bytes(p, w)
                .ok_or_else(|| Error::invalid("oxim: row"))?;
            let (_, rows) = f
                .plane_dimensions(p, w, h)
                .ok_or_else(|| Error::invalid("oxim: dims"))?;
            let mut data = Vec::with_capacity(row * rows as usize);
            for r in 0..rows as usize {
                let s = r * plane.stride;
                data.extend_from_slice(
                    plane
                        .data
                        .get(s..s + row)
                        .ok_or_else(|| Error::invalid("oxim: short plane"))?,
                );
            }
            tight.push(VideoPlane { stride: row, data });
        }
        let payload = encode_payload(&tight, vf.palette());
        let mut pkt = Packet::new(0, TIME_BASE, payload);
        pkt.pts = vf.pts;
        pkt.dts = vf.pts;
        pkt.flags.keyframe = true;
        self.pending.push(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if !self.pending.is_empty() {
            return Ok(self.pending.remove(0));
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

// ---- container -------------------------------------------------------------

struct OximDemuxer {
    name: &'static str,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    /// `(stream, delay_ms, payload)` in file order.
    frames: std::collections::VecDeque<(u32, u32, Vec<u8>)>,
    flags: u8,
    /// Running pts per stream.
    pts: Vec<i64>,
}

struct Header {
    width: u32,
    height: u32,
    format: PixelFormat,
    flags: u8,
    frames: u32,
    metadata: Vec<(String, String)>,
    /// Offset of the first frame record.
    body: usize,
}

fn parse_header(buf: &[u8]) -> Result<Header> {
    if buf.len() < 20 || &buf[0..4] != MAGIC {
        return Err(Error::invalid("oxim: bad magic"));
    }
    let width = rd_u32(buf, 4)?;
    let height = rd_u32(buf, 8)?;
    let format =
        format_of(buf[12]).ok_or_else(|| Error::unsupported("oxim: unknown layout tag"))?;
    let flags = buf[13];
    let frames = rd_u32(buf, 14)?;
    let meta_len = u16::from_le_bytes([buf[18], buf[19]]) as usize;
    let meta = buf
        .get(20..20 + meta_len)
        .ok_or_else(|| Error::invalid("oxim: truncated metadata"))?;
    let metadata = std::str::from_utf8(meta)
        .map_err(|_| Error::invalid("oxim: metadata not utf8"))?
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    Ok(Header {
        width,
        height,
        format,
        flags,
        frames,
        metadata,
        body: 20 + meta_len,
    })
}

/// One parsed OXIM block: its stream, metadata, frames, flags and where
/// the next block starts.
struct Block {
    info: StreamInfo,
    metadata: Vec<(String, String)>,
    frames: Vec<(u32, u32, Vec<u8>)>,
    flags: u8,
    next: usize,
}

/// Parse one OXIM block at `at`.
fn parse_block(buf: &[u8], at: usize, stream: u32, codec: &str) -> Result<Block> {
    let hdr = parse_header(&buf[at..])?;
    let mut pos = at + hdr.body;
    let mut frames = Vec::new();
    for _ in 0..hdr.frames {
        let delay = rd_u32(buf, pos)?;
        let len = rd_u32(buf, pos + 4)? as usize;
        let payload = buf
            .get(pos + 8..pos + 8 + len)
            .ok_or_else(|| Error::invalid("oxim: truncated frame"))?
            .to_vec();
        pos += 8 + len;
        frames.push((stream, delay, payload));
    }
    let mut params = CodecParameters::video(CodecId::new(codec));
    params.width = Some(hdr.width);
    params.height = Some(hdr.height);
    params.pixel_format = Some(hdr.format);
    let total: i64 = frames.iter().map(|(_, d, _)| *d as i64).sum();
    let info = StreamInfo {
        index: stream,
        time_base: TIME_BASE,
        duration: Some(total),
        start_time: Some(0),
        params,
    };
    Ok(Block {
        info,
        metadata: hdr.metadata,
        frames,
        flags: hdr.flags,
        next: pos,
    })
}

fn open_demuxer_named(
    name: &'static str,
    codec: &str,
    multi: bool,
    mut input: Box<dyn ReadSeek>,
) -> Result<Box<dyn Demuxer>> {
    input.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    input.read_to_end(&mut buf)?;
    let mut streams = Vec::new();
    let mut metadata = Vec::new();
    let mut frames = std::collections::VecDeque::new();
    let mut flags = None;
    let mut at = 0;
    loop {
        let block = parse_block(&buf, at, streams.len() as u32, codec)?;
        streams.push(block.info);
        if metadata.is_empty() {
            metadata = block.metadata;
        }
        frames.extend(block.frames);
        flags.get_or_insert(block.flags);
        at = block.next;
        if !multi || at >= buf.len() {
            break;
        }
    }
    let flags = flags.unwrap_or(0);
    let n = streams.len();
    Ok(Box::new(OximDemuxer {
        name,
        streams,
        metadata,
        frames,
        flags,
        pts: vec![0; n],
    }))
}

fn open_demuxer(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_demuxer_named(CONTAINER, CODEC, false, input)
}

fn open_demuxer_multi(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    open_demuxer_named(CONTAINER_MULTI, CODEC, true, input)
}

fn open_demuxer_rgb(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    open_demuxer_named(CONTAINER_RGB_ONLY, CODEC, false, input)
}

fn open_demuxer_nodec(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    open_demuxer_named(CONTAINER_NO_DECODER, CODEC_NO_DECODER, false, input)
}

impl Demuxer for OximDemuxer {
    fn format_name(&self) -> &str {
        self.name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let (stream, delay, payload) = self.frames.pop_front().ok_or(Error::Eof)?;
        let mut pkt = Packet::new(stream, TIME_BASE, payload);
        let pts = &mut self.pts[stream as usize];
        if self.flags & FLAG_NO_PTS == 0 {
            pkt.pts = Some(*pts);
            pkt.dts = Some(*pts);
        }
        if self.flags & FLAG_DURATIONS != 0 {
            pkt.duration = Some(delay as i64);
        }
        pkt.flags.keyframe = true;
        *pts += delay as i64;
        Ok(pkt)
    }
}

struct OximMuxer {
    name: &'static str,
    output: Box<dyn WriteSeek>,
    streams: Vec<StreamInfo>,
    packets: Vec<Packet>,
}

fn open_muxer_named(
    name: &'static str,
    rgb_only: bool,
    multi: bool,
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
) -> Result<Box<dyn Muxer>> {
    if streams.is_empty() || (!multi && streams.len() != 1) {
        return Err(Error::invalid(format!(
            "{name} muxer: exactly one video stream, {} given",
            streams.len()
        )));
    }
    for s in streams {
        if s.params.media_type != MediaType::Video {
            return Err(Error::invalid("oxim muxer: video streams only"));
        }
        let f = s
            .params
            .pixel_format
            .ok_or_else(|| Error::invalid("oxim muxer: stream has no pixel format"))?;
        if rgb_only && f != PixelFormat::Rgb24 {
            return Err(Error::unsupported(format!(
                "oximrgb muxer accepts Rgb24 streams only, not {f:?}"
            )));
        }
        if tag_of(f).is_none() {
            return Err(Error::unsupported(format!("oxim muxer: no tag for {f:?}")));
        }
    }
    Ok(Box::new(OximMuxer {
        name,
        output,
        streams: streams.to_vec(),
        packets: Vec::new(),
    }))
}

fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_muxer_named(CONTAINER, false, false, output, streams)
}

fn open_muxer_rgb(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_muxer_named(CONTAINER_RGB_ONLY, true, false, output, streams)
}

fn open_muxer_multi(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_muxer_named(CONTAINER_MULTI, false, true, output, streams)
}

impl Muxer for OximMuxer {
    fn format_name(&self) -> &str {
        self.name
    }

    fn write_header(&mut self) -> Result<()> {
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push(packet.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut out = Vec::new();
        for (si, stream) in self.streams.iter().enumerate() {
            let p = &stream.params;
            out.extend_from_slice(MAGIC);
            out.extend_from_slice(&p.width.unwrap_or(0).to_le_bytes());
            out.extend_from_slice(&p.height.unwrap_or(0).to_le_bytes());
            out.push(tag_of(p.pixel_format.unwrap_or(PixelFormat::Rgba)).expect("checked at open"));
            out.push(FLAG_DURATIONS);
            let packets: Vec<&Packet> = self
                .packets
                .iter()
                .filter(|pkt| pkt.stream_index as usize == si)
                .collect();
            out.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            let mut meta = format!("muxer={}\n", self.name).into_bytes();
            for (k, v) in p.options.iter() {
                meta.extend_from_slice(format!("{k}={v}\n").as_bytes());
            }
            out.extend_from_slice(&(meta.len() as u16).to_le_bytes());
            out.extend_from_slice(&meta);
            // Delays: the packet's duration in its own time base, else the
            // gap to the next packet, else 0 — rescaled to milliseconds.
            for (i, pkt) in packets.iter().enumerate() {
                let dur = pkt.duration.or_else(|| {
                    let next = packets.get(i + 1)?.pts?;
                    Some(next - pkt.pts?)
                });
                let delay_ms = dur
                    .map(|d| pkt.time_base.rescale(d, TIME_BASE))
                    .unwrap_or(0)
                    .max(0) as u32;
                out.extend_from_slice(&delay_ms.to_le_bytes());
                out.extend_from_slice(&(pkt.data.len() as u32).to_le_bytes());
                out.extend_from_slice(&pkt.data);
            }
        }
        self.output.write_all(&out)?;
        self.output.flush()?;
        Ok(())
    }
}

/// The documented convention: magic corroborated by the extension is
/// `MAX_PROBE_SCORE`, magic alone 50, extension alone
/// `PROBE_SCORE_EXTENSION`. The sibling containers share the magic and
/// score `MAX_PROBE_SCORE` only on their own extension, so an extension
/// hint decides between them.
fn probe(data: &ProbeData) -> ProbeScore {
    let magic = data.buf.len() >= 4 && &data.buf[0..4] == MAGIC;
    match (magic, data.ext) {
        (true, Some("oxim")) => MAX_PROBE_SCORE,
        (true, _) => 50,
        (false, Some("oxim")) => PROBE_SCORE_EXTENSION,
        _ => 0,
    }
}

fn probe_sibling(data: &ProbeData, ext: &str) -> ProbeScore {
    let magic = data.buf.len() >= 4 && &data.buf[0..4] == MAGIC;
    if data.ext == Some(ext) {
        if magic {
            MAX_PROBE_SCORE
        } else {
            PROBE_SCORE_EXTENSION
        }
    } else {
        0
    }
}

fn probe_rgb(data: &ProbeData) -> ProbeScore {
    probe_sibling(data, "oximrgb")
}

fn probe_nodec(data: &ProbeData) -> ProbeScore {
    probe_sibling(data, "oximnodec")
}

fn probe_multi(data: &ProbeData) -> ProbeScore {
    probe_sibling(data, "oximmulti")
}

/// Register the synthetic codec and containers into `ctx`, the way a
/// sibling crate's `register` does.
pub(crate) fn register(ctx: &mut RuntimeContext) {
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC))
            .capabilities(
                CodecCapabilities::video("oxim_sw")
                    .with_intra_only(true)
                    .with_lossless(true)
                    .with_pixel_formats(FORMATS.to_vec()),
            )
            .decoder(make_decoder)
            .encoder(make_encoder)
            .encoder_options::<OximEncOpts>(),
    );
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC_YUV_ONLY))
            .capabilities(
                CodecCapabilities::video("oxim_yuv_sw")
                    .with_intra_only(true)
                    .with_pixel_formats(vec![PixelFormat::Yuv420P]),
            )
            .decoder(make_decoder)
            .encoder(make_encoder_yuv),
    );
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC_UNDECLARED))
            .capabilities(CodecCapabilities::video("oxim_undeclared_sw").with_intra_only(true))
            .decoder(make_decoder)
            .encoder(make_encoder_undeclared),
    );
    let c = &mut ctx.containers;
    c.register_demuxer(CONTAINER, open_demuxer);
    c.register_muxer(CONTAINER, open_muxer);
    c.register_extension("oxim", CONTAINER);
    c.register_probe(CONTAINER, probe);
    c.register_demuxer(CONTAINER_RGB_ONLY, open_demuxer_rgb);
    c.register_muxer(CONTAINER_RGB_ONLY, open_muxer_rgb);
    c.register_extension("oximrgb", CONTAINER_RGB_ONLY);
    c.register_probe(CONTAINER_RGB_ONLY, probe_rgb);
    c.register_demuxer(CONTAINER_NO_DECODER, open_demuxer_nodec);
    c.register_extension("oximnodec", CONTAINER_NO_DECODER);
    c.register_probe(CONTAINER_NO_DECODER, probe_nodec);
    c.register_demuxer(CONTAINER_MULTI, open_demuxer_multi);
    c.register_muxer(CONTAINER_MULTI, open_muxer_multi);
    c.register_extension("oximmulti", CONTAINER_MULTI);
    c.register_probe(CONTAINER_MULTI, probe_multi);
}

/// A context with only the synthetic pair registered.
pub(crate) fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    register(&mut ctx);
    ctx
}
