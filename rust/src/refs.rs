//! Image reference grammar (port of `src/ref.ts`), shaped like Docker's:
//!
//! ```text
//! [registry-host[:port]/]name[:tag][@sha256:<64 hex>]
//! ```
//!
//! name = component ('/' component)*, component = `[a-z0-9]+([._-][a-z0-9]+)*`;
//! tag = `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}` (default `latest`). The first path
//! component is a registry host when it contains `.` or `:` or is `localhost`
//! and something follows it.
//!
//! A reference with a registry host is, locally, just a tag; the store keys it
//! as `registry/name:tag`. Docker Hub aliases (`docker.io`, `index.docker.io`,
//! `registry-1.docker.io`) are one registry, and a Hub name without a namespace
//! lives in `library/`. A reference with no registry host is local everywhere
//! except `pull` and `BASE`, which default it to Docker Hub (see
//! [`with_default_registry`]).

use crate::error::{ErrorCode::*, Result, SitefileError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: Option<String>,
    pub name: String,
    /// The explicit tag, if any.
    pub tag: Option<String>,
    /// `sha256:<hex>` when the reference pins an image ID.
    pub digest: Option<String>,
}

pub const DEFAULT_TAG: &str = "latest";
pub const DOCKER_HUB: &str = "docker.io";
const DOCKER_HUB_ALIASES: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];

fn bad(input: &str, why: &str) -> SitefileError {
    SitefileError::plain(E_USAGE, format!("invalid image reference '{input}': {why}"))
}

fn lower_alnum(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// `[a-z0-9]+([._-][a-z0-9]+)*`
pub(crate) fn is_component(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || !lower_alnum(b[0]) || !lower_alnum(b[b.len() - 1]) {
        return false;
    }
    let mut prev_sep = false;
    for &c in b {
        if lower_alnum(c) {
            prev_sep = false;
        } else if matches!(c, b'.' | b'_' | b'-') {
            if prev_sep {
                return false;
            }
            prev_sep = true;
        } else {
            return false;
        }
    }
    true
}

/// `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`
pub(crate) fn is_tag(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// `(?:[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?|\[[0-9A-Fa-f:]+\])(?::[0-9]{1,5})?`
pub(crate) fn is_host(s: &str) -> bool {
    let host_part_ok = |h: &str| -> bool {
        let b = h.as_bytes();
        if b.first() == Some(&b'[') {
            return b.len() > 2
                && b[b.len() - 1] == b']'
                && b[1..b.len() - 1]
                    .iter()
                    .all(|&c| c.is_ascii_hexdigit() || c == b':');
        }
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    };
    let port_ok = |p: &str| !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit());
    if host_part_ok(s) {
        return true;
    }
    // With a port: split at the last ':' (an IPv6 literal ends with ']').
    match s.rfind(':') {
        Some(i) => {
            let (h, p) = (&s[..i], &s[i + 1..]);
            (h.starts_with('[') && h.ends_with(']') || !h.contains(':'))
                && host_part_ok(h)
                && port_ok(p)
        }
        None => false,
    }
}

/// `sha256:<64 lowercase hex>`
pub(crate) fn is_digest(s: &str) -> bool {
    crate::digest::is_integrity(s)
}

pub fn parse_ref(input: &str) -> Result<ImageRef> {
    if input.is_empty() {
        return Err(bad(input, "empty reference"));
    }
    let mut rest = input;
    let mut digest = None;
    if let Some(at) = rest.find('@') {
        let d = &rest[at + 1..];
        rest = &rest[..at];
        if !is_digest(d) {
            return Err(bad(
                input,
                "the digest must be 'sha256:' followed by 64 lowercase hex digits",
            ));
        }
        digest = Some(d.to_string());
    }
    let mut tag = None;
    let from = rest.rfind('/').map(|i| i + 1).unwrap_or(0);
    if let Some(c) = rest[from..].find(':') {
        let colon = from + c;
        let t = &rest[colon + 1..];
        if !is_tag(t) {
            return Err(bad(
                input,
                &format!(
                    "invalid tag '{t}' (letters, digits, '_', '.', '-'; at most 128 characters)"
                ),
            ));
        }
        tag = Some(t.to_string());
        rest = &rest[..colon];
    }
    let mut parts: Vec<&str> = rest.split('/').collect();
    let mut registry = None;
    let first = parts[0];
    if parts.len() > 1 && (first.contains('.') || first.contains(':') || first == "localhost") {
        if !is_host(first) {
            return Err(bad(input, &format!("invalid registry host '{first}'")));
        }
        registry = Some(first.to_string());
        parts.remove(0);
    }
    let name = parts.join("/");
    let units = name.encode_utf16().count();
    if units == 0 || units > 255 {
        return Err(bad(input, "the name must be 1-255 characters"));
    }
    for c in &parts {
        if !is_component(c) {
            return Err(bad(input, &format!("invalid name component '{c}' (lowercase letters and digits separated by '.', '_' or '-')")));
        }
    }
    Ok(ImageRef {
        registry,
        name,
        tag,
        digest,
    })
}

/// Canonical form of a reference with a registry host (Docker Hub aliases and `library/` applied).
pub fn canonical_ref(r: &ImageRef) -> ImageRef {
    match &r.registry {
        Some(reg) if DOCKER_HUB_ALIASES.contains(&reg.to_ascii_lowercase().as_str()) => ImageRef {
            registry: Some(DOCKER_HUB.to_string()),
            name: if r.name.contains('/') {
                r.name.clone()
            } else {
                format!("library/{}", r.name)
            },
            ..r.clone()
        },
        _ => r.clone(),
    }
}

/// `pull` / `BASE` semantics: a reference without a registry host means Docker Hub.
pub fn with_default_registry(r: &ImageRef) -> ImageRef {
    if r.registry.is_none() {
        canonical_ref(&ImageRef {
            registry: Some(DOCKER_HUB.to_string()),
            ..r.clone()
        })
    } else {
        canonical_ref(r)
    }
}

/// The host to connect to for a (canonical) registry name.
pub fn registry_api_host(registry: &str) -> &str {
    if registry == DOCKER_HUB {
        "registry-1.docker.io"
    } else {
        registry
    }
}

/// Parse a reference that may be used as a local name or tag (a registry host is allowed).
pub fn parse_local_ref(input: &str) -> Result<ImageRef> {
    Ok(canonical_ref(&parse_ref(input)?))
}

/// The index key of a local reference: `[registry/]name:tag`.
pub fn ref_key(r: &ImageRef) -> String {
    let c = canonical_ref(r);
    let reg = c.registry.map(|r| format!("{r}/")).unwrap_or_default();
    format!(
        "{reg}{}:{}",
        c.name,
        c.tag.as_deref().unwrap_or(DEFAULT_TAG)
    )
}

/// Hex of at least 12 digits, optionally `sha256:`-prefixed: an image ID (prefix).
pub fn looks_like_image_id(s: &str) -> bool {
    let hex = s.strip_prefix("sha256:").unwrap_or(s);
    (12..=64).contains(&hex.len()) && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_valid_and_invalid() {
        let ok: [(&str, Option<&str>, &str, Option<&str>); 6] = [
            ("site", None, "site", None),
            ("site:v1.2_x-3", None, "site", Some("v1.2_x-3")),
            (
                "my.org/team/site-a_b:latest",
                Some("my.org"),
                "team/site-a_b",
                Some("latest"),
            ),
            (
                "team/site-a_b:latest",
                None,
                "team/site-a_b",
                Some("latest"),
            ),
            (
                "localhost:5000/site:1",
                Some("localhost:5000"),
                "site",
                Some("1"),
            ),
            (
                "registry.example.com/x/y",
                Some("registry.example.com"),
                "x/y",
                None,
            ),
        ];
        for (input, reg, name, tag) in ok {
            let r = parse_ref(input).unwrap();
            assert_eq!(
                (r.registry.as_deref(), r.name.as_str(), r.tag.as_deref()),
                (reg, name, tag),
                "{input}"
            );
        }
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            parse_ref(&format!("site:1@{digest}"))
                .unwrap()
                .digest
                .as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(ref_key(&parse_ref("site").unwrap()), "site:latest");
        let long_tag = format!("a:{}", "x".repeat(129));
        for b in [
            "",
            "Site",
            "a//b",
            "-a",
            "a-",
            "a:",
            "a:-x",
            &long_tag,
            "a b",
            "a@sha256:short",
            "a/B",
            "_a",
            "a..",
            "a/.b",
        ] {
            assert_eq!(
                parse_ref(b).unwrap_err().code,
                E_USAGE,
                "'{b}' should be invalid"
            );
        }
        assert_eq!(
            ref_key(&parse_local_ref("docker.io/library/nginx").unwrap()),
            "docker.io/library/nginx:latest"
        );
        assert_eq!(
            ref_key(&parse_local_ref("index.docker.io/nginx:alpine").unwrap()),
            "docker.io/library/nginx:alpine"
        );
        assert_eq!(
            ref_key(&parse_local_ref("localhost:5000/blog:1").unwrap()),
            "localhost:5000/blog:1"
        );
        assert_eq!(
            ref_key(&with_default_registry(&parse_ref("nginx:alpine").unwrap())),
            "docker.io/library/nginx:alpine"
        );
        assert_eq!(
            ref_key(&with_default_registry(&parse_ref("me/app").unwrap())),
            "docker.io/me/app:latest"
        );
        assert_eq!(registry_api_host("docker.io"), "registry-1.docker.io");
    }

    #[test]
    fn hosts_and_ids() {
        for h in [
            "localhost",
            "localhost:5000",
            "a.b.c",
            "[::1]:5000",
            "[fe80::1]",
            "x-y.z:1",
        ] {
            assert!(is_host(h), "{h}");
        }
        for h in ["-a.b", "a.b-", "a:b:c", "[::1", "a:123456", "a:"] {
            assert!(!is_host(h), "{h}");
        }
        assert!(parse_ref("bad_host.com:x/y").is_err());
        assert!(looks_like_image_id("0123456789ab"));
        assert!(looks_like_image_id(&format!("sha256:{}", "f".repeat(64))));
        assert!(!looks_like_image_id("0123456789a"));
        assert!(!looks_like_image_id("0123456789aG"));
    }
}
