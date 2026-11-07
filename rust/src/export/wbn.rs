//! Unsigned Web Bundle writer, format version "b2" (port of `src/wbn.ts`,
//! draft-ietf-wpack-bundled-responses). Layout, as one CBOR array:
//!
//! ```text
//! [ magic h'F09F8C90F09F93A6', version h'62320000' ("b2\0\0"),
//!   section-lengths (bstr holding a CBOR array of name, length, ...),
//!   sections (array: index, [primary,] responses),
//!   length (8-byte big-endian bstr: size of the whole bundle) ]
//! ```
//!
//! index = { url => [offset, length] } locating each response inside the
//! responses section; responses = [[headers bstr, payload bstr] ...];
//! primary = the base URL, present only when /index.html exists. The output
//! is byte-identical to the TypeScript writer for the same tree.

use crate::cbor::head_size;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::tree::Tree;

const MAGIC: [u8; 8] = [0xf0, 0x9f, 0x8c, 0x90, 0xf0, 0x9f, 0x93, 0xa6];
const VERSION: [u8; 4] = [0x62, 0x32, 0x00, 0x00];

/// Content type for a path, by extension (shared with the site server).
pub fn mime_type_for(path: &str) -> &'static str {
    let base = &path[path.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    let ext = match base.rfind('.') {
        Some(i) => base[i + 1..].to_ascii_lowercase(),
        None => String::new(),
    };
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "webmanifest" => "application/manifest+json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is percent-encoded (UTF-8, uppercase hex).
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn invalid(raw: &str, why: &str) -> SitefileError {
    SitefileError::plain(E_VALIDATION, format!("invalid base URL '{raw}': {why}"))
}

/// An absolute https URL ending in `/`, without credentials, query or fragment.
/// Returns its normalized form (lowercase scheme and host, default port dropped).
///
/// This is a subset of WHATWG URL parsing sufficient for https base URLs: IDN
/// hosts and percent-encoding normalization of unusual paths are not
/// rewritten the way `new URL()` would (such URLs are rejected instead).
pub fn validate_base_url(raw: Option<&str>) -> Result<String> {
    let raw = match raw {
        None | Some("") => {
            return Err(SitefileError::plain(
                E_VALIDATION,
                "wbn export needs a base URL: use 'EXPORT <path> AS wbn BASE https://example.com/' or --base-url",
            ))
        }
        Some(r) => r,
    };
    let not_abs = || invalid(raw, "not an absolute URL");
    let s = raw.trim_matches(|c: char| c <= ' ');
    // scheme
    let colon = s.find(':').ok_or_else(not_abs)?;
    let scheme = &s[..colon];
    let sb = scheme.as_bytes();
    if sb.is_empty()
        || !sb[0].is_ascii_alphabetic()
        || !sb
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
    {
        return Err(not_abs());
    }
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(invalid(raw, "must be https"));
    }
    // special scheme: any run of '/' or '\' introduces the authority
    let after = s[colon + 1..].trim_start_matches(['/', '\\']);
    let auth_end = after.find(['/', '\\', '?', '#']).unwrap_or(after.len());
    let authority = &after[..auth_end];
    let rest = &after[auth_end..];
    let (creds, hostport) = match authority.rfind('@') {
        Some(i) => (true, &authority[i + 1..]),
        None => (false, authority),
    };
    // host[:port]
    let (host, port) = if hostport.starts_with('[') {
        let close = hostport.find(']').ok_or_else(not_abs)?;
        let h = &hostport[..=close];
        if !h[1..close]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
            || close < 3
        {
            return Err(not_abs());
        }
        let p = &hostport[close + 1..];
        match p.strip_prefix(':') {
            Some(p) => (h.to_string(), Some(p)),
            None if p.is_empty() => (h.to_string(), None),
            None => return Err(not_abs()),
        }
    } else {
        match hostport.rfind(':') {
            Some(i) => (hostport[..i].to_ascii_lowercase(), Some(&hostport[i + 1..])),
            None => (hostport.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() {
        return Err(not_abs());
    }
    if !host.starts_with('[')
        && !host.bytes().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    b'.' | b'-'
                        | b'_'
                        | b'~'
                        | b'!'
                        | b'$'
                        | b'&'
                        | b'\''
                        | b'('
                        | b')'
                        | b'*'
                        | b'+'
                        | b','
                        | b';'
                        | b'='
                )
        })
    {
        return Err(not_abs());
    }
    let port = match port {
        None | Some("") => String::new(),
        Some(p) => {
            if !p.bytes().all(|c| c.is_ascii_digit()) {
                return Err(not_abs());
            }
            let n: u64 = p.parse().map_err(|_| not_abs())?;
            if n > 65535 {
                return Err(not_abs());
            }
            if n == 443 {
                String::new()
            } else {
                format!(":{n}")
            }
        }
    };
    if creds {
        return Err(invalid(raw, "must not contain credentials"));
    }
    if raw.contains('?') || raw.contains('#') {
        return Err(invalid(raw, "must not have a query or fragment"));
    }
    if !raw.ends_with('/') {
        return Err(invalid(raw, "must end with '/'"));
    }
    // path: backslashes are separators; encode what WHATWG's path set encodes
    let mut path = String::new();
    for c in rest.chars() {
        match c {
            '\\' => path.push('/'),
            c if (c as u32) <= 0x20
                || c as u32 >= 0x7f
                || matches!(c, '"' | '<' | '>' | '`' | '{' | '}') =>
            {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    path.push_str(&format!("%{b:02X}"));
                }
            }
            c => path.push(c),
        }
    }
    // dot segments
    let mut segs: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    for (i, seg) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match seg.to_ascii_lowercase().as_str() {
            "." | "%2e" => {
                if last {
                    segs.push("");
                }
            }
            ".." | ".%2e" | "%2e." | "%2e%2e" => {
                segs.pop();
                if last {
                    segs.push("");
                }
            }
            _ => segs.push(seg),
        }
    }
    let path = if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    };
    Ok(format!("https://{host}{port}{path}"))
}

fn encode_path(p: &str) -> String {
    p[1..]
        .split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n < 0x100 {
        out.extend([m | 24, n as u8]);
    } else if n < 0x10000 {
        out.push(m | 25);
        out.extend((n as u16).to_be_bytes());
    } else if n < 0x1_0000_0000 {
        out.push(m | 26);
        out.extend((n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    head(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}

fn text(out: &mut Vec<u8>, s: &str) {
    head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn uint(n: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    head(&mut v, 0, n);
    v
}

pub fn write_web_bundle(tree: &Tree, base_url: Option<&str>) -> Result<Vec<u8>> {
    let base = validate_base_url(base_url)?;
    let count = tree.len();

    let mut responses: Vec<u8> = Vec::new();
    head(&mut responses, 4, count as u64);
    let mut index: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (p, node) in tree.entries() {
        let mut headers = Vec::new();
        head(&mut headers, 5, 2);
        bytes(&mut headers, b":status");
        bytes(&mut headers, b"200");
        bytes(&mut headers, b"content-type");
        bytes(&mut headers, mime_type_for(p).as_bytes());
        let offset = responses.len() as u64;
        head(&mut responses, 4, 2);
        bytes(&mut responses, &headers);
        bytes(&mut responses, &node.bytes);
        let length = responses.len() as u64 - offset;
        // b2 index entries are [offset, length].
        let mut loc = Vec::new();
        head(&mut loc, 4, 2);
        loc.extend(uint(offset));
        loc.extend(uint(length));
        let mut urls = vec![format!("{base}{}", encode_path(p))];
        if p == "/index.html" || p.ends_with("/index.html") {
            urls.push(format!(
                "{base}{}",
                encode_path(&p[..p.len() - "index.html".len()])
            ));
        }
        for url in urls {
            let mut k = Vec::with_capacity(url.len() + 3);
            text(&mut k, &url);
            index.push((k, loc.clone()));
        }
    }
    // Deterministic CBOR (as cborMap in TypeScript): keys sorted by their encoded bytes.
    index.sort_by(|a, b| a.0.cmp(&b.0));
    let mut index_section =
        Vec::with_capacity(index.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() + 9);
    head(&mut index_section, 5, index.len() as u64);
    for (k, v) in index {
        index_section.extend(k);
        index_section.extend(v);
    }

    let mut sections: Vec<(&str, Vec<u8>)> = vec![("index", index_section)];
    if tree.is_file("/index.html") {
        let mut t = Vec::new();
        text(&mut t, &base);
        sections.push(("primary", t));
    }
    sections.push(("responses", responses));

    let mut lengths = Vec::new();
    head(&mut lengths, 4, sections.len() as u64 * 2);
    for (name, b) in &sections {
        text(&mut lengths, name);
        lengths.extend(uint(b.len() as u64));
    }

    let mut out = Vec::new();
    out.push(0x85); // b2: array(5) = magic, version, section-lengths, sections, length
    bytes(&mut out, &MAGIC);
    bytes(&mut out, &VERSION);
    bytes(&mut out, &lengths);
    head(&mut out, 4, sections.len() as u64);
    for (_, b) in &sections {
        out.extend_from_slice(b);
    }
    // The trailing length field is fixed-size, so the total is computable first.
    let total = out.len() as u64 + 1 + 8;
    bytes(&mut out, &total.to_be_bytes());
    debug_assert_eq!(head_size(count as u64), uint(count as u64).len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::make_file;

    fn mk(files: &[(&str, &str)]) -> Tree {
        Tree::from_files(
            files
                .iter()
                .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap()
    }

    // Minimal CBOR reader for the checks below.
    #[derive(Debug, Clone, PartialEq)]
    enum V {
        U(u64),
        B(Vec<u8>),
        T(String),
        A(Vec<V>),
        M(Vec<(V, V)>),
    }
    fn item(b: &[u8], off: usize) -> (V, usize) {
        let ib = b[off];
        let (major, info) = (ib >> 5, ib & 31);
        let mut p = off + 1;
        let n: u64 = match info {
            0..=23 => info as u64,
            24 => {
                p += 1;
                b[p - 1] as u64
            }
            25 => {
                p += 2;
                u16::from_be_bytes([b[p - 2], b[p - 1]]) as u64
            }
            26 => {
                p += 4;
                u32::from_be_bytes(b[p - 4..p].try_into().unwrap()) as u64
            }
            27 => {
                p += 8;
                u64::from_be_bytes(b[p - 8..p].try_into().unwrap())
            }
            _ => panic!(),
        };
        match major {
            0 => (V::U(n), p),
            2 => (V::B(b[p..p + n as usize].to_vec()), p + n as usize),
            3 => (
                V::T(String::from_utf8(b[p..p + n as usize].to_vec()).unwrap()),
                p + n as usize,
            ),
            4 => {
                let mut v = vec![];
                for _ in 0..n {
                    let (x, e) = item(b, p);
                    v.push(x);
                    p = e;
                }
                (V::A(v), p)
            }
            5 => {
                let mut v = vec![];
                for _ in 0..n {
                    let (k, e) = item(b, p);
                    let (x, e2) = item(b, e);
                    v.push((k, x));
                    p = e2;
                }
                (V::M(v), p)
            }
            _ => panic!(),
        }
    }

    struct Bundle {
        primary: String,
        section_lengths: Vec<V>,
        index: Vec<(V, V)>,
        index_len: usize,
        resp: Vec<u8>,
        total: u64,
        size: usize,
    }
    fn decode(b: &[u8]) -> Bundle {
        assert_eq!(b[0], 0x85);
        let (magic, p) = item(b, 1);
        assert_eq!(magic, V::B(MAGIC.to_vec()));
        let (version, p) = item(b, p);
        assert_eq!(version, V::B(b"b2\0\0".to_vec()));
        let (sl, mut p) = item(b, p);
        let V::B(sl) = sl else { panic!() };
        let V::A(section_lengths) = item(&sl, 0).0 else {
            panic!()
        };
        let names: Vec<String> = section_lengths
            .iter()
            .step_by(2)
            .map(|v| if let V::T(s) = v { s.clone() } else { panic!() })
            .collect();
        assert_eq!(b[p], 0x80 + names.len() as u8);
        p += 1;
        let mut primary = String::new();
        let mut index = vec![];
        let mut index_len = 0;
        let mut resp = vec![];
        for name in &names {
            let (v, e) = item(b, p);
            match (name.as_str(), v) {
                ("index", V::M(m)) => {
                    index = m;
                    index_len = e - p;
                }
                ("primary", V::T(t)) => primary = t,
                ("responses", _) => resp = b[p..e].to_vec(),
                _ => panic!(),
            }
            p = e;
        }
        assert_eq!(names.last().unwrap(), "responses");
        let (len, e) = item(b, p);
        assert_eq!(e, b.len());
        let V::B(len) = len else { panic!() };
        Bundle {
            primary,
            section_lengths,
            index,
            index_len,
            resp,
            total: u64::from_be_bytes(len.try_into().unwrap()),
            size: b.len(),
        }
    }

    #[test]
    fn structure_index_responses_primary() {
        let t = mk(&[
            ("/index.html", "<h1>home</h1>"),
            ("/about/index.html", "<h1>about</h1>"),
            ("/css/site.css", "p{}"),
            ("/img/a b.png", "PNG"),
            ("/data.json", "{\"a\":1}"),
            ("/blob", "raw"),
        ]);
        let w = decode(&write_web_bundle(&t, Some("https://example.com/site/")).unwrap());
        assert_eq!(w.primary, "https://example.com/site/");
        assert_eq!(w.total, w.size as u64);
        assert_eq!(
            w.section_lengths,
            vec![
                V::T("index".into()),
                V::U(w.index_len as u64),
                V::T("primary".into()),
                V::U(2 + 25),
                V::T("responses".into()),
                V::U(w.resp.len() as u64)
            ]
        );
        let mut urls: Vec<String> = w
            .index
            .iter()
            .map(|(k, _)| if let V::T(s) = k { s.clone() } else { panic!() })
            .collect();
        urls.sort();
        assert_eq!(
            urls,
            [
                "https://example.com/site/",
                "https://example.com/site/about/",
                "https://example.com/site/about/index.html",
                "https://example.com/site/blob",
                "https://example.com/site/css/site.css",
                "https://example.com/site/data.json",
                "https://example.com/site/img/a%20b.png",
                "https://example.com/site/index.html",
            ]
        );
        let get = |u: &str| {
            w.index
                .iter()
                .find(|(k, _)| *k == V::T(u.into()))
                .unwrap()
                .1
                .clone()
        };
        for (url, loc) in &w.index {
            let V::T(url) = url else { panic!() };
            let V::A(loc) = loc else { panic!() };
            assert_eq!(loc.len(), 2);
            let (V::U(offset), V::U(length)) = (&loc[0], &loc[1]) else {
                panic!()
            };
            let (r, end) = item(&w.resp, *offset as usize);
            assert_eq!((end - *offset as usize) as u64, *length);
            let V::A(r) = r else { panic!() };
            let (V::B(hb), V::B(payload)) = (&r[0], &r[1]) else {
                panic!()
            };
            let V::M(h) = item(hb, 0).0 else { panic!() };
            assert_eq!(h[0], (V::B(b":status".to_vec()), V::B(b"200".to_vec())));
            let rel = &url["https://example.com/site/".len()..];
            let file = if url.ends_with('/') {
                format!("/{rel}index.html")
            } else {
                format!("/{}", rel.replace("%20", " "))
            };
            assert_eq!(payload.as_slice(), &*t.get(&file).unwrap().bytes, "{url}");
            let want = mime_type_for(&file);
            assert_eq!(h[1].1, V::B(want.as_bytes().to_vec()));
        }
        assert_eq!(
            get("https://example.com/site/"),
            get("https://example.com/site/index.html")
        );
    }

    #[test]
    fn no_index_no_primary_deterministic_and_many() {
        let t = mk(&[("/a.txt", "a")]);
        assert_eq!(
            decode(&write_web_bundle(&t, Some("https://example.com/")).unwrap()).primary,
            ""
        );
        let empty =
            decode(&write_web_bundle(&Tree::empty(), Some("https://example.com/")).unwrap());
        assert_eq!(empty.resp, vec![0x80]);
        let files = (0..300).map(|i| {
            (
                format!("/f{i:03}.txt"),
                make_file(format!("file {i}").into_bytes(), "t", 0o644),
            )
        });
        let w = decode(
            &write_web_bundle(
                &Tree::from_files(files).unwrap(),
                Some("https://example.com/"),
            )
            .unwrap(),
        );
        assert_eq!(w.index.len(), 300);
        assert_eq!(&w.resp[..3], &[0x99, 0x01, 0x2c]);
        let V::A(loc) = &w
            .index
            .iter()
            .find(|(k, _)| *k == V::T("https://example.com/f299.txt".into()))
            .unwrap()
            .1
        else {
            panic!()
        };
        let V::U(off) = loc[0] else { panic!() };
        let V::A(r) = item(&w.resp, off as usize).0 else {
            panic!()
        };
        assert_eq!(r[1], V::B(b"file 299".to_vec()));
    }

    #[test]
    fn base_url_validation() {
        for bad in [
            None,
            Some(""),
            Some("example.com/"),
            Some("http://example.com/"),
            Some("https://example.com"),
            Some("https://u:p@example.com/"),
            Some("https://example.com/?q=1"),
            Some("https://example.com/#x"),
        ] {
            assert_eq!(
                write_web_bundle(&Tree::empty(), bad).unwrap_err().code,
                E_VALIDATION,
                "{bad:?}"
            );
        }
        assert_eq!(
            validate_base_url(Some("https://example.com/a/")).unwrap(),
            "https://example.com/a/"
        );
        assert_eq!(
            validate_base_url(Some("HTTPS://Example.COM:443/a/")).unwrap(),
            "https://example.com/a/"
        );
        assert_eq!(
            validate_base_url(Some("https://example.com:8443/a b/")).unwrap(),
            "https://example.com:8443/a%20b/"
        );
        assert_eq!(
            validate_base_url(Some("https://example.com/a/../b/")).unwrap(),
            "https://example.com/b/"
        );
        assert_eq!(
            validate_base_url(Some("http://x/")).unwrap_err().detail,
            "invalid base URL 'http://x/': must be https"
        );
        assert_eq!(
            validate_base_url(Some("https://")).unwrap_err().detail,
            "invalid base URL 'https://': not an absolute URL"
        );
    }

    #[test]
    fn mime_table() {
        for (p, m) in [
            ("/a.html", "text/html; charset=utf-8"),
            ("/a.CSS", "text/css; charset=utf-8"),
            ("/a.js", "text/javascript; charset=utf-8"),
            ("/a.mjs", "text/javascript; charset=utf-8"),
            ("/a.svg", "image/svg+xml; charset=utf-8"),
            ("/a.txt", "text/plain; charset=utf-8"),
            ("/a.webmanifest", "application/manifest+json; charset=utf-8"),
            ("/a.jpeg", "image/jpeg"),
            ("/a.wasm", "application/wasm"),
            ("/a.woff2", "font/woff2"),
            ("/a.mp4", "video/mp4"),
            ("/a.mp3", "audio/mpeg"),
            ("/a.unknown", "application/octet-stream"),
            ("/.hidden", "application/octet-stream"),
            ("/dir.d/noext", "application/octet-stream"),
        ] {
            assert_eq!(mime_type_for(p), m, "{p}");
        }
        assert_eq!(encode_uri_component("a b/é!*'()~"), "a%20b%2F%C3%A9!*'()~");
    }

    /// Byte-for-byte against the TypeScript writer (fixtures made with lib/src/wbn.js).
    #[test]
    fn identical_to_typescript_writer() {
        let t = mk(&[
            ("/index.html", "<h1>home</h1>"),
            ("/about/index.html", "<h1>about</h1>"),
            ("/css/site.css", "p{}"),
            ("/img/a b.png", "PNG"),
            ("/ünï/cødé.txt", "unicode"),
            ("/blob", "raw"),
        ]);
        let got = write_web_bundle(&t, Some("https://example.com/site/")).unwrap();
        assert_eq!(
            crate::digest::sha256(&got),
            WBN_FIXTURE_DIGEST,
            "wbn bytes differ from the TypeScript writer"
        );
        assert_eq!(got.len(), WBN_FIXTURE_LEN);
        let files = (0..300).map(|i| {
            (
                format!("/f{i:03}.txt"),
                make_file(format!("file {i}").into_bytes(), "t", 0o644),
            )
        });
        let big = write_web_bundle(
            &Tree::from_files(files).unwrap(),
            Some("https://example.com/"),
        )
        .unwrap();
        assert_eq!(crate::digest::sha256(&big), WBN_FIXTURE_300_DIGEST);
    }
    const WBN_FIXTURE_DIGEST: &str =
        "sha256:b235707145eabe9d02dca1f8a7ad249fb93f286d42f55c45b65a772b881491e6";
    const WBN_FIXTURE_LEN: usize = 799;
    const WBN_FIXTURE_300_DIGEST: &str =
        "sha256:fb0c6fbd1d2d1878a071d0452c3944450fbc846da05bcb2e636aaf3f8f76d015";
}
