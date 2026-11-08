//! Completion, hover, definition/references/rename and document symbols.
//! All of them work from the tolerant [`Index`], so they keep working while
//! the document does not parse; hover adds values from the last plan.

use crate::language::ast::REDIRECT_STATUSES;
use crate::language::ast::{
    is_ident, EscapeMode, ExportFormat, ImportKind, OciCompression, ValueType,
};
use crate::language::parser::{
    flags_of, Shape, EXPORT_CLAUSES, FEED_CLAUSES, FINGERPRINT_CLAUSES, IMPORT_CLAUSES,
    INSTRUCTIONS, REDIRECT_CLAUSES, SECRET_FORMS, SITEMAP_CLAUSES, SORT_ORDERS, TRANSFORM_CLAUSES,
};
use crate::language::scanner::{is_js_space, js_trim, version_directive};
use crate::lsp::diagnostics::Analysis;
use crate::lsp::docs;
use crate::lsp::index::{Def, Index, Instr, SymKind, Word};
use crate::lsp::text::{range, utf16_len, utf16_to_byte, Lines};
use serde_json::{json, Value};

// CompletionItemKind / SymbolKind values from the LSP specification.
const CIK_FUNCTION: u32 = 3;
const CIK_VARIABLE: u32 = 6;
const CIK_CLASS: u32 = 7;
const CIK_MODULE: u32 = 9;
const CIK_KEYWORD: u32 = 14;
const CIK_ENUM_MEMBER: u32 = 20;
const CIK_CONSTANT: u32 = 21;

const SK_MODULE: u32 = 2;
const SK_NAMESPACE: u32 = 3;
const SK_PACKAGE: u32 = 4;
const SK_METHOD: u32 = 6;
const SK_FUNCTION: u32 = 12;
const SK_VARIABLE: u32 = 13;
const SK_CONSTANT: u32 = 14;
const SK_EVENT: u32 = 24;

/// A completion candidate before it becomes JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    pub kind: u32,
    pub detail: Option<String>,
    pub insert: Option<String>,
}

fn kw_items<'a>(words: impl IntoIterator<Item = &'a str>, kind: u32) -> Vec<Item> {
    words
        .into_iter()
        .map(|w| Item {
            label: w.to_string(),
            kind,
            detail: None,
            insert: None,
        })
        .collect()
}

fn def_kind(k: SymKind) -> u32 {
    match k {
        SymKind::Stage => CIK_CLASS,
        SymKind::Import => CIK_MODULE,
        SymKind::Plugin | SymKind::Tool => CIK_FUNCTION,
        SymKind::Arg => CIK_CONSTANT,
        SymKind::Let | SymKind::LoopVar => CIK_VARIABLE,
    }
}

fn def_items(defs: &[&Def], suffix: &str) -> Vec<Item> {
    defs.iter()
        .map(|d| Item {
            label: format!("{}{suffix}", d.name),
            kind: def_kind(d.kind),
            detail: Some(format!("{} (line {})", d.kind.label(), d.line + 1)),
            insert: None,
        })
        .collect()
}

/// After the last unclosed `${` in `prefix`: the text typed since it.
fn open_interp(prefix: &str) -> Option<&str> {
    let at = prefix.rfind("${")?;
    let rest = &prefix[at + 2..];
    (!rest.contains('}')).then_some(rest)
}

/// The completion result: items and the start column of the text they replace.
pub struct Completion {
    pub items: Vec<Item>,
    pub start: u32,
}

/// Names a `${` can refer to here: ARG/LET before the instruction (and imports
/// in templates), plus the instruction's loop variables.
fn interp_items(ix: &Index, ins_ix: usize, template: bool) -> Vec<Item> {
    let ins = &ix.instrs[ins_ix];
    let kinds: &[SymKind] = if template {
        &[SymKind::Arg, SymKind::Let, SymKind::Import]
    } else {
        &[SymKind::Arg, SymKind::Let]
    };
    let mut defs = ix.defs_before(ins.start_line, kinds);
    let locals: Vec<&Def> = ix
        .defs
        .iter()
        .filter(|d| d.kind == SymKind::LoopVar && d.instr == ins_ix)
        .collect();
    defs.extend(locals);
    def_items(&defs, "")
}

/// Completion inside `${…}`: `partial` is what was typed after `${`.
fn complete_interp(
    ix: &Index,
    ins_ix: usize,
    partial: &str,
    col: u32,
    template: bool,
) -> Completion {
    // `${file "…" | ` and `${file "…" | im`: the helpers.
    if template {
        if let Some(rest) = partial
            .strip_prefix("file")
            .filter(|r| r.starts_with([' ', '\t']))
        {
            if let Some((_, after)) = rest.rsplit_once('|') {
                let typed = after.trim_start();
                if typed
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    return Completion {
                        items: kw_items(crate::media::file_helpers(), CIK_FUNCTION),
                        start: col - utf16_len(typed),
                    };
                }
            }
            return Completion {
                items: Vec::new(),
                start: col,
            };
        }
    }
    if partial.contains('.') {
        return Completion {
            items: Vec::new(),
            start: col,
        }; // a key of a value: unknown without data
    }
    if template
        && (partial.starts_with('#')
            || partial.starts_with('/')
            || partial.is_empty()
            || "file".starts_with(partial))
    {
        let mut items = kw_items(
            ["#each", "#if", "else", "/each", "/if", "file"],
            CIK_KEYWORD,
        );
        if !partial.starts_with('#') && !partial.starts_with('/') {
            items.extend(interp_items(ix, ins_ix, true));
        } else if partial.contains(' ') {
            // `${#each li` / `${#if !x`: a name after the block keyword.
            let name = partial.rsplit([' ', '!']).next().unwrap_or("");
            return Completion {
                items: interp_items(ix, ins_ix, true),
                start: col - utf16_len(name),
            };
        }
        return Completion {
            items,
            start: col - utf16_len(partial),
        };
    }
    Completion {
        items: interp_items(ix, ins_ix, template),
        start: col - utf16_len(partial),
    }
}

/// Completion at a 0-based line and UTF-16 column.
pub fn completion(lines: &Lines, ix: &Index, line: u32, col: u32) -> Completion {
    let text = lines.get(line);
    let prefix = &text[..utf16_to_byte(text, col)];
    let empty = Completion {
        items: Vec::new(),
        start: col,
    };
    let instructions = || Completion {
        items: kw_items(INSTRUCTIONS, CIK_KEYWORD)
            .into_iter()
            .map(with_signature)
            .collect(),
        start: col - utf16_len(prefix.trim_start()),
    };

    let Some(ins_ix) = ix.instr_at(line) else {
        if prefix.trim_start().starts_with('#') || prefix.trim_start().contains(char::is_whitespace)
        {
            return empty;
        }
        return instructions();
    };
    let ins = &ix.instrs[ins_ix];
    if let Some(body) = &ins.body {
        if body.contains_line(line) {
            if ins.keyword == "RENDER" || ins.keyword == "LET" {
                if let Some(partial) = open_interp(prefix) {
                    return complete_interp(ix, ins_ix, partial, col, true);
                }
            }
            return empty;
        }
    }
    if line > ins.end_line {
        return empty; // the closing delimiter line
    }
    // The word under the cursor (cursor inside or at its end) and the words before it.
    let cur: Option<&Word> = ins
        .words
        .iter()
        .find(|w| w.line == line && w.start < col && col <= w.end);
    let prior: Vec<&Word> = ins
        .words
        .iter()
        .filter(|w| (w.line, w.end) < (line, col) && Some(*w) != cur)
        .collect();
    let typed = cur
        .map(|w| &w.text[..utf16_to_byte(&w.text, col - w.start)])
        .unwrap_or("");
    let start = col - utf16_len(typed);
    if prior.is_empty() {
        return instructions();
    }
    if let Some(partial) = open_interp(typed) {
        if !typed.starts_with('\'') {
            return complete_interp(ix, ins_ix, partial, col, false);
        }
    }
    let kw = ins.keyword.as_str();
    let n = prior.len();
    let in_flags = prior[1..].iter().all(|w| w.text.starts_with("--")) && !flags_of(kw).is_empty();
    if in_flags && typed.starts_with("--") {
        if let Some(v) = typed.strip_prefix("--escape=").filter(|_| kw == "RENDER") {
            return Completion {
                items: kw_items(EscapeMode::ALL.map(EscapeMode::as_str), CIK_ENUM_MEMBER),
                start: col - utf16_len(v),
            };
        }
        if let Some(v) = typed.strip_prefix("--from=") {
            return Completion {
                items: def_items(&ix.defs_before(ins.start_line, &[SymKind::Stage]), ""),
                start: col - utf16_len(v),
            };
        }
        let items = flags_of(kw)
            .iter()
            .filter(|(name, shape)| {
                *shape == Shape::Repeat
                    || !prior
                        .iter()
                        .any(|w| w.text.starts_with(&format!("--{name}")))
            })
            .map(|(name, shape)| Item {
                label: if *shape == Shape::Bare {
                    format!("--{name}")
                } else {
                    format!("--{name}=")
                },
                kind: CIK_KEYWORD,
                detail: docs::flag(name).map(str::to_string),
                insert: None,
            })
            .collect();
        return Completion { items, start };
    }
    let prev = prior[n - 1].upper();
    let stages = || def_items(&ix.defs_before(ins.start_line, &[SymKind::Stage]), "");
    let scratch_and_stages = || {
        let mut v = kw_items(["scratch", "image"], CIK_KEYWORD);
        v.extend(stages());
        v
    };
    // `FROM image <ref> [AT <dir>] [AS name]`: clauses after the reference (`i` is the index of `image`).
    let image_clauses = |i: usize, allow_as: bool| -> Vec<Item> {
        let after = n - i; // words from `image` on
        let at_used = prior.get(i + 2).is_some_and(|w| w.upper() == "AT");
        match (after, prev.as_str()) {
            (_, "AT") | (_, "AS") | (1, _) => Vec::new(),
            (2, _) if allow_as => kw_items(["AT", "AS"], CIK_KEYWORD),
            (2, _) => kw_items(["AT"], CIK_KEYWORD),
            (4, _) if at_used && allow_as => kw_items(["AS"], CIK_KEYWORD),
            _ => Vec::new(),
        }
    };
    let imports = || ix.defs_before(ins.start_line, &[SymKind::Import]);
    let items: Vec<Item> = match kw {
        "FROM" => match n {
            1 => scratch_and_stages(),
            _ if prior[1].upper() == "IMAGE" => image_clauses(1, true),
            2 => kw_items(["AS"], CIK_KEYWORD),
            _ => Vec::new(),
        },
        "STAGE" => match (n, prev.as_str()) {
            (2, _) => kw_items(["FROM"], CIK_KEYWORD),
            (3, "FROM") => scratch_and_stages(),
            _ if n > 3 && prior[3].upper() == "IMAGE" => image_clauses(3, false),
            _ => Vec::new(),
        },
        "ARG" | "LET" => {
            // `html` (an html fragment) is a LET type only.
            let types = || {
                let mut t: Vec<&str> = ValueType::ALL.map(ValueType::as_str).to_vec();
                if kw == "LET" {
                    t.push("html");
                }
                kw_items(t, CIK_ENUM_MEMBER)
            };
            if let Some((_, t)) = typed.split_once(':').filter(|(_, t)| !t.contains('=')) {
                return Completion {
                    items: types(),
                    start: col - utf16_len(t),
                };
            }
            if n == 2 && (prior[1].text.ends_with(':') || prev == ":") || n == 3 && prev == ":" {
                types()
            } else {
                Vec::new()
            }
        }
        "IMPORT" => {
            // The AS of `SECRET <id> AS …` is not the import's AS clause.
            let used: Vec<String> = prior
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < 2 || prior[i - 2].upper() != "SECRET")
                .map(|(_, w)| w.upper())
                .collect();
            match prev.as_str() {
                _ if n == 2 => kw_items(["FROM"], CIK_KEYWORD),
                // SECRET <id> AS HEADER <Name> | SECRET <id> AS BEARER
                "AS" if n >= 3 && prior[n - 3].upper() == "SECRET" => {
                    kw_items(SECRET_FORMS, CIK_KEYWORD)
                }
                _ if n >= 2 && prior[n - 2].upper() == "SECRET" => kw_items(["AS"], CIK_KEYWORD),
                "AS" => kw_items(ImportKind::ALL.map(ImportKind::as_str), CIK_ENUM_MEMBER),
                "SORT" => kw_items(["BY"], CIK_KEYWORD),
                "FROM" | "INTEGRITY" | "BY" | "SCHEMA" | "SECRET" | "HEADER" | "ARG" => Vec::new(),
                "DRAFTS" => kw_items(["true", "false"], CIK_ENUM_MEMBER),
                _ if n >= 4 => {
                    let mut v: Vec<Item> = Vec::new();
                    if n >= 2 && prior[n - 2].upper() == "BY" {
                        v.extend(kw_items(SORT_ORDERS, CIK_KEYWORD));
                    }
                    for c in IMPORT_CLAUSES.iter().filter(|c| {
                        **c == "SECRET" || **c == "ARG" || !used.contains(&c.to_string())
                    }) {
                        v.push(if *c == "SORT" {
                            Item {
                                label: "SORT BY".into(),
                                kind: CIK_KEYWORD,
                                detail: None,
                                insert: Some("SORT BY ".into()),
                            }
                        } else {
                            kw_items([*c], CIK_KEYWORD).remove(0)
                        });
                    }
                    v
                }
                _ => Vec::new(),
            }
        }
        "PLUGIN" => match n {
            2 => kw_items(["FROM"], CIK_KEYWORD),
            4 => kw_items(["INTEGRITY"], CIK_KEYWORD),
            _ => Vec::new(),
        },
        "EXPORT" => match prev.as_str() {
            "AS" => kw_items(ExportFormat::ALL.map(ExportFormat::as_str), CIK_ENUM_MEMBER),
            "COMPRESSION" => kw_items(
                OciCompression::ALL.map(OciCompression::as_str),
                CIK_ENUM_MEMBER,
            ),
            "BASE" | "AT" => Vec::new(),
            _ if n >= 2 => {
                // Clauses come in order: offer the ones after the last one used.
                let last = prior
                    .iter()
                    .filter_map(|w| EXPORT_CLAUSES.iter().position(|c| w.upper() == *c))
                    .max();
                let from = last.map(|l| l + 1).unwrap_or(0);
                kw_items(EXPORT_CLAUSES[from..].iter().copied(), CIK_KEYWORD)
            }
            _ => Vec::new(),
        },
        "COPY" | "RENDER" | "TRANSFORM" => operation_items(ins, &prior, &prev, ix, &imports, typed),
        "FINGERPRINT" => {
            // Patterns, then [MANIFEST <dest>] [INTEGRITY], in this order, after at least one pattern.
            let ops: Vec<String> = prior
                .iter()
                .skip(1)
                .filter(|w| !w.text.starts_with("--"))
                .map(|w| w.upper())
                .collect();
            let last = ops
                .iter()
                .filter_map(|w| FINGERPRINT_CLAUSES.iter().position(|c| w == c))
                .max();
            if ops.is_empty() || prev == "MANIFEST" {
                Vec::new()
            } else {
                let from = last.map_or(0, |l| l + 1);
                kw_items(FINGERPRINT_CLAUSES[from..].iter().copied(), CIK_KEYWORD)
            }
        }
        "SITEMAP" | "FEED" | "REDIRECT" => {
            let ops: Vec<String> = prior
                .iter()
                .skip(1)
                .filter(|w| !w.text.starts_with("--"))
                .map(|w| w.upper())
                .collect();
            let clauses: &[&str] = match kw {
                "SITEMAP" => &SITEMAP_CLAUSES,
                "FEED" => &FEED_CLAUSES,
                _ => &REDIRECT_CLAUSES,
            };
            // Operands that come before the clauses: FEED's import, REDIRECT's from and to.
            let lead = match kw {
                "FEED" => 1,
                "REDIRECT" => 2,
                _ => 0,
            };
            if kw == "FEED" && ops.is_empty() {
                def_items(&imports(), "")
            } else if kw == "REDIRECT" && prev == "STATUS" {
                kw_items(
                    REDIRECT_STATUSES
                        .map(|s| s.to_string())
                        .iter()
                        .map(String::as_str),
                    CIK_ENUM_MEMBER,
                )
            } else if ops.len() < lead || clauses.contains(&prev.as_str()) {
                Vec::new()
            } else {
                // Clauses come in order (BASE and LINK are required): offer the next ones.
                let last = ops
                    .iter()
                    .filter_map(|w| clauses.iter().position(|c| w == c))
                    .max();
                match last.map(|l| clauses[l]) {
                    None if kw != "REDIRECT" => kw_items(["BASE"], CIK_KEYWORD),
                    Some("BASE") if kw == "FEED" => kw_items(["LINK"], CIK_KEYWORD),
                    _ => kw_items(
                        clauses[last.map(|l| l + 1).unwrap_or(0)..].iter().copied(),
                        CIK_KEYWORD,
                    ),
                }
            }
        }
        _ => Vec::new(),
    };
    Completion { items, start }
}

fn with_signature(mut i: Item) -> Item {
    i.detail = docs::signature(&i.label).map(str::to_string);
    i
}

/// COPY/RENDER/TRANSFORM after the flags.
fn operation_items<'a>(
    ins: &Instr,
    prior: &[&Word],
    prev: &str,
    ix: &'a Index,
    imports: &dyn Fn() -> Vec<&'a Def>,
    typed: &str,
) -> Vec<Item> {
    let kw = ins.keyword.as_str();
    let ops: Vec<String> = prior
        .iter()
        .skip(1)
        .filter(|w| !w.text.starts_with("--"))
        .map(|w| w.upper())
        .collect();
    let p = ops.len();
    let has_from_flag = prior.iter().any(|w| w.text.starts_with("--from="));
    let import_refs = || {
        if has_from_flag || typed.starts_with('.') || typed.starts_with('/') {
            Vec::new()
        } else {
            def_items(&imports(), ":")
        }
    };
    let list_imports = || def_items(&imports(), "");
    if prev == "IN" {
        return list_imports();
    }
    if prev == "FOR" || prev == "TO" {
        return Vec::new();
    }
    if p >= 2 && ops[p - 2] == "FOR" {
        return kw_items(["IN"], CIK_KEYWORD);
    }
    match kw {
        "COPY" => {
            if p == 0 {
                import_refs()
            } else {
                Vec::new()
            }
        }
        "RENDER" => match p {
            0 => import_refs(),
            1 => kw_items(["TO"], CIK_KEYWORD),
            3 => kw_items(["FOR"], CIK_KEYWORD),
            // FOR item IN list [BY key] [PAGE n [FIRST <dest>]], in this order.
            _ if p >= 7 && ops[3] == "FOR" && !matches!(prev, "BY" | "PAGE" | "FIRST") => {
                let after = &ops[7..];
                let has = |w: &str| after.iter().any(|o| o == w);
                if has("FIRST") {
                    Vec::new()
                } else if has("PAGE") {
                    kw_items(["FIRST"], CIK_KEYWORD)
                } else if has("BY") {
                    kw_items(["PAGE"], CIK_KEYWORD)
                } else {
                    kw_items(["BY", "PAGE"], CIK_KEYWORD)
                }
            }
            _ => Vec::new(),
        },
        _ => {
            // TRANSFORM [<source>] WITH name TO <dest> [OPTIONS …] [FOR …] [ARGS …]
            if prev == "WITH" {
                return def_items(
                    &ix.defs_before(ins.start_line, &[SymKind::Plugin, SymKind::Tool]),
                    "",
                );
            }
            if ops.iter().any(|o| o == "ARGS") {
                return Vec::new();
            }
            let with = ops.iter().position(|o| o == "WITH");
            let to = ops.iter().position(|o| o == "TO");
            match (with, to) {
                (None, _) if p == 0 => {
                    let mut v = kw_items(["WITH"], CIK_KEYWORD);
                    v.extend(import_refs());
                    v
                }
                (None, _) => kw_items(["WITH"], CIK_KEYWORD),
                (Some(w), None) if p == w + 2 => kw_items(["TO"], CIK_KEYWORD),
                (Some(_), Some(t)) if p >= t + 2 => {
                    let used: Vec<&String> = ops.iter().collect();
                    kw_items(
                        TRANSFORM_CLAUSES
                            .iter()
                            .copied()
                            .filter(|c| !used.iter().any(|u| u == c)),
                        CIK_KEYWORD,
                    )
                }
                _ => Vec::new(),
            }
        }
    }
}

pub fn completion_json(c: &Completion, line: u32, col: u32) -> Value {
    let items: Vec<Value> = c
        .items
        .iter()
        .map(|i| {
            let mut v = json!({
                "label": i.label,
                "kind": i.kind,
                "textEdit": { "range": range(line, c.start, line, col), "newText": i.insert.clone().unwrap_or_else(|| i.label.clone()) },
            });
            if let Some(d) = &i.detail {
                v["detail"] = json!(d);
            }
            v
        })
        .collect();
    json!({ "isIncomplete": false, "items": items })
}

// ------------------------------------------------------------------ hover

fn def_markdown(ix: &Index, def: &Def, analysis: Option<&Analysis>) -> String {
    let ins = &ix.instrs[def.instr];
    let mut s = format!(
        "**{}** `{}` — line {}\n\n```sitefile\n{}\n```",
        def.kind.label(),
        def.name,
        ins.start_line + 1,
        ins.text()
    );
    let plan = analysis.and_then(|a| a.plan.as_ref());
    match def.kind {
        SymKind::Let if ins.is_html_let() => {
            s.push_str("\n\nhtml fragment: evaluated where a RENDER template uses it (`${name}`), and inserted as markup; the values it substitutes are escaped by that RENDER's `--escape`.");
            if let Some(b) = &ins.body {
                s.push_str(&format!("\n\n```html\n{}```", b.text));
            }
        }
        SymKind::Arg | SymKind::Let => {
            if analysis.is_some_and(|a| a.required_args.contains(&def.name)) {
                s.push_str(&format!(
                    "\n\nNo default: a build needs `--build-arg {}=<value>`.",
                    def.name
                ));
            } else if let Some(v) = plan.and_then(|p| p.vars.get(&def.name)) {
                s.push_str(&format!("\n\nValue: `{}`", v.to_js_string()));
            }
        }
        SymKind::Import => {
            if let Some(spec) = plan.and_then(|p| p.imports.iter().find(|i| i.name == def.name)) {
                let kind = spec
                    .as_kind
                    .map(|k| k.as_str().to_string())
                    .unwrap_or_else(|| "default (tree for a directory, else file)".into());
                s.push_str(&format!("\n\nKind: {kind}  \nFrom: `{}`", spec.uri));
            }
        }
        _ => {}
    }
    s
}

fn md(value: String, line: u32, start: u32, end: u32) -> Value {
    json!({ "contents": { "kind": "markdown", "value": value }, "range": range(line, start, line, end) })
}

/// Hover on a comment: the `# sitefile=N` directive or a `# sitefile-lint:` comment (not inside a heredoc body).
pub fn comment_hover(lines: &Lines, ix: &Index, line: u32, col: u32) -> Option<Value> {
    if ix
        .instrs
        .iter()
        .any(|i| i.body.as_ref().is_some_and(|b| b.contains_line(line)))
    {
        return None;
    }
    let raw = lines.get(line);
    if !js_trim(raw).starts_with('#') {
        return None;
    }
    let chars: Vec<char> = raw.chars().collect();
    let hash = chars.iter().position(|c| *c == '#')?;
    let from_hash: String = chars[hash..].iter().collect();
    let text = if version_directive(&chars[hash..]).is_some() {
        docs::version_directive()
    } else if crate::lint::lint_comment(&from_hash).is_some() {
        docs::lint_comment()
    } else {
        return None;
    };
    let start = utf16_len(&chars[..hash].iter().collect::<String>());
    let end = utf16_len(raw.trim_end_matches(is_js_space));
    (start <= col && col <= end).then(|| md(text, line, start, end))
}

pub fn hover(ix: &Index, analysis: Option<&Analysis>, line: u32, col: u32) -> Option<Value> {
    if let Some(r) = ix.imported_at(line, col) {
        return Some(md(
            docs::imported_stage(&r.import, &r.stage),
            r.line,
            r.start,
            r.end,
        ));
    }
    if let Some(o) = ix.occ_at(line, col) {
        if let Some(d) = o.def {
            return Some(md(
                def_markdown(ix, &ix.defs[d], analysis),
                o.line,
                o.start,
                o.end,
            ));
        }
    }
    let ins = &ix.instrs[ix.instr_at(line)?];
    if line > ins.end_line {
        // A template body: `file` and the helpers of `${file … | helper}`.
        let b = ins.body.as_ref().filter(|b| {
            b.contains_line(line) && matches!(ins.keyword.as_str(), "RENDER" | "LET")
        })?;
        let text = b.text.lines().nth((line - b.start_line) as usize)?;
        return file_hover(text, col).map(|(t, s, e)| md(t.to_string(), line, s, e));
    }
    let at = ins.words.iter().position(|w| w.contains(line, col))?;
    let w = &ins.words[at];
    let kw = ins.keyword.as_str();
    let r = |text: &str| Some(md(text.to_string(), w.line, w.start, w.end));
    if at == 0 {
        return docs::instruction(&w.text).and_then(|t| r(&t));
    }
    if let Some(flag) = w
        .text
        .strip_prefix("--")
        .filter(|_| at < ins.first_operand())
    {
        let (name, value) = flag.split_once('=').unwrap_or((flag, ""));
        let value_starts = w.start + 2 + utf16_len(name) + 1;
        if name == "escape" && !value.is_empty() && col >= value_starts {
            return docs::escape(value).and_then(r);
        }
        return docs::flag(name).and_then(r);
    }
    let prev = ins.words[at - 1].upper();
    let text = match (kw, prev.as_str()) {
        ("IMPORT", "AS") => docs::import_kind(&w.text),
        ("EXPORT", "AS") => docs::export_format(&w.text),
        ("EXPORT", "COMPRESSION") => docs::compression(&w.text),
        ("ARG" | "LET", _) => {
            let t = w
                .text
                .rsplit(':')
                .next()
                .unwrap_or("")
                .split('=')
                .next()
                .unwrap_or("");
            docs::value_type(t).or_else(|| docs::value_type(&w.text))
        }
        _ => docs::clause(kw, &w.text),
    };
    text.and_then(r)
}

/// Hover in a template line: `file` of `${file` or a helper name after `|` inside one:
/// (text, start, end) in UTF-16 columns.
fn file_hover(text: &str, col: u32) -> Option<(&'static str, u32, u32)> {
    let chars: Vec<char> = text.chars().collect();
    let mut cols = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0u32;
    for c in &chars {
        cols.push(acc);
        acc += c.len_utf16() as u32;
    }
    cols.push(acc);
    let at = cols
        .iter()
        .position(|&c| c > col)
        .map(|k| k.saturating_sub(1))
        .unwrap_or(chars.len());
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let mut s = at.min(chars.len());
    while s > 0 && word(chars[s - 1]) {
        s -= 1;
    }
    let mut e = at.min(chars.len());
    while e < chars.len() && word(chars[e]) {
        e += 1;
    }
    if s == e {
        return None;
    }
    let w: String = chars[s..e].iter().collect();
    let before: String = chars[..s].iter().collect();
    if w == "file" && before.ends_with("${") {
        return Some((docs::template_file(), cols[s], cols[e]));
    }
    let open = before.rfind("${file")?;
    if before[open..].contains('}') || !before.trim_end().ends_with('|') {
        return None;
    }
    docs::file_helper(&w).map(|t| (t, cols[s], cols[e]))
}

// ------------------------------------------------------------------ navigation

fn location(uri: &str, line: u32, start: u32, end: u32) -> Value {
    json!({ "uri": uri, "range": range(line, start, line, end) })
}

pub fn definition(ix: &Index, uri: &str, line: u32, col: u32) -> Option<Value> {
    if let Some(r) = ix.imported_at(line, col) {
        return imported_definition(ix, uri, r);
    }
    let d = &ix.defs[ix.occ_at(line, col)?.def?];
    Some(location(uri, d.line, d.start, d.end))
}

/// The local directory an `IMPORT name FROM <dir> AS sitefile` names, as written (None for Git, URLs and `${…}`).
fn imported_dir<'a>(ix: &'a Index, import: &str) -> Option<&'a str> {
    let def = ix
        .defs
        .iter()
        .find(|d| d.kind == SymKind::Import && d.name == import)?;
    let ins = &ix.instrs[def.instr];
    let from = ins.find_kw("FROM", 2)?;
    let dir = ins.words.get(from + 1)?.text.as_str();
    let local = !dir.contains("://")
        && !dir.contains("${")
        && !dir.starts_with(['"', '\''])
        && !dir.to_ascii_lowercase().starts_with("git+");
    local.then_some(dir)
}

/// `file://` URI of an absolute path (`/` kept, other reserved and non-ASCII bytes percent-encoded).
fn file_uri(p: &std::path::Path) -> String {
    let mut s = String::from("file://");
    for b in p.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Go to the stage of a local Sitefile imported AS sitefile: `<dir>/Sitefile` next to this document
/// (the build context is the document's directory), read from disk.
fn imported_definition(ix: &Index, uri: &str, r: &crate::lsp::index::ImportedRef) -> Option<Value> {
    let dir = imported_dir(ix, &r.import)?;
    let base = crate::lsp::uri_path(uri)?;
    let mut path = base.parent()?.to_path_buf();
    for seg in dir.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            return None; // outside the context: a build refuses it
        }
        path.push(seg);
    }
    path.push("Sitefile");
    let text = std::fs::read_to_string(&path).ok()?;
    let other = crate::lsp::index::build(&Lines::new(&text));
    let d = other
        .defs
        .iter()
        .find(|d| d.kind == SymKind::Stage && d.name == r.stage)?;
    Some(location(&file_uri(&path), d.line, d.start, d.end))
}

pub fn references(ix: &Index, uri: &str, line: u32, col: u32, include_decl: bool) -> Value {
    let Some(def) = ix.occ_at(line, col).and_then(|o| o.def) else {
        return json!([]);
    };
    let v: Vec<Value> = ix
        .occs_of(def)
        .into_iter()
        .filter(|o| include_decl || !o.is_def)
        .map(|o| location(uri, o.line, o.start, o.end))
        .collect();
    json!(v)
}

pub fn prepare_rename(ix: &Index, line: u32, col: u32) -> Option<Value> {
    let o = ix.occ_at(line, col)?;
    o.def?;
    Some(json!({ "range": range(o.line, o.start, o.line, o.end), "placeholder": o.name }))
}

/// `Err` carries a message for an invalid new name.
pub fn rename(
    ix: &Index,
    uri: &str,
    line: u32,
    col: u32,
    new_name: &str,
) -> Result<Option<Value>, String> {
    if !is_ident(new_name) {
        return Err(format!(
            "'{new_name}' is not a valid name ([A-Za-z_][A-Za-z0-9_]*)"
        ));
    }
    let Some(def) = ix.occ_at(line, col).and_then(|o| o.def) else {
        return Ok(None);
    };
    let edits: Vec<Value> = ix
        .occs_of(def)
        .into_iter()
        .map(|o| json!({ "range": range(o.line, o.start, o.line, o.end), "newText": new_name }))
        .collect();
    let mut changes = serde_json::Map::new();
    changes.insert(uri.to_string(), json!(edits));
    Ok(Some(json!({ "changes": changes })))
}

// ------------------------------------------------------------------ symbols

fn instr_range(lines: &Lines, ins: &Instr) -> Value {
    let last = ins.last_line();
    range(ins.start_line, 0, last, lines.width(last))
}

fn word_range(w: &Word) -> Value {
    range(w.line, w.start, w.line, w.end)
}

fn short(s: String) -> String {
    if s.chars().count() <= 60 {
        return s;
    }
    let mut t: String = s.chars().take(59).collect();
    t.push('…');
    t
}

fn child_symbol(lines: &Lines, ins: &Instr) -> Value {
    let kw = ins.keyword.as_str();
    let named = matches!(kw, "ARG" | "LET" | "IMPORT" | "PLUGIN" | "TOOL");
    let name_word = if named { ins.words.get(1) } else { None };
    let (name, detail) = match name_word {
        Some(w) => {
            let n: String = w
                .text
                .chars()
                .take_while(|c| *c == '_' || c.is_ascii_alphanumeric())
                .collect();
            let n = if n.is_empty() { w.text.clone() } else { n };
            (n, kw.to_string())
        }
        None => (short(ins.text()), String::new()),
    };
    let kind = match kw {
        "ARG" => SK_CONSTANT,
        "LET" => SK_VARIABLE,
        "IMPORT" => SK_PACKAGE,
        "PLUGIN" | "TOOL" => SK_FUNCTION,
        "EXPORT" => SK_EVENT,
        _ => SK_METHOD,
    };
    let sel = name_word.unwrap_or(&ins.words[0]);
    let mut v = json!({
        "name": name,
        "kind": kind,
        "range": instr_range(lines, ins),
        "selectionRange": word_range(sel),
    });
    if !detail.is_empty() {
        v["detail"] = json!(detail);
    }
    v
}

/// Stages are top-level symbols with their instructions as children; ARGs
/// before the first stage are top-level too.
pub fn document_symbols(lines: &Lines, ix: &Index) -> Value {
    let mut top: Vec<Value> = Vec::new();
    let mut stage: Option<(Value, Vec<Value>)> = None;
    fn close(
        top: &mut Vec<Value>,
        stage: &mut Option<(Value, Vec<Value>)>,
        lines: &Lines,
        last: Option<&Instr>,
    ) {
        if let Some((mut s, children)) = stage.take() {
            if let Some(l) = last {
                let end = l.last_line();
                s["range"]["end"] = json!({ "line": end, "character": lines.width(end) });
            }
            s["children"] = json!(children);
            top.push(s);
        }
    }
    let mut prev: Option<&Instr> = None;
    for ins in &ix.instrs {
        if ins.words.is_empty() {
            continue;
        }
        let kw = ins.keyword.as_str();
        let starts_stage = kw == "FROM" || kw == "STAGE";
        let implicit = !starts_stage && stage.is_none() && kw != "ARG";
        if starts_stage || implicit {
            close(&mut top, &mut stage, lines, prev);
            // (name, selection word, detail, named)
            let (name, sel, detail, named) = if implicit {
                (
                    "(implicit FROM scratch)".to_string(),
                    &ins.words[0],
                    "unnamed stage".to_string(),
                    false,
                )
            } else if kw == "STAGE" {
                let w = ins.words.get(1).unwrap_or(&ins.words[0]);
                (w.text.clone(), w, short(ins.text()), true)
            } else {
                match ins.find_kw("AS", 2).and_then(|a| ins.words.get(a + 1)) {
                    Some(w) => (w.text.clone(), w, short(ins.text()), true),
                    None => (
                        short(ins.text()),
                        &ins.words[0],
                        "unnamed stage".to_string(),
                        false,
                    ),
                }
            };
            let sym = json!({
                "name": name,
                "detail": detail,
                "kind": if named { SK_NAMESPACE } else { SK_MODULE },
                "range": instr_range(lines, ins),
                "selectionRange": word_range(sel),
            });
            stage = Some((sym, Vec::new()));
            if starts_stage {
                prev = Some(ins);
                continue;
            }
        }
        match stage.as_mut() {
            Some((_, children)) => children.push(child_symbol(lines, ins)),
            None => top.push(child_symbol(lines, ins)),
        }
        prev = Some(ins);
    }
    close(&mut top, &mut stage, lines, prev);
    json!(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::analyze;
    use crate::lsp::index::build;

    const DOC: &str = "ARG TITLE = Hello\nARG URL\nIMPORT posts FROM ./posts AS markdown\nPLUGIN og FROM ./plugins/og.mjs\nSTAGE assets\nCOPY ./static/ /static/\nSTAGE site FROM assets\nRENDER --escape=html <<HTML TO /${TITLE}.html\n<h1>${TITLE}</h1>\nHTML\nRENDER ./post.html TO /posts/${p.slug}.html FOR p IN posts\nFROM site\nEXPORT / AS zip";

    fn labels_at(src: &str, line: u32, col: u32) -> Vec<String> {
        let lines = Lines::new(src);
        let ix = build(&lines);
        completion(&lines, &ix, line, col)
            .items
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    /// Complete at the end of `src` (its last line).
    fn labels(src: &str) -> Vec<String> {
        let lines = Lines::new(src);
        let last = lines.len() as u32 - 1;
        labels_at(src, last, lines.width(last))
    }

    #[test]
    fn instruction_keywords_at_line_start() {
        let l = labels("FROM scratch\nCO");
        assert_eq!(l, INSTRUCTIONS.map(String::from));
        assert_eq!(labels("FROM scratch\n"), INSTRUCTIONS.map(String::from));
        assert!(labels("# comment").is_empty());
    }

    #[test]
    fn clause_values() {
        assert_eq!(
            labels("IMPORT p FROM ./x AS "),
            ImportKind::ALL.map(|k| k.as_str().to_string())
        );
        assert_eq!(
            labels("IMPORT p FROM ./x AS ma"),
            ImportKind::ALL.map(|k| k.as_str().to_string())
        );
        assert_eq!(labels("RENDER ./t TO /a FOR x IN p "), ["BY", "PAGE"]);
        assert_eq!(labels("RENDER ./t TO /a FOR x IN p BY tags "), ["PAGE"]);
        assert_eq!(labels("RENDER ./t TO /a FOR x IN p PAGE 3 "), ["FIRST"]);
        assert_eq!(
            labels("RENDER ./t TO /a FOR x IN p PAGE 3 FIRST /i "),
            Vec::<String>::new()
        );
        assert_eq!(
            labels("IMPORT p FROM ./x AS json DRAFTS "),
            ["true", "false"]
        );
        assert_eq!(
            labels("IMPORT p FROM ./x "),
            [
                "AS",
                "INTEGRITY",
                "SORT BY",
                "SCHEMA",
                "DRAFTS",
                "SECRET",
                "ARG"
            ]
        );
        assert_eq!(
            labels("IMPORT p FROM ./x AS json SORT BY date "),
            [
                "ASC",
                "DESC",
                "INTEGRITY",
                "SCHEMA",
                "DRAFTS",
                "SECRET",
                "ARG"
            ]
        );
        // ARG NAME=value repeats; nothing is offered for the name itself.
        assert_eq!(
            labels("IMPORT t FROM ./t AS sitefile ARG A=1 "),
            ["INTEGRITY", "SORT BY", "SCHEMA", "DRAFTS", "SECRET", "ARG"]
        );
        assert!(labels("IMPORT t FROM ./t AS sitefile ARG ").is_empty());
        assert_eq!(labels("IMPORT p FROM https://e.x/d SECRET tok "), ["AS"]);
        assert_eq!(
            labels("IMPORT p FROM https://e.x/d SECRET tok AS "),
            ["HEADER", "BEARER"]
        );
        assert_eq!(
            labels("IMPORT p FROM https://e.x/d SECRET tok AS BEARER "),
            [
                "AS",
                "INTEGRITY",
                "SORT BY",
                "SCHEMA",
                "DRAFTS",
                "SECRET",
                "ARG"
            ]
        );
        assert_eq!(labels("IMPORT p "), ["FROM"]);
        assert_eq!(
            labels("EXPORT / AS "),
            ExportFormat::ALL.map(|f| f.as_str().to_string())
        );
        assert_eq!(labels("EXPORT / "), EXPORT_CLAUSES.map(String::from));
        assert_eq!(labels("EXPORT / AS oci BASE x "), ["AT", "COMPRESSION"]);
        assert_eq!(labels("EXPORT / AS oci COMPRESSION "), ["gzip", "none"]);
        assert_eq!(labels("RENDER --escape="), ["none", "html"]);
        assert_eq!(labels("ARG N: "), ["string", "number", "boolean"]);
        assert_eq!(labels("ARG N:nu"), ["string", "number", "boolean"]);
    }

    #[test]
    fn flags_in_context() {
        assert_eq!(
            labels("RENDER --"),
            ["--escape=", "--overwrite", "--from=", "--inline-svg"]
        );
        assert_eq!(
            labels("COPY --overwrite --"),
            ["--from=", "--exclude=", "--chmod=", "--parents"]
        );
        // --exclude is repeatable.
        assert_eq!(
            labels("COPY --exclude=a --chmod=644 --"),
            ["--overwrite", "--from=", "--exclude=", "--parents"]
        );
        assert_eq!(
            labels("TRANSFORM --include=*.jpg --"),
            ["--overwrite", "--from=", "--include=", "--exclude="]
        );
        assert!(labels("EXPORT --").is_empty());
    }

    #[test]
    fn stage_names_defined_earlier() {
        let src = "STAGE a\nCOPY x /x\nFROM scratch AS b\nCOPY --from=";
        assert_eq!(labels(src), ["a", "b"]);
        assert_eq!(labels("STAGE a\nSTAGE c FROM "), ["scratch", "image", "a"]);
        assert_eq!(labels("STAGE a\nFROM "), ["scratch", "image", "a"]);
        // Only stages before the line.
        assert_eq!(labels_at("FROM \nSTAGE late", 0, 5), ["scratch", "image"]);
    }

    #[test]
    fn from_image_clauses() {
        assert!(labels("FROM image ").is_empty());
        assert_eq!(labels("FROM image ghcr.io/me/theme:1 "), ["AT", "AS"]);
        assert!(labels("FROM image t:1 AT ").is_empty());
        assert_eq!(labels("FROM image t:1 AT /srv/www "), ["AS"]);
        assert!(labels("FROM image t:1 AS ").is_empty());
        assert!(labels("STAGE s FROM image ").is_empty());
        assert_eq!(labels("STAGE s FROM image t:1 "), ["AT"]);
        assert!(labels("STAGE s FROM image t:1 AT /x ").is_empty());
        assert_eq!(labels("FROM ./dir "), ["AS"]);
    }

    #[test]
    fn names_inside_interpolations() {
        let lines = Lines::new(DOC);
        let ix = build(&lines);
        // In an operand: ARG/LET and the loop variable, not imports.
        let c = completion(&lines, &ix, 10, 31);
        let l: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(l, ["TITLE", "URL", "p"]);
        assert_eq!(c.start, 31);
        // In a RENDER body: imports too, and block keywords.
        let src = format!("{}\n", DOC.replace("<h1>${TITLE}</h1>", "<h1>${T"));
        let lines = Lines::new(&src);
        let ix = build(&lines);
        let c = completion(&lines, &ix, 8, 7);
        let l: Vec<&str> = c.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(l, ["TITLE", "URL", "posts"]);
        assert_eq!(c.start, 6);
        assert_eq!(
            labels("ARG A\nRENDER <<H TO /x\n${"),
            ["#each", "#if", "else", "/each", "/if", "file", "A"]
        );
        assert_eq!(
            labels("ARG A\nIMPORT l FROM ./l AS json\nRENDER <<H TO /x\n${#each "),
            ["A", "l"]
        );
    }

    #[test]
    fn operation_clauses() {
        assert_eq!(labels("IMPORT a FROM ./a\nCOPY "), ["a:"]);
        assert!(labels("IMPORT a FROM ./a\nCOPY ./").is_empty());
        assert_eq!(labels("RENDER ./t "), ["TO"]);
        assert_eq!(labels("RENDER ./t TO /x "), ["FOR"]);
        assert_eq!(labels("RENDER ./t TO /x FOR p "), ["IN"]);
        assert_eq!(
            labels("IMPORT l FROM ./l AS json\nRENDER ./t TO /x FOR p IN "),
            ["l"]
        );
        assert_eq!(
            labels("PLUGIN og FROM ./o.mjs\nTOOL ff\nTRANSFORM ./a WITH "),
            ["og", "ff"]
        );
        assert_eq!(labels("TRANSFORM ./a WITH og "), ["TO"]);
        assert_eq!(
            labels("TRANSFORM ./a WITH og TO /b "),
            ["OPTIONS", "FOR", "ARGS"]
        );
        assert_eq!(
            labels("TRANSFORM ./a WITH og TO /b OPTIONS a=1 "),
            ["FOR", "ARGS"]
        );
        // SITEMAP, FEED and REDIRECT clauses, in order
        assert_eq!(labels("SITEMAP "), ["BASE"]);
        assert_eq!(labels("SITEMAP --overwrite "), ["BASE"]);
        assert!(labels("SITEMAP BASE ").is_empty());
        assert_eq!(labels("SITEMAP BASE https://x "), ["TO"]);
        assert_eq!(
            labels("IMPORT posts FROM ./p AS markdown\nFEED "),
            ["posts"]
        );
        assert_eq!(labels("FEED posts "), ["BASE"]);
        assert_eq!(labels("FEED posts BASE https://x "), ["LINK"]);
        assert_eq!(
            labels("FEED posts BASE https://x LINK /a "),
            ["TO", "TITLE"]
        );
        assert_eq!(
            labels("FEED posts BASE https://x LINK /a TO /f.xml "),
            ["TITLE"]
        );
        assert!(labels("REDIRECT /a ").is_empty());
        assert_eq!(labels("REDIRECT /a /b "), ["STATUS"]);
        assert_eq!(
            labels("REDIRECT /a /b STATUS "),
            ["301", "302", "303", "307", "308"]
        );
        assert_eq!(labels("HEADER --"), ["--overwrite"]);
        // FINGERPRINT: patterns, then MANIFEST once
        assert!(labels("FINGERPRINT ").is_empty());
        assert_eq!(
            labels("FINGERPRINT --"),
            ["--overwrite", "--no-cache-headers"]
        );
        assert_eq!(labels("FINGERPRINT /assets/ "), ["MANIFEST", "INTEGRITY"]);
        assert_eq!(
            labels("FINGERPRINT --overwrite /a/ /b/*.css "),
            ["MANIFEST", "INTEGRITY"]
        );
        assert!(labels("FINGERPRINT /a/ MANIFEST ").is_empty());
        assert_eq!(labels("FINGERPRINT /a/ MANIFEST /m.json "), ["INTEGRITY"]);
        assert!(labels("FINGERPRINT /a/ INTEGRITY ").is_empty());
        // CHECKLINKS: flags only
        assert_eq!(labels("CHECKLINKS --"), ["--warn", "--external"]);
        assert_eq!(labels("CHECKLINKS --warn --"), ["--external"]);
        assert!(labels("CHECKLINKS ").is_empty());
    }

    #[test]
    fn completion_json_replaces_the_typed_prefix() {
        let lines = Lines::new("COPY --ov");
        let ix = build(&lines);
        let c = completion(&lines, &ix, 0, 9);
        let j = completion_json(&c, 0, 9);
        assert_eq!(j["items"][0]["textEdit"]["range"], range(0, 5, 0, 9));
        assert_eq!(j["items"][0]["textEdit"]["newText"], "--overwrite");
    }

    #[test]
    fn hover_docs_and_names() {
        let lines = Lines::new(DOC);
        let ix = build(&lines);
        let a = analyze(DOC, "Sitefile");
        let h = |l, c| {
            hover(&ix, Some(&a), l, c).map(|v| v["contents"]["value"].as_str().unwrap().to_string())
        };
        assert!(h(2, 1).unwrap().contains("IMPORT name FROM <uri>"));
        assert!(h(2, 31).unwrap().starts_with("Markdown documents"));
        assert!(h(12, 13).unwrap().starts_with("A zip archive"));
        assert!(h(7, 10).unwrap().contains("`--escape=none|html`"));
        assert!(h(7, 18).unwrap().starts_with("Escape"));
        assert!(h(10, 45).unwrap().contains("`FOR item IN list`"));
        // Names: where they are defined, and the value the plan computed.
        let t = h(8, 7).unwrap();
        assert!(
            t.contains("**ARG** `TITLE` — line 1") && t.contains("Value: `Hello`"),
            "{t}"
        );
        assert!(h(1, 5)
            .unwrap()
            .contains("No default: a build needs `--build-arg URL=<value>`"));
        let s = h(11, 6).unwrap();
        assert!(
            s.contains("**stage** `site` — line 7") && s.contains("STAGE site FROM assets"),
            "{s}"
        );
        assert!(h(10, 55).unwrap().contains("Kind: markdown"));
        assert!(h(8, 1).is_none()); // template text
    }

    #[test]
    fn hover_on_the_version_directive() {
        let src = "  # sitefile=1\n# other\nCOPY <<EOF /a\n# sitefile=1\nEOF\nEXPORT /";
        let lines = Lines::new(src);
        let ix = build(&lines);
        let h = |l, c| comment_hover(&lines, &ix, l, c);
        let v = h(0, 5).unwrap();
        assert!(v["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("The syntax version this file is written for"));
        assert_eq!(v["range"], range(0, 2, 0, 14));
        assert!(h(0, 0).is_none());
        assert!(h(1, 3).is_none());
        assert!(h(3, 3).is_none()); // heredoc body
        let src = "# sitefile-lint: disable=all\nEXPORT /";
        let lines = Lines::new(src);
        let ix = build(&lines);
        let v = comment_hover(&lines, &ix, 0, 3).unwrap();
        assert!(v["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("`unused-name`"));
    }

    #[test]
    fn html_fragments_hover_definition_rename_completion() {
        let src = "ARG SITE = x\nLET nav: html = <nav>${SITE}</nav>\nLET header: html <<HTML\n<header>${nav}</header>\nHTML\nRENDER --escape=html <<HTML TO /i.html\n${header}${nav}\nHTML\nEXPORT /";
        let lines = Lines::new(src);
        let ix = build(&lines);
        let a = analyze(src, "Sitefile");
        // Hover at a use shows the fragment and its heredoc body.
        let h = hover(&ix, Some(&a), 6, 3).unwrap()["contents"]["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            h.contains("**LET** `header` — line 3")
                && h.contains("html fragment")
                && h.contains("```html\n<header>${nav}</header>\n```"),
            "{h}"
        );
        // Definition from the RENDER body and from inside another fragment's body.
        assert_eq!(
            definition(&ix, "file:///S", 6, 11).unwrap()["range"],
            range(1, 4, 1, 7)
        );
        assert_eq!(
            definition(&ix, "file:///S", 3, 11).unwrap()["range"],
            range(1, 4, 1, 7)
        );
        // SITE used in a one-line fragment.
        assert_eq!(
            definition(&ix, "file:///S", 1, 23).unwrap()["range"],
            range(0, 4, 0, 8)
        );
        // Rename `nav`: its LET, the header body and the RENDER body.
        let e = rename(&ix, "file:///S", 6, 11, "menu").unwrap().unwrap();
        assert_eq!(e["changes"]["file:///S"].as_array().unwrap().len(), 3);
        // Completion inside a fragment heredoc body offers names, and `html` after `LET x:`.
        let l = labels_at("ARG SITE = x\nLET h: html <<H\n${S\nH\nEXPORT /", 2, 3);
        assert!(l.contains(&"SITE".to_string()), "{l:?}");
        assert!(labels("LET h: ").contains(&"html".to_string()));
        assert!(!labels("ARG h: ").contains(&"html".to_string()));
        // No template error diagnostics beyond validate's.
        assert!(
            a.diagnostics
                .iter()
                .all(|d| d.severity != crate::lsp::diagnostics::ERROR),
            "{:?}",
            a.diagnostics
        );
    }

    #[test]
    fn file_helpers_completion_and_hover() {
        let src = "COPY ./a/ /img/\nRENDER --escape=html <<HTML TO /i.html\n${file \"/img/a.png\" | img alt=\"x\"}\n${fi\n${file /a.css | \nHTML\nEXPORT /";
        assert!(labels_at(src, 3, 4).contains(&"file".to_string()));
        let helpers = labels_at(src, 4, 16);
        assert!(
            helpers.contains(&"img".to_string()) && helpers.contains(&"stylesheet".to_string()),
            "{helpers:?}"
        );
        let lines = Lines::new(src);
        let ix = build(&lines);
        let h = |l, c| {
            hover(&ix, None, l, c).map(|v| v["contents"]["value"].as_str().unwrap().to_string())
        };
        assert!(h(2, 3).unwrap().contains("${file \"/path\""));
        assert!(h(2, 23).unwrap().starts_with("`<img src width height"));
        assert!(h(2, 12).is_none()); // inside the path
    }

    #[test]
    fn definition_references_rename() {
        let lines = Lines::new(DOC);
        let ix = build(&lines);
        // `assets` in `STAGE site FROM assets` → its STAGE line.
        assert_eq!(
            definition(&ix, "file:///S", 6, 18).unwrap()["range"],
            range(4, 6, 4, 12)
        );
        // TITLE: definition, the RENDER destination and the template body.
        let r = references(&ix, "file:///S", 0, 5, true);
        let at: Vec<(u64, u64)> = r
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["range"]["start"]["line"].as_u64().unwrap(),
                    l["range"]["start"]["character"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(at, [(0, 4), (7, 34), (8, 6)]);
        assert_eq!(
            references(&ix, "file:///S", 0, 5, false)
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let e = rename(&ix, "file:///S", 8, 7, "HEADING").unwrap().unwrap();
        assert_eq!(e["changes"]["file:///S"].as_array().unwrap().len(), 3);
        assert!(rename(&ix, "file:///S", 8, 7, "9x").is_err());
        assert_eq!(prepare_rename(&ix, 4, 8).unwrap()["placeholder"], "assets");
        assert!(definition(&ix, "file:///S", 5, 2).is_none());
    }

    #[test]
    fn imported_stages_definition_hover_references() {
        // IMPORT … AS sitefile (issue #97): `theme+assets` is a use of `theme` and a stage of ./theme/Sitefile.
        let dir = std::env::temp_dir().join(format!(
            "sitefile-lsp-import-{}",
            crate::export::exporter::random_hex(6)
        ));
        std::fs::create_dir_all(dir.join("my theme")).unwrap();
        std::fs::write(
            dir.join("my theme").join("Sitefile"),
            "ARG A = 1\n\nSTAGE assets\nEXPORT /\n",
        )
        .unwrap();
        let src = "IMPORT theme FROM \"./my theme\" AS sitefile ARG A=2\nIMPORT git FROM git+https://e.x/t.git#v1 AS sitefile\nSTAGE s FROM theme+assets\nCOPY --from=theme+assets / /a/\nCOPY --from=git+x / /b/\nEXPORT /\n";
        let ix = build(&Lines::new(src));
        // Quoted directories are not followed (the word keeps its quotes); unquoted ones are.
        assert!(definition(&ix, &file_uri(&dir.join("Sitefile")), 2, 21).is_none());
        let src = src
            .replace("\"./my theme\"", "./my%20theme")
            .replace("./my%20theme", "./mytheme");
        std::fs::rename(dir.join("my theme"), dir.join("mytheme")).unwrap();
        let ix = build(&Lines::new(&src));
        let uri = file_uri(&dir.join("Sitefile"));
        let d = definition(&ix, &uri, 2, 21).unwrap();
        assert_eq!(d["uri"], file_uri(&dir.join("mytheme").join("Sitefile")));
        assert_eq!(d["range"], range(2, 6, 2, 12));
        assert_eq!(
            definition(&ix, &uri, 3, 20).unwrap()["range"],
            range(2, 6, 2, 12)
        );
        // The import part goes to the IMPORT; it is a reference of it.
        assert_eq!(
            definition(&ix, &uri, 2, 15).unwrap()["range"],
            range(0, 7, 0, 12)
        );
        assert_eq!(
            references(&ix, &uri, 0, 8, true).as_array().unwrap().len(),
            3
        );
        // A Git import is not followed; hover explains the stage either way.
        assert!(definition(&ix, &uri, 4, 17).is_none());
        let h = hover(&ix, None, 2, 21).unwrap();
        assert!(h["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("stage `assets` of the Sitefile imported as `theme`"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_position_of_odd_documents() {
        let docs = [
            DOC,
            "",
            "\\",
            "COPY \\\n\\\n",
            "RENDER <<A TO /x\n${#each\n${",
            "ARG é😀=\"${\nLET x:=${'\nFROM\nSTAGE\nIMPORT\nEXPORT AS",
            "COPY --from= --escape= <<-T '${A' \"${B\nT\n\tT",
            "TRANSFORM WITH TO FOR IN ARGS OPTIONS\n#\n   # x\n\u{feff}FROM scratch",
        ];
        for src in docs {
            let lines = Lines::new(src);
            let ix = build(&lines);
            let a = analyze(src, "Sitefile");
            let _ = document_symbols(&lines, &ix);
            for line in 0..lines.len() as u32 + 1 {
                for col in 0..lines.width(line) + 2 {
                    let _ = completion(&lines, &ix, line, col);
                    let _ = hover(&ix, Some(&a), line, col);
                    let _ = definition(&ix, "u", line, col);
                    let _ = references(&ix, "u", line, col, true);
                    let _ = rename(&ix, "u", line, col, "x");
                }
            }
        }
    }

    #[test]
    fn symbols_by_stage() {
        let lines = Lines::new(DOC);
        let ix = build(&lines);
        let s = document_symbols(&lines, &ix);
        let names: Vec<&str> = s
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "TITLE",
                "URL",
                "(implicit FROM scratch)",
                "assets",
                "site",
                "FROM site"
            ]
        );
        let implicit = &s[2]["children"];
        assert_eq!(
            implicit
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["posts", "og"]
        );
        let site = &s[4];
        assert_eq!(site["children"].as_array().unwrap().len(), 2);
        // The stage range ends where its last instruction (a RENDER) ends.
        assert_eq!(site["range"]["end"]["line"], 10);
        assert_eq!(s[5]["children"][0]["name"], "EXPORT / AS zip");
    }
}
