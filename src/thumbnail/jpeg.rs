//! A baseline JPEG encoder (ITU-T T.81), enough for thumbnails: 8-bit RGB in,
//! JFIF out, with 4:2:0 chroma and the standard quantisation and Huffman
//! tables of Annex K, scaled for a quality as libjpeg does. It is our own
//! because only trusted libraries go into the binary, and the encoders on
//! offer bring single-maintainer crates with them; at the same quality it
//! matches libjpeg in size and error.

/// Annex K.1: luminance quantisation at quality 50, in natural order.
const LUMA_QUANTISATION: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
/// Annex K.2: chrominance quantisation at quality 50, in natural order.
const CHROMA_QUANTISATION: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
/// The natural position of each coefficient in zig-zag order (Figure A.6).
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const LUMA_DC: HuffmanSpec = HuffmanSpec {
    counts: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};
const CHROMA_DC: HuffmanSpec = HuffmanSpec {
    counts: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
    symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};
const LUMA_AC: HuffmanSpec = HuffmanSpec {
    counts: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
    symbols: &[
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};
const CHROMA_AC: HuffmanSpec = HuffmanSpec {
    counts: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
    symbols: &[
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};

/// A Huffman table as stored in the file (Annex K.3): the number of codes of
/// each length from 1 to 16 bits, then the symbols in code order.
struct HuffmanSpec {
    counts: [u8; 16],
    symbols: &'static [u8],
}

/// Each symbol's code and code length (Annex C), indexed by symbol.
struct Huffman {
    codes: [(u16, u8); 256],
}

impl Huffman {
    fn new(spec: &HuffmanSpec) -> Self {
        let mut codes = [(0, 0); 256];
        let (mut code, mut symbols) = (0u16, spec.symbols.iter());
        for (length, &count) in (1..=16u8).zip(&spec.counts) {
            for &symbol in symbols.by_ref().take(usize::from(count)) {
                codes[usize::from(symbol)] = (code, length);
                code += 1;
            }
            code <<= 1;
        }
        Self { codes }
    }
}

/// Entropy-coded data, with a 0x00 after each 0xFF byte (§B.1.1.5).
struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    count: u8,
}

impl BitWriter {
    fn write(&mut self, value: u16, length: u8) {
        self.bits = (self.bits << length) | u32::from(value & ((1u32 << length) - 1) as u16);
        self.count += length;
        while self.count >= 8 {
            self.count -= 8;
            let byte = (self.bits >> self.count) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
        }
    }

    /// Pads the last byte with 1 bits.
    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            let pad = 8 - self.count;
            self.write((1 << pad) - 1, pad);
        }
        self.out
    }
}

/// One colour component's tables and DC prediction while encoding.
struct Component {
    quantisation: [u16; 64],
    dc: Huffman,
    ac: Huffman,
    previous_dc: i32,
}

impl Component {
    fn encode(&mut self, block: &[f32; 64], out: &mut BitWriter) {
        let coefficients = forward_dct(block);
        let mut quantised = [0i32; 64];
        for (k, &natural) in ZIGZAG.iter().enumerate() {
            quantised[k] =
                (coefficients[natural] / f32::from(self.quantisation[natural])).round() as i32;
        }
        let dc = quantised[0] - self.previous_dc;
        self.previous_dc = quantised[0];
        let (size, bits) = magnitude(dc);
        let (code, length) = self.dc.codes[usize::from(size)];
        out.write(code, length);
        out.write(bits, size);

        let mut zeros = 0;
        for &coefficient in &quantised[1..] {
            if coefficient == 0 {
                zeros += 1;
                continue;
            }
            while zeros > 15 {
                let (code, length) = self.ac.codes[0xF0]; // 16 zeros
                out.write(code, length);
                zeros -= 16;
            }
            let (size, bits) = magnitude(coefficient);
            let (code, length) = self.ac.codes[usize::from((zeros << 4) | size)];
            out.write(code, length);
            out.write(bits, size);
            zeros = 0;
        }
        if zeros > 0 {
            let (code, length) = self.ac.codes[0x00]; // end of block
            out.write(code, length);
        }
    }
}

/// A coefficient's size category and its bits (§F.1.2.1): negative values are
/// stored as their one's complement.
fn magnitude(value: i32) -> (u8, u16) {
    let size = (32 - value.unsigned_abs().leading_zeros()) as u8;
    let bits = if value < 0 { value - 1 } else { value };
    (size, bits as u16)
}

/// The 8 × 8 forward DCT of level-shifted samples (§A.3.3), done as rows then
/// columns.
fn forward_dct(block: &[f32; 64]) -> [f32; 64] {
    let mut basis = [[0f32; 8]; 8];
    for (u, row) in basis.iter_mut().enumerate() {
        let scale = if u == 0 {
            std::f32::consts::FRAC_1_SQRT_2
        } else {
            1.0
        } / 2.0;
        for (x, value) in row.iter_mut().enumerate() {
            *value = scale * ((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI / 16.0).cos();
        }
    }
    let mut rows = [0f32; 64];
    for y in 0..8 {
        for u in 0..8 {
            rows[y * 8 + u] = (0..8).map(|x| basis[u][x] * block[y * 8 + x]).sum();
        }
    }
    let mut out = [0f32; 64];
    for v in 0..8 {
        for u in 0..8 {
            out[v * 8 + u] = (0..8).map(|y| basis[v][y] * rows[y * 8 + u]).sum();
        }
    }
    out
}

/// The quantisation table for `quality` (1–100), scaled as libjpeg does.
fn scaled(table: &[u16; 64], quality: u8) -> [u16; 64] {
    let quality = u32::from(quality.clamp(1, 100));
    let scale = if quality < 50 {
        5000 / quality
    } else {
        200 - 2 * quality
    };
    table.map(|value| ((u32::from(value) * scale + 50) / 100).clamp(1, 255) as u16)
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend([0xFF, marker]);
    out.extend(((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// An RGB picture, 3 bytes a pixel, row by row. Built from a function of
/// each pixel's position, so its size always matches its sides, which fit a
/// JPEG's 16 bits.
pub struct Rgb {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

impl Rgb {
    pub fn from_fn(
        width: u16,
        height: u16,
        mut pixel: impl FnMut(usize, usize) -> [u8; 3],
    ) -> Self {
        let (w, h) = (usize::from(width), usize::from(height));
        let mut pixels = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                pixels.extend(pixel(x, y));
            }
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// The pixel at `(x, y)`, which must be inside the picture.
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let at = (y * usize::from(self.width) + x) * 3;
        [self.pixels[at], self.pixels[at + 1], self.pixels[at + 2]]
    }
}

/// `rgb` as a JPEG of `quality` (1–100).
pub fn encode(rgb: &Rgb, quality: u8) -> Vec<u8> {
    let (width, height) = (usize::from(rgb.width), usize::from(rgb.height));
    let mut luma = Component {
        quantisation: scaled(&LUMA_QUANTISATION, quality),
        dc: Huffman::new(&LUMA_DC),
        ac: Huffman::new(&LUMA_AC),
        previous_dc: 0,
    };
    let chroma_component = || Component {
        quantisation: scaled(&CHROMA_QUANTISATION, quality),
        dc: Huffman::new(&CHROMA_DC),
        ac: Huffman::new(&CHROMA_AC),
        previous_dc: 0,
    };
    let (mut cb, mut cr) = (chroma_component(), chroma_component());

    let mut out = vec![0xFF, 0xD8];
    // JFIF 1.01, no density unit (1:1 pixels), no embedded thumbnail.
    segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    let mut tables = Vec::new();
    for (id, table) in [(0u8, &luma.quantisation), (1, &cb.quantisation)] {
        tables.push(id);
        tables.extend(ZIGZAG.iter().map(|&natural| table[natural] as u8));
    }
    segment(&mut out, 0xDB, &tables);
    let (w, h) = (rgb.width.to_be_bytes(), rgb.height.to_be_bytes());
    // 8-bit, 3 components: Y sampled 2 × 2 with table 0, Cb and Cr 1 × 1 with table 1.
    segment(
        &mut out,
        0xC0,
        &[
            8, h[0], h[1], w[0], w[1], 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1,
        ],
    );
    let mut huffman = Vec::new();
    for (class_and_id, spec) in [
        (0x00, &LUMA_DC),
        (0x10, &LUMA_AC),
        (0x01, &CHROMA_DC),
        (0x11, &CHROMA_AC),
    ] {
        huffman.push(class_and_id);
        huffman.extend(spec.counts);
        huffman.extend_from_slice(spec.symbols);
    }
    segment(&mut out, 0xC4, &huffman);
    segment(&mut out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);

    // JFIF's YCbCr (full-range BT.601), level-shifted, edges repeated.
    let pixel = |x: usize, y: usize| -> [f32; 3] {
        let [r, g, b] = rgb
            .pixel(x.min(width - 1), y.min(height - 1))
            .map(f32::from);
        [
            0.299 * r + 0.587 * g + 0.114 * b - 128.0,
            -0.168_736 * r - 0.331_264 * g + 0.5 * b,
            0.5 * r - 0.418_688 * g - 0.081_312 * b,
        ]
    };
    let mut data = BitWriter {
        out,
        bits: 0,
        count: 0,
    };
    for mcu_y in (0..height).step_by(16) {
        for mcu_x in (0..width).step_by(16) {
            for (dx, dy) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                let mut block = [0f32; 64];
                for (i, value) in block.iter_mut().enumerate() {
                    *value = pixel(mcu_x + dx + i % 8, mcu_y + dy + i / 8)[0];
                }
                luma.encode(&block, &mut data);
            }
            for (channel, component) in [(1, &mut cb), (2, &mut cr)] {
                let mut block = [0f32; 64];
                for (i, value) in block.iter_mut().enumerate() {
                    let (x, y) = (mcu_x + 2 * (i % 8), mcu_y + 2 * (i / 8));
                    let sum: f32 = [(0, 0), (1, 0), (0, 1), (1, 1)]
                        .iter()
                        .map(|&(sx, sy)| pixel(x + sx, y + sy)[channel])
                        .sum();
                    *value = sum / 4.0;
                }
                component.encode(&block, &mut data);
            }
        }
    }
    let mut out = data.finish();
    out.extend([0xFF, 0xD9]);
    out
}
