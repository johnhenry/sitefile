//! A small, safe YAML subset parser (port of `src/yaml.ts`; no dependencies,
//! no code execution).
//!
//! Supported:
//! - Block style by indentation (spaces only; a tab in indentation is an error).
//! - Mappings `key: value`, nested mappings, sequences `- item`, sequences of
//!   mappings (`- name: a` / `  url: /a`), mappings of sequences (including
//!   the indentless form), arbitrary nesting up to 64 levels.
//! - Scalars: "double" quoted (escapes \" \\ \n \t), 'single' quoted ('' escape),
//!   plain strings (trimmed; ` #` begins a comment), `true`/`false`, integers
//!   and decimals (numbers), `null` / `~` / empty (null). Dates such as
//!   2026-01-02 stay strings.
//! - Flow collections on a single line: `[a, b, {x: 1}]`, `{a: 1, b: [2, 3]}`.
//! - Block scalars `|` (literal) and `>` (folded) with optional `-` / `+`
//!   chomping; the indentation comes from the first content line.
//! - Comments, blank lines, an optional leading `---` and a trailing `...`.
//! - Keys are plain or quoted scalars and are always strings. Duplicate keys
//!   are an error. A `__proto__` key is ordinary data.
//! - An empty document is `null`.
//!
//! Not supported (E_DATA at the offending line and column): anchors and aliases
//! (`&` `*`), tags (`!`), multiple documents, complex keys (`? `), merge keys
//! (`<<`), multi-line flow collections, multi-line plain or quoted scalars,
//! block scalar indentation indicators, tabs for indentation.
//!
//! Numbers are JavaScript doubles: integral values in the safe range are stored
//! as JSON integers, others as floats (see [`crate::render::js::number`]).
//! Objects use JavaScript own-key order (integer-like keys first). Columns
//! count UTF-16 code units, as in the TypeScript version.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::render::js;
use serde_json::{Map, Value};

const MAX_DEPTH: usize = 64;

type W = Vec<u16>;

const SP: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;

const fn u(b: u8) -> u16 {
    b as u16
}

/// Parse a YAML document. `file` names it in errors.
pub fn parse_yaml(text: &str, file: &str) -> Result<Value> {
    parse_yaml_document(text, file, 1, false)
}

/// Like [`parse_yaml`]; `first_line` is the 1-based file line of the first line
/// of `text` (for YAML embedded in another file). With `root_mapping` the
/// document must be a mapping (or empty) and indented content at the top is
/// rejected. Used by frontmatter.
pub fn parse_yaml_document(
    text: &str,
    file: &str,
    first_line: u32,
    root_mapping: bool,
) -> Result<Value> {
    let units = js::u16s(text);
    let src: &[u16] = if units.first() == Some(&0xfeff) {
        &units[1..]
    } else {
        &units
    };
    let base = first_line.saturating_sub(1);
    let mut lines: Vec<Line> = src
        .split(|&c| c == u(b'\n'))
        .enumerate()
        .map(|(i, r)| {
            let raw = r.strip_suffix(&[u(b'\r')]).unwrap_or(r).to_vec();
            let indent = raw.iter().take_while(|&&c| c == SP).count();
            Line {
                no: i as u32 + 1,
                text: raw[indent..].to_vec(),
                raw,
                indent,
                parent: -1,
            }
        })
        .collect();
    let fail_at = |line: &Line, detail: &str, column: usize| -> SitefileError {
        SitefileError::new(
            E_DATA,
            detail,
            Some(SourceSpan::new(file, line.no + base, column as u32)),
        )
    };

    // Document markers.
    let mut started = false;
    let mut saw_start = false;
    let mut end = lines.len();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].indent == 0 {
            let t = js::trim_end(&lines[i].raw);
            let dashes = [u(b'-'); 3];
            if t == dashes || lines[i].raw.starts_with(&[u(b'-'), u(b'-'), u(b'-'), SP]) {
                if t != dashes {
                    return Err(fail_at(
                        &lines[i],
                        "content on the document marker line is not supported",
                        5,
                    ));
                }
                if started || saw_start {
                    return Err(fail_at(
                        &lines[i],
                        "multiple documents are not supported",
                        1,
                    ));
                }
                saw_start = true;
                lines[i].text.clear();
                i += 1;
                continue;
            }
            if t == [u(b'.'); 3] {
                for l in &lines[i + 1..] {
                    let rest = js::trim(&l.text);
                    if !rest.is_empty() && rest[0] != u(b'#') {
                        return Err(fail_at(l, "multiple documents are not supported", 1));
                    }
                }
                end = i;
                break;
            }
        }
        let tt = js::trim(&lines[i].text);
        if !started && !tt.is_empty() && tt[0] != u(b'#') {
            started = true;
        }
        i += 1;
    }
    lines.truncate(end);

    let mut p = Parser {
        lines,
        pos: 0,
        depth: 0,
        base,
        file,
    };
    let Some(first) = p.peek()? else {
        return Ok(Value::Null);
    };
    let fi = p.lines[first].indent;
    if fi > 0 && root_mapping {
        return Err(p.fail(first, "unexpected indentation", fi + 1));
    }
    if root_mapping {
        if is_item(&p.lines[first].text) {
            return Err(p.fail(first, "list item without a preceding key", fi + 1));
        }
        if p.try_key(first)?.is_none() {
            return Err(p.fail(first, "expected 'key: value'", fi + 1));
        }
    }
    let value = p.node_at(first)?;
    if let Some(extra) = p.peek()? {
        // `first` may have been re-indented while parsing (a sequence item).
        let (ei, fi) = (p.lines[extra].indent, p.lines[first].indent);
        let msg = if ei > fi {
            "unexpected indentation"
        } else {
            "unexpected content"
        };
        return Err(p.fail(extra, msg, ei + 1));
    }
    Ok(value)
}

struct Line {
    /// 1-based line within the parsed text.
    no: u32,
    raw: W,
    /// Column offset (0-based) of `text` within the line.
    indent: usize,
    text: W,
    /// Indent of the enclosing block parent (for block scalars).
    parent: isize,
}

/// `/^-(?:[ \t]|$)/`
fn is_item(t: &[u16]) -> bool {
    t.first() == Some(&u(b'-')) && (t.len() == 1 || t[1] == SP || t[1] == TAB)
}

/// `/^<c>(?:[ \t]|$)/`
fn starts_word(t: &[u16], c: u8) -> bool {
    t.first() == Some(&u(c)) && (t.len() == 1 || t[1] == SP || t[1] == TAB)
}

/// Index of the first `:` followed by space, tab or end (`/:(?:[ \t]|$)/`).
fn colon_index(t: &[u16]) -> Option<usize> {
    (0..t.len())
        .find(|&p| t[p] == u(b':') && (p + 1 == t.len() || t[p + 1] == SP || t[p + 1] == TAB))
}

/// `/(^|[ \t])#/` match index.
fn comment_index(t: &[u16]) -> Option<usize> {
    if t.first() == Some(&u(b'#')) {
        return Some(0);
    }
    (0..t.len().saturating_sub(1)).find(|&p| (t[p] == SP || t[p] == TAB) && t[p + 1] == u(b'#'))
}

fn strip_comment(t: &[u16]) -> &[u16] {
    match comment_index(t) {
        Some(i) => &t[..i],
        None => t,
    }
}

fn starts_with_any(t: &[u16], set: &[u8]) -> bool {
    t.first().is_some_and(|&c| set.iter().any(|&b| c == u(b)))
}

/// `-?\d+` or `-?\d+\.\d+` (ASCII digits).
fn is_number(p: &[u16]) -> bool {
    let digits = |s: &[u16]| !s.is_empty() && s.iter().all(|&c| (u(b'0')..=u(b'9')).contains(&c));
    let s = p.strip_prefix(&[u(b'-')]).unwrap_or(p);
    match s.iter().position(|&c| c == u(b'.')) {
        None => digits(s),
        Some(d) => digits(&s[..d]) && digits(&s[d + 1..]),
    }
}

fn plain_scalar(p: &[u16]) -> Value {
    let s = js::string(p);
    match s.as_str() {
        "" | "~" | "null" => Value::Null,
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ if is_number(p) => js::number(s.parse::<f64>().unwrap_or(f64::NAN)),
        _ => Value::String(s),
    }
}

fn fold(lines: &[W]) -> W {
    let mut out: W = Vec::new();
    let mut blanks = 0;
    let mut prev: Option<&W> = None;
    for line in lines {
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        match prev {
            None => {
                out = vec![u(b'\n'); blanks];
                out.extend_from_slice(line);
            }
            Some(p) => {
                let more = starts_with_any(p, b" \t") || starts_with_any(line, b" \t");
                if more {
                    out.extend(std::iter::repeat_n(u(b'\n'), blanks + 1));
                } else if blanks == 0 {
                    out.push(SP);
                } else {
                    out.extend(std::iter::repeat_n(u(b'\n'), blanks));
                }
                out.extend_from_slice(line);
            }
        }
        blanks = 0;
        prev = Some(line);
    }
    out
}

/// A failure at a column of the current line.
struct Fail<'a> {
    file: &'a str,
    line: u32,
}

impl Fail<'_> {
    fn at(&self, detail: impl Into<String>, column: usize) -> SitefileError {
        SitefileError::new(
            E_DATA,
            detail,
            Some(SourceSpan::new(self.file, self.line, column as u32)),
        )
    }
}

/// Only whitespace or a ` #` comment may follow a closed value.
fn assert_trailing(rest: &[u16], col: usize, fail: &Fail) -> Result<()> {
    let r = js::trim(rest);
    if r.is_empty() || (r[0] == u(b'#') && starts_with_any(rest, b" \t")) {
        return Ok(());
    }
    let lead = rest.len() - js::trim_start(rest).len();
    Err(fail.at(
        format!("unexpected text after value: '{}'", js::string(r)),
        col + lead,
    ))
}

/// Parse a quoted string at t[0]; returns the value and the index after it.
fn parse_quoted(t: &[u16], col: usize, fail: &Fail) -> Result<(String, usize)> {
    let q = t[0];
    let mut out: W = Vec::new();
    let mut i = 1;
    while i < t.len() {
        let c = t[i];
        if q == u(b'"') && c == u(b'\\') {
            let n = t.get(i + 1).copied();
            let mapped = match n {
                Some(x) if x == u(b'"') => u(b'"'),
                Some(x) if x == u(b'\\') => u(b'\\'),
                Some(x) if x == u(b'n') => u(b'\n'),
                Some(x) if x == u(b't') => u(b'\t'),
                _ => {
                    let shown = n.map(|x| js::string(&[x])).unwrap_or_default();
                    return Err(fail.at(format!("unsupported escape '\\{shown}'"), col + i));
                }
            };
            out.push(mapped);
            i += 2;
        } else if q == u(b'\'') && c == u(b'\'') && t.get(i + 1) == Some(&u(b'\'')) {
            out.push(u(b'\''));
            i += 2;
        } else if c == q {
            return Ok((js::string(&out), i + 1));
        } else {
            out.push(c);
            i += 1;
        }
    }
    Err(fail.at("unterminated quoted string", col))
}

fn ws(c: Option<&u16>) -> bool {
    matches!(c, Some(&SP) | Some(&TAB))
}

/// Parse a flow collection starting at t[start]. `col` is the column of t[0].
/// Returns the value and the index just after it.
fn parse_flow(
    t: &[u16],
    start: usize,
    col: usize,
    fail: &Fail,
    depth: usize,
) -> Result<(Value, usize)> {
    if depth > MAX_DEPTH {
        return Err(fail.at(
            format!("nesting is deeper than {MAX_DEPTH} levels"),
            col + start,
        ));
    }
    match t.get(start) {
        Some(&c) if c == u(b'[') => parse_flow_seq(t, start, col, fail, depth),
        Some(&c) if c == u(b'{') => parse_flow_map(t, start, col, fail, depth),
        _ => Err(fail.at("expected a flow collection", col + start)),
    }
}

fn parse_flow_seq(
    t: &[u16],
    start: usize,
    col: usize,
    fail: &Fail,
    depth: usize,
) -> Result<(Value, usize)> {
    let open = col + start;
    let unterminated = "unterminated list: missing ']'";
    let mut items: Vec<Value> = Vec::new();
    let mut i = start + 1;
    loop {
        while ws(t.get(i)) {
            i += 1;
        }
        if i >= t.len() {
            return Err(fail.at(unterminated, open));
        }
        if t[i] == u(b']') {
            i += 1;
            break;
        }
        if t[i] == u(b',') {
            return Err(fail.at("empty list item", col + i));
        }
        let (v, ni) = parse_flow_value(t, i, col, fail, depth, open, unterminated, false)?;
        items.push(v);
        i = ni;
        while ws(t.get(i)) {
            i += 1;
        }
        if i >= t.len() {
            return Err(fail.at(unterminated, open));
        }
        if t[i] == u(b',') {
            i += 1;
        } else if t[i] != u(b']') {
            return Err(fail.at("expected ',' or ']'", col + i));
        }
    }
    Ok((Value::Array(items), i))
}

fn parse_flow_map(
    t: &[u16],
    start: usize,
    col: usize,
    fail: &Fail,
    depth: usize,
) -> Result<(Value, usize)> {
    let open = col + start;
    let unterminated = "unterminated map: missing '}'";
    let mut map: Map<String, Value> = Map::new();
    let mut i = start + 1;
    loop {
        while ws(t.get(i)) {
            i += 1;
        }
        if i >= t.len() {
            return Err(fail.at(unterminated, open));
        }
        if t[i] == u(b'}') {
            i += 1;
            break;
        }
        if t[i] == u(b',') {
            return Err(fail.at("empty map entry", col + i));
        }

        // key
        let key_col = col + i;
        let key: String;
        if t[i] == u(b'"') || t[i] == u(b'\'') {
            let (k, len) = parse_quoted(&t[i..], col + i, fail)?;
            key = k;
            i += len;
            while ws(t.get(i)) {
                i += 1;
            }
            if t.get(i) != Some(&u(b':')) {
                return Err(fail.at("expected ':' after key", col + i));
            }
            i += 1;
        } else {
            let mut j = i;
            while j < t.len() && t[j] != u(b',') && t[j] != u(b'}') && t[j] != u(b']') {
                if t[j] == u(b':')
                    && (j + 1 >= t.len() || b" \t,}]".iter().any(|&b| t[j + 1] == u(b)))
                {
                    break;
                }
                j += 1;
            }
            if j >= t.len() {
                return Err(fail.at(unterminated, open));
            }
            if t[j] != u(b':') {
                return Err(fail.at("expected ':' after key", key_col));
            }
            let k = js::trim(&t[i..j]);
            if k.is_empty() {
                return Err(fail.at("empty key", key_col));
            }
            if starts_with_any(k, b"&*![{") {
                return Err(fail.at(
                    "anchors, aliases, tags and complex keys are not supported",
                    key_col,
                ));
            }
            key = js::string(k);
            if key == "<<" {
                return Err(fail.at("merge keys are not supported", key_col));
            }
            i = j + 1;
        }
        if map.contains_key(&key) {
            return Err(fail.at(format!("duplicate key '{key}'"), key_col));
        }

        // value
        while ws(t.get(i)) {
            i += 1;
        }
        if i >= t.len() {
            return Err(fail.at(unterminated, open));
        }
        let mut value = Value::Null;
        if t[i] != u(b',') && t[i] != u(b'}') {
            let (v, ni) = parse_flow_value(t, i, col, fail, depth, open, unterminated, true)?;
            value = v;
            i = ni;
        }
        map.insert(key, value);
        while ws(t.get(i)) {
            i += 1;
        }
        if i >= t.len() {
            return Err(fail.at(unterminated, open));
        }
        if t[i] == u(b',') {
            i += 1;
        } else if t[i] != u(b'}') {
            return Err(fail.at("expected ',' or '}'", col + i));
        }
    }
    Ok((Value::Object(js::object_order(map)), i))
}

#[allow(clippy::too_many_arguments)]
fn parse_flow_value(
    t: &[u16],
    i: usize,
    col: usize,
    fail: &Fail,
    depth: usize,
    open: usize,
    unterminated: &str,
    in_map: bool,
) -> Result<(Value, usize)> {
    let c = t[i];
    if c == u(b'[') || c == u(b'{') {
        return parse_flow(t, i, col, fail, depth + 1);
    }
    if c == u(b'"') || c == u(b'\'') {
        let (v, len) = parse_quoted(&t[i..], col + i, fail)?;
        return Ok((Value::String(v), i + len));
    }
    let mut j = i;
    while j < t.len() && t[j] != u(b',') && t[j] != u(b']') && t[j] != u(b'}') {
        j += 1;
    }
    if j >= t.len() {
        return Err(fail.at(unterminated, open));
    }
    let raw = js::trim(&t[i..j]);
    if starts_with_any(raw, b"&*!") {
        return Err(fail.at("anchors, aliases and tags are not supported", col + i));
    }
    // `/:(?:\s|$)/` (JavaScript whitespace) or `/^[|>]/`
    let nested =
        (0..raw.len()).any(|p| raw[p] == u(b':') && (p + 1 == raw.len() || js::is_ws(raw[p + 1])));
    if nested || starts_with_any(raw, b"|>") {
        let msg = if in_map {
            "nested maps on one line are not supported"
        } else {
            "unsupported list item"
        };
        return Err(fail.at(msg, col + i));
    }
    Ok((plain_scalar(raw), j))
}

struct Parser<'f> {
    lines: Vec<Line>,
    pos: usize,
    depth: usize,
    base: u32,
    file: &'f str,
}

impl<'f> Parser<'f> {
    fn fail(&self, line: usize, detail: impl Into<String>, column: usize) -> SitefileError {
        self.failer(line).at(detail, column)
    }

    fn failer(&self, line: usize) -> Fail<'f> {
        Fail {
            file: self.file,
            line: self.lines[line].no + self.base,
        }
    }

    /// Next significant line (skipping blanks and comments); leaves pos on it.
    fn peek(&mut self) -> Result<Option<usize>> {
        while self.pos < self.lines.len() {
            let l = &self.lines[self.pos];
            let t = js::trim(&l.text);
            if t.is_empty() || t[0] == u(b'#') {
                self.pos += 1;
                continue;
            }
            if l.text.first() == Some(&TAB) {
                return Err(self.fail(
                    self.pos,
                    "tabs are not allowed for indentation",
                    l.indent + 1,
                ));
            }
            return Ok(Some(self.pos));
        }
        Ok(None)
    }

    fn enter(&mut self, line: usize) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.fail(
                line,
                format!("nesting is deeper than {MAX_DEPTH} levels"),
                self.lines[line].indent + 1,
            ));
        }
        Ok(())
    }

    /// Parse the next node if it is indented at least `min_indent`, else null.
    fn parse_node(&mut self, min_indent: usize) -> Result<Value> {
        match self.peek()? {
            Some(l) if self.lines[l].indent >= min_indent => self.node_at(l),
            _ => Ok(Value::Null),
        }
    }

    /// Parse the node starting on `line` (which must be the current line).
    fn node_at(&mut self, line: usize) -> Result<Value> {
        if is_item(&self.lines[line].text) {
            return self.parse_seq(self.lines[line].indent);
        }
        if self.try_key(line)?.is_some() {
            return self.parse_map(self.lines[line].indent);
        }
        let parent = self.lines[line].parent;
        self.inline_value(line, 0, parent)
    }

    fn parse_seq(&mut self, n: usize) -> Result<Value> {
        self.enter(self.pos)?;
        let mut out: Vec<Value> = Vec::new();
        while let Some(line) = self.peek()? {
            let ind = self.lines[line].indent;
            if ind < n {
                break;
            }
            if ind > n {
                return Err(self.fail(line, "unexpected indentation", ind + 1));
            }
            if !is_item(&self.lines[line].text) {
                break;
            }
            let text = &self.lines[line].text;
            let spaces = text[1..].iter().take_while(|&&c| c == SP).count();
            let rest = text[1 + spaces..].to_vec();
            if rest.is_empty() || (spaces > 0 && rest[0] == u(b'#')) {
                self.pos += 1;
                out.push(self.parse_node(n + 1)?);
            } else {
                let l = &mut self.lines[line];
                l.indent = n + 1 + spaces;
                l.text = rest;
                l.parent = n as isize;
                out.push(self.node_at(line)?);
            }
        }
        self.depth -= 1;
        Ok(Value::Array(out))
    }

    fn parse_map(&mut self, n: usize) -> Result<Value> {
        self.enter(self.pos)?;
        let mut map: Map<String, Value> = Map::new();
        while let Some(line) = self.peek()? {
            let ind = self.lines[line].indent;
            if ind < n {
                break;
            }
            if ind > n {
                return Err(self.fail(line, "unexpected indentation", ind + 1));
            }
            if is_item(&self.lines[line].text) {
                return Err(self.fail(line, "list item without a preceding key", ind + 1));
            }
            let Some((key, value_idx)) = self.try_key(line)? else {
                return Err(self.fail(line, "expected 'key: value'", ind + 1));
            };
            if map.contains_key(&key) {
                return Err(self.fail(line, format!("duplicate key '{key}'"), ind + 1));
            }
            let trimmed = js::trim(&self.lines[line].text[value_idx..]);
            let value = if trimmed.is_empty() || trimmed[0] == u(b'#') {
                self.pos += 1;
                match self.peek()? {
                    Some(next) if self.lines[next].indent > n => self.node_at(next)?,
                    Some(next)
                        if self.lines[next].indent == n && is_item(&self.lines[next].text) =>
                    {
                        self.parse_seq(n)?
                    }
                    _ => Value::Null,
                }
            } else {
                self.inline_value(line, value_idx, n as isize)?
            };
            map.insert(key, value);
        }
        self.depth -= 1;
        Ok(Value::Object(js::object_order(map)))
    }

    /// If `line` is `key: ...`, return the key and the index just after the
    /// colon; fails for unsupported forms.
    fn try_key(&self, line: usize) -> Result<Option<(String, usize)>> {
        let t = &self.lines[line].text;
        let col = self.lines[line].indent + 1;
        let fail = self.failer(line);
        if t.first() == Some(&u(b'?')) && (t.len() == 1 || js::is_ws(t[1])) {
            return Err(fail.at("complex keys are not supported", col));
        }
        let Some(&c0) = t.first() else {
            return Ok(None);
        };
        if c0 == u(b'"') || c0 == u(b'\'') {
            let (key, end) = parse_quoted(t, col, &fail)?;
            let mut i = end;
            while ws(t.get(i)) {
                i += 1;
            }
            if t.get(i) == Some(&u(b':')) && (i + 1 >= t.len() || t[i + 1] == SP || t[i + 1] == TAB)
            {
                return Ok(Some((key, i + 1)));
            }
            return Ok(None);
        }
        if c0 == u(b'[') || c0 == u(b'{') {
            return Ok(None);
        }
        let head = strip_comment(t);
        let Some(m) = colon_index(head) else {
            return Ok(None);
        };
        let key = js::trim(&head[..m]);
        if key.is_empty() {
            return Err(fail.at("empty key", col));
        }
        if starts_with_any(key, b"&*!") {
            return Err(fail.at("anchors, aliases and tags are not supported", col));
        }
        let key = js::string(key);
        if key == "<<" {
            return Err(fail.at("merge keys are not supported", col));
        }
        Ok(Some((key, m + 1)))
    }

    /// Parse the scalar/flow/block-scalar value that starts at
    /// `line.text[idx]`. Advances pos past everything consumed.
    fn inline_value(&mut self, line: usize, idx: usize, parent_indent: isize) -> Result<Value> {
        let l = &self.lines[line];
        let t = &l.text[idx..];
        let lead = t.len() - js::trim_start(t).len();
        let s: W = t[lead..].to_vec();
        let col = l.indent + idx + lead + 1;
        let fail = self.failer(line);
        let first = s.first().copied();

        if first == Some(u(b'|')) || first == Some(u(b'>')) {
            return self
                .block_scalar(line, &s, col, parent_indent)
                .map(Value::String);
        }
        if first == Some(u(b'[')) || first == Some(u(b'{')) {
            let (v, end) = parse_flow(&s, 0, col, &fail, self.depth + 1)?;
            assert_trailing(&s[end..], col + end, &fail)?;
            self.pos += 1;
            return Ok(v);
        }
        if first == Some(u(b'"')) || first == Some(u(b'\'')) {
            let (v, end) = parse_quoted(&s, col, &fail)?;
            assert_trailing(&s[end..], col + end, &fail)?;
            self.pos += 1;
            return Ok(Value::String(v));
        }
        if starts_with_any(&s, b"&*!") {
            return Err(fail.at("anchors, aliases and tags are not supported", col));
        }
        if starts_word(&s, b'-') {
            return Err(fail.at("a list item is not allowed here", col));
        }
        if starts_word(&s, b'?') {
            return Err(fail.at("complex keys are not supported", col));
        }
        let plain = js::trim(strip_comment(&s));
        if colon_index(plain).is_some() {
            return Err(fail.at("nested maps on one line are not supported", col));
        }
        self.pos += 1;
        Ok(plain_scalar(plain))
    }

    fn block_scalar(
        &mut self,
        line: usize,
        s: &[u16],
        col: usize,
        parent_indent: isize,
    ) -> Result<String> {
        let fail = self.failer(line);
        // /^([|>])([+-]?)([0-9]?)([ \t]+#.*|[ \t]*)$/
        let style = s[0];
        let mut k = 1;
        let chomp = match s.get(k) {
            Some(&c) if c == u(b'+') || c == u(b'-') => {
                k += 1;
                c
            }
            _ => 0,
        };
        let has_indicator = s.get(k).is_some_and(|&c| (u(b'0')..=u(b'9')).contains(&c));
        if has_indicator {
            k += 1;
        }
        let rest = &s[k..];
        let wsn = rest.iter().take_while(|&&c| c == SP || c == TAB).count();
        let header_ok = wsn == rest.len()
            || (wsn > 0
                && rest[wsn] == u(b'#')
                && !rest[wsn..]
                    .iter()
                    .any(|&c| matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)));
        if !header_ok {
            return Err(fail.at("invalid block scalar header", col));
        }
        if has_indicator {
            return Err(fail.at("block scalar indentation indicators are not supported", col));
        }
        self.pos += 1;

        let mut content: Vec<W> = Vec::new();
        let mut block_indent: isize = -1;
        while self.pos < self.lines.len() {
            let raw = &self.lines[self.pos].raw;
            if js::trim(raw).is_empty() {
                content.push(Vec::new());
                self.pos += 1;
                continue;
            }
            let ind = raw.iter().take_while(|&&c| c == SP).count() as isize;
            if block_indent < 0 {
                if ind <= parent_indent {
                    break;
                }
                block_indent = ind;
            } else if ind < block_indent {
                if ind <= parent_indent {
                    break;
                }
                return Err(self.fail(
                    self.pos,
                    "bad indentation in block scalar",
                    ind as usize + 1,
                ));
            }
            content.push(raw[block_indent as usize..].to_vec());
            self.pos += 1;
        }

        let mut end = content.len();
        while end > 0 && content[end - 1].is_empty() {
            end -= 1;
        }
        let trailing = content.len() - end;
        let body = &content[..end];
        if body.is_empty() {
            return Ok(if chomp == u(b'+') {
                "\n".repeat(trailing)
            } else {
                String::new()
            });
        }
        let text = if style == u(b'|') {
            let mut w = Vec::new();
            for (k, l) in body.iter().enumerate() {
                if k > 0 {
                    w.push(u(b'\n'));
                }
                w.extend_from_slice(l);
            }
            w
        } else {
            fold(body)
        };
        let suffix = if chomp == u(b'-') {
            String::new()
        } else if chomp == u(b'+') {
            "\n".repeat(trailing + 1)
        } else {
            "\n".to_string()
        };
        Ok(js::string(&text) + &suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn y(t: &str) -> Value {
        parse_yaml(t, "data.yml").unwrap_or_else(|e| panic!("{t:?}: {e}"))
    }

    fn fails_at(text: &str, line: u32, column: u32, needle: &str, first_line: u32) {
        let e = parse_yaml_document(text, "data.yml", first_line, false).unwrap_err();
        assert_eq!(e.code, E_DATA, "{text:?}");
        assert_eq!(
            e.span,
            Some(SourceSpan::new("data.yml", line, column)),
            "{text:?}: {e}"
        );
        assert!(
            needle.split('|').any(|n| e.detail.contains(n)),
            "{text:?}: {}",
            e.detail
        );
    }
    fn fails(text: &str, line: u32, column: u32, needle: &str) {
        fails_at(text, line, column, needle, 1);
    }

    #[test]
    fn empty_documents_are_null() {
        for t in ["", "\n\n", "# only a comment\n", "---\n", "---\n...\n"] {
            assert_eq!(y(t), Value::Null, "{t:?}");
        }
    }

    #[test]
    fn every_scalar_type() {
        assert_eq!(
            y("s: Hello World\ni: 3\nneg: -7\nd: -1.5\nt: true\nf: false\nn: null\ntilde: ~\nempty:\nstr: 12abc\ndate: 2026-01-02\nver: 1.2.3\nq: \"true\"\nq2: '42'\n"),
            json!({ "s": "Hello World", "i": 3, "neg": -7, "d": -1.5, "t": true, "f": false, "n": null, "tilde": null, "empty": null, "str": "12abc", "date": "2026-01-02", "ver": "1.2.3", "q": "true", "q2": "42" })
        );
        // integral decimals are integers, as JS numbers are
        assert_eq!(
            y("a: 1.0\nb: 007\nc: -0").to_string(),
            r#"{"a":1,"b":7,"c":0}"#
        );
    }

    #[test]
    fn quoting_and_escapes() {
        assert_eq!(
            y("a: \"x \\\"y\\\" \\\\ \\n\\t z\"\nb: 'it''s'\ne: \"a # b\"\nf: 'a: b'\n"),
            json!({ "a": "x \"y\" \\ \n\t z", "b": "it's", "e": "a # b", "f": "a: b" })
        );
        assert_eq!(
            y("\"quoted key\": 1\n'single': 2\n\"a: b\": 3\n"),
            json!({ "quoted key": 1, "single": 2, "a: b": 3 })
        );
    }

    #[test]
    fn top_level_scalars() {
        assert_eq!(y("hello"), json!("hello"));
        assert_eq!(y("42"), json!(42));
        assert_eq!(y("\"x\""), json!("x"));
        assert_eq!(y("[1, 2]"), json!([1, 2]));
    }

    #[test]
    fn comments_and_blank_lines() {
        assert_eq!(
            y("# top\n\na: 1 # trailing\nb: http://x.com/#frag\n\n  # indented comment\nc: \"v\" # note\n"),
            json!({ "a": 1, "b": "http://x.com/#frag", "c": "v" })
        );
    }

    #[test]
    fn nested_mappings_and_sequences() {
        assert_eq!(
            y("site:\n  name: Demo\n  author:\n    first: A\n    last: B\n  tags: x\nother: 1\n"),
            json!({ "site": { "name": "Demo", "author": { "first": "A", "last": "B" }, "tags": "x" }, "other": 1 })
        );
        assert_eq!(
            y("- a\n- 2\n- \"c d\"\n-\n- ~\n"),
            json!(["a", 2, "c d", null, null])
        );
        assert_eq!(
            y("tags:\n  - one\n  # c\n  - two\nflat:\n- a\n- b\nnext: x\n"),
            json!({ "tags": ["one", "two"], "flat": ["a", "b"], "next": "x" })
        );
        assert_eq!(y("- - 1\n  - 2\n- - 3\n"), json!([[1, 2], [3]]));
        assert_eq!(y("-\n  - 1\n  - 2\n"), json!([[1, 2]]));
        let doc = "nav:\n  - name: Home\n    url: /\n  - name: About\n    url: /about\n    sub:\n      - name: Team\n        url: /team\n  -\n    name: Last\n";
        assert_eq!(
            y(doc),
            json!({ "nav": [
                { "name": "Home", "url": "/" },
                { "name": "About", "url": "/about", "sub": [{ "name": "Team", "url": "/team" }] },
                { "name": "Last" }
            ] })
        );
        assert_eq!(
            y("- a: 1\n  b:\n    - x\n    - y\n- c: 2\n"),
            json!([{ "a": 1, "b": ["x", "y"] }, { "c": 2 }])
        );
        assert_eq!(
            y("m:\n  - - a\n    - b\n  - - c\n"),
            json!({ "m": [["a", "b"], ["c"]] })
        );
    }

    #[test]
    fn flow_collections() {
        assert_eq!(
            y("a: [a, \"b c\", 3, true, 'x', null]\nb: []\nc: [a, b,]\nd: {}\n"),
            json!({ "a": ["a", "b c", 3, true, "x", null], "b": [], "c": ["a", "b"], "d": {} })
        );
        assert_eq!(
            y("a: {x: 1, y: [2, 3], z: {k: v}}\nb: [1, [2, [3]], {a: b}]\n"),
            json!({ "a": { "x": 1, "y": [2, 3], "z": { "k": "v" } }, "b": [1, [2, [3]], { "a": "b" }] })
        );
        assert_eq!(
            y("a: {\"q k\": 1, 's': \"v,}\", e: , n: ~}  # c\n"),
            json!({ "a": { "q k": 1, "s": "v,}", "e": null, "n": null } })
        );
        assert_eq!(y("- [1, 2]\n- {a: 1}\n"), json!([[1, 2], { "a": 1 }]));
        assert_eq!(
            y("u: [http://x.com/a, 1]\n"),
            json!({ "u": ["http://x.com/a", 1] })
        );
    }

    #[test]
    fn block_scalars() {
        assert_eq!(
            y("a: |\n  line1\n  line2\n\n  line4\nb: next\n"),
            json!({ "a": "line1\nline2\n\nline4\n", "b": "next" })
        );
        assert_eq!(
            y("a: |\n  x\n    indented\n  y\n"),
            json!({ "a": "x\n  indented\ny\n" })
        );
        assert_eq!(y("a: |   # note\n  hi\n"), json!({ "a": "hi\n" }));
        assert_eq!(
            y("a: |-\n  x\n  y\n\n\nb: 1\n"),
            json!({ "a": "x\ny", "b": 1 })
        );
        assert_eq!(y("a: |\n  x\n\n\nb: 1\n"), json!({ "a": "x\n", "b": 1 }));
        assert_eq!(
            y("a: |+\n  x\n\n\nb: 1\n"),
            json!({ "a": "x\n\n\n", "b": 1 })
        );
        assert_eq!(
            y("a: >-\n  x\n  y\nb: >+\n  p\n\nc: 1\n"),
            json!({ "a": "x y", "b": "p\n\n", "c": 1 })
        );
        assert_eq!(
            y("a: |\nb: >-\nc: 1\n"),
            json!({ "a": "", "b": "", "c": 1 })
        );
        assert_eq!(
            y("a: >\n  one\n  two\n\n  three\n    more\n  four\n"),
            json!({ "a": "one two\nthree\n  more\nfour\n" })
        );
        assert_eq!(y("a: >\n  p\n\n\n  q\n"), json!({ "a": "p\n\nq\n" }));
        assert_eq!(
            y("- |\n  a\n  b\n- >-\n  c\n  d\n- k: |\n    deep\n  j: 1\n"),
            json!(["a\nb\n", "c d", { "k": "deep\n", "j": 1 }])
        );
        assert_eq!(
            y("o:\n  body: |\n    # not a comment\n    key: not a key\n  after: 1\n"),
            json!({ "o": { "body": "# not a comment\nkey: not a key\n", "after": 1 } })
        );
        assert_eq!(y("|\n  top\n"), json!("top\n"));
    }

    #[test]
    fn document_markers() {
        assert_eq!(y("---\na: 1\n"), json!({ "a": 1 }));
        assert_eq!(y("# c\n---\na: 1\n...\n"), json!({ "a": 1 }));
        assert_eq!(y("a: 1\n...\n# trailing comment\n\n"), json!({ "a": 1 }));
        assert_eq!(
            y("---\r\na: 1\r\nb: [x]\r\n"),
            json!({ "a": 1, "b": ["x"] })
        );
        assert_eq!(y("\u{feff}a: 1"), json!({ "a": 1 }));
    }

    #[test]
    fn key_forms_and_js_key_order() {
        let v = y("url: http://x.com:80/p\n1: a\ntrue: b\n\"null\": c\n");
        assert_eq!(
            v,
            json!({ "1": "a", "true": "b", "null": "c", "url": "http://x.com:80/p" })
        );
        assert_eq!(
            v.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["1", "url", "true", "null"]
        );
        assert_eq!(y("key with spaces: v\n"), json!({ "key with spaces": "v" }));
        let r = y("__proto__: x\nnested:\n  __proto__:\n    polluted: true\n");
        assert_eq!(
            r.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["__proto__", "nested"]
        );
        assert_eq!(r["nested"]["__proto__"], json!({ "polluted": true }));
        let f = y("a: {__proto__: 1, b: 2}");
        assert_eq!(
            f["a"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["__proto__", "b"]
        );
    }

    #[test]
    fn duplicate_keys_and_tabs() {
        fails("a: 1\nb: 2\na: 3\n", 3, 1, "duplicate key 'a'");
        fails("x:\n  a: 1\n  a: 2\n", 3, 3, "duplicate key 'a'");
        fails("a: {b: 1, b: 2}\n", 1, 11, "duplicate key 'b'");
        fails("- k: 1\n  k: 2\n", 2, 3, "duplicate");
        fails("a:\n\tb: 1\n", 2, 1, "tab");
        fails("a:\n  b: 1\n \tc: 2\n", 3, 2, "tab");
        assert_eq!(y("a:\t1\n"), json!({ "a": 1 }));
    }

    #[test]
    fn indentation_errors() {
        fails("a: 1\n  b: 2\n", 2, 3, "indentation");
        fails("a:\n    b: 1\n  c: 2\n", 3, 3, "indentation");
        fails("- a\n  - b\n", 2, 3, "indentation|content");
        fails("a: 1\n- b\n", 2, 1, "without a preceding key");
        fails("a: 1\nplain\n", 2, 1, "key: value");
        fails("a: |\n    x\n  y\n", 3, 3, "block scalar");
    }

    #[test]
    fn unsupported_features() {
        fails("a: &x 1\n", 1, 4, "anchors");
        fails("a: *x\n", 1, 4, "anchors");
        fails("a: !!str 1\n", 1, 4, "tags");
        fails("&x a: 1\n", 1, 1, "anchors");
        fails("a: [&x 1]\n", 1, 5, "anchors");
        fails("<<: {a: 1}\n", 1, 1, "merge");
        fails("a: {<<: 1}\n", 1, 5, "merge");
        fails("? a\n: b\n", 1, 1, "complex");
        fails("a: 1\n---\nb: 2\n", 2, 1, "multiple documents");
        fails("---\na: 1\n---\nb: 2\n", 3, 1, "multiple documents");
        fails("a: 1\n...\nb: 2\n", 3, 1, "multiple documents");
        fails("--- text\n", 1, 5, "marker");
        fails("a: b: c\n", 1, 4, "nested maps");
        fails("a: - b\n", 1, 4, "list item");
        fails("a: |2\n  x\n", 1, 4, "indentation indicators");
        fails("a: | junk\n  x\n", 1, 4, "block scalar header");
        fails("a: [a: 1]\n", 1, 5, "unsupported list item");
    }

    #[test]
    fn scalar_and_flow_errors() {
        fails("a: \"open\n", 1, 4, "unterminated quoted");
        fails("a: \"x\" y\n", 1, 8, "unexpected text");
        fails("a: \"bad \\q\"\n", 1, 9, "escape");
        fails("a: [1, 2\n", 1, 4, "unterminated list");
        fails("a: [1, [2]\n", 1, 4, "unterminated list");
        fails("a: {x: 1\n", 1, 4, "unterminated map");
        fails("a: [1,,2]\n", 1, 7, "empty list item");
        fails("a: {x}\n", 1, 5, "':'");
        fails("a: [1] x\n", 1, 8, "unexpected text");
        assert_eq!(y("a: [1 2, x y]"), json!({ "a": ["1 2", "x y"] }));
        let e = parse_yaml("a: \"x\" y\n", "d").unwrap_err();
        assert_eq!(e.detail, "unexpected text after value: 'y'");
        // columns count UTF-16 code units
        fails("😀: \"open\n", 1, 5, "unterminated quoted");
    }

    #[test]
    fn depth_cap() {
        let ok = "[".repeat(60) + &"]".repeat(60);
        assert!(y(&format!("a: {ok}"))["a"].is_array());
        fails(
            &format!("a: {}{}\n", "[".repeat(70), "]".repeat(70)),
            1,
            4 + 63,
            "deeper than 64",
        );
        let deep = (0..70)
            .map(|i| format!("{}k{i}:", "  ".repeat(i)))
            .collect::<Vec<_>>()
            .join("\n")
            + " 1\n";
        let e = parse_yaml(&deep, "d").unwrap_err();
        assert!(e.code == E_DATA && e.detail.contains("deeper than 64"));
        let shallow = (0..30)
            .map(|i| format!("{}k{i}:", "  ".repeat(i)))
            .collect::<Vec<_>>()
            .join("\n")
            + " 1\n";
        assert!(parse_yaml(&shallow, "d").is_ok());
        assert_eq!(
            parse_yaml(&("- ".repeat(70) + "x"), "d").unwrap_err().code,
            E_DATA
        );
    }

    #[test]
    fn first_line_offsets() {
        fails_at("a: 1\nb: [1\n", 11, 4, "unterminated list", 10);
        fails_at("a: 1\na: 2\n", 6, 1, "duplicate", 5);
        fails_at("a:\n\tb\n", 3, 1, "tab", 2);
        fails_at("a: |\n   x\n  y\n", 12, 3, "block scalar", 10);
        fails_at("x:\n  - k: 1\n    k: 2\n", 4, 5, "duplicate", 2);
    }

    #[test]
    fn columns_inside_sequences_of_mappings() {
        fails("- a: 1\n  b: \"open\n", 2, 6, "unterminated quoted");
        fails("x:\n  - a: &z 1\n", 2, 8, "anchors");
    }

    #[test]
    fn differential_fixtures() {
        crate::render::fixtures::check("yaml.json", |t| parse_yaml(t, "f.yml"));
    }

    #[test]
    fn root_mapping() {
        let e = parse_yaml_document("  x: 1\n", "f", 2, true).unwrap_err();
        assert_eq!(
            (e.span.unwrap().line, e.detail.as_str()),
            (2, "unexpected indentation")
        );
        assert!(parse_yaml_document("- a\n", "f", 1, true)
            .unwrap_err()
            .detail
            .contains("without a preceding key"));
        assert!(parse_yaml_document("text\n", "f", 1, true)
            .unwrap_err()
            .detail
            .contains("key: value"));
        assert_eq!(parse_yaml_document("", "f", 1, true).unwrap(), Value::Null);
    }
}
