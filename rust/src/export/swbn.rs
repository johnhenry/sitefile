//! `EXPORT … AS swbn KEY <secret-id>`: a Signed Web Bundle for Isolated Web Apps (port of
//! `src/swbn.ts`, issue #207; the format is described there). Ed25519 is `crate::ed25519`.

use super::wbn::write_web_bundle_at;
use crate::cbor::{encode, Cbor};
use crate::error::{ErrorCode::E_SECRET, Result, SitefileError};
use crate::tree::Tree;
use base64::Engine;
use sha2::{Digest, Sha512};

const MAGIC: [u8; 8] = [0xf0, 0x9f, 0x96, 0x8b, 0xf0, 0x9f, 0x93, 0xa6];
const VERSION_B2: [u8; 4] = [0x32, 0x62, 0x00, 0x00];
const PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The 32-byte Ed25519 seed of a PEM PKCS #8 private key.
pub fn parse_ed25519_pem(pem: &str) -> Option<[u8; 32]> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let start = pem.find(BEGIN)? + BEGIN.len();
    let end = start + pem[start..].find(END)?;
    let body = &pem[start..end];
    if !body
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=') || c.is_ascii_whitespace())
    {
        return None;
    }
    let b64: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if der.len() != 48 || der[..16] != PKCS8_PREFIX {
        return None;
    }
    der[16..].try_into().ok()
}

fn base32(bytes: &[u8]) -> String {
    const B32: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let (mut bits, mut value) = (0u32, 0u32);
    for &b in bytes {
        value = (value << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((value >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        value &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(B32[((value << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The Web Bundle ID of an Ed25519 public key.
pub fn web_bundle_id(public_key: &[u8; 32]) -> String {
    let mut b = public_key.to_vec();
    b.extend_from_slice(&[0x00, 0x01, 0x02]);
    base32(&b)
}

/// The signed bundle and its Web Bundle ID. `key_pem` is the secret's value; `key_id` names it.
pub fn write_signed_web_bundle(tree: &Tree, key_pem: &str, key_id: &str) -> Result<(Vec<u8>, String)> {
    let seed = parse_ed25519_pem(key_pem).ok_or_else(|| {
        SitefileError::plain(
            E_SECRET,
            format!("EXPORT ... KEY {key_id}: the secret is not an unencrypted Ed25519 private key in PEM (PKCS #8, as 'openssl genpkey -algorithm ed25519' writes)"),
        )
    })?;
    let public_key = crate::ed25519::public_key(&seed);
    let id = web_bundle_id(&public_key);
    let bundle = write_web_bundle_at(tree, &format!("isolated-app://{id}/"))?;
    let attributes = Cbor::Map(vec![(Cbor::Text("webBundleId".into()), Cbor::Text(id.clone()))]);
    let head = vec![
        Cbor::Bytes(MAGIC.to_vec()),
        Cbor::Bytes(VERSION_B2.to_vec()),
        attributes,
    ];
    let mut stripped = head.clone();
    stripped.push(Cbor::Array(Vec::new()));
    let stripped = encode(&Cbor::Array(stripped));
    let sig_attributes = Cbor::Map(vec![(
        Cbor::Text("ed25519PublicKey".into()),
        Cbor::Bytes(public_key.to_vec()),
    )]);
    let sig_attr_bytes = encode(&sig_attributes);
    let hash = Sha512::digest(&bundle);
    let mut to_sign = Vec::new();
    for part in [&hash[..], &stripped, &sig_attr_bytes] {
        to_sign.extend_from_slice(&(part.len() as u64).to_be_bytes());
        to_sign.extend_from_slice(part);
    }
    let signature = crate::ed25519::sign(&seed, &to_sign);
    let mut block = head;
    block.push(Cbor::Array(vec![Cbor::Array(vec![
        sig_attributes,
        Cbor::Bytes(signature.to_vec()),
    ])]));
    let mut out = encode(&Cbor::Array(block));
    out.extend_from_slice(&bundle);
    Ok((out, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::make_file;

    #[test]
    fn matches_the_typescript_and_wbn_sign_bytes() {
        // RFC 8032's first Ed25519 test key; the digest is TypeScript's, which equals wbn-sign 0.3.1's output.
        let pem = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test/fixtures/swbn/rfc8032-test-key.pem"
        ))
        .unwrap();
        let t = Tree::from_files(vec![
            ("/index.html".to_string(), make_file(b"<p>iwa</p>".to_vec(), "t", 0o644)),
            (
                "/.well-known/manifest.webmanifest".to_string(),
                make_file(br#"{"name":"x"}"#.to_vec(), "t", 0o644),
            ),
        ])
        .unwrap();
        let (bytes, id) = write_signed_web_bundle(&t, &pem, "k").unwrap();
        assert_eq!(id, "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenaaaic");
        assert_eq!(
            crate::digest::sha256(&bytes),
            "sha256:f5d83a92a880a0705e033d6d5f9584afca57968b989334a8f572116c9e76bb25"
        );
        assert!(write_signed_web_bundle(&t, "nope", "k").is_err());
    }
}
