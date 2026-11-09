//! A TOML subset for host config files (port of `src/toml.ts`, issue #204):
//! the same values and the same errors.
//!
//! Supported: comments, `[table]` and `[[array.of.tables]]` headers (a header
//! path through an array of tables goes into its last element), bare and
//! quoted keys, dotted keys, basic strings (with `\b \t \n \f \r \" \\ \uXXXX
//! \UXXXXXXXX`), literal strings, decimal integers and floats (`_`
//! separators), booleans, arrays and inline tables. Not supported (E_DATA):
//! multi-line strings, dates and times, hex/octal/binary integers, `inf`/`nan`.
//! The result is JSON (objects keep the order keys were written in).

use crate::error::{ErrorCode::*, Result, SitefileError};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Created by a dotted key or as an intermediate of a header path.
    Implicit,
    /// Created (or claimed) by a `[header]`: it may not be defined twice.
    Defined,
    /// An inline table: closed.
    Inline,
}

#[derive(Debug, Clone)]
enum Node {
    Table(Vec<(String, Node)>, Kind),
    /// `true`: created by `[[header]]` (only those take more tables).
    Array(Vec<Node>, bool),
    Leaf(Value),
}

impl Node {
    fn into_value(self) -> Value {
        match self {
            Node::Leaf(v) => v,
            Node::Array(items, _) => {
                Value::Array(items.into_iter().map(Node::into_value).collect())
            }
            Node::Table(entries, _) => {
                let mut m = Map::new();
                for (k, v) in entries {
                    m.insert(k, v.into_value());
                }
                Value::Object(m)
            }
        }
    }
}

fn get_mut<'a>(entries: &'a mut [(String, Node)], k: &str) -> Option<&'a mut Node> {
    entries.iter_mut().find(|(n, _)| n == k).map(|(_, v)| v)
}

struct Parser<'a> {
    s: Vec<char>,
    i: usize,
    line: u32,
    file: &'a str,
}

fn is_bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl<'a> Parser<'a> {
    fn fail<T>(&self, msg: impl AsRef<str>) -> Result<T> {
        Err(SitefileError::plain(
            E_DATA,
            format!("{}:{}: {}", self.file, self.line, msg.as_ref()),
        ))
    }

    fn peek(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn starts_with(&self, t: &str) -> bool {
        t.chars().enumerate().all(|(k, c)| self.peek(k) == Some(c))
    }

    fn ws(&mut self) {
        while matches!(self.peek(0), Some(' ') | Some('\t')) {
            self.i += 1;
        }
    }

    fn comment(&mut self) {
        if self.peek(0) == Some('#') {
            while self.i < self.s.len() && self.peek(0) != Some('\n') {
                self.i += 1;
            }
        }
    }

    fn blank(&mut self) {
        loop {
            self.ws();
            self.comment();
            if self.peek(0) == Some('\r') && self.peek(1) == Some('\n') {
                self.i += 1;
            }
            if self.peek(0) == Some('\n') {
                self.i += 1;
                self.line += 1;
                continue;
            }
            return;
        }
    }

    fn end_of_line(&mut self) -> Result<()> {
        self.ws();
        self.comment();
        if self.peek(0) == Some('\r') && self.peek(1) == Some('\n') {
            self.i += 1;
        }
        if self.i >= self.s.len() {
            return Ok(());
        }
        if self.peek(0) != Some('\n') {
            return self.fail("expected the end of the line");
        }
        Ok(())
    }

    fn quoted(&mut self) -> Result<String> {
        let c = self.peek(0).unwrap_or('"');
        let triple: String = std::iter::repeat_n(c, 3).collect();
        if self.starts_with(&triple) {
            return self.fail("multi-line strings are not supported");
        }
        if c == '"' {
            self.basic()
        } else {
            self.literal()
        }
    }

    fn key(&mut self) -> Result<String> {
        if matches!(self.peek(0), Some('"') | Some('\'')) {
            return self.quoted();
        }
        let start = self.i;
        while self.peek(0).is_some_and(is_bare) {
            self.i += 1;
        }
        if self.i == start {
            return self.fail("expected a key");
        }
        Ok(self.s[start..self.i].iter().collect())
    }

    fn key_path(&mut self) -> Result<Vec<String>> {
        let mut path = vec![self.key()?];
        loop {
            self.ws();
            if self.peek(0) != Some('.') {
                return Ok(path);
            }
            self.i += 1;
            self.ws();
            path.push(self.key()?);
        }
    }

    fn basic(&mut self) -> Result<String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = match self.peek(0) {
                None | Some('\n') => return self.fail("unterminated string"),
                Some(c) => c,
            };
            self.i += 1;
            if c == '"' {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let e = self.peek(0);
            self.i += 1;
            match e {
                Some('b') => out.push('\u{8}'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('f') => out.push('\u{c}'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(u @ ('u' | 'U')) => {
                    let n = if u == 'u' { 4 } else { 8 };
                    let hex: String = self.s
                        [self.i.min(self.s.len())..(self.i + n).min(self.s.len())]
                        .iter()
                        .collect();
                    if hex.chars().count() != n || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                        return self.fail("invalid escape in a string");
                    }
                    let cp = u32::from_str_radix(&hex, 16).unwrap_or(u32::MAX);
                    match char::from_u32(cp) {
                        Some(ch) => out.push(ch),
                        None => return self.fail("invalid escape in a string"),
                    }
                    self.i += n;
                }
                _ => return self.fail("invalid escape in a string"),
            }
        }
    }

    fn literal(&mut self) -> Result<String> {
        self.i += 1;
        let start = self.i;
        loop {
            match self.peek(0) {
                None | Some('\n') => return self.fail("unterminated string"),
                Some('\'') => {
                    let v: String = self.s[start..self.i].iter().collect();
                    self.i += 1;
                    return Ok(v);
                }
                _ => self.i += 1,
            }
        }
    }

    fn value(&mut self) -> Result<Node> {
        match self.peek(0) {
            Some('"') | Some('\'') => return Ok(Node::Leaf(Value::String(self.quoted()?))),
            Some('[') => return self.array(),
            Some('{') => return self.inline_table(),
            _ => {}
        }
        let start = self.i;
        while self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | ':'))
        {
            self.i += 1;
        }
        let word: String = self.s[start..self.i].iter().collect();
        if word == "true" {
            return Ok(Node::Leaf(Value::Bool(true)));
        }
        if word == "false" {
            return Ok(Node::Leaf(Value::Bool(false)));
        }
        if let Some(n) = number(&word) {
            return Ok(Node::Leaf(n));
        }
        let b = word.as_bytes();
        let digits =
            |r: std::ops::Range<usize>| r.clone().all(|k| b.get(k).is_some_and(u8::is_ascii_digit));
        if (digits(0..4)
            && b.get(4) == Some(&b'-')
            && digits(5..7)
            && b.get(7) == Some(&b'-')
            && digits(8..10))
            || (digits(0..2) && b.get(2) == Some(&b':') && digits(3..5))
        {
            return self.fail("dates and times are not supported");
        }
        if word.is_empty() {
            return self.fail("expected a value");
        }
        self.fail(format!("unsupported value '{word}'"))
    }

    fn array(&mut self) -> Result<Node> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            self.blank();
            if self.peek(0) == Some(']') {
                self.i += 1;
                break;
            }
            out.push(self.value()?);
            self.blank();
            match self.peek(0) {
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                Some(']') => {
                    self.i += 1;
                    break;
                }
                _ => return self.fail("expected ',' or ']' in an array"),
            }
        }
        Ok(Node::Array(out, false))
    }

    fn inline_table(&mut self) -> Result<Node> {
        self.i += 1;
        let mut entries: Vec<(String, Node)> = Vec::new();
        self.ws();
        if self.peek(0) == Some('}') {
            self.i += 1;
            return Ok(Node::Table(entries, Kind::Inline));
        }
        loop {
            self.ws();
            let path = self.key_path()?;
            self.ws();
            if self.peek(0) != Some('=') {
                return self.fail("expected '=' after a key");
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            self.assign(&mut entries, &path, v)?;
            self.ws();
            match self.peek(0) {
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                Some('}') => {
                    self.i += 1;
                    break;
                }
                _ => return self.fail("expected ',' or '}' in an inline table"),
            }
        }
        Ok(Node::Table(entries, Kind::Inline))
    }

    /// Set `path` = `value` under `entries`, creating the intermediate tables of a dotted key.
    fn assign(
        &self,
        entries: &mut Vec<(String, Node)>,
        path: &[String],
        value: Node,
    ) -> Result<()> {
        let (last, init) = path.split_last().expect("a key path is never empty");
        let mut t = entries;
        for k in init {
            let exists = get_mut(t, k).is_some();
            if !exists {
                t.push((k.clone(), Node::Table(Vec::new(), Kind::Implicit)));
            }
            match get_mut(t, k) {
                Some(Node::Table(e, Kind::Implicit)) => t = e,
                _ => return self.fail(format!("key '{k}' is already defined")),
            }
        }
        if get_mut(t, last).is_some() {
            return self.fail(format!("key '{last}' is already defined"));
        }
        t.push((last.clone(), value));
        Ok(())
    }

    /// The table a header path names (through the last element of arrays of tables), creating tables.
    fn walk<'n>(
        &self,
        entries: &'n mut Vec<(String, Node)>,
        path: &[String],
    ) -> Result<&'n mut Node> {
        let (k, rest) = path.split_first().expect("header paths are never empty");
        if get_mut(entries, k).is_none() {
            entries.push((k.clone(), Node::Table(Vec::new(), Kind::Implicit)));
        }
        let cur = get_mut(entries, k).expect("just inserted");
        let next: &'n mut Node = match cur {
            Node::Array(items, true) => items.last_mut().expect("a table array has a table"),
            Node::Table(_, Kind::Implicit | Kind::Defined) => cur,
            _ => return self.fail(format!("key '{k}' is already defined")),
        };
        if rest.is_empty() {
            return Ok(next);
        }
        match next {
            Node::Table(e, _) => self.walk(e, rest),
            _ => unreachable!("only tables are walked into"),
        }
    }

    /// The entries of the table `path` names (the root for an empty path).
    fn entries_at<'n>(
        &self,
        root: &'n mut Vec<(String, Node)>,
        path: &[String],
    ) -> Result<&'n mut Vec<(String, Node)>> {
        if path.is_empty() {
            return Ok(root);
        }
        match self.walk(root, path)? {
            Node::Table(e, _) => Ok(e),
            _ => unreachable!("walk returns tables"),
        }
    }

    fn document(&mut self) -> Result<Value> {
        let mut root: Vec<(String, Node)> = Vec::new();
        // The table key/values go into, as a header path (array steps take the last element).
        let mut current: Vec<String> = Vec::new();
        loop {
            self.blank();
            if self.i >= self.s.len() {
                return Ok(Node::Table(root, Kind::Implicit).into_value());
            }
            if self.peek(0) == Some('[') {
                let many = self.peek(1) == Some('[');
                self.i += if many { 2 } else { 1 };
                self.ws();
                let path = self.key_path()?;
                self.ws();
                if !self.starts_with(if many { "]]" } else { "]" }) {
                    return self.fail(if many {
                        "expected ']]'"
                    } else {
                        "expected ']'"
                    });
                }
                self.i += if many { 2 } else { 1 };
                self.end_of_line()?;
                if many {
                    let (k, parent) = path.split_last().expect("never empty");
                    let entries = self.entries_at(&mut root, parent)?;
                    match get_mut(entries, k) {
                        None => entries.push((
                            k.clone(),
                            Node::Array(vec![Node::Table(Vec::new(), Kind::Implicit)], true),
                        )),
                        Some(Node::Array(items, true)) => {
                            items.push(Node::Table(Vec::new(), Kind::Implicit))
                        }
                        Some(_) => return self.fail(format!("key '{k}' is already defined")),
                    }
                } else {
                    let twice = format!("table '{}' is defined twice", path.join("."));
                    match self.walk(&mut root, &path)? {
                        Node::Table(_, kind @ Kind::Implicit) => *kind = Kind::Defined,
                        _ => return self.fail(twice),
                    }
                }
                current = path;
                continue;
            }
            let path = self.key_path()?;
            self.ws();
            if self.peek(0) != Some('=') {
                return self.fail("expected '=' after a key");
            }
            self.i += 1;
            self.ws();
            let line = self.line;
            let v = self.value()?;
            let end = self.line;
            self.line = line;
            let cur = current.clone();
            let entries = self.entries_at(&mut root, &cur)?;
            self.assign(entries, &path, v)?;
            self.line = end;
            self.end_of_line()?;
        }
    }
}

/// A decimal integer or float as TypeScript's `Number(...)` reads it, as JSON.
fn number(word: &str) -> Option<Value> {
    let b = word.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+') | Some(b'-')) {
        i = 1;
    }
    // digits with single underscores between them; no leading zero unless "0"
    let digits = |i: &mut usize, lead_zero_ok: bool| -> bool {
        let start = *i;
        if !b.get(*i).is_some_and(u8::is_ascii_digit) {
            return false;
        }
        if !lead_zero_ok && b[*i] == b'0' {
            *i += 1;
            return !b.get(*i).is_some_and(|c| c.is_ascii_digit() || *c == b'_');
        }
        *i += 1;
        loop {
            match b.get(*i) {
                Some(c) if c.is_ascii_digit() => *i += 1,
                Some(b'_') if b.get(*i + 1).is_some_and(u8::is_ascii_digit) => *i += 2,
                _ => break,
            }
        }
        *i > start
    };
    if !digits(&mut i, false) {
        return None;
    }
    let int_end = i;
    let mut float = false;
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i, true) {
            return None;
        }
        float = true;
    }
    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        if !digits(&mut i, true) {
            return None;
        }
        float = true;
    }
    if i != b.len() {
        return None;
    }
    let clean: String = word.chars().filter(|c| *c != '_').collect();
    let _ = int_end;
    let f: f64 = clean.parse().ok()?;
    if !float {
        if let Ok(n) = clean.parse::<i64>() {
            if (n as f64).abs() <= 9_007_199_254_740_991.0 {
                return Some(Value::from(n));
            }
        }
    }
    Some(crate::render::js::number(f))
}

/// Parse a TOML document (the subset above). Errors are E_DATA `file:line: message`.
pub fn parse_toml(text: &str, file: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    Parser {
        s: text.chars().collect(),
        i: 0,
        line: 1,
        file,
    }
    .document()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(t: &str) -> String {
        serde_json::to_string(&parse_toml(t, "f.toml").unwrap()).unwrap()
    }
    fn bad(t: &str) -> String {
        parse_toml(t, "f.toml").unwrap_err().detail
    }

    #[test]
    fn tables_arrays_and_values() {
        assert_eq!(
            ok("a = 1\nb = \"x\\ty\"\nc = 'lit\\n'\nd = [1, 2_000,\n  3.5, true, # c\n]\ne = { f = \"g\", h.i = false }\n[t.u]\nk = -2\n[[r]]\nfrom = \"/a\"\n[r.values]\nX = \"1\"\n[[r]]\nfrom = \"/b\"\n"),
            r#"{"a":1,"b":"x\ty","c":"lit\\n","d":[1,2000,3.5,true],"e":{"f":"g","h":{"i":false}},"t":{"u":{"k":-2}},"r":[{"from":"/a","values":{"X":"1"}},{"from":"/b"}]}"#
        );
        assert_eq!(
            ok("\"q k\" = 1\n'l' = 2\nx.y = 3\nx.z = 4\n"),
            r#"{"q k":1,"l":2,"x":{"y":3,"z":4}}"#
        );
        assert_eq!(
            ok("e = 1e3\nu = \"\\u00e9\\U0001F600\"\n"),
            r#"{"e":1000,"u":"é😀"}"#
        );
    }

    #[test]
    fn errors() {
        for (t, msg) in [
            ("a = 1\na = 2\n", "f.toml:2: key 'a' is already defined"),
            ("[t]\n[t]\n", "f.toml:2: table 't' is defined twice"),
            ("a = \"x\n", "f.toml:1: unterminated string"),
            (
                "a = \"\"\"x\"\"\"\n",
                "f.toml:1: multi-line strings are not supported",
            ),
            (
                "a = 1979-05-27\n",
                "f.toml:1: dates and times are not supported",
            ),
            ("a = 0x1F\n", "f.toml:1: unsupported value '0x1F'"),
            ("a = 01\n", "f.toml:1: unsupported value '01'"),
            ("a\n", "f.toml:1: expected '=' after a key"),
            ("a = 1 b\n", "f.toml:1: expected the end of the line"),
            ("a = [1 2]\n", "f.toml:1: expected ',' or ']' in an array"),
            (
                "a = { b = 1 }\n[a]\n",
                "f.toml:2: key 'a' is already defined",
            ),
            ("a = [1]\n[[a]]\n", "f.toml:2: key 'a' is already defined"),
            ("a = \"\\x\"\n", "f.toml:1: invalid escape in a string"),
            ("a =\n", "f.toml:1: expected a value"),
            ("[a\n", "f.toml:1: expected ']'"),
        ] {
            assert_eq!(bad(t), msg, "{t:?}");
        }
    }
}
