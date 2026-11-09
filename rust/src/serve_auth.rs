//! HTTP basic authentication for `sitefile serve --auth` / `--auth-file` (port of
//! `src/serve-auth.ts`; same verifier strings and rules, johnhenry/sitefile#177).
//!
//! A credential is kept only as a verifier, never as the password:
//! `pbkdf2-sha256$<iterations>$<salt base64>$<32-byte key base64>` (`--auth`,
//! `SITEFILE_SERVE_AUTH`) or `{SHA}<base64 SHA-1>` (an htpasswd `-s` entry).

use crate::error::{ErrorCode::*, Result, SitefileError};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthEntry {
    pub user: String,
    pub verifier: String,
}

/// PBKDF2 rounds of a verifier made here (checks are cached per server).
pub const AUTH_ITERATIONS: u32 = 20000;

/// `user:password`: the user is non-empty and has no `:`; the password is the rest.
pub fn parse_auth_pair(value: &str, what: &str) -> Result<(String, String)> {
    match value.find(':') {
        Some(i) if i > 0 && i < value.len() - 1 => {
            Ok((value[..i].to_string(), value[i + 1..].to_string()))
        }
        _ => Err(SitefileError::plain(
            E_USAGE,
            format!("{what} expects user:password"),
        )),
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// PBKDF2-HMAC-SHA256 with a 32-byte key (one block).
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut msg = salt.to_vec();
    msg.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &msg);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    out
}

/// A pbkdf2-sha256 verifier for `password` (a random salt unless one is given).
pub fn make_verifier(password: &str, salt: Option<&[u8]>, iterations: u32) -> String {
    let random;
    let salt = match salt {
        Some(s) => s,
        None => {
            random = (0..8)
                .flat_map(|_| {
                    let h = crate::export::exporter::random_hex(2);
                    u16::from_str_radix(&h, 16).unwrap_or(0).to_be_bytes()
                })
                .collect::<Vec<u8>>();
            &random
        }
    };
    let key = pbkdf2_sha256(password.as_bytes(), salt, iterations);
    format!(
        "pbkdf2-sha256${iterations}${}${}",
        STANDARD.encode(salt),
        STANDARD.encode(key)
    )
}

/// Canonical base64 (standard alphabet, padded), else `None`.
fn b64(s: &str) -> Option<Vec<u8>> {
    let b = STANDARD.decode(s).ok()?;
    (STANDARD.encode(&b) == s).then_some(b)
}

fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether `password` matches `verifier` (false for a malformed verifier).
pub fn verify_password(verifier: &str, password: &str) -> bool {
    if let Some(h) = verifier.strip_prefix("{SHA}") {
        let Some(want) = b64(h) else { return false };
        let mut s = crate::sha1::Sha1::new();
        s.update(password.as_bytes());
        return equal(&s.finish(), &want);
    }
    let parts: Vec<&str> = verifier.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let iter_ok = (1..=7).contains(&parts[1].len()) && parts[1].bytes().all(|b| b.is_ascii_digit());
    let Some(iterations) = parts[1].parse::<u32>().ok().filter(|n| iter_ok && *n >= 1) else {
        return false;
    };
    let (Some(salt), Some(want)) = (b64(parts[2]), b64(parts[3])) else {
        return false;
    };
    if salt.is_empty() || want.len() != 32 {
        return false;
    }
    equal(
        &pbkdf2_sha256(password.as_bytes(), &salt, iterations),
        &want,
    )
}

/// An htpasswd file (`parseHtpasswd` in src/serve-auth.ts): `user:hash` lines, only `{SHA}`.
pub fn parse_htpasswd(text: &str, origin: &str) -> Result<Vec<AuthEntry>> {
    let mut out = Vec::new();
    for (n, raw) in text.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let bad = |m: String| {
            Err(SitefileError::plain(
                E_USAGE,
                format!("{origin}:{}: {m}", n + 1),
            ))
        };
        let Some(i) = line.find(':').filter(|i| *i > 0) else {
            return bad("expected user:hash".into());
        };
        let (user, hash) = (&line[..i], &line[i + 1..]);
        if hash.starts_with("$2a$") || hash.starts_with("$2b$") || hash.starts_with("$2y$") {
            return bad(format!(
                "user '{user}': bcrypt entries are not supported yet; use htpasswd -s ({{SHA}})"
            ));
        }
        let sha_ok = hash.strip_prefix("{SHA}").is_some_and(|h| {
            h.len() == 28
                && h.ends_with('=')
                && h[..27]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        });
        if !sha_ok {
            return bad(format!(
                "user '{user}': unsupported password hash (only {{SHA}} entries, htpasswd -s)"
            ));
        }
        out.push(AuthEntry {
            user: user.to_string(),
            verifier: hash.to_string(),
        });
    }
    Ok(out)
}

/// The user and password of an `Authorization: Basic …` value.
pub fn basic_credentials(header: Option<&str>) -> Option<(String, String)> {
    let h = header?.trim();
    if h.len() < 6 || !h[..5].eq_ignore_ascii_case("basic") || h.as_bytes()[5] != b' ' {
        return None;
    }
    let token = h[5..].trim_start_matches(' ').trim_end_matches(' ');
    let pad = token.len() - token.trim_end_matches('=').len();
    let body = &token[..token.len() - pad];
    if body.is_empty()
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    let raw = b64(token)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let i = text.find(':')?;
    Some((text[..i].to_string(), text[i + 1..].to_string()))
}

/// Accepts `Authorization` values for a set of entries; good values are remembered (at most 64).
pub struct BasicAuth {
    pub entries: Vec<AuthEntry>,
    good: Mutex<HashSet<String>>,
}

impl BasicAuth {
    pub fn new(entries: Vec<AuthEntry>) -> BasicAuth {
        BasicAuth {
            entries,
            good: Mutex::new(HashSet::new()),
        }
    }

    pub fn allows(&self, header: Option<&str>) -> bool {
        let Some(h) = header else { return false };
        if self.good.lock().map(|g| g.contains(h)).unwrap_or(false) {
            return true;
        }
        let Some((user, password)) = basic_credentials(Some(h)) else {
            return false;
        };
        let mut ok = false;
        for e in &self.entries {
            if e.user == user && verify_password(&e.verifier, &password) {
                ok = true;
            }
        }
        if ok {
            if let Ok(mut g) = self.good.lock() {
                if g.len() >= 64 {
                    g.clear();
                }
                g.insert(h.to_string());
            }
        }
        ok
    }
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BasicAuth({} users)", self.entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_matches_rfc_7914_vector() {
        // RFC 7914 section 11: PBKDF2-HMAC-SHA256 (P="passwd", S="salt", c=1), first 32 bytes.
        let dk = pbkdf2_sha256(b"passwd", b"salt", 1);
        assert_eq!(
            crate::sha1::hex(&dk),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn verifiers_and_htpasswd() {
        let v = make_verifier("s3cret", Some(b"0123456789abcdef"), 1000);
        assert!(v.starts_with("pbkdf2-sha256$1000$MDEyMzQ1Njc4OWFiY2RlZg==$"));
        assert!(verify_password(&v, "s3cret"));
        assert!(!verify_password(&v, "s3cret!"));
        assert!(verify_password(
            &make_verifier("x", None, AUTH_ITERATIONS),
            "x"
        ));
        // htpasswd -bns alice password
        let e = parse_htpasswd("# c\nalice:{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=\n\n", "f").unwrap();
        assert!(verify_password(&e[0].verifier, "password"));
        for bad in ["bob:$2y$05$abc", "bob:$apr1$x$y", "bob:plain", "nouser"] {
            assert!(parse_htpasswd(bad, "f").is_err(), "{bad}");
        }
        assert_eq!(
            basic_credentials(Some("Basic YWxpY2U6cGFzczp3b3Jk")),
            Some(("alice".into(), "pass:word".into()))
        );
        for bad in ["Bearer x", "Basic", "Basic YWxpY2U", "Basic YWxpY2U6cGFzcw"] {
            assert_eq!(basic_credentials(Some(bad)), None, "{bad}");
        }
        let a = BasicAuth::new(e);
        assert!(a.allows(Some("Basic YWxpY2U6cGFzc3dvcmQ=")));
        assert!(a.allows(Some("Basic YWxpY2U6cGFzc3dvcmQ=")), "cached");
        assert!(!a.allows(Some("Basic YWxpY2U6d3Jvbmc=")));
        assert!(!a.allows(None));
    }
}
