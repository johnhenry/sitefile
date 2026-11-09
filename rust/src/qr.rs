//! A QR Code encoder (port of `src/qr.ts`, issue #224): byte mode, error
//! correction level M, the smallest version that fits, and the mask with the
//! lowest penalty. Both produce the same modules (test/fixtures/qr/*.txt).

const ECC_PER_BLOCK: [i32; 41] = [
    -1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28,
    28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28,
];
const NUM_BLOCKS: [i32; 41] = [
    -1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23,
    25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49,
];
const ECL_BITS: u32 = 0;

fn raw_data_modules(ver: i32) -> i32 {
    let mut result = (16 * ver + 128) * ver + 64;
    if ver >= 2 {
        let num_align = ver / 7 + 2;
        result -= (25 * num_align - 10) * num_align - 55;
        if ver >= 7 {
            result -= 36;
        }
    }
    result
}

fn data_codewords(ver: i32) -> i32 {
    raw_data_modules(ver) / 8 - ECC_PER_BLOCK[ver as usize] * NUM_BLOCKS[ver as usize]
}

fn gf_mul(x: u32, y: u32) -> u32 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11d);
        z ^= ((y >> i) & 1) * x;
    }
    z & 0xff
}

fn rs_divisor(degree: usize) -> Vec<u32> {
    let mut result = vec![0u32; degree];
    result[degree - 1] = 1;
    let mut root: u32 = 1;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    result
}

fn rs_remainder(data: &[u32], divisor: &[u32]) -> Vec<u32> {
    let mut result = vec![0u32; divisor.len()];
    for &b in data {
        let factor = b ^ result.remove(0);
        result.push(0);
        for (i, &coef) in divisor.iter().enumerate() {
            result[i] ^= gf_mul(coef, factor);
        }
    }
    result
}

/// A QR Code: `size` × `size` modules, `true` = dark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCode {
    pub version: i32,
    pub size: usize,
    pub modules: Vec<Vec<bool>>,
}

#[derive(Clone)]
struct Builder {
    version: i32,
    size: usize,
    modules: Vec<Vec<bool>>,
    is_function: Vec<Vec<bool>>,
}

impl Builder {
    fn new(version: i32) -> Builder {
        let size = (version * 4 + 17) as usize;
        Builder {
            version,
            size,
            modules: vec![vec![false; size]; size],
            is_function: vec![vec![false; size]; size],
        }
    }

    fn set(&mut self, x: i32, y: i32, dark: bool) {
        self.modules[y as usize][x as usize] = dark;
        self.is_function[y as usize][x as usize] = true;
    }

    fn alignment_positions(&self) -> Vec<i32> {
        if self.version == 1 {
            return Vec::new();
        }
        let num_align = self.version / 7 + 2;
        let step = if self.version == 32 {
            26
        } else {
            let num = self.version * 4 + 4;
            let den = num_align * 2 - 2;
            ((num + den - 1) / den) * 2
        };
        let mut result = vec![6];
        let mut pos = self.size as i32 - 7;
        while (result.len() as i32) < num_align {
            result.insert(1, pos);
            pos -= step;
        }
        result
    }

    fn draw_function_patterns(&mut self) {
        let n = self.size as i32;
        for i in 0..n {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        self.finder(3, 3);
        self.finder(n - 4, 3);
        self.finder(3, n - 4);
        let align = self.alignment_positions();
        let k = align.len();
        for i in 0..k {
            for j in 0..k {
                if !((i == 0 && j == 0) || (i == 0 && j == k - 1) || (i == k - 1 && j == 0)) {
                    self.alignment(align[i], align[j]);
                }
            }
        }
        self.format_bits(0);
        self.version_bits();
    }

    fn finder(&mut self, x: i32, y: i32) {
        let n = self.size as i32;
        for dy in -4..=4i32 {
            for dx in -4..=4i32 {
                let dist = dx.abs().max(dy.abs());
                let (xx, yy) = (x + dx, y + dy);
                if xx >= 0 && xx < n && yy >= 0 && yy < n {
                    self.set(xx, yy, dist != 2 && dist != 4);
                }
            }
        }
    }

    fn alignment(&mut self, x: i32, y: i32) {
        for dy in -2..=2i32 {
            for dx in -2..=2i32 {
                self.set(x + dx, y + dy, dx.abs().max(dy.abs()) != 1);
            }
        }
    }

    fn format_bits(&mut self, mask: u32) {
        let data = (ECL_BITS << 3) | mask;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = ((data << 10) | rem) ^ 0x5412;
        let bit = |i: u32| (bits >> i) & 1 != 0;
        let n = self.size as i32;
        for i in 0..=5 {
            self.set(8, i, bit(i as u32));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i, 8, bit(i as u32));
        }
        for i in 0..8 {
            self.set(n - 1 - i, 8, bit(i as u32));
        }
        for i in 8..15 {
            self.set(8, n - 15 + i, bit(i as u32));
        }
        self.set(8, n - 8, true);
    }

    fn version_bits(&mut self) {
        if self.version < 7 {
            return;
        }
        let mut rem = self.version as u32;
        for _ in 0..12 {
            rem = (rem << 1) ^ ((rem >> 11) * 0x1f25);
        }
        let bits = ((self.version as u32) << 12) | rem;
        let n = self.size as i32;
        for i in 0..18 {
            let dark = (bits >> i) & 1 != 0;
            let a = n - 11 + (i % 3);
            let b = i / 3;
            self.set(a, b, dark);
            self.set(b, a, dark);
        }
    }

    fn draw_codewords(&mut self, data: &[u32]) {
        let n = self.size as i32;
        let mut i: usize = 0;
        let mut right = n - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..n {
                for j in 0..2 {
                    let x = right - j;
                    let upward = ((right + 1) & 2) == 0;
                    let y = if upward { n - 1 - vert } else { vert };
                    if !self.is_function[y as usize][x as usize] && i < data.len() * 8 {
                        self.modules[y as usize][x as usize] =
                            (data[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u32) {
        for y in 0..self.size {
            for x in 0..self.size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => (x * y) % 2 + (x * y) % 3 == 0,
                    6 => ((x * y) % 2 + (x * y) % 3) % 2 == 0,
                    _ => ((x + y) % 2 + (x * y) % 3) % 2 == 0,
                };
                if !self.is_function[y][x] && invert {
                    self.modules[y][x] = !self.modules[y][x];
                }
            }
        }
    }

    fn penalty(&self) -> i64 {
        let n = self.size;
        let at = |x: usize, y: usize| self.modules[y][x];
        let mut result: i64 = 0;
        for horizontal in [true, false] {
            for a in 0..n {
                let mut run_color = false;
                let mut run_len: i64 = 0;
                let mut history = [0i64; 7];
                for b in 0..n {
                    let c = if horizontal { at(b, a) } else { at(a, b) };
                    if c == run_color {
                        run_len += 1;
                        if run_len == 5 {
                            result += 3;
                        } else if run_len > 5 {
                            result += 1;
                        }
                    } else {
                        add_history(run_len, &mut history, n as i64);
                        if !run_color {
                            result += count_patterns(&history) * 40;
                        }
                        run_color = c;
                        run_len = 1;
                    }
                }
                result += terminate_and_count(run_color, run_len, &mut history, n as i64) * 40;
            }
        }
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = at(x, y);
                if c == at(x + 1, y) && c == at(x, y + 1) && c == at(x + 1, y + 1) {
                    result += 3;
                }
            }
        }
        let dark = self.modules.iter().flatten().filter(|c| **c).count() as i64;
        let total = (n * n) as i64;
        let diff = (dark * 20 - total * 10).abs();
        let k = (diff + total - 1) / total - 1;
        result += k * 10;
        result
    }
}

fn count_patterns(h: &[i64; 7]) -> i64 {
    let n = h[1];
    let core = n > 0 && h[2] == n && h[3] == n * 3 && h[4] == n && h[5] == n;
    i64::from(core && h[0] >= n * 4 && h[6] >= n) + i64::from(core && h[6] >= n * 4 && h[0] >= n)
}

fn terminate_and_count(run_color: bool, run_len: i64, history: &mut [i64; 7], size: i64) -> i64 {
    let mut len = run_len;
    if run_color {
        add_history(len, history, size);
        len = 0;
    }
    len += size;
    add_history(len, history, size);
    count_patterns(history)
}

fn add_history(run_len: i64, history: &mut [i64; 7], size: i64) {
    let mut len = run_len;
    if history[0] == 0 {
        len += size;
    }
    history.copy_within(0..6, 1);
    history[0] = len;
}

/// Encode `text` (UTF-8) as a QR Code, or None when it is too long for version 40.
pub fn encode_qr(text: &str) -> Option<QrCode> {
    let bytes = text.as_bytes();
    let mut version = 1;
    while version <= 40 {
        let count_bits = if version < 10 { 8 } else { 16 };
        if 4 + count_bits + bytes.len() as i32 * 8 <= data_codewords(version) * 8 {
            break;
        }
        version += 1;
    }
    if version > 40 {
        return None;
    }
    let mut bits: Vec<u32> = Vec::new();
    let append = |bits: &mut Vec<u32>, val: u32, len: i32| {
        for i in (0..len).rev() {
            bits.push((val >> i) & 1);
        }
    };
    append(&mut bits, 4, 4);
    append(
        &mut bits,
        bytes.len() as u32,
        if version < 10 { 8 } else { 16 },
    );
    for &b in bytes {
        append(&mut bits, u32::from(b), 8);
    }
    let capacity = (data_codewords(version) * 8) as usize;
    let term = 4.min(capacity - bits.len()) as i32;
    append(&mut bits, 0, term);
    let fill = ((8 - bits.len() % 8) % 8) as i32;
    append(&mut bits, 0, fill);
    let mut pad: u32 = 0xec;
    while bits.len() < capacity {
        append(&mut bits, pad, 8);
        pad ^= 0xec ^ 0x11;
    }
    let data: Vec<u32> = bits
        .chunks(8)
        .map(|c| c.iter().fold(0, |a, b| (a << 1) | b))
        .collect();
    let num_blocks = NUM_BLOCKS[version as usize] as usize;
    let block_ecc = ECC_PER_BLOCK[version as usize] as usize;
    let raw_codewords = (raw_data_modules(version) / 8) as usize;
    let num_short = num_blocks - raw_codewords % num_blocks;
    let short_len = raw_codewords / num_blocks;
    let divisor = rs_divisor(block_ecc);
    let mut blocks: Vec<Vec<u32>> = Vec::new();
    let mut k = 0;
    for i in 0..num_blocks {
        let len = short_len - block_ecc + usize::from(i >= num_short);
        let mut dat: Vec<u32> = data[k..k + len].to_vec();
        k += len;
        let ecc = rs_remainder(&dat, &divisor);
        if i < num_short {
            dat.push(0);
        }
        dat.extend(ecc);
        blocks.push(dat);
    }
    let mut codewords: Vec<u32> = Vec::new();
    for i in 0..blocks[0].len() {
        for (j, block) in blocks.iter().enumerate() {
            if i != short_len - block_ecc || j >= num_short {
                codewords.push(block[i]);
            }
        }
    }
    let mut best: Option<Builder> = None;
    let mut best_penalty = i64::MAX;
    for mask in 0..8 {
        let mut b = Builder::new(version);
        b.draw_function_patterns();
        b.draw_codewords(&codewords);
        b.apply_mask(mask);
        b.format_bits(mask);
        let p = b.penalty();
        if p < best_penalty {
            best_penalty = p;
            best = Some(b);
        }
    }
    let b = best.expect("eight masks were tried");
    Some(QrCode {
        version,
        size: b.size,
        modules: b.modules,
    })
}

/// The code for a terminal: two rows of modules per line with half blocks and a quiet zone of two
/// modules (the same text as the TypeScript `qrTerminal`).
pub fn qr_terminal(code: &QrCode) -> String {
    let quiet = 2usize;
    let n = code.size + quiet * 2;
    let dark = |x: usize, y: usize| -> bool {
        x >= quiet
            && y >= quiet
            && x - quiet < code.size
            && y - quiet < code.size
            && code.modules[y - quiet][x - quiet]
    };
    let mut out = String::new();
    let mut y = 0;
    while y < n {
        for x in 0..n {
            let top = !dark(x, y);
            let bottom = if y + 1 < n { !dark(x, y + 1) } else { false };
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The goldens TypeScript checks too.
    #[test]
    fn matches_the_shared_fixtures() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test/fixtures/qr");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("txt") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let (first, rest) = text.split_once('\n').unwrap();
            let input = first.strip_prefix("# ").unwrap();
            assert_eq!(
                qr_terminal(&encode_qr(input).unwrap()),
                rest,
                "{}",
                p.display()
            );
            n += 1;
        }
        assert!(n >= 3);
    }

    #[test]
    fn format_and_version_bits_match_the_standard() {
        // ISO/IEC 18004 Table C.1 (level M) and Table D.1 (version 7).
        let want = [
            "101010000010010",
            "101000100100101",
            "101111001111100",
            "101101101001011",
            "100010111111001",
            "100000011001110",
            "100111110010111",
            "100101010100000",
        ];
        for (mask, w) in want.iter().enumerate() {
            let data = (ECL_BITS << 3) | mask as u32;
            let mut rem = data;
            for _ in 0..10 {
                rem = (rem << 1) ^ ((rem >> 9) * 0x537);
            }
            assert_eq!(format!("{:015b}", ((data << 10) | rem) ^ 0x5412), *w);
        }
        let mut rem = 7u32;
        for _ in 0..12 {
            rem = (rem << 1) ^ ((rem >> 11) * 0x1f25);
        }
        assert_eq!(format!("{:018b}", (7 << 12) | rem), "000111110010010100");
    }
}
