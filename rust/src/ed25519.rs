//! Ed25519 (RFC 8032) for signed Web Bundles (`EXPORT … AS swbn`): `ring`'s constant-time
//! implementation (already in the build through rustls; see rust/Cargo.toml). The TypeScript side
//! uses Node's `crypto.sign`; both produce the same signatures (Ed25519 is deterministic).

use ring::signature::{Ed25519KeyPair, KeyPair};

fn key_pair(seed: &[u8; 32]) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(seed).expect("any 32-byte seed is an Ed25519 key")
}

/// The public key of a 32-byte Ed25519 seed (private key).
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(key_pair(seed).public_key().as_ref());
    out
}

/// An Ed25519 signature of `msg` (deterministic, RFC 8032 section 5.1.6).
pub fn sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let mut out = [0u8; 64];
    out.copy_from_slice(key_pair(seed).sign(msg).as_ref());
    out
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
        ];
        for (sk, pk, msg, sig) in cases {
            let seed: [u8; 32] = hex(sk).try_into().unwrap();
            assert_eq!(public_key(&seed).to_vec(), hex(pk));
            assert_eq!(sign(&seed, &hex(msg)).to_vec(), hex(sig));
        }
    }
}
