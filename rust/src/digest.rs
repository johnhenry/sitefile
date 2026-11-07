//! `sha256:<hex>` digests.

use sha2::{Digest, Sha256};

pub fn sha256(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let mut s = String::with_capacity(71);
    s.push_str("sha256:");
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex of a digest without the `sha256:` prefix.
pub fn hex_of(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// `sha256:<64 lowercase hex>`
pub fn is_integrity(s: &str) -> bool {
    s.len() == 71
        && s.starts_with("sha256:")
        && s[7..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
