//! Frontmatter parser (port of `src/frontmatter.ts`). The block between the
//! `---` lines is parsed with the YAML subset documented in [`crate::yaml`]
//! (nested maps and lists, flow collections, block scalars, quoted strings,
//! ...), but it must be a mapping at the top level.
//!
//! Frontmatter exists only when the text starts (after an optional BOM) with a
//! `---` line. It ends at the next line that is exactly `---` or `...`.
//!
//! Every error is reported as E_FRONTMATTER with the file line and column.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::yaml::parse_yaml_document;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFrontmatter {
    pub data: Map<String, Value>,
    pub body: String,
    /// 1-based line of the file on which `body` starts.
    pub body_line: u32,
}

pub fn parse_frontmatter(text: &str, file: &str) -> Result<ParsedFrontmatter> {
    let src = text.strip_prefix('\u{feff}').unwrap_or(text);

    // Split into lines, remembering where each starts so the body can be sliced.
    let mut lines: Vec<&str> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut pos = 0;
    while pos <= src.len() {
        let nl = src[pos..].find('\n').map_or(src.len(), |k| pos + k);
        starts.push(pos);
        let l = &src[pos..nl];
        lines.push(l.strip_suffix('\r').unwrap_or(l));
        pos = nl + 1;
    }

    if lines[0] != "---" {
        return Ok(ParsedFrontmatter { data: Map::new(), body: text.to_string(), body_line: 1 });
    }

    let Some(close) = lines.iter().skip(1).position(|l| *l == "---" || *l == "...").map(|k| k + 1) else {
        return Err(SitefileError::new(
            E_FRONTMATTER,
            "unterminated frontmatter: missing closing '---'",
            Some(SourceSpan::new(file, 1, 1)),
        ));
    };

    let data = parse_block(&lines[1..close].join("\n"), file)?;
    let body_start = starts.get(close + 1).copied().unwrap_or(src.len());
    Ok(ParsedFrontmatter { data, body: src[body_start..].to_string(), body_line: close as u32 + 2 })
}

fn parse_block(block: &str, file: &str) -> Result<Map<String, Value>> {
    // The block starts on file line 2 (line 1 is the opening '---').
    match parse_yaml_document(block, file, 2, true) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Ok(Map::new()),
        Err(e) if e.code == E_DATA => Err(SitefileError::new(E_FRONTMATTER, e.detail, e.span)),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fm(t: &str) -> ParsedFrontmatter {
        parse_frontmatter(t, "post.md").unwrap()
    }
    fn data(t: &str) -> Value {
        Value::Object(fm(t).data)
    }
    fn fails(t: &str, line: u32, column: u32, needle: &str) {
        let e = parse_frontmatter(t, "post.md").unwrap_err();
        assert_eq!(e.code, E_FRONTMATTER, "{t:?}: {e}");
        assert_eq!(e.span, Some(SourceSpan::new("post.md", line, column)), "{t:?}: {e}");
        assert!(needle.split('|').any(|n| e.detail.contains(n)), "{}", e.detail);
    }

    #[test]
    fn no_frontmatter_returns_text_untouched() {
        let r = fm("# Hello\n---\nx\n");
        assert!(r.data.is_empty());
        assert_eq!(r.body, "# Hello\n---\nx\n");
        assert_eq!(r.body_line, 1);
        assert_eq!(fm(""), ParsedFrontmatter { data: Map::new(), body: String::new(), body_line: 1 });
        assert_eq!(fm("--- x\na: 1\n---\n").body_line, 1);
        assert_eq!(fm("\n---\na: 1\n---\n").body_line, 1);
    }

    #[test]
    fn scalars_and_body_line() {
        let r = fm("---\ntitle: Hello World\ncount: 3\nratio: -1.5\ndraft: false\npub: true\nnone: null\ntilde: ~\nempty:\n---\nBody\n");
        assert_eq!(
            Value::Object(r.data),
            json!({ "title": "Hello World", "count": 3, "ratio": -1.5, "draft": false, "pub": true, "none": null, "tilde": null, "empty": null })
        );
        assert_eq!(r.body, "Body\n");
        assert_eq!(r.body_line, 11);
        assert_eq!(data("---\ndate: 2026-01-02\nv: 1.2.3\nn: 12abc\n---\n"), json!({ "date": "2026-01-02", "v": "1.2.3", "n": "12abc" }));
    }

    #[test]
    fn quoting_comments_lists() {
        assert_eq!(
            data("---\na: \"x \\\"y\\\" \\\\ \\n\\t z\"\nb: 'it''s'\nc: \"true\"\nd: '42'\ne: \"a # b\"\n---\n"),
            json!({ "a": "x \"y\" \\ \n\t z", "b": "it's", "c": "true", "d": "42", "e": "a # b" })
        );
        assert_eq!(
            data("---\n# a comment\n\ntitle: Hi # trailing\nurl: http://x.com/#frag\nq: \"v\" # note\n---\n"),
            json!({ "title": "Hi", "url": "http://x.com/#frag", "q": "v" })
        );
        assert_eq!(
            data("---\ntags: [a, \"b c\", 3, true, 'x', null]\nnone: []\ntrail: [a, b,]\n---\n"),
            json!({ "tags": ["a", "b c", 3, true, "x", null], "none": [], "trail": ["a", "b"] })
        );
        assert_eq!(
            data("---\ntags:\n  - one\n  - \"two words\"\n  # c\n  - 3\nnext: x\nflat:\n- a\n- b\n---\n"),
            json!({ "tags": ["one", "two words", 3], "next": "x", "flat": ["a", "b"] })
        );
    }

    #[test]
    fn crlf_bom_and_closers() {
        let r = fm("\u{feff}---\r\ntitle: Hi\r\ntags: [a]\r\n---\r\nBody\r\nmore");
        assert_eq!(Value::Object(r.data), json!({ "title": "Hi", "tags": ["a"] }));
        assert_eq!(r.body, "Body\r\nmore");
        assert_eq!(r.body_line, 5);
        let r = fm("---\na: 1\n...\nB");
        assert_eq!((Value::Object(r.data), r.body.as_str(), r.body_line), (json!({ "a": 1 }), "B", 4));
        let r = fm("---\n---");
        assert_eq!((r.data.is_empty(), r.body.as_str(), r.body_line), (true, "", 3));
        let r = fm("---\n---\n");
        assert_eq!((r.data.is_empty(), r.body.as_str(), r.body_line), (true, "", 3));
        assert_eq!(fm("---\na: 1\n---\nx\n---\ny").body, "x\n---\ny");
    }

    #[test]
    fn keys_and_proto() {
        assert_eq!(data("---\n_a-b_1: http://x.com:80/p\n---\n"), json!({ "_a-b_1": "http://x.com:80/p" }));
        let r = fm("---\n__proto__: x\n---\n");
        assert_eq!(r.data.keys().collect::<Vec<_>>(), ["__proto__"]);
        assert_eq!(r.data["__proto__"], json!("x"));
    }

    #[test]
    fn nested_structures() {
        assert_eq!(data("---\na:\n  b: 1\n  c: [x, {y: 2}]\n---\n"), json!({ "a": { "b": 1, "c": ["x", { "y": 2 }] } }));
        assert_eq!(
            data("---\na: {b: 1}\nl:\n  - [1]\n  - n: 1\n    m: 2\nt: [1, [2]]\n---\n"),
            json!({ "a": { "b": 1 }, "l": [[1], { "n": 1, "m": 2 }], "t": [1, [2]] })
        );
        assert_eq!(data("---\na: |\n  x\n  y\nb: >-\n  p\n  q\n1bad: x\n---\n"), json!({ "a": "x\ny\n", "b": "p q", "1bad": "x" }));
    }

    #[test]
    fn differential_fixtures() {
        crate::render::fixtures::check("frontmatter.json", |t| {
            let r = parse_frontmatter(t, "p.md")?;
            Ok(json!([Value::Object(r.data), r.body, r.body_line]))
        });
    }

    #[test]
    fn errors() {
        fails("---\na: 1\n", 1, 1, "unterminated");
        fails("---\na: 1\nb: 2\na: 3\n---\n", 4, 1, "duplicate key 'a'");
        fails("---\na:\n  b: 1\n  b: 2\n---\n", 4, 3, "duplicate key 'b'");
        fails("---\na:\n\tb: 1\n---\n", 3, 1, "tab");
        fails("---\na: b: c\n---\n", 2, 4, "nested maps");
        fails("---\n  x: 1\n---\n", 2, 3, "indentation");
        fails("---\na: &x 1\n---\n", 2, 4, "anchors");
        fails("---\na: *x\n---\n", 2, 4, "anchors");
        fails("---\njust text\n---\n", 2, 1, "key: value");
        fails("---\n- orphan\n---\n", 2, 1, "without a preceding key");
        fails("---\na: x\n- y\n---\n", 3, 1, "without a preceding key|list");
        fails("---\na: \"open\n---\n", 2, 4, "unterminated quoted");
        fails("---\na: \"x\" y\n---\n", 2, 8, "unexpected text");
        fails("---\na: \"bad \\q\"\n---\n", 2, 9, "escape");
        fails("---\na: [1, 2\n---\n", 2, 4, "unterminated list");
        fails("---\na: [1,,2]\n---\n", 2, 7, "empty list item");
        fails("---\na: [1] x\n---\n", 2, 8, "unexpected text");
    }
}
