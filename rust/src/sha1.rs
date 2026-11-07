//! SHA-1 (FIPS 180-4), for Git object ids and packfile checksums only (src/resources/git.ts uses
//! Node's `crypto.createHash('sha1')`). Hand-rolled so the Git client adds no crate; it is never
//! used for anything Sitefile itself pins (those are SHA-256).

pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    len: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub fn new() -> Sha1 {
        Sha1 {
            h: [
                0x6745_2301,
                0xEFCD_AB89,
                0x98BA_DCFE,
                0x1032_5476,
                0xC3D2_E1F0,
            ],
            buf: [0; 64],
            len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.len > 0 {
            let n = (64 - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len < 64 {
                return;
            }
            let block = self.buf;
            self.block(&block);
            self.len = 0;
        }
        while data.len() >= 64 {
            let (b, rest) = data.split_at(64);
            self.block(b.try_into().expect("64 bytes"));
            data = rest;
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.len = data.len();
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.total as usize + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bits.to_be_bytes());
        let total = self.total;
        self.update(&pad);
        self.total = total;
        let mut out = [0u8; 20];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut bb, mut c, mut d, mut e] = self.h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((bb & c) | (!bb & d), 0x5A82_7999),
                20..=39 => (bb ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((bb & c) | (bb & d) | (c & d), 0x8F1B_BCDC),
                _ => (bb ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = bb.rotate_left(30);
            bb = a;
            a = t;
        }
        for (h, v) in self.h.iter_mut().zip([a, bb, c, d, e]) {
            *h = h.wrapping_add(v);
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Lowercase hex SHA-1 of the concatenation of `parts`.
pub fn sha1_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    hex(&h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vectors() {
        assert_eq!(sha1_hex(&[b""]), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_hex(&[b"abc"]),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            sha1_hex(&[b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"]),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let a = vec![b'a'; 1_000_000];
        assert_eq!(sha1_hex(&[&a]), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
        // Split updates agree with one update; Git's empty blob id.
        let s = b"The quick brown fox jumps over the lazy dog".repeat(7);
        assert_eq!(sha1_hex(&[&s[..5], &s[5..70], &s[70..]]), sha1_hex(&[&s]));
        assert_eq!(
            sha1_hex(&[b"blob 0\0"]),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }
}
