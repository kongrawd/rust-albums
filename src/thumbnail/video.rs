//! Video thumbnails: the first keyframe of an MP4 or QuickTime file (see
//! `mp4`), decoded by rust_h264 or rust_h265 and made a JPEG by `picture`.
//! Only that one frame is decoded, single-threaded, and pictures above
//! [`MAX_PIXELS`] are refused before the decoder allocates them.
//!
//! Releases are built with `panic = "abort"`, so a bug in a decoder aborts
//! the process running it: callers run this in a process of its own, never
//! in the server, to keep one bad file from taking the server down.

use super::picture::{self, Yuv420};
use super::{Error, MAX_PIXELS, Reason, mp4};
use std::io::{Read, Seek};

/// rust_h265's bound on the memory a picture's state may take.
const MAX_PICTURE_MEMORY: u64 = 256 << 20;

/// The thumbnail of the video in `file` (`len` bytes), as a JPEG.
pub fn thumbnail<R: Read + Seek>(file: &mut R, len: u64) -> Result<Vec<u8>, Error> {
    let keyframe = mp4::first_keyframe(file, len)?;
    let picture = decode_keyframe(&keyframe)?;
    Ok(picture::thumbnail(&picture, keyframe.orientation))
}

fn decode_keyframe(keyframe: &mp4::Keyframe) -> Result<Yuv420, Error> {
    match keyframe.codec {
        mp4::Codec::H264 => decode_h264(keyframe),
        mp4::Codec::Hevc => decode_hevc(keyframe),
    }
}

fn decode_h264(keyframe: &mp4::Keyframe) -> Result<Yuv420, Error> {
    use rust_h264::decoder::{ChromaFormat, Decoder};
    use rust_h264::nal::{NalUnitType, parse_avcc, parse_avcc_config};
    let config = parse_avcc_config(&keyframe.config).map_err(|_| Reason::Damaged)?;
    let sample = parse_avcc(&keyframe.sample, config.length_size);
    let nals = config
        .sps_nals
        .iter()
        .chain(&config.pps_nals)
        .chain(&sample);
    let mut decoder = Decoder::new();
    let mut frame = None;
    for nal in nals {
        // rust_h264 has no size limit of its own: check every SPS first, on
        // the coded size it allocates, not the cropped size it shows
        // (PicWidthInMbs and FrameHeightInMbs, equations 7-13 and 7-18).
        if nal.nal_unit_type == NalUnitType::Sps {
            let sps = rust_h264::sps::parse_sps(&nal.rbsp).map_err(|_| Reason::Damaged)?;
            let width = u64::from(sps.pic_width_in_mbs_minus1) + 1;
            let map_units = u64::from(sps.pic_height_in_map_units_minus1) + 1;
            let height = (2 - u64::from(sps.frame_mbs_only_flag)) * map_units;
            if width * 16 * height * 16 > u64::from(MAX_PIXELS) {
                return Err(Reason::PictureTooLarge.into());
            }
        }
        if let Some(decoded) = decoder.decode_nal(nal).map_err(|_| Reason::Damaged)? {
            frame = Some(decoded);
        }
    }
    let frame = frame.or_else(|| decoder.flush()).ok_or(Reason::Damaged)?;
    let is_420 = frame.chroma_format == ChromaFormat::Yuv420;
    // Above 8 bits, samples are 2 bytes each, little-endian.
    let wide = frame.bit_depth > 8;
    let plane = |samples| {
        if wide {
            Plane::LittleEndian16(samples)
        } else {
            Plane::Bytes(samples)
        }
    };
    let planes = [
        plane(&frame.y[..]),
        plane(&frame.u[..]),
        plane(&frame.v[..]),
    ];
    yuv420(frame.width, frame.height, is_420, frame.bit_depth, planes)
}

fn decode_hevc(keyframe: &mp4::Keyframe) -> Result<Yuv420, Error> {
    use rust_h265::{DecodeLimits, Decoder, PixelData, parse_hvcc, parse_nal};
    let config = HevcConfig::parse(&keyframe.config)?;
    if config.chroma_depth != config.luma_depth {
        return Err(Reason::BitDepths.into());
    }
    let limits = DecodeLimits {
        max_picture_pixels: u64::from(MAX_PIXELS),
        max_picture_memory_bytes: MAX_PICTURE_MEMORY,
    };
    let mut decoder = Decoder::with_limits(limits).with_display_order();
    for set in config.parameter_sets {
        let nal = parse_nal(set).ok_or(Reason::Damaged)?;
        decoder.decode_nal(&nal).map_err(|_| Reason::Damaged)?;
    }
    for nal in parse_hvcc(&keyframe.sample, config.length_size) {
        decoder.decode_nal(&nal).map_err(|_| Reason::Damaged)?;
    }
    let frame = decoder
        .next_output()
        .or_else(|| decoder.flush())
        .ok_or(Reason::Damaged)?;
    fn plane(samples: &PixelData) -> Plane<'_> {
        match samples {
            PixelData::U8(samples) => Plane::Bytes(samples),
            PixelData::U16(samples) => Plane::Wide(samples),
        }
    }
    let planes = [plane(&frame.y), plane(&frame.u), plane(&frame.v)];
    let is_420 = frame.chroma_format_idc == 1;
    yuv420(frame.width, frame.height, is_420, frame.bit_depth, planes)
}

/// A decoded plane, as a decoder gives it.
enum Plane<'a> {
    Bytes(&'a [u8]),
    /// Samples above 8 bits, 2 bytes each, little-endian (rust_h264).
    LittleEndian16(&'a [u8]),
    /// Samples above 8 bits (rust_h265).
    Wide(&'a [u16]),
}

impl Plane<'_> {
    /// The samples, rounded to 8 bits from `depth` bits.
    fn to_8bit(&self, depth: u8) -> Vec<u8> {
        let shift = u32::from(depth.saturating_sub(8));
        let round = |sample: u16| ((u32::from(sample) + (1 << shift >> 1)) >> shift).min(255) as u8;
        match self {
            Self::Bytes(samples) => samples.to_vec(),
            Self::LittleEndian16(bytes) => bytes
                .chunks_exact(2)
                .map(|s| round(u16::from_le_bytes([s[0], s[1]])))
                .collect(),
            Self::Wide(samples) => samples.iter().map(|&s| round(s)).collect(),
        }
    }
}

/// A decoded frame as a [`Yuv420`]: it must be 4:2:0, and its planes (at
/// `depth` bits) must fit its size.
fn yuv420(
    width: u32,
    height: u32,
    is_420: bool,
    depth: u8,
    planes: [Plane; 3],
) -> Result<Yuv420, Error> {
    if !is_420 {
        return Err(Reason::Chroma.into());
    }
    let [y, cb, cr] = planes.map(|plane| plane.to_8bit(depth));
    Yuv420::new(width as usize, height as usize, y, cb, cr).ok_or(Reason::Damaged.into())
}

/// What an `hvcC` box body says (ISO/IEC 14496-15 §8.3.3.1): profile and
/// format fields, including the bit depths (bytes 17 and 18) and the NAL unit
/// length size (byte 21), then arrays of length-prefixed parameter sets.
struct HevcConfig<'a> {
    luma_depth: u8,
    chroma_depth: u8,
    length_size: u8,
    parameter_sets: Vec<&'a [u8]>,
}

impl<'a> HevcConfig<'a> {
    fn parse(config: &'a [u8]) -> Result<Self, Error> {
        let byte = |at: usize| config.get(at).copied().ok_or(Reason::Damaged);
        let (luma_depth, chroma_depth) = ((byte(17)? & 7) + 8, (byte(18)? & 7) + 8);
        let length_size = (byte(21)? & 3) + 1;
        let mut at = 23;
        let mut parameter_sets = Vec::new();
        for _ in 0..byte(22)? {
            let count = u16::from_be_bytes([byte(at + 1)?, byte(at + 2)?]);
            at += 3;
            for _ in 0..count {
                let len = usize::from(u16::from_be_bytes([byte(at)?, byte(at + 1)?]));
                parameter_sets.push(config.get(at + 2..at + 2 + len).ok_or(Reason::Damaged)?);
                at += 2 + len;
            }
        }
        Ok(Self {
            luma_depth,
            chroma_depth,
            length_size,
            parameter_sets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::decode_jpeg;
    use super::*;
    use std::io::Cursor;

    const HEVC_DEMO: &[u8] = include_bytes!("../../storage/Samples/hevc-demo.mp4");
    /// H.264 High, 96 × 64 coded, red on the left and blue on the right,
    /// stored turned: shown upright it is 64 × 96, red on top.
    const PORTRAIT: &[u8] = include_bytes!("testdata/portrait.mov");
    /// HEVC Main 10, 1280 × 720, all green.
    const MAIN10: &[u8] = include_bytes!("testdata/main10.mp4");

    fn thumbnail_of(file: &[u8]) -> Result<Vec<u8>, Error> {
        thumbnail(&mut Cursor::new(file), file.len() as u64)
    }

    fn keyframe_of(file: &[u8]) -> mp4::Keyframe {
        mp4::first_keyframe(&mut Cursor::new(file), file.len() as u64).unwrap()
    }

    fn reason_of(result: Result<Yuv420, Error>) -> Reason {
        match result {
            Err(Error::Unsupported(reason)) => reason,
            Err(other) => panic!("expected unsupported, got {other}"),
            Ok(_) => panic!("expected unsupported, got a picture"),
        }
    }

    fn pixel(pixels: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
        let at = (y * width + x) * 3;
        [pixels[at], pixels[at + 1], pixels[at + 2]]
    }

    #[test]
    fn hevc_video_keeps_its_size_and_colours() {
        let (width, height, pixels) = decode_jpeg(&thumbnail_of(HEVC_DEMO).unwrap());
        assert_eq!((width, height), (640, 360), "small enough to keep its size");
        // The demo's background is a blue gradient.
        let [r, _, b] = pixel(&pixels, width, 8, 8);
        assert!(b > 100 && b > r + 50, "{r} {b}");
    }

    #[test]
    fn h264_video_is_shown_upright() {
        let (width, height, pixels) = decode_jpeg(&thumbnail_of(PORTRAIT).unwrap());
        assert_eq!((width, height), (64, 96));
        let [r, _, b] = pixel(&pixels, width, 32, 20);
        assert!(r > 200 && b < 60, "red on top: {r} {b}");
        let [r, _, b] = pixel(&pixels, width, 32, 76);
        assert!(b > 200 && r < 60, "blue below: {r} {b}");
    }

    #[test]
    fn ten_bit_video_is_shrunk_to_thumbnail_size() {
        let (width, height, pixels) = decode_jpeg(&thumbnail_of(MAIN10).unwrap());
        assert_eq!((width, height), (711, 400));
        let [r, g, b] = pixel(&pixels, width, 355, 200);
        assert!(g > 150 && r < 40 && b < 100, "green: {r} {g} {b}");
    }

    /// Flips a few bits after each sample's first bytes, `count` times, and
    /// decodes: an error or a damaged picture, never a panic or a read error.
    fn decode_damaged(count: u32) -> std::time::Duration {
        let mut state = 0x2545_f491_u32;
        let mut slowest = std::time::Duration::ZERO;
        for file in [HEVC_DEMO, PORTRAIT] {
            let keyframe = keyframe_of(file);
            for _ in 0..count {
                let mut damaged = keyframe.sample.clone();
                for _ in 0..1 + state % 8 {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let at = 8 + state as usize % (damaged.len() - 8);
                    damaged[at] ^= 1 << (state % 8);
                }
                let damaged = mp4::Keyframe {
                    sample: damaged,
                    config: keyframe.config.clone(),
                    ..keyframe
                };
                let started = std::time::Instant::now();
                let result = decode_keyframe(&damaged);
                slowest = slowest.max(started.elapsed());
                assert!(!matches!(result, Err(Error::Io(_))));
            }
        }
        slowest
    }

    #[test]
    fn damaged_keyframes_never_panic() {
        decode_damaged(300);
    }

    /// The same, at length: run once in a while with
    /// `cargo test --release -- --ignored damaged_keyframes_at_length`.
    #[test]
    #[ignore]
    fn damaged_keyframes_at_length() {
        let slowest = decode_damaged(200_000);
        eprintln!("slowest damaged keyframe: {slowest:?}");
    }

    /// Bits, then Exp-Golomb codes, written MSB first, for hand-made H.264.
    struct Writer(Vec<bool>);

    impl Writer {
        fn u(&mut self, value: u32, count: u32) -> &mut Self {
            self.0.extend((0..count).rev().map(|i| value >> i & 1 == 1));
            self
        }

        fn ue(&mut self, value: u32) -> &mut Self {
            let bits = 32 - (value + 1).leading_zeros();
            self.u(0, bits - 1).u(value + 1, bits)
        }

        /// The bits as a NAL unit with this header byte, after the stop bit.
        fn nal(&mut self, header: u8) -> Vec<u8> {
            self.u(1, 1);
            while !self.0.len().is_multiple_of(8) {
                self.0.push(false);
            }
            let bytes = self
                .0
                .chunks(8)
                .map(|byte| byte.iter().fold(0, |b, &bit| b << 1 | u8::from(bit)));
            std::iter::once(header).chain(bytes).collect()
        }
    }

    /// A Baseline H.264 SPS for `width_mbs` × `height_mbs` macroblocks,
    /// cropped by `crop` (left, right, top, bottom, in 2-pixel units), and a
    /// plain PPS, as an `avcC` box body.
    fn avc_config(width_mbs: u32, height_mbs: u32, crop: Option<[u32; 4]>) -> Vec<u8> {
        let mut sps = Writer(Vec::new());
        sps.u(66, 8)
            .u(0, 8)
            .u(51, 8)
            .ue(0)
            .ue(0)
            .ue(2)
            .ue(1)
            .u(0, 1);
        sps.ue(width_mbs - 1).ue(height_mbs - 1).u(1, 1).u(1, 1);
        match crop {
            Some(edges) => {
                sps.u(1, 1);
                for edge in edges {
                    sps.ue(edge);
                }
            }
            None => {
                sps.u(0, 1);
            }
        }
        let sps = sps.u(0, 1).nal(0x67);
        let pps = Writer(Vec::new())
            .ue(0)
            .ue(0)
            .u(0, 1)
            .u(0, 1)
            .ue(0)
            .ue(0)
            .ue(0)
            .u(0, 3)
            .ue(0)
            .ue(0)
            .ue(0)
            .u(1, 1)
            .u(0, 2)
            .nal(0x68);
        let mut config = vec![1, 66, 0, 51, 0xFF, 0xE1];
        config.extend((sps.len() as u16).to_be_bytes());
        config.extend(&sps);
        config.push(1);
        config.extend((pps.len() as u16).to_be_bytes());
        config.extend(&pps);
        config
    }

    /// A sample of one IDR slice of that SPS and PPS: a valid I-slice header,
    /// so a decoder gets as far as allocating the picture, then garbage.
    fn idr_slice() -> Vec<u8> {
        let mut slice = Writer(Vec::new());
        // first_mb_in_slice 0, slice_type 7 (I), pic_parameter_set_id 0,
        // frame_num (4 bits), idr_pic_id 0, no_output_of_prior_pics_flag and
        // long_term_reference_flag 0, slice_qp_delta 0, deblocking off (1).
        slice.ue(0).ue(7).ue(0).u(0, 4).ue(0).u(0, 2).ue(0).ue(1);
        slice.u(0x5555, 16).u(0x5555, 16);
        let nal = slice.nal(0x65);
        [(nal.len() as u32).to_be_bytes().to_vec(), nal].concat()
    }

    /// rust_h264 has no size limit, so a parameter set describing a picture
    /// larger than `MAX_PIXELS` is refused before decoding: also when its
    /// cropping makes it look small, as the decoder allocates the full size.
    #[test]
    fn refuses_an_oversized_h264_picture() {
        let keyframe = |config| mp4::Keyframe {
            codec: mp4::Codec::H264,
            config,
            sample: idr_slice(),
            orientation: super::super::Orientation::Upright,
        };
        // 8192 × 4320.
        let large = keyframe(avc_config(512, 270, None));
        assert_eq!(reason_of(decode_keyframe(&large)), Reason::PictureTooLarge);
        // 4112 × 2320, just above the limit, cropped to 16 × 16.
        let cropped = keyframe(avc_config(257, 145, Some([2048, 0, 0, 1152])));
        assert_eq!(
            reason_of(decode_keyframe(&cropped)),
            Reason::PictureTooLarge
        );
    }

    /// rust_h265 stores chroma at its own bit depth, without saying which:
    /// streams whose chroma and luma depths differ are refused.
    #[test]
    fn refuses_hevc_with_chroma_at_another_depth() {
        let mut keyframe = keyframe_of(MAIN10);
        // hvcC byte 18: 5 reserved bits, then bitDepthChromaMinus8. Make it 8-bit.
        keyframe.config[18] = 0xF8;
        assert_eq!(reason_of(decode_keyframe(&keyframe)), Reason::BitDepths);
    }

    #[test]
    fn a_file_without_video_has_no_thumbnail() {
        let error = thumbnail_of(b"\0\0\0\x0cftypM4A ").unwrap_err();
        assert!(
            matches!(error, Error::Unsupported(Reason::NoIndex)),
            "{error}"
        );
    }
}
