//! A Web Bundle as template data (port of `src/resources/bundle-data.ts`):
//!
//! ```text
//! { version, primaryUrl, entries: [ { url, path, status, contentType,
//!   headers, size, content? } ] }
//! ```
//!
//! `path` is the URL's pathname; `content` is set only for textual responses
//! (text/*, JSON, XML, JavaScript, SVG) whose body is valid UTF-8. Binary
//! bodies are not exposed as data: use `AS tree` and COPY to place them.

use crate::resources::wbn_reader::{split_scheme, WebBundle};
use serde_json::{Map, Value};

pub fn bundle_value(bundle: &WebBundle) -> Value {
    let entries: Vec<Value> = bundle
        .entries
        .iter()
        .map(|e| {
            let content_type = e.header("content-type").unwrap_or("").to_string();
            let mut o = Map::new();
            o.insert("url".into(), Value::String(e.url.clone()));
            o.insert("path".into(), Value::String(path_of(&e.url)));
            o.insert("status".into(), Value::from(e.status));
            o.insert("contentType".into(), Value::String(content_type.clone()));
            let headers: Map<String, Value> = e.headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
            o.insert("headers".into(), Value::Object(headers));
            o.insert("size".into(), Value::from(e.body.len()));
            if is_textual(&content_type) {
                if let Ok(s) = std::str::from_utf8(&e.body) {
                    // TextDecoder strips a leading BOM.
                    o.insert("content".into(), Value::String(s.strip_prefix('\u{feff}').unwrap_or(s).to_string()));
                }
            }
            Value::Object(o)
        })
        .collect();
    let mut out = Map::new();
    out.insert("version".into(), Value::String(bundle.version.into()));
    out.insert("primaryUrl".into(), bundle.primary_url.clone().map(Value::String).unwrap_or(Value::Null));
    out.insert("entries".into(), Value::Array(entries));
    Value::Object(out)
}

/// `/^(text\/|application\/(json|xml|javascript|ecmascript|manifest\+json|[a-z0-9.+-]*\+(json|xml))|image\/svg\+xml)/i`
fn is_textual(content_type: &str) -> bool {
    let t = content_type.trim().to_ascii_lowercase();
    if t.starts_with("text/") || t.starts_with("image/svg+xml") {
        return true;
    }
    let Some(rest) = t.strip_prefix("application/") else { return false };
    if ["json", "xml", "javascript", "ecmascript", "manifest+json"].iter().any(|p| rest.starts_with(p)) {
        return true;
    }
    let run = rest.bytes().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'+' | b'-')).count();
    (0..=run).any(|i| rest[i..].starts_with("+json") || rest[i..].starts_with("+xml"))
}

/// `new URL(url).pathname`, or the URL itself if it does not parse.
fn path_of(url: &str) -> String {
    let Some((scheme, rest)) = split_scheme(url) else { return url.to_string() };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "http" || scheme == "https" {
        return match ureq::get(url).request_url() {
            Ok(u) => u.path().to_string(),
            Err(_) => url.to_string(),
        };
    }
    // Opaque or non-special URLs: the path runs up to the query or fragment.
    let rest = match rest.strip_prefix("//") {
        Some(r) => &r[r.find('/').unwrap_or(r.len())..],
        None => rest,
    };
    rest[..rest.find(['?', '#']).unwrap_or(rest.len())].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::wbn_reader::BundleEntry;
    use std::sync::Arc;

    fn entry(url: &str, ct: &str, body: &[u8]) -> BundleEntry {
        BundleEntry { url: url.into(), status: 200, headers: vec![("content-type".into(), ct.into())], body: Arc::from(body) }
    }

    #[test]
    fn shape_and_textual_content() {
        let b = WebBundle {
            version: "b2",
            primary_url: None,
            entries: vec![
                entry("https://x.test/a%20b.css?q=1", "text/css", b"a{}"),
                entry("https://x.test/i.png", "image/png", b"\x89PNG"),
                entry("https://x.test/bad.txt", "text/plain", b"\xff"),
                entry("https://x.test/m.json", "application/ld+json", b"{}"),
            ],
        };
        let v = bundle_value(&b);
        assert_eq!(v["version"], "b2");
        assert_eq!(v["primaryUrl"], Value::Null);
        let e = &v["entries"];
        assert_eq!(e[0]["path"], "/a%20b.css");
        assert_eq!(e[0]["status"], 200);
        assert_eq!(e[0]["contentType"], "text/css");
        assert_eq!(e[0]["headers"]["content-type"], "text/css");
        assert_eq!(e[0]["size"], 3);
        assert_eq!(e[0]["content"], "a{}");
        assert!(e[1].get("content").is_none());
        assert!(e[2].get("content").is_none());
        assert_eq!(e[3]["content"], "{}");
        let keys: Vec<&String> = e[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["url", "path", "status", "contentType", "headers", "size", "content"]);
    }

    #[test]
    fn textual_types() {
        for t in ["text/html", " TEXT/plain", "application/json", "application/javascript", "application/manifest+json", "application/vnd.api+json", "application/atom+xml", "image/svg+xml"] {
            assert!(is_textual(t), "{t}");
        }
        for t in ["", "image/png", "application/octet-stream", "application/wasm"] {
            assert!(!is_textual(t), "{t}");
        }
    }

    #[test]
    fn paths_of_urls() {
        assert_eq!(path_of("https://x.test"), "/");
        assert_eq!(path_of("https://x.test/a/b?x#y"), "/a/b");
        assert_eq!(path_of("data:text/plain,hi"), "text/plain,hi");
        assert_eq!(path_of("not a url"), "not a url");
    }
}
