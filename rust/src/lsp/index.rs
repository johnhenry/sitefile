//! A tolerant, line-based index of a Sitefile for editor features (symbols,
//! definitions, references, completion context, hover).
//!
//! Diagnostics come from the real pipeline (`diagnostics.rs`); this index only
//! has to cope with a document that does not parse yet (the user is typing).
//! It follows the scanner's line rules (comments, `\` continuations, heredoc
//! bodies after COPY/RENDER/TRANSFORM and `LET name: html`) and splits each instruction into
//! whitespace-separated words, keeping quoted strings whole. Positions are
//! 0-based lines and UTF-16 columns.

use crate::language::ast::{imported_stage, is_ident};
use crate::language::scanner::{is_html_let, is_js_space};
use crate::lsp::text::{utf16_len, Lines};
use std::collections::HashMap;

/// One word of an instruction (a word never spans a continuation).
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

impl Word {
    pub fn upper(&self) -> String {
        self.text.to_uppercase()
    }
    pub fn contains(&self, line: u32, col: u32) -> bool {
        self.line == line && self.start <= col && col <= self.end
    }
}

/// A heredoc body.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub delimiter: String,
    /// First body line.
    pub start_line: u32,
    /// The closing delimiter line (or the last line when unterminated).
    pub end_line: u32,
    pub closed: bool,
    /// The `<<NAME` word's index in `Instr::words`.
    pub marker: usize,
    /// The body lines, each ending in `\n` (for hover).
    pub text: String,
}

impl Body {
    /// Body lines, excluding the closing delimiter line.
    pub fn contains_line(&self, line: u32) -> bool {
        line >= self.start_line && (line < self.end_line || (!self.closed && line == self.end_line))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instr {
    /// The first word, uppercased.
    pub keyword: String,
    /// Every word, the keyword first.
    pub words: Vec<Word>,
    pub start_line: u32,
    /// Last physical line of the instruction itself (continuations included, body excluded).
    pub end_line: u32,
    pub body: Option<Body>,
}

impl Instr {
    /// The instruction's text with continuations joined (no body).
    pub fn text(&self) -> String {
        self.words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
    /// Last line covered, heredoc body included.
    pub fn last_line(&self) -> u32 {
        self.body
            .as_ref()
            .map(|b| b.end_line)
            .unwrap_or(self.end_line)
    }
    /// Index of the first word after the leading `--flags`.
    pub fn first_operand(&self) -> usize {
        let mut i = 1;
        while self.words.get(i).is_some_and(|w| w.text.starts_with("--")) {
            i += 1;
        }
        i
    }
    /// `LET name: html …`: an html fragment, whose value may be a heredoc template.
    pub fn is_html_let(&self) -> bool {
        is_html_let(&self.text().chars().collect::<Vec<_>>())
    }
    /// Index of the first word (after `from`) that equals `kw` (case-insensitive).
    pub fn find_kw(&self, kw: &str, from: usize) -> Option<usize> {
        (from..self.words.len()).find(|&i| self.words[i].text.eq_ignore_ascii_case(kw))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymKind {
    Stage,
    Arg,
    Let,
    Import,
    Plugin,
    Tool,
    /// `FOR item IN list` on RENDER/TRANSFORM, or `${#each list as item}` in a RENDER body.
    LoopVar,
}

impl SymKind {
    pub fn label(self) -> &'static str {
        match self {
            SymKind::Stage => "stage",
            SymKind::Arg => "ARG",
            SymKind::Let => "LET",
            SymKind::Import => "IMPORT",
            SymKind::Plugin => "PLUGIN",
            SymKind::Tool => "TOOL",
            SymKind::LoopVar => "loop variable",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: SymKind,
    pub line: u32,
    pub start: u32,
    pub end: u32,
    /// Index of the defining instruction.
    pub instr: usize,
}

/// A use or definition of a name.
#[derive(Debug, Clone, PartialEq)]
pub struct Occ {
    pub name: String,
    pub line: u32,
    pub start: u32,
    pub end: u32,
    /// Index into `Index::defs`, if the name resolves.
    pub def: Option<usize>,
    pub is_def: bool,
}

/// The `<stage>` part of `<import>+<stage>` (a stage of a Sitefile imported AS sitefile), in FROM, STAGE … FROM or `--from=`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedRef {
    pub import: String,
    pub stage: String,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Index {
    pub instrs: Vec<Instr>,
    pub defs: Vec<Def>,
    pub occs: Vec<Occ>,
    pub imported: Vec<ImportedRef>,
}

fn trailing_backslashes(s: &str) -> usize {
    s.chars().rev().take_while(|c| *c == '\\').count()
}

/// Split `text` (one physical line, from UTF-16 column `base`) into words.
fn words_of(text: &str, line: u32, base: u32, out: &mut Vec<Word>) {
    let chars: Vec<char> = text.chars().collect();
    let mut col = base;
    let mut i = 0;
    while i < chars.len() {
        if is_js_space(chars[i]) {
            col += chars[i].len_utf16() as u32;
            i += 1;
            continue;
        }
        let start_i = i;
        let start_col = col;
        let mut quote: Option<char> = None;
        while i < chars.len() {
            let c = chars[i];
            match quote {
                Some(q) => {
                    if c == '\\' && q == '"' && i + 1 < chars.len() {
                        col += c.len_utf16() as u32;
                        i += 1;
                    } else if c == q {
                        quote = None;
                    }
                }
                None => {
                    if is_js_space(c) {
                        break;
                    }
                    if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == '\\' && i + 1 < chars.len() {
                        col += c.len_utf16() as u32;
                        i += 1;
                    }
                }
            }
            col += chars[i].len_utf16() as u32;
            i += 1;
        }
        out.push(Word {
            text: chars[start_i..i].iter().collect(),
            line,
            start: start_col,
            end: col,
        });
    }
}

/// `<<NAME` / `<<-NAME`: (strip tabs, delimiter).
fn heredoc_marker(word: &str) -> Option<(bool, String)> {
    let rest = word.strip_prefix("<<")?;
    let (strip, name) = match rest.strip_prefix('-') {
        Some(n) => (true, n),
        None => (false, rest),
    };
    is_ident(name).then(|| (strip, name.to_string()))
}

/// Split a document into instructions.
pub fn instructions(lines: &Lines) -> Vec<Instr> {
    let mut out: Vec<Instr> = Vec::new();
    let mut cur: Option<Instr> = None;
    let n = lines.len() as u32;
    let mut ln = 0u32;
    while ln < n {
        let raw = lines.get(ln);
        let trimmed = raw.trim_matches(is_js_space);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            ln += 1;
            continue;
        }
        let cont = trailing_backslashes(raw) % 2 == 1;
        let body = if cont { &raw[..raw.len() - 1] } else { raw };
        let instr = cur.get_or_insert_with(|| Instr {
            keyword: String::new(),
            words: Vec::new(),
            start_line: ln,
            end_line: ln,
            body: None,
        });
        words_of(body, ln, 0, &mut instr.words);
        instr.end_line = ln;
        ln += 1;
        if cont {
            continue;
        }
        let mut instr = cur.take().unwrap_or_else(|| unreachable!());
        instr.keyword = instr.words.first().map(Word::upper).unwrap_or_default();
        if matches!(instr.keyword.as_str(), "COPY" | "RENDER" | "TRANSFORM") || instr.is_html_let()
        {
            let marker = instr
                .words
                .iter()
                .position(|w| heredoc_marker(&w.text).is_some());
            if let Some(m) = marker {
                let (strip, delimiter) = heredoc_marker(&instr.words[m].text).unwrap_or_default();
                let start_line = ln;
                let mut closed = false;
                while ln < n {
                    let l = lines.get(ln);
                    let candidate = if strip { l.trim_start_matches('\t') } else { l };
                    if candidate == delimiter {
                        closed = true;
                        break;
                    }
                    ln += 1;
                }
                let end_line = if closed {
                    ln
                } else {
                    n.saturating_sub(1).max(start_line)
                };
                let last = if closed { end_line } else { n };
                let text: String = (start_line..last)
                    .map(|l| format!("{}\n", lines.get(l)))
                    .collect();
                instr.body = Some(Body {
                    delimiter,
                    start_line,
                    end_line,
                    closed,
                    marker: m,
                    text,
                });
                ln = end_line + 1;
            }
        }
        out.push(instr);
    }
    if let Some(mut instr) = cur {
        instr.keyword = instr.words.first().map(Word::upper).unwrap_or_default();
        out.push(instr);
    }
    out
}

/// A `${name…}` reference found in text: (name, start col, end col) relative to the text,
/// in UTF-16 units. `each_var` is set for `${#each list as item}` (the `item` word).
#[derive(Debug, Clone, PartialEq)]
pub struct Interp {
    pub name: String,
    pub start: u32,
    pub end: u32,
    pub each_var: Option<(String, u32, u32)>,
}

fn ident_at(chars: &[char], mut i: usize) -> Option<(usize, usize)> {
    let start = i;
    if !matches!(chars.get(i), Some(c) if *c == '_' || c.is_ascii_alphabetic()) {
        return None;
    }
    while matches!(chars.get(i), Some(c) if *c == '_' || c.is_ascii_alphanumeric()) {
        i += 1;
    }
    Some((start, i))
}

/// Find `${NAME`, `${NAME.key}`, `${#each NAME as item}`, `${#if !NAME}` in `text`.
/// `$${` (a literal `${` in templates) and `\${` are skipped.
pub fn interpolations(text: &str) -> Vec<Interp> {
    let chars: Vec<char> = text.chars().collect();
    let mut cols = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0u32;
    for c in &chars {
        cols.push(acc);
        acc += c.len_utf16() as u32;
    }
    cols.push(acc);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '$' && chars[i + 1] == '$' && chars.get(i + 2) == Some(&'{') {
            i += 3;
            continue;
        }
        if chars[i] != '$' || chars[i + 1] != '{' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while chars.get(j).is_some_and(|c| *c == ' ') {
            j += 1;
        }
        let mut each = false;
        if chars.get(j) == Some(&'#') {
            let word: String = chars[j + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            each = word == "each";
            j += 1 + word.len();
            while chars.get(j).is_some_and(|c| *c == ' ') {
                j += 1;
            }
            if chars.get(j) == Some(&'!') {
                j += 1;
            }
        }
        if let Some((a, b)) = ident_at(&chars, j) {
            let name: String = chars[a..b].iter().collect();
            // `${else}` is a block tag, not a name.
            if !(name == "else" && chars.get(b) == Some(&'}')) {
                let mut each_var = None;
                if each {
                    // `as item`: skip the dotted path, then ` as `.
                    let mut k = b;
                    while matches!(chars.get(k), Some(c) if *c == '.' || *c == '-' || *c == '_' || c.is_ascii_alphanumeric())
                    {
                        k += 1;
                    }
                    let rest: String = chars[k..].iter().take(4).collect();
                    if rest == " as " {
                        if let Some((va, vb)) = ident_at(&chars, k + 4) {
                            each_var = Some((chars[va..vb].iter().collect(), cols[va], cols[vb]));
                        }
                    }
                }
                out.push(Interp {
                    name,
                    start: cols[a],
                    end: cols[b],
                    each_var,
                });
            }
            i = b;
        } else {
            i = j.max(i + 2);
        }
    }
    out
}

/// Leading identifier of a word (`NAME` of `NAME:number=1`): its char length.
fn leading_ident(s: &str) -> Option<&str> {
    let n = s
        .chars()
        .take_while(|c| *c == '_' || c.is_ascii_alphanumeric())
        .count();
    let name = &s[..n];
    is_ident(name).then_some(name)
}

enum Pending {
    /// A use of a name in the global namespace.
    Global,
    /// A bare FROM base that is a stage only if a stage of that name exists.
    StageOnly,
    /// Inside instruction `usize`: a loop variable of that instruction wins.
    Scoped(usize),
}

pub fn build(lines: &Lines) -> Index {
    let instrs = instructions(lines);
    let mut defs: Vec<Def> = Vec::new();
    let mut uses: Vec<(String, u32, u32, u32, Pending)> = Vec::new();
    let mut imported: Vec<ImportedRef> = Vec::new();
    // `<import>+<stage>` at `start` of `line`: a use of the import, then the stage of the imported Sitefile.
    let mut imported_ref = |text: &str,
                            line: u32,
                            start: u32,
                            uses: &mut Vec<(String, u32, u32, u32, Pending)>|
     -> bool {
        let Some((import, stage)) = imported_stage(text) else {
            return false;
        };
        let mid = start + utf16_len(import);
        uses.push((import.to_string(), line, start, mid, Pending::Global));
        imported.push(ImportedRef {
            import: import.to_string(),
            stage: stage.to_string(),
            line,
            start: mid + 1,
            end: mid + 1 + utf16_len(stage),
        });
        true
    };

    for (ix, ins) in instrs.iter().enumerate() {
        let w = &ins.words;
        let word = |i: usize| w.get(i);
        let name_def = |defs: &mut Vec<Def>, i: usize, kind: SymKind| {
            if let Some(word) = w.get(i) {
                if let Some(name) = leading_ident(&word.text) {
                    let end = word.start + utf16_len(name);
                    defs.push(Def {
                        name: name.to_string(),
                        kind,
                        line: word.line,
                        start: word.start,
                        end,
                        instr: ix,
                    });
                }
            }
        };
        match ins.keyword.as_str() {
            "FROM" => {
                if let Some(b) = word(1) {
                    if imported_ref(&b.text, b.line, b.start, &mut uses) {
                    } else if is_ident(&b.text) && !b.text.eq_ignore_ascii_case("scratch") {
                        uses.push((b.text.clone(), b.line, b.start, b.end, Pending::StageOnly));
                    }
                }
                if let Some(a) = ins.find_kw("AS", 2) {
                    name_def(&mut defs, a + 1, SymKind::Stage);
                }
            }
            "STAGE" => {
                name_def(&mut defs, 1, SymKind::Stage);
                if let Some(f) = ins.find_kw("FROM", 2) {
                    if let Some(b) = word(f + 1) {
                        if imported_ref(&b.text, b.line, b.start, &mut uses) {
                        } else if is_ident(&b.text) && !b.text.eq_ignore_ascii_case("scratch") {
                            uses.push((b.text.clone(), b.line, b.start, b.end, Pending::StageOnly));
                        }
                    }
                }
            }
            "ARG" => name_def(&mut defs, 1, SymKind::Arg),
            "LET" => name_def(&mut defs, 1, SymKind::Let),
            "IMPORT" => name_def(&mut defs, 1, SymKind::Import),
            "PLUGIN" => name_def(&mut defs, 1, SymKind::Plugin),
            "TOOL" => name_def(&mut defs, 1, SymKind::Tool),
            "FEED" => {
                if let Some(l) = word(ins.first_operand()) {
                    if is_ident(&l.text) {
                        uses.push((l.text.clone(), l.line, l.start, l.end, Pending::Global));
                    }
                }
            }
            "COPY" | "RENDER" | "TRANSFORM" => {
                for fw in &w[1..ins.first_operand()] {
                    if let Some(v) = fw.text.strip_prefix("--from=") {
                        if imported_ref(v, fw.line, fw.start + 7, &mut uses) {
                        } else if is_ident(v) {
                            let start = fw.start + 7;
                            uses.push((
                                v.to_string(),
                                fw.line,
                                start,
                                start + utf16_len(v),
                                Pending::Global,
                            ));
                        }
                    }
                }
                let src = ins.first_operand();
                if let Some(s) = word(src) {
                    if let Some((name, _)) = s.text.split_once(':') {
                        if is_ident(name) && !s.text.contains("://") {
                            uses.push((
                                name.to_string(),
                                s.line,
                                s.start,
                                s.start + utf16_len(name),
                                Pending::Global,
                            ));
                        }
                    }
                }
                if let Some(f) = ins.find_kw("FOR", src) {
                    name_def(&mut defs, f + 1, SymKind::LoopVar);
                    if ins
                        .words
                        .get(f + 2)
                        .is_some_and(|w| w.text.eq_ignore_ascii_case("IN"))
                    {
                        if let Some(l) = word(f + 3) {
                            if is_ident(&l.text) {
                                uses.push((
                                    l.text.clone(),
                                    l.line,
                                    l.start,
                                    l.end,
                                    Pending::Global,
                                ));
                            }
                        }
                    }
                }
                if ins.keyword == "TRANSFORM" {
                    if let Some(k) = ins.find_kw("WITH", 1) {
                        if let Some(p) = word(k + 1) {
                            if is_ident(&p.text) {
                                uses.push((
                                    p.text.clone(),
                                    p.line,
                                    p.start,
                                    p.end,
                                    Pending::Global,
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // `${…}` in operands (not single-quoted ones, not the keyword).
        for word in w.iter().skip(1) {
            if word.text.starts_with('\'') {
                continue;
            }
            for it in interpolations(&word.text) {
                uses.push((
                    it.name,
                    word.line,
                    word.start + it.start,
                    word.start + it.end,
                    Pending::Scoped(ix),
                ));
            }
        }
        // RENDER and html LET heredoc bodies are templates.
        if let (Some(b), "RENDER" | "LET") = (&ins.body, ins.keyword.as_str()) {
            for l in b.start_line..=b.end_line {
                if !b.contains_line(l) {
                    continue;
                }
                for it in interpolations(lines.get(l)) {
                    if let Some((v, s, e)) = &it.each_var {
                        defs.push(Def {
                            name: v.clone(),
                            kind: SymKind::LoopVar,
                            line: l,
                            start: *s,
                            end: *e,
                            instr: ix,
                        });
                    }
                    uses.push((it.name, l, it.start, it.end, Pending::Scoped(ix)));
                }
            }
        }
    }

    // First definition of a global name wins (later ones are E_DUPLICATE).
    let mut global: HashMap<&str, usize> = HashMap::new();
    for (i, d) in defs.iter().enumerate() {
        if d.kind != SymKind::LoopVar {
            global.entry(d.name.as_str()).or_insert(i);
        }
    }
    let mut occs: Vec<Occ> = defs
        .iter()
        .enumerate()
        .map(|(i, d)| Occ {
            name: d.name.clone(),
            line: d.line,
            start: d.start,
            end: d.end,
            def: Some(i),
            is_def: true,
        })
        .collect();
    for (name, line, start, end, how) in uses {
        let def = match how {
            Pending::Global => global.get(name.as_str()).copied(),
            Pending::StageOnly => global
                .get(name.as_str())
                .copied()
                .filter(|&i| defs[i].kind == SymKind::Stage),
            Pending::Scoped(ix) => defs
                .iter()
                .position(|d| d.kind == SymKind::LoopVar && d.instr == ix && d.name == name)
                .or_else(|| global.get(name.as_str()).copied()),
        };
        if matches!(how, Pending::StageOnly) && def.is_none() {
            continue; // a context directory, not a name
        }
        occs.push(Occ {
            name,
            line,
            start,
            end,
            def,
            is_def: false,
        });
    }
    Index {
        instrs,
        defs,
        occs,
        imported,
    }
}

impl Index {
    /// The occurrence under the cursor.
    pub fn occ_at(&self, line: u32, col: u32) -> Option<&Occ> {
        self.occs
            .iter()
            .find(|o| o.line == line && o.start <= col && col <= o.end)
    }
    /// Every occurrence of definition `def` (its definition first).
    pub fn occs_of(&self, def: usize) -> Vec<&Occ> {
        let mut v: Vec<&Occ> = self.occs.iter().filter(|o| o.def == Some(def)).collect();
        v.sort_by_key(|o| (!o.is_def, o.line, o.start));
        v
    }
    /// The `<stage>` of an `<import>+<stage>` under the cursor.
    pub fn imported_at(&self, line: u32, col: u32) -> Option<&ImportedRef> {
        self.imported
            .iter()
            .find(|r| r.line == line && r.start <= col && col <= r.end)
    }
    /// The instruction whose lines (body included) contain `line`.
    pub fn instr_at(&self, line: u32) -> Option<usize> {
        self.instrs
            .iter()
            .position(|i| i.start_line <= line && line <= i.last_line())
    }
    /// Global definitions of the given kinds that appear before `line`.
    pub fn defs_before(&self, line: u32, kinds: &[SymKind]) -> Vec<&Def> {
        let mut seen = std::collections::HashSet::new();
        self.defs
            .iter()
            .filter(|d| d.line < line && kinds.contains(&d.kind) && seen.insert(d.name.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(src: &str) -> Index {
        build(&Lines::new(src))
    }

    #[test]
    fn instructions_continuations_and_heredocs() {
        let src = "# c\nFROM scratch AS a\nCOPY \\\n  ./x \\\n  /y\nRENDER <<HTML TO /i.html\n<p>${T}</p>\nHTML\nEXPORT /";
        let ix = idx(src);
        let kws: Vec<&str> = ix.instrs.iter().map(|i| i.keyword.as_str()).collect();
        assert_eq!(kws, ["FROM", "COPY", "RENDER", "EXPORT"]);
        assert_eq!((ix.instrs[1].start_line, ix.instrs[1].end_line), (2, 4));
        assert_eq!(
            ix.instrs[1].words[2],
            Word {
                text: "/y".into(),
                line: 4,
                start: 2,
                end: 4
            }
        );
        let b = ix.instrs[2].body.as_ref().unwrap();
        assert_eq!((b.start_line, b.end_line, b.closed), (6, 7, true));
        assert!(b.contains_line(6) && !b.contains_line(7));
        assert_eq!(ix.instr_at(6), Some(2));
    }

    #[test]
    fn unterminated_heredoc_runs_to_the_end() {
        let ix = idx("RENDER <<A TO /x\n${B}\n");
        let b = ix.instrs[0].body.as_ref().unwrap();
        assert!(!b.closed);
        assert!(b.contains_line(1));
    }

    #[test]
    fn quoted_words_and_utf16_columns() {
        let ix = idx("COPY \"a b\" /é😀x");
        let w = &ix.instrs[0].words;
        assert_eq!(w[1].text, "\"a b\"");
        assert_eq!((w[2].start, w[2].end), (11, 16));
    }

    #[test]
    fn interpolation_scan() {
        let v = interpolations(
            "a ${X} $${no} \\${no} ${post.title} ${#each posts as p}${else}${/each} ${#if !y}",
        );
        let names: Vec<&str> = v.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["X", "post", "posts", "y"]);
        assert_eq!((v[0].start, v[0].end), (4, 5));
        assert_eq!(v[2].each_var.as_ref().map(|e| e.0.as_str()), Some("p"));
    }

    #[test]
    fn definitions_and_references() {
        let src = "ARG TITLE: string = Hi\nIMPORT posts FROM ./posts AS markdown\nSTAGE site\nRENDER --from=site ./t TO /${TITLE}/${p.slug}.html FOR p IN posts\nFROM site\nCOPY --from=site / /\nEXPORT /";
        let ix = idx(src);
        let names: Vec<(&str, SymKind)> =
            ix.defs.iter().map(|d| (d.name.as_str(), d.kind)).collect();
        assert_eq!(
            names,
            [
                ("TITLE", SymKind::Arg),
                ("posts", SymKind::Import),
                ("site", SymKind::Stage),
                ("p", SymKind::LoopVar)
            ]
        );
        let site = ix.defs.iter().position(|d| d.name == "site").unwrap();
        let refs: Vec<(u32, u32)> = ix.occs_of(site).iter().map(|o| (o.line, o.start)).collect();
        assert_eq!(refs, [(2, 6), (3, 14), (4, 5), (5, 12)]);
        let p = ix.occ_at(3, 38).unwrap();
        assert_eq!((p.name.as_str(), p.is_def), ("p", false));
        assert_eq!(ix.defs[p.def.unwrap()].kind, SymKind::LoopVar);
        assert!(ix.occ_at(3, 55).unwrap().is_def);
        // A FROM base that is not a stage is a directory: no occurrence.
        assert!(idx("FROM public\nEXPORT /").occs.is_empty());
    }
}
