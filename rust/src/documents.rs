//! Markdown and data documents (port of `src/resources/documents.ts`).
//!
//! A Markdown document as templates see it is an object with its frontmatter
//! fields plus
//!
//! - `slug`: the file name without `.md` and without a leading `YYYY-MM-DD-`
//!   (frontmatter `slug` wins)
//! - `date`: that leading date, if the frontmatter has no `date`
//! - `path`: the file's path inside the import (`/2026-01-02-hello.md`)
//! - `content`: the body rendered to HTML (insert it with `${post.content|raw}`)
//! - `excerpt`: plain text of the first paragraph (up to ~200 characters)
//!
//! `path`, `content` and `excerpt` are always computed; frontmatter keys with
//! those names are ignored. Keys are in JavaScript object order (the TS version
//! builds these with object spread).

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::frontmatter::parse_frontmatter;
use crate::memo::DocMemo;
use crate::markdown::render_markdown;
use crate::render::{decode_utf8, js};
use crate::tree::Tree;
use crate::values::js_number;
use crate::yaml::parse_yaml;
use serde_json::{Map, Value};
use std::cmp::Ordering;

pub type MarkdownDocument = Map<String, Value>;

/// Which files a [`data_collection`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    /// `.json` files, parsed as JSON.
    Json,
    /// `.yaml` / `.yml` files, parsed with [`crate::yaml`].
    Yaml,
}

/// `posix.basename`.
fn basename(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

/// Remove a case-insensitive ASCII suffix (one of `exts`, each with its dot).
fn strip_ext<'a>(name: &'a str, exts: &[&str]) -> Option<&'a str> {
    for e in exts {
        if name.len() >= e.len() && name.is_char_boundary(name.len() - e.len()) && name[name.len() - e.len()..].eq_ignore_ascii_case(e) {
            return Some(&name[..name.len() - e.len()]);
        }
    }
    None
}

const MD_EXTS: [&str; 2] = [".md", ".markdown"];

/// `^(\d{4}-\d{2}-\d{2})-(.+)$`: (date, rest).
fn dated(stem: &str) -> Option<(&str, &str)> {
    let b = stem.as_bytes();
    if b.len() < 12 {
        return None;
    }
    let shape = b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    if !shape || b[10] != b'-' {
        return None;
    }
    // bytes 0..=10 are ASCII, so 11 is a char boundary
    let rest = &stem[11..];
    if rest.chars().any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')) {
        return None;
    }
    Some((&stem[..10], rest))
}

/// `^[A-Za-z0-9][A-Za-z0-9._~-]*$`
fn is_safe_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._~-".contains(c))
}

/// `JSON.stringify(v)` for an error message (`undefined` when missing).
fn json_stringify(v: Option<&Value>) -> String {
    v.map_or_else(|| "undefined".to_string(), |v| v.to_string())
}

/// `{ slug, date? }` from a file stem.
fn base_fields(stem: &str) -> MarkdownDocument {
    let mut doc = Map::new();
    match dated(stem) {
        Some((date, rest)) => {
            doc.insert("slug".into(), Value::String(rest.into()));
            doc.insert("date".into(), Value::String(date.into()));
        }
        None => {
            doc.insert("slug".into(), Value::String(stem.into()));
        }
    }
    doc
}

/// Parse one Markdown file into a document (a JSON object). `label` names it
/// in errors.
pub fn markdown_document(bytes: &[u8], path: &str, label: &str, span: Option<&SourceSpan>) -> Result<Value> {
    markdown_map(bytes, path, label, span).map(Value::Object)
}

fn markdown_map(bytes: &[u8], path: &str, label: &str, span: Option<&SourceSpan>) -> Result<MarkdownDocument> {
    let text = decode_utf8(bytes, &format!("'{label}'"), span)?;
    let fm = parse_frontmatter(&text, label)?;
    let md = render_markdown(&fm.body);
    let name = basename(path);
    let stem = strip_ext(name, &MD_EXTS).unwrap_or(name);
    let mut doc = base_fields(stem);
    for (k, v) in fm.data {
        doc.insert(k, v);
    }
    doc.insert("path".into(), Value::String(path.into()));
    doc.insert("content".into(), Value::String(md.html));
    doc.insert("excerpt".into(), Value::String(md.excerpt));
    let doc = js::object_order(doc);
    match doc.get("slug") {
        Some(Value::String(s)) if is_safe_slug(s) => Ok(doc),
        slug => Err(SitefileError::new(
            E_VALIDATION,
            format!(
                "'{label}': slug {} must be letters, digits, '.', '_', '~' or '-' (it becomes part of a URL)",
                json_stringify(slug)
            ),
            span.cloned(),
        )),
    }
}

/// Every `.md` / `.markdown` file in a tree, as documents, ordered by path.
/// `memo` (incremental rebuilds) reuses a document whose bytes, path and label are unchanged.
pub fn markdown_collection(tree: &Tree, label_prefix: &str, span: Option<&SourceSpan>, memo: Option<DocMemo>) -> Result<Vec<MarkdownDocument>> {
    let mut docs = Vec::new();
    for (p, node) in tree.entries() {
        if strip_ext(p, &MD_EXTS).is_none() {
            continue;
        }
        let label = format!("{label_prefix}{p}");
        let mut make = || markdown_map(&node.bytes, p, &label, span);
        docs.push(match memo {
            Some(m) => m(&["md", &node.digest, p, &label], &mut make)?,
            None => make()?,
        });
    }
    Ok(docs)
}

/// Every `.json` (`DataKind::Json`) or `.yaml`/`.yml` (`DataKind::Yaml`) file in
/// a tree, as a list ordered by path. An object becomes
/// `{ slug, date?, path, ...fields }` (slug and date from the file name as for
/// Markdown, overridable by the data); any other value becomes
/// `{ slug, path, value }`.
pub fn data_collection(tree: &Tree, kind: DataKind, label_prefix: &str, span: Option<&SourceSpan>, memo: Option<DocMemo>) -> Result<Vec<MarkdownDocument>> {
    let exts: &[&str] = match kind {
        DataKind::Json => &[".json"],
        DataKind::Yaml => &[".yaml", ".yml"],
    };
    let mut docs = Vec::new();
    for (p, node) in tree.entries() {
        if strip_ext(p, exts).is_none() {
            continue;
        }
        let label = format!("{label_prefix}{p}");
        let mut make = || data_document(&node.bytes, kind, exts, p, &label, span);
        let tag = match kind {
            DataKind::Json => "json",
            DataKind::Yaml => "yaml",
        };
        docs.push(match memo {
            Some(m) => m(&[tag, &node.digest, p, &label], &mut make)?,
            None => make()?,
        });
    }
    Ok(docs)
}

fn data_document(bytes: &[u8], kind: DataKind, exts: &[&str], p: &String, label: &str, span: Option<&SourceSpan>) -> Result<MarkdownDocument> {
    let text = decode_utf8(bytes, &format!("'{label}'"), span)?;
    let value = match kind {
        DataKind::Yaml => parse_yaml(&text, &label)?,
        // The detail after the colon is serde_json's message, not V8's.
        DataKind::Json => match serde_json::from_str::<Value>(&text) {
            Ok(v) => js::from_json_parse(v),
            Err(e) => return Err(SitefileError::new(E_TYPE, format!("{label} is not valid JSON: {e}"), span.cloned())),
        },
    };
    let name = basename(p);
    let stem = strip_ext(name, exts).unwrap_or(name);
    let mut doc = base_fields(stem);
    match value {
        Value::Object(fields) => {
            for (k, v) in fields {
                doc.insert(k, v);
            }
        }
        other => {
            doc.insert("value".into(), other);
        }
    }
    doc.insert("path".into(), Value::String(p.clone()));
    let doc = js::object_order(doc);
    match doc.get("slug") {
        Some(Value::String(s)) if is_safe_slug(s) => {}
        slug => {
            return Err(SitefileError::new(
                E_VALIDATION,
                format!("'{label}': slug {} must be letters, digits, '.', '_', '~' or '-'", json_stringify(slug)),
                span.cloned(),
            ))
        }
    }
    Ok(doc)
}

/// `^-?\d+(\.\d+)?([eE][+-]?\d+)?$`
fn is_numeric(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let s = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > s
    };
    if !digits(&mut i) {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

/// `String(v)` as JavaScript would print it for sorting.
fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js_number(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = js::trim_str(s);
            if is_numeric(t) {
                t.parse::<f64>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Numbers, and numeric strings such as CSV cells, compare numerically;
/// everything else as strings (UTF-16 order). `None` stands for a NaN result,
/// which JavaScript's sort treats as "equal" without the path tiebreak.
fn cmp_values(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        return x.partial_cmp(&y);
    }
    Some(js::cmp_utf16(&js_string(Some(a)), &js_string(Some(b))))
}

fn by_path(a: &Value, b: &Value) -> Ordering {
    js::cmp_utf16(&js_string(a.get("path")), &js_string(b.get("path")))
}

/// Sort documents by a field. Numbers and numeric strings compare numerically,
/// everything else as strings (ISO dates sort correctly). Documents missing the
/// field (or with null) go last in either direction; ties keep path order.
///
/// Uses a stable merge sort that tolerates an inconsistent comparator (mixed
/// numbers and strings), where `slice::sort_by` may panic.
pub fn sort_documents(docs: Vec<Value>, key: &str, descending: bool) -> Vec<Value> {
    let cmp = |a: &Value, b: &Value| -> Ordering {
        let va = a.get(key).filter(|v| !v.is_null());
        let vb = b.get(key).filter(|v| !v.is_null());
        match (va, vb) {
            (None, None) => by_path(a, b),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => match cmp_values(x, y) {
                None => Ordering::Equal,
                Some(Ordering::Equal) => by_path(a, b),
                Some(c) if descending => c.reverse(),
                Some(c) => c,
            },
        }
    };
    merge_sort(docs, &cmp)
}

fn merge_sort<T>(v: Vec<T>, cmp: &dyn Fn(&T, &T) -> Ordering) -> Vec<T> {
    if v.len() <= 1 {
        return v;
    }
    let mut left = v;
    let right = left.split_off(left.len() / 2);
    let left = merge_sort(left, cmp);
    let right = merge_sort(right, cmp);
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut l = left.into_iter().peekable();
    let mut r = right.into_iter().peekable();
    loop {
        let take_right = match (l.peek(), r.peek()) {
            (Some(a), Some(b)) => cmp(b, a) == Ordering::Less,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => break,
        };
        let next = if take_right { r.next() } else { l.next() };
        out.extend(next);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::make_file;
    use serde_json::json;

    fn tree(files: &[(&str, &str)]) -> Tree {
        Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644)))).unwrap()
    }
    fn paths(docs: &[Value]) -> Vec<&str> {
        docs.iter().map(|d| d["path"].as_str().unwrap()).collect()
    }

    #[test]
    fn sort_numbers_strings_missing_last_path_tiebreak() {
        let docs = vec![
            json!({ "path": "/c", "date": "2026-01-02" }),
            json!({ "path": "/a" }),
            json!({ "path": "/b", "date": "2026-03-01" }),
            json!({ "path": "/d", "date": "2026-01-02" }),
        ];
        assert_eq!(paths(&sort_documents(docs.clone(), "date", true)), ["/b", "/c", "/d", "/a"]);
        assert_eq!(paths(&sort_documents(docs, "date", false)), ["/c", "/d", "/b", "/a"]);
        let n = vec![json!({ "path": "/x", "n": 10 }), json!({ "path": "/y", "n": 9 })];
        assert_eq!(paths(&sort_documents(n, "n", false)), ["/y", "/x"]);
        // CSV cells: numeric strings sort numerically
        let csv = vec![json!({ "path": "/b", "age": "9" }), json!({ "path": "/a", "age": "10" }), json!({ "path": "/c", "age": "100" })];
        assert_eq!(paths(&sort_documents(csv, "age", true)), ["/c", "/a", "/b"]);
        // null counts as missing; mixed types never panic
        let mixed = vec![
            json!({ "path": "/1", "k": 2 }),
            json!({ "path": "/2", "k": "10" }),
            json!({ "path": "/3", "k": "1a" }),
            json!({ "path": "/4", "k": null }),
            json!({ "path": "/5", "k": [1, 2] }),
        ];
        let out = sort_documents(mixed, "k", false);
        assert_eq!(out.len(), 5);
        assert_eq!(out[4]["path"], "/4");
    }

    #[test]
    fn markdown_documents_from_a_tree() {
        let t = tree(&[
            ("/2026-01-05-hello.md", "---\ntitle: Hello\ntags: [a, b]\n---\n\nFirst **post**.\n"),
            ("/2026-02-01-second.md", "---\ntitle: Second & more\nslug: two\n---\nBody <b>two</b>.\n"),
            ("/notes.txt", "not markdown"),
            ("/sub/About.MARKDOWN", "Hi."),
        ]);
        let docs: Vec<Value> = markdown_collection(&t, "./posts", None, None).unwrap().into_iter().map(Value::Object).collect();
        assert_eq!(docs.len(), 3);
        assert_eq!(
            docs[0],
            json!({ "slug": "hello", "date": "2026-01-05", "title": "Hello", "tags": ["a", "b"], "path": "/2026-01-05-hello.md",
                    "content": "<p>First <strong>post</strong>.</p>\n", "excerpt": "First post." })
        );
        assert_eq!(docs[0].as_object().unwrap().keys().collect::<Vec<_>>(), ["slug", "date", "title", "tags", "path", "content", "excerpt"]);
        assert_eq!(docs[1]["slug"], "two");
        assert_eq!(docs[1]["content"], "<p>Body &lt;b&gt;two&lt;/b&gt;.</p>\n");
        assert_eq!(docs[1]["excerpt"], "Body <b>two</b>.");
        assert_eq!(docs[2]["slug"], "About");
        assert!(docs[2].get("date").is_none());
        let sorted = sort_documents(docs, "date", true);
        assert_eq!(paths(&sorted), ["/2026-02-01-second.md", "/2026-01-05-hello.md", "/sub/About.MARKDOWN"]);
    }

    #[test]
    fn computed_fields_win_and_frontmatter_date_wins() {
        let doc = markdown_document(b"---\npath: /x\ncontent: c\ndate: 2020-01-01\n2: n\n---\nHi", "/2026-01-01-a.md", "p:/a", None).unwrap();
        assert_eq!(doc["path"], "/2026-01-01-a.md");
        assert_eq!(doc["content"], "<p>Hi</p>\n");
        assert_eq!(doc["date"], "2020-01-01");
        assert_eq!(doc.as_object().unwrap().keys().collect::<Vec<_>>(), ["2", "slug", "date", "path", "content", "excerpt"]);
    }

    #[test]
    fn markdown_errors() {
        let span = SourceSpan::new("Sitefile", 2, 1);
        let e = markdown_document(b"---\nslug: ../evil\n---\n", "/a.md", "./p/a.md", Some(&span)).unwrap_err();
        assert_eq!(e.code, E_VALIDATION);
        assert_eq!(e.detail, "'./p/a.md': slug \"../evil\" must be letters, digits, '.', '_', '~' or '-' (it becomes part of a URL)");
        assert_eq!(e.span, Some(span));
        let e = markdown_document(b"---\nslug: 3\n---\n", "/a.md", "a", None).unwrap_err();
        assert!(e.detail.contains("slug 3 must"));
        let e = markdown_document(b"---\ntitle: ok\nalias: &anchor value\n---\n", "/a.md", "./p/a.md", None).unwrap_err();
        assert_eq!(e.code, E_FRONTMATTER);
        assert_eq!(e.span, Some(SourceSpan::new("./p/a.md", 3, 8)));
        assert_eq!(markdown_document(&[0xff], "/a.md", "a", None).unwrap_err().code, E_ENCODING);
    }

    #[test]
    fn data_collections() {
        let t = tree(&[
            ("/2026-01-02-alpha.json", "{\"title\":\"Alpha\",\"rank\":2}"),
            ("/beta.json", "{\"title\":\"Beta\",\"rank\":1,\"slug\":\"b\"}"),
            ("/readme.txt", "ignored"),
            ("/a.yaml", "title: A\n"),
            ("/b.yml", "- 1\n- 2\n"),
        ]);
        let items: Vec<Value> = data_collection(&t, DataKind::Json, "./items", None, None).unwrap().into_iter().map(Value::Object).collect();
        assert_eq!(items[0], json!({ "slug": "alpha", "date": "2026-01-02", "title": "Alpha", "rank": 2, "path": "/2026-01-02-alpha.json" }));
        let items = sort_documents(items, "rank", false);
        assert_eq!(paths(&items), ["/beta.json", "/2026-01-02-alpha.json"]);
        assert_eq!(items[0]["slug"], "b");
        let cfg: Vec<Value> = data_collection(&t, DataKind::Yaml, "./cfg", None, None).unwrap().into_iter().map(Value::Object).collect();
        assert_eq!(cfg, vec![json!({ "slug": "a", "title": "A", "path": "/a.yaml" }), json!({ "slug": "b", "value": [1, 2], "path": "/b.yml" })]);
        assert_eq!(cfg[1].as_object().unwrap().keys().collect::<Vec<_>>(), ["slug", "value", "path"]);

        let bad = tree(&[("/x.json", "{nope")]);
        let e = data_collection(&bad, DataKind::Json, "./d", None, None).unwrap_err();
        assert_eq!(e.code, E_TYPE);
        assert!(e.detail.starts_with("./d/x.json is not valid JSON: "));
        let bad = tree(&[("/x.yaml", "a: &x 1\nb: *x\n")]);
        assert_eq!(data_collection(&bad, DataKind::Yaml, "./d", None, None).unwrap_err().code, E_DATA);
        let bad = tree(&[("/x.json", "{\"slug\": \"a b\"}")]);
        let e = data_collection(&bad, DataKind::Json, "./d", None, None).unwrap_err();
        assert_eq!(e.detail, "'./d/x.json': slug \"a b\" must be letters, digits, '.', '_', '~' or '-'");
    }

    #[test]
    fn slug_and_date_rules() {
        assert_eq!(dated("2026-01-02-hello"), Some(("2026-01-02", "hello")));
        assert_eq!(dated("2026-01-02-"), None);
        assert_eq!(dated("2026-1-02-x"), None);
        assert_eq!(dated("2026-01-02é"), None);
        assert_eq!(dated("2026-01-02-é"), Some(("2026-01-02", "é")));
        assert!(is_safe_slug("a.b_c~d-e9"));
        for s in ["", "-a", ".a", "a b", "a/b", "é"] {
            assert!(!is_safe_slug(s), "{s}");
        }
        assert_eq!(basename("/a/b/c.md"), "c.md");
    }
}
