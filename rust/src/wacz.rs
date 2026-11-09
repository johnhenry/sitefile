//! `EXPORT / AS wacz BASE <url>` (port of `src/wacz.ts`, issue #206): a WACZ 1.1.1 package
//! (Webrecorder), the format ReplayWeb.page opens. The layout is described in `src/wacz.ts` and
//! docs/LANGUAGE.md.

use crate::deflate::gzip;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::archive_writers::{write_zip, WriterOptions};
use crate::export::oci::iso_of;
use crate::language::scanner::{is_js_space, js_trim};
use crate::tree::{make_file, Tree};
use crate::warc::warc_records;
use serde_json::json;
use sha2::{Digest, Sha256};

fn sha256(b: &[u8]) -> String {
    let hex: String = Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// `YYYYMMDDhhmmss`.
fn timestamp14(epoch: Option<u64>) -> String {
    iso_of(epoch)
        .chars()
        .filter(|c| !matches!(c, '-' | ':' | 'T' | 'Z'))
        .collect()
}

/// The SURT form of an http(s) URL: `org,example)/path` (host labels reversed, a leading `www.` dropped).
pub fn surt(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("http://") {
        &url[7..]
    } else if lower.starts_with("https://") {
        &url[8..]
    } else {
        return url.to_string();
    };
    if rest.contains('\n')
        || rest.contains('\r')
        || rest.contains('\u{2028}')
        || rest.contains('\u{2029}')
    {
        return url.to_string();
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].to_lowercase();
    let path = &rest[end..];
    let mut labels: Vec<&str> = host.split('.').collect();
    if labels[0] == "www" && labels.len() > 1 {
        labels.remove(0);
    }
    labels.reverse();
    format!(
        "{}){}",
        labels.join(","),
        if path.is_empty() { "/" } else { path }
    )
}

fn starts_with_ci(s: &str, at: usize, pat: &str) -> bool {
    s.as_bytes()
        .get(at..at + pat.len())
        .is_some_and(|b| b.eq_ignore_ascii_case(pat.as_bytes()))
}

/// `/<title(?:\s[^>]*)?>([^<]*)<\/title>/i`, whitespace runs collapsed and trimmed (as `src/wacz.ts`).
fn title_of(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let s: &str = &text;
    for (i, _) in s.match_indices('<') {
        if !starts_with_ci(s, i, "<title") {
            continue;
        }
        let mut at = i + "<title".len();
        let next = s[at..].chars().next();
        match next {
            Some('>') => {}
            Some(c) if is_js_space(c) => match s[at..].find('>') {
                Some(k) => at += k,
                None => continue,
            },
            _ => continue,
        }
        at += 1; // '>'
        let end = s[at..].find('<').map_or(s.len(), |k| at + k);
        if !starts_with_ci(s, end, "</title>") {
            continue;
        }
        let mut out = String::new();
        let mut space = false;
        for c in s[at..end].chars() {
            if is_js_space(c) {
                space = true;
            } else {
                if space {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
        if space {
            out.push(' ');
        }
        let t = js_trim(&out);
        return if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    None
}

/// `EXPORT … AS wacz`: the WACZ file.
pub fn write_wacz(tree: &Tree, base_url: Option<&str>, epoch: Option<u64>) -> Result<Vec<u8>> {
    if !matches!(base_url, Some(b) if !b.is_empty()) {
        return Err(SitefileError::plain(
            E_VALIDATION,
            "wacz export needs a base URL: use 'EXPORT <path> AS wacz BASE https://example.com/' or --base-url",
        ));
    }
    let records = warc_records(tree, base_url, epoch)?;
    let ts = timestamp14(epoch);
    let created = iso_of(epoch);
    let mut warc: Vec<u8> = Vec::new();
    let mut cdx: Vec<String> = Vec::new();
    let mut pages: Vec<String> = Vec::new();
    for r in &records {
        let member = gzip(&r.bytes);
        if let Some(res) = &r.response {
            let mime = js_trim(res.ty.split(';').next().unwrap_or(""));
            let entry = json!({
                "url": res.uri, "mime": mime, "status": "200", "digest": res.digest,
                "length": member.len(), "offset": warc.len(), "filename": "data.warc.gz",
            });
            cdx.push(format!("{} {ts} {entry}", surt(&res.uri)));
            // An index page once, by its directory URL; other HTML pages by their own.
            let index = res.path == "/index.html" || res.path.ends_with("/index.html");
            if res.ty.starts_with("text/html") && (!index || res.uri.ends_with('/')) {
                let mut page = json!({ "id": &sha256(res.uri.as_bytes())[7..39], "url": res.uri, "ts": created });
                if let Some(title) = tree.get(&res.path).and_then(|n| title_of(&n.bytes)) {
                    page["title"] = json!(title);
                }
                pages.push(page.to_string());
            }
        }
        warc.extend_from_slice(&member);
    }
    // JavaScript's default sort: UTF-16 code units.
    cdx.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut pages_jsonl = json!({ "format": "json-pages-1.0", "id": "pages", "title": "All Pages" })
        .to_string() + "\n";
    for p in &pages {
        pages_jsonl.push_str(p);
        pages_jsonl.push('\n');
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("archive/data.warc.gz", warc),
        (
            "indexes/index.cdx",
            cdx.iter()
                .map(|l| format!("{l}\n"))
                .collect::<String>()
                .into_bytes(),
        ),
        ("pages/pages.jsonl", pages_jsonl.into_bytes()),
    ];
    let resources: Vec<_> = files
        .iter()
        .map(|(path, bytes)| {
            json!({
                "name": &path[path.rfind('/').map_or(0, |i| i + 1)..],
                "path": path, "hash": sha256(bytes), "bytes": bytes.len(),
            })
        })
        .collect();
    let datapackage = serde_json::to_string_pretty(&json!({
        "profile": "data-package", "resources": resources, "wacz_version": "1.1.1",
        "software": "sitefile", "created": created,
    }))
    .expect("JSON")
        + "\n";
    let digest = serde_json::to_string_pretty(
        &json!({ "path": "datapackage.json", "hash": sha256(datapackage.as_bytes()) }),
    )
    .expect("JSON")
        + "\n";
    let mut out: Vec<(String, crate::tree::Node)> = files
        .into_iter()
        .map(|(p, b)| (format!("/{p}"), make_file(b, "wacz", 0o644)))
        .collect();
    out.push((
        "/datapackage.json".into(),
        make_file(datapackage.into_bytes(), "wacz", 0o644),
    ));
    out.push((
        "/datapackage-digest.json".into(),
        make_file(digest.into_bytes(), "wacz", 0o644),
    ));
    write_zip(
        &Tree::from_files(out)?,
        &WriterOptions {
            epoch,
            dirs: Vec::new(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surt_reverses_the_host() {
        assert_eq!(surt("https://www.Example.com/a/b?x"), "com,example)/a/b?x");
        assert_eq!(surt("http://example.org"), "org,example)/");
        assert_eq!(surt("ftp://x"), "ftp://x");
    }

    #[test]
    fn package_holds_the_gzipped_warc_and_is_deterministic() {
        use crate::language::ast::OciCompression;
        let tree = Tree::from_files([
            (
                "/index.html".to_string(),
                make_file(b"<title>Home</title>".to_vec(), "t", 0o644),
            ),
            ("/a.css".to_string(), make_file(b"p{}".to_vec(), "t", 0o644)),
        ])
        .unwrap();
        let base = Some("https://example.org/");
        let a = write_wacz(&tree, base, Some(1_700_000_000)).unwrap();
        assert_eq!(a, write_wacz(&tree, base, Some(1_700_000_000)).unwrap());
        let warc =
            crate::warc::write_warc(&tree, base, Some(1_700_000_000), Some(OciCompression::Gzip))
                .unwrap();
        // The gzipped WARC is stored as is (it does not deflate smaller).
        assert!(a.windows(warc.len()).any(|w| w == warc.as_slice()));
        assert!(write_wacz(&tree, None, None)
            .unwrap_err()
            .to_string()
            .contains("wacz export needs a base URL"));
    }

    #[test]
    fn titles() {
        assert_eq!(
            title_of(b"<html><TITLE lang=en>  A \n b </title>").as_deref(),
            Some("A b")
        );
        assert_eq!(
            title_of(b"<titlex>no</titlex><title>yes</title>").as_deref(),
            Some("yes")
        );
        assert_eq!(title_of(b"<title> </title>"), None);
        assert_eq!(title_of(b"<p>none"), None);
    }
}
