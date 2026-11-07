//! Reader for unsigned Web Bundles, formats b1 and b2 (port of
//! `src/resources/wbn-reader.ts`). Layouts, as implemented by the WICG Go
//! reference reader (go/bundle/decoder.go):
//!
//! ```text
//! b2: array(5) [ magic, version "b2\0\0", section-lengths, sections, length ]
//! b1: array(6) [ magic, version "b1\0\0", primary URL (text), section-lengths, sections, length ]
//! ```
//!
//! section-lengths is a byte string holding a CBOR array of (name, length) pairs;
//! `sections` is an array with one item per pair; `responses` must be last. The
//! trailing length is an 8-byte big-endian byte string equal to the bundle size.
//! index = { url => value }: b2 value = [offset, length]; b1 value =
//! [variants-value bstr, offset, length, ...] (empty variants-value: exactly one
//! location). Offsets are counted from the first byte of the responses section's
//! CBOR encoding. A response is array(2) [ bstr(CBOR map of bstr -> bstr incl.
//! ":status"), body bstr ].
//!
//! Signatures, manifest and any unknown section are length-checked and ignored;
//! nothing is verified or executed.

use crate::cbor::{self, Cbor, CborError};
use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::normalize_virtual;
use crate::resources::types::ArchiveLimits;
use crate::tree::{make_file, Node, Tree};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub url: String,
    pub status: u16,
    /// Response headers in order (lower-case names, unique: a repeated name keeps
    /// its first position and its last value, like a JavaScript object).
    pub headers: Vec<(String, String)>,
    pub body: Arc<[u8]>,
}

impl BundleEntry {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBundle {
    /// `b1` or `b2`.
    pub version: &'static str,
    pub primary_url: Option<String>,
    /// Sorted by URL.
    pub entries: Vec<BundleEntry>,
}

const MAGIC: [u8; 8] = [0xf0, 0x9f, 0x8c, 0x90, 0xf0, 0x9f, 0x93, 0xa6];
/// Magic prefix: array head, bstr(8) head, then the 8 magic bytes.
const HEAD_LEN: usize = 10;
/// The trailing length field: bstr(8) head + 8 bytes.
const TAIL_LEN: usize = 9;
const MAX_SECTION_LENGTHS: u64 = 8192;

fn has_magic(b: &[u8]) -> bool {
    b.len() >= HEAD_LEN && (b[0] == 0x85 || b[0] == 0x86) && b[1] == 0x48 && b[2..10] == MAGIC
}

pub fn is_web_bundle(bytes: &[u8]) -> bool {
    has_magic(bytes)
}

struct Ctx<'a> {
    origin: &'a str,
    span: Option<&'a SourceSpan>,
}

impl Ctx<'_> {
    fn bad(&self, why: impl AsRef<str>) -> SitefileError {
        SitefileError::new(
            E_ARCHIVE,
            format!("{}: web bundle: {}", self.origin, why.as_ref()),
            self.span.cloned(),
        )
    }
    fn cbor(&self, e: CborError) -> SitefileError {
        self.bad(format!("malformed CBOR: {e}"))
    }
    fn decode(&self, b: &[u8], max_items: u64) -> Result<(Cbor, usize)> {
        cbor::decode(
            b,
            &cbor::Limits {
                max_items,
                ..Default::default()
            },
        )
        .map_err(|e| self.cbor(e))
    }
    fn head(&self, b: &[u8], pos: usize) -> Result<(u8, u64, usize)> {
        cbor::read_head(b, pos).map_err(|e| self.cbor(e))
    }
}

/// A parsed response: status, headers, body.
type Parsed = (u16, Vec<(String, String)>, Arc<[u8]>);

struct Section {
    name: String,
    length: usize,
    start: usize,
}

pub fn read_web_bundle(
    bytes: &[u8],
    origin: &str,
    limits: &ArchiveLimits,
    span: Option<&SourceSpan>,
) -> Result<WebBundle> {
    let c = Ctx { origin, span };
    if !has_magic(bytes) {
        return Err(c.bad("bad magic bytes"));
    }
    if bytes.len() < HEAD_LEN + 5 {
        return Err(c.bad("truncated header"));
    }
    let is_b1 = bytes[0] == 0x86;
    if bytes[HEAD_LEN] != 0x44
        || bytes[HEAD_LEN + 1] != 0x62
        || bytes[HEAD_LEN + 3] != 0
        || bytes[HEAD_LEN + 4] != 0
    {
        return Err(c.bad("unrecognized version field"));
    }
    let ver = bytes[HEAD_LEN + 2];
    if ver != if is_b1 { 0x31 } else { 0x32 } {
        return Err(c.bad(if ver == 0x31 || ver == 0x32 {
            "header does not match its version"
        } else {
            "unsupported version (expected b1 or b2)"
        }));
    }
    let version = if is_b1 { "b1" } else { "b2" };
    let mut pos = HEAD_LEN + 5;

    let mut primary_url: Option<String> = None;
    if is_b1 {
        let (v, end) = c.decode(&bytes[pos..], 1)?;
        match v {
            Cbor::Text(t) => primary_url = Some(t),
            _ => return Err(c.bad("primary URL is not a text string")),
        }
        pos += end;
    }

    // section-lengths
    let (major, arg, next) = c.head(bytes, pos)?;
    if major != 2 || arg > cbor::MAX_SAFE {
        return Err(c.bad("section-lengths is not a byte string"));
    }
    if arg >= MAX_SECTION_LENGTHS {
        return Err(c.bad("section-lengths is too long"));
    }
    let sl_len = arg as usize;
    if next + sl_len > bytes.len() {
        return Err(c.bad("truncated section-lengths"));
    }
    let (sl, sl_end) = c.decode(&bytes[next..next + sl_len], MAX_SECTION_LENGTHS)?;
    let sl = match sl {
        Cbor::Array(items) if sl_end == sl_len && items.len() % 2 == 0 => items,
        _ => return Err(c.bad("malformed section-lengths")),
    };
    pos = next + sl_len;

    let (major, count, sec_next) = c.head(bytes, pos)?;
    if major != 4 {
        return Err(c.bad("sections is not an array"));
    }
    let pairs = (sl.len() / 2) as u64;
    if count != pairs {
        return Err(c.bad(format!(
            "section-lengths declares {pairs} sections but {count} are present"
        )));
    }
    let limit = bytes.len() - TAIL_LEN;
    let mut cursor = sec_next;
    let mut seen = HashSet::new();
    let mut sections: Vec<Section> = Vec::new();
    for pair in sl.chunks(2) {
        let (name, length) = match (&pair[0], &pair[1]) {
            (Cbor::Text(n), l) if l.as_number().is_some() => (n.clone(), l.as_number().unwrap()),
            _ => return Err(c.bad("malformed section-lengths entry")),
        };
        if !seen.insert(name.clone()) {
            return Err(c.bad(format!("duplicate section '{name}'")));
        }
        let end = usize::try_from(length)
            .ok()
            .and_then(|l| cursor.checked_add(l));
        match end {
            Some(e) if e <= limit => {
                sections.push(Section {
                    name,
                    length: length as usize,
                    start: cursor,
                });
                cursor = e;
            }
            _ => {
                return Err(c.bad(format!(
                    "section '{name}' extends past the end of the bundle"
                )))
            }
        }
    }
    let responses = match sections.last() {
        Some(s) if s.name == "responses" => (s.start, s.length),
        _ => return Err(c.bad("the last section must be 'responses'")),
    };

    // trailing length
    if cursor + TAIL_LEN != bytes.len() {
        return Err(c.bad("section lengths do not add up to the bundle size"));
    }
    if bytes[cursor] != 0x48 {
        return Err(c.bad("malformed trailing length field"));
    }
    let mut lenb = [0u8; 8];
    lenb.copy_from_slice(&bytes[cursor + 1..cursor + 9]);
    let declared = u64::from_be_bytes(lenb);
    if declared != bytes.len() as u64 {
        return Err(c.bad(format!(
            "length field says {declared} bytes but the bundle is {}",
            bytes.len()
        )));
    }

    let section = |name: &str| {
        sections
            .iter()
            .find(|s| s.name == name)
            .map(|s| &bytes[s.start..s.start + s.length])
    };

    if let Some(ps) = section("primary") {
        let (v, end) = c.decode(ps, 1)?;
        match v {
            Cbor::Text(t) if end == ps.len() => primary_url = Some(t),
            _ => return Err(c.bad("malformed 'primary' section")),
        }
    }
    if let Some(p) = &primary_url {
        if !is_absolute_url(p) {
            return Err(c.bad(format!("primary URL '{p}' is not an absolute URL")));
        }
    }

    let index_section = section("index").ok_or_else(|| c.bad("missing 'index' section"))?;
    let (major, n, _) = c.head(index_section, 0)?;
    if major != 5 {
        return Err(c.bad("index is not a map"));
    }
    if n > limits.max_entries {
        return Err(SitefileError::new(
            E_ARCHIVE_LIMIT,
            format!(
                "{origin}: web bundle has more than {} entries",
                limits.max_entries
            ),
            span.cloned(),
        ));
    }
    let (index, end) = c.decode(index_section, cbor::Limits::default().max_items)?;
    let index = match index {
        Cbor::Map(m) if end == index_section.len() => m,
        _ => return Err(c.bad("malformed index section")),
    };

    let mut parsed: HashMap<(u64, u64), Parsed> = HashMap::new();
    let mut entries: Vec<BundleEntry> = Vec::new();
    let mut total: u64 = 0;
    for (k, v) in index {
        let url = match k {
            Cbor::Text(t) => t,
            _ => return Err(c.bad("index key is not a text string")),
        };
        let value = match v {
            Cbor::Array(a) => a,
            _ => return Err(c.bad(format!("index value for '{url}' is not an array"))),
        };
        let loc: &[Cbor] = if is_b1 {
            if !matches!(value.first(), Some(Cbor::Bytes(_))) {
                return Err(c.bad(format!("index value for '{url}' has no variants-value")));
            }
            if value.len() != 3 {
                return Err(c.bad(if value.len() % 2 == 1 && value.len() > 3 {
                    format!("'{url}' has multiple variants, which are not supported")
                } else {
                    format!("index value for '{url}' has an invalid size")
                }));
            }
            &value[1..]
        } else {
            if value.len() != 2 {
                return Err(c.bad(format!("index value for '{url}' must be [offset, length]")));
            }
            &value[..]
        };
        let (off, len) = match (&loc[0], &loc[1]) {
            (o, l) if o.as_number().is_some() && l.as_number().is_some() => {
                (o.as_number().unwrap(), l.as_number().unwrap())
            }
            _ => {
                return Err(c.bad(format!(
                    "index offset/length for '{url}' is not an unsigned integer"
                )))
            }
        };
        match off.checked_add(len) {
            Some(e) if e <= responses.1 as u64 => {}
            _ => {
                return Err(c.bad(format!(
                    "response for '{url}' lies outside the responses section"
                )))
            }
        }
        let r = match parsed.get(&(off, len)) {
            Some(r) => r.clone(),
            None => {
                let s = responses.0 + off as usize;
                let r = parse_response(&bytes[s..s + len as usize], &url, &c)?;
                parsed.insert((off, len), r.clone());
                r
            }
        };
        total = total.saturating_add(r.2.len() as u64);
        if total > limits.max_total_bytes {
            return Err(SitefileError::new(
                E_ARCHIVE_LIMIT,
                format!(
                    "{origin}: web bundle expands beyond {} bytes",
                    limits.max_total_bytes
                ),
                span.cloned(),
            ));
        }
        entries.push(BundleEntry {
            url,
            status: r.0,
            headers: r.1,
            body: r.2,
        });
    }
    entries.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(WebBundle {
        version,
        primary_url,
        entries,
    })
}

fn ascii_of(b: &[u8]) -> Option<String> {
    if b.iter().all(|&x| (0x20..=0x7e).contains(&x)) {
        Some(b.iter().map(|&x| x as char).collect())
    } else {
        None
    }
}

fn parse_response(b: &[u8], url: &str, c: &Ctx) -> Result<Parsed> {
    let (r, end) = c.decode(b, 16)?;
    let items = match r {
        Cbor::Array(a) if end == b.len() && a.len() == 2 => a,
        _ => return Err(c.bad(format!("response for '{url}' is not [headers, body]"))),
    };
    let mut it = items.into_iter();
    let (hdr, body) = match (it.next(), it.next()) {
        (Some(Cbor::Bytes(h)), Some(Cbor::Bytes(body))) => (h, body),
        _ => return Err(c.bad(format!("response for '{url}' must hold two byte strings"))),
    };
    let (h, hend) = c.decode(&hdr, 100_000)?;
    let map = match h {
        Cbor::Map(m) if hend == hdr.len() => m,
        _ => return Err(c.bad(format!("response headers for '{url}' are not a CBOR map"))),
    };
    let mut status: Option<u16> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    for (k, v) in map {
        let (k, v) = match (k, v) {
            (Cbor::Bytes(k), Cbor::Bytes(v)) => (k, v),
            _ => {
                return Err(c.bad(format!(
                    "response headers for '{url}' must map byte strings to byte strings"
                )))
            }
        };
        let (name, value) = match (ascii_of(&k), ascii_of(&v)) {
            (Some(n), Some(v)) => (n, v),
            _ => return Err(c.bad(format!("non-ASCII response header for '{url}'"))),
        };
        if name != name.to_ascii_lowercase() {
            return Err(c.bad(format!(
                "response header '{name}' for '{url}' is not lower-case"
            )));
        }
        if name.starts_with(':') {
            if name != ":status" {
                return Err(c.bad(format!("unexpected pseudo-header '{name}' for '{url}'")));
            }
            if value.len() != 3 || !value.bytes().all(|x| x.is_ascii_digit()) {
                return Err(c.bad(format!("invalid :status '{value}' for '{url}'")));
            }
            status = value.parse().ok();
        } else if let Some(slot) = headers.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = value;
        } else {
            headers.push((name, value));
        }
    }
    let status = status.ok_or_else(|| c.bad(format!("response for '{url}' has no :status")))?;
    // Object.fromEntries: array-index names ("0", "10", …) come first, in numeric order.
    let index = |n: &str| -> Option<u64> {
        let ok = n == "0"
            || (!n.starts_with('0')
                && !n.is_empty()
                && n.len() <= 10
                && n.bytes().all(|x| x.is_ascii_digit()));
        ok.then(|| n.parse::<u64>().ok())
            .flatten()
            .filter(|&v| v <= 4_294_967_294)
    };
    let (mut numeric, named): (Vec<_>, Vec<_>) =
        headers.into_iter().partition(|(n, _)| index(n).is_some());
    numeric.sort_by_key(|(n, _)| index(n));
    let headers: Vec<(String, String)> = numeric.into_iter().chain(named).collect();
    Ok((status, headers, Arc::from(body)))
}

// ------------------------------------------------------------- URL helpers

/// `^[A-Za-z][A-Za-z0-9+.-]*:` — the scheme of an absolute URL and the rest.
pub(crate) fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let colon = s.find(':')?;
    let scheme = &s[..colon];
    let mut ch = scheme.chars();
    let first = ch.next()?;
    if !first.is_ascii_alphabetic()
        || !ch.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    {
        return None;
    }
    Some((scheme, &s[colon + 1..]))
}

/// WHATWG host serialization (`new URL('<scheme>://<authority>/').host`) for
/// http(s), via ureq's URL parser (the `url` crate). None if the host is invalid.
pub(crate) fn whatwg_host(scheme: &str, authority: &str) -> Option<String> {
    url_crate_host(scheme, authority).or_else(|| ascii_punycode_host(scheme, authority))
}

/// Node's URL parser (ada) takes a host that is all ASCII after
/// percent-decoding as it is: lower-cased, its `xn--` labels never
/// punycode-checked. The `url` crate rejects such a label when it is not
/// valid punycode (`xn--a`, `xn--`), so check the host with those labels
/// replaced and put them back (found by differential fuzzing, #81).
fn ascii_punycode_host(scheme: &str, authority: &str) -> Option<String> {
    let (host, port) = match authority.rfind(':') {
        Some(i) if !authority.starts_with('[') => (&authority[..i], &authority[i..]),
        _ => (authority, ""),
    };
    let decoded = String::from_utf8(percent_decode_bytes(host)?).ok()?;
    if !decoded.is_ascii() {
        return None;
    }
    let labels: Vec<String> = decoded.split('.').map(|l| l.to_ascii_lowercase()).collect();
    if !labels.iter().any(|l| l.starts_with("xn--")) {
        return None;
    }
    let stand_in = |i: usize| format!("sitefile-xn-{i}");
    let probe: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if l.starts_with("xn--") {
                stand_in(i)
            } else {
                l.clone()
            }
        })
        .collect();
    let parsed = url_crate_host(scheme, &format!("{}{port}", probe.join(".")))?;
    let (parsed_host, parsed_port) = match parsed.rfind(':') {
        Some(i) => (&parsed[..i], &parsed[i..]),
        None => (parsed.as_str(), ""),
    };
    let out: Vec<String> = parsed_host
        .split('.')
        .enumerate()
        .map(|(i, l)| {
            if l == stand_in(i) {
                labels[i].clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    Some(format!("{}{parsed_port}", out.join(".")))
}

/// Percent-decode into bytes (invalid escapes are kept as-is, like the URL parser).
fn percent_decode_bytes(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |x: u8| (x as char).to_digit(16);
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(out)
}

fn url_crate_host(scheme: &str, authority: &str) -> Option<String> {
    let req = ureq::get(&format!("{scheme}://{authority}/"));
    let u = req.request_url().ok()?;
    let url = u.as_url();
    let host = url.host_str()?.to_string();
    Some(match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// Whether `new URL(s)` would succeed. Like the URL parser, leading and
/// trailing C0 controls and spaces are trimmed and tabs and newlines removed
/// first. Special schemes need a valid host (`file` an empty one or a host
/// without credentials or port); others an opaque host without forbidden
/// code points and a valid port, if they have an authority.
fn is_absolute_url(s: &str) -> bool {
    let s: String = s
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let Some((scheme, rest)) = split_scheme(&s) else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let special = matches!(
        scheme.as_str(),
        "http" | "https" | "ws" | "wss" | "ftp" | "file"
    );
    let authority = |r: &str, delims: &[char]| -> String {
        let end = r.find(delims).unwrap_or(r.len());
        r[..end].to_string()
    };
    match scheme.as_str() {
        "http" | "https" | "ws" | "wss" | "ftp" => {
            let auth = authority(rest.trim_start_matches(['/', '\\']), &['/', '\\', '?', '#']);
            let host_part = auth.rsplit('@').next().unwrap_or(&auth);
            !host_part.is_empty()
                && whatwg_host(if scheme == "https" { "https" } else { "http" }, host_part)
                    .is_some()
        }
        "file" => {
            let b = rest.as_bytes();
            if !(b.len() >= 2 && matches!(b[0], b'/' | b'\\') && matches!(b[1], b'/' | b'\\')) {
                return true;
            }
            let r = &rest[2..];
            let auth = authority(r, &['/', '\\', '?', '#']);
            if auth.is_empty() {
                return true;
            }
            let bracketed = auth.starts_with('[');
            !auth.contains('@')
                && (bracketed || !auth.contains(':'))
                && whatwg_host("http", &auth).is_some()
        }
        _ if !special => {
            let Some(r) = rest.strip_prefix("//") else {
                return true;
            };
            let auth = authority(r, &['/', '?', '#']);
            let host_port = auth.rsplit('@').next().unwrap_or(&auth);
            let (host, port) = if host_port.starts_with('[') {
                match host_port.find(']') {
                    Some(i) => (&host_port[..=i], &host_port[i + 1..]),
                    None => return false,
                }
            } else {
                match host_port.rfind(':') {
                    Some(i) => (&host_port[..i], &host_port[i..]),
                    None => (host_port, ""),
                }
            };
            let port_ok = match port.strip_prefix(':') {
                None => port.is_empty(),
                Some(p) => {
                    p.bytes().all(|b| b.is_ascii_digit())
                        && (p.is_empty() || p.parse::<u32>().is_ok_and(|n| n <= 65535))
                }
            };
            let host_ok = if host.starts_with('[') {
                whatwg_host("http", host).is_some()
            } else {
                !host.chars().any(|c| {
                    matches!(
                        c,
                        '\0' | ' '
                            | '#'
                            | '/'
                            | ':'
                            | '<'
                            | '>'
                            | '?'
                            | '@'
                            | '['
                            | '\\'
                            | ']'
                            | '^'
                            | '|'
                    )
                })
            };
            port_ok && host_ok
        }
        _ => true,
    }
}

/// `decodeURIComponent`: percent-decoded bytes must be valid UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if !b.contains(&b'%') {
        return Some(s.to_string());
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |x: u8| (x as char).to_digit(16);
            let hi = b.get(i + 1).copied().and_then(hex)?;
            let lo = b.get(i + 2).copied().and_then(hex)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// --------------------------------------------------------------- to a Tree

struct Item<'a> {
    entry: &'a BundleEntry,
    origin: String,
    host: String,
    path: String,
    query: bool,
}

/// Turn a bundle into a Tree. Only status-200 entries become files. When all
/// such URLs share one origin the artifact path is the URL path; otherwise each
/// path is prefixed with `/<host[:port]>`. A path ending in `/` maps to
/// `<path>index.html`; segments are percent-decoded.
pub fn web_bundle_to_tree(
    bundle: &WebBundle,
    origin: &str,
    span: Option<&SourceSpan>,
) -> Result<Tree> {
    let unsafe_url = |url: &str, why: &str| {
        SitefileError::new(
            E_ARCHIVE_UNSAFE,
            format!(
                "{origin}: unsafe web bundle URL '{}': {why}",
                url.replace('\0', "\\0")
            ),
            span.cloned(),
        )
    };

    let mut items: Vec<Item> = Vec::new();
    for entry in &bundle.entries {
        if entry.status != 200 {
            continue;
        }
        let url = entry.url.as_str();
        // ^([A-Za-z][A-Za-z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$
        let (scheme, rest) = match split_scheme(url) {
            Some((s, r)) if r.starts_with("//") => (s, &r[2..]),
            _ => return Err(unsafe_url(url, "not an absolute http(s) URL")),
        };
        let a_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..a_end];
        let rest = &rest[a_end..];
        let p_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let raw_path = &rest[..p_end];
        let rest = &rest[p_end..];
        let (query, fragment) = match rest.strip_prefix('?') {
            Some(q) => {
                let f = q.find('#').unwrap_or(q.len());
                (true, &q[f..])
            }
            None => (false, rest),
        };
        if fragment.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
            // `.` in `(#.*)` does not match line terminators.
            return Err(unsafe_url(url, "not an absolute http(s) URL"));
        }
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(unsafe_url(
                url,
                &format!("scheme '{scheme}:' is not allowed"),
            ));
        }
        if !fragment.is_empty() {
            return Err(unsafe_url(url, "URL has a fragment"));
        }
        if authority.contains('@') {
            return Err(unsafe_url(url, "URL has credentials"));
        }
        let host =
            whatwg_host(&scheme, authority).ok_or_else(|| unsafe_url(url, "invalid host"))?;
        let raw_path = if raw_path.is_empty() { "/" } else { raw_path };
        if raw_path.contains('\\') {
            return Err(unsafe_url(url, "backslash in path"));
        }
        if !raw_path.starts_with('/') {
            return Err(unsafe_url(url, "invalid path"));
        }
        let mut segs: Vec<&str> = raw_path[1..].split('/').collect();
        let trailing = segs.last() == Some(&"");
        if trailing {
            segs.pop();
        }
        let mut decoded: Vec<String> = Vec::new();
        for s in segs {
            let d = percent_decode(s)
                .ok_or_else(|| unsafe_url(url, "invalid percent-encoding in path"))?;
            if d.is_empty() {
                return Err(unsafe_url(url, "empty path segment"));
            }
            if d == "." || d == ".." {
                return Err(unsafe_url(url, &format!("'{d}' path segment")));
            }
            if d.contains('\0') {
                return Err(unsafe_url(url, "NUL byte in path"));
            }
            if d.contains('\\') {
                return Err(unsafe_url(url, "backslash in path"));
            }
            if d.contains('/') {
                return Err(unsafe_url(url, "encoded slash in path"));
            }
            decoded.push(d);
        }
        if trailing || decoded.is_empty() {
            decoded.push("index.html".into());
        }
        items.push(Item {
            entry,
            origin: format!("{scheme}://{host}"),
            host,
            path: format!("/{}", decoded.join("/")),
            query,
        });
    }

    let multi = items
        .iter()
        .map(|i| i.origin.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1;
    let mut order: Vec<String> = Vec::new();
    let mut files: HashMap<String, (Node, &str, bool)> = HashMap::new();
    for it in &items {
        let raw = if multi {
            format!("/{}{}", it.host, it.path)
        } else {
            it.path.clone()
        };
        let path = normalize_virtual(&raw, span, "web bundle URL path")?.path;
        if let Some((node, prev_url, prev_query)) = files.get(&path) {
            if *prev_url != it.entry.url && (*prev_query || it.query) {
                return Err(unsafe_url(
                    &it.entry.url,
                    &format!("its query string makes it collide with '{prev_url}' at '{path}'"),
                ));
            }
            if node.digest != sha256(&it.entry.body) {
                return Err(SitefileError::new(
                    E_CONFLICT,
                    format!("{origin}: '{prev_url}' and '{}' both map to '{path}' with different content", it.entry.url),
                    span.cloned(),
                ));
            }
            continue;
        }
        let node = make_file(
            it.entry.body.clone(),
            format!("{origin}:{}", it.entry.url),
            0o644,
        );
        files.insert(path.clone(), (node, it.entry.url.as_str(), it.query));
        order.push(path);
    }
    let list: Vec<(String, Node)> = order
        .into_iter()
        .map(|p| {
            let n = files[&p].0.clone();
            (p, n)
        })
        .collect();
    Tree::from_files(list)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::resources::archive::extract_archive;
    use crate::resources::types::DEFAULT_ARCHIVE_LIMITS as L;

    // ------------------------------------------------------------ builders

    pub(crate) fn head(major: u8, n: u64) -> Vec<u8> {
        let mut v = cbor::encode(&Cbor::Uint(n));
        v[0] |= major << 5;
        v
    }
    pub(crate) fn cuint(n: u64) -> Vec<u8> {
        head(0, n)
    }
    pub(crate) fn cbytes(b: &[u8]) -> Vec<u8> {
        let mut v = head(2, b.len() as u64);
        v.extend_from_slice(b);
        v
    }
    pub(crate) fn ctext(s: &str) -> Vec<u8> {
        let mut v = head(3, s.len() as u64);
        v.extend_from_slice(s.as_bytes());
        v
    }
    pub(crate) fn carray(items: &[Vec<u8>]) -> Vec<u8> {
        let mut v = head(4, items.len() as u64);
        for i in items {
            v.extend_from_slice(i);
        }
        v
    }
    pub(crate) fn cmap(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut v = head(5, pairs.len() as u64);
        for (k, x) in pairs {
            v.extend_from_slice(k);
            v.extend_from_slice(x);
        }
        v
    }

    #[derive(Clone)]
    pub(crate) struct Resp {
        pub status: &'static str,
        pub body: String,
        pub headers: Vec<(&'static str, &'static str)>,
    }
    pub(crate) fn resp(body: &str) -> Resp {
        Resp {
            status: "200",
            body: body.into(),
            headers: vec![("content-type", "text/plain")],
        }
    }

    pub(crate) fn response(r: &Resp) -> Vec<u8> {
        let mut pairs = vec![(cbytes(b":status"), cbytes(r.status.as_bytes()))];
        for (k, v) in &r.headers {
            pairs.push((cbytes(k.as_bytes()), cbytes(v.as_bytes())));
        }
        carray(&[cbytes(&cmap(&pairs)), cbytes(r.body.as_bytes())])
    }

    pub(crate) struct Built {
        pub index: Vec<u8>,
        pub responses: Vec<u8>,
    }

    /// Each URL gets its own response; `b1` index values carry a leading empty variants-value.
    pub(crate) fn build(urls: &[(&str, Resp)], b1: bool) -> Built {
        let rs: Vec<Vec<u8>> = urls.iter().map(|(_, r)| response(r)).collect();
        let mut off = if rs.len() < 24 { 1 } else { 3 };
        let mut idx = Vec::new();
        for (i, (u, _)) in urls.iter().enumerate() {
            let mut loc = vec![cuint(off), cuint(rs[i].len() as u64)];
            if b1 {
                loc.insert(0, cbytes(b""));
            }
            idx.push((ctext(u), carray(&loc)));
            off += rs[i].len() as u64;
        }
        Built {
            index: cmap(&idx),
            responses: carray(&rs),
        }
    }

    #[derive(Default)]
    pub(crate) struct Opts {
        pub b1: bool,
        pub primary: Option<&'static str>,
        pub sections: Option<Vec<(&'static str, Vec<u8>)>>,
        pub lengths: Option<Vec<(&'static str, u64)>>,
        pub total: Option<u64>,
    }

    pub(crate) fn bundle(b: &Built, o: Opts) -> Vec<u8> {
        let sections = o.sections.unwrap_or_else(|| {
            vec![
                ("index", b.index.clone()),
                ("responses", b.responses.clone()),
            ]
        });
        let lens = o
            .lengths
            .unwrap_or_else(|| sections.iter().map(|(n, x)| (*n, x.len() as u64)).collect());
        let sl = carray(
            &lens
                .iter()
                .flat_map(|(n, l)| [ctext(n), cuint(*l)])
                .collect::<Vec<_>>(),
        );
        let mut out = vec![if o.b1 { 0x86 } else { 0x85 }];
        out.extend(cbytes(&MAGIC));
        out.extend(cbytes(if o.b1 { b"b1\0\0" } else { b"b2\0\0" }));
        if o.b1 {
            out.extend(ctext(o.primary.unwrap_or("https://x.test/")));
        }
        out.extend(cbytes(&sl));
        out.extend(carray(
            &sections.into_iter().map(|(_, x)| x).collect::<Vec<_>>(),
        ));
        let total = out.len() as u64 + 9;
        out.extend(cbytes(&o.total.unwrap_or(total).to_be_bytes()));
        out
    }

    fn read(b: &[u8]) -> Result<WebBundle> {
        read_web_bundle(b, "in.wbn", &L, None)
    }
    fn read_l(b: &[u8], l: ArchiveLimits) -> Result<WebBundle> {
        read_web_bundle(b, "in.wbn", &l, None)
    }
    fn simple() -> Vec<(&'static str, Resp)> {
        vec![
            ("https://x.test/index.html", resp("<h1>x</h1>")),
            ("https://x.test/a.css", resp("a{}")),
        ]
    }
    fn good() -> Vec<u8> {
        bundle(&build(&simple(), false), Opts::default())
    }
    fn content(t: &Tree) -> Vec<(String, String)> {
        t.entries()
            .map(|(p, n)| (p.clone(), String::from_utf8_lossy(&n.bytes).into_owned()))
            .collect()
    }
    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        v.sort();
        v
    }
    fn code<T: std::fmt::Debug>(r: Result<T>) -> crate::error::ErrorCode {
        r.unwrap_err().code
    }
    fn msg<T: std::fmt::Debug>(r: Result<T>) -> String {
        r.unwrap_err().message()
    }

    // ------------------------------------------------------------ basics

    #[test]
    fn extract_archive_maps_a_bundle_with_digests_origins_and_modes() {
        let urls = vec![
            ("https://example.com/index.html", resp("<h1>hi</h1>")),
            ("https://example.com/css/a.css", resp("a{}")),
            ("https://example.com/img/a%20b.png", resp("png")),
            ("https://example.com/sub/", resp("sub")),
        ];
        let out = extract_archive(
            &bundle(&build(&urls, false), Opts::default()),
            "in.wbn",
            &L,
            None,
        )
        .unwrap();
        assert_eq!(
            content(&out),
            kv(&[
                ("/index.html", "<h1>hi</h1>"),
                ("/css/a.css", "a{}"),
                ("/img/a b.png", "png"),
                ("/sub/index.html", "sub")
            ])
        );
        assert_eq!(out.get("/css/a.css").unwrap().digest, sha256(b"a{}"));
        assert_eq!(
            out.get("/css/a.css").unwrap().origin,
            "in.wbn:https://example.com/css/a.css"
        );
        assert_eq!(out.get("/css/a.css").unwrap().mode, 0o644);
    }

    #[test]
    fn an_empty_bundle_is_an_empty_tree() {
        let b = Built {
            index: cmap(&[]),
            responses: carray(&[]),
        };
        assert_eq!(
            extract_archive(&bundle(&b, Opts::default()), "in.wbn", &L, None)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn headers_status_and_sorting() {
        let mut css = resp("a{}");
        css.headers = vec![("content-type", "text/css; charset=utf-8"), ("x-a", "1")];
        let b = read(&bundle(
            &build(
                &[
                    ("https://x.test/z", resp("z")),
                    ("https://x.test/a.css", css),
                ],
                false,
            ),
            Opts::default(),
        ))
        .unwrap();
        assert_eq!(b.version, "b2");
        assert_eq!(b.primary_url, None);
        assert_eq!(
            b.entries.iter().map(|e| e.url.as_str()).collect::<Vec<_>>(),
            vec!["https://x.test/a.css", "https://x.test/z"]
        );
        assert_eq!(b.entries[0].status, 200);
        assert_eq!(
            b.entries[0].header("content-type"),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(&*b.entries[0].body, b"a{}");
    }

    #[test]
    fn is_web_bundle_b1_and_b2_magic_nothing_else() {
        assert!(is_web_bundle(&good()));
        assert!(is_web_bundle(&bundle(
            &build(&simple(), true),
            Opts {
                b1: true,
                ..Default::default()
            }
        )));
        assert!(!is_web_bundle(b"hello world, not a bundle"));
        assert!(!is_web_bundle(b""));
        let mut x = vec![0x84, 0x48];
        x.extend(MAGIC);
        assert!(!is_web_bundle(&x));
    }

    // ------------------------------------------------------------ b1

    #[test]
    fn b1_primary_url_in_header_single_variant_entries() {
        let b = read(&bundle(
            &build(&simple(), true),
            Opts {
                b1: true,
                primary: Some("https://x.test/index.html"),
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(b.version, "b1");
        assert_eq!(b.primary_url.as_deref(), Some("https://x.test/index.html"));
        assert_eq!(
            b.entries.iter().map(|e| e.url.as_str()).collect::<Vec<_>>(),
            vec!["https://x.test/a.css", "https://x.test/index.html"]
        );
        assert_eq!(&*b.entries[1].body, b"<h1>x</h1>");
    }

    #[test]
    fn b1_extract_archive_maps_it_to_a_tree() {
        let out = extract_archive(
            &bundle(
                &build(&simple(), true),
                Opts {
                    b1: true,
                    ..Default::default()
                },
            ),
            "in.wbn",
            &L,
            None,
        )
        .unwrap();
        assert_eq!(
            content(&out),
            kv(&[("/a.css", "a{}"), ("/index.html", "<h1>x</h1>")])
        );
    }

    #[test]
    fn b1_multiple_variants_is_an_error() {
        let r = response(&resp("x"));
        let n = r.len() as u64;
        let responses = carray(&[r.clone(), r]);
        let v = cbytes(b"Accept-Language;en;fr");
        let index = cmap(&[(
            ctext("https://x.test/"),
            carray(&[v, cuint(1), cuint(n), cuint(1 + n), cuint(n)]),
        )]);
        let e = read(&bundle(
            &Built { index, responses },
            Opts {
                b1: true,
                ..Default::default()
            },
        ))
        .unwrap_err();
        assert_eq!(e.code, E_ARCHIVE);
        assert!(e.message().contains("multiple variants"));
    }

    #[test]
    fn b1_single_named_variant_is_read() {
        let r = response(&resp("x"));
        let index = cmap(&[(
            ctext("https://x.test/"),
            carray(&[cbytes(b"Accept;text/html"), cuint(1), cuint(r.len() as u64)]),
        )]);
        let b = read(&bundle(
            &Built {
                index,
                responses: carray(&[r]),
            },
            Opts {
                b1: true,
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(b.entries.len(), 1);
    }

    #[test]
    fn b1_header_with_b2_version_field_is_rejected() {
        let mut bytes = bundle(
            &build(&simple(), true),
            Opts {
                b1: true,
                ..Default::default()
            },
        );
        bytes[12] = 0x32;
        assert_eq!(code(read(&bytes)), E_ARCHIVE);
    }

    // ------------------------------------------------------------ url -> path

    fn tree_of_bundle(urls: &[(&str, Resp)]) -> Result<Tree> {
        web_bundle_to_tree(
            &read(&bundle(&build(urls, false), Opts::default())).unwrap(),
            "in.wbn",
            None,
        )
    }

    #[test]
    fn single_origin_paths_directory_urls_and_percent_decoding() {
        let t = tree_of_bundle(&[
            ("https://x.test/", resp("root")),
            ("https://x.test/docs/", resp("docs")),
            ("https://x.test/a%20b/c%C3%A9.txt", resp("enc")),
            ("https://x.test/deep/er/f.js", resp("js")),
        ])
        .unwrap();
        assert_eq!(
            content(&t),
            kv(&[
                ("/index.html", "root"),
                ("/docs/index.html", "docs"),
                ("/a b/cé.txt", "enc"),
                ("/deep/er/f.js", "js")
            ])
        );
    }

    #[test]
    fn multiple_origins_are_prefixed_with_host_and_port() {
        let t = tree_of_bundle(&[
            ("https://a.test/index.html", resp("A")),
            ("https://b.test:8443/x/", resp("B")),
            ("http://c.test", resp("C")),
        ])
        .unwrap();
        assert_eq!(
            content(&t),
            kv(&[
                ("/a.test/index.html", "A"),
                ("/b.test:8443/x/index.html", "B"),
                ("/c.test/index.html", "C")
            ])
        );
    }

    #[test]
    fn default_ports_and_case_are_normalized_like_url_host() {
        let t = tree_of_bundle(&[
            ("https://X.Test:443/a", resp("A")),
            ("https://x.test/b", resp("B")),
        ])
        .unwrap();
        assert_eq!(content(&t), kv(&[("/a", "A"), ("/b", "B")]));
    }

    #[test]
    fn non_200_entries_are_skipped() {
        let mut nf = resp("nf");
        nf.status = "404";
        let mut moved = resp("");
        moved.status = "301";
        moved.headers = vec![("location", "/ok.txt")];
        let mut weird = resp("weird");
        weird.status = "204";
        let urls = vec![
            ("https://x.test/ok.txt", resp("ok")),
            ("https://other.test/missing", nf),
            ("https://x.test/moved", moved),
            ("data:text/plain,hi", weird),
        ];
        let b = read(&bundle(&build(&urls, false), Opts::default())).unwrap();
        assert_eq!(
            b.entries
                .iter()
                .find(|e| e.url.ends_with("/moved"))
                .unwrap()
                .header("location"),
            Some("/ok.txt")
        );
        assert_eq!(
            content(&web_bundle_to_tree(&b, "in.wbn", None).unwrap()),
            kv(&[("/ok.txt", "ok")])
        );
    }

    #[test]
    fn aliases_with_identical_bytes_are_fine_different_bytes_conflict() {
        let same = tree_of_bundle(&[
            ("https://x.test/", resp("same")),
            ("https://x.test/index.html", resp("same")),
        ])
        .unwrap();
        assert_eq!(content(&same), kv(&[("/index.html", "same")]));
        assert_eq!(
            code(tree_of_bundle(&[
                ("https://x.test/", resp("one")),
                ("https://x.test/index.html", resp("two"))
            ])),
            E_CONFLICT
        );
    }

    #[test]
    fn a_file_and_a_directory_at_the_same_path_conflict() {
        assert_eq!(
            code(tree_of_bundle(&[
                ("https://x.test/a", resp("file")),
                ("https://x.test/a/b", resp("nested"))
            ])),
            E_CONFLICT
        );
    }

    fn tree_of(urls: &[&str]) -> Result<Tree> {
        let entries = urls
            .iter()
            .map(|u| BundleEntry {
                url: u.to_string(),
                status: 200,
                headers: vec![],
                body: Arc::from(&b"x"[..]),
            })
            .collect();
        web_bundle_to_tree(
            &WebBundle {
                version: "b2",
                primary_url: None,
                entries,
            },
            "in.wbn",
            None,
        )
    }

    #[test]
    fn unsafe_urls_are_rejected() {
        for url in [
            "https://x.test/a/../b",
            "https://x.test/%2e%2e/b",
            "https://x.test/a/%2E%2e/b",
            "https://x.test/a/./b",
            "https://x.test/a%2Fb",
            "https://x.test/a%00b",
            "https://x.test/a\\b",
            "https://x.test/a%5Cb",
            "https://x.test//double",
            "https://x.test/a//b",
            "https://x.test/%ff",
            "https://x.test/page#frag",
            "https://user:pw@x.test/page",
            "ftp://x.test/file",
            "file:///etc/passwd",
            "not a url",
            "https://x.test/a/%2e",
            "https://bad host/x",
            "https:///x",
        ] {
            assert_eq!(code(tree_of(&[url])), E_ARCHIVE_UNSAFE, "{url}");
        }
    }

    #[test]
    fn query_strings_ok_alone_rejected_when_colliding() {
        assert_eq!(
            content(&tree_of(&["https://x.test/page?lang=en"]).unwrap()),
            kv(&[("/page", "x")])
        );
        assert_eq!(
            code(tree_of(&[
                "https://x.test/page?lang=en",
                "https://x.test/page?lang=fr"
            ])),
            E_ARCHIVE_UNSAFE
        );
        assert_eq!(
            code(tree_of(&["https://x.test/page", "https://x.test/page?x=1"])),
            E_ARCHIVE_UNSAFE
        );
        assert_eq!(
            code(tree_of(&[
                "https://x.test/?x=1",
                "https://x.test/index.html"
            ])),
            E_ARCHIVE_UNSAFE
        );
    }

    // ------------------------------------------------------------ corruption

    #[test]
    fn corrupt_bad_magic_truncated_empty() {
        let g = good();
        let mut bad = g.clone();
        bad[5] = 0;
        assert_eq!(code(read(&bad)), E_ARCHIVE);
        for n in [0, 5, 10, 12, 20, g.len() - 1, g.len() - 9, g.len() / 2] {
            assert_eq!(code(read(&g[..n])), E_ARCHIVE, "truncated to {n}");
        }
        let mut longer = g.clone();
        longer.push(0);
        assert_eq!(code(read(&longer)), E_ARCHIVE);
    }

    #[test]
    fn corrupt_unsupported_version() {
        let mut g = good();
        g[12] = 0x33;
        assert!(msg(read(&g)).contains("unsupported version"));
    }

    #[test]
    fn corrupt_trailing_length_field_does_not_match() {
        let b = build(&simple(), false);
        assert!(msg(read(&bundle(
            &b,
            Opts {
                total: Some(1234),
                ..Default::default()
            }
        )))
        .contains("length field"));
    }

    #[test]
    fn corrupt_declared_section_lengths_do_not_add_up() {
        let b = build(&simple(), false);
        let (il, rl) = (b.index.len() as u64, b.responses.len() as u64);
        for lengths in [
            vec![("index", il + 3), ("responses", rl)],
            vec![("index", il), ("responses", rl - 1)],
            vec![("index", 1_000_000_000), ("responses", rl)],
        ] {
            assert_eq!(
                code(read(&bundle(
                    &b,
                    Opts {
                        lengths: Some(lengths),
                        ..Default::default()
                    }
                ))),
                E_ARCHIVE
            );
        }
    }

    #[test]
    fn corrupt_index_offsets_out_of_range() {
        let r = response(&resp("x"));
        let n = r.len() as u64;
        for (off, len) in [(1, n + 1), (5000, 3), (1 << 40, 1), (u64::MAX, 2)] {
            let index = cmap(&[(ctext("https://x.test/"), carray(&[cuint(off), cuint(len)]))]);
            assert_eq!(
                code(read(&bundle(
                    &Built {
                        index,
                        responses: carray(std::slice::from_ref(&r))
                    },
                    Opts::default()
                ))),
                E_ARCHIVE,
                "{off},{len}"
            );
        }
    }

    #[test]
    fn corrupt_index_offset_into_the_middle_of_a_response() {
        let r = response(&resp("xxxx"));
        let index = cmap(&[(
            ctext("https://x.test/"),
            carray(&[cuint(3), cuint(r.len() as u64 - 3)]),
        )]);
        assert_eq!(
            code(read(&bundle(
                &Built {
                    index,
                    responses: carray(&[r])
                },
                Opts::default()
            ))),
            E_ARCHIVE
        );
    }

    #[test]
    fn corrupt_section_layouts() {
        let b = build(&simple(), false);
        let i = || ("index", b.index.clone());
        let r = || ("responses", b.responses.clone());
        for sections in [vec![i()], vec![r(), i()], vec![i(), i(), r()], vec![r()]] {
            assert_eq!(
                code(read(&bundle(
                    &b,
                    Opts {
                        sections: Some(sections),
                        ..Default::default()
                    }
                ))),
                E_ARCHIVE
            );
        }
    }

    #[test]
    fn corrupt_sections_count_differs_from_section_lengths() {
        let b = build(&simple(), false);
        let lengths = vec![
            ("index", b.index.len() as u64),
            ("extra", 0),
            ("responses", b.responses.len() as u64),
        ];
        assert_eq!(
            code(read(&bundle(
                &b,
                Opts {
                    lengths: Some(lengths),
                    ..Default::default()
                }
            ))),
            E_ARCHIVE
        );
    }

    #[test]
    fn corrupt_bad_response_shapes() {
        let mk = |r: Vec<u8>| {
            let index = cmap(&[(
                ctext("https://x.test/"),
                carray(&[cuint(1), cuint(r.len() as u64)]),
            )]);
            bundle(
                &Built {
                    index,
                    responses: carray(&[r]),
                },
                Opts::default(),
            )
        };
        let hdr = |pairs: &[(&str, &str)]| {
            cbytes(&cmap(
                &pairs
                    .iter()
                    .map(|(k, v)| (cbytes(k.as_bytes()), cbytes(v.as_bytes())))
                    .collect::<Vec<_>>(),
            ))
        };
        let body = cbytes(b"b");
        for r in [
            carray(&[hdr(&[("content-type", "x")]), body.clone()]),
            carray(&[hdr(&[(":status", "20x")]), body.clone()]),
            carray(&[hdr(&[(":status", "200"), (":path", "/")]), body.clone()]),
            carray(&[
                hdr(&[(":status", "200"), ("Content-Type", "x")]),
                body.clone(),
            ]),
            carray(&[hdr(&[(":status", "200"), ("x", "caf\u{e9}")]), body.clone()]),
            carray(&[
                cbytes(&cmap(&[(ctext(":status"), ctext("200"))])),
                body.clone(),
            ]),
            carray(&[hdr(&[(":status", "200")]), body.clone(), body.clone()]),
            body.clone(),
        ] {
            assert_eq!(code(read(&mk(r))), E_ARCHIVE);
        }
    }

    #[test]
    fn corrupt_extract_archive_reports_e_archive() {
        let g = good();
        assert_eq!(
            code(extract_archive(&g[..g.len() - 4], "in.wbn", &L, None)),
            E_ARCHIVE
        );
    }

    #[test]
    fn unknown_signatures_and_manifest_sections_are_ignored() {
        let b = build(&simple(), false);
        let junk = b"not even cbor \xff".to_vec();
        let sections = vec![
            ("index", b.index.clone()),
            ("signatures", junk.clone()),
            ("manifest", junk.clone()),
            ("x-future", junk),
            ("responses", b.responses.clone()),
        ];
        assert_eq!(
            read(&bundle(
                &b,
                Opts {
                    sections: Some(sections),
                    ..Default::default()
                }
            ))
            .unwrap()
            .entries
            .len(),
            2
        );
    }

    #[test]
    fn primary_section_is_read_invalid_one_rejected() {
        let b = build(&simple(), false);
        let with = |p: Vec<u8>| {
            bundle(
                &b,
                Opts {
                    sections: Some(vec![
                        ("index", b.index.clone()),
                        ("primary", p),
                        ("responses", b.responses.clone()),
                    ]),
                    ..Default::default()
                },
            )
        };
        assert_eq!(
            read(&with(ctext("https://x.test/")))
                .unwrap()
                .primary_url
                .as_deref(),
            Some("https://x.test/")
        );
        assert_eq!(code(read(&with(cuint(5)))), E_ARCHIVE);
        assert_eq!(code(read(&with(ctext("/rel")))), E_ARCHIVE);
        assert!(read(&with(ctext("urn:x"))).is_ok());
    }

    // ------------------------------------------------------------ limits

    #[test]
    fn limits_entry_count_and_total_body_bytes() {
        let g = bundle(
            &build(
                &[
                    ("https://x.test/a", resp("aaaa")),
                    ("https://x.test/b", resp("bbbb")),
                    ("https://x.test/c", resp("cccc")),
                ],
                false,
            ),
            Opts::default(),
        );
        assert_eq!(
            read_l(
                &g,
                ArchiveLimits {
                    max_entries: 3,
                    ..L
                }
            )
            .unwrap()
            .entries
            .len(),
            3
        );
        assert_eq!(
            code(read_l(
                &g,
                ArchiveLimits {
                    max_entries: 2,
                    ..L
                }
            )),
            E_ARCHIVE_LIMIT
        );
        assert_eq!(
            read_l(
                &g,
                ArchiveLimits {
                    max_total_bytes: 12,
                    ..L
                }
            )
            .unwrap()
            .entries
            .len(),
            3
        );
        assert_eq!(
            code(read_l(
                &g,
                ArchiveLimits {
                    max_total_bytes: 11,
                    ..L
                }
            )),
            E_ARCHIVE_LIMIT
        );
        assert_eq!(
            code(extract_archive(
                &g,
                "in.wbn",
                &ArchiveLimits {
                    max_entries: 1,
                    ..L
                },
                None
            )),
            E_ARCHIVE_LIMIT
        );
    }

    #[test]
    fn limits_many_aliases_of_one_big_body_count_per_entry() {
        let big = response(&resp(&"x".repeat(100)));
        let n = big.len() as u64;
        let index = cmap(&["a", "b", "c"].map(|s| {
            (
                ctext(&format!("https://x.test/{s}")),
                carray(&[cuint(1), cuint(n)]),
            )
        }));
        let g = bundle(
            &Built {
                index,
                responses: carray(&[big]),
            },
            Opts::default(),
        );
        assert_eq!(
            code(read_l(
                &g,
                ArchiveLimits {
                    max_total_bytes: 250,
                    ..L
                }
            )),
            E_ARCHIVE_LIMIT
        );
    }

    #[test]
    fn a_negative_index_offset_or_section_length_is_rejected() {
        let r = response(&resp("x"));
        let index = cmap(&[(
            ctext("https://x.test/a"),
            carray(&[vec![0x20], cuint(r.len() as u64)]),
        )]);
        let g = bundle(
            &Built {
                index,
                responses: carray(&[r]),
            },
            Opts::default(),
        );
        assert!(
            msg(read(&g))
                .contains("index offset/length for 'https://x.test/a' is not an unsigned integer"),
            "{}",
            msg(read(&g))
        );
        let mut g = good();
        // section-lengths ["index", n, …]: make n negative (uint8 head 0x18 -> nint8 head 0x38)
        let at = g.windows(5).position(|w| w == b"index").unwrap() + 5;
        assert_eq!(g[at], 0x18);
        g[at] = 0x38;
        assert!(
            msg(read(&g)).contains("malformed section-lengths entry"),
            "{}",
            msg(read(&g))
        );
    }

    #[test]
    fn header_names_that_are_array_indices_come_first() {
        let mut r = resp("x");
        r.headers = vec![("x-b", "1"), ("10", "a"), ("2", "b")];
        let g = bundle(&build(&[("https://x.test/a", r)], false), Opts::default());
        let names: Vec<_> = read(&g).unwrap().entries[0]
            .headers
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(names, ["2", "10", "x-b"]);
    }

    #[test]
    fn primary_urls_are_checked_like_the_url_parser() {
        for (u, ok) in [
            ("\u{0}https://e.x/", true),
            (" https://e.x/ ", true),
            ("ht\ttps://e.x/", true),
            ("file://h:1/a", false),
            ("file://u@h/a", false),
            ("file:///a", true),
            ("x://a b/", false),
            ("x://h:80/", true),
            ("x://h:8a/", false),
            ("http://xn--a.x/", true),
            ("http://\u{e9}.xn--a/", false),
        ] {
            assert_eq!(is_absolute_url(u), ok, "{u:?}");
        }
    }

    #[test]
    fn a_declared_index_size_beyond_the_input_does_not_allocate() {
        let g = bundle(
            &Built {
                index: vec![0xbb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
                responses: carray(&[]),
            },
            Opts::default(),
        );
        assert_eq!(code(read(&g)), E_ARCHIVE_LIMIT);
        let g2 = bundle(
            &Built {
                index: vec![0xb9, 0x01, 0x00],
                responses: carray(&[]),
            },
            Opts::default(),
        );
        assert_eq!(code(read(&g2)), E_ARCHIVE);
    }
}
