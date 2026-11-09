//! A small, deterministic Zstandard encoder for `dcz` deltas (port of `src/zstd.ts`, issue #215;
//! the algorithm is described there). Byte-identical to the TypeScript encoder.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

const MAX_BLOCK: usize = 128 * 1024;
const MIN_MATCH: usize = 4;
const NEAR: usize = 16;

const LL_DIST: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DIST: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DIST: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

struct CTable {
    log: u32,
    next: Vec<u32>,
    delta_nb_bits: Vec<i64>,
    delta_find_state: Vec<i64>,
}

fn highbit(v: u32) -> u32 {
    31 - v.leading_zeros()
}

fn build_ctable(dist: &[i32], log: u32) -> CTable {
    let size = 1usize << log;
    let mut symbols = vec![0usize; size];
    let mut high = size as i64 - 1;
    for (s, &n) in dist.iter().enumerate() {
        if n == -1 {
            symbols[high as usize] = s;
            high -= 1;
        }
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let mut pos = 0usize;
    for (s, &n) in dist.iter().enumerate() {
        for _ in 0..n.max(0) {
            symbols[pos] = s;
            loop {
                pos = (pos + step) & (size - 1);
                if pos as i64 <= high {
                    break;
                }
            }
        }
    }
    let mut cumul = vec![0usize];
    for &n in dist {
        let last = *cumul.last().expect("non-empty");
        cumul.push(last + if n == -1 { 1 } else { n as usize });
    }
    let mut next = vec![0u32; size];
    let mut at = cumul.clone();
    for (u, &s) in symbols.iter().enumerate() {
        next[at[s]] = (size + u) as u32;
        at[s] += 1;
    }
    let mut delta_nb_bits = Vec::new();
    let mut delta_find_state = Vec::new();
    let mut total: i64 = 0;
    for &n in dist {
        if n == -1 || n == 1 {
            delta_nb_bits.push(((log as i64) << 16) - size as i64);
            delta_find_state.push(total - 1);
            total += 1;
        } else {
            let max_bits_out = log - highbit((n - 1) as u32);
            delta_nb_bits.push(((max_bits_out as i64) << 16) - ((n as i64) << max_bits_out));
            delta_find_state.push(total - n as i64);
            total += n as i64;
        }
    }
    CTable {
        log,
        next,
        delta_nb_bits,
        delta_find_state,
    }
}

#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u128,
    n: u32,
}

impl Bits {
    fn add(&mut self, value: u64, bits: u32) {
        if bits == 0 {
            return;
        }
        let v = (value as u128) & ((1u128 << bits) - 1);
        self.acc |= v << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn close(mut self) -> Vec<u8> {
        self.add(1, 1);
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

struct FseState<'a> {
    t: &'a CTable,
    value: i64,
}

impl<'a> FseState<'a> {
    fn new(t: &'a CTable, symbol: usize) -> Self {
        let nb_bits_out = (t.delta_nb_bits[symbol] + (1 << 15)) >> 16;
        let v = (nb_bits_out << 16) - t.delta_nb_bits[symbol];
        let value = t.next[((v >> nb_bits_out) + t.delta_find_state[symbol]) as usize] as i64;
        FseState { t, value }
    }
    fn encode(&mut self, bits: &mut Bits, symbol: usize) {
        let nb_bits_out = (self.value + self.t.delta_nb_bits[symbol]) >> 16;
        bits.add(self.value as u64, nb_bits_out as u32);
        self.value = self.t.next
            [((self.value >> nb_bits_out) + self.t.delta_find_state[symbol]) as usize]
            as i64;
    }
    fn flush(&self, bits: &mut Bits) {
        bits.add(self.value as u64, self.t.log);
    }
}

const LL_BASE: [u32; 20] = [
    16, 18, 20, 22, 24, 28, 32, 40, 48, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768,
    65536,
];
const LL_EXTRA: [u32; 20] = [
    1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
const ML_BASE: [u32; 21] = [
    35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027, 2051, 4099, 8195, 16387,
    32771, 65539,
];
const ML_EXTRA: [u32; 21] = [
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

fn ll_code(ll: u32) -> (usize, u32, u64) {
    if ll < 16 {
        return (ll as usize, 0, 0);
    }
    let i = LL_BASE.iter().rposition(|&b| b <= ll).expect("ll >= 16");
    (16 + i, LL_EXTRA[i], (ll - LL_BASE[i]) as u64)
}

fn ml_code(ml: u32) -> (usize, u32, u64) {
    if ml < 35 {
        return ((ml - 3) as usize, 0, 0);
    }
    let i = ML_BASE.iter().rposition(|&b| b <= ml).expect("ml >= 35");
    (32 + i, ML_EXTRA[i], (ml - ML_BASE[i]) as u64)
}

struct Sequence {
    ll: u32,
    ml: u32,
    offset: u32,
}

fn literals_header(n: usize) -> Vec<u8> {
    if n < 32 {
        vec![(n << 3) as u8]
    } else if n < 4096 {
        vec![((1 << 2) | ((n & 0xf) << 4)) as u8, (n >> 4) as u8]
    } else {
        vec![
            ((3 << 2) | ((n & 0xf) << 4)) as u8,
            ((n >> 4) & 0xff) as u8,
            (n >> 12) as u8,
        ]
    }
}

fn sequences_header(n: usize) -> Vec<u8> {
    if n < 128 {
        vec![n as u8]
    } else if n < 0x7f00 {
        vec![((n >> 8) + 0x80) as u8, (n & 0xff) as u8]
    } else {
        vec![0xff, ((n - 0x7f00) & 0xff) as u8, ((n - 0x7f00) >> 8) as u8]
    }
}

struct Tables {
    ll: CTable,
    ml: CTable,
    of: CTable,
}

fn compressed_block(t: &Tables, literals: &[u8], seqs: &[Sequence]) -> Vec<u8> {
    let mut out = literals_header(literals.len());
    out.extend_from_slice(literals);
    out.extend(sequences_header(seqs.len()));
    if seqs.is_empty() {
        return out;
    }
    out.push(0);
    type Code = (usize, u32, u64);
    let codes: Vec<(Code, Code, Code)> = seqs
        .iter()
        .map(|s| {
            let off_base = s.offset + 3;
            let of = highbit(off_base);
            (
                ll_code(s.ll),
                ml_code(s.ml),
                (of as usize, of, (off_base - (1 << of)) as u64),
            )
        })
        .collect();
    let mut bits = Bits::default();
    let last = codes.last().expect("non-empty");
    let mut ml = FseState::new(&t.ml, last.1 .0);
    let mut of = FseState::new(&t.of, last.2 .0);
    let mut ll = FseState::new(&t.ll, last.0 .0);
    let extras = |bits: &mut Bits, c: &(Code, Code, Code)| {
        bits.add(c.0 .2, c.0 .1);
        bits.add(c.1 .2, c.1 .1);
        bits.add(c.2 .2, c.2 .1);
    };
    extras(&mut bits, last);
    for c in codes[..codes.len() - 1].iter().rev() {
        of.encode(&mut bits, c.2 .0);
        ml.encode(&mut bits, c.1 .0);
        ll.encode(&mut bits, c.0 .0);
        extras(&mut bits, c);
    }
    ml.flush(&mut bits);
    of.flush(&mut bits);
    ll.flush(&mut bits);
    out.extend(bits.close());
    out
}

fn block_header(last: bool, ty: u32, size: usize) -> [u8; 3] {
    let v = (last as u32) | (ty << 1) | ((size as u32) << 3);
    [v as u8, (v >> 8) as u8, (v >> 16) as u8]
}

/// One frame of `content` compressed against the raw-content `dictionary`.
pub fn zstd_compress(content: &[u8], dictionary: &[u8]) -> Vec<u8> {
    let tables = Tables {
        ll: build_ctable(&LL_DIST, 6),
        ml: build_ctable(&ML_DIST, 6),
        of: build_ctable(&OF_DIST, 5),
    };
    let mut all = dictionary.to_vec();
    all.extend_from_slice(content);
    let key = |p: usize| u32::from_le_bytes([all[p], all[p + 1], all[p + 2], all[p + 3]]);
    let mut table: HashMap<u32, usize> = HashMap::new();
    let end = all.len();
    let insert = |table: &mut HashMap<u32, usize>, p: usize| {
        if p + MIN_MATCH <= end {
            table.insert(key(p), p);
        }
    };
    for p in 0..dictionary.len() {
        insert(&mut table, p);
    }
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd];
    let need = end.max(1);
    let mut window_log = 10u32;
    while (1usize << window_log) < need {
        window_log += 1;
    }
    out.push(0x80);
    out.push(((window_log - 10) << 3) as u8);
    out.extend_from_slice(&(content.len() as u32).to_le_bytes());
    let mut pos = dictionary.len();
    let mut last_offset = 0usize;
    if pos == end {
        out.extend(block_header(true, 0, 0));
        return out;
    }
    while pos < end {
        let block_end = end.min(pos + MAX_BLOCK);
        let block_start = pos;
        let mut literals = Vec::new();
        let mut seqs = Vec::new();
        let mut run = 0u32;
        while pos < block_end {
            let match_len = |c: usize| {
                let mut n = 0;
                while pos + n < block_end && all[c + n] == all[pos + n] {
                    n += 1;
                }
                n
            };
            let mut len = 0usize;
            let mut cand = usize::MAX;
            // The previous match's offset first (an edited file keeps its alignment), then the hash table.
            if last_offset > 0 && pos >= last_offset {
                cand = pos - last_offset;
                len = match_len(cand);
            }
            let try_offset = |off: usize, len: &mut usize, cand: &mut usize| {
                if off == 0 || off > pos || pos - off == *cand {
                    return;
                }
                let n = match_len(pos - off);
                if n > *len {
                    *len = n;
                    *cand = pos - off;
                }
            };
            // The same place in the dictionary (the previous version of the file), then offsets near the
            // previous one (a small insertion or deletion since the last match).
            try_offset(dictionary.len(), &mut len, &mut cand);
            if last_offset > 0 {
                for d in 1..=NEAR {
                    if last_offset > d {
                        try_offset(last_offset - d, &mut len, &mut cand);
                    }
                    try_offset(last_offset + d, &mut len, &mut cand);
                }
            }
            if pos + MIN_MATCH <= end {
                if let Some(&c) = table.get(&key(pos)) {
                    if c != cand {
                        let n = match_len(c);
                        if n > len {
                            len = n;
                            cand = c;
                        }
                    }
                }
            }
            if len >= MIN_MATCH {
                last_offset = pos - cand;
                seqs.push(Sequence {
                    ll: run,
                    ml: len as u32,
                    offset: (pos - cand) as u32,
                });
                run = 0;
                for k in 0..len {
                    insert(&mut table, pos + k);
                }
                pos += len;
            } else {
                literals.push(all[pos]);
                run += 1;
                insert(&mut table, pos);
                pos += 1;
            }
        }
        let raw = &all[block_start..block_end];
        let block = compressed_block(&tables, &literals, &seqs);
        let last = block_end == end;
        if block.len() < raw.len() {
            out.extend(block_header(last, 2, block.len()));
            out.extend(block);
        } else {
            out.extend(block_header(last, 0, raw.len()));
            out.extend_from_slice(raw);
        }
    }
    out
}

/// A `dcz` delta: the skippable-frame magic, the SHA-256 of the dictionary, then the frame.
pub fn dcz_encode(content: &[u8], dictionary: &[u8]) -> Vec<u8> {
    let mut out = vec![0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];
    out.extend_from_slice(&Sha256::digest(dictionary));
    out.extend(zstd_compress(content, dictionary));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        crate::digest::sha256(b)["sha256:".len()..].to_string()
    }

    #[test]
    fn same_bytes_as_typescript() {
        let mut dict = String::new();
        for i in 0..5000u64 {
            dict.push_str(&format!("line {} of text\n", (i * 7919) % 1000));
        }
        let t = dict.replacen("line 5 ", "LINE 5 ", 1) + "tail";
        let d = dcz_encode(t.as_bytes(), dict.as_bytes());
        assert_eq!(d.len(), 76);
        assert_eq!(
            hex(&d),
            "7d60bc78bf0a87b0ec4cf342484cf47fecb394cee21c3777caea97414d527b29"
        );
        let bin: Vec<u8> = (0..300_000u64)
            .map(|i| ((((i * 2_654_435_761) as u32) >> 13) & 255) as u8)
            .collect();
        let z = zstd_compress(&bin, &[]);
        assert_eq!(z.len(), 34483);
        assert_eq!(
            hex(&z),
            "238b55161a0431956632f8fd74aa7f5bbee71d5601993338815ad0e40f65b39c"
        );
        assert_eq!(
            hex(&zstd_compress(&[], &[])),
            "75db1663d3ad0f2d7f360589df9ae3dbc57ec8a7569dd148ac48aff9a55cd3c5"
        );
        assert_eq!(
            hex(&zstd_compress(dict.as_bytes(), &[])),
            "43ef299268621da31f3f233d41474d2d94e7ab70f3d6de70509bc84dc8171d88"
        );
    }
}
