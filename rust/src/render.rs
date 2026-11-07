//! RENDER templates (port of `src/render.ts`): parsing `${…}` substitutions and
//! `${#each}` / `${#if}` blocks, rendering them against data, HTML escaping,
//! strict UTF-8 decoding and JSON path walking.
//!
//! # Lookup interface
//!
//! [`render_template`] does not know where names come from; the caller passes
//! two callbacks:
//!
//! - [`TemplateLookup`] resolves `${name}` / `${name.a.b}` to a [`Scalar`]. It
//!   reports E_UNDEFINED / E_TYPE itself for unknown names or non-scalar values
//!   (typically by calling [`resolve_json_path`]).
//! - [`TemplateValueLookup`] resolves any value for `${#each}` and `${#if}`
//!   (lists, documents, scalars). `loose == true` (used by `#if`, and by
//!   `#each` on a dotted path) means a missing name or key yields `Ok(None)`
//!   (JavaScript `undefined`) instead of an error. It returns a [`Cow`] so
//!   values owned by the caller's environment are borrowed, not cloned (a FOR
//!   loop over many documents would otherwise copy the whole list per page);
//!   [`walk`] does the path walking and returns a borrow.
//!
//! Loop variables are resolved by the renderer itself and shadow every other
//! name, so the callbacks never see them.
//!
//! The [`js`] module holds the JavaScript string semantics (UTF-16 code units,
//! `/\s/` whitespace, own-property key order) that the data-format ports share.

use crate::error::{err, ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::EscapeMode;
use crate::values::Scalar;
use serde_json::Value;
use std::borrow::Cow;

/// Resolves a template variable to a scalar: `(name, path, span)`.
pub type TemplateLookup<'a> = dyn Fn(&str, &[String], &SourceSpan) -> Result<Scalar> + 'a;

/// Resolves any value for blocks: `(name, path, span, loose)`. `Ok(None)` is
/// JavaScript `undefined` (only returned when `loose`).
pub type TemplateValueLookup<'v> =
    dyn Fn(&str, &[String], &SourceSpan, bool) -> Result<Option<Cow<'v, Value>>> + 'v;

/// A name with a dotted path, as used by blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub name: String,
    pub path: Vec<String>,
    pub span: SourceSpan,
}

/// A piece of a `${file …}` string: text, or a `${name.key}` reference.
#[derive(Debug, Clone, PartialEq)]
pub enum HelperPart {
    Text(String),
    Ref {
        name: String,
        path: Vec<String>,
        span: SourceSpan,
    },
}

/// An attribute of a `${file … | helper …}`: `name="…"` or a bare `name`.
#[derive(Debug, Clone, PartialEq)]
pub struct FileAttr {
    pub name: String,
    pub value: Option<Vec<HelperPart>>,
}

/// A parsed RENDER template: text, substitutions and blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateNode {
    Text(String),
    /// `${file "<path>" [| helper attr…]}`: markup or a value for a file of the RENDER's tree (`media.rs`).
    File {
        path: Vec<HelperPart>,
        helper: Option<String>,
        attrs: Vec<FileAttr>,
        span: SourceSpan,
    },
    /// `raw` is true only for `${x|raw}`, which is never escaped.
    Var {
        name: String,
        path: Vec<String>,
        span: SourceSpan,
        raw: bool,
    },
    Each {
        list: Ref,
        as_name: String,
        body: Vec<TemplateNode>,
        empty: Vec<TemplateNode>,
        span: SourceSpan,
    },
    If {
        cond: Ref,
        negate: bool,
        then: Vec<TemplateNode>,
        otherwise: Vec<TemplateNode>,
        span: SourceSpan,
    },
}

enum Token {
    Text(String),
    File(TemplateNode),
    Var {
        name: String,
        path: Vec<String>,
        span: SourceSpan,
        raw: bool,
    },
    OpenEach {
        list: Ref,
        as_name: String,
        span: SourceSpan,
    },
    OpenIf {
        cond: Ref,
        negate: bool,
        span: SourceSpan,
    },
    Else(SourceSpan),
    CloseEach(SourceSpan),
    CloseIf(SourceSpan),
}

// ------------------------------------------------------------ small matchers

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
fn is_seg_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Length of the longest `REF` (`[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z0-9_-]+)*`)
/// at the start of `s`, or 0.
fn match_ref(s: &[u8]) -> usize {
    if s.is_empty() || !is_ident_start(s[0]) {
        return 0;
    }
    let mut i = 1;
    while i < s.len() && is_ident_char(s[i]) {
        i += 1;
    }
    while i < s.len() && s[i] == b'.' {
        let mut j = i + 1;
        while j < s.len() && is_seg_char(s[j]) {
            j += 1;
        }
        if j == i + 1 {
            break;
        }
        i = j;
    }
    i
}

/// Length of an identifier at the start of `s`, or 0.
fn match_ident(s: &[u8]) -> usize {
    if s.is_empty() || !is_ident_start(s[0]) {
        return 0;
    }
    let mut i = 1;
    while i < s.len() && is_ident_char(s[i]) {
        i += 1;
    }
    i
}

/// Byte length of the JavaScript `/\s*/` run at the start of `s`.
fn ws_len(s: &str) -> usize {
    s.len() - s.trim_start_matches(js::is_ws_char).len()
}

fn split_ref(text: &str, span: &SourceSpan) -> Ref {
    let mut it = text.split('.');
    let name = it.next().unwrap_or("").to_string();
    Ref {
        name,
        path: it.map(str::to_string).collect(),
        span: span.clone(),
    }
}

/// `^(#each|#if|else|/each|/if)(?![A-Za-z0-9_.])`
fn block_word(expr: &str) -> Option<&'static str> {
    for w in ["#each", "#if", "else", "/each", "/if"] {
        if let Some(rest) = expr.strip_prefix(w) {
            let next = rest.as_bytes().first().copied();
            if !matches!(next, Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'.') {
                return Some(w);
            }
        }
    }
    None
}

/// `^#each\s+(REF)\s+as\s+(IDENT)\s*$`
fn match_each(expr: &str) -> Option<(&str, &str)> {
    let rest = expr.strip_prefix("#each")?;
    let w = ws_len(rest);
    if w == 0 {
        return None;
    }
    let rest = &rest[w..];
    let r = match_ref(rest.as_bytes());
    if r == 0 {
        return None;
    }
    let (list, rest) = rest.split_at(r);
    let w = ws_len(rest);
    if w == 0 {
        return None;
    }
    let rest = rest[w..].strip_prefix("as")?;
    let w = ws_len(rest);
    if w == 0 {
        return None;
    }
    let rest = &rest[w..];
    let n = match_ident(rest.as_bytes());
    if n == 0 {
        return None;
    }
    let (name, rest) = rest.split_at(n);
    if ws_len(rest) != rest.len() {
        return None;
    }
    Some((list, name))
}

/// `^#if\s+(!?)\s*(REF)\s*$`
fn match_if(expr: &str) -> Option<(bool, &str)> {
    let rest = expr.strip_prefix("#if")?;
    let w = ws_len(rest);
    if w == 0 {
        return None;
    }
    let mut rest = &rest[w..];
    let negate = rest.starts_with('!');
    if negate {
        rest = &rest[1..];
    }
    rest = &rest[ws_len(rest)..];
    let r = match_ref(rest.as_bytes());
    if r == 0 {
        return None;
    }
    let (cond, tail) = rest.split_at(r);
    if ws_len(tail) != tail.len() {
        return None;
    }
    Some((negate, cond))
}

/// `^(REF)(?:\|(raw))?$`
fn match_var(expr: &str) -> Option<(&str, bool)> {
    let r = match_ref(expr.as_bytes());
    if r == 0 {
        return None;
    }
    match &expr[r..] {
        "" => Some((&expr[..r], false)),
        "|raw" => Some((&expr[..r], true)),
        _ => None,
    }
}

fn block_token(expr: &str, span: &SourceSpan) -> Result<Token> {
    let word = block_word(expr).unwrap_or("");
    if word == "#each" {
        let Some((list, as_name)) = match_each(expr) else {
            return err(
                E_TEMPLATE,
                format!("expected ${{#each list as item}}, got '${{{expr}}}'"),
                Some(span),
            );
        };
        return Ok(Token::OpenEach {
            list: split_ref(list, span),
            as_name: as_name.to_string(),
            span: span.clone(),
        });
    }
    if word == "#if" {
        let Some((negate, cond)) = match_if(expr) else {
            return err(
                E_TEMPLATE,
                format!("expected ${{#if value}} or ${{#if !value}}, got '${{{expr}}}'"),
                Some(span),
            );
        };
        return Ok(Token::OpenIf {
            cond: split_ref(cond, span),
            negate,
            span: span.clone(),
        });
    }
    if js::trim_str(expr) != word {
        return err(
            E_TEMPLATE,
            format!("unexpected text in '${{{expr}}}'"),
            Some(span),
        );
    }
    Ok(match word {
        "else" => Token::Else(span.clone()),
        "/each" => Token::CloseEach(span.clone()),
        _ => Token::CloseIf(span.clone()),
    })
}

/// Parse a RENDER template.
///
/// - `${name}` / `${name.seg.seg}` substitutes a value (segments may be array
///   indexes); `${name|raw}` substitutes it without escaping.
/// - `${#each list as item}…${else}…${/each}` repeats for each element of a list
///   (the `${else}` part renders when the list is empty). A missing key such as
///   `post.tags` counts as an empty list; an unknown top-level name is an error.
/// - `${#if x}…${else}…${/if}` / `${#if !x}`: false, null, missing, '', 0 and
///   empty lists are falsy.
/// - A block tag alone on its line removes that whole line from the output.
/// - `$${` writes a literal `${`; a `$` not followed by `{` is literal.
/// - Anything else inside `${…}`, or unbalanced blocks, fails with E_TEMPLATE.
///
/// `first_line` > 1 when the template is a heredoc body inside the Sitefile.
pub fn parse_render_template(text: &str, file: &str, first_line: u32) -> Result<Vec<TemplateNode>> {
    parse_render_template_at(text, file, first_line, 1)
}

/// [`parse_render_template`] for a template that starts at UTF-16 column `first_column` of
/// `first_line` (`LET name: html = …`): columns on its first line are offset by it.
pub fn parse_render_template_at(
    text: &str,
    file: &str,
    first_line: u32,
    first_column: u32,
) -> Result<Vec<TemplateNode>> {
    let b = text.as_bytes();
    let n = b.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut buf = String::new();
    let mut line = first_line;
    let mut line_start = 0usize;
    let mut i = 0usize;
    let span_at = |idx: usize, line: u32, line_start: usize| -> SourceSpan {
        let offset = if line == first_line {
            first_column - 1
        } else {
            0
        };
        SourceSpan::new(
            file,
            line,
            js::utf16_len(&text[line_start..idx]) as u32 + 1 + offset,
        )
    };
    fn flush(buf: &mut String, tokens: &mut Vec<Token>) {
        if !buf.is_empty() {
            tokens.push(Token::Text(std::mem::take(buf)));
        }
    }

    while i < n {
        // Copy a run of ordinary bytes (everything except `\n` and `$`) at once;
        // both are ASCII, so the run boundaries are char boundaries.
        if b[i] != b'\n' && b[i] != b'$' {
            let s = i;
            while i < n && b[i] != b'\n' && b[i] != b'$' {
                i += 1;
            }
            buf.push_str(&text[s..i]);
            continue;
        }
        if b[i] == b'\n' {
            buf.push('\n');
            i += 1;
            line += 1;
            line_start = i;
            continue;
        }
        // b[i] == '$'
        if b.get(i + 1) == Some(&b'$') && b.get(i + 2) == Some(&b'{') {
            buf.push_str("${");
            i += 3;
            continue;
        }
        if b.get(i + 1) != Some(&b'{') {
            buf.push('$');
            i += 1;
            continue;
        }
        let span = span_at(i, line, line_start);
        if text[i + 2..].starts_with("file ") || text[i + 2..].starts_with("file\t") {
            // `${file …}`: strings in it may hold `}`, so it is read by its own scanner.
            let (node, end) =
                parse_file_expr(text, i + 2, &|k| span_at(k, line, line_start), &span)?;
            flush(&mut buf, &mut tokens);
            tokens.push(Token::File(node));
            i = end;
            continue;
        }
        let close = text[i + 2..].find('}').map(|k| k + i + 2);
        let nl = text[i + 2..].find('\n').map(|k| k + i + 2);
        let close = match close {
            Some(c) if !matches!(nl, Some(l) if l < c) => c,
            _ => return err(E_TEMPLATE, "unterminated '${' in template", Some(&span)),
        };
        let expr = &text[i + 2..close];

        if block_word(expr).is_some() {
            let tok = block_token(expr, &span)?;
            // A block tag alone on its line: drop the line's indentation and newline.
            let before = &text[line_start..i];
            let line_end = nl.unwrap_or(n);
            let after = &text[close + 1..line_end];
            if before.bytes().all(|c| c == b' ' || c == b'\t')
                && after.bytes().all(|c| c == b' ' || c == b'\t' || c == b'\r')
            {
                if buf.ends_with(before) {
                    buf.truncate(buf.len() - before.len());
                }
                flush(&mut buf, &mut tokens);
                tokens.push(tok);
                i = line_end;
                if i < n {
                    i += 1; // the newline
                    line += 1;
                    line_start = i;
                }
                continue;
            }
            flush(&mut buf, &mut tokens);
            tokens.push(tok);
            i = close + 1;
            continue;
        }

        let Some((reference, raw)) = match_var(expr) else {
            return err(
                E_TEMPLATE,
                format!(
                    "invalid template expression '${{{expr}}}' (expected ${{name}}, ${{name.key}}, ${{name|raw}} or a block like ${{#each list as item}}; write $${{ for a literal '${{')"
                ),
                Some(&span),
            );
        };
        flush(&mut buf, &mut tokens);
        let r = split_ref(reference, &span);
        tokens.push(Token::Var {
            name: r.name,
            path: r.path,
            span,
            raw,
        });
        i = close + 1;
    }
    flush(&mut buf, &mut tokens);
    build_tree(tokens, file, line)
}

struct Frame {
    open: Token,
    first: Vec<TemplateNode>,
    second: Option<Vec<TemplateNode>>,
}

fn build_tree(tokens: Vec<Token>, file: &str, end_line: u32) -> Result<Vec<TemplateNode>> {
    let mut root: Vec<TemplateNode> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    fn target<'a>(
        root: &'a mut Vec<TemplateNode>,
        stack: &'a mut [Frame],
    ) -> &'a mut Vec<TemplateNode> {
        match stack.last_mut() {
            Some(f) => match f.second.as_mut() {
                Some(s) => s,
                None => &mut f.first,
            },
            None => root,
        }
    }
    for t in tokens {
        let closing_each = matches!(t, Token::CloseEach(_));
        match t {
            Token::Text(v) => target(&mut root, &mut stack).push(TemplateNode::Text(v)),
            Token::Var {
                name,
                path,
                span,
                raw,
            } => target(&mut root, &mut stack).push(TemplateNode::Var {
                name,
                path,
                span,
                raw,
            }),
            Token::File(node) => target(&mut root, &mut stack).push(node),
            open @ (Token::OpenEach { .. } | Token::OpenIf { .. }) => stack.push(Frame {
                open,
                first: Vec::new(),
                second: None,
            }),
            Token::Else(span) => {
                let Some(f) = stack.last_mut() else {
                    return err(
                        E_TEMPLATE,
                        "${else} outside of ${#if} or ${#each}",
                        Some(&span),
                    );
                };
                if f.second.is_some() {
                    return err(E_TEMPLATE, "a block can have only one ${else}", Some(&span));
                }
                f.second = Some(Vec::new());
            }
            Token::CloseEach(span) | Token::CloseIf(span) => {
                let is_each = matches!(stack.last().map(|f| &f.open), Some(Token::OpenEach { .. }));
                let is_if = matches!(stack.last().map(|f| &f.open), Some(Token::OpenIf { .. }));
                let want_each = closing_each;
                if !(if want_each { is_each } else { is_if }) {
                    // The TS version pops before checking; an error ends parsing either way.
                    return err(
                        E_TEMPLATE,
                        format!(
                            "${{{}}} without a matching opening tag",
                            if want_each { "/each" } else { "/if" }
                        ),
                        Some(&span),
                    );
                }
                let node = match stack.pop() {
                    Some(Frame {
                        open:
                            Token::OpenEach {
                                list,
                                as_name,
                                span,
                            },
                        first,
                        second,
                    }) => TemplateNode::Each {
                        list,
                        as_name,
                        body: first,
                        empty: second.unwrap_or_default(),
                        span,
                    },
                    Some(Frame {
                        open: Token::OpenIf { cond, negate, span },
                        first,
                        second,
                    }) => TemplateNode::If {
                        cond,
                        negate,
                        then: first,
                        otherwise: second.unwrap_or_default(),
                        span,
                    },
                    _ => continue, // checked above
                };
                target(&mut root, &mut stack).push(node);
            }
        }
    }
    if let Some(open) = stack.pop() {
        let (tag, span) = match &open.open {
            Token::OpenEach { span, .. } => ("${#each}", span),
            Token::OpenIf { span, .. }
            | Token::Else(span)
            | Token::CloseEach(span)
            | Token::CloseIf(span) => ("${#if}", span),
            Token::Text(_) | Token::Var { .. } | Token::File(_) => return Ok(merge_text(root)),
        };
        return err(
            E_TEMPLATE,
            format!("{tag} opened here is never closed (template ends at {file}:{end_line})"),
            Some(span),
        );
    }
    Ok(merge_text(root))
}

fn merge_text(nodes: Vec<TemplateNode>) -> Vec<TemplateNode> {
    let mut out: Vec<TemplateNode> = Vec::with_capacity(nodes.len());
    for n in nodes {
        match (n, out.last_mut()) {
            (TemplateNode::Text(v), Some(TemplateNode::Text(prev))) => prev.push_str(&v),
            (TemplateNode::Text(v), _) if v.is_empty() => {}
            (n, _) => out.push(n),
        }
    }
    out
}

/// How a [`FreeRef`] is used: `${name…}`, the list of `${#each}`, the condition of `${#if}`, or
/// inside a `${file …}` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUse {
    Var,
    Each,
    If,
    Text,
}

/// The code point at byte `i`, for messages.
fn char_at(text: &str, i: usize) -> String {
    text[i..]
        .chars()
        .next()
        .map(String::from)
        .unwrap_or_default()
}

/// `${file <value> [| <helper> [<name>[=<value>]]…]}` from `start` (after `${`); returns the node and
/// the index after its `}`. A value is a "double-quoted" string (`\` escapes, `${name}` references),
/// a 'single-quoted' one (literal), or a bare word (escapes and references, up to whitespace, `|` or
/// `}`). Errors point at the `${`.
fn parse_file_expr(
    text: &str,
    start: usize,
    span_at: &dyn Fn(usize) -> SourceSpan,
    span: &SourceSpan,
) -> Result<(TemplateNode, usize)> {
    let b = text.as_bytes();
    let fail = |d: &str| -> SitefileError { SitefileError::at(E_TEMPLATE, d.to_string(), span) };
    let unterminated = || fail("unterminated '${' in template");
    let mut i = start + 4;
    let ws = |i: &mut usize| {
        while matches!(b.get(*i), Some(b' ' | b'\t')) {
            *i += 1;
        }
    };
    let at_end = |i: usize| i >= b.len() || b[i] == b'\n';
    let value = |i: &mut usize| -> Result<Vec<HelperPart>> {
        let mut parts: Vec<HelperPart> = Vec::new();
        let add = |parts: &mut Vec<HelperPart>, s: &str| match parts.last_mut() {
            Some(HelperPart::Text(t)) => t.push_str(s),
            _ => parts.push(HelperPart::Text(s.to_string())),
        };
        if b[*i] == b'\'' {
            let close = text[*i + 1..].find('\'').map(|k| k + *i + 1);
            let nl = text[*i + 1..].find('\n').map(|k| k + *i + 1);
            let close = match close {
                Some(c) if !matches!(nl, Some(l) if l < c) => c,
                _ => return Err(fail("unterminated string in ${file …}")),
            };
            add(&mut parts, &text[*i + 1..close]);
            *i = close + 1;
            return Ok(if parts.is_empty() {
                vec![HelperPart::Text(String::new())]
            } else {
                parts
            });
        }
        let quoted = b[*i] == b'"';
        if quoted {
            *i += 1;
        }
        loop {
            if at_end(*i) {
                if quoted {
                    return Err(fail("unterminated string in ${file …}"));
                }
                break;
            }
            let ch = b[*i];
            if quoted && ch == b'"' {
                *i += 1;
                break;
            }
            if !quoted && matches!(ch, b' ' | b'\t' | b'|' | b'}' | b'"' | b'\'') {
                break;
            }
            if ch == b'\\' {
                if *i + 1 >= b.len() || b[*i + 1] == b'\n' {
                    return Err(if quoted {
                        fail("unterminated string in ${file …}")
                    } else {
                        unterminated()
                    });
                }
                let c = char_at(text, *i + 1);
                add(&mut parts, &c);
                *i += 1 + c.len();
                continue;
            }
            if ch == b'$' && b.get(*i + 1) == Some(&b'{') {
                let r = match_ref(&b[*i + 2..]);
                if r == 0 || b.get(*i + 2 + r) != Some(&b'}') {
                    return Err(fail(
                        "expected ${name} or ${name.key} inside a ${file …} string",
                    ));
                }
                let reference = split_ref(&text[*i + 2..*i + 2 + r], &span_at(*i));
                parts.push(HelperPart::Ref {
                    name: reference.name,
                    path: reference.path,
                    span: reference.span,
                });
                *i += 3 + r;
                continue;
            }
            let c = char_at(text, *i);
            add(&mut parts, &c);
            *i += c.len();
        }
        Ok(if parts.is_empty() {
            vec![HelperPart::Text(String::new())]
        } else {
            parts
        })
    };
    ws(&mut i);
    if at_end(i) {
        return Err(unterminated());
    }
    if b[i] == b'}' || b[i] == b'|' {
        return Err(fail(
            "expected a path after ${file, as in ${file \"/img/a.png\"}",
        ));
    }
    let path = value(&mut i)?;
    ws(&mut i);
    let mut helper: Option<String> = None;
    let mut attrs: Vec<FileAttr> = Vec::new();
    let list = || crate::media::file_helpers().collect::<Vec<_>>().join(", ");
    if b.get(i) == Some(&b'|') {
        i += 1;
        ws(&mut i);
        let n = b[i..]
            .iter()
            .enumerate()
            .take_while(|(k, c)| {
                if *k == 0 {
                    c.is_ascii_lowercase()
                } else {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || **c == b'-'
                }
            })
            .count();
        if n == 0 {
            return Err(fail(&format!(
                "expected a helper name after '|' ({})",
                list()
            )));
        }
        let h = text[i..i + n].to_string();
        i += n;
        if !crate::media::file_helpers().any(|x| x == h) {
            return Err(fail(&format!(
                "unknown file helper '{h}' (expected {})",
                list()
            )));
        }
        loop {
            let before = i;
            ws(&mut i);
            if at_end(i) || b[i] == b'}' {
                break;
            }
            let n = b[i..]
                .iter()
                .enumerate()
                .take_while(|(k, c)| {
                    let start = c.is_ascii_alphabetic() || **c == b'_' || **c == b':';
                    if *k == 0 {
                        start
                    } else {
                        start || c.is_ascii_digit() || **c == b'.' || **c == b'-'
                    }
                })
                .count();
            if i == before || n == 0 {
                return Err(fail(&format!(
                    "expected an attribute (name or name=\"value\") in ${{file … | {h}}}, got '{}'",
                    char_at(text, i)
                )));
            }
            let name = text[i..i + n].to_string();
            i += n;
            let mut v = None;
            if b.get(i) == Some(&b'=') {
                i += 1;
                if at_end(i) || matches!(b[i], b' ' | b'\t' | b'}') {
                    return Err(fail(&format!("expected a value after '{name}='")));
                }
                v = Some(value(&mut i)?);
            }
            if attrs
                .iter()
                .any(|a| a.name.to_ascii_lowercase() == name.to_ascii_lowercase())
            {
                return Err(fail(&format!("attribute '{name}' given twice")));
            }
            attrs.push(FileAttr { name, value: v });
        }
        if !attrs.is_empty() && crate::media::VALUE_HELPERS.contains(&h.as_str()) {
            return Err(fail(&format!("the {h} helper takes no attributes")));
        }
        helper = Some(h);
    }
    ws(&mut i);
    if at_end(i) {
        return Err(unterminated());
    }
    if b[i] != b'}' {
        return Err(fail(&format!(
            "expected | or }} after the path in ${{file …}}, got '{}'",
            char_at(text, i)
        )));
    }
    Ok((
        TemplateNode::File {
            path,
            helper,
            attrs,
            span: span.clone(),
        },
        i + 1,
    ))
}

/// Every `${name}` reference of a `${file …}` expression, in order: (name, path, span).
pub fn file_refs<'a>(
    path: &'a [HelperPart],
    attrs: &'a [FileAttr],
) -> Vec<(&'a str, &'a [String], &'a SourceSpan)> {
    std::iter::once(path)
        .chain(attrs.iter().filter_map(|a| a.value.as_deref()))
        .flatten()
        .filter_map(|p| match p {
            HelperPart::Ref { name, path, span } => Some((name.as_str(), path.as_slice(), span)),
            HelperPart::Text(_) => None,
        })
        .collect()
}

/// A name a template reads from outside itself (not a `${#each}` variable of its own).
#[derive(Debug, Clone, PartialEq)]
pub struct FreeRef {
    pub name: String,
    pub path: Vec<String>,
    pub span: SourceSpan,
    pub usage: RefUse,
}

/// The names a template reads that its own `${#each}` blocks do not bind, in template order.
pub fn free_refs(nodes: &[TemplateNode]) -> Vec<FreeRef> {
    fn walk_nodes(list: &[TemplateNode], bound: &mut Vec<String>, out: &mut Vec<FreeRef>) {
        for n in list {
            match n {
                TemplateNode::Text(_) => {}
                TemplateNode::Var {
                    name, path, span, ..
                } => {
                    if !bound.contains(name) {
                        out.push(FreeRef {
                            name: name.clone(),
                            path: path.clone(),
                            span: span.clone(),
                            usage: RefUse::Var,
                        });
                    }
                }
                TemplateNode::File { path, attrs, .. } => {
                    for (name, path, span) in file_refs(path, attrs) {
                        if !bound.iter().any(|b| b == name) {
                            out.push(FreeRef {
                                name: name.to_string(),
                                path: path.to_vec(),
                                span: span.clone(),
                                usage: RefUse::Text,
                            });
                        }
                    }
                }
                TemplateNode::Each {
                    list,
                    as_name,
                    body,
                    empty,
                    ..
                } => {
                    if !bound.contains(&list.name) {
                        out.push(FreeRef {
                            name: list.name.clone(),
                            path: list.path.clone(),
                            span: list.span.clone(),
                            usage: RefUse::Each,
                        });
                    }
                    bound.push(as_name.clone());
                    walk_nodes(body, bound, out);
                    bound.pop();
                    walk_nodes(empty, bound, out);
                }
                TemplateNode::If {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
                    if !bound.contains(&cond.name) {
                        out.push(FreeRef {
                            name: cond.name.clone(),
                            path: cond.path.clone(),
                            span: cond.span.clone(),
                            usage: RefUse::If,
                        });
                    }
                    walk_nodes(then, bound, out);
                    walk_nodes(otherwise, bound, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk_nodes(nodes, &mut Vec::new(), &mut out);
    out
}

/// What [`render_template_with`] needs beyond values: html fragments (`LET name: html`,
/// `IMPORT … AS html`).
pub struct TemplateContext<'a> {
    /// Whether `name` is an html fragment (checked after loop variables, before `lookup`).
    pub is_fragment: &'a dyn Fn(&str) -> bool,
    /// The parsed template of fragment `name` (may fail, e.g. E_TEMPLATE in an imported partial).
    pub fragment: &'a dyn Fn(&str) -> Result<std::sync::Arc<Vec<TemplateNode>>>,
    /// The stage's tree at this RENDER, for `${file …}`.
    pub tree: Option<&'a dyn crate::media::TemplateTree>,
    /// RENDER --inline-svg.
    pub inline_svg: bool,
}

/// E_TYPE for a fragment used where a value is needed.
pub fn fragment_not_value(name: &str, span: &SourceSpan) -> SitefileError {
    SitefileError::at(
        E_TYPE,
        format!("'{name}' is an html fragment, not a value; insert it with ${{{name}}}"),
        span,
    )
}

/// An error raised inside fragment `name`, with the place it was used added to the message.
fn in_fragment(e: SitefileError, name: &str, span: &SourceSpan) -> SitefileError {
    let at = e.span.clone().unwrap_or_else(|| span.clone());
    SitefileError::at(
        e.code,
        format!(
            "{} (in html fragment '{name}' used at {}:{}:{})",
            e.detail, span.file, span.line, span.column
        ),
        &at,
    )
}

// ------------------------------------------------------------------ render

/// Loop variables, innermost first.
struct Scope<'a> {
    name: &'a str,
    value: &'a Value,
    parent: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    fn get(scope: Option<&Scope<'a>>, name: &str) -> Option<&'a Value> {
        let mut cur = scope;
        while let Some(s) = cur {
            if s.name == name {
                return Some(s.value);
            }
            cur = s.parent;
        }
        None
    }
}

struct Ctx<'c, 'v> {
    lookup: &'c TemplateLookup<'c>,
    values: Option<&'c TemplateValueLookup<'v>>,
    escape: EscapeMode,
    fragments: Option<&'c TemplateContext<'c>>,
    /// Fragments being rendered, outermost first (a fragment that reaches itself is an error).
    using: std::cell::RefCell<Vec<String>>,
}

impl Ctx<'_, '_> {
    fn is_fragment(&self, name: &str, scope: Option<&Scope<'_>>) -> bool {
        Scope::get(scope, name).is_none() && self.fragments.is_some_and(|f| (f.is_fragment)(name))
    }
}

/// Render a parsed template. `lookup` resolves scalars; `values` resolves lists
/// and conditions for blocks (needed only if the template has blocks). Loop
/// variables are resolved here and shadow every other name. Escaping applies to
/// substituted values only, never to template text or `|raw` values.
pub fn render_template<'v>(
    nodes: &[TemplateNode],
    lookup: &TemplateLookup<'_>,
    escape: EscapeMode,
    values: Option<&TemplateValueLookup<'v>>,
) -> Result<String> {
    render_template_with(nodes, lookup, escape, values, None)
}

/// [`render_template`] with html fragments: `${name}` naming one renders its template in place
/// (same escape mode, same loop variables); its markup is never escaped, the values it substitutes are.
pub fn render_template_with<'v>(
    nodes: &[TemplateNode],
    lookup: &TemplateLookup<'_>,
    escape: EscapeMode,
    values: Option<&TemplateValueLookup<'v>>,
    fragments: Option<&TemplateContext<'_>>,
) -> Result<String> {
    let ctx = Ctx {
        lookup,
        values,
        escape,
        fragments,
        using: std::cell::RefCell::new(Vec::new()),
    };
    // Pages are usually kilobytes; start there to avoid repeated regrowth.
    let mut out = String::with_capacity(16 * 1024);
    run(&ctx, nodes, None, &mut out)?;
    Ok(out)
}

fn value_of<'s, 'v: 's>(
    ctx: &Ctx<'_, 'v>,
    r: &Ref,
    loose: bool,
    scope: Option<&Scope<'s>>,
) -> Result<Option<Cow<'s, Value>>> {
    if let Some(v) = Scope::get(scope, &r.name) {
        return Ok(walk(v, &r.path, &r.name, &r.span, loose)?.map(Cow::Borrowed));
    }
    if ctx.is_fragment(&r.name, None) {
        return Err(fragment_not_value(&r.name, &r.span));
    }
    match ctx.values {
        None => err(
            E_TEMPLATE,
            "this template uses blocks, which need list values",
            Some(&r.span),
        ),
        Some(values) => values(&r.name, &r.path, &r.span, loose),
    }
}

fn run<'s, 'v: 's>(
    ctx: &Ctx<'_, 'v>,
    list: &[TemplateNode],
    scope: Option<&Scope<'s>>,
    out: &mut String,
) -> Result<()> {
    for n in list {
        match n {
            TemplateNode::Text(v) => out.push_str(v),
            TemplateNode::File {
                path,
                helper,
                attrs,
                span,
            } => {
                // References inside its strings are values (a fragment is not one).
                let text_of = |t: &[HelperPart]| -> Result<String> {
                    let mut s = String::new();
                    for p in t {
                        match p {
                            HelperPart::Text(v) => s.push_str(v),
                            HelperPart::Ref { name, path, span } => {
                                let v = match Scope::get(scope, name) {
                                    Some(item) => resolve_json_path(item, path, name, span)?,
                                    None if ctx.is_fragment(name, None) => {
                                        return Err(fragment_not_value(name, span))
                                    }
                                    None => (ctx.lookup)(name, path, span)?,
                                };
                                s.push_str(&v.to_js_string());
                            }
                        }
                    }
                    Ok(s)
                };
                let Some(tree) = ctx.fragments.and_then(|f| f.tree) else {
                    return err(
                        E_TEMPLATE,
                        "${file …} reads the tree of a RENDER, which this template has not",
                        Some(span),
                    );
                };
                let raw = text_of(path)?;
                let mut evaluated = Vec::with_capacity(attrs.len());
                for a in attrs {
                    let value = match &a.value {
                        Some(v) => Some(text_of(v)?),
                        None => None,
                    };
                    evaluated.push(crate::media::HelperAttr {
                        name: a.name.clone(),
                        value,
                    });
                }
                let opts = crate::media::HelperOptions {
                    inline_svg: ctx.fragments.is_some_and(|f| f.inline_svg),
                };
                match crate::media::render_file(
                    &raw,
                    helper.as_deref(),
                    &evaluated,
                    tree,
                    opts,
                    span,
                )? {
                    crate::media::HelperOutput::Markup(m) => out.push_str(&m),
                    crate::media::HelperOutput::Value(v) if ctx.escape == EscapeMode::Html => {
                        escape_html_into(&v, out)
                    }
                    crate::media::HelperOutput::Value(v) => out.push_str(&v),
                }
            }
            TemplateNode::Var {
                name,
                path,
                span,
                raw,
            } => {
                if ctx.is_fragment(name, scope) {
                    render_fragment(ctx, name, path, span, scope, out)?;
                    continue;
                }
                let v = match Scope::get(scope, name) {
                    Some(item) => {
                        // Fast path: a loop variable whose path leads to a string is
                        // written straight from the borrowed value (no copy). Anything
                        // else goes through resolve_json_path, which owns the errors.
                        if let Ok(Some(Value::String(s))) = walk(item, path, name, span, true) {
                            if *raw || ctx.escape != EscapeMode::Html {
                                out.push_str(s);
                            } else {
                                escape_html_into(s, out);
                            }
                            continue;
                        }
                        resolve_json_path(item, path, name, span)?
                    }
                    None => (ctx.lookup)(name, path, span)?,
                };
                let s = v.to_js_string();
                if *raw || ctx.escape != EscapeMode::Html {
                    out.push_str(&s);
                } else {
                    escape_html_into(&s, out);
                }
            }
            TemplateNode::Each {
                list,
                as_name,
                body,
                empty,
                ..
            } => {
                // A missing key (post.tags on a post without tags) is an empty list;
                // an unknown top-level name is still an error, so typos are caught.
                let found = value_of(ctx, list, !list.path.is_empty(), scope)?;
                let items: &[Value] = match found.as_deref() {
                    None | Some(Value::Null) => &[],
                    Some(Value::Array(a)) => a,
                    Some(other) => {
                        return err(
                            E_TYPE,
                            format!(
                                "{} is {}, not a list; ${{#each}} needs a list",
                                dotted(list),
                                describe(Some(other))
                            ),
                            Some(&list.span),
                        )
                    }
                };
                if items.is_empty() {
                    run(ctx, empty, scope, out)?;
                    continue;
                }
                for item in items {
                    let child = Scope {
                        name: as_name,
                        value: item,
                        parent: scope,
                    };
                    run(ctx, body, Some(&child), out)?;
                }
            }
            TemplateNode::If {
                cond,
                negate,
                then,
                otherwise,
                ..
            } => {
                let v = value_of(ctx, cond, true, scope)?;
                let truthy = is_truthy(v.as_deref());
                run(
                    ctx,
                    if truthy != *negate { then } else { otherwise },
                    scope,
                    out,
                )?;
            }
        }
    }
    Ok(())
}

fn render_fragment<'s, 'v: 's>(
    ctx: &Ctx<'_, 'v>,
    name: &str,
    path: &[String],
    span: &SourceSpan,
    scope: Option<&Scope<'s>>,
    out: &mut String,
) -> Result<()> {
    if !path.is_empty() {
        return Err(fragment_not_value(name, span));
    }
    let chain = {
        let using = ctx.using.borrow();
        using.iter().position(|u| u == name).map(|i| {
            using[i..]
                .iter()
                .map(String::as_str)
                .chain([name])
                .collect::<Vec<_>>()
                .join(" → ")
        })
    };
    if let Some(chain) = chain {
        return err(
            E_TEMPLATE,
            format!("html fragment '{name}' uses itself ({chain})"),
            Some(span),
        );
    }
    let Some(f) = ctx.fragments else {
        return Ok(());
    };
    ctx.using.borrow_mut().push(name.to_string());
    let result = (f.fragment)(name).and_then(|nodes| run(ctx, &nodes, scope, out));
    ctx.using.borrow_mut().pop();
    result.map_err(|e| in_fragment(e, name, span))
}

fn dotted(r: &Ref) -> String {
    let mut s = format!("'{}", r.name);
    for p in &r.path {
        s.push('.');
        s.push_str(p);
    }
    s.push('\'');
    s
}

/// JavaScript truthiness for template conditions; `None` is `undefined`.
/// Arrays are truthy when non-empty, objects always.
pub fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::Object(_)) => true,
    }
}

/// `/^(0|[1-9][0-9]*)$/` and below `len`.
fn array_index(seg: &str, len: usize) -> Option<usize> {
    let b = seg.as_bytes();
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) || (b.len() > 1 && b[0] == b'0') {
        return None;
    }
    // Too many digits to be an index of any real array.
    seg.parse::<usize>().ok().filter(|&i| i < len)
}

/// Walk `value` along `path`. Strict mode fails like [`resolve_json_path`] on a
/// missing key; loose mode returns `None`. Any value type may be returned.
pub fn walk<'a>(
    value: &'a Value,
    path: &[String],
    name: &str,
    span: &SourceSpan,
    loose: bool,
) -> Result<Option<&'a Value>> {
    let mut cur = value;
    let mut walked = name.to_string();
    for seg in path {
        let parent = walked.clone();
        walked.push('.');
        walked.push_str(seg);
        let next = match cur {
            Value::Array(a) => array_index(seg, a.len()).map(|i| &a[i]),
            Value::Object(m) => m.get(seg.as_str()),
            _ => None,
        };
        match next {
            Some(v) => cur = v,
            None => {
                if loose {
                    return Ok(None);
                }
                return err(
                    E_UNDEFINED,
                    format!(
                        "'{walked}' is undefined ('{parent}' is {})",
                        describe(Some(cur))
                    ),
                    Some(span),
                );
            }
        }
    }
    Ok(Some(cur))
}

/// HTML-escape `& < > " '`.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_html_into(s, &mut out);
    out
}

pub(crate) fn escape_html_into(s: &str, out: &mut String) {
    let mut last = 0;
    for (i, b) in s.bytes().enumerate() {
        let rep = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&#39;",
            _ => continue,
        };
        out.push_str(&s[last..i]);
        out.push_str(rep);
        last = i + 1;
    }
    out.push_str(&s[last..]);
}

/// Decode strict UTF-8 (E_ENCODING on invalid bytes), stripping a leading BOM.
pub fn decode_utf8(bytes: &[u8], what: &str, span: Option<&SourceSpan>) -> Result<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.strip_prefix('\u{feff}').unwrap_or(s).to_string()),
        Err(_) => Err(SitefileError::new(
            E_ENCODING,
            format!("{what} is not valid UTF-8"),
            span.cloned(),
        )),
    }
}

/// Walk a parsed JSON value along `path` and return the scalar found there.
/// Missing keys/indexes fail with E_UNDEFINED; null, objects and arrays fail
/// with E_TYPE. Messages name the full dotted path.
pub fn resolve_json_path(
    value: &Value,
    path: &[String],
    name: &str,
    span: &SourceSpan,
) -> Result<Scalar> {
    let mut cur = value;
    let mut walked = name.to_string();
    for seg in path {
        let parent = walked.clone();
        walked.push('.');
        walked.push_str(seg);
        match cur {
            Value::Array(a) => match array_index(seg, a.len()) {
                Some(i) => cur = &a[i],
                None => {
                    return err(
                        E_UNDEFINED,
                        format!(
                            "'{walked}' is undefined: '{parent}' is an array of length {}",
                            a.len()
                        ),
                        Some(span),
                    )
                }
            },
            Value::Object(m) => match m.get(seg.as_str()) {
                Some(v) => cur = v,
                None => {
                    return err(
                        E_UNDEFINED,
                        format!("'{walked}' is undefined: '{parent}' has no key '{seg}'"),
                        Some(span),
                    )
                }
            },
            other => {
                return err(
                    E_UNDEFINED,
                    format!(
                        "'{walked}' is undefined: '{parent}' is {}, not an object or array",
                        describe(Some(other))
                    ),
                    Some(span),
                )
            }
        }
    }
    match cur {
        Value::String(s) => Ok(Scalar::Str(s.clone())),
        Value::Number(n) => Ok(Scalar::Num(n.as_f64().unwrap_or(f64::NAN))),
        Value::Bool(b) => Ok(Scalar::Bool(*b)),
        other => err(
            E_TYPE,
            format!(
                "'{walked}' is {}; templates can only substitute strings, numbers and booleans",
                describe(Some(other))
            ),
            Some(span),
        ),
    }
}

/// `describe()` from the TS version; `None` is `undefined`.
pub(crate) fn describe(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Array(_)) => "an array",
        Some(Value::Object(_)) => "an object",
        Some(Value::String(_)) => "a string",
        Some(Value::Number(_)) => "a number",
        Some(Value::Bool(_)) => "a boolean",
    }
}

/// JavaScript string semantics shared by the data-format ports.
pub mod js {
    use serde_json::{Map, Number, Value};
    use std::cmp::Ordering;

    /// JavaScript `WhiteSpace` and `LineTerminator` (what `/\s/` and `trim()` use).
    pub fn is_ws(c: u16) -> bool {
        matches!(
            c,
            0x09..=0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
        )
    }
    pub fn is_ws_char(c: char) -> bool {
        (c as u32) < 0x10000 && is_ws(c as u32 as u16)
    }
    /// `String.prototype.trim()` on a `&str`.
    pub fn trim_str(s: &str) -> &str {
        s.trim_matches(is_ws_char)
    }
    pub fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    /// UTF-16 to a Rust string (a lone surrogate becomes U+FFFD, as when Node
    /// encodes such a string as UTF-8).
    pub fn string(v: &[u16]) -> String {
        String::from_utf16_lossy(v)
    }
    pub fn utf16_len(s: &str) -> usize {
        if s.is_ascii() {
            s.len()
        } else {
            s.encode_utf16().count()
        }
    }
    pub fn trim(v: &[u16]) -> &[u16] {
        trim_end(trim_start(v))
    }
    pub fn trim_start(v: &[u16]) -> &[u16] {
        let i = v.iter().position(|&c| !is_ws(c)).unwrap_or(v.len());
        &v[i..]
    }
    pub fn trim_end(v: &[u16]) -> &[u16] {
        let i = v
            .iter()
            .rposition(|&c| !is_ws(c))
            .map(|p| p + 1)
            .unwrap_or(0);
        &v[..i]
    }
    /// `indexOf(needle, from)`.
    pub fn index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
        if needle.is_empty() {
            return Some(from.min(hay.len()));
        }
        if from >= hay.len() || needle.len() > hay.len() {
            return None;
        }
        (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
    }
    pub fn index_of_unit(hay: &[u16], c: u16, from: usize) -> Option<usize> {
        if from >= hay.len() {
            return None;
        }
        hay[from..].iter().position(|&x| x == c).map(|p| p + from)
    }
    /// JavaScript `<` on strings: UTF-16 code unit order.
    pub fn cmp_utf16(a: &str, b: &str) -> Ordering {
        if a.is_ascii() && b.is_ascii() {
            return a.cmp(b);
        }
        a.encode_utf16().cmp(b.encode_utf16())
    }

    /// Is `k` an array index (canonical decimal below 2^32 - 1)? Such keys come
    /// first, ascending, in a JavaScript object's own-key order.
    fn array_index_key(k: &str) -> Option<u32> {
        let b = k.as_bytes();
        if b.is_empty()
            || b.len() > 10
            || !b.iter().all(u8::is_ascii_digit)
            || (b.len() > 1 && b[0] == b'0')
        {
            return None;
        }
        k.parse::<u64>()
            .ok()
            .filter(|&n| n < u32::MAX as u64)
            .map(|n| n as u32)
    }

    /// Reorder a map's keys as a JavaScript object would enumerate them:
    /// integer-like keys ascending, then the rest in insertion order.
    pub fn object_order(map: Map<String, Value>) -> Map<String, Value> {
        if !map.keys().any(|k| array_index_key(k).is_some()) {
            return map;
        }
        let mut ints: Vec<(u32, String, Value)> = Vec::new();
        let mut rest: Vec<(String, Value)> = Vec::new();
        for (k, v) in map {
            match array_index_key(&k) {
                Some(n) => ints.push((n, k, v)),
                None => rest.push((k, v)),
            }
        }
        ints.sort_by_key(|e| e.0);
        let mut out = Map::with_capacity(ints.len() + rest.len());
        for (_, k, v) in ints {
            out.insert(k, v);
        }
        for (k, v) in rest {
            out.insert(k, v);
        }
        out
    }

    /// A JavaScript number as JSON: integral values in the safe range become
    /// integers (so they print as `1`, not `1.0`), others stay floats. NaN and
    /// infinities (not representable) become null, as `JSON.stringify` does.
    pub fn number(f: f64) -> Value {
        if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 {
            return Value::Number(Number::from(f as i64));
        }
        Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }

    /// Make a value parsed by serde_json look like the result of `JSON.parse`:
    /// numbers are JS doubles, objects use JS key order.
    pub fn from_json_parse(v: Value) -> Value {
        match v {
            Value::Number(n) => {
                if n.is_i64()
                    && n.as_i64()
                        .is_some_and(|i| i.unsigned_abs() <= 9_007_199_254_740_991)
                {
                    Value::Number(n)
                } else {
                    number(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            Value::Array(a) => Value::Array(a.into_iter().map(from_json_parse).collect()),
            Value::Object(m) => Value::Object(object_order(
                m.into_iter()
                    .map(|(k, v)| (k, from_json_parse(v)))
                    .collect(),
            )),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(name: &str) -> Option<Scalar> {
        Some(match name {
            "TITLE" => Scalar::Str("Hello".into()),
            "N" => Scalar::Num(3.0),
            "B" => Scalar::Bool(true),
            "X" => Scalar::Str("<a href=\"x\">&'</a>".into()),
            _ => return None,
        })
    }
    fn lookup(name: &str, path: &[String], span: &SourceSpan) -> Result<Scalar> {
        if !path.is_empty() {
            return Ok(Scalar::Str(format!("{name}.{}", path.join("."))));
        }
        vars(name).ok_or_else(|| SitefileError::at(E_UNDEFINED, format!("undefined {name}"), span))
    }
    fn render(t: &str, escape: EscapeMode) -> Result<String> {
        render_template(
            &parse_render_template(t, "t.html", 1)?,
            &lookup,
            escape,
            None,
        )
    }
    fn r(t: &str) -> String {
        render(t, EscapeMode::None).unwrap()
    }
    fn error_at(t: &str, code: crate::error::ErrorCode, line: u32, column: u32) {
        let e = parse_render_template(t, "t.html", 1).unwrap_err();
        assert_eq!(e.code, code, "{t}");
        assert_eq!(
            e.span,
            Some(SourceSpan::new("t.html", line, column)),
            "{t}: {e}"
        );
    }
    fn sp(f: &str, l: u32, c: u32) -> SourceSpan {
        SourceSpan::new(f, l, c)
    }

    #[test]
    fn substitutes_variables() {
        assert_eq!(r("<h1>${TITLE}</h1> ${N} ${B}"), "<h1>Hello</h1> 3 true");
        assert_eq!(r("${TITLE}${TITLE}"), "HelloHello");
        assert_eq!(r(""), "");
    }

    #[test]
    fn parses_dotted_paths_with_spans_and_merges_text() {
        let parts = parse_render_template("a\n  ${data.site.0.my-key} b", "f", 1).unwrap();
        assert_eq!(
            parts,
            vec![
                TemplateNode::Text("a\n  ".into()),
                TemplateNode::Var {
                    name: "data".into(),
                    path: vec!["site".into(), "0".into(), "my-key".into()],
                    span: sp("f", 2, 3),
                    raw: false
                },
                TemplateNode::Text(" b".into()),
            ]
        );
        assert_eq!(
            parse_render_template("x $ $${y} z", "f", 1).unwrap(),
            vec![TemplateNode::Text("x $ ${y} z".into())]
        );
    }

    #[test]
    fn literal_dollar() {
        assert_eq!(r("$${TITLE}"), "${TITLE}");
        assert_eq!(r("cost: $5, $ $a $}"), "cost: $5, $ $a $}");
        assert_eq!(r("end$"), "end$");
        assert_eq!(r("$$${TITLE}"), "$${TITLE}");
    }

    #[test]
    fn invalid_or_unterminated_expressions_report_line_and_column() {
        error_at("ok\nab ${1bad}", E_TEMPLATE, 2, 4);
        error_at("ok\r\nline\r\n  ${a b}", E_TEMPLATE, 3, 3);
        error_at("x\r\n${unterminated", E_TEMPLATE, 2, 1);
        error_at("${a\n}", E_TEMPLATE, 1, 1);
        error_at("${}", E_TEMPLATE, 1, 1);
        error_at("${a.}", E_TEMPLATE, 1, 1);
        error_at("${a..b}", E_TEMPLATE, 1, 1);
        error_at("${ a }", E_TEMPLATE, 1, 1);
        // Columns count UTF-16 code units.
        error_at("é😀${bad bad}", E_TEMPLATE, 1, 4);
    }

    #[test]
    fn var_spans_on_crlf_templates() {
        let parts = parse_render_template("a\r\nb\r\n xx${V}", "f", 1).unwrap();
        let span = parts.iter().find_map(|p| match p {
            TemplateNode::Var { span, .. } => Some(span.clone()),
            _ => None,
        });
        assert_eq!(span, Some(sp("f", 3, 4)));
        let parts = parse_render_template("${V}", "Sitefile", 7).unwrap();
        assert!(
            matches!(&parts[0], TemplateNode::Var { span, .. } if *span == sp("Sitefile", 7, 1))
        );
    }

    #[test]
    fn html_escaping_applies_to_values_only() {
        assert_eq!(
            render("<p title=\"${X}\">&amp;</p>", EscapeMode::Html).unwrap(),
            "<p title=\"&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;\">&amp;</p>"
        );
        assert_eq!(r("<p>${X}</p>"), "<p><a href=\"x\">&'</a></p>");
        assert_eq!(escape_html("&<>\"'x"), "&amp;&lt;&gt;&quot;&#39;x");
    }

    #[test]
    fn lookup_errors_propagate() {
        let e = render("${MISSING}", EscapeMode::None).unwrap_err();
        assert!(e.detail.contains("undefined MISSING"));
    }

    #[test]
    fn decode_utf8_strips_bom_and_rejects_invalid() {
        assert_eq!(
            decode_utf8(&[0xef, 0xbb, 0xbf, 0x68, 0x69], "tpl", None).unwrap(),
            "hi"
        );
        assert_eq!(
            decode_utf8("héllo ✓".as_bytes(), "tpl", None).unwrap(),
            "héllo ✓"
        );
        let span = sp("Sitefile", 4, 1);
        let e = decode_utf8(&[0x68, 0xff, 0x69], "template './x.html'", Some(&span)).unwrap_err();
        assert_eq!(e.code, E_ENCODING);
        assert_eq!(e.span, Some(span));
        assert!(e.message().contains("x.html"));
        assert_eq!(
            decode_utf8(&[0xc3], "tpl", None).unwrap_err().code,
            E_ENCODING
        );
    }

    #[test]
    fn resolve_json_path_cases() {
        let span = sp("Sitefile", 1, 1);
        let data = json!({ "site": { "title": "T", "n": 2, "ok": false, "tags": ["a", "b"], "nil": null, "obj": {} }, "my-key": "k" });
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resolve_json_path(&data, &p(&["site", "title"]), "data", &span).unwrap(),
            Scalar::Str("T".into())
        );
        assert_eq!(
            resolve_json_path(&data, &p(&["site", "n"]), "data", &span).unwrap(),
            Scalar::Num(2.0)
        );
        assert_eq!(
            resolve_json_path(&data, &p(&["site", "ok"]), "data", &span).unwrap(),
            Scalar::Bool(false)
        );
        assert_eq!(
            resolve_json_path(&data, &p(&["site", "tags", "1"]), "data", &span).unwrap(),
            Scalar::Str("b".into())
        );
        assert_eq!(
            resolve_json_path(&data, &p(&["my-key"]), "data", &span).unwrap(),
            Scalar::Str("k".into())
        );
        assert_eq!(
            resolve_json_path(&json!("plain"), &[], "data", &span).unwrap(),
            Scalar::Str("plain".into())
        );
        let fails = |path: &[&str], code, needle: &str| {
            let e = resolve_json_path(&data, &p(path), "data", &span).unwrap_err();
            assert_eq!(e.code, code, "{path:?}");
            assert!(e.message().contains(needle), "{}", e.message());
            assert_eq!(e.span.as_ref(), Some(&span));
        };
        fails(&["site", "missing"], E_UNDEFINED, "data.site.missing");
        fails(&["site", "tags", "5"], E_UNDEFINED, "data.site.tags.5");
        fails(&["site", "tags", "x"], E_UNDEFINED, "data.site.tags.x");
        fails(&["site", "tags", "01"], E_UNDEFINED, "data.site.tags.01");
        fails(&["site", "title", "x"], E_UNDEFINED, "data.site.title.x");
        fails(&["site", "toString"], E_UNDEFINED, "data.site.toString");
        fails(&["site", "nil"], E_TYPE, "data.site.nil");
        fails(&["site", "obj"], E_TYPE, "data.site.obj");
        fails(&["site", "tags"], E_TYPE, "data.site.tags");
        fails(&["site", "nil", "x"], E_UNDEFINED, "data.site.nil.x");
        assert_eq!(
            resolve_json_path(&data, &[], "data", &span)
                .unwrap_err()
                .code,
            E_TYPE
        );
        let e = resolve_json_path(&data, &p(&["site", "missing"]), "data", &span).unwrap_err();
        assert_eq!(
            e.detail,
            "'data.site.missing' is undefined: 'data.site' has no key 'missing'"
        );
        let e = resolve_json_path(&data, &p(&["site", "tags", "5"]), "data", &span).unwrap_err();
        assert_eq!(
            e.detail,
            "'data.site.tags.5' is undefined: 'data.site.tags' is an array of length 2"
        );
    }

    // ------------------------------------------------- blocks (test/blog.test.ts)

    fn blog_data() -> Value {
        json!({
            "T": "Blog",
            "posts": [
                { "title": "A & B", "tags": ["x", "y"], "html": "<p>a</p>" },
                { "title": "C", "tags": [], "draft": true }
            ],
            "none": []
        })
    }
    fn get<'a>(data: &'a Value, name: &str, p: &[String]) -> Option<&'a Value> {
        let mut v = data.get(name)?;
        for s in p {
            v = match v {
                Value::Array(a) => a.get(s.parse::<usize>().ok()?)?,
                Value::Object(m) => m.get(s)?,
                _ => return None,
            };
        }
        Some(v)
    }
    fn blog(t: &str, escape: EscapeMode) -> Result<String> {
        let data = blog_data();
        let lookup = |name: &str, p: &[String], span: &SourceSpan| -> Result<Scalar> {
            match get(&data, name, p) {
                Some(Value::String(s)) => Ok(Scalar::Str(s.clone())),
                Some(Value::Number(n)) => Ok(Scalar::Num(n.as_f64().unwrap())),
                Some(Value::Bool(b)) => Ok(Scalar::Bool(*b)),
                _ => Err(SitefileError::at(
                    E_UNDEFINED,
                    format!("undefined {name}"),
                    span,
                )),
            }
        };
        let values = |name: &str,
                      p: &[String],
                      span: &SourceSpan,
                      loose: bool|
         -> Result<Option<Cow<'_, Value>>> {
            match get(&data, name, p) {
                Some(v) => Ok(Some(Cow::Borrowed(v))),
                None if loose => Ok(None),
                None => Err(SitefileError::at(
                    E_UNDEFINED,
                    format!("undefined {name}"),
                    span,
                )),
            }
        };
        render_template(
            &parse_render_template(t, "t.html", 1)?,
            &lookup,
            escape,
            Some(&values),
        )
    }
    fn b(t: &str) -> String {
        blog(t, EscapeMode::Html).unwrap()
    }

    #[test]
    fn each_repeats_shadows_and_else() {
        assert_eq!(
            b("${#each posts as p}[${p.title}]${/each}"),
            "[A &amp; B][C]"
        );
        assert_eq!(
            b("${#each posts as T}${T.title}${/each} ${T}"),
            "A &amp; BC Blog"
        );
        assert_eq!(b("${#each none as p}x${else}empty${/each}"), "empty");
        assert_eq!(
            b("${#each posts as p}${#each p.tags as t}${t},${/each};${/each}"),
            "x,y,;;"
        );
        assert_eq!(
            b("${#each posts as p}${#each p.missing as t}${t}${else}-${/each}${/each}"),
            "--"
        );
        assert!(blog("${#each nope as p}${/each}", EscapeMode::Html)
            .unwrap_err()
            .detail
            .contains("undefined nope"));
        // nested loops with the same name restore the outer binding
        assert_eq!(
            b("${#each posts as p}${#each p.tags as p}${p}${/each}${p.title}${/each}"),
            "xyA &amp; BC"
        );
    }

    #[test]
    fn if_else_negation_and_loose_keys() {
        assert_eq!(
            b("${#each posts as p}${#if p.draft}D${else}P${/if}${/each}"),
            "PD"
        );
        assert_eq!(
            blog(
                "${#each posts as p}${#if !p.draft}${p.title}${/if}${/each}",
                EscapeMode::None
            )
            .unwrap(),
            "A & B"
        );
        assert_eq!(b("${#if missing.key}yes${else}no${/if}"), "no");
        assert_eq!(
            b("${#each posts as p}${#if p.tags}T${else}-${/if}${/each}"),
            "T-"
        );
    }

    #[test]
    fn raw_skips_escaping() {
        assert_eq!(
            b("${posts.0.html|raw} ${posts.0.html}"),
            "<p>a</p> &lt;p&gt;a&lt;/p&gt;"
        );
    }

    #[test]
    fn block_tag_alone_on_line_removes_line() {
        let t = "<ul>\n  ${#each posts as p}\n  <li>${p.title}</li>\n  ${/each}\n</ul>\n";
        assert_eq!(
            blog(t, EscapeMode::None).unwrap(),
            "<ul>\n  <li>A & B</li>\n  <li>C</li>\n</ul>\n"
        );
        let t = "a\r\n  ${#if T}  \r\nb\r\n${/if}\r\n";
        assert_eq!(blog(t, EscapeMode::None).unwrap(), "a\r\nb\r\n");
    }

    #[test]
    fn template_block_errors() {
        let at = |t: &str, line: u32, column: u32| {
            let e = blog(t, EscapeMode::Html).unwrap_err();
            assert_eq!(e.code, E_TEMPLATE, "{t}");
            assert_eq!(e.span, Some(sp("t.html", line, column)), "{t}");
        };
        at("${#each posts}x${/each}", 1, 1);
        at("a\n${#if T}x", 2, 1);
        at("${/each}", 1, 1);
        at("${#if T}x${/each}", 1, 10);
        at("${#if T}a${else}b${else}c${/if}", 1, 18);
        at("${T|upper}", 1, 1);
        assert_eq!(
            blog("${#each T as x}${/each}", EscapeMode::Html)
                .unwrap_err()
                .code,
            E_TYPE
        );
        let e = blog("a\n${#if T}x", EscapeMode::Html).unwrap_err();
        assert_eq!(
            e.detail,
            "${#if} opened here is never closed (template ends at t.html:2)"
        );
        let e = blog("${#each T as x}${/each}", EscapeMode::Html).unwrap_err();
        assert_eq!(
            e.detail,
            "'T' is a string, not a list; ${#each} needs a list"
        );
        // blocks without a values callback
        let e = render("${#if T}x${/if}", EscapeMode::None).unwrap_err();
        assert_eq!(e.code, E_TEMPLATE);
    }

    #[test]
    fn truthiness_and_walk() {
        assert!(!is_truthy(None));
        for v in [
            json!(null),
            json!(false),
            json!(""),
            json!(0),
            json!(-0.0),
            json!([]),
        ] {
            assert!(!is_truthy(Some(&v)), "{v}");
        }
        for v in [json!(true), json!("0"), json!(1), json!([0]), json!({})] {
            assert!(is_truthy(Some(&v)), "{v}");
        }
        let span = sp("f", 1, 1);
        let v = json!({"a": [1, {"b": 2}]});
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            walk(&v, &p(&["a", "1", "b"]), "x", &span, false).unwrap(),
            Some(&json!(2))
        );
        assert_eq!(walk(&v, &p(&["a", "9"]), "x", &span, true).unwrap(), None);
        let e = walk(&v, &p(&["a", "9"]), "x", &span, false).unwrap_err();
        assert_eq!(e.detail, "'x.a.9' is undefined ('x.a' is an array)");
    }

    #[test]
    fn js_object_order() {
        let m: serde_json::Map<String, Value> =
            [("b", 1), ("10", 2), ("a", 3), ("2", 4), ("01", 5)]
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
        let keys: Vec<String> = js::object_order(m).keys().cloned().collect();
        assert_eq!(keys, ["2", "10", "b", "a", "01"]);
        assert_eq!(
            js::from_json_parse(serde_json::from_str("[1.0, 2.5, 12345678901234567890]").unwrap())
                .to_string(),
            "[1,2.5,1.2345678901234567e+19]"
        );
    }

    #[test]
    fn fragments_render_in_place_and_report_the_use() {
        let span = SourceSpan::new("t", 1, 1);
        let lookup = |name: &str, _: &[String], s: &SourceSpan| -> Result<Scalar> {
            match name {
                "T" => Ok(Scalar::Str("<&>".into())),
                _ => err(
                    E_UNDEFINED,
                    format!("'{name}' is not an ARG, LET or IMPORT"),
                    Some(s),
                ),
            }
        };
        let frag = |name: &str| -> Result<std::sync::Arc<Vec<TemplateNode>>> {
            Ok(std::sync::Arc::new(match name {
                "a" => parse_render_template_at("<b title=\"${T}\">${T}</b>", "Sitefile", 4, 15)?,
                "b" => parse_render_template("[${c}]", "p.html", 1)?,
                "c" => parse_render_template("${b}", "q.html", 1)?,
                _ => parse_render_template("${nope}", "Sitefile", 9)?,
            }))
        };
        let ctx = TemplateContext {
            is_fragment: &|n: &str| matches!(n, "a" | "b" | "c" | "d"),
            fragment: &frag,
            tree: None,
            inline_svg: false,
        };
        let run = |t: &str, e: EscapeMode| {
            render_template_with(
                &parse_render_template(t, "Sitefile", 1)?,
                &lookup,
                e,
                None,
                Some(&ctx),
            )
        };
        assert_eq!(run("${a}|${T}|${a|raw}", EscapeMode::Html).unwrap(), "<b title=\"&lt;&amp;&gt;\">&lt;&amp;&gt;</b>|&lt;&amp;&gt;|<b title=\"&lt;&amp;&gt;\">&lt;&amp;&gt;</b>");
        assert_eq!(
            run("${a}", EscapeMode::None).unwrap(),
            "<b title=\"<&>\"><&></b>"
        );
        let e = run("x ${d}", EscapeMode::Html).unwrap_err();
        assert_eq!(e.message(), "Sitefile:9:1: 'nope' is not an ARG, LET or IMPORT (in html fragment 'd' used at Sitefile:1:3)");
        let e = run("${b}", EscapeMode::Html).unwrap_err();
        assert_eq!((e.code, e.detail.as_str()), (E_TEMPLATE, "html fragment 'b' uses itself (b → c → b) (in html fragment 'c' used at p.html:1:2) (in html fragment 'b' used at Sitefile:1:1)"));
        assert_eq!(
            run("${a.x}", EscapeMode::Html).unwrap_err().detail,
            "'a' is an html fragment, not a value; insert it with ${a}"
        );
        // An #each variable shadows a fragment of the same name.
        let values =
            |_: &str, _: &[String], _: &SourceSpan, _: bool| -> Result<Option<Cow<'_, Value>>> {
                Ok(Some(Cow::Owned(serde_json::json!(["v"]))))
            };
        let nodes = parse_render_template("${#each xs as a}${a}${/each}", "Sitefile", 1).unwrap();
        assert_eq!(
            render_template_with(&nodes, &lookup, EscapeMode::Html, Some(&values), Some(&ctx))
                .unwrap(),
            "v"
        );
        let _ = span;
    }

    #[test]
    fn differential_fixtures() {
        let data = json!({
            "T": "A & <b>", "N": 3, "F": 1.5, "Z": 0, "B": false, "E": "", "nil": null,
            "list": [{ "t": "x", "tags": ["a", "b"] }, { "t": "y", "tags": [] }, { "t": "z" }], "empty": [], "obj": { "k": "v", "n": [1, 2] }
        });
        let lookup = |name: &str, p: &[String], span: &SourceSpan| -> Result<Scalar> {
            match data.get(name) {
                Some(v) => resolve_json_path(v, p, name, span),
                None => err(
                    E_UNDEFINED,
                    format!("'{name}' is not an ARG, LET or IMPORT"),
                    Some(span),
                ),
            }
        };
        let values = |name: &str,
                      p: &[String],
                      span: &SourceSpan,
                      loose: bool|
         -> Result<Option<Cow<'_, Value>>> {
            match data.get(name) {
                Some(v) => Ok(walk(v, p, name, span, loose)?.map(Cow::Borrowed)),
                None if loose => Ok(None),
                None => err(
                    E_UNDEFINED,
                    format!("'{name}' is not an ARG, LET or IMPORT"),
                    Some(span),
                ),
            }
        };
        // html fragments, as in generate.mjs.
        let fragments = [
            ("F1", "<b title=\"${T}\">${T}</b>"),
            ("F2", "[${F1}|${i.t}]"),
            ("LOOP", "${#each list as x}${x.t}${/each}"),
            ("REC", "(${REC2})"),
            ("REC2", "${REC}"),
            ("BAD", "x ${#if}"),
        ];
        let is_fragment = |n: &str| fragments.iter().any(|(k, _)| *k == n);
        let fragment = |n: &str| -> Result<std::sync::Arc<Vec<TemplateNode>>> {
            let text = fragments
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, t)| *t)
                .unwrap_or_default();
            Ok(std::sync::Arc::new(parse_render_template(
                text,
                &format!("{n}.html"),
                1,
            )?))
        };
        // File helpers, as in generate.mjs: a 3x2 PNG header, a stylesheet and a directory.
        struct Files(Vec<(&'static str, Vec<u8>)>);
        impl crate::media::TemplateTree for Files {
            fn get(&self, path: &str) -> Option<&[u8]> {
                self.0.iter().find(|(p, _)| *p == path).map(|(_, b)| &b[..])
            }
            fn is_directory(&self, path: &str) -> bool {
                self.0
                    .iter()
                    .any(|(p, _)| p.starts_with(&format!("{path}/")))
            }
        }
        let png = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52, 0,
            0, 0, 3, 0, 0, 0, 2,
        ];
        let meta = br#"{"version":1,"src":"/m-2.png","type":"image/png","width":2,"height":1,"srcset":"/m-1.png 1w, /m-2.png 2w","sources":[{"type":"image/webp","srcset":"/m-1.webp 1w"},{"type":"image/png","srcset":"/m-1.png 1w, /m-2.png 2w"}],"variants":[]}"#.to_vec();
        let files = Files(vec![
            ("/m.json", meta),
            ("/a.png", png),
            ("/s.css", b"h1 { color: <red> }\n".to_vec()),
            ("/d/x.txt", b"x".to_vec()),
        ]);
        let ctx = TemplateContext {
            is_fragment: &is_fragment,
            fragment: &fragment,
            tree: Some(&files),
            inline_svg: false,
        };
        fixtures::check("template.json", |t| {
            let nodes = parse_render_template(t, "t.html", 3)?;
            let html =
                render_template_with(&nodes, &lookup, EscapeMode::Html, Some(&values), Some(&ctx))?;
            let none =
                render_template_with(&nodes, &lookup, EscapeMode::None, Some(&values), Some(&ctx))?;
            Ok(json!([html, none]))
        });
    }
}

/// Differential fixtures generated from the TypeScript implementation by
/// `tests/fixtures/data/generate.mjs`.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::js;
    use crate::error::Result;
    use serde_json::Value;

    /// Run `f` on every fixture input of `name` and compare the value (as JSON
    /// text, so key order counts) or the error (code, detail, line, column).
    pub(crate) fn check(name: &str, f: impl Fn(&str) -> Result<Value>) {
        // SITEFILE_DATA_FIXTURES points at a larger, locally generated set
        // (FUZZ_DIR=… FUZZ_COUNT=… node tests/fixtures/data/generate.mjs).
        let dir = std::env::var_os("SITEFILE_DATA_FIXTURES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data")
            });
        let path = dir.join(name);
        let cases: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture file"))
                .expect("fixture JSON");
        let mut failures = Vec::new();
        for case in &cases {
            let input = case["input"].as_str().unwrap_or_default();
            let got = f(input);
            let ok = match (&case["ok"], &case["error"], &got) {
                (Value::String(text), _, Ok(v)) => {
                    let want =
                        js::from_json_parse(serde_json::from_str(text).unwrap_or(Value::Null))
                            .to_string();
                    let got = v.to_string();
                    (want == got)
                        .then_some(())
                        .ok_or(format!("want {want}\n  got  {got}"))
                }
                (_, Value::Object(e), Err(g)) => {
                    let span = g.span.as_ref().map_or((0, 0), |s| (s.line, s.column));
                    let want = (
                        e["code"].as_str().unwrap_or(""),
                        e["detail"].as_str().unwrap_or(""),
                        e["line"].as_u64(),
                        e["column"].as_u64(),
                    );
                    let have = (
                        g.code.as_str(),
                        g.detail.as_str(),
                        Some(span.0 as u64),
                        Some(span.1 as u64),
                    );
                    (want == have)
                        .then_some(())
                        .ok_or(format!("want {want:?}\n  got  {have:?}"))
                }
                (_, _, Ok(v)) => Err(format!("want error {}\n  got  {v}", case["error"])),
                (_, _, Err(g)) => Err(format!(
                    "want {}\n  got  error {} {:?}",
                    case["ok"],
                    g.message(),
                    g.code
                )),
            };
            if let Err(msg) = ok {
                failures.push(format!("{input:?}\n  {msg}"));
            }
        }
        assert!(cases.len() >= 1000, "{name}: only {} fixtures", cases.len());
        assert!(
            failures.is_empty(),
            "{name}: {} of {} differ:\n{}",
            failures.len(),
            cases.len(),
            failures
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
