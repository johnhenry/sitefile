//! Logical lines and the token cursor (src/language/scanner.ts).
//!
//! The TypeScript scanner indexes JavaScript strings, so it works in UTF-16
//! code units. Here a logical line is a `Vec<char>`: every boundary the scanner
//! cuts at is an ASCII or whitespace character, so the tokens are the same, and
//! each character's position records its UTF-16 column, as JavaScript reports it.

use crate::error::{ErrorCode, ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::{InlineBody, Operand, TemplatePart};

/// A 1-based line and UTF-16 column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

/// A heredoc body that followed a logical line.
#[derive(Debug, Clone, PartialEq)]
pub struct Heredoc {
    /// Index in the line's characters of the `<<` marker.
    pub at: usize,
    /// Length of the marker text (`<<-NAME`).
    pub length: usize,
    pub delimiter: String,
    pub text: String,
    pub body_line: u32,
}

/// One instruction's text after continuations are joined, with a source position per character.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalLine {
    pub chars: Vec<char>,
    pub positions: Vec<Pos>,
    /// Position just after the last character (for "expected X" errors at end of line).
    pub end: Pos,
    /// A heredoc body that followed this line (COPY, RENDER and TRANSFORM only).
    pub heredoc: Option<Heredoc>,
}

impl LogicalLine {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
}

/// JavaScript's `\s` (also what `String.prototype.trim` removes).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `s.trim()` in JavaScript.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn is_ident_start(c: Option<char>) -> bool {
    matches!(c, Some(c) if c == '_' || c.is_ascii_alphabetic())
}

fn is_ident_char(c: Option<char>) -> bool {
    matches!(c, Some(c) if c == '_' || c.is_ascii_alphanumeric())
}

/// Instructions whose source may be a heredoc.
const HEREDOC_INSTRUCTIONS: [&str; 3] = ["COPY", "RENDER", "TRANSFORM"];

#[derive(Debug, Clone)]
struct Marker {
    at: usize,
    length: usize,
    strip: bool,
    delimiter: String,
}

/// `<<NAME` / `<<-NAME` markers that start a word outside quotes.
fn heredoc_markers(text: &[char]) -> Vec<Marker> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i];
        if let Some(q) = quote {
            if ch == '\\' && q == '"' {
                i += 1;
            } else if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '\\' {
            i += 2;
            continue;
        }
        if ch != '<' || (i > 0 && !is_js_space(text[i - 1])) {
            i += 1;
            continue;
        }
        // ^<<(-?)([A-Za-z_][A-Za-z0-9_]*)(?=\s|$)
        if text.get(i + 1) == Some(&'<') {
            let mut j = i + 2;
            let strip = text.get(j) == Some(&'-');
            if strip {
                j += 1;
            }
            if is_ident_start(text.get(j).copied()) {
                let name_start = j;
                while is_ident_char(text.get(j).copied()) {
                    j += 1;
                }
                if text.get(j).is_none_or(|c| is_js_space(*c)) {
                    out.push(Marker { at: i, length: j - i, strip, delimiter: text[name_start..j].iter().collect() });
                }
            }
        }
        i += 1;
    }
    out
}

fn trailing_backslashes(s: &[char]) -> usize {
    s.iter().rev().take_while(|c| **c == '\\').count()
}

/// `source.split(/\r?\n/)`
fn physical_lines(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(nl) = rest.find('\n') {
        let piece = &rest[..nl];
        out.push(piece.strip_suffix('\r').unwrap_or(piece));
        rest = &rest[nl + 1..];
    }
    out.push(rest);
    out
}

struct OpenHeredoc {
    marker: Marker,
    lines: Vec<String>,
    start: u32,
}

fn span_of(file: &str, p: Pos) -> SourceSpan {
    SourceSpan::new(file, p.line, p.column)
}

/// Split source into logical lines. Blank lines and comment lines (first non-blank
/// char `#`) are skipped, also inside continuations. A line ending in an odd number
/// of `\` continues; pieces are trimmed at the join and joined with one space.
pub fn logical_lines(source: &str, file: &str) -> Result<Vec<LogicalLine>> {
    let mut out: Vec<LogicalLine> = Vec::new();
    let mut text: Vec<char> = Vec::new();
    let mut positions: Vec<Pos> = Vec::new();
    let mut end = Pos { line: 1, column: 1 };
    let mut continuing = false;
    // Set while reading a heredoc body: lines are taken verbatim until the delimiter.
    let mut heredoc_body: Option<OpenHeredoc> = None;

    let mut physical = physical_lines(source);
    // A trailing newline is not an extra (empty) line of a heredoc body.
    if physical.len() > 1 && physical.last() == Some(&"") {
        physical.pop();
    }
    for (index, raw) in physical.iter().enumerate() {
        let line = (index + 1) as u32;
        if let Some(open) = heredoc_body.as_mut() {
            let candidate = if open.marker.strip { raw.trim_start_matches('\t') } else { raw };
            if candidate == open.marker.delimiter {
                let body: String = open.lines.iter().map(|l| format!("{l}\n")).collect();
                let heredoc = Heredoc {
                    at: open.marker.at,
                    length: open.marker.length,
                    delimiter: open.marker.delimiter.clone(),
                    text: body,
                    body_line: open.start,
                };
                if let Some(owner) = out.last_mut() {
                    owner.heredoc = Some(heredoc);
                }
                heredoc_body = None;
            } else {
                open.lines.push(candidate.to_string());
            }
            continue;
        }
        let trimmed = js_trim(raw);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let chars: Vec<char> = raw.chars().collect();
        // cols[k]: UTF-16 offset of chars[k] (cols[len] is the line length).
        let mut cols = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0u32;
        for c in &chars {
            cols.push(acc);
            acc += c.len_utf16() as u32;
        }
        cols.push(acc);

        let cont = trailing_backslashes(&chars) % 2 == 1;
        let body = if cont { &chars[..chars.len() - 1] } else { &chars[..] };
        let mut start = 0;
        if continuing {
            while start < body.len() && is_js_space(body[start]) {
                start += 1;
            }
        }
        let mut stop = body.len();
        if cont {
            while stop > start && is_js_space(body[stop - 1]) {
                stop -= 1;
            }
        }
        if continuing && !text.is_empty() && stop > start {
            text.push(' ');
            positions.push(end);
        }
        for c in start..stop {
            text.push(body[c]);
            positions.push(Pos { line, column: cols[c] + 1 });
        }
        if stop > start || !continuing {
            end = Pos { line, column: cols[stop] + 1 };
        }
        if cont {
            continuing = true;
            continue;
        }
        let logical = LogicalLine {
            chars: std::mem::take(&mut text),
            positions: std::mem::take(&mut positions),
            end,
            heredoc: None,
        };
        // /^\S+/: an indented first piece has no keyword here.
        let keyword: String =
            logical.chars.iter().take_while(|c| !is_js_space(**c)).collect::<String>().to_uppercase();
        if HEREDOC_INSTRUCTIONS.contains(&keyword.as_str()) {
            let markers = heredoc_markers(&logical.chars);
            if let Some(second) = markers.get(1) {
                let p = logical.positions[second.at];
                return Err(SitefileError::at(
                    E_SYNTAX,
                    "only one inline (<<) body is allowed per instruction",
                    &span_of(file, p),
                ));
            }
            if let Some(first) = markers.into_iter().next() {
                heredoc_body = Some(OpenHeredoc { marker: first, lines: Vec::new(), start: line + 1 });
            }
        }
        out.push(logical);
        continuing = false;
    }
    if let Some(open) = heredoc_body {
        let owner = out.last().map(|o| o.positions[open.marker.at]).unwrap_or(Pos { line: 1, column: 1 });
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("inline body is missing its closing '{}' line", open.marker.delimiter),
            &span_of(file, owner),
        ));
    }
    if continuing && !js_trim(&text.iter().collect::<String>()).is_empty() {
        out.push(LogicalLine { chars: text, positions, end, heredoc: None });
    }
    Ok(out)
}

/// `--name` or `--name=value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub name: String,
    pub value: Option<String>,
    pub span: SourceSpan,
}

/// Reads tokens from one logical line. All methods skip leading whitespace.
pub struct Cursor<'a> {
    line: &'a LogicalLine,
    t: &'a [char],
    i: usize,
    inline_used: bool,
    file: &'a str,
}

impl<'a> Cursor<'a> {
    pub fn new(line: &'a LogicalLine, file: &'a str) -> Self {
        Cursor { line, t: &line.chars, i: 0, inline_used: false, file }
    }

    pub fn file(&self) -> &str {
        self.file
    }

    pub fn span_at(&self, i: usize) -> SourceSpan {
        let p = self.line.positions.get(i).copied().unwrap_or(self.line.end);
        span_of(self.file, p)
    }

    /// Span of the next non-whitespace character (or end of line).
    pub fn span(&mut self) -> SourceSpan {
        self.skip_ws();
        self.span_at(self.i)
    }

    /// An error at character index `at`.
    pub fn error_at(&self, code: ErrorCode, detail: impl Into<String>, at: usize) -> SitefileError {
        SitefileError::at(code, detail, &self.span_at(at))
    }

    /// An error at the current position.
    pub fn error(&self, code: ErrorCode, detail: impl Into<String>) -> SitefileError {
        self.error_at(code, detail, self.i)
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.t[a..b].iter().collect()
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.t.get(self.i + k).copied()
    }

    pub fn skip_ws(&mut self) {
        while self.i < self.t.len() && is_js_space(self.t[self.i]) {
            self.i += 1;
        }
    }

    pub fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.i >= self.t.len()
    }

    fn word_end(&self) -> usize {
        let mut j = self.i;
        while j < self.t.len() && !is_js_space(self.t[j]) {
            j += 1;
        }
        j
    }

    /// Consume raw text up to the next whitespace (`""` at end of line).
    pub fn word(&mut self) -> String {
        self.skip_ws();
        let j = self.word_end();
        let w = self.slice(self.i, j);
        self.i = j;
        w
    }

    /// Consume `kw` (case-insensitive) if it is the next whole bare word.
    pub fn keyword(&mut self, kw: &str) -> bool {
        self.skip_ws();
        let j = self.word_end();
        if self.slice(self.i, j).to_lowercase() != kw.to_lowercase() {
            return false;
        }
        self.i = j;
        true
    }

    /// Whether `kw` (case-insensitive) is the next whole bare word, without consuming it.
    pub fn peek_keyword(&mut self, kw: &str) -> bool {
        let at = self.i;
        let found = self.keyword(kw);
        self.i = at;
        found
    }

    /// The current position, for [`Cursor::reset`] (one-token lookahead).
    pub fn mark(&self) -> usize {
        self.i
    }

    pub fn reset(&mut self, mark: usize) {
        self.i = mark;
    }

    pub fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        let at = self.span();
        if !self.keyword(kw) {
            return Err(SitefileError::at(E_SYNTAX, format!("expected {}", kw.to_uppercase()), &at));
        }
        Ok(())
    }

    /// Consume `ch` if it is the next non-whitespace character.
    pub fn eat(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.peek(0) != Some(ch) {
            return false;
        }
        self.i += 1;
        true
    }

    /// Read an identifier. It must be followed by whitespace, end of line, or one
    /// of the characters in `stops`.
    pub fn ident(&mut self, what: &str, stops: &str) -> Result<String> {
        self.skip_ws();
        let start = self.i;
        if !is_ident_start(self.peek(0)) {
            return Err(self.error(E_SYNTAX, format!("expected {what}")));
        }
        while is_ident_char(self.peek(0)) {
            self.i += 1;
        }
        if let Some(next) = self.peek(0) {
            if !is_js_space(next) && !stops.contains(next) {
                // JavaScript reports one UTF-16 unit: half of an astral character, which prints as U+FFFD.
                let shown = if next.len_utf16() > 1 { '\u{fffd}' } else { next };
                return Err(self.error(E_SYNTAX, format!("invalid {what}: unexpected '{shown}'")));
            }
        }
        Ok(self.slice(start, self.i))
    }

    /// Read `--name` or `--name=value` if the next word starts with `--`.
    pub fn flag(&mut self) -> Option<Flag> {
        self.skip_ws();
        if !(self.peek(0) == Some('-') && self.peek(1) == Some('-')) {
            return None;
        }
        let span = self.span_at(self.i);
        let w = self.word();
        let w = &w[2..];
        Some(match w.split_once('=') {
            None => Flag { name: w.to_string(), value: None, span },
            Some((n, v)) => Flag { name: n.to_string(), value: Some(v.to_string()), span },
        })
    }

    pub fn expect_end(&mut self) -> Result<()> {
        if self.at_end() {
            return Ok(());
        }
        let at = self.i;
        let w = self.word();
        Err(self.error_at(E_SYNTAX, format!("unexpected '{w}'"), at))
    }

    /// Consume this line's heredoc marker if it is the next word.
    pub fn inline(&mut self) -> Option<InlineBody> {
        self.skip_ws();
        let h = self.line.heredoc.as_ref()?;
        if h.at != self.i {
            return None;
        }
        let span = self.span_at(self.i);
        self.i += h.length;
        self.inline_used = true;
        Some(InlineBody { delimiter: h.delimiter.clone(), text: h.text.clone(), span, body_line: h.body_line })
    }

    /// Fail if the line has a heredoc that was not used as the source.
    pub fn expect_inline_used(&self, place: &str) -> Result<()> {
        match &self.line.heredoc {
            Some(h) if !self.inline_used => Err(self.error_at(
                E_SYNTAX,
                format!("an inline <<{} body can only be {place}", h.delimiter),
                h.at,
            )),
            _ => Ok(()),
        }
    }

    /// Read a bare, "double-quoted" or 'single-quoted' operand.
    pub fn operand(&mut self, what: &str) -> Result<Operand> {
        self.skip_ws();
        let start = self.i;
        let Some(quote) = self.peek(0) else {
            return Err(self.error(E_SYNTAX, format!("expected {what}")));
        };
        let mut parts: Vec<TemplatePart> = Vec::new();
        fn add_text(parts: &mut Vec<TemplatePart>, s: &str) {
            if let Some(TemplatePart::Text(last)) = parts.last_mut() {
                last.push_str(s);
            } else {
                parts.push(TemplatePart::Text(s.to_string()));
            }
        }

        if quote == '\'' {
            let Some(close) = self.t[start + 1..].iter().position(|c| *c == '\'').map(|k| k + start + 1) else {
                return Err(self.error_at(E_SYNTAX, "unterminated single-quoted string", start));
            };
            if close > start + 1 {
                let s = self.slice(start + 1, close);
                add_text(&mut parts, &s);
            }
            self.i = close + 1;
        } else if quote == '"' {
            self.i += 1;
            loop {
                let Some(ch) = self.peek(0) else {
                    return Err(self.error_at(E_SYNTAX, "unterminated double-quoted string", start));
                };
                if ch == '"' {
                    self.i += 1;
                    break;
                }
                if ch == '\\' {
                    let Some(next) = self.peek(1) else {
                        return Err(self.error_at(E_SYNTAX, "unterminated double-quoted string", start));
                    };
                    let c = match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    };
                    add_text(&mut parts, c.encode_utf8(&mut [0; 4]));
                    self.i += 2;
                } else if ch == '$' && self.peek(1) == Some('{') {
                    parts.push(self.interpolation()?);
                } else {
                    add_text(&mut parts, ch.encode_utf8(&mut [0; 4]));
                    self.i += 1;
                }
            }
        } else {
            while let Some(ch) = self.peek(0).filter(|c| !is_js_space(*c)) {
                if ch == '\\' {
                    let Some(next) = self.peek(1) else {
                        return Err(self.error(E_SYNTAX, "dangling escape at end of line"));
                    };
                    add_text(&mut parts, next.encode_utf8(&mut [0; 4]));
                    self.i += 2;
                } else if ch == '$' && self.peek(1) == Some('{') {
                    parts.push(self.interpolation()?);
                } else {
                    add_text(&mut parts, ch.encode_utf8(&mut [0; 4]));
                    self.i += 1;
                }
            }
            return Ok(Operand { parts, span: self.span_at(start) });
        }

        if let Some(after) = self.peek(0) {
            if !is_js_space(after) {
                return Err(self.error(E_SYNTAX, "unexpected character after closing quote"));
            }
        }
        Ok(Operand { parts, span: self.span_at(start) })
    }

    /// At `${`: read `${NAME}` or `${NAME.key…}`.
    fn interpolation(&mut self) -> Result<TemplatePart> {
        let at = self.i;
        self.i += 2;
        if !is_ident_start(self.peek(0)) {
            return Err(self.error_at(E_SYNTAX, "expected a name after ${", at));
        }
        let name_start = self.i;
        while is_ident_char(self.peek(0)) {
            self.i += 1;
        }
        let name = self.slice(name_start, self.i);
        // Dotted paths are parsed here; the validator allows them only for a FOR loop variable.
        let mut path = Vec::new();
        while self.peek(0) == Some('.') {
            self.i += 1;
            let seg_start = self.i;
            while matches!(self.peek(0), Some(c) if c == '_' || c == '-' || c.is_ascii_alphanumeric()) {
                self.i += 1;
            }
            if self.i == seg_start {
                return Err(self.error_at(E_SYNTAX, format!("expected a key after '.' in ${{{name}…}}"), at));
            }
            path.push(self.slice(seg_start, self.i));
        }
        if self.peek(0) != Some('}') {
            return Err(self.error_at(E_SYNTAX, "unterminated ${...}: expected }", at));
        }
        self.i += 1;
        Ok(TemplatePart::Var { name, path, span: self.span_at(at) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<String> {
        logical_lines(src, "Sitefile").unwrap().iter().map(|l| l.text()).collect()
    }

    #[test]
    fn blank_and_comment_lines_are_skipped() {
        assert_eq!(
            texts("\n# comment\n   # indented comment\nFROM scratch\n\n  \nEXPORT /\n"),
            ["FROM scratch", "EXPORT /"]
        );
    }

    #[test]
    fn continuations_join_pieces_and_keep_positions() {
        let lines = logical_lines("COPY \\\n    ./a   \\\n  /b", "Sitefile").unwrap();
        let l = &lines[0];
        assert_eq!(l.text(), "COPY ./a /b");
        let at = |i: usize| (l.positions[i].line, l.positions[i].column);
        assert_eq!(at(0), (1, 1));
        assert_eq!(at(5), (2, 5));
        assert_eq!(at(9), (3, 3));
    }

    #[test]
    fn columns_count_utf16_code_units() {
        let lines = logical_lines("COPY 😀é x", "Sitefile").unwrap();
        let l = &lines[0];
        // 😀 is two UTF-16 units, é one.
        assert_eq!(l.positions[5].column, 6);
        assert_eq!(l.positions[6].column, 8);
        assert_eq!(l.positions[8].column, 10);
        assert_eq!(l.end.column, 11);
    }

    #[test]
    fn js_whitespace() {
        assert!(is_js_space('\u{feff}') && is_js_space('\u{a0}') && !is_js_space('\u{85}'));
    }

    #[test]
    fn file_ending_in_a_continuation() {
        assert_eq!(texts("COPY a \\\n"), ["COPY a"]);
        assert!(texts("\\").is_empty());
    }
}
