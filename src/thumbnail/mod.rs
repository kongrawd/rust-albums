//! Thumbnails: small JPEGs shown in album grids instead of loading originals.
//! Videos are decoded in a child process (see [`child`]), so a bug in a
//! decoder aborts that process, never the server.

// Wired into the server by the thumbnail route; until then only tests use it.
#![allow(dead_code)]

#[cfg(unix)]
pub mod child;
mod jpeg;
pub mod mp4;
pub mod picture;
pub mod video;

use std::{fmt, io};

/// The largest picture a thumbnail is made from, 4096 × 2304 (above 4K UHD
/// and DCI 4K). It bounds the memory decoding takes: about 100 MB for 4K
/// 10-bit HEVC.
pub const MAX_PIXELS: u32 = 4096 * 2304;

/// Why a file has no thumbnail.
#[derive(Debug)]
pub enum Error {
    /// Reading failed, so the storage may be at fault rather than the file.
    Io(io::Error),
    /// The file holds nothing a thumbnail can be made of.
    Unsupported(Reason),
}

/// What makes a file unsupported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// The file is damaged or not what its name says.
    Damaged,
    /// An MP4 or QuickTime file without its index (the `moov` box).
    NoIndex,
    /// An index too large to be real.
    IndexTooLarge,
    NoVideoTrack,
    /// Video in a codec other than H.264 or HEVC.
    Codec,
    /// A picture above [`MAX_PIXELS`].
    PictureTooLarge,
    NoKeyframe,
    KeyframeTooLarge,
    /// Chroma other than 4:2:0.
    Chroma,
    /// Chroma stored at another bit depth than luma.
    BitDepths,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Damaged => "damaged file",
            Self::NoIndex => "no index (moov box)",
            Self::IndexTooLarge => "index too large",
            Self::NoVideoTrack => "no video track",
            Self::Codec => "not H.264 or HEVC",
            Self::PictureTooLarge => "picture too large",
            Self::NoKeyframe => "no keyframe",
            Self::KeyframeTooLarge => "keyframe too large",
            Self::Chroma => "only 4:2:0 video is supported",
            Self::BitDepths => "chroma and luma bit depths differ",
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => err.fmt(f),
            Self::Unsupported(reason) => reason.fmt(f),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<Reason> for Error {
    fn from(reason: Reason) -> Self {
        Self::Unsupported(reason)
    }
}

/// How a stored picture is turned to show it upright: the plain turns that
/// video tracks use (photos' mirrored EXIF orientations aren't needed yet).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orientation {
    Upright,
    /// Turned a quarter clockwise to show it upright.
    TurnedRight,
    UpsideDown,
    /// Turned a quarter anticlockwise to show it upright.
    TurnedLeft,
}

/// Helpers shared by the tests of every step.
#[cfg(test)]
mod testing {
    /// Decodes a thumbnail: its width, height and RGB pixels. It must be a
    /// baseline JPEG in colour.
    pub fn decode_jpeg(jpeg: &[u8]) -> (usize, usize, Vec<u8>) {
        let mut decoder = jpeg_decoder::Decoder::new(jpeg);
        let pixels = decoder.decode().unwrap();
        let info = decoder.info().unwrap();
        assert_eq!(info.pixel_format, jpeg_decoder::PixelFormat::RGB24);
        assert_eq!(
            info.coding_process,
            jpeg_decoder::CodingProcess::DctSequential
        );
        (usize::from(info.width), usize::from(info.height), pixels)
    }
}
