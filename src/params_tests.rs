//! The gateway labels pictures from the stream parameters merged with
//! what the decoder reports (`Decoder::output_params`), read before
//! the first picture and again only when the decoder's
//! `output_params_generation` moves.

use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecResolver, Decoder, Demuxer, Error,
    Frame, Packet, PixelFormat, ReadSeek, Result, RuntimeContext, StreamInfo, TimeBase, VideoFrame,
    VideoPlane,
};

use crate::{OpenOptions, decode_bytes_with};

const CODEC: &str = "oxim_watch";
const PICTURES: u8 = 6;
/// The first picture decoded in the new shape.
const SWITCH_AT: u8 = 3;

static QUERIES: AtomicUsize = AtomicUsize::new(0);

struct Demux {
    streams: Vec<StreamInfo>,
    next: u8,
}

impl Demuxer for Demux {
    fn format_name(&self) -> &str {
        CODEC
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        if self.next == PICTURES {
            return Err(Error::Eof);
        }
        let n = self.next;
        self.next += 1;
        Ok(Packet::new(0, TimeBase::new(1, 10), vec![n]).with_pts(n as i64))
    }
}

fn open_demuxer(_: Box<dyn ReadSeek>, _: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    // The container's claim: 2×2 Rgb24.
    let mut params = CodecParameters::video(CodecId::new(CODEC));
    params.width = Some(2);
    params.height = Some(2);
    params.pixel_format = Some(PixelFormat::Rgb24);
    Ok(Box::new(Demux {
        streams: vec![StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 10),
            duration: None,
            start_time: Some(0),
            params,
        }],
        next: 0,
    }))
}

/// Emits 2×2 Rgba, then 4×4 Gray8 from picture `SWITCH_AT`.
struct Watched {
    pending: Option<u8>,
    switched: bool,
    eof: bool,
}

impl Decoder for Watched {
    fn codec_id(&self) -> &CodecId {
        static ID: std::sync::OnceLock<CodecId> = std::sync::OnceLock::new();
        ID.get_or_init(|| CodecId::new(CODEC))
    }
    fn send_packet(&mut self, p: &Packet) -> Result<()> {
        self.pending = Some(p.data[0]);
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(n) = self.pending.take() else {
            return Err(if self.eof {
                Error::Eof
            } else {
                Error::NeedMore
            });
        };
        self.switched = n >= SWITCH_AT;
        let plane = if self.switched {
            VideoPlane {
                stride: 4,
                data: vec![n; 16],
            }
        } else {
            VideoPlane {
                stride: 8,
                data: vec![n; 16],
            }
        };
        Ok(Frame::Video(VideoFrame {
            pts: Some(n as i64),
            planes: vec![plane],
        }))
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
    fn output_params(&self) -> Option<CodecParameters> {
        QUERIES.fetch_add(1, Ordering::SeqCst);
        let mut p = CodecParameters::video(CodecId::new(CODEC));
        if self.switched {
            p.width = Some(4);
            p.height = Some(4);
            p.pixel_format = Some(PixelFormat::Gray8);
        } else {
            p.width = Some(2);
            p.height = Some(2);
            p.pixel_format = Some(PixelFormat::Rgba);
        }
        Some(p)
    }
    fn output_params_generation(&self) -> u64 {
        u64::from(self.switched)
    }
    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.output_params().and_then(|p| p.pixel_format)
    }
}

fn make_decoder(_: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Watched {
        pending: None,
        switched: false,
        eof: false,
    }))
}

#[test]
fn pictures_are_labelled_from_the_watched_decoder_report() {
    let mut ctx = RuntimeContext::new();
    ctx.codecs.register(
        CodecInfo::new(CodecId::new(CODEC))
            .capabilities(CodecCapabilities::video("oxim_watch_sw").with_intra_only(true))
            .decoder(make_decoder),
    );
    ctx.containers.register_demuxer(CODEC, open_demuxer);
    ctx.containers.register_extension(CODEC, CODEC);

    let opts = OpenOptions::new().with_ext_hint(CODEC);
    let before = QUERIES.load(Ordering::SeqCst);
    let file = decode_bytes_with(&ctx, &[0u8; 4], &opts).unwrap();
    // Read before the first picture, on the first picture and at the
    // switch — not once per picture.
    assert_eq!(QUERIES.load(Ordering::SeqCst) - before, 3);

    assert_eq!(file.len(), PICTURES as usize);
    for (i, img) in file.iter().enumerate() {
        let (w, f) = if (i as u8) < SWITCH_AT {
            (2, PixelFormat::Rgba)
        } else {
            (4, PixelFormat::Gray8)
        };
        // The decoder's report wins over the container's Rgb24 claim.
        assert_eq!((img.width(), img.format()), (w, f), "picture {i}");
        assert_eq!(img.params().width, Some(w));
    }
    // One parameter set per shape: shared within, copied at the switch.
    let f = file.frames();
    assert!(std::ptr::eq(f[0].params(), f[1].params()));
    assert!(!std::ptr::eq(f[2].params(), f[3].params()));
    assert!(std::ptr::eq(f[3].params(), f[5].params()));
}
