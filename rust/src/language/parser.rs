//! Sitefile syntax: logical lines to instructions (src/language/parser.ts).
//! Semantic rules live in `validate.rs`.

use crate::digest::is_integrity;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::{
    imported_stage, is_ident, literal_text, DraftsClause, EscapeMode, ExportFormat, ForEach,
    FragmentBody, FromBase, GroupBy, ImportArg, ImportKind, ImportSecret, Instruction,
    OciCompression, Operand, PageClause, SchemaClause, SortBy, Source, StageRef, TemplatePart,
    TransformOption, ValueType, REDIRECT_STATUSES,
};
use crate::language::scanner::{logical_lines, Cursor};
use crate::resources::ignore::is_ignore_pattern;

const NOT_YET: [&str; 5] = ["REMOVE", "GENERATE", "ASSERT", "META", "EXEC"];

// Keyword tables. The parser below matches these words; the language server
// (`crate::lsp`) completes and documents them from the same lists.

/// Every instruction keyword `parse_instruction` accepts.
pub const INSTRUCTIONS: [&str; 17] = [
    "FROM",
    "STAGE",
    "ARG",
    "LET",
    "IMPORT",
    "COPY",
    "RENDER",
    "TRANSFORM",
    "PLUGIN",
    "TOOL",
    "SITEMAP",
    "FEED",
    "REDIRECT",
    "HEADER",
    "FINGERPRINT",
    "CHECKLINKS",
    "EXPORT",
];
/// Clauses after `IMPORT name FROM <uri>`, in any order (`SORT` is followed by `BY key [ASC|DESC]`,
/// `SECRET` by `<id> AS HEADER <Name>` or `<id> AS BEARER`, and may repeat; `ARG` by `NAME=value`,
/// for `AS sitefile`, and may repeat).
pub const IMPORT_CLAUSES: [&str; 7] = [
    "AS",
    "INTEGRITY",
    "SORT",
    "SCHEMA",
    "DRAFTS",
    "SECRET",
    "ARG",
];
/// Clauses after a RENDER's `FOR item IN list`, in this order (`FIRST` only after `PAGE n`).
pub const RENDER_LOOP_CLAUSES: [&str; 3] = ["BY", "PAGE", "FIRST"];
/// What follows `SECRET <id> AS`.
pub const SECRET_FORMS: [&str; 2] = ["HEADER", "BEARER"];
/// `SORT BY key` directions.
pub const SORT_ORDERS: [&str; 2] = ["ASC", "DESC"];
/// Clauses after `EXPORT <path>`, in this order.
pub const EXPORT_CLAUSES: [&str; 4] = ["AS", "BASE", "AT", "COMPRESSION"];
/// Clauses after `TRANSFORM … TO <dest>`.
pub const TRANSFORM_CLAUSES: [&str; 3] = ["OPTIONS", "FOR", "ARGS"];
/// Clauses after `SITEMAP`, in this order (`BASE` is required).
pub const SITEMAP_CLAUSES: [&str; 2] = ["BASE", "TO"];
/// Clauses after `FEED <import>`, in this order (`BASE` and `LINK` are required).
pub const FEED_CLAUSES: [&str; 4] = ["BASE", "LINK", "TO", "TITLE"];
/// Clauses after `FINGERPRINT <pattern>…`.
pub const FINGERPRINT_CLAUSES: [&str; 1] = ["MANIFEST"];
/// Clauses after `REDIRECT <from> <to>`.
pub const REDIRECT_CLAUSES: [&str; 1] = ["STATUS"];
/// Leading flags of SITEMAP, FEED, REDIRECT, HEADER and FINGERPRINT.
pub const SITE_FLAGS: &[(&str, Shape)] = &[("overwrite", Shape::Bare)];
/// Leading flags of CHECKLINKS.
pub const CHECKLINKS_FLAGS: &[(&str, Shape)] = &[("warn", Shape::Bare), ("external", Shape::Bare)];
/// Leading flags of COPY, RENDER and TRANSFORM.
pub const COPY_FLAGS: &[(&str, Shape)] = &[
    ("overwrite", Shape::Bare),
    ("from", Shape::Value),
    ("exclude", Shape::Repeat),
    ("chmod", Shape::Value),
    ("parents", Shape::Bare),
];
pub const RENDER_FLAGS: &[(&str, Shape)] = &[
    ("escape", Shape::Value),
    ("overwrite", Shape::Bare),
    ("from", Shape::Value),
    ("inline-svg", Shape::Bare),
];
pub const TRANSFORM_FLAGS: &[(&str, Shape)] = &[
    ("overwrite", Shape::Bare),
    ("from", Shape::Value),
    ("include", Shape::Repeat),
    ("exclude", Shape::Repeat),
];

/// The leading flags `keyword` accepts (empty for instructions without flags).
pub fn flags_of(keyword: &str) -> &'static [(&'static str, Shape)] {
    match keyword.to_uppercase().as_str() {
        "COPY" => COPY_FLAGS,
        "RENDER" => RENDER_FLAGS,
        "TRANSFORM" => TRANSFORM_FLAGS,
        "SITEMAP" | "FEED" | "REDIRECT" | "HEADER" | "FINGERPRINT" => SITE_FLAGS,
        "CHECKLINKS" => CHECKLINKS_FLAGS,
        _ => &[],
    }
}

/// Parse Sitefile source into instructions (syntax only; see `validate.rs` for semantics).
pub fn parse_sitefile(source: &str, file: &str) -> Result<Vec<Instruction>> {
    logical_lines(source, file)?
        .iter()
        .map(|line| parse_instruction(&mut Cursor::new(line, file)))
        .collect()
}

/// Like [`parse_sitefile`], but keeps going after a bad instruction: every
/// logical line is parsed by the same code, and each one that fails adds its
/// error. A scanner error (an unclosed heredoc, two `<<` bodies) is the only
/// error then. Used by the language server to report several syntax errors.
pub fn parse_sitefile_all(
    source: &str,
    file: &str,
) -> std::result::Result<Vec<Instruction>, Vec<SitefileError>> {
    let lines = logical_lines(source, file).map_err(|e| vec![e])?;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for line in &lines {
        match parse_instruction(&mut Cursor::new(line, file)) {
            Ok(i) => out.push(i),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn parse_instruction(c: &mut Cursor) -> Result<Instruction> {
    let span = c.span();
    let word = c.word();
    let kw = word.to_uppercase();
    match kw.as_str() {
        "FROM" => parse_from(c, span),
        "STAGE" => parse_stage(c, span),
        "ARG" => parse_binding(c, span, true),
        "LET" => parse_binding(c, span, false),
        "IMPORT" => parse_import(c, span),
        "COPY" => parse_copy(c, span),
        "RENDER" => parse_render(c, span),
        "EXPORT" => parse_export(c, span),
        "PLUGIN" => parse_plugin(c, span),
        "TOOL" => parse_tool(c, span),
        "TRANSFORM" => parse_transform(c, span),
        "SITEMAP" => parse_sitemap(c, span),
        "FEED" => parse_feed(c, span),
        "REDIRECT" => parse_redirect(c, span),
        "HEADER" => parse_header(c, span),
        "FINGERPRINT" => parse_fingerprint(c, span),
        "CHECKLINKS" => parse_check_links(c, span),
        "RUN" => Err(SitefileError::at(
            E_UNSUPPORTED,
            "RUN is not supported: Sitefile never runs shell commands",
            &span,
        )),
        k if NOT_YET.contains(&k) => Err(SitefileError::at(
            E_UNSUPPORTED,
            format!("{k} is not supported yet"),
            &span,
        )),
        _ => Err(SitefileError::at(
            E_UNKNOWN_INSTRUCTION,
            format!("unknown instruction '{word}'"),
            &span,
        )),
    }
}

/// A FROM base: `scratch`, `image <ref> [AT <dir>]`, or an operand (stage or directory). `image` is the
/// keyword only when something other than `AS` follows it, so `FROM image` and `FROM image AS x` still
/// name a stage or directory called `image`.
fn from_base(c: &mut Cursor) -> Result<FromBase> {
    if c.keyword("scratch") {
        return Ok(FromBase::Scratch);
    }
    let mark = c.mark();
    if c.keyword("image") && !c.at_end() && !c.peek_keyword("AS") {
        let reference = c.operand("image reference")?;
        let at = if c.keyword("AT") {
            Some(c.operand("image directory")?)
        } else {
            None
        };
        return Ok(FromBase::Image { reference, at });
    }
    c.reset(mark);
    Ok(FromBase::Operand(c.operand("base")?))
}

fn parse_from(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let base = from_base(c)?;
    let alias = if c.keyword("AS") {
        Some(c.ident("stage name", "")?)
    } else {
        None
    };
    c.expect_end()?;
    Ok(Instruction::From {
        span,
        base,
        alias,
        stage_kw: false,
    })
}

/// `STAGE name [FROM base]`: the same IR as `FROM base AS name` (an omitted base is `scratch`).
fn parse_stage(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let alias = c.ident("stage name", "")?;
    let mut base = FromBase::Scratch;
    if c.keyword("FROM") {
        base = from_base(c)?;
    }
    c.expect_end()?;
    Ok(Instruction::From {
        span,
        base,
        alias: Some(alias),
        stage_kw: true,
    })
}

fn parse_binding(c: &mut Cursor, span: SourceSpan, is_arg: bool) -> Result<Instruction> {
    let kind = if is_arg { "ARG" } else { "LET" };
    let name = c.ident(&format!("{kind} name"), ":=")?;
    let mut ty = ValueType::String;
    if c.eat(':') {
        let at = c.span();
        let t = c.ident("type", "=")?.to_lowercase();
        if t == "html" {
            if is_arg {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("ARG {name} cannot be html: an html value is markup written in the Sitefile (LET {name}: html = …)"),
                    &at,
                ));
            }
            return parse_fragment(c, span, name);
        }
        ty = match ValueType::parse(&t) {
            Some(ty) => ty,
            None => {
                let or_html = if is_arg { "" } else { ", or html" };
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("unknown type '{t}' (expected string, number or boolean{or_html})"),
                    &at,
                ));
            }
        };
    }
    let value = if c.eat('=') {
        Some(c.operand("value")?)
    } else {
        None
    };
    c.expect_end()?;
    if is_arg {
        return Ok(Instruction::Arg {
            span,
            name,
            ty,
            default_value: value,
        });
    }
    match value {
        Some(value) => Ok(Instruction::Let {
            span,
            name,
            ty,
            value,
        }),
        None => Err(SitefileError::at(
            E_SYNTAX,
            format!("LET {name} requires a value (LET {name} = ...)"),
            &span,
        )),
    }
}

/// `LET name: html = <markup>` (the rest of the line is the template) or `LET name: html <<HTML`
/// (also `= <<HTML`). A one-line fragment cannot use `\` continuations, so its error positions are exact.
fn parse_fragment(c: &mut Cursor, span: SourceSpan, name: String) -> Result<Instruction> {
    let eq = c.eat('=');
    if let Some(inline) = c.inline() {
        c.expect_end()?;
        return Ok(Instruction::Fragment {
            span,
            name,
            body: FragmentBody::Inline(inline),
        });
    }
    c.expect_inline_used(&format!("the value of LET {name}: html"))?;
    let at = c.span();
    let (text, rest_span, multiline) = c.rest();
    if !eq || text.is_empty() {
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("LET {name}: html requires a value (LET {name}: html = <markup>, or a heredoc: LET {name}: html <<HTML)"),
            if eq { &at } else { &rest_span },
        ));
    }
    if multiline {
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("LET {name}: html on one line cannot continue onto the next line; write it as a heredoc (LET {name}: html <<HTML)"),
            &rest_span,
        ));
    }
    Ok(Instruction::Fragment {
        span,
        name,
        body: FragmentBody::Line {
            text,
            span: rest_span,
        },
    })
}

/// `INTEGRITY sha256:…` after the keyword: a literal digest.
fn integrity_digest(c: &mut Cursor) -> Result<String> {
    let op = c.operand("integrity digest")?;
    match literal_text(&op) {
        Some(text) if is_integrity(&text) => Ok(text),
        _ => Err(SitefileError::at(
            E_SYNTAX,
            "INTEGRITY must be sha256:<64 lowercase hex digits>",
            &op.span,
        )),
    }
}

fn parse_import(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let name = c.ident("import name", "")?;
    c.expect_keyword("FROM")?;
    let uri = c.operand("URI")?;
    let mut as_kind: Option<ImportKind> = None;
    let mut integrity: Option<String> = None;
    let mut sort: Option<SortBy> = None;
    let mut schema: Option<SchemaClause> = None;
    let mut drafts: Option<DraftsClause> = None;
    let mut secrets: Vec<ImportSecret> = Vec::new();
    let mut args: Vec<ImportArg> = Vec::new();
    while !c.at_end() {
        let at = c.span();
        if c.keyword("AS") {
            if as_kind.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "AS given twice", &at));
            }
            let kind_at = c.span();
            let k = c.ident("import kind", "")?.to_lowercase();
            as_kind = Some(ImportKind::parse(&k).ok_or_else(|| {
                SitefileError::at(
                    E_SYNTAX,
                    format!("unknown import kind '{k}' (expected file, text, json, yaml, csv, markdown, wbn, html, tree or sitefile)"),
                    &kind_at,
                )
            })?);
        } else if c.keyword("INTEGRITY") {
            if integrity.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "INTEGRITY given twice", &at));
            }
            integrity = Some(integrity_digest(c)?);
        } else if c.keyword("SORT") {
            if sort.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "SORT given twice", &at));
            }
            c.expect_keyword("BY")?;
            let key = c.ident("sort key", "")?;
            let descending = if c.keyword("DESC") {
                true
            } else {
                c.keyword("ASC");
                false
            };
            sort = Some(SortBy {
                key,
                descending,
                span: at,
            });
        } else if c.keyword("SCHEMA") {
            if schema.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "SCHEMA given twice", &at));
            }
            schema = Some(SchemaClause {
                path: c.operand("schema path")?,
                span: at,
            });
        } else if c.keyword("DRAFTS") {
            if drafts.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "DRAFTS given twice", &at));
            }
            drafts = Some(DraftsClause {
                value: c.operand("true or false")?,
                span: at,
            });
        } else if c.keyword("SECRET") {
            let s = parse_secret_clause(c, at.clone())?;
            if secrets
                .iter()
                .any(|p| p.header.to_lowercase() == s.header.to_lowercase())
            {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("header '{}' is set by two SECRET clauses", s.header),
                    &at,
                ));
            }
            secrets.push(s);
        } else if c.keyword("ARG") {
            // `ARG NAME=value`: a build arg of the Sitefile imported AS sitefile.
            let arg_name = c.ident("ARG name", "=")?;
            if !c.eat('=') {
                return Err(c.error(
                    E_SYNTAX,
                    format!("expected = after ARG {arg_name} (ARG {arg_name}=<value>)"),
                ));
            }
            if args.iter().any(|a| a.name == arg_name) {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("ARG {arg_name} given twice"),
                    &at,
                ));
            }
            let value = c.operand("ARG value")?;
            args.push(ImportArg {
                name: arg_name,
                value,
                span: at,
            });
        } else {
            let w = c.word();
            return Err(SitefileError::at(E_SYNTAX, format!("unexpected '{w}' (expected AS, INTEGRITY, SORT BY, SCHEMA, DRAFTS, SECRET or ARG)"), &at));
        }
    }
    Ok(Instruction::Import {
        span,
        name,
        uri,
        as_kind,
        integrity,
        sort,
        schema,
        drafts,
        secrets,
        args,
    })
}

/// Headers the HTTP client sets itself (or that frame the request): a secret cannot replace them.
const RESERVED_HEADERS: [&str; 14] = [
    "accept",
    "accept-encoding",
    "connection",
    "content-encoding",
    "content-length",
    "content-type",
    "git-protocol",
    "host",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
];

/// An HTTP token (RFC 9110 tchar).
fn is_header_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
}

/// `SECRET <id> AS HEADER <Name>` or `SECRET <id> AS BEARER` (after the SECRET keyword).
fn parse_secret_clause(c: &mut Cursor, span: SourceSpan) -> Result<ImportSecret> {
    let id = c.ident("secret id", "")?;
    c.expect_keyword("AS")?;
    if c.keyword("BEARER") {
        return Ok(ImportSecret {
            id,
            header: "Authorization".into(),
            bearer: true,
            span,
        });
    }
    let at = c.span();
    if !c.keyword("HEADER") {
        return Err(SitefileError::at(
            E_SYNTAX,
            "expected HEADER <name> or BEARER",
            &at,
        ));
    }
    let name_at = c.span();
    let header = c.word();
    if header.is_empty() {
        return Err(SitefileError::at(
            E_SYNTAX,
            "expected a header name",
            &name_at,
        ));
    }
    if !is_header_token(&header) {
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("invalid header name '{header}'"),
            &name_at,
        ));
    }
    if RESERVED_HEADERS.contains(&header.to_lowercase().as_str()) {
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("header '{header}' is set by sitefile and cannot carry a secret"),
            &name_at,
        ));
    }
    Ok(ImportSecret {
        id,
        header,
        bearer: false,
        span,
    })
}

/// Whether a flag is bare (`--overwrite`) or takes a value (`--from=name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Bare,
    Value,
    /// A value flag that may be given several times (`--exclude`).
    Repeat,
}

struct ParsedFlag {
    name: &'static str,
    value: String,
    span: SourceSpan,
}

struct Flags(Vec<ParsedFlag>);

impl Flags {
    fn get(&self, name: &str) -> Option<&ParsedFlag> {
        self.0.iter().find(|f| f.name == name)
    }
    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
    /// Every occurrence of a repeatable flag, in order.
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a ParsedFlag> + 'a {
        self.0.iter().filter(move |f| f.name == name)
    }
}

/// Read leading `--flags` (value `""` for a bare flag).
fn parse_flags(c: &mut Cursor, allowed: &[(&'static str, Shape)]) -> Result<Flags> {
    let mut out = Flags(Vec::new());
    while let Some(f) = c.flag() {
        let Some(&(name, shape)) = allowed.iter().find(|(n, _)| *n == f.name) else {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("unknown flag --{}", f.name),
                &f.span,
            ));
        };
        if out.has(name) && shape != Shape::Repeat {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("--{name} given twice"),
                &f.span,
            ));
        }
        if shape == Shape::Bare && f.value.is_some() {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("--{name} takes no value"),
                &f.span,
            ));
        }
        if shape != Shape::Bare && f.value.as_deref().unwrap_or("").is_empty() {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("--{name} requires a value (--{name}=...)"),
                &f.span,
            ));
        }
        out.0.push(ParsedFlag {
            name,
            value: f.value.unwrap_or_default(),
            span: f.span,
        });
    }
    Ok(out)
}

fn stage_ref(flags: &Flags) -> Result<Option<StageRef>> {
    let Some(f) = flags.get("from") else {
        return Ok(None);
    };
    // A stage of this Sitefile, or `<import>+<stage>` for a stage of a Sitefile imported AS sitefile.
    if !is_ident(&f.value) && imported_stage(&f.value).is_none() {
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("--from needs a stage name, got '{}'", f.value),
            &f.span,
        ));
    }
    Ok(Some(StageRef {
        name: f.value.clone(),
        span: f.span.clone(),
    }))
}

fn no_inline_from(source: &Source, flags: &Flags) -> Result<()> {
    match (flags.get("from"), source) {
        (Some(from), Source::Inline(_)) => Err(SitefileError::at(
            E_SYNTAX,
            "--from cannot be combined with an inline (<<) body",
            &from.span,
        )),
        _ => Ok(()),
    }
}

fn inline_or_operand(c: &mut Cursor, what: &str) -> Result<Source> {
    match c.inline() {
        Some(body) => Ok(Source::Inline(body)),
        None => Ok(Source::Operand(c.operand(what)?)),
    }
}

/// `--chmod` values: Sitefile trees only hold 0644 and 0755 files.
fn chmod_mode(value: &str) -> Option<u32> {
    match value {
        "644" | "0644" => Some(0o644),
        "755" | "0755" => Some(0o755),
        _ => None,
    }
}

/// Repeatable pattern flags (`--exclude`, `--include`), in the order given: each value must be a `.sitefileignore` rule.
fn check_patterns<'a>(patterns: impl Iterator<Item = &'a ParsedFlag>) -> Result<()> {
    for e in patterns {
        if !is_ignore_pattern(&e.value) {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("--{}={} is not a pattern (it is blank or a # comment in .sitefileignore syntax)", e.name, e.value),
                &e.span,
            ));
        }
    }
    Ok(())
}

fn parse_copy(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, COPY_FLAGS)?;
    check_patterns(flags.all("exclude"))?;
    let chmod = match flags.get("chmod") {
        None => None,
        Some(f) => Some(chmod_mode(&f.value).ok_or_else(|| {
            SitefileError::at(
                E_SYNTAX,
                format!("--chmod must be 644 or 755 (files in a Sitefile tree are 0644 or 0755), got '{}'", f.value),
                &f.span,
            )
        })?),
    };
    let source = inline_or_operand(c, "source")?;
    let dest = c.operand("destination")?;
    c.expect_end()?;
    c.expect_inline_used("the COPY source")?;
    no_inline_from(&source, &flags)?;
    for name in ["exclude", "parents"] {
        if let (Some(f), Source::Inline(_)) = (flags.get(name), &source) {
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("--{name} cannot be combined with an inline (<<) body"),
                &f.span,
            ));
        }
    }
    let from = stage_ref(&flags)?;
    let exclude = flags.all("exclude").map(|f| f.value.clone()).collect();
    Ok(Instruction::Copy {
        span,
        source,
        dest,
        from,
        overwrite: flags.has("overwrite"),
        exclude,
        chmod,
        parents: flags.has("parents"),
    })
}

fn parse_for(c: &mut Cursor, span: SourceSpan) -> Result<ForEach> {
    let as_name = c.ident("loop variable", "")?;
    c.expect_keyword("IN")?;
    let list = c.ident("list name", "")?;
    Ok(ForEach {
        as_name,
        list,
        span,
        by: None,
        page: None,
    })
}

/// `FOR item IN list [BY key] [PAGE n [FIRST <dest>]]` of a RENDER.
fn parse_render_for(c: &mut Cursor, span: SourceSpan) -> Result<ForEach> {
    let mut each = parse_for(c, span)?;
    let by_at = c.span();
    if c.keyword("BY") {
        each.by = Some(GroupBy {
            key: c.ident("group key", "")?,
            span: by_at,
        });
    }
    let page_at = c.span();
    if c.keyword("PAGE") {
        let size = c.operand("page size")?;
        let first = if c.keyword("FIRST") {
            Some(c.operand("first page destination")?)
        } else {
            None
        };
        each.page = Some(PageClause {
            size,
            first,
            span: page_at,
        });
    }
    Ok(each)
}

fn parse_render(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, RENDER_FLAGS)?;
    let escape = match flags.get("escape") {
        None => EscapeMode::None,
        Some(f) => match EscapeMode::parse(&f.value) {
            Some(m) => m,
            None => {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("--escape must be none or html, got '{}'", f.value),
                    &f.span,
                ))
            }
        },
    };
    let template = inline_or_operand(c, "template")?;
    c.expect_keyword("TO")?;
    let dest = c.operand("destination")?;
    let for_at = c.span();
    let each = if c.keyword("FOR") {
        Some(parse_render_for(c, for_at)?)
    } else {
        None
    };
    c.expect_end()?;
    c.expect_inline_used("the RENDER template")?;
    no_inline_from(&template, &flags)?;
    let from = stage_ref(&flags)?;
    Ok(Instruction::Render {
        span,
        template,
        dest,
        from,
        escape,
        each,
        overwrite: flags.has("overwrite"),
        inline_svg: flags.has("inline-svg"),
    })
}

fn parse_plugin(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let name = c.ident("plugin name", "")?;
    c.expect_keyword("FROM")?;
    let uri = c.operand("plugin path")?;
    let mut integrity = None;
    let at = c.span();
    if c.keyword("INTEGRITY") {
        integrity = Some(integrity_digest(c)?);
    } else if !c.at_end() {
        let w = c.word();
        return Err(SitefileError::at(
            E_SYNTAX,
            format!("unexpected '{w}' (expected INTEGRITY)"),
            &at,
        ));
    }
    c.expect_end()?;
    Ok(Instruction::Plugin {
        span,
        name,
        uri,
        integrity,
    })
}

fn parse_tool(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let name = c.ident("tool name", "")?;
    c.expect_end()?;
    Ok(Instruction::Tool { span, name })
}

/// ARGS takes the rest of the line: every word is one argv entry.
fn parse_args(c: &mut Cursor) -> Result<Vec<Operand>> {
    let mut args = Vec::new();
    while !c.at_end() {
        args.push(c.operand("argument")?);
    }
    Ok(args)
}

fn parse_transform(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, TRANSFORM_FLAGS)?;
    let patterns: Vec<&ParsedFlag> = flags
        .0
        .iter()
        .filter(|f| f.name == "include" || f.name == "exclude")
        .collect();
    check_patterns(patterns.iter().copied())?;
    // The source is optional (only with FOR): `TRANSFORM WITH og TO … FOR post IN posts`.
    let source = if c.keyword("WITH") {
        None
    } else {
        Some(inline_or_operand(c, "source")?)
    };
    if source.is_some() {
        c.expect_keyword("WITH")?;
    }
    let plugin_span = c.span();
    let plugin = c.ident("plugin name", "")?;
    c.expect_keyword("TO")?;
    let dest = c.operand("destination")?;
    let mut options: Vec<TransformOption> = Vec::new();
    let mut each: Option<ForEach> = None;
    let mut saw_options = false;
    let mut args: Option<Vec<Operand>> = None;
    while !c.at_end() {
        let at = c.span();
        if c.keyword("OPTIONS") {
            if saw_options {
                return Err(SitefileError::at(E_SYNTAX, "OPTIONS given twice", &at));
            }
            saw_options = true;
            // key=value pairs until FOR, ARGS or the end of the line.
            while !c.at_end() {
                let key_at = c.span();
                if c.keyword("FOR") {
                    each = Some(parse_for(c, key_at)?);
                    break;
                }
                if c.keyword("ARGS") {
                    args = Some(parse_args(c)?);
                    break;
                }
                let key = c.ident("option name", "=")?;
                if !c.eat('=') {
                    return Err(SitefileError::at(
                        E_SYNTAX,
                        format!("expected {key}=value"),
                        &key_at,
                    ));
                }
                if options.iter().any(|o| o.key == key) {
                    return Err(SitefileError::at(
                        E_SYNTAX,
                        format!("option '{key}' given twice"),
                        &key_at,
                    ));
                }
                let value = c.operand(&format!("value of {key}"))?;
                options.push(TransformOption {
                    key,
                    value,
                    span: key_at,
                });
            }
        } else if c.keyword("FOR") {
            if each.is_some() {
                return Err(SitefileError::at(E_SYNTAX, "FOR given twice", &at));
            }
            each = Some(parse_for(c, at)?);
        } else if c.keyword("ARGS") {
            args = Some(parse_args(c)?);
        } else {
            let w = c.word();
            return Err(SitefileError::at(
                E_SYNTAX,
                format!("unexpected '{w}' (expected OPTIONS, FOR or ARGS)"),
                &at,
            ));
        }
    }
    c.expect_end()?;
    c.expect_inline_used("the TRANSFORM source")?;
    if let Some(s) = &source {
        no_inline_from(s, &flags)?;
    }
    if source.is_none() {
        if let Some(f) = flags.get("from") {
            return Err(SitefileError::at(
                E_SYNTAX,
                "--from needs a source path",
                &f.span,
            ));
        }
    }
    // --include/--exclude pick the files of a directory source, which runs the plugin once per file.
    if let Some(first) = patterns.first() {
        let name = first.name;
        match &source {
            None => {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("--{name} needs a source directory"),
                    &first.span,
                ))
            }
            Some(Source::Inline(_)) => {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("--{name} cannot be combined with an inline (<<) body"),
                    &first.span,
                ))
            }
            Some(_) if each.is_some() => {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!(
                        "--{name} filters a directory source, which cannot be combined with FOR"
                    ),
                    &first.span,
                ))
            }
            Some(_) => {}
        }
    }
    let include: Vec<String> = flags.all("include").map(|f| f.value.clone()).collect();
    let exclude: Vec<String> = flags.all("exclude").map(|f| f.value.clone()).collect();
    let from = stage_ref(&flags)?;
    Ok(Instruction::Transform {
        span,
        source,
        from,
        plugin,
        plugin_span,
        dest,
        options,
        each,
        args,
        include,
        exclude,
        overwrite: flags.has("overwrite"),
    })
}

/// `SITEMAP [--overwrite] BASE <url> [TO <dest>]`
fn parse_sitemap(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, SITE_FLAGS)?;
    c.expect_keyword("BASE")?;
    let base = c.operand("base URL")?;
    let dest = if c.keyword("TO") {
        Some(c.operand("destination")?)
    } else {
        None
    };
    c.expect_end()?;
    Ok(Instruction::Sitemap {
        span,
        base,
        dest,
        overwrite: flags.has("overwrite"),
    })
}

/// `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]` (clauses in this order)
fn parse_feed(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, SITE_FLAGS)?;
    let list_span = c.span();
    let list = c.ident("import name", "")?;
    c.expect_keyword("BASE")?;
    let base = c.operand("base URL")?;
    c.expect_keyword("LINK")?;
    let link = c.operand("link path")?;
    let dest = if c.keyword("TO") {
        Some(c.operand("destination")?)
    } else {
        None
    };
    let title = if c.keyword("TITLE") {
        Some(c.operand("feed title")?)
    } else {
        None
    };
    c.expect_end()?;
    Ok(Instruction::Feed {
        span,
        list,
        list_span,
        base,
        link,
        dest,
        title,
        overwrite: flags.has("overwrite"),
    })
}

/// `REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]`
fn parse_redirect(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, SITE_FLAGS)?;
    let from = c.operand("redirect source path")?;
    let to = c.operand("redirect target")?;
    let mut status = 301;
    if c.keyword("STATUS") {
        let at = c.span();
        let w = c.word();
        status = match REDIRECT_STATUSES.iter().find(|s| s.to_string() == w) {
            Some(s) => *s,
            None => {
                let msg = if w.is_empty() {
                    "expected a status after STATUS (301, 302, 303, 307 or 308)".to_string()
                } else {
                    format!("unknown redirect status '{w}' (expected 301, 302, 303, 307 or 308)")
                };
                return Err(SitefileError::at(E_SYNTAX, msg, &at));
            }
        };
    }
    c.expect_end()?;
    Ok(Instruction::Redirect {
        span,
        from,
        to,
        status,
        overwrite: flags.has("overwrite"),
    })
}

/// `HEADER [--overwrite] <path-pattern> <Name>: <value …>`
fn parse_header(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, SITE_FLAGS)?;
    let pattern = c.operand("path pattern")?;
    let raw = c.operand("header name")?;
    let mut parts = raw.parts.clone();
    let head = match parts.pop() {
        Some(TemplatePart::Text(t)) if t.ends_with(':') => t[..t.len() - 1].to_string(),
        _ => {
            return Err(SitefileError::at(
                E_SYNTAX,
                "expected 'Name:' (a header name and a colon, then the value: HEADER /* X-Frame-Options: DENY)",
                &raw.span,
            ))
        }
    };
    if !head.is_empty() {
        parts.push(TemplatePart::Text(head));
    }
    if parts.is_empty() {
        return Err(SitefileError::at(
            E_SYNTAX,
            "expected a header name before the colon",
            &raw.span,
        ));
    }
    let mut value = Vec::new();
    while !c.at_end() {
        value.push(c.operand("header value")?);
    }
    if value.is_empty() {
        let at = c.span();
        return Err(SitefileError::at(E_SYNTAX, "expected header value", &at));
    }
    Ok(Instruction::Header {
        span,
        pattern,
        name: Operand {
            parts,
            span: raw.span,
        },
        value,
        overwrite: flags.has("overwrite"),
    })
}

/// `FINGERPRINT [--overwrite] <pattern>… [MANIFEST <dest>]`
fn parse_fingerprint(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, SITE_FLAGS)?;
    let mut patterns = Vec::new();
    while !c.at_end() && !c.peek_keyword("MANIFEST") {
        patterns.push(c.operand("path pattern")?);
    }
    if patterns.is_empty() {
        return Err(c.error(
            E_SYNTAX,
            "expected a path pattern (FINGERPRINT /assets/ or FINGERPRINT /**/*.css)",
        ));
    }
    let manifest = if c.keyword("MANIFEST") {
        Some(c.operand("manifest destination")?)
    } else {
        None
    };
    c.expect_end()?;
    Ok(Instruction::Fingerprint {
        span,
        patterns,
        manifest,
        overwrite: flags.has("overwrite"),
    })
}

/// `CHECKLINKS [--warn] [--external]`
fn parse_check_links(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, CHECKLINKS_FLAGS)?;
    c.expect_end()?;
    Ok(Instruction::CheckLinks {
        span,
        warn: flags.has("warn"),
        external: flags.has("external"),
    })
}

fn parse_export(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let path = c.operand("path")?;
    let mut format = ExportFormat::Directory;
    if c.keyword("AS") {
        let at = c.span();
        let name = c.word();
        if name.is_empty() {
            return Err(SitefileError::at(
                E_SYNTAX,
                "expected export format after AS",
                &at,
            ));
        }
        format = ExportFormat::parse(&name).ok_or_else(|| {
            SitefileError::at(
                E_UNSUPPORTED,
                format!("export format '{name}' is not supported (expected directory, zip, tar, tgz, wbn, oci or car)"),
                &at,
            )
        })?;
    }
    let base = if c.keyword("BASE") {
        Some(c.operand("base URL")?)
    } else {
        None
    };
    let at = if c.keyword("AT") {
        Some(c.operand("image root path")?)
    } else {
        None
    };
    let mut compression = None;
    if c.keyword("COMPRESSION") {
        let cspan = c.span();
        let name = c.word();
        let Some(v) = OciCompression::parse(&name) else {
            let msg = if name.is_empty() {
                "expected gzip or none after COMPRESSION".to_string()
            } else {
                format!("unknown compression '{name}' (expected gzip or none)")
            };
            return Err(SitefileError::at(E_SYNTAX, msg, &cspan));
        };
        compression = Some((v, cspan));
    }
    c.expect_end()?;
    Ok(Instruction::Export {
        span,
        path,
        format,
        base,
        at,
        compression,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::language::ast::TemplatePart;

    pub(crate) fn one(src: &str) -> Instruction {
        let mut all = parse_sitefile(src, "Sitefile").unwrap();
        assert_eq!(all.len(), 1, "{src}");
        all.remove(0)
    }

    #[test]
    fn copy_exclude_chmod_parents() {
        match one("COPY --exclude=*.md --exclude=drafts/ --chmod=0755 --parents ./src /out/") {
            Instruction::Copy {
                exclude,
                chmod,
                parents,
                ..
            } => {
                assert_eq!(exclude, ["*.md", "drafts/"]);
                assert_eq!(chmod, Some(0o755));
                assert!(parents);
            }
            other => panic!("{other:?}"),
        }
        match one("COPY ./a /b") {
            Instruction::Copy {
                exclude,
                chmod,
                parents,
                ..
            } => assert!(exclude.is_empty() && chmod.is_none() && !parents),
            other => panic!("{other:?}"),
        }
        let err = |src: &str| {
            let e = parse_sitefile(src, "Sitefile").unwrap_err();
            (e.code, e.span.map(|s| s.column), e.detail)
        };
        assert_eq!(
            err("COPY --chmod=600 a /b"),
            (
                E_SYNTAX,
                Some(6),
                "--chmod must be 644 or 755 (files in a Sitefile tree are 0644 or 0755), got '600'"
                    .into()
            )
        );
        assert_eq!(
            err("COPY --chmod=644 --chmod=644 a /b").2,
            "--chmod given twice"
        );
        assert_eq!(
            err("COPY --exclude a /b").2,
            "--exclude requires a value (--exclude=...)"
        );
        assert!(err("COPY --exclude=#x a /b")
            .2
            .starts_with("--exclude=#x is not a pattern"));
        assert_eq!(
            err("COPY --parents <<EOF /b\nx\nEOF").2,
            "--parents cannot be combined with an inline (<<) body"
        );
        assert_eq!(
            err("COPY --exclude=x <<EOF /b\nx\nEOF").2,
            "--exclude cannot be combined with an inline (<<) body"
        );
        assert_eq!(
            parse_sitefile("COPY --chmod=755 <<EOF /b\nx\nEOF", "Sitefile")
                .unwrap()
                .len(),
            1
        );
    }

    fn copy_op(src: &str, dest: bool) -> Operand {
        match one(src) {
            Instruction::Copy {
                source: Source::Operand(s),
                dest: d,
                ..
            } => {
                if dest {
                    d
                } else {
                    s
                }
            }
            other => panic!("not a COPY with an operand source: {other:?}"),
        }
    }

    #[derive(Debug, PartialEq)]
    enum P {
        T(&'static str),
        V(&'static str, u32, u32),
    }

    fn parts(o: &Operand) -> Vec<String> {
        o.parts
            .iter()
            .map(|p| match p {
                TemplatePart::Text(t) => t.clone(),
                TemplatePart::Var { name, span, .. } => {
                    format!("${{{name}}}@{}:{}", span.line, span.column)
                }
            })
            .collect()
    }

    fn expect(ps: &[P]) -> Vec<String> {
        ps.iter()
            .map(|p| match p {
                P::T(t) => t.to_string(),
                P::V(n, l, c) => format!("${{{n}}}@{l}:{c}"),
            })
            .collect()
    }

    pub(crate) fn parse_err(src: &str, code: ErrorCode, line: u32, column: u32, contains: &str) {
        let e = parse_sitefile(src, "Sitefile").expect_err(src);
        assert_eq!(e.code, code, "{src}: {e}");
        let s = e.span.as_ref().expect("span");
        assert_eq!((s.line, s.column), (line, column), "{src}: {e}");
        assert!(e.detail.contains(contains), "{src}: {e} lacks {contains:?}");
    }

    fn lit(o: &Operand) -> String {
        literal_text(o).unwrap()
    }

    #[test]
    fn comment_and_blank_lines_inside_a_continuation() {
        let ins = parse_sitefile(
            "COPY \\\n# a comment\n\n  ./a \\\n  # another\n  /b\nEXPORT /",
            "Sitefile",
        )
        .unwrap();
        assert_eq!(ins.len(), 2);
        let Instruction::Copy {
            source: Source::Operand(s),
            dest,
            ..
        } = &ins[0]
        else {
            panic!()
        };
        assert_eq!(lit(s), "./a");
        assert_eq!((dest.span.line, dest.span.column), (6, 3));
    }

    #[test]
    fn trailing_backslashes() {
        let ins = parse_sitefile("COPY ./a /b\\\\\nEXPORT /", "Sitefile").unwrap();
        assert_eq!(ins.len(), 2);
        let Instruction::Copy { dest, .. } = &ins[0] else {
            panic!()
        };
        assert_eq!(lit(dest), "/b\\");
        let ins = parse_sitefile("COPY ./a\\\\\\\n /b", "Sitefile").unwrap();
        assert_eq!(ins.len(), 1);
        let Instruction::Copy {
            source: Source::Operand(s),
            ..
        } = &ins[0]
        else {
            panic!()
        };
        assert_eq!(lit(s), "./a\\");
    }

    #[test]
    fn hash_mid_line_and_crlf() {
        assert_eq!(lit(&copy_op("COPY ./a#b /c#d", true)), "/c#d");
        assert_eq!(
            parse_sitefile("FROM scratch\r\nEXPORT /\r\n", "Sitefile")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn error_spans_across_continuations() {
        parse_err("COPY \\\n  ./a \\\n  /b extra", E_SYNTAX, 3, 6, "");
    }

    #[test]
    fn quoted_operands() {
        assert_eq!(
            parts(&copy_op("COPY \"a b\" x", false)),
            expect(&[P::T("a b")])
        );
        assert_eq!(
            parts(&copy_op("COPY 'a b' x", false)),
            expect(&[P::T("a b")])
        );
        assert_eq!(
            parts(&copy_op("COPY a\\ b x", false)),
            expect(&[P::T("a b")])
        );
        assert!(parts(&copy_op("COPY \"\" x", false)).is_empty());
    }

    #[test]
    fn escapes() {
        assert_eq!(
            parts(&copy_op("COPY \"a\\\"b\\\\c\\n\\t\\$\\q\" x", false)),
            ["a\"b\\c\n\t$q"]
        );
        assert_eq!(parts(&copy_op("COPY a\\$\\{b\\n x", false)), ["a${bn"]);
        assert_eq!(
            parts(&copy_op("COPY 'a\\n${X}\\' x", false)),
            ["a\\n${X}\\"]
        );
        assert_eq!(parts(&copy_op("COPY $a$ x", false)), ["$a$"]);
        assert_eq!(parts(&copy_op("COPY \"cost: $5\" x", false)), ["cost: $5"]);
    }

    #[test]
    fn interpolation_parts() {
        assert_eq!(
            parts(&copy_op("COPY a${X}b${Y_2} x", false)),
            expect(&[P::T("a"), P::V("X", 1, 7), P::T("b"), P::V("Y_2", 1, 12)])
        );
        assert_eq!(
            parts(&copy_op("COPY \"${A} ${B}\" x", false)),
            expect(&[P::V("A", 1, 7), P::T(" "), P::V("B", 1, 12)])
        );
        match &copy_op("COPY ${X} y", false).parts[0] {
            TemplatePart::Var { path, .. } => assert!(path.is_empty()),
            _ => panic!(),
        }
        match &copy_op("COPY ${a.b-c.d_1} y", false).parts[0] {
            TemplatePart::Var { name, path, .. } => assert_eq!(
                (name.as_str(), path.clone()),
                ("a", vec!["b-c".to_string(), "d_1".into()])
            ),
            _ => panic!(),
        }
    }

    #[test]
    fn interpolation_errors() {
        parse_err(
            "COPY ${a.} x",
            E_SYNTAX,
            1,
            6,
            "expected a key after '.' in ${a…}",
        );
        parse_err("COPY ${} x", E_SYNTAX, 1, 6, "expected a name after ${");
        parse_err("COPY ${1A} x", E_SYNTAX, 1, 6, "");
        parse_err(
            "COPY ab${X x",
            E_SYNTAX,
            1,
            8,
            "unterminated ${...}: expected }",
        );
        parse_err("COPY \"${X\" x", E_SYNTAX, 1, 7, "");
    }

    #[test]
    fn unterminated_quotes() {
        parse_err("COPY \"abc x", E_SYNTAX, 1, 6, "unterminated");
        parse_err("COPY x 'abc", E_SYNTAX, 1, 8, "unterminated");
        parse_err("COPY \"abc\\", E_SYNTAX, 1, 6, "unterminated");
        parse_err(
            "COPY \"a\"b x",
            E_SYNTAX,
            1,
            9,
            "unexpected character after closing quote",
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let src = format!(
            "from Scratch as base\ncopy a b\nrender ./t to /x\nimport n from ./x as JSON integrity sha256:{}\nexport / as Directory",
            "a".repeat(64)
        );
        let ins = parse_sitefile(&src, "Sitefile").unwrap();
        let kinds: Vec<_> = ins.iter().map(|i| i.keyword()).collect();
        assert_eq!(kinds, ["FROM", "COPY", "RENDER", "IMPORT", "EXPORT"]);
        assert_eq!(
            ins[0],
            Instruction::From {
                span: SourceSpan::new("Sitefile", 1, 1),
                base: FromBase::Scratch,
                alias: Some("base".into()),
                stage_kw: false
            }
        );
        assert!(matches!(
            ins[3],
            Instruction::Import {
                as_kind: Some(ImportKind::Json),
                ..
            }
        ));
    }

    #[test]
    fn instruction_span() {
        let ins = parse_sitefile("\n\n   COPY a b", "x/Sitefile").unwrap();
        assert_eq!(ins[0].span(), &SourceSpan::new("x/Sitefile", 3, 4));
    }

    #[test]
    fn stage_forms() {
        let at = SourceSpan::new("Sitefile", 1, 1);
        assert_eq!(
            one("STAGE site"),
            Instruction::From {
                span: at.clone(),
                base: FromBase::Scratch,
                alias: Some("site".into()),
                stage_kw: true
            }
        );
        assert_eq!(one("stage site from scratch"), one("STAGE site"));
        assert!(
            matches!(one("STAGE site FROM ./base"), Instruction::From { base: FromBase::Operand(ref o), alias: Some(ref a), stage_kw: true, .. } if lit(o) == "./base" && a == "site")
        );
        assert!(
            matches!(one("STAGE b FROM a"), Instruction::From { base: FromBase::Operand(ref o), alias: Some(ref a), .. } if lit(o) == "a" && a == "b")
        );
        assert!(matches!(
            one("FROM scratch AS site"),
            Instruction::From {
                stage_kw: false,
                ..
            }
        ));
        parse_err("STAGE", E_SYNTAX, 1, 6, "expected stage name");
        parse_err(
            "STAGE FROM scratch",
            E_SYNTAX,
            1,
            12,
            "unexpected 'scratch'",
        );
        parse_err("STAGE x FROM", E_SYNTAX, 1, 13, "expected base");
        parse_err("STAGE x extra", E_SYNTAX, 1, 9, "unexpected 'extra'");
        parse_err("STAGE x AS y", E_SYNTAX, 1, 9, "unexpected 'AS'");
        parse_err(
            "STAGE x FROM scratch extra",
            E_SYNTAX,
            1,
            22,
            "unexpected 'extra'",
        );
        parse_err("STAGE x-y", E_SYNTAX, 1, 8, "invalid stage name");
        parse_err("FROM", E_SYNTAX, 1, 5, "expected base");
    }

    #[test]
    fn from_forms() {
        assert_eq!(
            one("FROM scratch"),
            Instruction::From {
                span: SourceSpan::new("Sitefile", 1, 1),
                base: FromBase::Scratch,
                alias: None,
                stage_kw: false
            }
        );
        assert!(
            matches!(one("FROM \"scratch\""), Instruction::From { base: FromBase::Operand(ref o), .. } if lit(o) == "scratch")
        );
        assert!(
            matches!(one("FROM ./base AS site"), Instruction::From { base: FromBase::Operand(_), alias: Some(ref a), .. } if a == "site")
        );
        parse_err("FROM", E_SYNTAX, 1, 5, "expected base");
        parse_err("FROM scratch AS", E_SYNTAX, 1, 16, "expected stage name");
        parse_err("FROM scratch extra", E_SYNTAX, 1, 14, "unexpected 'extra'");
    }

    #[test]
    fn from_image_forms() {
        let image = |src: &str| match one(src) {
            Instruction::From {
                base: FromBase::Image { reference, at },
                alias,
                stage_kw,
                ..
            } => (lit(&reference), at.as_ref().map(lit), alias, stage_kw),
            other => panic!("{src}: {other:?}"),
        };
        assert_eq!(
            image("FROM image ghcr.io/me/theme:1.2 AS theme"),
            (
                "ghcr.io/me/theme:1.2".into(),
                None,
                Some("theme".into()),
                false
            )
        );
        assert_eq!(
            image("from IMAGE theme:1 at /srv/www"),
            ("theme:1".into(), Some("/srv/www".into()), None, false)
        );
        assert_eq!(
            image("STAGE site FROM image t:1 AT /x"),
            ("t:1".into(), Some("/x".into()), Some("site".into()), true)
        );
        // `image` followed by nothing or AS is still a stage or directory named image.
        assert!(
            matches!(one("FROM image"), Instruction::From { base: FromBase::Operand(ref o), .. } if lit(o) == "image")
        );
        assert!(
            matches!(one("FROM image AS x"), Instruction::From { base: FromBase::Operand(ref o), alias: Some(ref a), .. } if lit(o) == "image" && a == "x")
        );
        assert!(
            matches!(one("STAGE s FROM image"), Instruction::From { base: FromBase::Operand(ref o), .. } if lit(o) == "image")
        );
        parse_err(
            "FROM image theme:1 AT",
            E_SYNTAX,
            1,
            22,
            "expected image directory",
        );
        parse_err(
            "FROM image theme:1 extra",
            E_SYNTAX,
            1,
            20,
            "unexpected 'extra'",
        );
        parse_err(
            "STAGE s FROM image t:1 AS x",
            E_SYNTAX,
            1,
            24,
            "unexpected 'AS'",
        );
    }

    #[test]
    fn arg_forms() {
        let cases: [(&str, &str, ValueType, Option<&str>); 7] = [
            ("ARG X", "X", ValueType::String, None),
            ("ARG X=v", "X", ValueType::String, Some("v")),
            ("ARG X = \"a b\"", "X", ValueType::String, Some("a b")),
            ("ARG N: number = 3", "N", ValueType::Number, Some("3")),
            ("ARG N:number=3", "N", ValueType::Number, Some("3")),
            ("ARG B : boolean", "B", ValueType::Boolean, None),
            ("ARG S:STRING=x", "S", ValueType::String, Some("x")),
        ];
        for (src, n, t, v) in cases {
            let Instruction::Arg {
                name,
                ty,
                default_value,
                ..
            } = one(src)
            else {
                panic!("{src}")
            };
            assert_eq!(name, n, "{src}");
            assert_eq!(ty, t, "{src}");
            assert_eq!(default_value.as_ref().map(lit).as_deref(), v, "{src}");
        }
        parse_err("ARG", E_SYNTAX, 1, 4, "expected ARG name");
        parse_err("ARG 1X", E_SYNTAX, 1, 5, "");
        parse_err(
            "ARG X-Y",
            E_SYNTAX,
            1,
            6,
            "invalid ARG name: unexpected '-'",
        );
        parse_err("ARG X: date", E_SYNTAX, 1, 8, "unknown type 'date'");
        parse_err("ARG X:", E_SYNTAX, 1, 7, "");
        parse_err("ARG X =", E_SYNTAX, 1, 8, "expected value");
        parse_err("ARG X 5", E_SYNTAX, 1, 7, "");
        parse_err("ARG X = a b", E_SYNTAX, 1, 11, "");
    }

    #[test]
    fn let_forms() {
        assert!(
            matches!(one("LET G: boolean = true"), Instruction::Let { ref name, ty: ValueType::Boolean, ref value, .. } if name == "G" && lit(value) == "true")
        );
        assert!(matches!(
            one("LET M=\"hi ${X}\""),
            Instruction::Let {
                ty: ValueType::String,
                ..
            }
        ));
        parse_err(
            "LET X",
            E_SYNTAX,
            1,
            1,
            "LET X requires a value (LET X = ...)",
        );
        parse_err("LET X: number", E_SYNTAX, 1, 1, "requires a value");
    }

    #[test]
    fn import_forms() {
        let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
        let Instruction::Import {
            name,
            uri,
            as_kind,
            integrity,
            ..
        } = one("IMPORT logo FROM https://example.com/logo.png")
        else {
            panic!()
        };
        assert_eq!(
            (name.as_str(), lit(&uri).as_str()),
            ("logo", "https://example.com/logo.png")
        );
        assert!(as_kind.is_none() && integrity.is_none());
        let Instruction::Import {
            as_kind, integrity, ..
        } = one(&format!(
            "IMPORT d FROM ./data.json AS json INTEGRITY {digest}"
        ))
        else {
            panic!()
        };
        assert_eq!(
            (as_kind, integrity.as_deref()),
            (Some(ImportKind::Json), Some(digest.as_str()))
        );
        let Instruction::Import {
            as_kind,
            integrity,
            uri,
            ..
        } = one(&format!(
            "IMPORT d FROM \"./x y\" INTEGRITY \"{digest}\" AS tree"
        ))
        else {
            panic!()
        };
        assert_eq!(
            (as_kind, integrity.as_deref(), lit(&uri).as_str()),
            (Some(ImportKind::Tree), Some(digest.as_str()), "./x y")
        );
        parse_err("IMPORT d ./x", E_SYNTAX, 1, 10, "expected FROM");
        parse_err("IMPORT FROM ./x", E_SYNTAX, 1, 13, "FROM");
        parse_err("IMPORT d FROM", E_SYNTAX, 1, 14, "expected URI");
        parse_err(
            "IMPORT d FROM ./x AS toml",
            E_SYNTAX,
            1,
            22,
            "unknown import kind 'toml'",
        );
        parse_err(
            "IMPORT d FROM ./x AS text AS json",
            E_SYNTAX,
            1,
            27,
            "AS given twice",
        );
        parse_err(
            &format!("IMPORT d FROM ./x INTEGRITY {digest} INTEGRITY {digest}"),
            E_SYNTAX,
            1,
            101,
            "twice",
        );
        parse_err(
            "IMPORT d FROM ./x INTEGRITY sha256:abc",
            E_SYNTAX,
            1,
            29,
            "sha256",
        );
        parse_err(
            &format!("IMPORT d FROM ./x INTEGRITY {}", digest.to_uppercase()),
            E_SYNTAX,
            1,
            29,
            "",
        );
        parse_err("IMPORT d FROM ./x INTEGRITY ${X}", E_SYNTAX, 1, 29, "");
        parse_err(
            "IMPORT d FROM ./x EXTRA",
            E_SYNTAX,
            1,
            19,
            "unexpected 'EXTRA' (expected AS, INTEGRITY, SORT BY, SCHEMA, DRAFTS, SECRET or ARG)",
        );
        let Instruction::Import { sort, .. } =
            one("IMPORT p FROM ./p AS markdown SORT BY date DESC")
        else {
            panic!()
        };
        let s = sort.unwrap();
        assert!(s.key == "date" && s.descending && s.span.column == 31);
        let Instruction::Import { sort, .. } = one("IMPORT p FROM ./p SORT BY date ASC AS csv")
        else {
            panic!()
        };
        assert!(!sort.unwrap().descending);
        parse_err(
            "IMPORT p FROM ./p SORT BY a SORT BY b",
            E_SYNTAX,
            1,
            29,
            "SORT given twice",
        );
        parse_err(
            "IMPORT p FROM ./p SORT date",
            E_SYNTAX,
            1,
            24,
            "expected BY",
        );
    }

    #[test]
    fn copy_forms() {
        let a = one("COPY ./public/ /");
        let Instruction::Copy {
            source: Source::Operand(s),
            dest,
            overwrite,
            span,
            ..
        } = &a
        else {
            panic!()
        };
        assert!(!overwrite && lit(s) == "./public/" && lit(dest) == "/");
        assert_eq!((span.column, dest.span.column), (1, 16));
        assert!(
            matches!(one("COPY --overwrite site: /"), Instruction::Copy { overwrite: true, source: Source::Operand(ref s), .. } if lit(s) == "site:")
        );
        assert!(matches!(
            one("COPY --from=build /a /b"),
            Instruction::Copy { .. }
        ));
        parse_err(
            "COPY --chown=1 /a /b",
            E_SYNTAX,
            1,
            6,
            "unknown flag --chown",
        );
        parse_err(
            "COPY --overwrite=yes /a /b",
            E_SYNTAX,
            1,
            6,
            "--overwrite takes no value",
        );
        parse_err(
            "COPY --overwrite --overwrite /a /b",
            E_SYNTAX,
            1,
            18,
            "--overwrite given twice",
        );
        parse_err("COPY ./a", E_SYNTAX, 1, 9, "expected destination");
        parse_err("COPY", E_SYNTAX, 1, 5, "expected source");
        parse_err("COPY a b c", E_SYNTAX, 1, 10, "unexpected 'c'");
    }

    #[test]
    fn render_forms() {
        assert!(matches!(
            one("RENDER ./t.html TO /index.html"),
            Instruction::Render {
                escape: EscapeMode::None,
                overwrite: false,
                ..
            }
        ));
        assert!(matches!(
            one("RENDER --overwrite --escape=html \"./t.html\" to \"/i.html\""),
            Instruction::Render { escape: EscapeMode::Html, overwrite: true, ref dest, .. } if lit(dest) == "/i.html"
        ));
        assert!(matches!(
            one("RENDER --escape=none t TO /x"),
            Instruction::Render { .. }
        ));
        assert!(matches!(
            one("RENDER --escape=HTML t TO /x"),
            Instruction::Render {
                escape: EscapeMode::Html,
                ..
            }
        ));
        parse_err(
            "RENDER --overwrite --escape=js t TO /x",
            E_SYNTAX,
            1,
            20,
            "--escape must be none or html, got 'js'",
        );
        parse_err(
            "RENDER --escape t TO /x",
            E_SYNTAX,
            1,
            8,
            "--escape requires a value (--escape=...)",
        );
        parse_err("RENDER t /x", E_SYNTAX, 1, 10, "expected TO");
        parse_err("RENDER t TO", E_SYNTAX, 1, 12, "expected destination");
        assert!(matches!(
            one("RENDER --from=a t TO /x"),
            Instruction::Render { from: Some(_), .. }
        ));
        let Instruction::Render { each, .. } =
            one("RENDER ./t TO /p/${post.slug}.html FOR post IN posts")
        else {
            panic!()
        };
        let e = each.unwrap();
        assert_eq!(
            (e.as_name.as_str(), e.list.as_str(), e.span.column),
            ("post", "posts", 36)
        );
        parse_err("RENDER t TO /x FOR a posts", E_SYNTAX, 1, 22, "expected IN");
    }

    #[test]
    fn export_forms() {
        assert!(
            matches!(one("EXPORT /"), Instruction::Export { format: ExportFormat::Directory, ref path, .. } if lit(path) == "/")
        );
        assert!(matches!(
            one("EXPORT /site AS directory"),
            Instruction::Export { .. }
        ));
        assert!(matches!(
            one("EXPORT / AS zip"),
            Instruction::Export {
                format: ExportFormat::Zip,
                ..
            }
        ));
        for (as_name, fmt) in [
            ("TAR", ExportFormat::Tar),
            ("tgz", ExportFormat::Tgz),
            ("tar.gz", ExportFormat::Tgz),
            ("wbn", ExportFormat::Wbn),
            ("oci", ExportFormat::Oci),
        ] {
            assert!(
                matches!(one(&format!("EXPORT / AS {as_name}")), Instruction::Export { format, .. } if format == fmt)
            );
        }
        let Instruction::Export { base, at, .. } =
            one("EXPORT / AS oci BASE https://x/ AT /srv/www")
        else {
            panic!()
        };
        assert!(base.is_some() && at.is_some());
        let Instruction::Export { compression, .. } =
            one("EXPORT / AS oci AT /srv COMPRESSION None")
        else {
            panic!()
        };
        assert_eq!(
            compression.map(|(v, s)| (v, s.column)),
            Some((OciCompression::None, 37))
        );
        let Instruction::Export { compression, .. } = one("EXPORT / AS oci COMPRESSION gzip")
        else {
            panic!()
        };
        assert_eq!(compression.map(|c| c.0), Some(OciCompression::Gzip));
        parse_err(
            "EXPORT / AS oci COMPRESSION zstd",
            E_SYNTAX,
            1,
            29,
            "unknown compression 'zstd' (expected gzip or none)",
        );
        parse_err(
            "EXPORT / AS oci COMPRESSION",
            E_SYNTAX,
            1,
            28,
            "expected gzip or none after COMPRESSION",
        );
        parse_err(
            "EXPORT / AS rar",
            E_UNSUPPORTED,
            1,
            13,
            "export format 'rar' is not supported",
        );
        parse_err(
            "EXPORT / AS",
            E_SYNTAX,
            1,
            12,
            "expected export format after AS",
        );
        parse_err("EXPORT", E_SYNTAX, 1, 7, "expected path");
        parse_err("EXPORT / AS directory more", E_SYNTAX, 1, 23, "");
        parse_err("EXPORT / AS wbn BASE", E_SYNTAX, 1, 21, "expected base URL");
    }

    #[test]
    fn unsupported_and_unknown() {
        parse_err(
            "RUN npm build",
            E_UNSUPPORTED,
            1,
            1,
            "never runs shell commands",
        );
        for kw in ["REMOVE", "GENERATE", "ASSERT", "META", "exec"] {
            parse_err(
                &format!("  {kw} x"),
                E_UNSUPPORTED,
                1,
                3,
                "not supported yet",
            );
        }
        parse_err("  exec x", E_UNSUPPORTED, 1, 3, "EXEC is not supported yet");
        parse_err(
            "\nWORKDIR /x",
            E_UNKNOWN_INSTRUCTION,
            2,
            1,
            "unknown instruction 'WORKDIR'",
        );
    }

    #[test]
    fn from_flag() {
        let Instruction::Copy { from, .. } = one("COPY --from=build /out/ /") else {
            panic!()
        };
        let f = from.unwrap();
        assert_eq!((f.name.as_str(), f.span.column), ("build", 6));
        assert!(
            matches!(one("RENDER --from=build --escape=html /t.html TO /x"), Instruction::Render { from: Some(ref f), .. } if f.name == "build")
        );
        assert!(matches!(
            one("COPY a b"),
            Instruction::Copy { from: None, .. }
        ));
        parse_err("COPY --from a b", E_SYNTAX, 1, 6, "requires a value");
        parse_err(
            "COPY --from=a-b x y",
            E_SYNTAX,
            1,
            6,
            "--from needs a stage name, got 'a-b'",
        );
        parse_err("COPY --from=a --from=b x y", E_SYNTAX, 1, 15, "twice");
    }

    #[test]
    fn plugin_tool_transform_forms() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(matches!(
            one(&format!("PLUGIN p FROM ./p.mjs INTEGRITY {digest}")),
            Instruction::Plugin {
                integrity: Some(_),
                ..
            }
        ));
        parse_err(
            "PLUGIN p FROM ./p.mjs EXTRA",
            E_SYNTAX,
            1,
            23,
            "unexpected 'EXTRA' (expected INTEGRITY)",
        );
        parse_err("PLUGIN p ./p.mjs", E_SYNTAX, 1, 10, "expected FROM");
        assert!(
            matches!(one("TOOL ffmpeg"), Instruction::Tool { ref name, .. } if name == "ffmpeg")
        );
        parse_err("TOOL a b", E_SYNTAX, 1, 8, "unexpected 'b'");

        let Instruction::Transform {
            source,
            plugin,
            plugin_span,
            options,
            each,
            args,
            ..
        } = one("TRANSFORM ./a.txt WITH up TO /a.txt OPTIONS x=1 y=\"two words\"")
        else {
            panic!()
        };
        assert!(
            source.is_some()
                && plugin == "up"
                && plugin_span.column == 24
                && each.is_none()
                && args.is_none()
        );
        assert_eq!(
            options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            ["x", "y"]
        );
        assert_eq!(lit(&options[1].value), "two words");

        let Instruction::Transform {
            source,
            each,
            options,
            ..
        } = one(
            "TRANSFORM WITH up TO /c/${post.slug}.svg OPTIONS t=${post.title} FOR post IN posts",
        )
        else {
            panic!()
        };
        assert!(source.is_none() && each.unwrap().as_name == "post" && options.len() == 1);

        let Instruction::Transform { args, options, .. } =
            one("TRANSFORM ./a WITH t TO /a OPTIONS w=3 ARGS -f \"lavfi x\" --w={option.w} {input} {output}")
        else {
            panic!()
        };
        assert_eq!(args.unwrap().len(), 5);
        assert_eq!(options.len(), 1);
        let Instruction::Transform { args, each, .. } =
            one("TRANSFORM ./a WITH t TO /a FOR x IN xs ARGS a b")
        else {
            panic!()
        };
        assert!(each.is_some() && args.unwrap().len() == 2);

        // inside OPTIONS a second OPTIONS reads as a key (as in TypeScript)
        parse_err(
            "TRANSFORM ./a WITH p TO /a OPTIONS a=1 OPTIONS b=2",
            E_SYNTAX,
            1,
            40,
            "expected OPTIONS=value",
        );
        parse_err(
            "TRANSFORM ./a WITH p TO /a OPTIONS a=1 FOR x IN y OPTIONS b=2",
            E_SYNTAX,
            1,
            51,
            "OPTIONS given twice",
        );
        parse_err(
            "TRANSFORM ./a WITH p TO /a OPTIONS a=1 a=2",
            E_SYNTAX,
            1,
            40,
            "option 'a' given twice",
        );
        parse_err(
            "TRANSFORM ./a WITH p TO /a OPTIONS a",
            E_SYNTAX,
            1,
            36,
            "expected a=value",
        );
        parse_err(
            "TRANSFORM ./a WITH p TO /a FOR x IN y FOR z IN w",
            E_SYNTAX,
            1,
            39,
            "FOR given twice",
        );
        parse_err(
            "TRANSFORM ./a WITH p TO /a EXTRA",
            E_SYNTAX,
            1,
            28,
            "unexpected 'EXTRA' (expected OPTIONS, FOR or ARGS)",
        );
        parse_err("TRANSFORM ./a p TO /a", E_SYNTAX, 1, 15, "expected WITH");
        parse_err(
            "TRANSFORM --from=s WITH p TO /a",
            E_SYNTAX,
            1,
            11,
            "--from needs a source path",
        );
        parse_err(
            "TRANSFORM ./a WITH 9p TO /a",
            E_SYNTAX,
            1,
            20,
            "expected plugin name",
        );
    }

    #[test]
    fn site_forms() {
        assert!(matches!(
            one("SITEMAP --overwrite BASE https://x.example TO /s.xml"),
            Instruction::Sitemap {
                overwrite: true,
                dest: Some(_),
                ..
            }
        ));
        let Instruction::Feed {
            list,
            dest,
            title,
            list_span,
            ..
        } = one(
            "feed posts base https://x link /p/${item.slug}.html to /atom.xml title \"My blog\"",
        )
        else {
            panic!()
        };
        assert!(list == "posts" && dest.is_some() && title.is_some() && list_span.column == 6);
        assert!(matches!(
            one("REDIRECT /old /new STATUS 308"),
            Instruction::Redirect { status: 308, .. }
        ));
        assert!(matches!(
            one("REDIRECT /old https://x.example/new"),
            Instruction::Redirect { status: 301, .. }
        ));
        let Instruction::Header { name, value, .. } =
            one("HEADER /assets/* Cache-Control: public, max-age=60")
        else {
            panic!()
        };
        assert_eq!((lit(&name).as_str(), value.len()), ("Cache-Control", 2));
        let Instruction::Header { name, .. } = one("HEADER /* X-${N}: 1") else {
            panic!()
        };
        assert_eq!(parts(&name).len(), 2);
        parse_err("SITEMAP TO /a.xml", E_SYNTAX, 1, 9, "expected BASE");
        parse_err("SITEMAP BASE", E_SYNTAX, 1, 13, "expected base URL");
        parse_err(
            "SITEMAP BASE https://x extra",
            E_SYNTAX,
            1,
            24,
            "unexpected 'extra'",
        );
        parse_err(
            "SITEMAP --from=a BASE https://x",
            E_SYNTAX,
            1,
            9,
            "unknown flag --from",
        );
        parse_err(
            "FEED posts BASE https://x",
            E_SYNTAX,
            1,
            26,
            "expected LINK",
        );
        parse_err(
            "FEED posts BASE https://x LINK /a TITLE t TO /f.xml",
            E_SYNTAX,
            1,
            43,
            "unexpected 'TO'",
        );
        parse_err(
            "FEED 1x BASE https://x LINK /a",
            E_SYNTAX,
            1,
            6,
            "expected import name",
        );
        parse_err(
            "REDIRECT /old /new STATUS 200",
            E_SYNTAX,
            1,
            27,
            "unknown redirect status '200'",
        );
        parse_err(
            "REDIRECT /old /new STATUS",
            E_SYNTAX,
            1,
            26,
            "expected a status after STATUS",
        );
        parse_err("REDIRECT /old", E_SYNTAX, 1, 14, "expected redirect target");
        parse_err(
            "HEADER /* X-Frame-Options DENY",
            E_SYNTAX,
            1,
            11,
            "expected 'Name:'",
        );
        parse_err(
            "HEADER /* : DENY",
            E_SYNTAX,
            1,
            11,
            "expected a header name before the colon",
        );
        parse_err(
            "HEADER /* X-Frame-Options:",
            E_SYNTAX,
            1,
            27,
            "expected header value",
        );
    }

    #[test]
    fn heredoc_bodies() {
        let src =
            "FROM scratch\nCOPY <<EOF /a.txt\n# not a comment\n\n  indented \\\nEOF\nEXPORT /";
        let ins = parse_sitefile(src, "Sitefile").unwrap();
        let Instruction::Copy {
            source: Source::Inline(b),
            ..
        } = &ins[1]
        else {
            panic!()
        };
        assert_eq!(b.delimiter, "EOF");
        assert_eq!(b.text, "# not a comment\n\n  indented \\\n");
        assert_eq!(b.body_line, 3);
        assert_eq!((b.span.line, b.span.column), (2, 6));

        let inline_text = |src: &str| -> String {
            let ins = parse_sitefile(src, "Sitefile").unwrap();
            ins.into_iter()
                .find_map(|i| match i {
                    Instruction::Copy {
                        source: Source::Inline(b),
                        ..
                    } => Some(b.text),
                    Instruction::Render {
                        template: Source::Inline(b),
                        ..
                    } => Some(b.text),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            inline_text("FROM scratch\nCOPY <<-EOF /a\n\t\tone\n\ttwo\n\tEOF\nEXPORT /\n"),
            "one\ntwo\n"
        );
        assert_eq!(inline_text("FROM scratch\nCOPY <<E /a\nE\nEXPORT /"), "");
        assert_eq!(
            inline_text("FROM scratch\r\nCOPY <<E /a\r\nx\r\nE\r\nEXPORT /\r\n"),
            "x\n"
        );

        let ins = parse_sitefile(
            "FROM scratch\nRENDER \\\n  <<T \\\n  TO /i.html\nhello\nT\nEXPORT /",
            "Sitefile",
        )
        .unwrap();
        let Instruction::Render {
            template: Source::Inline(b),
            ..
        } = &ins[1]
        else {
            panic!()
        };
        assert_eq!(b.body_line, 5);

        let ins = parse_sitefile(
            "FROM scratch\nTRANSFORM <<T WITH p TO /x\nbody\nT\nEXPORT /",
            "Sitefile",
        )
        .unwrap();
        assert!(matches!(
            &ins[1],
            Instruction::Transform {
                source: Some(Source::Inline(_)),
                ..
            }
        ));

        // quoted or mid-word << is not a heredoc; nor is << in other instructions
        assert_eq!(
            parse_sitefile(
                "FROM scratch\nCOPY \"<<EOF\" /a\nCOPY ./x<<y /b\nEXPORT /",
                "Sitefile"
            )
            .unwrap()
            .len(),
            4
        );
        assert_eq!(
            parse_sitefile("FROM scratch\nLET X = <<EOF\nEXPORT /", "Sitefile")
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn heredoc_errors() {
        parse_err(
            "FROM scratch\nCOPY <<EOF /a\nno end\n",
            E_SYNTAX,
            2,
            6,
            "inline body is missing its closing 'EOF' line",
        );
        parse_err(
            "FROM scratch\nCOPY <<A <<B\nA\nEXPORT /",
            E_SYNTAX,
            2,
            10,
            "only one inline (<<) body",
        );
        parse_err(
            "FROM scratch\nCOPY ./a <<EOF\nx\nEOF\nEXPORT /",
            E_SYNTAX,
            2,
            10,
            "an inline <<EOF body can only be the COPY source",
        );
        parse_err(
            "FROM scratch AS s\nFROM scratch\nCOPY --from=s <<EOF /a\nx\nEOF\nEXPORT /",
            E_SYNTAX,
            3,
            6,
            "--from cannot be combined",
        );
    }

    #[test]
    fn keyword_tables_match_the_parser() {
        for kw in INSTRUCTIONS {
            if let Err(e) = parse_sitefile(kw, "Sitefile") {
                assert_ne!(e.code, ErrorCode::E_UNKNOWN_INSTRUCTION, "{kw}");
            }
        }
        assert_eq!(
            parse_sitefile("FOO", "Sitefile").unwrap_err().code,
            ErrorCode::E_UNKNOWN_INSTRUCTION
        );
        for k in ImportKind::ALL {
            assert_eq!(ImportKind::parse(&k.as_str().to_uppercase()), Some(k));
        }
        for f in ExportFormat::ALL {
            assert_eq!(ExportFormat::parse(f.as_str()), Some(f));
        }
        assert_eq!(ExportFormat::parse("TAR.GZ"), Some(ExportFormat::Tgz));
        for (kw, flags) in [
            ("COPY", COPY_FLAGS),
            ("RENDER", RENDER_FLAGS),
            ("TRANSFORM", TRANSFORM_FLAGS),
            ("SITEMAP", SITE_FLAGS),
            ("FEED", SITE_FLAGS),
            ("REDIRECT", SITE_FLAGS),
            ("HEADER", SITE_FLAGS),
            ("FINGERPRINT", SITE_FLAGS),
            ("CHECKLINKS", CHECKLINKS_FLAGS),
        ] {
            assert_eq!(flags_of(kw), flags);
            for (name, shape) in flags {
                let flag = if *shape == Shape::Bare {
                    format!("--{name}")
                } else {
                    format!("--{name}=x")
                };
                // The flag is accepted (the line may still fail later, for a missing operand).
                if let Err(e) = parse_sitefile(&format!("{kw} {flag}"), "Sitefile") {
                    assert!(!e.detail.contains("unknown flag"), "{kw} {flag}: {e}");
                }
            }
        }
        for clause in IMPORT_CLAUSES {
            let e = parse_sitefile(&format!("IMPORT a FROM ./a {clause}"), "Sitefile").unwrap_err();
            assert!(!e.detail.contains("unexpected"), "{clause}: {e}");
        }
    }

    #[test]
    fn import_secret_clauses() {
        let Instruction::Import {
            secrets, as_kind, ..
        } = one(
            "IMPORT d FROM https://e.x/d SECRET k AS HEADER X-Api-Key AS json secret t as bearer",
        )
        else {
            panic!()
        };
        assert_eq!(as_kind, Some(ImportKind::Json));
        let got: Vec<_> = secrets
            .iter()
            .map(|s| (s.id.as_str(), s.header.as_str(), s.bearer, s.span.column))
            .collect();
        assert_eq!(
            got,
            [
                ("k", "X-Api-Key", false, 29),
                ("t", "Authorization", true, 66)
            ]
        );
        let err = |src: &str| parse_sitefile(src, "Sitefile").unwrap_err().detail;
        assert_eq!(err("IMPORT d FROM https://e.x/d SECRET k"), "expected AS");
        assert_eq!(
            err("IMPORT d FROM https://e.x/d SECRET k AS COOKIE"),
            "expected HEADER <name> or BEARER"
        );
        assert_eq!(
            err("IMPORT d FROM https://e.x/d SECRET k AS HEADER"),
            "expected a header name"
        );
        assert_eq!(
            err("IMPORT d FROM https://e.x/d SECRET k AS HEADER X:y"),
            "invalid header name 'X:y'"
        );
        assert_eq!(
            err("IMPORT d FROM https://e.x/d SECRET k AS HEADER Host"),
            "header 'Host' is set by sitefile and cannot carry a secret"
        );
        assert_eq!(
            err("IMPORT d FROM https://e.x/d SECRET a AS BEARER SECRET b AS HEADER authorization"),
            "header 'authorization' is set by two SECRET clauses"
        );
        assert_eq!(
            err("IMPORT d FROM https://e.x/d BOGUS"),
            "unexpected 'BOGUS' (expected AS, INTEGRITY, SORT BY, SCHEMA, DRAFTS, SECRET or ARG)"
        );
    }

    #[test]
    fn parse_all_reports_every_bad_line() {
        let errs = parse_sitefile_all("COPY\nFROM scratch\nBOGUS x\nEXPORT / extra", "Sitefile")
            .unwrap_err();
        let lines: Vec<u32> = errs.iter().map(|e| e.span.as_ref().unwrap().line).collect();
        assert_eq!(lines, [1, 3, 4]);
        assert_eq!(
            parse_sitefile_all("FROM scratch\nEXPORT /", "Sitefile")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            parse_sitefile_all("COPY <<A /a\nx", "Sitefile")
                .unwrap_err()
                .len(),
            1
        );
    }
}
