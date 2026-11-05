//! Deterministic DEFLATE (RFC 1951) and gzip (RFC 1952) encoder, a port of
//! `src/deflate.ts`. Both must produce the same bytes for every input, so
//! zip entries, tar.gz exports and OCI layers (and therefore image IDs) are
//! identical between the two implementations. The full specification is at
//! the top of `src/deflate.ts` (and in docs/DEFLATE.md); any change must be
//! made in both files and the fixtures regenerated with
//! `node rust/tests/fixtures/deflate/generate.mjs`.
//!
//! In short: hash-chain LZ77 over a 32 KiB window (15-bit multiplicative hash
//! of 3 bytes, at most 64 candidates, nearest wins ties, length-3 matches
//! farther than 4096 dropped), one-step-at-a-time lazy matching below length
//! 16 (16 candidates when the current match is >= 8), blocks of 16384
//! symbols written as the smallest of stored / fixed / dynamic (ties in that
//! order), package-merge code lengths, a fixed run-length scheme for the code
//! lengths. gzip: fixed 10-byte header (MTIME 0, XFL 0, OS 3).
//!
//! Decompression still uses flate2: any inflater reads this output.

const WSIZE: usize = 32768;
const WMASK: usize = WSIZE - 1;
const HASH_BITS: u32 = 15;
const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const MAX_CHAIN: u32 = 64;
const LAZY_LIMIT: usize = 16;
const GOOD_LENGTH: usize = 8;
const TOO_FAR: usize = 4096;
const BLOCK_SYMBOLS: usize = 16384;
const STORED_MAX: usize = 65535;

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// match length (3..=258) → length code index 0..28
static LEN_CODE: [u8; MAX_MATCH + 1] = {
    let mut t = [0u8; MAX_MATCH + 1];
    let mut c = 0;
    while c < 29 {
        let end = if c == 28 { 259 } else { LENGTH_BASE[c + 1] as usize };
        let mut l = LENGTH_BASE[c] as usize;
        while l < end {
            t[l] = c as u8;
            l += 1;
        }
        c += 1;
    }
    t
};

/// distance - 1 → distance code, for distances 1..=32768
static DIST_CODE: [u8; WSIZE] = {
    let mut t = [0u8; WSIZE];
    let mut c = 0;
    while c < 30 {
        let end = if c == 29 { WSIZE + 1 } else { DIST_BASE[c + 1] as usize };
        let mut d = DIST_BASE[c] as usize;
        while d < end {
            t[d - 1] = c as u8;
            d += 1;
        }
        c += 1;
    }
    t
};

fn fixed_lit_len() -> [u8; 288] {
    let mut t = [0u8; 288];
    for (i, v) in t.iter_mut().enumerate() {
        *v = if i < 144 {
            8
        } else if i < 256 {
            9
        } else if i < 280 {
            7
        } else {
            8
        };
    }
    t
}

/// Package-merge length-limited Huffman code lengths.
pub fn code_lengths(freq: &[u32], max_bits: usize) -> Vec<u8> {
    let n = freq.len();
    let mut lens = vec![0u8; n];
    let mut used: Vec<usize> = (0..n).filter(|&i| freq[i] > 0).collect();
    if used.len() < 2 {
        let mut i = 0;
        while used.len() < 2 && i < n {
            if !used.contains(&i) {
                used.push(i);
            }
            i += 1;
        }
        for &s in &used {
            lens[s] = 1;
        }
        return lens;
    }
    used.sort_by(|&a, &b| freq[a].cmp(&freq[b]).then(a.cmp(&b)));
    let k = used.len();
    let leaf_w: Vec<u64> = used.iter().map(|&s| freq[s] as u64).collect();
    // Per level: item i is a leaf (index into `used`) or package b (children 2b, 2b+1 of the previous level).
    #[derive(Clone, Copy)]
    enum Item {
        Leaf(usize),
        Pkg(usize),
    }
    let mut levels: Vec<Vec<Item>> = Vec::with_capacity(max_bits);
    let mut prev_w: Vec<u64> = leaf_w.clone();
    levels.push((0..k).map(Item::Leaf).collect());
    for _ in 1..max_bits {
        let pw: Vec<u64> = prev_w.chunks_exact(2).map(|c| c[0] + c[1]).collect();
        let mut w = Vec::with_capacity(k + pw.len());
        let mut items = Vec::with_capacity(k + pw.len());
        let (mut a, mut b) = (0, 0);
        while a < k || b < pw.len() {
            if b >= pw.len() || (a < k && leaf_w[a] <= pw[b]) {
                w.push(leaf_w[a]);
                items.push(Item::Leaf(a));
                a += 1;
            } else {
                w.push(pw[b]);
                items.push(Item::Pkg(b));
                b += 1;
            }
        }
        levels.push(items);
        prev_w = w;
    }
    let mut take = 2 * k - 2;
    for items in levels.iter().rev() {
        let mut next = 0;
        for it in &items[..take] {
            match *it {
                Item::Leaf(i) => lens[used[i]] += 1,
                Item::Pkg(b) => next = next.max(2 * b + 2),
            }
        }
        take = next;
    }
    lens
}

/// Canonical codes, bit-reversed for LSB-first output.
fn canonical_codes(lens: &[u8]) -> Vec<u16> {
    let mut count = [0u16; 16];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u16; 16];
    let mut code = 0u16;
    for b in 1..16 {
        code = (code + count[b - 1]) << 1;
        next[b] = code;
    }
    let mut codes = vec![0u16; lens.len()];
    for (i, &l) in lens.iter().enumerate() {
        if l == 0 {
            continue;
        }
        let c = next[l as usize];
        next[l as usize] += 1;
        codes[i] = c.reverse_bits() >> (16 - l as u32);
    }
    codes
}

struct BitWriter {
    buf: Vec<u8>,
    bits: u64,
    nbits: u32,
}

impl BitWriter {
    #[inline(always)]
    fn put(&mut self, value: u32, n: u32) {
        self.bits |= (value as u64) << self.nbits;
        self.nbits += n;
        if self.nbits >= 32 {
            self.buf.extend_from_slice(&(self.bits as u32).to_le_bytes());
            self.bits >>= 32;
            self.nbits -= 32;
        }
    }
    fn align_byte(&mut self) {
        while self.nbits > 0 {
            self.buf.push(self.bits as u8);
            self.bits >>= 8;
            self.nbits = self.nbits.saturating_sub(8);
        }
        self.bits = 0;
    }
    fn bit_pos(&self) -> usize {
        self.buf.len() * 8 + self.nbits as usize
    }
}

struct DynamicHeader {
    lit_lens: Vec<u8>,
    dist_lens: Vec<u8>,
    hlit: usize,
    hdist: usize,
    hclen: usize,
    cl_lens: Vec<u8>,
    /// RLE symbols; 16..18 are followed by their extra value.
    rle: Vec<u8>,
    bits: u64,
}

fn dynamic_header(lit_freq: &[u32; 286], dist_freq: &[u32; 30]) -> DynamicHeader {
    let lit_lens = code_lengths(lit_freq, 15);
    let dist_lens = code_lengths(dist_freq, 15);
    let mut hlit = 286;
    while hlit > 257 && lit_lens[hlit - 1] == 0 {
        hlit -= 1;
    }
    let mut hdist = 30;
    while hdist > 1 && dist_lens[hdist - 1] == 0 {
        hdist -= 1;
    }
    let seq: Vec<u8> = lit_lens[..hlit].iter().chain(&dist_lens[..hdist]).copied().collect();
    let mut rle = Vec::new();
    let mut cl_freq = [0u32; 19];
    let mut i = 0;
    while i < seq.len() {
        let v = seq[i];
        let mut r = 1;
        while i + r < seq.len() && seq[i + r] == v {
            r += 1;
        }
        if v == 0 {
            if r >= 11 {
                let m = r.min(138);
                rle.extend_from_slice(&[18, (m - 11) as u8]);
                cl_freq[18] += 1;
                i += m;
            } else if r >= 3 {
                rle.extend_from_slice(&[17, (r - 3) as u8]);
                cl_freq[17] += 1;
                i += r;
            } else {
                rle.push(0);
                cl_freq[0] += 1;
                i += 1;
            }
        } else {
            rle.push(v);
            cl_freq[v as usize] += 1;
            i += 1;
            let mut rest = r - 1;
            while rest >= 3 {
                let m = rest.min(6);
                rle.extend_from_slice(&[16, (m - 3) as u8]);
                cl_freq[16] += 1;
                i += m;
                rest -= m;
            }
        }
    }
    let cl_lens = code_lengths(&cl_freq, 7);
    let mut hclen = 19;
    while hclen > 4 && cl_lens[CL_ORDER[hclen - 1]] == 0 {
        hclen -= 1;
    }
    let mut bits = (5 + 5 + 4 + 3 * hclen) as u64;
    for s in 0..19 {
        bits += cl_freq[s] as u64 * cl_lens[s] as u64;
    }
    bits += cl_freq[16] as u64 * 2 + cl_freq[17] as u64 * 3 + cl_freq[18] as u64 * 7;
    DynamicHeader { lit_lens, dist_lens, hlit, hdist, hclen, cl_lens, rle, bits }
}

#[inline(always)]
fn hash(d: &[u8], p: usize) -> usize {
    let x = ((d[p] as u32) << 16) | ((d[p + 1] as u32) << 8) | d[p + 2] as u32;
    (x.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
}

struct Encoder<'a> {
    d: &'a [u8],
    n: usize,
    head: Vec<i32>,
    prev: Vec<i32>,
    ins: usize,
    s_len: Vec<u16>,
    s_dist: Vec<u16>,
    block_start: usize,
    lit_freq: [u32; 286],
    dist_freq: [u32; 30],
    out: BitWriter,
    fixed_lit_len: [u8; 288],
    fixed_lit_code: Vec<u16>,
    fixed_dist_len: [u8; 30],
    fixed_dist_code: Vec<u16>,
}

impl<'a> Encoder<'a> {
    fn new(d: &'a [u8]) -> Self {
        let fixed_lit_len = fixed_lit_len();
        let fixed_dist_len = [5u8; 30];
        Encoder {
            d,
            n: d.len(),
            head: vec![-1; 1 << HASH_BITS],
            // Only read for positions inserted in this run, so the initial value does not matter.
            prev: vec![-1; WSIZE],
            ins: 0,
            s_len: Vec::with_capacity(BLOCK_SYMBOLS + MAX_MATCH + 8),
            s_dist: Vec::with_capacity(BLOCK_SYMBOLS + MAX_MATCH + 8),
            block_start: 0,
            lit_freq: [0; 286],
            dist_freq: [0; 30],
            out: BitWriter { buf: Vec::with_capacity(d.len() / 2 + 1024), bits: 0, nbits: 0 },
            fixed_lit_code: canonical_codes(&fixed_lit_len),
            fixed_lit_len,
            fixed_dist_code: canonical_codes(&fixed_dist_len),
            fixed_dist_len,
        }
    }

    fn insert_up_to(&mut self, end: usize) {
        let last = end.min(self.n.saturating_sub(2));
        let d = self.d;
        for p in self.ins..last {
            let h = hash(d, p);
            self.prev[p & WMASK] = self.head[h];
            self.head[h] = p as i32;
        }
        if end > self.ins {
            self.ins = end;
        }
    }

    /// (len, dist) of the best match at p longer than `at_least` (len 0 if none), then inserts p.
    fn find_match(&mut self, p: usize, at_least: usize, max_chain: u32) -> (usize, usize) {
        let d = self.d;
        let n = self.n;
        if p + MIN_MATCH > n {
            return (0, 0);
        }
        let max_len = MAX_MATCH.min(n - p);
        if at_least >= max_len {
            self.insert_up_to(p + 1);
            return (0, 0);
        }
        let h = hash(d, p);
        let mut c = self.head[h] as isize;
        let mut chain = max_chain;
        let mut best = at_least;
        let mut best_dist = 0usize;
        let limit = p.saturating_sub(WSIZE) as isize;
        let scan = &d[p..p + max_len];
        let mut scan_end = scan[best];
        while c >= limit && chain > 0 {
            chain -= 1;
            let cu = c as usize;
            if d[cu + best] == scan_end {
                let cand = &d[cu..cu + max_len];
                let len = common_prefix(cand, scan);
                if len > best {
                    best = len;
                    best_dist = p - cu;
                    if len >= max_len {
                        break;
                    }
                    scan_end = scan[best];
                }
            }
            let nx = self.prev[cu & WMASK] as isize;
            if nx >= c {
                break;
            }
            c = nx;
        }
        self.prev[p & WMASK] = self.head[h];
        self.head[h] = p as i32;
        self.ins = p + 1;
        if best == MIN_MATCH && best_dist > TOO_FAR {
            best = 0;
        }
        if best >= MIN_MATCH && best_dist > 0 {
            (best, best_dist)
        } else {
            (0, 0)
        }
    }

    #[inline]
    fn literal(&mut self, p: usize) {
        let b = self.d[p];
        self.s_len.push(b as u16);
        self.s_dist.push(0);
        self.lit_freq[b as usize] += 1;
    }

    #[inline]
    fn push_match(&mut self, len: usize, dist: usize) {
        self.s_len.push(len as u16);
        self.s_dist.push(dist as u16);
        self.lit_freq[257 + LEN_CODE[len] as usize] += 1;
        self.dist_freq[DIST_CODE[dist - 1] as usize] += 1;
    }

    fn run(mut self) -> Vec<u8> {
        let n = self.n;
        let mut p = 0;
        while p < n {
            if self.s_len.len() >= BLOCK_SYMBOLS {
                self.flush(p, false);
            }
            let (mut len, mut dist) = self.find_match(p, 0, MAX_CHAIN);
            if len < MIN_MATCH {
                self.literal(p);
                p += 1;
                continue;
            }
            while len < LAZY_LIMIT && p + 1 < n {
                let chain = if len >= GOOD_LENGTH { MAX_CHAIN >> 2 } else { MAX_CHAIN };
                let (l2, d2) = self.find_match(p + 1, len, chain);
                if l2 > len {
                    self.literal(p);
                    p += 1;
                    len = l2;
                    dist = d2;
                } else {
                    break;
                }
            }
            self.push_match(len, dist);
            self.insert_up_to(p + len);
            p += len;
        }
        self.flush(n, true);
        self.out.align_byte();
        self.out.buf
    }

    /// Write the buffered symbols, which cover input [block_start, end).
    fn flush(&mut self, end: usize, last: bool) {
        let span = end - self.block_start;
        self.lit_freq[256] = 1;
        let lf = self.lit_freq;
        let df = self.dist_freq;
        let mut extra = 0u64;
        for c in 0..29 {
            extra += lf[257 + c] as u64 * LENGTH_EXTRA[c] as u64;
        }
        for c in 0..30 {
            extra += df[c] as u64 * DIST_EXTRA[c] as u64;
        }
        let mut fixed_bits = 3 + extra;
        for s in 0..286 {
            fixed_bits += lf[s] as u64 * self.fixed_lit_len[s] as u64;
        }
        for s in 0..30 {
            fixed_bits += df[s] as u64 * 5;
        }
        let dynh = dynamic_header(&lf, &df);
        let mut dyn_bits = 3 + dynh.bits + extra;
        for s in 0..286 {
            dyn_bits += lf[s] as u64 * dynh.lit_lens[s] as u64;
        }
        for s in 0..30 {
            dyn_bits += df[s] as u64 * dynh.dist_lens[s] as u64;
        }
        let stored_bits = if span <= STORED_MAX {
            let pos = self.out.bit_pos();
            (3 + ((8 - ((pos + 3) % 8)) % 8) + 32 + 8 * span) as u64
        } else {
            u64::MAX
        };
        let bfinal = last as u32;
        if stored_bits <= fixed_bits && stored_bits <= dyn_bits {
            self.out.put(bfinal, 3);
            self.out.align_byte();
            let s = span as u16;
            self.out.buf.extend_from_slice(&s.to_le_bytes());
            self.out.buf.extend_from_slice(&(!s).to_le_bytes());
            self.out.buf.extend_from_slice(&self.d[self.block_start..end]);
        } else if fixed_bits <= dyn_bits {
            self.out.put(bfinal | (1 << 1), 3);
            let (lc, ll, dc, dl) = (
                std::mem::take(&mut self.fixed_lit_code),
                self.fixed_lit_len,
                std::mem::take(&mut self.fixed_dist_code),
                self.fixed_dist_len,
            );
            self.write_symbols(&lc, &ll, &dc, &dl);
            self.fixed_lit_code = lc;
            self.fixed_dist_code = dc;
        } else {
            self.out.put(bfinal | (2 << 1), 3);
            self.out.put((dynh.hlit - 257) as u32, 5);
            self.out.put((dynh.hdist - 1) as u32, 5);
            self.out.put((dynh.hclen - 4) as u32, 4);
            for &o in &CL_ORDER[..dynh.hclen] {
                self.out.put(dynh.cl_lens[o] as u32, 3);
            }
            let cl_codes = canonical_codes(&dynh.cl_lens);
            let mut i = 0;
            while i < dynh.rle.len() {
                let s = dynh.rle[i] as usize;
                self.out.put(cl_codes[s] as u32, dynh.cl_lens[s] as u32);
                match s {
                    16 => {
                        i += 1;
                        self.out.put(dynh.rle[i] as u32, 2)
                    }
                    17 => {
                        i += 1;
                        self.out.put(dynh.rle[i] as u32, 3)
                    }
                    18 => {
                        i += 1;
                        self.out.put(dynh.rle[i] as u32, 7)
                    }
                    _ => {}
                }
                i += 1;
            }
            let lc = canonical_codes(&dynh.lit_lens);
            let dc = canonical_codes(&dynh.dist_lens);
            self.write_symbols(&lc, &dynh.lit_lens, &dc, &dynh.dist_lens);
        }
        self.s_len.clear();
        self.s_dist.clear();
        self.block_start = end;
        self.lit_freq = [0; 286];
        self.dist_freq = [0; 30];
    }

    fn write_symbols(&mut self, lit_code: &[u16], lit_len: &[u8], dist_code: &[u16], dist_len: &[u8]) {
        let out = &mut self.out;
        for (&l, &dist) in self.s_len.iter().zip(&self.s_dist) {
            if dist == 0 {
                let b = l as usize;
                out.put(lit_code[b] as u32, lit_len[b] as u32);
            } else {
                let len = l as usize;
                let lc = LEN_CODE[len] as usize;
                out.put(lit_code[257 + lc] as u32, lit_len[257 + lc] as u32);
                out.put((len - LENGTH_BASE[lc] as usize) as u32, LENGTH_EXTRA[lc] as u32);
                let dist = dist as usize;
                let dc = DIST_CODE[dist - 1] as usize;
                out.put(dist_code[dc] as u32, dist_len[dc] as u32);
                out.put((dist - DIST_BASE[dc] as usize) as u32, DIST_EXTRA[dc] as u32);
            }
        }
        out.put(lit_code[256] as u32, lit_len[256] as u32);
    }
}

/// Length of the common prefix of two equal-length slices, 8 bytes at a time.
#[inline(always)]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let diff = x ^ y;
        if diff != 0 {
            return i + (diff.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Raw DEFLATE stream (zip method 8).
pub fn deflate_raw(data: &[u8]) -> Vec<u8> {
    Encoder::new(data).run()
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

/// gzip member with a fixed header (no mtime, no name, XFL 0, OS 3).
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let body = deflate_raw(data);
    let mut out = Vec::with_capacity(body.len() + 18);
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::{DeflateDecoder, GzDecoder};
    use std::io::Read;

    fn inflate(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        DeflateDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    /// Deterministic pseudo-random bytes (mulberry32, as in the fixture generator).
    fn random(n: usize, mut seed: u32) -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed = seed.wrapping_add(0x6d2b_79f5);
                let mut t = seed;
                t = (t ^ (t >> 15)).wrapping_mul(t | 1);
                t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
                ((t ^ (t >> 14)) >> 24) as u8
            })
            .collect()
    }

    fn inputs() -> Vec<Vec<u8>> {
        let text = "The quick brown fox jumps over the lazy dog. <p class=\"post\">Lorem ipsum dolor sit amet</p>\n";
        let mut v = vec![
            vec![],
            vec![0],
            vec![255],
            b"ab".to_vec(),
            b"abc".to_vec(),
            b"aaaa".to_vec(),
            vec![0; 100_000],
            vec![7; 258 * 3 + 1],
            random(1, 1),
            random(1000, 2),
            random(70_000, 3),
            text.repeat(2000).into_bytes(),
            (0..200_000u32).map(|i| (i % 251) as u8).collect(),
            (0..300_000u32).map(|i| ((i / 7) % 3) as u8 + b'a').collect(),
        ];
        // >1 MB of mixed text and noise.
        let mut big = text.repeat(15_000).into_bytes();
        big.extend(random(200_000, 4));
        v.push(big);
        // Around block boundaries: all-literal inputs of BLOCK_SYMBOLS ± 1 and the stored limit.
        for n in [BLOCK_SYMBOLS - 1, BLOCK_SYMBOLS, BLOCK_SYMBOLS + 1, STORED_MAX, STORED_MAX + 1, 2 * BLOCK_SYMBOLS + 1] {
            v.push(random(n, n as u32));
        }
        v
    }

    #[test]
    fn round_trips_through_flate2() {
        for input in inputs() {
            let raw = deflate_raw(&input);
            assert_eq!(inflate(&raw), input, "len {}", input.len());
            let gz = gzip(&input);
            let mut out = Vec::new();
            GzDecoder::new(&gz[..]).read_to_end(&mut out).unwrap();
            assert_eq!(out, input);
            assert_eq!(&gz[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3]);
        }
    }

    #[test]
    fn empty_input_is_one_fixed_block() {
        assert_eq!(deflate_raw(b""), vec![0x03, 0x00]);
    }

    #[test]
    fn incompressible_data_is_stored() {
        let r = random(10_000, 9);
        assert_eq!(deflate_raw(&r).len(), 10_000 + 5);
    }

    #[test]
    fn package_merge_limits_lengths() {
        // Fibonacci frequencies force an unlimited Huffman tree deeper than 15.
        let mut f = vec![1u32, 1];
        while f.len() < 30 {
            let k = f.len();
            f.push(f[k - 1] + f[k - 2]);
        }
        let l = code_lengths(&f, 15);
        assert!(l.iter().all(|&x| (1..=15).contains(&x)));
        let kraft: f64 = l.iter().map(|&x| 0.5f64.powi(x as i32)).sum();
        assert!((kraft - 1.0).abs() < 1e-12);
        assert_eq!(code_lengths(&[0, 0, 0], 7), vec![1, 1, 0]);
        assert_eq!(code_lengths(&[0, 0, 5], 7), vec![1, 0, 1]);
    }

    /// Byte-for-byte against the TypeScript encoder (rust/tests/fixtures/deflate/generate.mjs).
    #[test]
    fn identical_to_typescript_encoder() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/deflate/fixtures.json");
        let text = std::fs::read_to_string(path).unwrap();
        let cases: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut n = 0;
        for c in cases["cases"].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let input = fixture_input(&c["input"]);
            assert_eq!(input.len() as u64, c["length"].as_u64().unwrap(), "{name}: input length");
            assert_eq!(crate::digest::sha256(&input), c["inputDigest"].as_str().unwrap(), "{name}: input");
            let raw = deflate_raw(&input);
            assert_eq!(raw.len() as u64, c["rawLength"].as_u64().unwrap(), "{name}: raw length");
            assert_eq!(crate::digest::sha256(&raw), c["raw"].as_str().unwrap(), "{name}: raw bytes");
            assert_eq!(crate::digest::sha256(&gzip(&input)), c["gzip"].as_str().unwrap(), "{name}: gzip bytes");
            n += 1;
        }
        assert!(n >= 20, "{n} fixtures");
    }

    /// Rebuild a fixture input from its recipe (see generate.mjs).
    fn fixture_input(spec: &serde_json::Value) -> Vec<u8> {
        let mut out = Vec::new();
        for part in spec.as_array().unwrap() {
            let kind = part[0].as_str().unwrap();
            match kind {
                "hex" => {
                    let h = part[1].as_str().unwrap();
                    out.extend((0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()));
                }
                "text" => {
                    let s = part[1].as_str().unwrap();
                    let times = part[2].as_u64().unwrap() as usize;
                    out.extend(s.repeat(times).into_bytes());
                }
                "random" => out.extend(random(part[1].as_u64().unwrap() as usize, part[2].as_u64().unwrap() as u32)),
                "fill" => out.extend(std::iter::repeat_n(part[1].as_u64().unwrap() as u8, part[2].as_u64().unwrap() as usize)),
                "mod" => {
                    let (n, m, div) = (part[1].as_u64().unwrap(), part[2].as_u64().unwrap(), part[3].as_u64().unwrap());
                    out.extend((0..n).map(|i| ((i / div) % m) as u8));
                }
                "words" => {
                    let (n, seed) = (part[1].as_u64().unwrap() as usize, part[2].as_u64().unwrap() as u32);
                    out.extend(words(n, seed));
                }
                k => panic!("unknown fixture part {k}"),
            }
        }
        out
    }

    /// Pseudo-random words from a small vocabulary, joined by spaces/newlines (as in generate.mjs).
    fn words(n: usize, seed: u32) -> Vec<u8> {
        const VOCAB: [&str; 16] = [
            "the", "site", "file", "build", "layer", "<div>", "</div>", "image", "static", "deflate", "class=\"x\"", "a", "of", "and", "to", "{}",
        ];
        let r = random(n * 2, seed);
        let mut out = Vec::new();
        for i in 0..n {
            out.extend_from_slice(VOCAB[(r[2 * i] & 15) as usize].as_bytes());
            out.push(if r[2 * i + 1] < 32 { b'\n' } else { b' ' });
        }
        out
    }
}
