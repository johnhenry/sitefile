//! Ed25519 signing (RFC 8032), for signed Web Bundles (`EXPORT … AS swbn`). The TypeScript side uses
//! Node's `crypto.sign`; Rust's std has no Ed25519 and the dependency policy (rust/Cargo.toml) adds no
//! crate for one signature per build, so this is a small, straightforward implementation on `sha2`'s
//! SHA-512: field arithmetic mod 2^255-19 in five 51-bit limbs, extended twisted Edwards coordinates
//! with the complete addition formula, and scalars mod the group order by long division. It signs
//! once per export with a key the user supplies; it is **not constant time**, which is acceptable for
//! a local build tool and is documented in docs/LANGUAGE.md. Tested against the RFC 8032 vectors.

use sha2::{Digest, Sha512};

type Fe = [u64; 5];
const MASK: u64 = (1 << 51) - 1;

fn carry(mut r: [u128; 5]) -> Fe {
    let mut c;
    for _ in 0..2 {
        c = r[0] >> 51;
        r[0] &= MASK as u128;
        r[1] += c;
        c = r[1] >> 51;
        r[1] &= MASK as u128;
        r[2] += c;
        c = r[2] >> 51;
        r[2] &= MASK as u128;
        r[3] += c;
        c = r[3] >> 51;
        r[3] &= MASK as u128;
        r[4] += c;
        c = r[4] >> 51;
        r[4] &= MASK as u128;
        r[0] += c * 19;
    }
    [r[0] as u64, r[1] as u64, r[2] as u64, r[3] as u64, r[4] as u64]
}

fn add(a: &Fe, b: &Fe) -> Fe {
    carry(std::array::from_fn(|i| (a[i] + b[i]) as u128))
}

fn sub(a: &Fe, b: &Fe) -> Fe {
    const TWO_P: Fe = [
        0xFFFFFFFFFFFDA,
        0xFFFFFFFFFFFFE,
        0xFFFFFFFFFFFFE,
        0xFFFFFFFFFFFFE,
        0xFFFFFFFFFFFFE,
    ];
    carry(std::array::from_fn(|i| (a[i] + TWO_P[i] - b[i]) as u128))
}

fn mul(a: &Fe, b: &Fe) -> Fe {
    let m = |x: u64, y: u64| x as u128 * y as u128;
    let b19: [u64; 5] = std::array::from_fn(|i| b[i] * 19);
    carry([
        m(a[0], b[0]) + m(a[1], b19[4]) + m(a[2], b19[3]) + m(a[3], b19[2]) + m(a[4], b19[1]),
        m(a[0], b[1]) + m(a[1], b[0]) + m(a[2], b19[4]) + m(a[3], b19[3]) + m(a[4], b19[2]),
        m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + m(a[3], b19[4]) + m(a[4], b19[3]),
        m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + m(a[4], b19[4]),
        m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]),
    ])
}

fn from_u64(n: u64) -> Fe {
    [n, 0, 0, 0, 0]
}

fn fe_from_bytes(b: &[u8; 32]) -> Fe {
    let mut n = [0u8; 32];
    n.copy_from_slice(b);
    n[31] &= 0x7f;
    let word = |i: usize| -> u64 {
        // 64 bits starting at bit offset i
        let byte = i / 8;
        let mut v: u64 = 0;
        for k in 0..8 {
            if byte + k < 32 {
                v |= (n[byte + k] as u64) << (8 * k);
            }
        }
        v >> (i % 8)
    };
    [
        word(0) & MASK,
        word(51) & MASK,
        word(102) & MASK,
        word(153) & MASK,
        word(204) & MASK,
    ]
}

fn fe_to_bytes(a: &Fe) -> [u8; 32] {
    let mut t = carry(std::array::from_fn(|i| a[i] as u128));
    // t is below 2^255 + small: subtract p once when t >= p.
    let mut q = (t[0] + 19) >> 51;
    q = (t[1] + q) >> 51;
    q = (t[2] + q) >> 51;
    q = (t[3] + q) >> 51;
    q = (t[4] + q) >> 51;
    t[0] += 19 * q;
    for i in 0..4 {
        t[i + 1] += t[i] >> 51;
        t[i] &= MASK;
    }
    t[4] &= MASK;
    let mut out = [0u8; 32];
    let mut acc: u128 = 0;
    let mut bits = 0;
    let mut o = 0;
    for limb in t {
        acc |= (limb as u128) << bits;
        bits += 51;
        while bits >= 8 && o < 32 {
            out[o] = acc as u8;
            acc >>= 8;
            bits -= 8;
            o += 1;
        }
    }
    if o < 32 {
        out[o] = acc as u8;
    }
    out
}

fn pow(a: &Fe, exp_le: &[u8; 32]) -> Fe {
    let mut r = from_u64(1);
    for i in (0..256).rev() {
        r = mul(&r, &r);
        if (exp_le[i / 8] >> (i % 8)) & 1 == 1 {
            r = mul(&r, a);
        }
    }
    r
}

fn exp(low: u8, high: u8) -> [u8; 32] {
    let mut e = [0xffu8; 32];
    e[0] = low;
    e[31] = high;
    e
}

fn inv(a: &Fe) -> Fe {
    pow(a, &exp(0xeb, 0x7f)) // p - 2
}

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

struct Curve {
    d2: Fe,
    base: Point,
}

fn curve() -> Curve {
    let neg = |a: &Fe| sub(&from_u64(0), a);
    let d = mul(&neg(&from_u64(121665)), &inv(&from_u64(121666)));
    // The base point: y = 4/5, x even.
    let y = mul(&from_u64(4), &inv(&from_u64(5)));
    let yy = mul(&y, &y);
    let u = sub(&yy, &from_u64(1));
    let v = add(&mul(&d, &yy), &from_u64(1));
    let x2 = mul(&u, &inv(&v));
    let mut x = pow(&x2, &exp(0xfe, 0x0f)); // (p + 3) / 8
    if fe_to_bytes(&mul(&x, &x)) != fe_to_bytes(&x2) {
        let sqrt_m1 = pow(&from_u64(2), &exp(0xfb, 0x1f)); // 2^((p - 1) / 4)
        x = mul(&x, &sqrt_m1);
    }
    if fe_to_bytes(&x)[0] & 1 == 1 {
        x = neg(&x);
    }
    Curve {
        d2: add(&d, &d),
        base: Point {
            x,
            y,
            z: from_u64(1),
            t: mul(&x, &y),
        },
    }
}

fn point_add(c: &Curve, p: &Point, q: &Point) -> Point {
    let a = mul(&sub(&p.y, &p.x), &sub(&q.y, &q.x));
    let b = mul(&add(&p.y, &p.x), &add(&q.y, &q.x));
    let cc = mul(&mul(&p.t, &c.d2), &q.t);
    let dd = mul(&add(&p.z, &p.z), &q.z);
    let e = sub(&b, &a);
    let f = sub(&dd, &cc);
    let g = add(&dd, &cc);
    let h = add(&b, &a);
    Point {
        x: mul(&e, &f),
        y: mul(&g, &h),
        t: mul(&e, &h),
        z: mul(&f, &g),
    }
}

fn scalar_mult(c: &Curve, k: &[u8; 32]) -> Point {
    let mut q = Point {
        x: from_u64(0),
        y: from_u64(1),
        z: from_u64(1),
        t: from_u64(0),
    };
    for i in (0..256).rev() {
        q = point_add(c, &q, &q);
        if (k[i / 8] >> (i % 8)) & 1 == 1 {
            q = point_add(c, &q, &c.base);
        }
    }
    q
}

fn encode(p: &Point) -> [u8; 32] {
    let zi = inv(&p.z);
    let x = fe_to_bytes(&mul(&p.x, &zi));
    let mut y = fe_to_bytes(&mul(&p.y, &zi));
    y[31] |= (x[0] & 1) << 7;
    y
}

// ---------------------------------------------------------------- scalars mod L

const L: [u64; 5] = [0x5812631a5cf5d3ed, 0x14def9dea2f79cd6, 0, 0x1000000000000000, 0];

fn ge_l(r: &[u64; 5]) -> bool {
    for i in (0..5).rev() {
        if r[i] != L[i] {
            return r[i] > L[i];
        }
    }
    true
}

/// A little-endian number of `words` 64-bit words, mod L.
fn reduce(words: &[u64]) -> [u8; 32] {
    let mut r = [0u64; 5];
    for i in (0..words.len() * 64).rev() {
        let bit = (words[i / 64] >> (i % 64)) & 1;
        for k in (1..5).rev() {
            r[k] = (r[k] << 1) | (r[k - 1] >> 63);
        }
        r[0] = (r[0] << 1) | bit;
        if ge_l(&r) {
            let mut borrow = 0u128;
            for k in 0..5 {
                let v = (r[k] as u128).wrapping_sub(L[k] as u128 + borrow);
                r[k] = v as u64;
                borrow = (v >> 127) & 1;
            }
        }
    }
    let mut out = [0u8; 32];
    for k in 0..4 {
        out[k * 8..k * 8 + 8].copy_from_slice(&r[k].to_le_bytes());
    }
    out
}

fn words(b: &[u8]) -> Vec<u64> {
    b.chunks(8)
        .map(|c| {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            u64::from_le_bytes(w)
        })
        .collect()
}

/// (k * a + r) mod L.
fn mul_add(k: &[u8; 32], a: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let (kw, aw, rw) = (words(k), words(a), words(r));
    let mut t = [0u128; 9];
    for i in 0..4 {
        for j in 0..4 {
            let p = kw[i] as u128 * aw[j] as u128;
            t[i + j] += p & u64::MAX as u128;
            t[i + j + 1] += p >> 64;
        }
    }
    for i in 0..4 {
        t[i] += rw[i] as u128;
    }
    let mut out = [0u64; 9];
    let mut c = 0u128;
    for i in 0..9 {
        let v = t[i] + c;
        out[i] = v as u64;
        c = v >> 64;
    }
    reduce(&out)
}

/// The public key of a 32-byte Ed25519 seed (private key).
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    let h = Sha512::digest(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&h[..32]);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    let c = curve();
    encode(&scalar_mult(&c, &a))
}

/// An Ed25519 signature of `msg` (deterministic, RFC 8032 section 5.1.6).
pub fn sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let c = curve();
    let h = Sha512::digest(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&h[..32]);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    let pk = encode(&scalar_mult(&c, &a));
    let mut hr = Sha512::new();
    hr.update(&h[32..]);
    hr.update(msg);
    let r = reduce(&words(&hr.finalize()));
    let big_r = encode(&scalar_mult(&c, &r));
    let mut hk = Sha512::new();
    hk.update(big_r);
    hk.update(pk);
    hk.update(msg);
    let k = reduce(&words(&hk.finalize()));
    let s = mul_add(&k, &a, &r);
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(&s);
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc8032_vectors() {
        let cases = [
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                "72",
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            ),
            (
                "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
                "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
                "af82",
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
            ),
        ];
        for (sk, pk, msg, sig) in cases {
            let seed: [u8; 32] = hex(sk).try_into().unwrap();
            assert_eq!(public_key(&seed).to_vec(), hex(pk));
            assert_eq!(sign(&seed, &hex(msg)).to_vec(), hex(sig));
        }
    }
}
