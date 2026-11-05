//! Sitefile syntax: logical lines to instructions (src/language/parser.ts).
//! Semantic rules live in `validate.rs`.

use crate::digest::is_integrity;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::{
    is_ident, literal_text, EscapeMode, ExportFormat, ForEach, FromBase, ImportKind, Instruction, Operand,
    SortBy, Source, StageRef, TransformOption, ValueType,
};
use crate::language::scanner::{logical_lines, Cursor};

const NOT_YET: [&str; 5] = ["REMOVE", "GENERATE", "ASSERT", "META", "EXEC"];

/// Parse Sitefile source into instructions (syntax only; see `validate.rs` for semantics).
pub fn parse_sitefile(source: &str, file: &str) -> Result<Vec<Instruction>> {
    logical_lines(source, file)?.iter().map(|line| parse_instruction(&mut Cursor::new(line, file))).collect()
}

fn parse_instruction(c: &mut Cursor) -> Result<Instruction> {
    let span = c.span();
    let word = c.word();
    let kw = word.to_uppercase();
    match kw.as_str() {
        "FROM" => parse_from(c, span),
        "ARG" => parse_binding(c, span, true),
        "LET" => parse_binding(c, span, false),
        "IMPORT" => parse_import(c, span),
        "COPY" => parse_copy(c, span),
        "RENDER" => parse_render(c, span),
        "EXPORT" => parse_export(c, span),
        "PLUGIN" => parse_plugin(c, span),
        "TOOL" => parse_tool(c, span),
        "TRANSFORM" => parse_transform(c, span),
        "RUN" => Err(SitefileError::at(E_UNSUPPORTED, "RUN is not supported: Sitefile never runs shell commands", &span)),
        k if NOT_YET.contains(&k) => Err(SitefileError::at(E_UNSUPPORTED, format!("{k} is not supported yet"), &span)),
        _ => Err(SitefileError::at(E_UNKNOWN_INSTRUCTION, format!("unknown instruction '{word}'"), &span)),
    }
}

fn parse_from(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let base = if c.keyword("scratch") { FromBase::Scratch } else { FromBase::Operand(c.operand("base")?) };
    let alias = if c.keyword("AS") { Some(c.ident("stage name", "")?) } else { None };
    c.expect_end()?;
    Ok(Instruction::From { span, base, alias })
}

fn parse_binding(c: &mut Cursor, span: SourceSpan, is_arg: bool) -> Result<Instruction> {
    let kind = if is_arg { "ARG" } else { "LET" };
    let name = c.ident(&format!("{kind} name"), ":=")?;
    let mut ty = ValueType::String;
    if c.eat(':') {
        let at = c.span();
        let t = c.ident("type", "=")?.to_lowercase();
        ty = match t.as_str() {
            "string" => ValueType::String,
            "number" => ValueType::Number,
            "boolean" => ValueType::Boolean,
            _ => {
                return Err(SitefileError::at(
                    E_SYNTAX,
                    format!("unknown type '{t}' (expected string, number or boolean)"),
                    &at,
                ))
            }
        };
    }
    let value = if c.eat('=') { Some(c.operand("value")?) } else { None };
    c.expect_end()?;
    if is_arg {
        return Ok(Instruction::Arg { span, name, ty, default_value: value });
    }
    match value {
        Some(value) => Ok(Instruction::Let { span, name, ty, value }),
        None => Err(SitefileError::at(E_SYNTAX, format!("LET {name} requires a value (LET {name} = ...)"), &span)),
    }
}

/// `INTEGRITY sha256:…` after the keyword: a literal digest.
fn integrity_digest(c: &mut Cursor) -> Result<String> {
    let op = c.operand("integrity digest")?;
    match literal_text(&op) {
        Some(text) if is_integrity(&text) => Ok(text),
        _ => Err(SitefileError::at(E_SYNTAX, "INTEGRITY must be sha256:<64 lowercase hex digits>", &op.span)),
    }
}

fn parse_import(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let name = c.ident("import name", "")?;
    c.expect_keyword("FROM")?;
    let uri = c.operand("URI")?;
    let mut as_kind: Option<ImportKind> = None;
    let mut integrity: Option<String> = None;
    let mut sort: Option<SortBy> = None;
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
                    format!("unknown import kind '{k}' (expected file, text, json, yaml, csv, markdown, wbn or tree)"),
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
            sort = Some(SortBy { key, descending, span: at });
        } else {
            let w = c.word();
            return Err(SitefileError::at(E_SYNTAX, format!("unexpected '{w}' (expected AS, INTEGRITY or SORT BY)"), &at));
        }
    }
    Ok(Instruction::Import { span, name, uri, as_kind, integrity, sort })
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Bare,
    Value,
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
}

/// Read leading `--flags` (value `""` for a bare flag).
fn parse_flags(c: &mut Cursor, allowed: &[(&'static str, Shape)]) -> Result<Flags> {
    let mut out = Flags(Vec::new());
    while let Some(f) = c.flag() {
        let Some(&(name, shape)) = allowed.iter().find(|(n, _)| *n == f.name) else {
            return Err(SitefileError::at(E_SYNTAX, format!("unknown flag --{}", f.name), &f.span));
        };
        if out.has(name) {
            return Err(SitefileError::at(E_SYNTAX, format!("--{name} given twice"), &f.span));
        }
        if shape == Shape::Bare && f.value.is_some() {
            return Err(SitefileError::at(E_SYNTAX, format!("--{name} takes no value"), &f.span));
        }
        if shape == Shape::Value && f.value.as_deref().unwrap_or("").is_empty() {
            return Err(SitefileError::at(E_SYNTAX, format!("--{name} requires a value (--{name}=...)"), &f.span));
        }
        out.0.push(ParsedFlag { name, value: f.value.unwrap_or_default(), span: f.span });
    }
    Ok(out)
}

fn stage_ref(flags: &Flags) -> Result<Option<StageRef>> {
    let Some(f) = flags.get("from") else { return Ok(None) };
    if !is_ident(&f.value) {
        return Err(SitefileError::at(E_SYNTAX, format!("--from needs a stage name, got '{}'", f.value), &f.span));
    }
    Ok(Some(StageRef { name: f.value.clone(), span: f.span.clone() }))
}

fn no_inline_from(source: &Source, flags: &Flags) -> Result<()> {
    match (flags.get("from"), source) {
        (Some(from), Source::Inline(_)) => {
            Err(SitefileError::at(E_SYNTAX, "--from cannot be combined with an inline (<<) body", &from.span))
        }
        _ => Ok(()),
    }
}

fn inline_or_operand(c: &mut Cursor, what: &str) -> Result<Source> {
    match c.inline() {
        Some(body) => Ok(Source::Inline(body)),
        None => Ok(Source::Operand(c.operand(what)?)),
    }
}

fn parse_copy(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, &[("overwrite", Shape::Bare), ("from", Shape::Value)])?;
    let source = inline_or_operand(c, "source")?;
    let dest = c.operand("destination")?;
    c.expect_end()?;
    c.expect_inline_used("the COPY source")?;
    no_inline_from(&source, &flags)?;
    let from = stage_ref(&flags)?;
    Ok(Instruction::Copy { span, source, dest, from, overwrite: flags.has("overwrite") })
}

fn parse_for(c: &mut Cursor, span: SourceSpan) -> Result<ForEach> {
    let as_name = c.ident("loop variable", "")?;
    c.expect_keyword("IN")?;
    let list = c.ident("list name", "")?;
    Ok(ForEach { as_name, list, span })
}

fn parse_render(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let flags = parse_flags(c, &[("escape", Shape::Value), ("overwrite", Shape::Bare), ("from", Shape::Value)])?;
    let escape = match flags.get("escape") {
        None => EscapeMode::None,
        Some(f) => match f.value.to_lowercase().as_str() {
            "none" => EscapeMode::None,
            "html" => EscapeMode::Html,
            _ => {
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
    let each = if c.keyword("FOR") { Some(parse_for(c, for_at)?) } else { None };
    c.expect_end()?;
    c.expect_inline_used("the RENDER template")?;
    no_inline_from(&template, &flags)?;
    let from = stage_ref(&flags)?;
    Ok(Instruction::Render { span, template, dest, from, escape, each, overwrite: flags.has("overwrite") })
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
        return Err(SitefileError::at(E_SYNTAX, format!("unexpected '{w}' (expected INTEGRITY)"), &at));
    }
    c.expect_end()?;
    Ok(Instruction::Plugin { span, name, uri, integrity })
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
    let flags = parse_flags(c, &[("overwrite", Shape::Bare), ("from", Shape::Value)])?;
    // The source is optional (only with FOR): `TRANSFORM WITH og TO … FOR post IN posts`.
    let source = if c.keyword("WITH") { None } else { Some(inline_or_operand(c, "source")?) };
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
                    return Err(SitefileError::at(E_SYNTAX, format!("expected {key}=value"), &key_at));
                }
                if options.iter().any(|o| o.key == key) {
                    return Err(SitefileError::at(E_SYNTAX, format!("option '{key}' given twice"), &key_at));
                }
                let value = c.operand(&format!("value of {key}"))?;
                options.push(TransformOption { key, value, span: key_at });
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
            return Err(SitefileError::at(E_SYNTAX, "--from needs a source path", &f.span));
        }
    }
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
        overwrite: flags.has("overwrite"),
    })
}

fn parse_export(c: &mut Cursor, span: SourceSpan) -> Result<Instruction> {
    let path = c.operand("path")?;
    let mut format = ExportFormat::Directory;
    if c.keyword("AS") {
        let at = c.span();
        let name = c.word();
        if name.is_empty() {
            return Err(SitefileError::at(E_SYNTAX, "expected export format after AS", &at));
        }
        format = ExportFormat::parse(&name).ok_or_else(|| {
            SitefileError::at(
                E_UNSUPPORTED,
                format!("export format '{name}' is not supported (expected directory, zip, tar, tgz, wbn or oci)"),
                &at,
            )
        })?;
    }
    let base = if c.keyword("BASE") { Some(c.operand("base URL")?) } else { None };
    let at = if c.keyword("AT") { Some(c.operand("image root path")?) } else { None };
    c.expect_end()?;
    Ok(Instruction::Export { span, path, format, base, at })
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

    fn copy_op(src: &str, dest: bool) -> Operand {
        match one(src) {
            Instruction::Copy { source: Source::Operand(s), dest: d, .. } => {
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
                TemplatePart::Var { name, span, .. } => format!("${{{name}}}@{}:{}", span.line, span.column),
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
        let ins = parse_sitefile("COPY \\\n# a comment\n\n  ./a \\\n  # another\n  /b\nEXPORT /", "Sitefile").unwrap();
        assert_eq!(ins.len(), 2);
        let Instruction::Copy { source: Source::Operand(s), dest, .. } = &ins[0] else { panic!() };
        assert_eq!(lit(s), "./a");
        assert_eq!((dest.span.line, dest.span.column), (6, 3));
    }

    #[test]
    fn trailing_backslashes() {
        let ins = parse_sitefile("COPY ./a /b\\\\\nEXPORT /", "Sitefile").unwrap();
        assert_eq!(ins.len(), 2);
        let Instruction::Copy { dest, .. } = &ins[0] else { panic!() };
        assert_eq!(lit(dest), "/b\\");
        let ins = parse_sitefile("COPY ./a\\\\\\\n /b", "Sitefile").unwrap();
        assert_eq!(ins.len(), 1);
        let Instruction::Copy { source: Source::Operand(s), .. } = &ins[0] else { panic!() };
        assert_eq!(lit(s), "./a\\");
    }

    #[test]
    fn hash_mid_line_and_crlf() {
        assert_eq!(lit(&copy_op("COPY ./a#b /c#d", true)), "/c#d");
        assert_eq!(parse_sitefile("FROM scratch\r\nEXPORT /\r\n", "Sitefile").unwrap().len(), 2);
    }

    #[test]
    fn error_spans_across_continuations() {
        parse_err("COPY \\\n  ./a \\\n  /b extra", E_SYNTAX, 3, 6, "");
    }

    #[test]
    fn quoted_operands() {
        assert_eq!(parts(&copy_op("COPY \"a b\" x", false)), expect(&[P::T("a b")]));
        assert_eq!(parts(&copy_op("COPY 'a b' x", false)), expect(&[P::T("a b")]));
        assert_eq!(parts(&copy_op("COPY a\\ b x", false)), expect(&[P::T("a b")]));
        assert!(parts(&copy_op("COPY \"\" x", false)).is_empty());
    }

    #[test]
    fn escapes() {
        assert_eq!(parts(&copy_op("COPY \"a\\\"b\\\\c\\n\\t\\$\\q\" x", false)), ["a\"b\\c\n\t$q"]);
        assert_eq!(parts(&copy_op("COPY a\\$\\{b\\n x", false)), ["a${bn"]);
        assert_eq!(parts(&copy_op("COPY 'a\\n${X}\\' x", false)), ["a\\n${X}\\"]);
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
            TemplatePart::Var { name, path, .. } => assert_eq!((name.as_str(), path.clone()), ("a", vec!["b-c".to_string(), "d_1".into()])),
            _ => panic!(),
        }
    }

    #[test]
    fn interpolation_errors() {
        parse_err("COPY ${a.} x", E_SYNTAX, 1, 6, "expected a key after '.' in ${a…}");
        parse_err("COPY ${} x", E_SYNTAX, 1, 6, "expected a name after ${");
        parse_err("COPY ${1A} x", E_SYNTAX, 1, 6, "");
        parse_err("COPY ab${X x", E_SYNTAX, 1, 8, "unterminated ${...}: expected }");
        parse_err("COPY \"${X\" x", E_SYNTAX, 1, 7, "");
    }

    #[test]
    fn unterminated_quotes() {
        parse_err("COPY \"abc x", E_SYNTAX, 1, 6, "unterminated");
        parse_err("COPY x 'abc", E_SYNTAX, 1, 8, "unterminated");
        parse_err("COPY \"abc\\", E_SYNTAX, 1, 6, "unterminated");
        parse_err("COPY \"a\"b x", E_SYNTAX, 1, 9, "unexpected character after closing quote");
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
            Instruction::From { span: SourceSpan::new("Sitefile", 1, 1), base: FromBase::Scratch, alias: Some("base".into()) }
        );
        assert!(matches!(ins[3], Instruction::Import { as_kind: Some(ImportKind::Json), .. }));
    }

    #[test]
    fn instruction_span() {
        let ins = parse_sitefile("\n\n   COPY a b", "x/Sitefile").unwrap();
        assert_eq!(ins[0].span(), &SourceSpan::new("x/Sitefile", 3, 4));
    }

    #[test]
    fn from_forms() {
        assert_eq!(one("FROM scratch"), Instruction::From { span: SourceSpan::new("Sitefile", 1, 1), base: FromBase::Scratch, alias: None });
        assert!(matches!(one("FROM \"scratch\""), Instruction::From { base: FromBase::Operand(ref o), .. } if lit(o) == "scratch"));
        assert!(matches!(one("FROM ./base AS site"), Instruction::From { base: FromBase::Operand(_), alias: Some(ref a), .. } if a == "site"));
        parse_err("FROM", E_SYNTAX, 1, 5, "expected base");
        parse_err("FROM scratch AS", E_SYNTAX, 1, 16, "expected stage name");
        parse_err("FROM scratch extra", E_SYNTAX, 1, 14, "unexpected 'extra'");
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
            let Instruction::Arg { name, ty, default_value, .. } = one(src) else { panic!("{src}") };
            assert_eq!(name, n, "{src}");
            assert_eq!(ty, t, "{src}");
            assert_eq!(default_value.as_ref().map(lit).as_deref(), v, "{src}");
        }
        parse_err("ARG", E_SYNTAX, 1, 4, "expected ARG name");
        parse_err("ARG 1X", E_SYNTAX, 1, 5, "");
        parse_err("ARG X-Y", E_SYNTAX, 1, 6, "invalid ARG name: unexpected '-'");
        parse_err("ARG X: date", E_SYNTAX, 1, 8, "unknown type 'date'");
        parse_err("ARG X:", E_SYNTAX, 1, 7, "");
        parse_err("ARG X =", E_SYNTAX, 1, 8, "expected value");
        parse_err("ARG X 5", E_SYNTAX, 1, 7, "");
        parse_err("ARG X = a b", E_SYNTAX, 1, 11, "");
    }

    #[test]
    fn let_forms() {
        assert!(matches!(one("LET G: boolean = true"), Instruction::Let { ref name, ty: ValueType::Boolean, ref value, .. } if name == "G" && lit(value) == "true"));
        assert!(matches!(one("LET M=\"hi ${X}\""), Instruction::Let { ty: ValueType::String, .. }));
        parse_err("LET X", E_SYNTAX, 1, 1, "LET X requires a value (LET X = ...)");
        parse_err("LET X: number", E_SYNTAX, 1, 1, "requires a value");
    }

    #[test]
    fn import_forms() {
        let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
        let Instruction::Import { name, uri, as_kind, integrity, .. } = one("IMPORT logo FROM https://example.com/logo.png") else { panic!() };
        assert_eq!((name.as_str(), lit(&uri).as_str()), ("logo", "https://example.com/logo.png"));
        assert!(as_kind.is_none() && integrity.is_none());
        let Instruction::Import { as_kind, integrity, .. } = one(&format!("IMPORT d FROM ./data.json AS json INTEGRITY {digest}")) else { panic!() };
        assert_eq!((as_kind, integrity.as_deref()), (Some(ImportKind::Json), Some(digest.as_str())));
        let Instruction::Import { as_kind, integrity, uri, .. } = one(&format!("IMPORT d FROM \"./x y\" INTEGRITY \"{digest}\" AS tree")) else { panic!() };
        assert_eq!((as_kind, integrity.as_deref(), lit(&uri).as_str()), (Some(ImportKind::Tree), Some(digest.as_str()), "./x y"));
        parse_err("IMPORT d ./x", E_SYNTAX, 1, 10, "expected FROM");
        parse_err("IMPORT FROM ./x", E_SYNTAX, 1, 13, "FROM");
        parse_err("IMPORT d FROM", E_SYNTAX, 1, 14, "expected URI");
        parse_err("IMPORT d FROM ./x AS toml", E_SYNTAX, 1, 22, "unknown import kind 'toml'");
        parse_err("IMPORT d FROM ./x AS text AS json", E_SYNTAX, 1, 27, "AS given twice");
        parse_err(&format!("IMPORT d FROM ./x INTEGRITY {digest} INTEGRITY {digest}"), E_SYNTAX, 1, 101, "twice");
        parse_err("IMPORT d FROM ./x INTEGRITY sha256:abc", E_SYNTAX, 1, 29, "sha256");
        parse_err(&format!("IMPORT d FROM ./x INTEGRITY {}", digest.to_uppercase()), E_SYNTAX, 1, 29, "");
        parse_err("IMPORT d FROM ./x INTEGRITY ${X}", E_SYNTAX, 1, 29, "");
        parse_err("IMPORT d FROM ./x EXTRA", E_SYNTAX, 1, 19, "unexpected 'EXTRA' (expected AS, INTEGRITY or SORT BY)");
        let Instruction::Import { sort, .. } = one("IMPORT p FROM ./p AS markdown SORT BY date DESC") else { panic!() };
        let s = sort.unwrap();
        assert!(s.key == "date" && s.descending && s.span.column == 31);
        let Instruction::Import { sort, .. } = one("IMPORT p FROM ./p SORT BY date ASC AS csv") else { panic!() };
        assert!(!sort.unwrap().descending);
        parse_err("IMPORT p FROM ./p SORT BY a SORT BY b", E_SYNTAX, 1, 29, "SORT given twice");
        parse_err("IMPORT p FROM ./p SORT date", E_SYNTAX, 1, 24, "expected BY");
    }

    #[test]
    fn copy_forms() {
        let a = one("COPY ./public/ /");
        let Instruction::Copy { source: Source::Operand(s), dest, overwrite, span, .. } = &a else { panic!() };
        assert!(!overwrite && lit(s) == "./public/" && lit(dest) == "/");
        assert_eq!((span.column, dest.span.column), (1, 16));
        assert!(matches!(one("COPY --overwrite site: /"), Instruction::Copy { overwrite: true, source: Source::Operand(ref s), .. } if lit(s) == "site:"));
        assert!(matches!(one("COPY --from=build /a /b"), Instruction::Copy { .. }));
        parse_err("COPY --chown=1 /a /b", E_SYNTAX, 1, 6, "unknown flag --chown");
        parse_err("COPY --overwrite=yes /a /b", E_SYNTAX, 1, 6, "--overwrite takes no value");
        parse_err("COPY --overwrite --overwrite /a /b", E_SYNTAX, 1, 18, "--overwrite given twice");
        parse_err("COPY ./a", E_SYNTAX, 1, 9, "expected destination");
        parse_err("COPY", E_SYNTAX, 1, 5, "expected source");
        parse_err("COPY a b c", E_SYNTAX, 1, 10, "unexpected 'c'");
    }

    #[test]
    fn render_forms() {
        assert!(matches!(one("RENDER ./t.html TO /index.html"), Instruction::Render { escape: EscapeMode::None, overwrite: false, .. }));
        assert!(matches!(
            one("RENDER --overwrite --escape=html \"./t.html\" to \"/i.html\""),
            Instruction::Render { escape: EscapeMode::Html, overwrite: true, ref dest, .. } if lit(dest) == "/i.html"
        ));
        assert!(matches!(one("RENDER --escape=none t TO /x"), Instruction::Render { .. }));
        assert!(matches!(one("RENDER --escape=HTML t TO /x"), Instruction::Render { escape: EscapeMode::Html, .. }));
        parse_err("RENDER --overwrite --escape=js t TO /x", E_SYNTAX, 1, 20, "--escape must be none or html, got 'js'");
        parse_err("RENDER --escape t TO /x", E_SYNTAX, 1, 8, "--escape requires a value (--escape=...)");
        parse_err("RENDER t /x", E_SYNTAX, 1, 10, "expected TO");
        parse_err("RENDER t TO", E_SYNTAX, 1, 12, "expected destination");
        assert!(matches!(one("RENDER --from=a t TO /x"), Instruction::Render { from: Some(_), .. }));
        let Instruction::Render { each, .. } = one("RENDER ./t TO /p/${post.slug}.html FOR post IN posts") else { panic!() };
        let e = each.unwrap();
        assert_eq!((e.as_name.as_str(), e.list.as_str(), e.span.column), ("post", "posts", 36));
        parse_err("RENDER t TO /x FOR a posts", E_SYNTAX, 1, 22, "expected IN");
    }

    #[test]
    fn export_forms() {
        assert!(matches!(one("EXPORT /"), Instruction::Export { format: ExportFormat::Directory, ref path, .. } if lit(path) == "/"));
        assert!(matches!(one("EXPORT /site AS directory"), Instruction::Export { .. }));
        assert!(matches!(one("EXPORT / AS zip"), Instruction::Export { format: ExportFormat::Zip, .. }));
        for (as_name, fmt) in [("TAR", ExportFormat::Tar), ("tgz", ExportFormat::Tgz), ("tar.gz", ExportFormat::Tgz), ("wbn", ExportFormat::Wbn), ("oci", ExportFormat::Oci)] {
            assert!(matches!(one(&format!("EXPORT / AS {as_name}")), Instruction::Export { format, .. } if format == fmt));
        }
        let Instruction::Export { base, at, .. } = one("EXPORT / AS oci BASE https://x/ AT /srv/www") else { panic!() };
        assert!(base.is_some() && at.is_some());
        parse_err("EXPORT / AS rar", E_UNSUPPORTED, 1, 13, "export format 'rar' is not supported");
        parse_err("EXPORT / AS", E_SYNTAX, 1, 12, "expected export format after AS");
        parse_err("EXPORT", E_SYNTAX, 1, 7, "expected path");
        parse_err("EXPORT / AS directory more", E_SYNTAX, 1, 23, "");
        parse_err("EXPORT / AS wbn BASE", E_SYNTAX, 1, 21, "expected base URL");
    }

    #[test]
    fn unsupported_and_unknown() {
        parse_err("RUN npm build", E_UNSUPPORTED, 1, 1, "never runs shell commands");
        for kw in ["REMOVE", "GENERATE", "ASSERT", "META", "exec"] {
            parse_err(&format!("  {kw} x"), E_UNSUPPORTED, 1, 3, "not supported yet");
        }
        parse_err("  exec x", E_UNSUPPORTED, 1, 3, "EXEC is not supported yet");
        parse_err("\nWORKDIR /x", E_UNKNOWN_INSTRUCTION, 2, 1, "unknown instruction 'WORKDIR'");
    }

    #[test]
    fn from_flag() {
        let Instruction::Copy { from, .. } = one("COPY --from=build /out/ /") else { panic!() };
        let f = from.unwrap();
        assert_eq!((f.name.as_str(), f.span.column), ("build", 6));
        assert!(matches!(one("RENDER --from=build --escape=html /t.html TO /x"), Instruction::Render { from: Some(ref f), .. } if f.name == "build"));
        assert!(matches!(one("COPY a b"), Instruction::Copy { from: None, .. }));
        parse_err("COPY --from a b", E_SYNTAX, 1, 6, "requires a value");
        parse_err("COPY --from=a-b x y", E_SYNTAX, 1, 6, "--from needs a stage name, got 'a-b'");
        parse_err("COPY --from=a --from=b x y", E_SYNTAX, 1, 15, "twice");
    }

    #[test]
    fn plugin_tool_transform_forms() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(matches!(one(&format!("PLUGIN p FROM ./p.mjs INTEGRITY {digest}")), Instruction::Plugin { integrity: Some(_), .. }));
        parse_err("PLUGIN p FROM ./p.mjs EXTRA", E_SYNTAX, 1, 23, "unexpected 'EXTRA' (expected INTEGRITY)");
        parse_err("PLUGIN p ./p.mjs", E_SYNTAX, 1, 10, "expected FROM");
        assert!(matches!(one("TOOL ffmpeg"), Instruction::Tool { ref name, .. } if name == "ffmpeg"));
        parse_err("TOOL a b", E_SYNTAX, 1, 8, "unexpected 'b'");

        let Instruction::Transform { source, plugin, plugin_span, options, each, args, .. } =
            one("TRANSFORM ./a.txt WITH up TO /a.txt OPTIONS x=1 y=\"two words\"")
        else {
            panic!()
        };
        assert!(source.is_some() && plugin == "up" && plugin_span.column == 24 && each.is_none() && args.is_none());
        assert_eq!(options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(lit(&options[1].value), "two words");

        let Instruction::Transform { source, each, options, .. } =
            one("TRANSFORM WITH up TO /c/${post.slug}.svg OPTIONS t=${post.title} FOR post IN posts")
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
        let Instruction::Transform { args, each, .. } = one("TRANSFORM ./a WITH t TO /a FOR x IN xs ARGS a b") else { panic!() };
        assert!(each.is_some() && args.unwrap().len() == 2);

        // inside OPTIONS a second OPTIONS reads as a key (as in TypeScript)
        parse_err("TRANSFORM ./a WITH p TO /a OPTIONS a=1 OPTIONS b=2", E_SYNTAX, 1, 40, "expected OPTIONS=value");
        parse_err("TRANSFORM ./a WITH p TO /a OPTIONS a=1 FOR x IN y OPTIONS b=2", E_SYNTAX, 1, 51, "OPTIONS given twice");
        parse_err("TRANSFORM ./a WITH p TO /a OPTIONS a=1 a=2", E_SYNTAX, 1, 40, "option 'a' given twice");
        parse_err("TRANSFORM ./a WITH p TO /a OPTIONS a", E_SYNTAX, 1, 36, "expected a=value");
        parse_err("TRANSFORM ./a WITH p TO /a FOR x IN y FOR z IN w", E_SYNTAX, 1, 39, "FOR given twice");
        parse_err("TRANSFORM ./a WITH p TO /a EXTRA", E_SYNTAX, 1, 28, "unexpected 'EXTRA' (expected OPTIONS, FOR or ARGS)");
        parse_err("TRANSFORM ./a p TO /a", E_SYNTAX, 1, 15, "expected WITH");
        parse_err("TRANSFORM --from=s WITH p TO /a", E_SYNTAX, 1, 11, "--from needs a source path");
        parse_err("TRANSFORM ./a WITH 9p TO /a", E_SYNTAX, 1, 20, "expected plugin name");
    }

    #[test]
    fn heredoc_bodies() {
        let src = "FROM scratch\nCOPY <<EOF /a.txt\n# not a comment\n\n  indented \\\nEOF\nEXPORT /";
        let ins = parse_sitefile(src, "Sitefile").unwrap();
        let Instruction::Copy { source: Source::Inline(b), .. } = &ins[1] else { panic!() };
        assert_eq!(b.delimiter, "EOF");
        assert_eq!(b.text, "# not a comment\n\n  indented \\\n");
        assert_eq!(b.body_line, 3);
        assert_eq!((b.span.line, b.span.column), (2, 6));

        let inline_text = |src: &str| -> String {
            let ins = parse_sitefile(src, "Sitefile").unwrap();
            ins.into_iter()
                .find_map(|i| match i {
                    Instruction::Copy { source: Source::Inline(b), .. } => Some(b.text),
                    Instruction::Render { template: Source::Inline(b), .. } => Some(b.text),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(inline_text("FROM scratch\nCOPY <<-EOF /a\n\t\tone\n\ttwo\n\tEOF\nEXPORT /\n"), "one\ntwo\n");
        assert_eq!(inline_text("FROM scratch\nCOPY <<E /a\nE\nEXPORT /"), "");
        assert_eq!(inline_text("FROM scratch\r\nCOPY <<E /a\r\nx\r\nE\r\nEXPORT /\r\n"), "x\n");

        let ins = parse_sitefile("FROM scratch\nRENDER \\\n  <<T \\\n  TO /i.html\nhello\nT\nEXPORT /", "Sitefile").unwrap();
        let Instruction::Render { template: Source::Inline(b), .. } = &ins[1] else { panic!() };
        assert_eq!(b.body_line, 5);

        let ins = parse_sitefile("FROM scratch\nTRANSFORM <<T WITH p TO /x\nbody\nT\nEXPORT /", "Sitefile").unwrap();
        assert!(matches!(&ins[1], Instruction::Transform { source: Some(Source::Inline(_)), .. }));

        // quoted or mid-word << is not a heredoc; nor is << in other instructions
        assert_eq!(parse_sitefile("FROM scratch\nCOPY \"<<EOF\" /a\nCOPY ./x<<y /b\nEXPORT /", "Sitefile").unwrap().len(), 4);
        assert_eq!(parse_sitefile("FROM scratch\nLET X = <<EOF\nEXPORT /", "Sitefile").unwrap().len(), 3);
    }

    #[test]
    fn heredoc_errors() {
        parse_err("FROM scratch\nCOPY <<EOF /a\nno end\n", E_SYNTAX, 2, 6, "inline body is missing its closing 'EOF' line");
        parse_err("FROM scratch\nCOPY <<A <<B\nA\nEXPORT /", E_SYNTAX, 2, 10, "only one inline (<<) body");
        parse_err("FROM scratch\nCOPY ./a <<EOF\nx\nEOF\nEXPORT /", E_SYNTAX, 2, 10, "an inline <<EOF body can only be the COPY source");
        parse_err("FROM scratch AS s\nFROM scratch\nCOPY --from=s <<EOF /a\nx\nEOF\nEXPORT /", E_SYNTAX, 3, 6, "--from cannot be combined");
    }
}
