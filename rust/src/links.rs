//! References between a site's files (port of `src/links.ts`): the URLs HTML and CSS files contain,
//! how they resolve to tree paths, and how a reference is rewritten when its target is renamed. Both
//! implementations find the same references; offsets here are byte offsets (TypeScript uses UTF-16),
//! which is why the differential fixture (`tests/fixtures/links/`) compares lines and text, not offsets.
//! See the TypeScript module for what is and is not recognised.

/// One URL found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Byte offsets of the URL as written (`raw` is `text[start..end]`).
    pub start: usize,
    pub end: usize,
    pub raw: String,
    /// The URL with character references (HTML) or escapes (CSS) decoded and ASCII whitespace trimmed.
    pub url: String,
    /// 1-based line of `start`.
    pub line: u32,
    /// Written as CSS (a CSS file, a `<style>` element or a `style` attribute), so CSS escapes apply.
    pub css: bool,
    /// For a URL in an HTML attribute (not CSS): the lower-case element and attribute names, `img src`.
    pub element: Option<String>,
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// CSS name characters (ASCII letters, digits, `-`, `_`, anything non-ASCII).
fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
}

fn trim_ws(s: &[u8]) -> (usize, usize) {
    let mut a = 0;
    let mut b = s.len();
    while a < b && is_ws(s[a]) {
        a += 1;
    }
    while b > a && is_ws(s[b - 1]) {
        b -= 1;
    }
    (a, b)
}

fn trimmed(s: &str) -> String {
    let (a, b) = trim_ws(s.as_bytes());
    s[a..b].to_string()
}

/// A code point from a character reference or CSS escape: 0, surrogates and values above U+10FFFF become U+FFFD.
fn code_point(n: u32) -> char {
    if n == 0 {
        return '\u{fffd}';
    }
    char::from_u32(n).unwrap_or('\u{fffd}')
}

/// Decoded text, and for each of its byte offsets (and its end) the byte offset in the source it came from.
pub struct Mapped {
    pub text: String,
    pub map: Vec<usize>,
}

fn push_mapped(out: &mut String, map: &mut Vec<usize>, s: &str, at: usize) {
    out.push_str(s);
    map.extend(std::iter::repeat_n(at, s.len()));
}

/// Decode the character references this scanner knows; others stay as written.
pub fn decode_html(s: &str) -> Mapped {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut i = 0;
    while i < s.len() {
        if s.as_bytes()[i] != b'&' {
            let ch = s[i..].chars().next().unwrap();
            for k in 0..ch.len_utf8() {
                map.push(i + k);
            }
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let name = s[i + 1..].find(';').map(|k| &s[i + 1..i + 1 + k]);
        let decoded: Option<String> = name.and_then(|name| {
            let named = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => None,
            };
            if let Some(c) = named {
                return Some(c.to_string());
            }
            let b = name.as_bytes();
            if b.len() >= 2 && b.len() <= 9 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_digit)
            {
                return Some(code_point(name[1..].parse::<u32>().ok()?).to_string());
            }
            if b.len() >= 3
                && b.len() <= 10
                && b[0] == b'#'
                && (b[1] == b'x' || b[1] == b'X')
                && b[2..].iter().all(u8::is_ascii_hexdigit)
            {
                return Some(code_point(u32::from_str_radix(&name[2..], 16).ok()?).to_string());
            }
            None
        });
        match decoded {
            Some(d) => {
                push_mapped(&mut out, &mut map, &d, i);
                i += 1 + name.map_or(0, str::len) + 1;
            }
            None => {
                map.push(i);
                out.push('&');
                i += 1;
            }
        }
    }
    map.push(s.len());
    Mapped { text: out, map }
}

pub fn decode_html_text(s: &str) -> String {
    if s.contains('&') {
        decode_html(s).text
    } else {
        s.to_string()
    }
}

/// Decode CSS escapes (see `decodeCss` in the TypeScript module).
pub fn decode_css(s: &str) -> Mapped {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let ch = s[i..].chars().next().unwrap();
            for k in 0..ch.len_utf8() {
                map.push(i + k);
            }
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let at = i;
        i += 1;
        if i >= b.len() {
            break;
        }
        if b[i] == b'\n' {
            i += 1;
            continue;
        }
        if b[i].is_ascii_hexdigit() {
            let mut j = i;
            while j < b.len() && j - i < 6 && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            let ch = code_point(u32::from_str_radix(&s[i..j], 16).unwrap_or(0));
            push_mapped(&mut out, &mut map, ch.encode_utf8(&mut [0; 4]), at);
            i = j;
            if i + 1 < b.len() && b[i] == b'\r' && b[i + 1] == b'\n' {
                i += 2;
            } else if i < b.len() && is_ws(b[i]) {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        push_mapped(&mut out, &mut map, ch.encode_utf8(&mut [0; 4]), at);
        i += ch.len_utf8();
    }
    map.push(s.len());
    Mapped { text: out, map }
}

pub fn decode_css_text(s: &str) -> String {
    if s.contains('\\') {
        decode_css(s).text
    } else {
        s.to_string()
    }
}

/// Line numbers by byte offset.
struct Lines(Vec<usize>);

impl Lines {
    fn new(text: &str) -> Lines {
        Lines(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i)
                .collect(),
        )
    }
    fn at(&self, pos: usize) -> u32 {
        (self.0.partition_point(|&p| p < pos) + 1) as u32
    }
}

struct Collector<'a> {
    text: &'a str,
    lines: Lines,
    out: Vec<Reference>,
}

impl Collector<'_> {
    fn push(&mut self, start: usize, end: usize, url: String, css: bool) {
        let line = self.lines.at(start);
        self.out.push(Reference {
            start,
            end,
            raw: self.text[start..end].to_string(),
            url,
            line,
            css,
            element: None,
        });
    }
    fn push_attr(&mut self, start: usize, end: usize, url: String, tag: &str, name: &str) {
        self.push(start, end, url, false);
        if let Some(r) = self.out.last_mut() {
            r.element = Some(format!("{tag} {name}"));
        }
    }
}

fn find_from(text: &[u8], pat: &[u8], from: usize) -> Option<usize> {
    if from > text.len() {
        return None;
    }
    text[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|k| k + from)
}

fn find_byte(text: &[u8], c: u8, from: usize) -> Option<usize> {
    if from > text.len() {
        return None;
    }
    text[from..].iter().position(|&b| b == c).map(|k| k + from)
}

fn lower_ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_ascii_lowercase()
}

// ---------------------------------------------------------------- CSS

/// The end of a CSS string starting at the quote `at`: the index of its closing quote, or None (newline or end).
fn css_string_end(b: &[u8], at: usize, limit: usize) -> Option<usize> {
    let q = b[at];
    let mut i = at + 1;
    while i < limit {
        let c = b[i];
        if c == b'\\' {
            i += 2;
        } else if c == q {
            return Some(i);
        } else if c == b'\n' {
            return None;
        } else {
            i += 1;
        }
    }
    None
}

fn scan_css_range(col: &mut Collector, from: usize, to: usize) {
    let text = col.text;
    let b = text.as_bytes();
    let mut i = from;
    let mut after_import = false;
    while i < to {
        let c = b[i];
        if c == b'/' && i + 1 < to && b[i + 1] == b'*' {
            i = match find_from(b, b"*/", i + 2) {
                Some(j) if j + 2 <= to => j + 2,
                _ => to,
            };
            continue;
        }
        if c == b'"' || c == b'\'' {
            match css_string_end(b, i, to) {
                None => {
                    i = match find_byte(b, b'\n', i + 1) {
                        Some(nl) if nl < to => nl,
                        _ => to,
                    };
                    after_import = false;
                }
                Some(end) => {
                    if after_import {
                        col.push(
                            i + 1,
                            end,
                            trimmed(&decode_css_text(&text[i + 1..end])),
                            true,
                        );
                    }
                    after_import = false;
                    i = end + 1;
                }
            }
            continue;
        }
        if c == b'\\' {
            // The backslash and the whole character it escapes.
            i += 1 + text[i + 1..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if c == b'@' {
            let mut j = i + 1;
            while j < to && is_name_char(b[j]) {
                j += 1;
            }
            after_import = lower_ascii(&b[i + 1..j]) == "import";
            i = j;
            continue;
        }
        if is_name_char(c) {
            let mut j = i;
            while j < to && is_name_char(b[j]) {
                j += 1;
            }
            if j < to && b[j] == b'(' && lower_ascii(&b[i..j]) == "url" {
                i = css_url(col, j + 1, to);
                after_import = false;
                continue;
            }
            i = j;
            continue;
        }
        if c == b';' || c == b'{' || c == b'}' {
            after_import = false;
        }
        i += 1;
    }
}

/// The contents of `url(` starting at `i` (after the parenthesis); returns where scanning continues.
fn css_url(col: &mut Collector, mut i: usize, to: usize) -> usize {
    let text = col.text;
    let b = text.as_bytes();
    while i < to && is_ws(b[i]) {
        i += 1;
    }
    if i >= to {
        return to;
    }
    let q = b[i];
    if q == b'"' || q == b'\'' {
        let Some(end) = css_string_end(b, i, to) else {
            return i + 1;
        };
        let mut k = end + 1;
        while k < to && is_ws(b[k]) {
            k += 1;
        }
        if k < to && b[k] == b')' {
            col.push(
                i + 1,
                end,
                trimmed(&decode_css_text(&text[i + 1..end])),
                true,
            );
            return k + 1;
        }
        return end + 1;
    }
    let start = i;
    while i < to {
        let c = b[i];
        if c == b')' || is_ws(c) || c == b'"' || c == b'\'' || c == b'(' {
            break;
        }
        i += if c == b'\\' { 2 } else { 1 };
    }
    if i > to {
        i = to;
    }
    // Keep the end on a character boundary (an escape before a multi-byte character).
    while !text.is_char_boundary(i) {
        i += 1;
    }
    let end = i;
    while i < to && is_ws(b[i]) {
        i += 1;
    }
    if i < to && b[i] == b')' {
        if end > start {
            col.push(start, end, decode_css_text(&text[start..end]), true);
        }
        return i + 1;
    }
    i
}

/// The URLs a CSS file references, in order.
pub fn scan_css(text: &str) -> Vec<Reference> {
    let mut col = Collector {
        text,
        lines: Lines::new(text),
        out: Vec::new(),
    };
    scan_css_range(&mut col, 0, text.len());
    col.out
}

// ---------------------------------------------------------------- HTML

const RAW_TEXT: [&str; 5] = ["script", "style", "textarea", "title", "xmp"];

/// srcset candidates: the offsets of each URL in `value`.
fn srcset_urls(value: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < value.len() {
        while i < value.len() && (is_ws(value[i]) || value[i] == b',') {
            i += 1;
        }
        if i >= value.len() {
            break;
        }
        let start = i;
        while i < value.len() && !is_ws(value[i]) {
            i += 1;
        }
        let mut end = i;
        if value[end - 1] == b',' {
            while end > start && value[end - 1] == b',' {
                end -= 1;
            }
        } else {
            let mut depth = 0u32;
            while i < value.len() {
                let c = value[i];
                if c == b'(' {
                    depth += 1;
                } else if c == b')' && depth > 0 {
                    depth -= 1;
                } else if c == b',' && depth == 0 {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
        if end > start {
            out.push((start, end));
        }
    }
    out
}

fn html_attribute(col: &mut Collector, tag: &str, name: &str, start: usize, end: usize) {
    let text = col.text;
    let value = &text[start..end];
    if name == "style" {
        scan_css_range(col, start, end);
        return;
    }
    if name == "srcset" || name == "imagesrcset" {
        for (a, b) in srcset_urls(value.as_bytes()) {
            col.push_attr(
                start + a,
                start + b,
                decode_html_text(&value[a..b]),
                tag,
                name,
            );
        }
        return;
    }
    let url = name == "src"
        || name == "poster"
        || name == "xlink:href"
        || (name == "href" && tag != "base")
        || (name == "data" && tag == "object");
    if !url {
        return;
    }
    let (a, b) = trim_ws(value.as_bytes());
    col.push_attr(
        start + a,
        start + b,
        trimmed(&decode_html_text(&value[a..b])),
        tag,
        name,
    );
}

/// The URLs an HTML file references, in order (including those of its CSS).
pub fn scan_html(text: &str) -> Vec<Reference> {
    let mut col = Collector {
        text,
        lines: Lines::new(text),
        out: Vec::new(),
    };
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        let Some(lt) = find_byte(b, b'<', i) else {
            break;
        };
        i = lt;
        let next = b.get(i + 1).copied();
        if b[i..].starts_with(b"<!--") {
            i = find_from(b, b"-->", i + 4).map_or(n, |j| j + 3);
            continue;
        }
        if matches!(next, Some(b'!' | b'?' | b'/')) {
            i = find_byte(b, b'>', i + 2).map_or(n, |j| j + 1);
            continue;
        }
        if !next.is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && !is_ws(b[j]) && b[j] != b'/' && b[j] != b'>' {
            j += 1;
        }
        let tag = lower_ascii(&b[i + 1..j]);
        i = j;
        loop {
            while i < n && (is_ws(b[i]) || b[i] == b'/') {
                i += 1;
            }
            if i >= n {
                break;
            }
            if b[i] == b'>' {
                i += 1;
                break;
            }
            let name_start = i;
            // The first character may be `=` (and may be multi-byte).
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            while i < n && !is_ws(b[i]) && b[i] != b'/' && b[i] != b'>' && b[i] != b'=' {
                i += 1;
            }
            let name = lower_ascii(&b[name_start..i]);
            let mut k = i;
            while k < n && is_ws(b[k]) {
                k += 1;
            }
            if k >= n || b[k] != b'=' {
                continue;
            }
            i = k + 1;
            while i < n && is_ws(b[i]) {
                i += 1;
            }
            let (start, end);
            if i < n && (b[i] == b'"' || b[i] == b'\'') {
                start = i + 1;
                match find_byte(b, b[i], start) {
                    Some(close) => {
                        end = close;
                        i = close + 1;
                    }
                    None => {
                        end = n;
                        i = n;
                    }
                }
            } else {
                start = i;
                while i < n && !is_ws(b[i]) && b[i] != b'>' {
                    i += 1;
                }
                end = i;
            }
            html_attribute(&mut col, &tag, &name, start, end);
        }
        if RAW_TEXT.contains(&tag.as_str()) {
            let close = index_of_close_tag(b, &tag, i);
            if tag == "style" {
                scan_css_range(&mut col, i, close);
            }
            i = close;
        }
    }
    col.out
}

/// Where `</tag` (ASCII case-insensitive, followed by whitespace, `/` or `>`) starts at or after `from`.
fn index_of_close_tag(b: &[u8], tag: &str, from: usize) -> usize {
    let mut i = from;
    loop {
        let Some(j) = find_from(b, b"</", i) else {
            return b.len();
        };
        let after = j + 2 + tag.len();
        let name = &b[j + 2..after.min(b.len())];
        if name.eq_ignore_ascii_case(tag.as_bytes())
            && (after >= b.len() || is_ws(b[after]) || b[after] == b'/' || b[after] == b'>')
        {
            return j;
        }
        i = j + 2;
    }
}

/// Whether a path is HTML or CSS (by extension, ASCII case-insensitive).
pub fn reference_kind(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".html") || lower.ends_with(".htm") {
        Some("html")
    } else if lower.ends_with(".css") {
        Some("css")
    } else {
        None
    }
}

/// A file's text and references; None when it is not HTML or CSS, or not valid UTF-8.
pub fn scan_file(path: &str, bytes: &[u8]) -> Option<(String, Vec<Reference>)> {
    let kind = reference_kind(path)?;
    let text = std::str::from_utf8(bytes).ok()?;
    let refs = if kind == "html" {
        scan_html(text)
    } else {
        scan_css(text)
    };
    Some((text.to_string(), refs))
}

// ---------------------------------------------------------------- resolution

/// What a reference points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Nothing to check: empty, a fragment or query of the same document, or a non-http(s) scheme.
    None,
    /// An http(s) URL or a protocol-relative `//host/…`.
    External(String),
    /// A file of the site: an absolute, normalized NFC path; `dir` when the URL names a directory.
    Internal { path: String, dir: bool },
}

/// Percent-decode one path segment; a segment whose decoding is not valid UTF-8 stays as written.
pub fn percent_decode(seg: &str) -> String {
    if !seg.contains('%') {
        return seg.to_string();
    }
    let b = seg.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            out.push(u8::from_str_radix(&seg[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| seg.to_string())
}

fn scheme_len(url: &str) -> Option<usize> {
    let b = url.as_bytes();
    if !b.first()?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'+' | b'.' | b'-')) {
        i += 1;
    }
    (i < b.len() && b[i] == b':').then_some(i + 1)
}

/// Resolve a reference's URL against the path of the file that contains it. `root` is the directory the
/// site is served from (the EXPORT path; see `resolveReference` in the TypeScript module).
pub fn resolve_reference(url: &str, from: &str, root: &str) -> Target {
    if url.is_empty() || url.starts_with('#') || url.starts_with('?') {
        return Target::None;
    }
    if url.starts_with("//") {
        return Target::External(url.to_string());
    }
    if let Some(n) = scheme_len(url) {
        let s = url[..n].to_ascii_lowercase();
        return if s == "http:" || s == "https:" {
            Target::External(url.to_string())
        } else {
            Target::None
        };
    }
    let raw = url.find(['?', '#']).map_or(url, |k| &url[..k]);
    let root_segs: Vec<String> = if root == "/" {
        Vec::new()
    } else {
        root.split('/').skip(1).map(str::to_string).collect()
    };
    let below = root == "/" || from.starts_with(&format!("{root}/"));
    let absolute = raw.starts_with('/');
    let mut segs: Vec<String> = if absolute {
        root_segs.clone()
    } else {
        let dir: Vec<&str> = from.split('/').skip(1).collect();
        dir[..dir.len().saturating_sub(1)]
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    let floor = if absolute || below {
        root_segs.len()
    } else {
        0
    };
    let parts: Vec<&str> = if absolute {
        raw.split('/').skip(1).collect()
    } else {
        raw.split('/').collect()
    };
    let mut dir = false;
    for (i, part) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let seg = percent_decode(part);
        if seg.is_empty() || seg == "." {
            if last {
                dir = true;
            }
        } else if seg == ".." {
            if segs.len() > floor {
                segs.pop();
            }
            if last {
                dir = true;
            }
        } else {
            segs.push(seg);
        }
    }
    if segs.is_empty() {
        dir = true;
    }
    let path = format!("/{}", segs.join("/"));
    Target::Internal {
        path: crate::paths::nfc(&path).into_owned(),
        dir,
    }
}

// ---------------------------------------------------------------- rewriting

/// Percent-encode a path, keeping `/` and `A-Z a-z 0-9 - . _ ~`.
pub fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for &b in p.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn basename(p: &str) -> &str {
    &p[p.rfind('/').map_or(0, |k| k + 1)..]
}

/// A relative URL from the file `from` to the file `to` (both absolute tree paths), percent-encoded.
pub fn relative_url(from: &str, to: &str) -> String {
    let all: Vec<&str> = from.split('/').skip(1).collect();
    let dir = &all[..all.len().saturating_sub(1)];
    let target: Vec<&str> = to.split('/').skip(1).collect();
    let mut k = 0;
    while k < dir.len() && k + 1 < target.len() && dir[k] == target[k] {
        k += 1;
    }
    let rest: Vec<String> = target[k..].iter().map(|s| encode_path(s)).collect();
    format!("{}{}", "../".repeat(dir.len() - k), rest.join("/"))
}

/// The reference `raw` (written in `from`, resolved to `old_path`) pointing at `new_path` instead (see
/// `renameReference` in the TypeScript module).
pub fn rename_reference(
    raw: &str,
    css: bool,
    from: &str,
    old_path: &str,
    new_path: &str,
) -> String {
    let Mapped { text, map } = if css {
        decode_css(raw)
    } else {
        decode_html(raw)
    };
    let (t0, t1) = trim_ws(text.as_bytes());
    let url = &text[t0..t1];
    let path_end = t0 + url.find(['?', '#']).unwrap_or(url.len());
    let seg_start = if path_end > t0 {
        text[t0..path_end].rfind('/').map_or(t0, |k| t0 + k + 1)
    } else {
        t0
    };
    let seg = &text[seg_start..path_end];
    let old_base = basename(old_path);
    let new_base = basename(new_path);
    let insert_at = |offset: usize, insert: &str| {
        format!("{}{insert}{}", &raw[..map[offset]], &raw[map[offset]..])
    };
    if percent_decode(seg) == old_base {
        match old_base.rfind('.') {
            Some(dot) if dot > 0 => {
                let ext = &old_base[dot..];
                if let Some(d) = seg.rfind('.') {
                    if percent_decode(&seg[d..]) == ext {
                        return insert_at(
                            seg_start + d,
                            &new_base[dot..new_base.len() - ext.len()],
                        );
                    }
                }
            }
            _ => return insert_at(path_end, &new_base[old_base.len()..]),
        }
    }
    format!(
        "{}{}{}",
        &raw[..map[t0]],
        relative_url(from, new_path),
        &raw[map[path_end]..]
    )
}

/// `text` with each reference whose target `rename` maps to a new path rewritten; None when nothing changed.
pub fn rewrite_references(
    text: &str,
    refs: &[Reference],
    from: &str,
    root: &str,
    rename: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut changed = false;
    for r in refs {
        let Target::Internal { path, dir: false } = resolve_reference(&r.url, from, root) else {
            continue;
        };
        let Some(to) = rename(&path) else { continue };
        out.push_str(&text[at..r.start]);
        out.push_str(&rename_reference(&r.raw, r.css, from, &path, &to));
        at = r.end;
        changed = true;
    }
    if !changed {
        return None;
    }
    out.push_str(&text[at..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(refs: &[Reference]) -> Vec<(u32, &str)> {
        refs.iter().map(|r| (r.line, r.url.as_str())).collect()
    }

    #[test]
    fn html_attributes_and_css() {
        let html = "<link href='a.css'>\n<img src=\" b.png \" srcset=\"c.png 1x, d%20e.png 2x\">\n<base href=/x/><object data=o.svg></object>\n<!-- <img src=no.png> --><script src=s.js>var x = '<img src=no.png>'</script>\n<style>@import 'i.css'; p{background:url(\"bg.png\")}</style><p style=\"background: url(p.png)\">\n<a href=\"x?a=1&amp;b=2\">";
        assert_eq!(
            urls(&scan_html(html)),
            [
                (1, "a.css"),
                (2, "b.png"),
                (2, "c.png"),
                (2, "d%20e.png"),
                (3, "o.svg"),
                (4, "s.js"),
                (5, "i.css"),
                (5, "bg.png"),
                (5, "p.png"),
                (6, "x?a=1&b=2")
            ]
        );
    }

    #[test]
    fn css_comments_strings_and_escapes() {
        let css = "/* url(no.png) */ a { content: \"url(no.png)\"; background: url( 'x\\2e png' ) }\n@import url(i.css);\n.myurl(no) { b: url(y\\).png) }";
        assert_eq!(
            urls(&scan_css(css)),
            [(1, "x.png"), (2, "i.css"), (3, "y).png")]
        );
    }

    #[test]
    fn resolve() {
        assert_eq!(
            resolve_reference("../img/a.png?x#y", "/css/site.css", "/"),
            Target::Internal {
                path: "/img/a.png".into(),
                dir: false
            }
        );
        assert_eq!(
            resolve_reference("/../../a/", "/x.html", "/"),
            Target::Internal {
                path: "/a".into(),
                dir: true
            }
        );
        assert_eq!(
            resolve_reference("caf%C3%A9.css", "/x.html", "/"),
            Target::Internal {
                path: "/café.css".into(),
                dir: false
            }
        );
        assert_eq!(
            resolve_reference("mailto:x@y", "/x.html", "/"),
            Target::None
        );
        assert_eq!(resolve_reference("#top", "/x.html", "/"), Target::None);
        assert_eq!(
            resolve_reference("//cdn.example/a.js", "/x.html", "/"),
            Target::External("//cdn.example/a.js".into())
        );
        assert_eq!(
            resolve_reference("HTTPS://e.x/", "/x.html", "/"),
            Target::External("HTTPS://e.x/".into())
        );
        assert_eq!(
            resolve_reference("/img/a.png", "/www/x.html", "/www"),
            Target::Internal {
                path: "/www/img/a.png".into(),
                dir: false
            }
        );
        assert_eq!(
            resolve_reference("../../img/a.png", "/www/x.html", "/www"),
            Target::Internal {
                path: "/www/img/a.png".into(),
                dir: false
            }
        );
        assert_eq!(
            resolve_reference("../../img/a.png", "/src/x.html", "/www"),
            Target::Internal {
                path: "/img/a.png".into(),
                dir: false
            }
        );
    }

    #[test]
    fn rename() {
        assert_eq!(
            rename_reference(
                "css/a.css?v=1",
                false,
                "/x.html",
                "/css/a.css",
                "/css/a.0123456789.css"
            ),
            "css/a.0123456789.css?v=1"
        );
        assert_eq!(
            rename_reference(
                "LICENSE",
                false,
                "/x.html",
                "/LICENSE",
                "/LICENSE.0123456789"
            ),
            "LICENSE.0123456789"
        );
        assert_eq!(
            rename_reference(
                "caf&#xe9;&#46;png#x",
                false,
                "/x.html",
                "/café.png",
                "/café.0123456789.png"
            ),
            "caf&#xe9;.0123456789&#46;png#x"
        );
        assert_eq!(
            rename_reference(" a\\2e png ", true, "/x.css", "/a.png", "/a.0123456789.png"),
            " a.0123456789\\2e png "
        );
        assert_eq!(
            rename_reference(
                "/a/b%2Ecss?v",
                false,
                "/a/x/y.html",
                "/a/b.css",
                "/a/b.0123456789.css"
            ),
            "../b.0123456789.css?v"
        );
    }
}
