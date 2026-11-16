//! Finds the first keyframe of a video in an MP4 or QuickTime (MOV) file,
//! reading only the file's index (the `moov` box) and that one frame. The index
//! may come before or after the media data (phones write it last), and the
//! media data is never read through: a 4 GB video costs a few seeks and reads
//! of some kilobytes.
//!
//! The boxes are those of ISO/IEC 14496-12, which grew out of QuickTime's. The
//! codec configurations (`avcC`, `hvcC`, ISO/IEC 14496-15) are passed on as
//! they are, for the decoder. Fragmented MP4, as streamed, isn't supported: its
//! samples aren't in the index. Every offset and count in the file is checked,
//! so a damaged file is an error, never a panic or a runaway read.

use super::{Error, MAX_PIXELS, Orientation, Reason};
use std::io::{Read, Seek, SeekFrom};

/// The largest index read into memory. A one-hour 60 fps video's is a few MB.
const MAX_INDEX: u64 = 32 << 20;
/// The largest keyframe read. A 4K keyframe is a few MB.
const MAX_SAMPLE: u64 = 16 << 20;
/// Bytes of a visual sample entry before its child boxes (§12.1.3).
const VISUAL_SAMPLE_ENTRY: usize = 78;
const DAMAGED: Error = Error::Unsupported(Reason::Damaged);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Debug)]
pub struct Keyframe {
    pub codec: Codec,
    /// The codec configuration, the body of the `avcC` or `hvcC` box: the
    /// parameter sets and the size of each NAL unit's length prefix.
    pub config: Vec<u8>,
    /// The first sync sample: NAL units, each prefixed by its length.
    pub sample: Vec<u8>,
    /// How the picture is shown upright, from the track's matrix: phones
    /// store portrait video sideways.
    pub orientation: Orientation,
}

/// The first keyframe of the first H.264 or HEVC video track in `file`, which
/// is `len` bytes long.
pub fn first_keyframe<R: Read + Seek>(file: &mut R, len: u64) -> Result<Keyframe, Error> {
    let index = read_index(file, len)?;
    let mut unsupported = Error::Unsupported(Reason::NoVideoTrack);
    for (kind, trak) in boxes(&index) {
        if &kind != b"trak" {
            continue;
        }
        match video_track(trak, len) {
            Ok(Some(track)) => {
                let mut sample = vec![0; track.sample_size as usize];
                file.seek(SeekFrom::Start(track.sample_offset))?;
                file.read_exact(&mut sample)?;
                return Ok(Keyframe {
                    codec: track.codec,
                    config: track.config.to_vec(),
                    sample,
                    orientation: track.orientation,
                });
            }
            Ok(None) => {}
            // Another video track may still be readable.
            Err(err) => unsupported = err,
        }
    }
    Err(unsupported)
}

/// The body of the file's `moov` box, found by seeking from box to box.
fn read_index<R: Read + Seek>(file: &mut R, len: u64) -> Result<Vec<u8>, Error> {
    let mut position = 0;
    while len - position >= 8 {
        let remaining = len - position;
        let mut header = [0u8; 16];
        let read = remaining.min(16) as usize;
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut header[..read])?;
        let (kind, size, header_len) = box_header(&header[..read], remaining).ok_or(DAMAGED)?;
        if &kind == b"moov" {
            let body_len = size - header_len;
            if body_len > MAX_INDEX {
                return Err(Reason::IndexTooLarge.into());
            }
            let mut body = vec![0; body_len as usize];
            file.seek(SeekFrom::Start(position + header_len))?;
            file.read_exact(&mut body)?;
            return Ok(body);
        }
        position += size;
    }
    Err(Reason::NoIndex.into())
}

/// A box header (§4.2) at the start of `bytes`: the box's type, its size and
/// the header's own size. A 32-bit size of 1 means a 64-bit size follows; 0
/// means the box runs to the end, `remaining` bytes from its start. `None`
/// when the header is cut off or the size impossible.
fn box_header(bytes: &[u8], remaining: u64) -> Option<([u8; 4], u64, u64)> {
    let kind: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    let (size, header_len) = match u32_at(bytes, 0).ok()? {
        1 => (u64_at(bytes, 8).ok()?, 16),
        0 => (remaining, 8),
        size => (u64::from(size), 8),
    };
    (header_len <= size && size <= remaining).then_some((kind, size, header_len))
}

/// A video track as found in the index, borrowing from it; `first_keyframe`
/// copies it into the owned [`Keyframe`] once the keyframe is read.
struct Track<'a> {
    codec: Codec,
    config: &'a [u8],
    orientation: Orientation,
    sample_offset: u64,
    sample_size: u64,
}

/// The track's codec, size and first keyframe, or `None` if it isn't video.
fn video_track(trak: &[u8], file_len: u64) -> Result<Option<Track<'_>>, Error> {
    let Some(mdia) = child(trak, b"mdia") else {
        return Ok(None);
    };
    // The handler type, after version, flags and 4 more bytes (QuickTime's
    // component type).
    if child(mdia, b"hdlr").and_then(|hdlr| hdlr.get(8..12)) != Some(b"vide") {
        return Ok(None);
    }
    let stbl = child(mdia, b"minf")
        .and_then(|minf| child(minf, b"stbl"))
        .ok_or(DAMAGED)?;
    // The first sample description, after version, flags and the entry count.
    let stsd = child(stbl, b"stsd").ok_or(DAMAGED)?;
    let (entry_kind, entry) = boxes(stsd.get(8..).ok_or(DAMAGED)?).next().ok_or(DAMAGED)?;
    let (codec, config_kind) = match &entry_kind {
        b"avc1" | b"avc3" => (Codec::H264, b"avcC"),
        b"hvc1" | b"hev1" => (Codec::Hevc, b"hvcC"),
        _ => return Err(Reason::Codec.into()),
    };
    let width = u16::from_be_bytes(entry.get(24..26).ok_or(DAMAGED)?.try_into().unwrap());
    let height = u16::from_be_bytes(entry.get(26..28).ok_or(DAMAGED)?.try_into().unwrap());
    if width == 0 || height == 0 {
        return Err(DAMAGED);
    }
    if u32::from(width) * u32::from(height) > MAX_PIXELS {
        return Err(Reason::PictureTooLarge.into());
    }
    let config = entry
        .get(VISUAL_SAMPLE_ENTRY..)
        .and_then(|children| child(children, config_kind))
        .ok_or(DAMAGED)?;
    let (sample_offset, sample_size) = first_keyframe_location(stbl, file_len)?;
    Ok(Some(Track {
        codec,
        config,
        orientation: child(trak, b"tkhd").map_or(Orientation::Upright, orientation),
        sample_offset,
        sample_size,
    }))
}

/// Where the first sync sample is in the file, and its size, from the sample
/// tables: `stss` names it, `stsc` gives its chunk, `stco`/`co64` the chunk's
/// offset, and `stsz` the sizes of it and the samples before it in the chunk.
fn first_keyframe_location(stbl: &[u8], file_len: u64) -> Result<(u64, u64), Error> {
    // Without `stss`, every sample is a sync sample.
    let sample = match child(stbl, b"stss") {
        Some(stss) if u32_at(stss, 4)? == 0 => return Err(Reason::NoKeyframe.into()),
        Some(stss) => u64::from(u32_at(stss, 8)?),
        None => 1,
    };
    let sizes = SampleSizes::new(child(stbl, b"stsz").ok_or(DAMAGED)?)?;
    if sample == 0 || sample > sizes.count {
        return Err(Reason::NoKeyframe.into());
    }

    let (chunk, chunk_first_sample) = chunk_of(child(stbl, b"stsc").ok_or(DAMAGED)?, sample)?;
    let mut offset = match (child(stbl, b"stco"), child(stbl, b"co64")) {
        (Some(stco), _) => u64::from(u32_at(stco, entry_at(8, chunk - 1, 4)?)?),
        (None, Some(co64)) => u64_at(co64, entry_at(8, chunk - 1, 8)?)?,
        (None, None) => return Err(DAMAGED),
    };
    offset = offset
        .checked_add(sizes.total(chunk_first_sample..sample)?)
        .ok_or(DAMAGED)?;
    let size = sizes.size_of(sample)?;
    if size == 0 || size > MAX_SAMPLE {
        return Err(Reason::KeyframeTooLarge.into());
    }
    match offset.checked_add(size) {
        Some(end) if end <= file_len => Ok((offset, size)),
        _ => Err(DAMAGED),
    }
}

/// The sample size table (`stsz`): one size for every sample, or each its own.
struct SampleSizes<'a> {
    table: &'a [u8],
    /// Every sample's size, or 0 when each has its own.
    fixed: u32,
    count: u64,
}

impl<'a> SampleSizes<'a> {
    fn new(table: &'a [u8]) -> Result<Self, Error> {
        let (fixed, count) = (u32_at(table, 4)?, u64::from(u32_at(table, 8)?));
        if fixed == 0 && (table.len() as u64) < 12 + 4 * count {
            return Err(DAMAGED);
        }
        Ok(Self {
            table,
            fixed,
            count,
        })
    }

    /// The size of the 1-based `sample`.
    fn size_of(&self, sample: u64) -> Result<u64, Error> {
        match self.fixed {
            0 => Ok(u64::from(u32_at(self.table, entry_at(12, sample - 1, 4)?)?)),
            fixed => Ok(u64::from(fixed)),
        }
    }

    /// The total size of `samples`.
    fn total(&self, mut samples: std::ops::Range<u64>) -> Result<u64, Error> {
        match self.fixed {
            0 => samples.try_fold(0u64, |sum, sample| {
                sum.checked_add(self.size_of(sample)?).ok_or(DAMAGED)
            }),
            fixed => Ok(u64::from(fixed) * (samples.end - samples.start)),
        }
    }
}

/// The 1-based chunk holding `sample`, and the chunk's first sample, from the
/// sample-to-chunk table: runs of chunks with the same number of samples.
fn chunk_of(stsc: &[u8], sample: u64) -> Result<(u64, u64), Error> {
    let entries = u32_at(stsc, 4)? as usize;
    let mut first_sample = 1;
    for entry in 0..entries {
        let first_chunk = u64::from(u32_at(stsc, 8 + 12 * entry)?);
        let per_chunk = u64::from(u32_at(stsc, 12 + 12 * entry)?);
        if first_chunk == 0 || per_chunk == 0 {
            return Err(DAMAGED);
        }
        // The last run goes on to the last chunk.
        let next_chunk = match entry + 1 < entries {
            true => u64::from(u32_at(stsc, 8 + 12 * (entry + 1))?),
            false => u64::MAX,
        };
        if next_chunk <= first_chunk {
            return Err(DAMAGED);
        }
        let into = sample - first_sample;
        let run = (next_chunk - first_chunk).saturating_mul(per_chunk);
        if into < run {
            let chunks_before = into / per_chunk;
            return Ok((
                first_chunk + chunks_before,
                first_sample + chunks_before * per_chunk,
            ));
        }
        first_sample += run;
    }
    Err(DAMAGED)
}

/// The orientation in a track header's matrix (§8.3.2). Only plain turns are
/// recognised; a mirrored or skewed picture is shown as stored.
fn orientation(tkhd: &[u8]) -> Orientation {
    // After version and flags: times, track id and duration (20 or 32 bytes),
    // then 16 more bytes before the matrix `a b u c d v x y w`.
    let at = 4 + if tkhd.first() == Some(&1) { 32 } else { 20 } + 16;
    let entry = |i: usize| u32_at(tkhd, at + 4 * i).map(|v| (v as i32).signum());
    match (entry(0), entry(1), entry(3), entry(4)) {
        (Ok(0), Ok(1), Ok(-1), Ok(0)) => Orientation::TurnedRight,
        (Ok(-1), Ok(0), Ok(0), Ok(-1)) => Orientation::UpsideDown,
        (Ok(0), Ok(-1), Ok(1), Ok(0)) => Orientation::TurnedLeft,
        _ => Orientation::Upright,
    }
}

/// The boxes in `body`, as (type, body), up to the first damaged one.
fn boxes(mut body: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let (kind, size, header_len) = box_header(body, body.len() as u64)?;
        let (this, rest) = body.split_at(size as usize);
        body = rest;
        Some((kind, &this[header_len as usize..]))
    })
}

fn child<'a>(body: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(body).find(|(k, _)| k == kind).map(|(_, body)| body)
}

/// Where entry `index` of a table starts: `start` plus `index` entries of
/// `width` bytes. Checked, as a damaged count could overflow a 32-bit `usize`.
fn entry_at(start: usize, index: u64, width: usize) -> Result<usize, Error> {
    usize::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(width))
        .and_then(|offset| offset.checked_add(start))
        .ok_or(DAMAGED)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, Error> {
    let field = bytes
        .get(at..at.checked_add(4).ok_or(DAMAGED)?)
        .ok_or(DAMAGED)?;
    Ok(u32::from_be_bytes(field.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, Error> {
    let field = bytes
        .get(at..at.checked_add(8).ok_or(DAMAGED)?)
        .ok_or(DAMAGED)?;
    Ok(u64::from_be_bytes(field.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    const HEVC_DEMO: &[u8] = include_bytes!("../../storage/Samples/hevc-demo.mp4");
    /// Fixed-point 16.16 matrix entries (ISO/IEC 14496-12 §6.2.2).
    const ONE: i32 = 0x1_0000;
    const IDENTITY: [i32; 4] = [ONE, 0, 0, ONE];

    fn read(file: &[u8]) -> Result<Keyframe, Error> {
        first_keyframe(&mut Cursor::new(file), file.len() as u64)
    }

    /// Why `file` has no keyframe; it must be unsupported, not unreadable.
    fn reason_of(file: &[u8]) -> Reason {
        match read(file) {
            Err(Error::Unsupported(reason)) => reason,
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn finds_the_first_keyframe_of_a_real_file() {
        let keyframe = read(HEVC_DEMO).unwrap();
        assert_eq!(keyframe.codec, Codec::Hevc);
        assert_eq!(keyframe.orientation, Orientation::Upright);
        assert_eq!(keyframe.config[0], 1, "hvcC configurationVersion");
        // Its first sample is the keyframe, 6,401 bytes at offset 4,716.
        assert_eq!(keyframe.sample, HEVC_DEMO[4716..4716 + 6401]);
    }

    #[test]
    fn finds_the_keyframe_wherever_the_index_is() {
        for moov_first in [true, false] {
            let video = Video {
                moov_first,
                ..Video::default()
            };
            let keyframe = read(&video.build()).unwrap();
            assert_eq!(keyframe.codec, Codec::H264);
            assert_eq!(keyframe.config, b"\x01avcC body");
            assert_eq!(keyframe.sample, video.samples[0]);
        }
    }

    /// The keyframe's place comes from the sample tables: which sample it is,
    /// which chunk holds it, and the sizes of the samples before it there.
    #[test]
    fn finds_the_keyframe_from_the_sample_tables() {
        let second = Video {
            keyframes: Some(vec![2, 3]),
            ..Video::default()
        };
        let third = Video {
            keyframes: Some(vec![3]),
            co64: true,
            ..Video::default()
        };
        let every_sample = Video {
            keyframes: None,
            ..Video::default()
        };
        for (video, sample) in [(&second, 1), (&third, 2), (&every_sample, 0)] {
            let keyframe = read(&video.build()).unwrap();
            assert_eq!(keyframe.sample, video.samples[sample]);
        }
    }

    /// Phones store portrait video sideways, with a matrix that turns it.
    #[test]
    fn reads_the_turn_from_the_track_matrix() {
        use Orientation::*;
        for (matrix, orientation) in [
            (IDENTITY, Upright),
            ([0, ONE, -ONE, 0], TurnedRight),
            ([-ONE, 0, 0, -ONE], UpsideDown),
            ([0, -ONE, ONE, 0], TurnedLeft),
            ([-ONE, 0, 0, ONE], Upright), // mirrored: shown as stored
        ] {
            let video = Video {
                matrix,
                ..Video::default()
            };
            assert_eq!(
                read(&video.build()).unwrap().orientation,
                orientation,
                "{matrix:?}"
            );
        }
    }

    #[test]
    fn takes_the_video_track_and_refuses_what_it_cannot_decode() {
        let after_audio = Video {
            audio_track_first: true,
            ..Video::default()
        };
        assert_eq!(read(&after_audio.build()).unwrap().codec, Codec::H264);
        let hevc = Video {
            sample_entry: b"hev1",
            config_box: b"hvcC",
            ..Video::default()
        };
        assert_eq!(read(&hevc.build()).unwrap().codec, Codec::Hevc);

        for (video, reason) in [
            (
                Video {
                    sample_entry: b"apcn", // ProRes
                    ..Video::default()
                },
                Reason::Codec,
            ),
            (
                Video {
                    size: (8192, 4320),
                    ..Video::default()
                },
                Reason::PictureTooLarge,
            ),
            (
                Video {
                    keyframes: Some(vec![]),
                    ..Video::default()
                },
                Reason::NoKeyframe,
            ),
            (
                Video {
                    video_track: false,
                    ..Video::default()
                },
                Reason::NoVideoTrack,
            ),
        ] {
            assert_eq!(reason_of(&video.build()), reason);
        }
        assert_eq!(reason_of(&boxed(b"ftyp", b"M4A ")), Reason::NoIndex);
    }

    /// A damaged file is an error, never a panic: any cut, any changed byte.
    /// Nor is it ever taken for a storage failure, as reading from memory
    /// can't fail.
    #[test]
    fn damaged_files_are_errors() {
        let file = Video {
            moov_first: true,
            ..Video::default()
        }
        .build();
        let not_unreadable = |file: &[u8]| {
            let result = read(file);
            assert!(!matches!(result, Err(Error::Io(_))), "{result:?}");
        };
        for len in 0..file.len() {
            not_unreadable(&file[..len]);
        }
        let mut state = 0x9e37_79b9_u32;
        for _ in 0..20_000 {
            let mut damaged = file.clone();
            for _ in 0..4 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                damaged[state as usize % file.len()] = (state >> 24) as u8;
            }
            not_unreadable(&damaged);
        }
    }

    /// A box with a 64-bit size whose size is cut off is damaged, not a
    /// storage failure.
    #[test]
    fn a_cut_off_64_bit_box_is_damaged() {
        assert_eq!(reason_of(b"\0\0\0\x01moov\0\0\0\0"), Reason::Damaged);
    }

    /// An index too large to be real isn't read into memory.
    #[test]
    fn refuses_an_oversized_index() {
        let mut file = boxed(b"ftyp", b"isom");
        let moov_len = (MAX_INDEX + 9) as u32;
        file.extend(moov_len.to_be_bytes());
        file.extend(b"moov");
        file.resize(file.len() + moov_len as usize - 8, 0);
        assert_eq!(reason_of(&file), Reason::IndexTooLarge);
    }

    /// Only the index and the keyframe are read, however large the file.
    #[test]
    fn reads_only_the_index_and_the_keyframe() {
        struct Counting<R>(R, u64);
        impl<R: Read> Read for Counting<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(buf)?;
                self.1 += n as u64;
                Ok(n)
            }
        }
        impl<R: Seek> Seek for Counting<R> {
            fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
                self.0.seek(to)
            }
        }
        let mut samples = Video::default().samples;
        samples.push(vec![7; 16 << 20]); // 16 MB of media data after the keyframe
        let video = Video {
            samples,
            chunks: vec![2, 2],
            ..Video::default()
        };
        let file = video.build();
        let mut counting = Counting(Cursor::new(&file), 0);
        first_keyframe(&mut counting, file.len() as u64).unwrap();
        assert!(counting.1 < 4096, "read {} bytes", counting.1);
    }

    /// A video track to build an MP4 file from, in the test.
    #[derive(Clone)]
    struct Video {
        sample_entry: &'static [u8; 4],
        config_box: &'static [u8; 4],
        size: (u16, u16),
        /// `a b c d` of the track header's matrix.
        matrix: [i32; 4],
        samples: Vec<Vec<u8>>,
        /// Samples per chunk, in order.
        chunks: Vec<u32>,
        /// The sync sample table, 1-based; `None` leaves it out.
        keyframes: Option<Vec<u32>>,
        co64: bool,
        moov_first: bool,
        audio_track_first: bool,
        /// `false` leaves only the audio track.
        video_track: bool,
    }

    impl Default for Video {
        fn default() -> Self {
            Self {
                sample_entry: b"avc1",
                config_box: b"avcC",
                size: (64, 48),
                matrix: IDENTITY,
                samples: vec![b"frame one".to_vec(), b"frame 2".to_vec(), b"3".to_vec()],
                chunks: vec![2, 1],
                keyframes: Some(vec![1]),
                co64: false,
                moov_first: false,
                audio_track_first: false,
                video_track: true,
            }
        }
    }

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A full box: version 0 and no flags, then `body`.
    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        boxed(kind, &[&[0u8; 4][..], body].concat())
    }

    fn u32s(values: impl IntoIterator<Item = u32>) -> Vec<u8> {
        values.into_iter().flat_map(u32::to_be_bytes).collect()
    }

    fn handler(kind: &[u8; 4]) -> Vec<u8> {
        full(b"hdlr", &[&[0u8; 4][..], kind, &[0u8; 13]].concat())
    }

    impl Video {
        fn track_header(&self) -> Vec<u8> {
            let [a, b, c, d] = self.matrix;
            let mut body = vec![0u8; 36];
            let matrix = u32s([a, b, 0, c, d, 0, 0, 0, 0x4000_0000].map(|v| v as u32));
            body.extend(matrix);
            body.extend(u32s([
                u32::from(self.size.0) << 16,
                u32::from(self.size.1) << 16,
            ]));
            full(b"tkhd", &body)
        }

        fn sample_table(&self, mdat_body_start: u64) -> Vec<u8> {
            let mut entry = vec![0u8; 78];
            entry[24..26].copy_from_slice(&self.size.0.to_be_bytes());
            entry[26..28].copy_from_slice(&self.size.1.to_be_bytes());
            entry.extend(boxed(self.config_box, b"\x01avcC body"));
            let stsd = full(
                b"stsd",
                &[u32s([1]), boxed(self.sample_entry, &entry)].concat(),
            );
            let stsc = full(
                b"stsc",
                &[
                    u32s([self.chunks.len() as u32]),
                    u32s(
                        (1..)
                            .zip(&self.chunks)
                            .flat_map(|(chunk, &samples)| [chunk, samples, 1]),
                    ),
                ]
                .concat(),
            );
            let sizes = self.samples.iter().map(|s| s.len() as u32);
            let stsz = full(
                b"stsz",
                &[u32s([0, self.samples.len() as u32]), u32s(sizes)].concat(),
            );
            let mut offsets = Vec::new();
            let (mut offset, mut sample) = (mdat_body_start, 0);
            for &count in &self.chunks {
                offsets.push(offset);
                for _ in 0..count {
                    offset += self.samples[sample].len() as u64;
                    sample += 1;
                }
            }
            let chunk_offsets = if self.co64 {
                let wide: Vec<u8> = offsets.iter().flat_map(|o| o.to_be_bytes()).collect();
                full(b"co64", &[u32s([offsets.len() as u32]), wide].concat())
            } else {
                let narrow = u32s(offsets.iter().map(|&o| o as u32));
                full(b"stco", &[u32s([offsets.len() as u32]), narrow].concat())
            };
            let mut stbl = [stsd, stsc, stsz, chunk_offsets].concat();
            if let Some(keyframes) = &self.keyframes {
                let table = [u32s([keyframes.len() as u32]), u32s(keyframes.clone())].concat();
                stbl.extend(full(b"stss", &table));
            }
            boxed(b"stbl", &stbl)
        }

        fn movie(&self, mdat_body_start: u64) -> Vec<u8> {
            let minf = boxed(b"minf", &self.sample_table(mdat_body_start));
            let mdia = boxed(b"mdia", &[handler(b"vide"), minf].concat());
            let video = boxed(b"trak", &[self.track_header(), mdia].concat());
            let audio = boxed(
                b"trak",
                &[full(b"tkhd", &[0u8; 80]), boxed(b"mdia", &handler(b"soun"))].concat(),
            );
            let tracks = match (self.video_track, self.audio_track_first) {
                (false, _) => audio,
                (true, true) => [audio, video].concat(),
                (true, false) => video,
            };
            boxed(b"moov", &[full(b"mvhd", &[0u8; 96]), tracks].concat())
        }

        fn build(&self) -> Vec<u8> {
            let ftyp = boxed(b"ftyp", b"isom\0\0\x02\0isomavc1");
            let mdat = boxed(b"mdat", &self.samples.concat());
            if self.moov_first {
                // The index's size doesn't depend on the offsets in it.
                let moov_len = self.movie(0).len() as u64;
                let start = (ftyp.len() as u64) + moov_len + 8;
                [ftyp, self.movie(start), mdat].concat()
            } else {
                let start = ftyp.len() as u64 + 8;
                [ftyp, mdat, self.movie(start)].concat()
            }
        }
    }
}
