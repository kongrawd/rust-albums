//! Thumbnail pictures: a decoded picture shrunk to thumbnail size, turned
//! upright, converted to RGB and encoded as a JPEG (see `jpeg`). Shared by
//! every format a thumbnail is made from: video decoders produce [`Yuv420`].

use super::Orientation;
use super::jpeg::{self, Rgb};

/// Thumbnails are this many pixels on their short side, or smaller pictures
/// keep their size. Grid tiles are square crops, so the short side is what a
/// tile shows: 400 covers a three-column phone grid at 3× pixel density.
const SHORT_SIDE: usize = 400;
/// JPEG quality: thumbnails of 15–25 KB, without visible artefacts at tile size.
const QUALITY: u8 = 80;

/// A decoded video picture: 8 bits per sample, 4:2:0 chroma and BT.709 colours
/// in limited range, as phones and cameras record. The planes go row by row
/// without padding; the chroma planes are half the size, rounded up.
pub struct Yuv420 {
    width: u16,
    height: u16,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl Yuv420 {
    /// The picture, if its sides are 1 to 65,535 and the planes fit them.
    pub fn new(width: usize, height: usize, y: Vec<u8>, cb: Vec<u8>, cr: Vec<u8>) -> Option<Self> {
        let (width16, height16) = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        let fits = width > 0 && height > 0 && y.len() == width * height;
        (fits && cb.len() == chroma && cr.len() == chroma).then_some(Self {
            width: width16,
            height: height16,
            y,
            cb,
            cr,
        })
    }
}

impl Orientation {
    fn swaps_sides(self) -> bool {
        matches!(self, Self::TurnedRight | Self::TurnedLeft)
    }

    /// The stored pixel shown at `(x, y)` of the upright picture, for a
    /// stored picture `width` × `height`.
    fn source(self, x: usize, y: usize, width: usize, height: usize) -> (usize, usize) {
        match self {
            Self::Upright => (x, y),
            Self::TurnedRight => (y, height - 1 - x),
            Self::UpsideDown => (width - 1 - x, height - 1 - y),
            Self::TurnedLeft => (width - 1 - y, x),
        }
    }
}

/// The thumbnail of `picture`, shown in `orientation`, as a JPEG.
pub fn thumbnail(picture: &Yuv420, orientation: Orientation) -> Vec<u8> {
    let small = shrink(picture);
    let (width, height) = (small.width(), small.height());
    let (shown_width, shown_height) = match orientation.swaps_sides() {
        true => (height, width),
        false => (width, height),
    };
    let upright = Rgb::from_fn(shown_width, shown_height, |x, y| {
        let (sx, sy) = orientation.source(x, y, usize::from(width), usize::from(height));
        small.pixel(sx, sy)
    });
    jpeg::encode(&upright, QUALITY)
}

/// `width` × `height` scaled to [`SHORT_SIDE`] on its short side, if larger.
fn thumbnail_size(width: u16, height: u16) -> (u16, u16) {
    let short = usize::from(width.min(height));
    if short <= SHORT_SIDE {
        return (width, height);
    }
    // Never larger than the side it scales down, so it still fits 16 bits.
    let scaled = |side: u16| {
        let side = usize::from(side);
        ((side * SHORT_SIDE + short / 2) / short).clamp(1, side) as u16
    };
    (scaled(width), scaled(height))
}

/// `picture` at thumbnail size in RGB: each pixel averages the samples it
/// covers (a box filter), which keeps detail when shrinking a lot.
fn shrink(picture: &Yuv420) -> Rgb {
    let (width, height) = thumbnail_size(picture.width, picture.height);
    let (source_width, source_height) = (usize::from(picture.width), usize::from(picture.height));
    let chroma_width = source_width.div_ceil(2);
    let span = |i: usize, out: u16, of: usize| {
        let start = i * of / usize::from(out);
        (start, ((i + 1) * of / usize::from(out)).max(start + 1))
    };
    Rgb::from_fn(width, height, |x, y| {
        let (y0, y1) = span(y, height, source_height);
        let (x0, x1) = span(x, width, source_width);
        let (mut luma, mut count) = (0u32, 0u32);
        for row in y0..y1 {
            let line = &picture.y[row * source_width..][x0..x1];
            luma += line.iter().map(|&s| u32::from(s)).sum::<u32>();
            count += line.len() as u32;
        }
        let (mut cb, mut cr, mut chroma_count) = (0u32, 0u32, 0u32);
        for row in y0 / 2..y1.div_ceil(2) {
            for column in x0 / 2..x1.div_ceil(2) {
                let at = row * chroma_width + column;
                cb += u32::from(picture.cb[at]);
                cr += u32::from(picture.cr[at]);
                chroma_count += 1;
            }
        }
        let average = |sum: u32, n: u32| sum as f32 / n as f32;
        bt709_to_rgb(
            average(luma, count),
            average(cb, chroma_count),
            average(cr, chroma_count),
        )
    })
}

/// Limited-range BT.709 (luma 16–235, chroma 16–240) to full-range RGB.
fn bt709_to_rgb(y: f32, cb: f32, cr: f32) -> [u8; 3] {
    let (y, cb, cr) = ((y - 16.0) * (255.0 / 219.0), cb - 128.0, cr - 128.0);
    let scale = 255.0 / 224.0;
    let channel = |value: f32| value.round().clamp(0.0, 255.0) as u8;
    [
        channel(y + scale * 1.5748 * cr),
        channel(y - scale * (0.1873 * cb + 0.4681 * cr)),
        channel(y + scale * 1.8556 * cb),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::testing::decode_jpeg;
    use super::*;
    use Orientation::*;

    /// BT.709 limited-range values of a few colours, and their RGB.
    const RED: ([u8; 3], [u8; 3]) = ([63, 102, 240], [255, 0, 0]);
    const BLUE: ([u8; 3], [u8; 3]) = ([32, 240, 118], [0, 0, 255]);
    const GREEN: ([u8; 3], [u8; 3]) = ([173, 42, 26], [0, 255, 0]);
    const WHITE: ([u8; 3], [u8; 3]) = ([235, 128, 128], [255, 255, 255]);
    const GREY: ([u8; 3], [u8; 3]) = ([126, 128, 128], [128, 128, 128]);

    /// A picture whose samples come from `colour(x, y)`: luma at each pixel,
    /// chroma at the top-left pixel of each 2 × 2.
    fn picture(width: usize, height: usize, colour: impl Fn(usize, usize) -> [u8; 3]) -> Yuv420 {
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        let plane = |channel: usize, step: usize, w: usize, h: usize| -> Vec<u8> {
            let mut samples = Vec::with_capacity(w * h);
            for y in 0..h {
                for x in 0..w {
                    samples.push(colour(x * step, y * step)[channel]);
                }
            }
            samples
        };
        let (y, cb, cr) = (
            plane(0, 1, width, height),
            plane(1, 2, chroma_width, chroma_height),
            plane(2, 2, chroma_width, chroma_height),
        );
        Yuv420::new(width, height, y, cb, cr).unwrap()
    }

    /// Red and blue on top, green and white below.
    fn quadrants(width: usize, height: usize) -> Yuv420 {
        picture(width, height, |x, y| {
            match (x < width / 2, y < height / 2) {
                (true, true) => RED.0,
                (false, true) => BLUE.0,
                (true, false) => GREEN.0,
                (false, false) => WHITE.0,
            }
        })
    }

    fn solid(width: usize, height: usize, colour: [u8; 3]) -> Yuv420 {
        picture(width, height, |_, _| colour)
    }

    fn near(actual: &[u8], expected: [u8; 3], tolerance: u8) -> bool {
        actual
            .iter()
            .zip(expected)
            .all(|(&a, e)| a.abs_diff(e) <= tolerance)
    }

    /// A picture's sides must fit a JPEG and its planes must fit its sides.
    #[test]
    fn pictures_are_checked_when_made() {
        let plane = |len| vec![0; len];
        assert!(Yuv420::new(2, 2, plane(4), plane(1), plane(1)).is_some());
        assert!(
            Yuv420::new(3, 3, plane(9), plane(4), plane(4)).is_some(),
            "odd sides"
        );
        for (width, height, y, chroma) in [
            (2, 2, 3, 1),                // luma too short
            (2, 2, 4, 2),                // chroma too long
            (0, 2, 0, 0),                // no width
            (65_536, 1, 65_536, 32_768), // too wide for a JPEG
        ] {
            let made = Yuv420::new(width, height, plane(y), plane(chroma), plane(chroma));
            assert!(made.is_none(), "{width}x{height}");
        }
    }

    #[test]
    fn is_a_baseline_jpeg_of_the_same_colours() {
        let jpeg = thumbnail(&solid(40, 24, GREY.0), Upright);
        assert_eq!(jpeg[..4], [0xFF, 0xD8, 0xFF, 0xE0], "starts with JFIF");
        assert_eq!(jpeg[jpeg.len() - 2..], [0xFF, 0xD9]);
        let (width, height, pixels) = decode_jpeg(&jpeg);
        assert_eq!((width, height), (40, 24));
        assert!(
            pixels.chunks(3).all(|p| near(p, GREY.1, 3)),
            "{:?}",
            &pixels[..3]
        );
        for colour in [RED, BLUE, GREEN, WHITE] {
            let (_, _, pixels) = decode_jpeg(&thumbnail(&solid(16, 16, colour.0), Upright));
            assert!(
                near(&pixels[..3], colour.1, 10),
                "{:?} for {:?}",
                &pixels[..3],
                colour.1
            );
        }
    }

    /// Fine detail survives: a grey pattern of gradients and 4-pixel checks
    /// comes back close to its source, pixel by pixel.
    #[test]
    fn keeps_fine_detail() {
        let checks = |x: usize, y: usize| {
            if (x / 4 + y / 4).is_multiple_of(2) {
                40
            } else {
                0
            }
        };
        let luma = |x: usize, y: usize| (30 + x * 2 + y + checks(x, y)) as u8;
        let (width, height) = (64, 48);
        let grey = picture(width, height, |x, y| [luma(x, y), 128, 128]);
        let (_, _, pixels) = decode_jpeg(&thumbnail(&grey, Upright));
        let mut total = 0u32;
        for (i, pixel) in pixels.chunks(3).enumerate() {
            let expected = bt709_to_rgb(f32::from(luma(i % width, i / width)), 128.0, 128.0)[0];
            let error = u32::from(pixel[1].abs_diff(expected));
            assert!(error < 40, "pixel {i}: {pixel:?} for {expected}");
            total += error;
        }
        // libjpeg's own encoder at the same quality averages 3.2 here.
        let mean = total as f32 / (width * height) as f32;
        assert!(mean < 4.0, "mean error {mean}");
    }

    /// Large pictures shrink to 400 px on their short side; small ones keep
    /// their size, odd ones included.
    #[test]
    fn shrinks_to_the_thumbnail_size_but_never_enlarges() {
        for ((width, height), expected) in [
            ((1280, 720), (711, 400)),
            ((720, 1280), (400, 711)),
            ((400, 400), (400, 400)),
            ((96, 64), (96, 64)),
            ((33, 17), (33, 17)),
            ((1, 1), (1, 1)),
        ] {
            let (w, h, _) = decode_jpeg(&thumbnail(&quadrants(width, height), Upright));
            assert_eq!((w, h), expected, "{width}x{height}");
        }
    }

    /// Each turn puts the quadrants where it says: turned right, the left
    /// column (red over green) becomes the top row, green first.
    #[test]
    fn turns_as_each_orientation_says() {
        let [r, b, g, w] = [RED.1, BLUE.1, GREEN.1, WHITE.1];
        for (orientation, expected) in [
            (Upright, [[r, b], [g, w]]),
            (TurnedRight, [[g, r], [w, b]]),
            (UpsideDown, [[w, g], [b, r]]),
            (TurnedLeft, [[b, w], [r, g]]),
        ] {
            let (width, height, pixels) = decode_jpeg(&thumbnail(&quadrants(96, 64), orientation));
            let turned = matches!(orientation, TurnedRight | TurnedLeft);
            assert_eq!((width, height), if turned { (64, 96) } else { (96, 64) });
            for (row, colours) in expected.iter().enumerate() {
                for (column, &colour) in colours.iter().enumerate() {
                    let (x, y) = (
                        width / 4 + column * width / 2,
                        height / 4 + row * height / 2,
                    );
                    let pixel = &pixels[(y * width + x) * 3..][..3];
                    assert!(
                        near(pixel, colour, 24),
                        "{orientation:?} row {row} column {column}: {pixel:?}"
                    );
                }
            }
        }
    }
}
