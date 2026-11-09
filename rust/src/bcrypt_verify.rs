//! bcrypt password verification for `serve --auth-file` (port of `src/bcrypt.ts`,
//! johnhenry/sitefile#278). EksBlowfish comes from the `bcrypt` crate (its raw
//! `bcrypt(cost, salt, key)`); parsing, the key rules and the constant-time
//! comparison are written here so both implementations accept exactly the same
//! hashes and passwords.

const ALPHABET: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// bcrypt's base64 (its own alphabet, no padding) of `bytes`.
fn encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        let chars = (chunk.len() * 8).div_ceil(6).min(4);
        for k in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * k)) & 63) as usize] as char);
        }
    }
    out
}

/// `n` bytes from bcrypt base64 `s`, or `None` unless `s` is exactly their canonical encoding.
fn decode(s: &str, n: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = ((acc << 6) | v) & 0xff_ffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if out.len() < n {
                out.push((acc >> bits) as u8);
            }
        }
    }
    (out.len() == n && encode(&out) == s).then_some(out)
}

/// Whether `hash` looks like a bcrypt entry at all (`$2a$`, `$2b$`, `$2y$`).
pub fn is_bcrypt_prefix(hash: &str) -> bool {
    ["$2a$", "$2b$", "$2y$"].iter().any(|p| hash.starts_with(p))
}

/// A parsed bcrypt hash.
pub struct Bcrypt {
    pub cost: u32,
    pub salt: [u8; 16],
    pub digest: Vec<u8>,
}

/// The parts of a well-formed bcrypt hash (`$2a$`/`$2b$`/`$2y$`, cost 04 to 31,
/// 22 + 31 characters with canonical encodings), else `None`.
pub fn parse_bcrypt(hash: &str) -> Option<Bcrypt> {
    let b = hash.as_bytes();
    if b.len() != 60 || !is_bcrypt_prefix(hash) || b[6] != b'$' {
        return None;
    }
    if !b[4].is_ascii_digit() || !b[5].is_ascii_digit() {
        return None;
    }
    let cost = u32::from(b[4] - b'0') * 10 + u32::from(b[5] - b'0');
    if !(4..=31).contains(&cost) {
        return None;
    }
    let salt: [u8; 16] = decode(&hash[7..29], 16)?.try_into().ok()?;
    let digest = decode(&hash[29..], 23)?;
    Some(Bcrypt { cost, salt, digest })
}

/// The key bcrypt hashes for `password`: its UTF-8 bytes up to the first NUL,
/// then a NUL, at most 72 bytes in all (`bcryptKey` in src/bcrypt.ts).
pub fn bcrypt_key(password: &str) -> Vec<u8> {
    let pw = password.as_bytes();
    let bytes = &pw[..pw.iter().position(|&c| c == 0).unwrap_or(pw.len())];
    let mut key = bytes.to_vec();
    key.push(0);
    key.truncate(72);
    key
}

/// The bcrypt hash string of `password` with this cost and salt (tests).
pub fn bcrypt_hash(password: &str, cost: u32, salt: [u8; 16], prefix: &str) -> String {
    let raw = bcrypt::bcrypt(cost, salt, &bcrypt_key(password));
    format!("{prefix}{cost:02}${}{}", encode(&salt), encode(&raw[..23]))
}

/// Whether `password` matches bcrypt `hash` (false for a malformed one). The
/// digests are compared in constant time.
pub fn verify_bcrypt(hash: &str, password: &str) -> bool {
    let Some(p) = parse_bcrypt(hash) else {
        return false;
    };
    let raw = bcrypt::bcrypt(p.cost, p.salt, &bcrypt_key(password));
    raw[..23]
        .iter()
        .zip(&p.digest)
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenBSD's regression vectors ($2a$) and htpasswd -B output ($2y$), shared with test/bcrypt.test.ts.
    pub(crate) const VECTORS: &[(&str, &str)] = &[
        (
            "$2a$06$DCq7YPn5Rq63x1Lad4cll.TV4S6ytwfsfvkgY8jIucDrjc8deX1s.",
            "",
        ),
        (
            "$2a$06$m0CrhHm10qJ3lXRY.5zDGO3rS2KdeeWLuGmsfGlMfOxih58VYVfxe",
            "a",
        ),
        (
            "$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i",
            "abc",
        ),
        (
            "$2a$06$.rCVZVOThsIa97pEDOxvGuRRgzG64bvtJ0938xuqzv18d3ZpQhstC",
            "abcdefghijklmnopqrstuvwxyz",
        ),
        (
            "$2a$06$fPIsBO8qRqkjj273rfaOI.HtSV9jLDpTbZn782DC6/t7qT67P6FfO",
            "~!@#$%^&*()      ~!@#$%^&*()PNBFRD",
        ),
        (
            "$2y$04$H52uuRGqEIy8.sR2oMtiEu08EXLUPljPpA9ItS9ur0y1xB84QRQI2",
            "correct horse",
        ),
        (
            "$2y$05$oazHu0lR350oOyo2jRbUTuzi6JA4Fsrc5wlinCvXNWT8q8B/KDyUW",
            "correct horse",
        ),
        (
            "$2y$04$NAqzALLvFANm2zUmRyTxDujofZJsxPeCfzjmO1uKWS.Yw9VLGqdqu",
            "",
        ),
        (
            "$2y$04$fIY1G.Modf5ClXH1OSug7OWDvAiHLGvTDprrHq68qfgrlUv.Nynp.",
            "ünïcødé ✓",
        ),
    ];

    #[test]
    fn vectors_verify_and_round_trip() {
        for (hash, pw) in VECTORS {
            assert!(verify_bcrypt(hash, pw), "{hash}");
            assert!(!verify_bcrypt(hash, &format!("{pw}x")), "{hash}");
            let p = parse_bcrypt(hash).unwrap();
            assert_eq!(&bcrypt_hash(pw, p.cost, p.salt, &hash[..4]), hash);
            // $2a$, $2b$ and $2y$ compute the same.
            for prefix in ["$2a$", "$2b$", "$2y$"] {
                assert!(verify_bcrypt(&format!("{prefix}{}", &hash[4..]), pw));
            }
        }
    }

    #[test]
    fn keys_are_truncated_at_72_bytes_and_at_nul() {
        // htpasswd -bnBC 4 u <50 x then 50 y>: only the first 72 bytes count.
        let h = "$2y$04$ZftbXfh1CkyZ6xMWQVjV9.Wm8FDybgCYApVIKqfcgxfyHOex7MIOi";
        let x50 = "x".repeat(50);
        assert!(verify_bcrypt(h, &format!("{x50}{}", "y".repeat(50))));
        assert!(verify_bcrypt(h, &format!("{x50}{}", "y".repeat(22))));
        assert!(!verify_bcrypt(h, &format!("{x50}{}", "y".repeat(21))));
        assert!(verify_bcrypt(VECTORS[2].0, "abc\0def"));
        assert_eq!(bcrypt_key("ab").as_slice(), b"ab\0");
    }

    #[test]
    fn malformed_hashes_are_refused() {
        let ok = VECTORS[0].0;
        assert!(parse_bcrypt(ok).is_some());
        let bads = [
            ok[..59].to_string(),
            format!("{ok}."),
            ok.replacen("$06$", "$03$", 1),
            ok.replacen("$06$", "$32$", 1),
            ok.replacen("$2a$", "$2x$", 1),
            ok.replacen("DCq7", "DC!7", 1),
            // a salt or hash whose last character has bits set that the bytes do not use
            format!("{}/{}", &ok[..28], &ok[29..]),
            format!("{}/", &ok[..59]),
        ];
        for bad in &bads {
            assert!(parse_bcrypt(bad).is_none(), "{bad}");
            assert!(!verify_bcrypt(bad, ""), "{bad}");
        }
        assert!(parse_bcrypt(&ok.replacen("$06$", "$31$", 1)).is_some());
    }
}
