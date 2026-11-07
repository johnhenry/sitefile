//! A small, dependency-free, deterministic Markdown renderer (port of
//! `src/markdown.ts`), producing byte-identical HTML.
//!
//! Blocks: ATX headings (#..######, optional closing #s), setext headings,
//! paragraphs (hard break = two trailing spaces or a trailing backslash),
//! fenced code (``` and ~~~, info word becomes class="language-x" when it is
//! [A-Za-z0-9_+-]+), indented code, blockquotes (recursive, lazy continuation),
//! bullet lists (- * +) and ordered lists (1. / 1), `start` kept when not 1),
//! nesting by indentation, tight vs loose lists, thematic breaks (--- *** ___).
//!
//! Inlines: backslash escapes, code spans, **strong** / __strong__, *em* /
//! _em_ (intraword `_` is literal), ~~del~~, [text](url "title"),
//! ![alt](url "title"), <autolinks>. Emphasis that does not close stays literal.
//!
//! Not supported: reference links, HTML entities (`&` is always escaped, so
//! `&amp;` shows literally), tables, footnotes, task lists.
//!
//! Safety: raw HTML is never passed through; every `<` in the source is
//! escaped. Link and image URLs must be http, https, mailto, relative,
//! `#fragment` or `/absolute`; anything else (javascript:, data:, ...) is
//! rendered as plain text without an <a> / <img>.
//!
//! Fidelity: the TypeScript version indexes strings by UTF-16 code unit (so an
//! astral character next to `*` is seen as a lone surrogate, which is neither
//! punctuation nor space). This port works on UTF-16 code units for the same
//! results, and uses JavaScript's `/\s/` whitespace and a `\p{P}\p{S}` table
//! generated from Node. Differential fixtures in `rust/tests/fixtures/markdown`
//! hold TypeScript output for comparison.

use crate::render::js::{self, is_ws};

/// Result of [`render_markdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownResult {
    pub html: String,
    /// Plain text of the first paragraph, at most 200 characters (UTF-16 units).
    pub excerpt: String,
}

const MAX_DEPTH: usize = 32;
const EXCERPT_MAX: usize = 200;

type W = Vec<u16>;

const BS: u16 = b'\\' as u16;
const BT: u16 = b'`' as u16;
const SP: u16 = b' ' as u16;
const TAB: u16 = b'\t' as u16;
const NL: u16 = b'\n' as u16;

const fn u(b: u8) -> u16 {
    b as u16
}

// ---------------------------------------------------------------- escaping

/// HTML-escape a UTF-16 string into `out`.
fn escape_into(s: &[u16], out: &mut String) {
    crate::render::escape_html_into(&js::string(s), out);
}

fn esc(s: &[u16]) -> String {
    let mut out = String::new();
    escape_into(s, &mut out);
    out
}

/// Scheme check on a URL stripped of whitespace/control characters.
fn is_safe_url(url: &[u16]) -> bool {
    let stripped: W = url
        .iter()
        .copied()
        .filter(|&c| !matches!(c, 0x0000..=0x0020 | 0x007f..=0x009f | 0x00ad | 0x200b..=0x200f | 0x2028 | 0x2029 | 0xfeff))
        .collect();
    match stripped
        .iter()
        .position(|&c| c == u(b':') || c == u(b'/') || c == u(b'?') || c == u(b'#'))
    {
        Some(p) if stripped[p] == u(b':') => {
            let s = &stripped[..p];
            let eq = |w: &str| {
                s.len() == w.len()
                    && s.iter()
                        .zip(w.bytes())
                        .all(|(&c, b)| c < 0x80 && (c as u8).eq_ignore_ascii_case(&b))
            };
            eq("http") || eq("https") || eq("mailto")
        }
        _ => true,
    }
}

fn encode_url(url: &[u16]) -> String {
    let mut w: W = Vec::with_capacity(url.len());
    for &c in url {
        if c <= 0x1f || c == 0x7f {
            continue;
        }
        if c == SP {
            w.extend_from_slice(&[u(b'%'), u(b'2'), u(b'0')]);
        } else {
            w.push(c);
        }
    }
    esc(&w)
}

fn is_ascii_punct(c: u16) -> bool {
    c < 0x80 && (c as u8).is_ascii_punctuation()
}

fn unescape_md(s: &[u16]) -> W {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == BS && i + 1 < s.len() && is_ascii_punct(s[i + 1]) {
            out.push(s[i + 1]);
            i += 2;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

// ------------------------------------------------------------------ inlines

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Em,
    Strong,
    Del,
}

impl Tag {
    fn name(self) -> &'static str {
        match self {
            Tag::Em => "em",
            Tag::Strong => "strong",
            Tag::Del => "del",
        }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Text(W),
    Code(W),
    Br,
    Wrap(Tag, Vec<Node>),
    Link {
        href: W,
        title: Option<W>,
        c: Vec<Node>,
    },
    Img {
        src: W,
        alt: W,
        title: Option<W>,
    },
}

#[derive(Debug, Clone)]
struct Delim {
    ch: u16,
    n: usize,
    orig: usize,
    open: bool,
    close: bool,
}

#[derive(Debug, Clone)]
enum Item {
    Node(Node),
    Delim(Delim),
}

fn is_punct(c: Option<u16>) -> bool {
    let Some(c) = c else { return false };
    PUNCT_RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn is_space(c: Option<u16>) -> bool {
    c.is_none_or(is_ws)
}

fn run_len(s: &[u16], from: usize, c: u16) -> usize {
    s[from.min(s.len())..]
        .iter()
        .take_while(|&&x| x == c)
        .count()
}

/// Index of the `]` matching an already-opened `[` (scan starts at `from`).
fn find_close_bracket(s: &[u16], from: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut j = from;
    while j < s.len() {
        let c = s[j];
        if c == BS {
            j += 1;
        } else if c == BT {
            let run = run_len(s, j, BT).max(1);
            let end = js::index_of(s, &s[j..j + run], j + run);
            j = match end {
                None => j + run - 1,
                Some(e) => e + run - 1,
            };
        } else if c == u(b'[') {
            depth += 1;
        } else if c == u(b']') {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

struct Dest {
    href: W,
    title: Option<W>,
    end: usize,
}

/// Parse `(dest "title")` starting at the `(` at s[k].
fn parse_destination(s: &[u16], k: usize) -> Option<Dest> {
    let ws = |mut i: usize| {
        while i < s.len() && (s[i] == SP || s[i] == TAB || s[i] == NL) {
            i += 1;
        }
        i
    };
    let mut i = ws(k + 1);
    let href;
    if s.get(i) == Some(&u(b'<')) {
        let mut j = i + 1;
        while j < s.len() && s[j] != u(b'>') && s[j] != u(b'<') && s[j] != NL {
            j += 1;
        }
        if s.get(j) != Some(&u(b'>')) {
            return None;
        }
        href = unescape_md(&s[i + 1..j]);
        i = j + 1;
    } else {
        let start = i;
        let mut depth = 0usize;
        while i < s.len() {
            let c = s[i];
            if c == BS && i + 1 < s.len() {
                i += 2;
                continue;
            }
            if is_ws(c) || c <= 0x1f {
                break;
            }
            if c == u(b'(') {
                depth += 1;
            } else if c == u(b')') {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            i += 1;
        }
        if depth != 0 {
            return None;
        }
        href = unescape_md(&s[start..i]);
    }
    let after_dest = i;
    i = ws(i);
    if s.get(i) == Some(&u(b')')) {
        return Some(Dest {
            href,
            title: None,
            end: i + 1,
        });
    }
    let close = match s.get(i) {
        Some(&c) if c == u(b'"') => u(b'"'),
        Some(&c) if c == u(b'\'') => u(b'\''),
        Some(&c) if c == u(b'(') => u(b')'),
        _ => return None,
    };
    if i == after_dest {
        return None;
    }
    let mut j = i + 1;
    while j < s.len() && s[j] != close {
        j += if s[j] == BS { 2 } else { 1 };
    }
    if j >= s.len() {
        return None;
    }
    let title = unescape_md(&s[i + 1..j]);
    let j = ws(j + 1);
    if s.get(j) == Some(&u(b')')) {
        Some(Dest {
            href,
            title: Some(title),
            end: j + 1,
        })
    } else {
        None
    }
}

fn is_alpha(c: u16) -> bool {
    c < 0x80 && (c as u8).is_ascii_alphabetic()
}
fn is_alnum(c: u16) -> bool {
    c < 0x80 && (c as u8).is_ascii_alphanumeric()
}

/// `^<([A-Za-z][A-Za-z0-9+.-]{1,31}:[^\s<>]*)>` on `s`: (match length, url).
fn match_autolink(s: &[u16]) -> Option<(usize, &[u16])> {
    if s.first() != Some(&u(b'<')) || !s.get(1).is_some_and(|&c| is_alpha(c)) {
        return None;
    }
    let mut i = 2;
    while i < s.len() && (is_alnum(s[i]) || s[i] == u(b'+') || s[i] == u(b'.') || s[i] == u(b'-')) {
        i += 1;
    }
    let k = i - 2;
    if !(1..=31).contains(&k) || s.get(i) != Some(&u(b':')) {
        return None;
    }
    i += 1;
    while i < s.len() && !is_ws(s[i]) && s[i] != u(b'<') && s[i] != u(b'>') {
        i += 1;
    }
    if s.get(i) != Some(&u(b'>')) {
        return None;
    }
    Some((i + 1, &s[1..i]))
}

/// `^<([A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)>` on `s`.
fn match_mail(s: &[u16]) -> Option<(usize, &[u16])> {
    if s.first() != Some(&u(b'<')) {
        return None;
    }
    let local = |c: u16| is_alnum(c) || [b'.', b'_', b'%', b'+', b'-'].iter().any(|&b| c == u(b));
    let dom = |c: u16| is_alnum(c) || c == u(b'-');
    let mut i = 1;
    while i < s.len() && local(s[i]) {
        i += 1;
    }
    if i == 1 || s.get(i) != Some(&u(b'@')) {
        return None;
    }
    i += 1;
    let ds = i;
    while i < s.len() && (dom(s[i]) || s[i] == u(b'.')) {
        i += 1;
    }
    if s.get(i) != Some(&u(b'>')) {
        return None;
    }
    let mut parts = 0;
    for p in s[ds..i].split(|&c| c == u(b'.')) {
        if p.is_empty() {
            return None;
        }
        parts += 1;
    }
    if parts < 2 {
        return None;
    }
    Some((i + 1, &s[1..i]))
}

fn flush(buf: &mut W, items: &mut Vec<Item>) {
    if !buf.is_empty() {
        items.push(Item::Node(Node::Text(std::mem::take(buf))));
    }
}

fn parse_inline(s: &[u16], depth: usize) -> Vec<Node> {
    let mut items: Vec<Item> = Vec::new();
    let mut buf: W = Vec::new();

    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        let next = s.get(i + 1).copied();

        if c == BS {
            if next == Some(NL) {
                flush(&mut buf, &mut items);
                items.push(Item::Node(Node::Br));
                i += 2;
            } else if let Some(nx) = next.filter(|&x| is_ascii_punct(x)) {
                buf.push(nx);
                i += 2;
            } else {
                buf.push(c);
                i += 1;
            }
        } else if c == NL {
            let spaces = buf.iter().rev().take_while(|&&x| x == SP).count();
            buf.truncate(buf.len() - spaces);
            if spaces >= 2 {
                flush(&mut buf, &mut items);
                items.push(Item::Node(Node::Br));
            } else {
                buf.push(NL);
            }
            i += 1;
        } else if c == BT {
            let run = run_len(s, i, BT).max(1);
            let mut end = None;
            let mut j = js::index_of_unit(s, BT, i + run);
            while let Some(jj) = j {
                let r = run_len(s, jj, BT).max(1);
                if r == run {
                    end = Some(jj);
                    break;
                }
                j = js::index_of_unit(s, BT, jj + r);
            }
            match end {
                None => {
                    buf.extend_from_slice(&s[i..i + run]);
                    i += run;
                }
                Some(end) => {
                    let mut code: W = s[i + run..end]
                        .iter()
                        .map(|&x| if x == NL { SP } else { x })
                        .collect();
                    if code.len() > 2
                        && code[0] == SP
                        && code[code.len() - 1] == SP
                        && !js::trim(&code).is_empty()
                    {
                        code = code[1..code.len() - 1].to_vec();
                    }
                    flush(&mut buf, &mut items);
                    items.push(Item::Node(Node::Code(code)));
                    i = end + run;
                }
            }
        } else if c == u(b'*') || c == u(b'_') || c == u(b'~') {
            let n = run_len(s, i, c);
            if c == u(b'~') && n != 2 {
                buf.extend(std::iter::repeat_n(c, n));
                i += n;
                continue;
            }
            let before = if i > 0 { Some(s[i - 1]) } else { None };
            let after = s.get(i + n).copied();
            let left =
                !is_space(after) && (!is_punct(after) || is_space(before) || is_punct(before));
            let right =
                !is_space(before) && (!is_punct(before) || is_space(after) || is_punct(after));
            let open = if c == u(b'_') {
                left && (!right || is_punct(before))
            } else {
                left
            };
            let close = if c == u(b'_') {
                right && (!left || is_punct(after))
            } else {
                right
            };
            flush(&mut buf, &mut items);
            items.push(Item::Delim(Delim {
                ch: c,
                n,
                orig: n,
                open,
                close,
            }));
            i += n;
        } else if c == u(b'[') || (c == u(b'!') && next == Some(u(b'['))) {
            let is_img = c == u(b'!');
            let label_start = i + if is_img { 2 } else { 1 };
            let close_idx = if depth < MAX_DEPTH {
                find_close_bracket(s, label_start)
            } else {
                None
            };
            let dest = close_idx.and_then(|ci| {
                if s.get(ci + 1) == Some(&u(b'(')) {
                    parse_destination(s, ci + 1)
                } else {
                    None
                }
            });
            let (Some(close_idx), Some(dest)) = (close_idx, dest) else {
                if is_img {
                    buf.extend_from_slice(&[u(b'!'), u(b'[')]);
                } else {
                    buf.push(u(b'['));
                }
                i = label_start;
                continue;
            };
            let children = parse_inline(&s[label_start..close_idx], depth + 1);
            flush(&mut buf, &mut items);
            if is_img {
                let alt = nodes_to_text(&children);
                if is_safe_url(&dest.href) {
                    items.push(Item::Node(Node::Img {
                        src: dest.href,
                        alt,
                        title: dest.title,
                    }));
                } else {
                    items.push(Item::Node(Node::Text(alt)));
                }
            } else if is_safe_url(&dest.href) {
                items.push(Item::Node(Node::Link {
                    href: dest.href,
                    title: dest.title,
                    c: children,
                }));
            } else {
                items.extend(children.into_iter().map(Item::Node));
            }
            i = dest.end;
        } else if c == u(b'<') {
            let rest = &s[i..];
            if let Some((len, url)) = match_autolink(rest).filter(|(_, url)| is_safe_url(url)) {
                flush(&mut buf, &mut items);
                items.push(Item::Node(Node::Link {
                    href: url.to_vec(),
                    title: None,
                    c: vec![Node::Text(url.to_vec())],
                }));
                i += len;
            } else if let Some((len, addr)) = match_mail(rest) {
                flush(&mut buf, &mut items);
                let mut href = js::u16s("mailto:");
                href.extend_from_slice(addr);
                items.push(Item::Node(Node::Link {
                    href,
                    title: None,
                    c: vec![Node::Text(addr.to_vec())],
                }));
                i += len;
            } else {
                buf.push(c);
                i += 1;
            }
        } else {
            buf.push(c);
            i += 1;
        }
    }
    flush(&mut buf, &mut items);
    resolve_emphasis(items)
}

fn delim_to_text(it: Item) -> Node {
    match it {
        Item::Delim(d) => Node::Text(vec![d.ch; d.n]),
        Item::Node(n) => n,
    }
}

fn delim(items: &[Item], k: usize) -> Option<&Delim> {
    match items.get(k) {
        Some(Item::Delim(d)) => Some(d),
        _ => None,
    }
}

fn delim_mut(items: &mut [Item], k: usize) -> Option<&mut Delim> {
    match items.get_mut(k) {
        Some(Item::Delim(d)) => Some(d),
        _ => None,
    }
}

/// CommonMark-style delimiter matching, simplified.
fn resolve_emphasis(mut items: Vec<Item>) -> Vec<Node> {
    let mut ci = 0;
    while ci < items.len() {
        if !delim(&items, ci).is_some_and(|d| d.close) {
            ci += 1;
            continue;
        }
        while let Some(closer) = delim(&items, ci) {
            if closer.n == 0 {
                break;
            }
            let (cch, cn, corig, copen) = (closer.ch, closer.n, closer.orig, closer.open);
            let mut oi = None;
            for k in (0..ci).rev() {
                let Some(o) = delim(&items, k) else { continue };
                if o.ch != cch || !o.open || o.n == 0 {
                    continue;
                }
                let rule3 = (o.close || copen)
                    && (o.orig + corig) % 3 == 0
                    && !(o.orig % 3 == 0 && corig % 3 == 0);
                if rule3 {
                    continue;
                }
                oi = Some(k);
                break;
            }
            let Some(oi) = oi else { break };
            let on = delim(&items, oi).map_or(0, |d| d.n);
            let used = if cch == u(b'~') || (on >= 2 && cn >= 2) {
                2
            } else {
                1
            };
            let tag = if cch == u(b'~') {
                Tag::Del
            } else if used == 2 {
                Tag::Strong
            } else {
                Tag::Em
            };
            let inner: Vec<Node> = items.drain(oi + 1..ci).map(delim_to_text).collect();
            items.insert(oi + 1, Item::Node(Node::Wrap(tag, inner)));
            ci = oi + 2;
            let mut opener_gone = false;
            if let Some(o) = delim_mut(&mut items, oi) {
                o.n = o.n.saturating_sub(used);
                opener_gone = o.n == 0;
            }
            let mut closer_gone = false;
            if let Some(c) = delim_mut(&mut items, ci) {
                c.n = c.n.saturating_sub(used);
                closer_gone = c.n == 0;
            }
            if opener_gone {
                items.remove(oi);
                ci -= 1;
            }
            if closer_gone {
                items.remove(ci);
                ci -= 1;
                break;
            }
        }
        ci += 1;
    }
    items.into_iter().map(delim_to_text).collect()
}

fn nodes_to_text(nodes: &[Node]) -> W {
    let mut out = Vec::new();
    text_into(nodes, &mut out);
    out
}

fn text_into(nodes: &[Node], out: &mut W) {
    for n in nodes {
        match n {
            Node::Text(v) | Node::Code(v) => out.extend_from_slice(v),
            Node::Br => out.push(SP),
            Node::Img { alt, .. } => out.extend_from_slice(alt),
            Node::Wrap(_, c) | Node::Link { c, .. } => text_into(c, out),
        }
    }
}

fn nodes_to_html(nodes: &[Node]) -> String {
    let mut out = String::new();
    html_into(nodes, &mut out);
    out
}

fn html_into(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(v) => escape_into(v, out),
            Node::Code(v) => {
                out.push_str("<code>");
                escape_into(v, out);
                out.push_str("</code>");
            }
            Node::Br => out.push_str("<br>\n"),
            Node::Wrap(tag, c) => {
                out.push('<');
                out.push_str(tag.name());
                out.push('>');
                html_into(c, out);
                out.push_str("</");
                out.push_str(tag.name());
                out.push('>');
            }
            Node::Link { href, title, c } => {
                out.push_str("<a href=\"");
                out.push_str(&encode_url(href));
                out.push('"');
                if let Some(t) = title {
                    out.push_str(" title=\"");
                    escape_into(t, out);
                    out.push('"');
                }
                out.push('>');
                html_into(c, out);
                out.push_str("</a>");
            }
            Node::Img { src, alt, title } => {
                out.push_str("<img src=\"");
                out.push_str(&encode_url(src));
                out.push_str("\" alt=\"");
                escape_into(alt, out);
                out.push('"');
                if let Some(t) = title {
                    out.push_str(" title=\"");
                    escape_into(t, out);
                    out.push('"');
                }
                out.push('>');
            }
        }
    }
}

// ------------------------------------------------------------------- blocks

struct Block {
    html: String,
    /// Inline HTML when this block is a paragraph (lets tight lists drop the <p>).
    para: Option<String>,
    blank_before: bool,
}

impl Block {
    fn new(html: String) -> Block {
        Block {
            html,
            para: None,
            blank_before: false,
        }
    }
}

struct State {
    excerpt: Option<W>,
}

fn indent_of(l: &[u16]) -> usize {
    l.iter().take_while(|&&c| c == SP).count()
}

fn is_blank(l: &[u16]) -> bool {
    js::trim(l).is_empty()
}

/// Would `.*` match all of `s` (no line terminators)?
fn dot_all(s: &[u16]) -> bool {
    !s.iter()
        .any(|&c| matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029))
}

/// `^ {0,3}`: the number of leading spaces when at most three.
fn lead3(l: &[u16]) -> Option<usize> {
    let n = indent_of(l);
    (n <= 3).then_some(n)
}

struct Fence {
    indent: usize,
    ch: u16,
    len: usize,
    info: W,
}

/// `FENCE_RE` plus the "no backtick in a backtick fence's info" rule.
fn open_fence(l: &[u16]) -> Option<Fence> {
    let n = lead3(l)?;
    let ch = *l.get(n)?;
    if ch != BT && ch != u(b'~') {
        return None;
    }
    let len = run_len(l, n, ch);
    if len < 3 {
        return None;
    }
    let rest = &l[n + len..];
    if !dot_all(rest) || (ch == BT && rest.contains(&BT)) {
        return None;
    }
    let t = js::trim(rest);
    let end = t.iter().position(|&c| is_ws(c)).unwrap_or(t.len());
    Some(Fence {
        indent: n,
        ch,
        len,
        info: t[..end].to_vec(),
    })
}

/// `^ {0,3}<ch>{len,}[ \t]*$`
fn closes_fence(l: &[u16], ch: u16, len: usize) -> bool {
    let Some(n) = lead3(l) else { return false };
    let r = run_len(l, n, ch);
    r >= len && l[n + r..].iter().all(|&c| c == SP || c == TAB)
}

/// `ATX_RE`: (level, raw content).
fn atx(l: &[u16]) -> Option<(usize, &[u16])> {
    let n = lead3(l)?;
    let h = run_len(l, n, u(b'#'));
    if !(1..=6).contains(&h) {
        return None;
    }
    let after = &l[n + h..];
    if after.is_empty() {
        return Some((h, after));
    }
    if after[0] != SP && after[0] != TAB {
        return None;
    }
    let skip = after.iter().take_while(|&&c| c == SP || c == TAB).count();
    let rest = &after[skip..];
    if !dot_all(rest) {
        return None;
    }
    Some((h, rest))
}

fn is_sp_tab(c: &u16) -> bool {
    *c == SP || *c == TAB
}

/// `content.replace(/(^|[ \t]+)#+[ \t]*$/, '').trim()`
fn atx_content(s: &[u16]) -> &[u16] {
    let e = s.len() - s.iter().rev().take_while(|c| is_sp_tab(c)).count();
    let h = e - s[..e].iter().rev().take_while(|&&c| c == u(b'#')).count();
    let cut = if h == e {
        s
    } else if h == 0 {
        &s[..0]
    } else if is_sp_tab(&s[h - 1]) {
        let w = h - s[..h].iter().rev().take_while(|c| is_sp_tab(c)).count();
        &s[..w]
    } else {
        s
    };
    js::trim(cut)
}

fn is_hr(l: &[u16]) -> bool {
    let Some(n) = lead3(l) else { return false };
    let Some(&c) = l.get(n) else { return false };
    if c != u(b'-') && c != u(b'*') && c != u(b'_') {
        return false;
    }
    let rest = &l[n + 1..];
    rest.iter().all(|&x| x == c || x == SP || x == TAB)
        && rest.iter().filter(|&&x| x == c).count() >= 2
}

struct ListM<'a> {
    base: usize,
    marker: &'a [u16],
    spaces: usize,
    rest: &'a [u16],
}

fn is_digit(c: u16) -> bool {
    (u(b'0')..=u(b'9')).contains(&c)
}

impl ListM<'_> {
    fn key(&self) -> u16 {
        self.marker[self.marker.len() - 1]
    }
    fn ordered(&self) -> bool {
        is_digit(self.marker[0])
    }
    fn number(&self) -> u64 {
        self.marker
            .iter()
            .take_while(|&&c| is_digit(c))
            .fold(0u64, |a, &c| a * 10 + (c - u(b'0')) as u64)
    }
}

/// `LIST_RE = /^( {0,3})([-*+]|\d{1,9}[.)])( +|$)(.*)$/`
fn list_m(l: &[u16]) -> Option<ListM<'_>> {
    let n = lead3(l)?;
    let c = *l.get(n)?;
    let mlen = if c == u(b'-') || c == u(b'*') || c == u(b'+') {
        1
    } else {
        let d = l[n..].iter().take_while(|&&x| is_digit(x)).count();
        if !(1..=9).contains(&d) {
            return None;
        }
        let t = *l.get(n + d)?;
        if t != u(b'.') && t != u(b')') {
            return None;
        }
        d + 1
    };
    let after = n + mlen;
    let spaces = run_len(l, after, SP);
    if spaces == 0 && after < l.len() {
        return None;
    }
    let rest = &l[after + spaces..];
    if !dot_all(rest) {
        return None;
    }
    Some(ListM {
        base: n,
        marker: &l[n..after],
        spaces,
        rest,
    })
}

fn is_quote(l: &[u16]) -> bool {
    lead3(l).is_some_and(|n| l.get(n) == Some(&u(b'>')))
}

/// `^ {0,3}<ch>+[ \t]*$`
fn is_setext(l: &[u16], ch: u8) -> bool {
    let Some(n) = lead3(l) else { return false };
    let r = run_len(l, n, u(ch));
    r >= 1 && l[n + r..].iter().all(is_sp_tab)
}

/// Can this line interrupt a paragraph?
fn interrupts_paragraph(l: &[u16]) -> bool {
    if open_fence(l).is_some() || atx(l).is_some() || is_hr(l) || is_quote(l) {
        return true;
    }
    match list_m(l) {
        Some(m) => !m.rest.is_empty() && (!m.ordered() || m.number() == 1),
        None => false,
    }
}

fn join_lines<'a>(lines: impl Iterator<Item = &'a [u16]>) -> W {
    let mut out = Vec::new();
    for (k, l) in lines.enumerate() {
        if k > 0 {
            out.push(NL);
        }
        out.extend_from_slice(l);
    }
    out
}

fn make_paragraph(lines: &[&[u16]], st: &mut State) -> Block {
    let joined = join_lines(lines.iter().map(|l| js::trim_start(l)));
    let text = js::trim_end(&joined);
    let nodes = parse_inline(text, 0);
    if st.excerpt.is_none() {
        let raw = nodes_to_text(&nodes);
        let mut plain: W = Vec::with_capacity(raw.len());
        let mut in_ws = false;
        for &c in &raw {
            if is_ws(c) {
                if !in_ws {
                    plain.push(SP);
                }
                in_ws = true;
            } else {
                plain.push(c);
                in_ws = false;
            }
        }
        let plain = js::trim(&plain);
        if !plain.is_empty() {
            st.excerpt = Some(plain.to_vec());
        }
    }
    let inner = nodes_to_html(&nodes);
    Block {
        html: format!("<p>{inner}</p>"),
        para: Some(inner),
        blank_before: false,
    }
}

fn code_block(body: &[&[u16]], info: &[u16]) -> Block {
    let safe = !info.is_empty()
        && info
            .iter()
            .all(|&c| is_alnum(c) || c == u(b'_') || c == u(b'+') || c == u(b'-'));
    let cls = if safe {
        format!(" class=\"language-{}\"", js::string(info))
    } else {
        String::new()
    };
    let text = if body.is_empty() {
        String::new()
    } else {
        esc(&join_lines(body.iter().copied())) + "\n"
    };
    Block::new(format!("<pre><code{cls}>{text}</code></pre>"))
}

fn push(out: &mut Vec<Block>, blank: &mut bool, mut b: Block) {
    b.blank_before = *blank && !out.is_empty();
    out.push(b);
    *blank = false;
}

fn parse_blocks(lines: &[&[u16]], st: &mut State, depth: usize) -> Vec<Block> {
    if depth > MAX_DEPTH {
        let text: Vec<&[u16]> = lines.iter().copied().filter(|l| !is_blank(l)).collect();
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![make_paragraph(&text, st)]
        };
    }
    let mut out: Vec<Block> = Vec::new();
    let mut blank = false;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) {
            blank = true;
            i += 1;
            continue;
        }

        // fenced code
        if let Some(f) = open_fence(line) {
            let mut body: Vec<&[u16]> = Vec::new();
            i += 1;
            while i < lines.len() && !closes_fence(lines[i], f.ch, f.len) {
                let l = lines[i];
                body.push(&l[f.indent.min(indent_of(l))..]);
                i += 1;
            }
            i += 1; // closing fence (or past the end for an unterminated one)
            push(&mut out, &mut blank, code_block(&body, &f.info));
            continue;
        }

        // ATX heading
        if let Some((level, content)) = atx(line) {
            let inner = nodes_to_html(&parse_inline(atx_content(content), 0));
            push(
                &mut out,
                &mut blank,
                Block::new(format!("<h{level}>{inner}</h{level}>")),
            );
            i += 1;
            continue;
        }

        if is_hr(line) {
            push(&mut out, &mut blank, Block::new("<hr>".into()));
            i += 1;
            continue;
        }

        // blockquote
        if is_quote(line) {
            let mut inner: Vec<&[u16]> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if is_quote(l) {
                    let mut s = indent_of(l) + 1;
                    if l.get(s) == Some(&SP) {
                        s += 1;
                    }
                    inner.push(&l[s..]);
                } else if !is_blank(l)
                    && inner.last().is_some_and(|p| !is_blank(p))
                    && !interrupts_paragraph(l)
                {
                    inner.push(l); // lazy continuation
                } else {
                    break;
                }
                i += 1;
            }
            let blocks = parse_blocks(&inner, st, depth + 1);
            let mut html = String::from("<blockquote>\n");
            for b in &blocks {
                html.push_str(&b.html);
                html.push('\n');
            }
            html.push_str("</blockquote>");
            push(&mut out, &mut blank, Block::new(html));
            continue;
        }

        // list
        if list_m(line).is_some() {
            let (block, next) = parse_list(lines, i, st, depth);
            push(&mut out, &mut blank, block);
            i = next;
            continue;
        }

        // indented code
        if indent_of(line) >= 4 {
            let mut body: Vec<&[u16]> = Vec::new();
            while i < lines.len() && (is_blank(lines[i]) || indent_of(lines[i]) >= 4) {
                let l = lines[i];
                body.push(&l[4.min(l.len())..]);
                i += 1;
            }
            while body.last().is_some_and(|l| is_blank(l)) {
                body.pop();
            }
            push(&mut out, &mut blank, code_block(&body, &[]));
            continue;
        }

        // paragraph / setext heading
        let mut para: Vec<&[u16]> = vec![line];
        i += 1;
        let mut setext = 0;
        while i < lines.len() {
            let l = lines[i];
            if is_blank(l) {
                break;
            }
            if is_setext(l, b'=') {
                setext = 1;
            } else if is_setext(l, b'-') {
                setext = 2;
            }
            if setext != 0 {
                i += 1;
                break;
            }
            if interrupts_paragraph(l) {
                break;
            }
            para.push(l);
            i += 1;
        }
        let p = make_paragraph(&para, st); // a setext heading records the excerpt like a paragraph
        if setext != 0 {
            let inner = p.para.unwrap_or_default();
            push(
                &mut out,
                &mut blank,
                Block::new(format!("<h{setext}>{inner}</h{setext}>")),
            );
        } else {
            push(&mut out, &mut blank, p);
        }
    }
    out
}

fn parse_list(lines: &[&[u16]], start: usize, st: &mut State, depth: usize) -> (Block, usize) {
    let Some(first) = list_m(lines[start]) else {
        return (Block::new(String::new()), start + 1);
    };
    let key = first.key();
    let ordered = first.ordered();
    let start_num = if ordered { first.number() } else { 1 };
    let mut items: Vec<Vec<W>> = Vec::new();
    let mut loose = false;
    let mut i = start;

    let sibling_at = |idx: usize| -> Option<ListM<'_>> {
        let l = *lines.get(idx)?;
        if is_hr(l) {
            return None;
        }
        list_m(l).filter(|m| m.key() == key)
    };

    while i < lines.len() {
        let Some(m) = sibling_at(i) else { break };
        let marker_len = m.marker.len();
        let mut rest: W = m.rest.to_vec();
        let content_indent;
        if rest.is_empty() {
            content_indent = m.base + marker_len + 1;
        } else if m.spaces > 4 {
            content_indent = m.base + marker_len + 1;
            let mut r = vec![SP; m.spaces - 1];
            r.extend_from_slice(&rest);
            rest = r;
        } else {
            content_indent = m.base + marker_len + m.spaces;
        }

        let mut body: Vec<W> = vec![rest];
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            if is_blank(l) {
                let mut j = i;
                while j < lines.len() && is_blank(lines[j]) {
                    j += 1;
                }
                if j < lines.len() && indent_of(lines[j]) >= content_indent {
                    while i < j {
                        body.push(Vec::new());
                        i += 1;
                    }
                    continue;
                }
                break;
            }
            if indent_of(l) >= content_indent {
                body.push(l[content_indent..].to_vec());
            } else if sibling_at(i).is_some()
                || list_m(l).is_some()
                || is_hr(l)
                || interrupts_paragraph(l)
                || body.last().is_none_or(|b| is_blank(b))
            {
                break;
            } else {
                body.push(js::trim_start(l).to_vec()); // lazy continuation
            }
            i += 1;
        }
        items.push(body);

        let mut j = i;
        while j < lines.len() && is_blank(lines[j]) {
            j += 1;
        }
        if j > i && sibling_at(j).is_some() {
            loose = true;
            i = j;
        } else if j == i {
            continue;
        } else {
            break;
        }
    }

    let rendered: Vec<Vec<Block>> = items
        .iter()
        .map(|body| {
            let refs: Vec<&[u16]> = body.iter().map(Vec::as_slice).collect();
            parse_blocks(&refs, st, depth + 1)
        })
        .collect();
    if rendered
        .iter()
        .any(|blocks| blocks.iter().skip(1).any(|b| b.blank_before))
    {
        loose = true;
    }

    let lis: Vec<String> = rendered
        .iter()
        .map(|blocks| {
            let Some(last) = blocks.last() else {
                return "<li></li>".to_string();
            };
            let mut inner = String::new();
            for (k, b) in blocks.iter().enumerate() {
                match &b.para {
                    Some(p) if !loose => {
                        if k > 0 {
                            inner.push('\n');
                        }
                        inner.push_str(p);
                    }
                    _ => {
                        inner.push('\n');
                        inner.push_str(&b.html);
                    }
                }
            }
            if loose || last.para.is_none() {
                inner.push('\n');
            }
            format!("<li>{inner}</li>")
        })
        .collect();
    let tag = if ordered { "ol" } else { "ul" };
    let attr = if ordered && start_num != 1 {
        format!(" start=\"{start_num}\"")
    } else {
        String::new()
    };
    (
        Block::new(format!("<{tag}{attr}>\n{}\n</{tag}>", lis.join("\n"))),
        i,
    )
}

// ------------------------------------------------------------------ excerpt

fn truncate(text: &[u16]) -> String {
    if text.len() <= EXCERPT_MAX {
        return js::string(text);
    }
    let mut cut = &text[..EXCERPT_MAX - 1];
    if text[EXCERPT_MAX - 1] != SP {
        if let Some(sp) = cut.iter().rposition(|&c| c == SP).filter(|&sp| sp > 0) {
            cut = &cut[..sp];
        }
    }
    cut = js::trim_end(cut);
    if cut.last().is_some_and(|&c| (0xd800..=0xdbff).contains(&c)) {
        cut = &cut[..cut.len() - 1];
    }
    js::string(cut) + "…"
}

/// Render Markdown to HTML (byte-identical to the TypeScript renderer) and
/// extract the first paragraph's text as an excerpt.
pub fn render_markdown(src: &str) -> MarkdownResult {
    // \r\n? -> \n, NUL -> U+FFFD, then split into lines with leading tabs expanded.
    let units = js::u16s(src);
    let mut norm: W = Vec::with_capacity(units.len());
    let mut k = 0;
    while k < units.len() {
        let c = units[k];
        if c == u(b'\r') {
            norm.push(NL);
            if units.get(k + 1) == Some(&NL) {
                k += 1;
            }
        } else if c == 0 {
            norm.push(0xfffd);
        } else {
            norm.push(c);
        }
        k += 1;
    }
    let lines: Vec<W> = norm
        .split(|&c| c == NL)
        .map(|l| {
            let lead = l.iter().take_while(|c| is_sp_tab(c)).count();
            let mut out = Vec::with_capacity(l.len() + lead * 3);
            for &c in &l[..lead] {
                if c == TAB {
                    out.extend_from_slice(&[SP; 4]);
                } else {
                    out.push(SP);
                }
            }
            out.extend_from_slice(&l[lead..]);
            out
        })
        .collect();
    let refs: Vec<&[u16]> = lines.iter().map(Vec::as_slice).collect();
    let mut st = State { excerpt: None };
    let blocks = parse_blocks(&refs, &mut st, 0);
    let mut html = String::new();
    for b in &blocks {
        html.push_str(&b.html);
        html.push('\n');
    }
    MarkdownResult {
        html,
        excerpt: truncate(st.excerpt.as_deref().unwrap_or(&[])),
    }
}

/// BMP code units in Unicode categories P or S (`/[\p{P}\p{S}]/u`), generated
/// with Node v26.11.1 (Unicode 17.0). Surrogates are excluded: a lone surrogate
/// is neither.
#[rustfmt::skip]
const PUNCT_RANGES: [(u16, u16); 205] = [
    (0x0021, 0x002f), (0x003a, 0x0040), (0x005b, 0x0060), (0x007b, 0x007e), (0x00a1, 0x00a9), (0x00ab, 0x00ac),
    (0x00ae, 0x00b1), (0x00b4, 0x00b4), (0x00b6, 0x00b8), (0x00bb, 0x00bb), (0x00bf, 0x00bf), (0x00d7, 0x00d7),
    (0x00f7, 0x00f7), (0x02c2, 0x02c5), (0x02d2, 0x02df), (0x02e5, 0x02eb), (0x02ed, 0x02ed), (0x02ef, 0x02ff),
    (0x0375, 0x0375), (0x037e, 0x037e), (0x0384, 0x0385), (0x0387, 0x0387), (0x03f6, 0x03f6), (0x0482, 0x0482),
    (0x055a, 0x055f), (0x0589, 0x058a), (0x058d, 0x058f), (0x05be, 0x05be), (0x05c0, 0x05c0), (0x05c3, 0x05c3),
    (0x05c6, 0x05c6), (0x05f3, 0x05f4), (0x0606, 0x060f), (0x061b, 0x061b), (0x061d, 0x061f), (0x066a, 0x066d),
    (0x06d4, 0x06d4), (0x06de, 0x06de), (0x06e9, 0x06e9), (0x06fd, 0x06fe), (0x0700, 0x070d), (0x07f6, 0x07f9),
    (0x07fe, 0x07ff), (0x0830, 0x083e), (0x085e, 0x085e), (0x0888, 0x0888), (0x0964, 0x0965), (0x0970, 0x0970),
    (0x09f2, 0x09f3), (0x09fa, 0x09fb), (0x09fd, 0x09fd), (0x0a76, 0x0a76), (0x0af0, 0x0af1), (0x0b70, 0x0b70),
    (0x0bf3, 0x0bfa), (0x0c77, 0x0c77), (0x0c7f, 0x0c7f), (0x0c84, 0x0c84), (0x0d4f, 0x0d4f), (0x0d79, 0x0d79),
    (0x0df4, 0x0df4), (0x0e3f, 0x0e3f), (0x0e4f, 0x0e4f), (0x0e5a, 0x0e5b), (0x0f01, 0x0f17), (0x0f1a, 0x0f1f),
    (0x0f34, 0x0f34), (0x0f36, 0x0f36), (0x0f38, 0x0f38), (0x0f3a, 0x0f3d), (0x0f85, 0x0f85), (0x0fbe, 0x0fc5),
    (0x0fc7, 0x0fcc), (0x0fce, 0x0fda), (0x104a, 0x104f), (0x109e, 0x109f), (0x10fb, 0x10fb), (0x1360, 0x1368),
    (0x1390, 0x1399), (0x1400, 0x1400), (0x166d, 0x166e), (0x169b, 0x169c), (0x16eb, 0x16ed), (0x1735, 0x1736),
    (0x17d4, 0x17d6), (0x17d8, 0x17db), (0x1800, 0x180a), (0x1940, 0x1940), (0x1944, 0x1945), (0x19de, 0x19ff),
    (0x1a1e, 0x1a1f), (0x1aa0, 0x1aa6), (0x1aa8, 0x1aad), (0x1b4e, 0x1b4f), (0x1b5a, 0x1b6a), (0x1b74, 0x1b7f),
    (0x1bfc, 0x1bff), (0x1c3b, 0x1c3f), (0x1c7e, 0x1c7f), (0x1cc0, 0x1cc7), (0x1cd3, 0x1cd3), (0x1fbd, 0x1fbd),
    (0x1fbf, 0x1fc1), (0x1fcd, 0x1fcf), (0x1fdd, 0x1fdf), (0x1fed, 0x1fef), (0x1ffd, 0x1ffe), (0x2010, 0x2027),
    (0x2030, 0x205e), (0x207a, 0x207e), (0x208a, 0x208e), (0x20a0, 0x20c1), (0x2100, 0x2101), (0x2103, 0x2106),
    (0x2108, 0x2109), (0x2114, 0x2114), (0x2116, 0x2118), (0x211e, 0x2123), (0x2125, 0x2125), (0x2127, 0x2127),
    (0x2129, 0x2129), (0x212e, 0x212e), (0x213a, 0x213b), (0x2140, 0x2144), (0x214a, 0x214d), (0x214f, 0x214f),
    (0x218a, 0x218b), (0x2190, 0x2429), (0x2440, 0x244a), (0x249c, 0x24e9), (0x2500, 0x2775), (0x2794, 0x2b73),
    (0x2b76, 0x2bff), (0x2ce5, 0x2cea), (0x2cf9, 0x2cfc), (0x2cfe, 0x2cff), (0x2d70, 0x2d70), (0x2e00, 0x2e2e),
    (0x2e30, 0x2e5d), (0x2e80, 0x2e99), (0x2e9b, 0x2ef3), (0x2f00, 0x2fd5), (0x2ff0, 0x2fff), (0x3001, 0x3004),
    (0x3008, 0x3020), (0x3030, 0x3030), (0x3036, 0x3037), (0x303d, 0x303f), (0x309b, 0x309c), (0x30a0, 0x30a0),
    (0x30fb, 0x30fb), (0x3190, 0x3191), (0x3196, 0x319f), (0x31c0, 0x31e5), (0x31ef, 0x31ef), (0x3200, 0x321e),
    (0x322a, 0x3247), (0x3250, 0x3250), (0x3260, 0x327f), (0x328a, 0x32b0), (0x32c0, 0x33ff), (0x4dc0, 0x4dff),
    (0xa490, 0xa4c6), (0xa4fe, 0xa4ff), (0xa60d, 0xa60f), (0xa673, 0xa673), (0xa67e, 0xa67e), (0xa6f2, 0xa6f7),
    (0xa700, 0xa716), (0xa720, 0xa721), (0xa789, 0xa78a), (0xa828, 0xa82b), (0xa836, 0xa839), (0xa874, 0xa877),
    (0xa8ce, 0xa8cf), (0xa8f8, 0xa8fa), (0xa8fc, 0xa8fc), (0xa92e, 0xa92f), (0xa95f, 0xa95f), (0xa9c1, 0xa9cd),
    (0xa9de, 0xa9df), (0xaa5c, 0xaa5f), (0xaa77, 0xaa79), (0xaade, 0xaadf), (0xaaf0, 0xaaf1), (0xab5b, 0xab5b),
    (0xab6a, 0xab6b), (0xabeb, 0xabeb), (0xfb29, 0xfb29), (0xfbb2, 0xfbd2), (0xfd3e, 0xfd4f), (0xfd90, 0xfd91),
    (0xfdc8, 0xfdcf), (0xfdfc, 0xfdff), (0xfe10, 0xfe19), (0xfe30, 0xfe52), (0xfe54, 0xfe66), (0xfe68, 0xfe6b),
    (0xff01, 0xff0f), (0xff1a, 0xff20), (0xff3b, 0xff40), (0xff5b, 0xff65), (0xffe0, 0xffe6), (0xffe8, 0xffee),
    (0xfffc, 0xfffd),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn html(s: &str) -> String {
        render_markdown(s).html
    }
    fn eq(src: &str, expected: &str) {
        assert_eq!(html(src), expected, "input: {src:?}");
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            render_markdown(""),
            MarkdownResult {
                html: String::new(),
                excerpt: String::new()
            }
        );
        assert_eq!(
            render_markdown("\n\n  \n"),
            MarkdownResult {
                html: String::new(),
                excerpt: String::new()
            }
        );
    }

    #[test]
    fn paragraphs_line_endings_headings() {
        eq("one\ntwo\n\nthree", "<p>one\ntwo</p>\n<p>three</p>\n");
        eq("a\r\nb\r\n\r\nc", "<p>a\nb</p>\n<p>c</p>\n");
        eq("# T\r\n\r\ntext", "<h1>T</h1>\n<p>text</p>\n");
        eq(
            "# a\n## b ##\n###### f\n####### no",
            "<h1>a</h1>\n<h2>b</h2>\n<h6>f</h6>\n<p>####### no</p>\n",
        );
        eq(
            "#\n#nospace\n# *x* #",
            "<h1></h1>\n<p>#nospace</p>\n<h1><em>x</em></h1>\n",
        );
        eq("# a # b", "<h1>a # b</h1>\n");
        eq(
            "Title\n=====\n\nSub\n---\n",
            "<h1>Title</h1>\n<h2>Sub</h2>\n",
        );
        eq("multi\nline\n===", "<h1>multi\nline</h1>\n");
    }

    #[test]
    fn hard_breaks_and_thematic_breaks() {
        eq("a  \nb", "<p>a<br>\nb</p>\n");
        eq("a\\\nb", "<p>a<br>\nb</p>\n");
        eq("a \nb", "<p>a\nb</p>\n");
        eq("end  ", "<p>end</p>\n");
        eq("end\\", "<p>end\\</p>\n");
        eq("---\n\n***\n\n___\n\n- - -", "<hr>\n<hr>\n<hr>\n<hr>\n");
        eq("a\n\n--", "<p>a</p>\n<p>--</p>\n");
    }

    #[test]
    fn code_blocks() {
        eq("```js\nlet a = <b> & \"c\";\n\nx\n```", "<pre><code class=\"language-js\">let a = &lt;b&gt; &amp; &quot;c&quot;;\n\nx\n</code></pre>\n");
        eq("~~~\n*not em*\n~~~", "<pre><code>*not em*\n</code></pre>\n");
        eq("````\n```\n````", "<pre><code>```\n</code></pre>\n");
        eq("```\n```", "<pre><code></code></pre>\n");
        eq(
            "```c++ extra\nx\n```",
            "<pre><code class=\"language-c++\">x\n</code></pre>\n",
        );
        eq("```a\"b\nx\n```", "<pre><code>x\n</code></pre>\n");
        eq("```<script>\nx\n```", "<pre><code>x\n</code></pre>\n");
        eq("```\nabc\n\ndef", "<pre><code>abc\n\ndef\n</code></pre>\n");
        eq(
            "> ```\n> x",
            "<blockquote>\n<pre><code>x\n</code></pre>\n</blockquote>\n",
        );
        eq(
            "    a\n\n    b\n\npara",
            "<pre><code>a\n\nb\n</code></pre>\n<p>para</p>\n",
        );
        eq("para\n    cont", "<p>para\ncont</p>\n");
        eq("\ttabbed", "<pre><code>tabbed\n</code></pre>\n");
    }

    #[test]
    fn blockquotes() {
        eq("> a\n> b", "<blockquote>\n<p>a\nb</p>\n</blockquote>\n");
        eq("> a\nlazy", "<blockquote>\n<p>a\nlazy</p>\n</blockquote>\n");
        eq(
            "> # h\n>\n> - x\n>> deep",
            "<blockquote>\n<h1>h</h1>\n<ul>\n<li>x</li>\n</ul>\n<blockquote>\n<p>deep</p>\n</blockquote>\n</blockquote>\n",
        );
        eq(
            "> a\n\n> b",
            "<blockquote>\n<p>a</p>\n</blockquote>\n<blockquote>\n<p>b</p>\n</blockquote>\n",
        );
    }

    #[test]
    fn lists() {
        eq("- a\n- b", "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        eq("* a\n* b", "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        eq("+ a", "<ul>\n<li>a</li>\n</ul>\n");
        eq(
            "- a\n* b",
            "<ul>\n<li>a</li>\n</ul>\n<ul>\n<li>b</li>\n</ul>\n",
        );
        eq("1. a\n2. b", "<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n");
        eq("1) a\n2) b", "<ol>\n<li>a</li>\n<li>b</li>\n</ol>\n");
        eq(
            "3. a\n4. b",
            "<ol start=\"3\">\n<li>a</li>\n<li>b</li>\n</ol>\n",
        );
        eq("0. a", "<ol start=\"0\">\n<li>a</li>\n</ol>\n");
        eq(
            "- a\n\n- b",
            "<ul>\n<li>\n<p>a</p>\n</li>\n<li>\n<p>b</p>\n</li>\n</ul>\n",
        );
        eq(
            "- a\n\n  b\n- c",
            "<ul>\n<li>\n<p>a</p>\n<p>b</p>\n</li>\n<li>\n<p>c</p>\n</li>\n</ul>\n",
        );
        eq("- a\n  - b\n    - c\n- d", "<ul>\n<li>a\n<ul>\n<li>b\n<ul>\n<li>c</li>\n</ul>\n</li>\n</ul>\n</li>\n<li>d</li>\n</ul>\n");
        eq(
            "1. a\n   - x\n2. b",
            "<ol>\n<li>a\n<ul>\n<li>x</li>\n</ul>\n</li>\n<li>b</li>\n</ol>\n",
        );
        let out = html("- a\n  - b\n\n  - c\n- d");
        assert!(out.contains("<li>a\n<ul>\n<li>\n<p>b</p>"), "{out}");
        assert!(out.contains("<li>d</li>"), "{out}");
        eq(
            "- one\n  two\n- three",
            "<ul>\n<li>one\ntwo</li>\n<li>three</li>\n</ul>\n",
        );
        eq("- one\ntwo", "<ul>\n<li>one\ntwo</li>\n</ul>\n");
        eq(
            "- ```\n  x\n  ```",
            "<ul>\n<li>\n<pre><code>x\n</code></pre>\n</li>\n</ul>\n",
        );
        eq("- a\n\npara", "<ul>\n<li>a</li>\n</ul>\n<p>para</p>\n");
        eq("text\n- a", "<p>text</p>\n<ul>\n<li>a</li>\n</ul>\n");
        eq("text\n2. a", "<p>text\n2. a</p>\n");
        eq("text\n1. a", "<p>text</p>\n<ol>\n<li>a</li>\n</ol>\n");
        eq("-\n- b", "<ul>\n<li></li>\n<li>b</li>\n</ul>\n");
    }

    #[test]
    fn inlines() {
        eq(
            "*a* _b_ **c** __d__ ~~e~~",
            "<p><em>a</em> <em>b</em> <strong>c</strong> <strong>d</strong> <del>e</del></p>\n",
        );
        eq("***both***", "<p><em><strong>both</strong></em></p>\n");
        eq("**a *b* c**", "<p><strong>a <em>b</em> c</strong></p>\n");
        eq("a*b*c", "<p>a<em>b</em>c</p>\n");
        eq(
            "snake_case_word and 2*3*4",
            "<p>snake_case_word and 2<em>3</em>4</p>\n",
        );
        eq("foo_bar_", "<p>foo_bar_</p>\n");
        eq("*a", "<p>*a</p>\n");
        eq("**a *b", "<p>**a *b</p>\n");
        eq("a * b * c", "<p>a * b * c</p>\n");
        eq("~single~ ~~~triple~~~", "<p>~single~ ~~~triple~~~</p>\n");
        eq("~~open", "<p>~~open</p>\n");
        eq("\\*not em\\* \\# \\\\ \\a", "<p>*not em* # \\ \\a</p>\n");
        eq("\\<b\\>", "<p>&lt;b&gt;</p>\n");
        eq(
            "`a *b* <i>` ``x ` y``",
            "<p><code>a *b* &lt;i&gt;</code> <code>x ` y</code></p>\n",
        );
        eq("` a `", "<p><code>a</code></p>\n");
        eq("`unclosed", "<p>`unclosed</p>\n");
        eq("`a\nb`", "<p><code>a b</code></p>\n");
    }

    #[test]
    fn links_images_autolinks() {
        eq(
            "[t](http://x.com)",
            "<p><a href=\"http://x.com\">t</a></p>\n",
        );
        eq(
            "[t](/a/b \"T \\\"q\\\"\")",
            "<p><a href=\"/a/b\" title=\"T &quot;q&quot;\">t</a></p>\n",
        );
        eq(
            "[*t*](rel.html 'x')",
            "<p><a href=\"rel.html\" title=\"x\"><em>t</em></a></p>\n",
        );
        eq(
            "[t](#frag) [m](mailto:a@b.c) [p](http://x/(a))",
            "<p><a href=\"#frag\">t</a> <a href=\"mailto:a@b.c\">m</a> <a href=\"http://x/(a)\">p</a></p>\n",
        );
        eq("[t](<a b.html>)", "<p><a href=\"a%20b.html\">t</a></p>\n");
        eq("[t]()", "<p><a href=\"\">t</a></p>\n");
        eq("[t] (x) [u](x", "<p>[t] (x) [u](x</p>\n");
        eq("[unclosed(x)", "<p>[unclosed(x)</p>\n");
        eq(
            "![a *b*](/i.png \"T\")",
            "<p><img src=\"/i.png\" alt=\"a b\" title=\"T\"></p>\n",
        );
        eq("![](x.png)", "<p><img src=\"x.png\" alt=\"\"></p>\n");
        eq(
            "![a\"b](x.png)",
            "<p><img src=\"x.png\" alt=\"a&quot;b\"></p>\n",
        );
        eq(
            "<https://example.com/a?b=1&c=2>",
            "<p><a href=\"https://example.com/a?b=1&amp;c=2\">https://example.com/a?b=1&amp;c=2</a></p>\n",
        );
        eq(
            "<me@example.com>",
            "<p><a href=\"mailto:me@example.com\">me@example.com</a></p>\n",
        );
        eq("a < b > c", "<p>a &lt; b &gt; c</p>\n");
        eq(
            "a & b < c > \"d\" 'e'",
            "<p>a &amp; b &lt; c &gt; &quot;d&quot; &#39;e&#39;</p>\n",
        );
        eq("&amp; &lt;", "<p>&amp;amp; &amp;lt;</p>\n");
    }

    #[test]
    fn safety() {
        eq(
            "<script>alert(1)</script>",
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>\n",
        );
        eq(
            "<div onclick=\"x\">hi</div>",
            "<p>&lt;div onclick=&quot;x&quot;&gt;hi&lt;/div&gt;</p>\n",
        );
        eq(
            "*a* <img src=x onerror=alert(1)>",
            "<p><em>a</em> &lt;img src=x onerror=alert(1)&gt;</p>\n",
        );
        eq("# <b>x</b>", "<h1>&lt;b&gt;x&lt;/b&gt;</h1>\n");
        eq(
            "> <iframe>",
            "<blockquote>\n<p>&lt;iframe&gt;</p>\n</blockquote>\n",
        );
        eq("<!-- c -->", "<p>&lt;!-- c --&gt;</p>\n");
        for src in [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "[x](<java\tscript:alert(1)>)",
            "[x](<  javascript:alert(1)>)",
            "[x](data:text/html;base64,AAAA)",
            "[x](vbscript:msgbox)",
            "[x](file:///etc/passwd)",
            "[x](<\u{1}javascript:alert(1)>)",
        ] {
            let out = html(src);
            assert!(!out.contains("<a"), "{src} -> {out}");
            assert!(!out.to_ascii_lowercase().contains("href"), "{src} -> {out}");
        }
        eq(
            "[click *me*](javascript:alert(1))",
            "<p>click <em>me</em></p>\n",
        );
        eq(
            "[x](<jav&#x09;ascript:alert(1)>)",
            "<p><a href=\"jav&amp;#x09;ascript:alert(1)\">x</a></p>\n",
        );
        eq("![alt text](javascript:alert(1))", "<p>alt text</p>\n");
        eq("![a](data:image/svg+xml;base64,AAAA)", "<p>a</p>\n");
        eq(
            "<javascript:alert(1)>",
            "<p>&lt;javascript:alert(1)&gt;</p>\n",
        );
        eq("<data:text/html,x>", "<p>&lt;data:text/html,x&gt;</p>\n");
        eq(
            "[t](/a\"onmouseover=\"x)",
            "<p><a href=\"/a&quot;onmouseover=&quot;x\">t</a></p>\n",
        );
        eq(
            "[t](<a b\"c> \"ti'tle\")",
            "<p><a href=\"a%20b&quot;c\" title=\"ti&#39;tle\">t</a></p>\n",
        );
        eq(
            "![a'b](x.png 'q\"r')",
            "<p><img src=\"x.png\" alt=\"a&#39;b\" title=\"q&quot;r\"></p>\n",
        );
        eq(
            "[a](../x/y.html) [b](/abs) [c](?q=a:b) [d](x/y:z)",
            "<p><a href=\"../x/y.html\">a</a> <a href=\"/abs\">b</a> <a href=\"?q=a:b\">c</a> <a href=\"x/y:z\">d</a></p>\n",
        );
    }

    #[test]
    fn deep_nesting_and_determinism() {
        assert!(html(&("> ".repeat(500) + "x")).contains('x'));
        let nested = html(&("[".repeat(500) + "x" + &"](y)".repeat(500)));
        assert!(!nested.is_empty());
        let src = "# T\n\n- a\n- b\n\n*x* [l](/u)";
        assert_eq!(html(src), html(src));
    }

    #[test]
    fn excerpts() {
        let r = render_markdown(
            "# Title\n\nHello **bold** and `code` with [a link](/x)  \nand <b>raw</b>.\n\nSecond.",
        );
        assert_eq!(r.excerpt, "Hello bold and code with a link and <b>raw</b>.");
        assert_eq!(render_markdown("a   ![pic](x.png)\n\tb").excerpt, "a pic b");
        assert_eq!(render_markdown("# Only heading").excerpt, "");
        assert_eq!(render_markdown("```\ncode\n```").excerpt, "");
        assert_eq!(render_markdown(&"x".repeat(200)).excerpt, "x".repeat(200));
        let words = (0..60)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ex = render_markdown(&words).excerpt;
        assert!(ex.ends_with('…'));
        assert!(ex.encode_utf16().count() <= 200);
        let body = ex.trim_end_matches('…');
        assert!(words.starts_with(body));
        assert_eq!(&words[body.len()..body.len() + 1], " ");
        assert_eq!(
            render_markdown(&"y".repeat(500)).excerpt,
            "y".repeat(199) + "…"
        );
        // never splits a surrogate pair
        let ex = render_markdown(&"😀".repeat(150)).excerpt;
        assert_eq!(ex, "😀".repeat(99) + "…");
    }

    /// A larger, locally generated fuzz file (see generate.mjs).
    #[test]
    #[ignore]
    fn differential_external() {
        let Ok(path) = std::env::var("SITEFILE_MD_FUZZ") else {
            return;
        };
        let fuzz: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut bad = 0;
        for case in &fuzz {
            let src = case["input"].as_str().unwrap();
            let got = render_markdown(src);
            if got.html != case["html"].as_str().unwrap()
                || got.excerpt != case["excerpt"].as_str().unwrap()
            {
                if bad < 10 {
                    eprintln!(
                        "{src:?}\n  want {:?} / {:?}\n  got  {:?} / {:?}",
                        case["html"], case["excerpt"], got.html, got.excerpt
                    );
                }
                bad += 1;
            }
        }
        assert_eq!(bad, 0, "{bad} of {} differ", fuzz.len());
    }

    /// Byte-for-byte comparison with TypeScript output (see
    /// `tests/fixtures/markdown/generate.mjs`).
    #[test]
    fn differential_fixtures() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/markdown");
        let mut checked = 0;
        let mut failures = Vec::new();
        let mut names: Vec<_> = std::fs::read_dir(dir.join("cases"))
            .expect("fixture dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        names.sort();
        for p in names {
            let src = std::fs::read_to_string(&p).unwrap();
            let want_html = std::fs::read_to_string(p.with_extension("html")).unwrap();
            let want_excerpt = std::fs::read_to_string(p.with_extension("excerpt")).unwrap();
            let got = render_markdown(&src);
            if got.html != want_html || got.excerpt != want_excerpt {
                failures.push(format!(
                    "{}:\n  want {want_html:?} / {want_excerpt:?}\n  got  {:?} / {:?}",
                    p.display(),
                    got.html,
                    got.excerpt
                ));
            }
            checked += 1;
        }
        let fuzz: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(dir.join("fuzz.json")).unwrap()).unwrap();
        for case in &fuzz {
            let src = case["input"].as_str().unwrap();
            let got = render_markdown(src);
            if got.html != case["html"].as_str().unwrap()
                || got.excerpt != case["excerpt"].as_str().unwrap()
            {
                failures.push(format!(
                    "fuzz {src:?}:\n  want {:?} / {:?}\n  got  {:?} / {:?}",
                    case["html"], case["excerpt"], got.html, got.excerpt
                ));
            }
            checked += 1;
        }
        assert!(checked > 1000, "only {checked} fixtures");
        assert!(
            failures.is_empty(),
            "{} of {checked} differ:\n{}",
            failures.len(),
            failures
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
