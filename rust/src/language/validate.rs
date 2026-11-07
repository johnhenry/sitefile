//! Semantic rules (src/language/validate.ts; docs/LANGUAGE.md, "v0.1 implementation decisions").

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::language::ast::{
    is_ident, literal_text, stage_names, FromBase, FEED_ITEM, ImportKind, Instruction, Operand, Program, Source, StageRef,
    TemplatePart,
};
use crate::language::parser::parse_sitefile;
use std::collections::{BTreeSet, HashSet};

/// Each operand of an instruction, and whether it may use a FOR loop variable
/// (the destination, OPTIONS values and ARGS of RENDER/TRANSFORM).
pub(crate) fn operands_of(ins: &Instruction) -> Vec<(&Operand, bool)> {
    fn src(s: &Source) -> Option<&Operand> {
        s.as_operand()
    }
    match ins {
        Instruction::From { base: FromBase::Operand(o), .. } => vec![(o, false)],
        Instruction::From { base: FromBase::Image { reference, at }, .. } => std::iter::once(reference).chain(at).map(|o| (o, false)).collect(),
        Instruction::From { .. } | Instruction::Tool { .. } => vec![],
        Instruction::Arg { default_value, .. } => default_value.iter().map(|o| (o, false)).collect(),
        Instruction::Let { value, .. } => vec![(value, false)],
        Instruction::Import { uri, schema, .. } => std::iter::once(uri).chain(schema.as_ref().map(|s| &s.path)).map(|o| (o, false)).collect(),
        Instruction::Plugin { uri, .. } => vec![(uri, false)],
        Instruction::Copy { source, dest, .. } => src(source).map(|o| (o, false)).into_iter().chain([(dest, false)]).collect(),
        Instruction::Render { template, dest, .. } => {
            src(template).map(|o| (o, false)).into_iter().chain([(dest, true)]).collect()
        }
        Instruction::Transform { source, dest, options, args, .. } => source
            .as_ref()
            .and_then(src)
            .map(|o| (o, false))
            .into_iter()
            .chain([(dest, true)])
            .chain(options.iter().map(|o| (&o.value, true)))
            .chain(args.iter().flatten().map(|a| (a, true)))
            .collect(),
        Instruction::Sitemap { base, dest, .. } => std::iter::once(base).chain(dest).map(|o| (o, false)).collect(),
        Instruction::Feed { base, link, dest, title, .. } => {
            [(base, false), (link, true)].into_iter().chain(dest.iter().chain(title).map(|o| (o, false))).collect()
        }
        Instruction::Redirect { from, to, .. } => vec![(from, false), (to, false)],
        Instruction::Header { pattern, name, value, .. } => {
            [(pattern, false), (name, false)].into_iter().chain(value.iter().map(|o| (o, false))).collect()
        }
        Instruction::Fingerprint { patterns, manifest, .. } => patterns.iter().chain(manifest).map(|o| (o, false)).collect(),
        Instruction::CheckLinks { .. } => Vec::new(),
        Instruction::Export { path, .. } => vec![(path, false)],
    }
}

/// Matches of `\{(input|output|option\.[A-Za-z_][A-Za-z0-9_-]*)\}` in `text`:
/// `(byte start, byte end, name)`. These are the only placeholders the host
/// replaces inside TOOL arguments; any other `{…}` stays literal (e.g. ffmpeg's `%{pts}`).
pub fn tool_placeholders(text: &str) -> Vec<(usize, usize, &str)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            let rest = &text[i + 1..];
            let name_len = if rest.starts_with("input}") {
                Some(5)
            } else if rest.starts_with("output}") {
                Some(6)
            } else if let Some(key) = rest.strip_prefix("option.") {
                let kb = key.as_bytes();
                let ok_start = kb.first().is_some_and(|c| *c == b'_' || c.is_ascii_alphabetic());
                let n = kb.iter().take_while(|c| **c == b'_' || **c == b'-' || c.is_ascii_alphanumeric()).count();
                (ok_start && kb.get(n) == Some(&b'}')).then_some(7 + n)
            } else {
                None
            };
            if let Some(n) = name_len {
                out.push((i, i + n + 2, &text[i + 1..i + 1 + n]));
                i += n + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn check_tool_args(ins: &Instruction) -> Result<()> {
    let Instruction::Transform { span, source, plugin, options, args, .. } = ins else { return Ok(()) };
    let args = match args {
        Some(a) if !a.is_empty() => a,
        _ => {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("TRANSFORM … WITH {plugin}: a TOOL needs ARGS (the argument list, e.g. ARGS -i {{input}} {{output}})"),
                span,
            ))
        }
    };
    let mut uses_output = false;
    for arg in args {
        for part in &arg.parts {
            let TemplatePart::Text(value) = part else { continue };
            for (_, _, name) in tool_placeholders(value) {
                if name == "output" {
                    uses_output = true;
                } else if name == "input" {
                    if source.is_none() {
                        return Err(SitefileError::at(E_VALIDATION, "{input} needs a TRANSFORM source file", &arg.span));
                    }
                } else if !options.iter().any(|o| format!("option.{}", o.key) == name) {
                    return Err(SitefileError::at(
                        E_UNDEFINED,
                        format!("{{{name}}}: no OPTIONS {}=… on this TRANSFORM", &name[7..]),
                        &arg.span,
                    ));
                }
            }
        }
    }
    if !uses_output {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("TRANSFORM … WITH {plugin}: ARGS must contain {{output}}, the file the tool writes"),
            span,
        ));
    }
    Ok(())
}

/// Import kinds that can produce a list (and so can be sorted).
fn sortable(kind: Option<ImportKind>) -> bool {
    matches!(kind, Some(ImportKind::Markdown | ImportKind::Json | ImportKind::Yaml | ImportKind::Csv))
}

fn duplicate(name: &str, span: &SourceSpan) -> SitefileError {
    SitefileError::at(E_DUPLICATE, format!("'{name}' is already defined"), span)
}

/// Names declared so far: one namespace for ARG/LET, IMPORT, stage, PLUGIN and TOOL names.
#[derive(Default)]
struct Scope<'a> {
    vars: HashSet<&'a str>,
    imports: HashSet<&'a str>,
    declared_stages: HashSet<&'a str>,
    finished_stages: HashSet<&'a str>,
    plugins: HashSet<&'a str>,
    tools: HashSet<&'a str>,
}

impl Scope<'_> {
    fn taken(&self, name: &str) -> bool {
        self.vars.contains(name)
            || self.imports.contains(name)
            || self.declared_stages.contains(name)
            || self.plugins.contains(name)
            || self.tools.contains(name)
    }
}

/// Check semantic rules and wrap the instructions in a [`Program`].
pub fn validate(instructions: Vec<Instruction>, file: &str) -> Result<Program> {
    let instructions = check(instructions, file)?;
    Ok(Program { file: file.to_string(), instructions })
}

/// How an undefined-base error names the instruction: `FROM` or `STAGE <name> FROM`.
pub fn from_lead(stage_kw: bool, alias: Option<&str>) -> String {
    match (stage_kw, alias) {
        (true, Some(a)) => format!("STAGE {a} FROM"),
        _ => "FROM".to_string(),
    }
}

/// Returns the instructions with the implicit `FROM scratch` inserted when the file has no FROM before its first non-ARG instruction.
fn check(input: Vec<Instruction>, file: &str) -> Result<Vec<Instruction>> {
    let implicit_at = input.iter().position(|i| !matches!(i, Instruction::Arg { .. }));
    let implicit = implicit_at.filter(|_| !matches!(input[implicit_at.unwrap_or(0)], Instruction::From { .. }));
    let mut instructions = input;
    if let Some(at) = implicit {
        let span = instructions[at].span().clone();
        instructions.insert(at, Instruction::From { span, base: FromBase::Scratch, alias: None, stage_kw: false });
    }
    let instructions = &instructions[..];
    let Some(last) = instructions.last() else {
        return Err(SitefileError::at(E_VALIDATION, "Sitefile has no instructions", &SourceSpan::new(file, 1, 1)));
    };
    let all_stages = stage_names(instructions);
    let mut scope = Scope::default();
    let mut current_stage: Option<&str> = None;
    let mut saw_export = false;

    for ins in instructions {
        if saw_export {
            return Err(SitefileError::at(E_VALIDATION, "EXPORT must be the last instruction", ins.span()));
        }

        // RENDER/TRANSFORM … FOR item IN list: the destination (and TRANSFORM options/args) may use ${item.key}.
        // FEED … LINK <path>: the link may use ${item.key} for the current element.
        let loop_var = match ins {
            Instruction::Render { each, .. } | Instruction::Transform { each, .. } => each.as_ref().map(|e| e.as_name.as_str()),
            Instruction::Feed { .. } => Some(FEED_ITEM),
            _ => None,
        };
        for (op, is_dest) in operands_of(ins) {
            for part in &op.parts {
                let TemplatePart::Var { name, path, span } = part else { continue };
                if is_dest && Some(name.as_str()) == loop_var {
                    continue;
                }
                if !path.is_empty() {
                    let dotted = std::iter::once(name.as_str()).chain(path.iter().map(String::as_str)).collect::<Vec<_>>().join(".");
                    return Err(SitefileError::at(
                        E_SYNTAX,
                        format!("dotted access ${{{dotted}}} is only allowed in RENDER templates and for a RENDER … FOR loop variable"),
                        span,
                    ));
                }
                if scope.vars.contains(name.as_str()) {
                    continue;
                }
                let hint = if scope.imports.contains(name.as_str()) {
                    format!(" ('{name}' is an import; use it in a RENDER template)")
                } else {
                    String::new()
                };
                return Err(SitefileError::at(
                    E_UNDEFINED,
                    format!("undefined variable ${{{name}}}: declare it with ARG or LET on an earlier line{hint}"),
                    span,
                ));
            }
        }

        if let Instruction::From { span, base, alias, stage_kw } = ins {
            if let Some(cur) = current_stage {
                scope.finished_stages.insert(cur);
            }
            if let FromBase::Operand(op) = base {
                // A bare word naming a stage must name an earlier one; './name' always means a directory.
                if let Some(text) = literal_text(op) {
                    if is_ident(&text) && all_stages.contains(&text) && !scope.finished_stages.contains(text.as_str()) {
                        return Err(SitefileError::at(
                            E_UNDEFINED,
                            format!("{} {text}: stage '{text}' is not defined before this line (write ./{text} for a local directory)", from_lead(*stage_kw, alias.as_deref())),
                            &op.span,
                        ));
                    }
                }
            }
            if let Some(a) = alias {
                if scope.taken(a) {
                    return Err(duplicate(a, span));
                }
                scope.declared_stages.insert(a);
            }
            current_stage = alias.as_deref();
            continue;
        }
        match ins {
            Instruction::Arg { name, span, .. } | Instruction::Let { name, span, .. } => {
                if scope.taken(name) {
                    return Err(duplicate(name, span));
                }
                scope.vars.insert(name);
            }
            Instruction::Import { name, span, as_kind, sort, schema, .. } => {
                if scope.taken(name) {
                    return Err(duplicate(name, span));
                }
                if let Some(sort) = sort {
                    if !sortable(*as_kind) {
                        return Err(SitefileError::at(
                            E_VALIDATION,
                            format!("IMPORT {name}: SORT BY applies to lists: AS markdown, json, yaml or csv"),
                            &sort.span,
                        ));
                    }
                }
                if let Some(schema) = schema {
                    if !sortable(*as_kind) {
                        return Err(SitefileError::at(
                            E_VALIDATION,
                            format!("IMPORT {name}: SCHEMA applies to documents and lists: AS markdown, json, yaml or csv"),
                            &schema.span,
                        ));
                    }
                }
                scope.imports.insert(name);
            }
            Instruction::Copy { source, from, .. } => check_source(source, &scope, from.as_ref(), &all_stages)?,
            Instruction::Render { template, from, each, .. } => {
                check_source(template, &scope, from.as_ref(), &all_stages)?;
                if let Some(e) = each {
                    check_each(&e.as_name, &e.list, &e.span, &scope)?;
                    if all_stages.contains(&e.as_name) {
                        return Err(loop_var_taken(&e.as_name, &e.span));
                    }
                }
            }
            Instruction::Plugin { name, span, .. } => {
                if scope.taken(name) {
                    return Err(duplicate(name, span));
                }
                scope.plugins.insert(name);
            }
            Instruction::Tool { name, span } => {
                if scope.taken(name) {
                    return Err(duplicate(name, span));
                }
                scope.tools.insert(name);
            }
            Instruction::Transform { span, source, from, plugin, plugin_span, each, args, .. } => {
                if scope.tools.contains(plugin.as_str()) {
                    check_tool_args(ins)?;
                } else if !scope.plugins.contains(plugin.as_str()) {
                    return Err(SitefileError::at(
                        E_UNDEFINED,
                        format!("TRANSFORM … WITH {plugin}: no PLUGIN or TOOL {plugin} is declared on an earlier line"),
                        plugin_span,
                    ));
                } else if args.is_some() {
                    return Err(SitefileError::at(
                        E_VALIDATION,
                        format!("ARGS applies to a TOOL; {plugin} is a PLUGIN (pass values with OPTIONS)"),
                        span,
                    ));
                }
                // With no source and no FOR the plugin gets only its OPTIONS (e.g. a badge or a generated file).
                if let Some(s) = source {
                    check_source(s, &scope, from.as_ref(), &all_stages)?;
                }
                if let Some(e) = each {
                    check_each(&e.as_name, &e.list, &e.span, &scope)?;
                }
            }
            Instruction::Feed { list, list_span, .. } => {
                if !scope.imports.contains(list.as_str()) {
                    return Err(SitefileError::at(
                        E_UNDEFINED,
                        format!("FEED {list}: '{list}' is not an IMPORT declared on an earlier line"),
                        list_span,
                    ));
                }
            }
            Instruction::Sitemap { .. } | Instruction::Redirect { .. } | Instruction::Header { .. } | Instruction::Fingerprint { .. } | Instruction::CheckLinks { .. } => {}
            Instruction::Export { .. } => saw_export = true,
            Instruction::From { .. } => {}
        }
    }

    if !saw_export {
        return Err(SitefileError::at(E_VALIDATION, "missing EXPORT: the last instruction must be EXPORT", last.span()));
    }
    Ok(instructions.to_vec())
}

fn loop_var_taken(as_name: &str, span: &SourceSpan) -> SitefileError {
    SitefileError::at(
        E_DUPLICATE,
        format!("FOR {as_name}: '{as_name}' is already defined; pick another loop variable name"),
        span,
    )
}

fn check_each(as_name: &str, list: &str, span: &SourceSpan, scope: &Scope) -> Result<()> {
    if !scope.imports.contains(list) {
        return Err(SitefileError::at(
            E_UNDEFINED,
            format!("FOR {as_name} IN {list}: '{list}' is not an IMPORT declared on an earlier line"),
            span,
        ));
    }
    if scope.taken(as_name) {
        return Err(loop_var_taken(as_name, span));
    }
    Ok(())
}

/// `^([A-Za-z_][A-Za-z0-9_]*):` — the import name of `name:…`.
fn import_ref_name(text: &str) -> Option<&str> {
    let (name, _) = text.split_once(':')?;
    is_ident(name).then_some(name)
}

fn check_source(source: &Source, scope: &Scope, from: Option<&StageRef>, all: &BTreeSet<String>) -> Result<()> {
    let Source::Operand(op) = source else { return Ok(()) }; // content is in the Sitefile itself
    if let Some(from) = from {
        // The source is a path inside the stage, not an import ref or local path.
        if !scope.finished_stages.contains(from.name.as_str()) {
            let why = if all.contains(&from.name) {
                "is not complete before this line (a stage cannot read itself or a later stage)"
            } else {
                "is not defined"
            };
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!("--from={}: stage '{}' {why}", from.name, from.name),
                &from.span,
            ));
        }
        return Ok(());
    }
    let Some(text) = literal_text(op) else { return Ok(()) };
    if text.contains("://") {
        return Err(SitefileError::at(E_VALIDATION, "URL sources are not allowed here: use IMPORT then COPY", &op.span));
    }
    if let Some(name) = import_ref_name(&text) {
        if !scope.imports.contains(name) {
            return Err(SitefileError::at(
                E_UNDEFINED,
                format!("unknown import '{name}' (declare it with IMPORT on an earlier line, or write ./{text} for a local path)"),
                &op.span,
            ));
        }
    }
    Ok(())
}

/// Parse and validate a Sitefile.
pub fn parse_program(source: &str, file: &str) -> Result<Program> {
    validate(parse_sitefile(source, file)?, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    const HEAD: &str = "FROM scratch\n";
    const TAIL: &str = "\nEXPORT /";

    fn prog(src: &str) -> Program {
        parse_program(src, "Sitefile").unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    fn prog_err(src: &str, code: ErrorCode, line: u32, column: u32, contains: &str) {
        let e = parse_program(src, "Sitefile").expect_err(src);
        assert_eq!(e.code, code, "{src}: {e}");
        let s = e.span.as_ref().expect("span");
        assert_eq!((s.line, s.column), (line, column), "{src}: {e}");
        assert!(e.detail.contains(contains), "{src}: {e} lacks {contains:?}");
    }

    /// Code and line only (the TS tests of blog/plugins/exec check just these).
    fn fails(src: &str, code: ErrorCode, line: u32) {
        let e = parse_program(src, "Sitefile").expect_err(src);
        assert_eq!((e.code, e.span.as_ref().map(|s| s.line)), (code, Some(line)), "{src}: {e}");
    }

    fn kinds(p: &Program) -> Vec<&'static str> {
        p.instructions.iter().map(|i| i.keyword()).collect()
    }

    #[test]
    fn site_instructions_validate() {
        prog_err("FEED posts BASE https://x LINK /a\nEXPORT /", E_UNDEFINED, 1, 6, "'posts' is not an IMPORT");
        prog("IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x LINK /p/${item.slug}.html\nEXPORT /");
        prog_err("IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x/${item.a} LINK /a\nEXPORT /", E_SYNTAX, 2, 27, "dotted access");
        prog_err("SITEMAP BASE ${URL}\nEXPORT /", E_UNDEFINED, 1, 14, "undefined variable ${URL}");
        assert_eq!(kinds(&prog("REDIRECT /a /b\nEXPORT /")), ["FROM", "REDIRECT", "EXPORT"]);
    }

    #[test]
    fn hello_example_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/hello/Sitefile");
        let src = std::fs::read_to_string(path).unwrap();
        let p = parse_program(&src, path).unwrap();
        assert_eq!(p.file, path);
        assert_eq!(kinds(&p), ["ARG", "FROM", "LET", "COPY", "RENDER", "EXPORT"]);
    }

    #[test]
    fn full_program_with_imports_and_variables() {
        let src = [
            "ARG SITE = demo",
            "FROM scratch",
            "ARG VERSION: number = 1",
            "LET OUT = \"/v${VERSION}\"",
            "IMPORT assets FROM ./assets",
            "IMPORT data FROM https://example.com/${SITE}.json AS json",
            "COPY assets: ${OUT}/",
            "COPY assets:/img/logo.png /logo.png",
            "COPY ./foo:bar /x",
            "RENDER --escape=html ./t.html TO ${OUT}/index.html",
            "EXPORT ${OUT}",
        ]
        .join("\n");
        assert_eq!(prog(&src).instructions.len(), 11);
    }

    #[test]
    fn validate_accepts_parsed_instructions() {
        let p = validate(parse_sitefile(&format!("{HEAD}COPY a /b{TAIL}"), "F").unwrap(), "F").unwrap();
        assert_eq!(p.file, "F");
        assert_eq!(p.instructions.len(), 3);
    }

    #[test]
    fn empty_sitefile() {
        prog_err("", E_VALIDATION, 1, 1, "Sitefile has no instructions");
        prog_err("# only comments\n\n", E_VALIDATION, 1, 1, "");
    }

    #[test]
    fn from_rules() {
        prog("FROM ./base\nEXPORT /");
        prog("FROM scratch\nFROM scratch AS b\nEXPORT /");
        assert_eq!(prog("ARG X\nFROM scratch\nEXPORT /").instructions.len(), 3);
    }

    #[test]
    fn implicit_from_scratch() {
        let kinds = |src: &str| prog(src).instructions.iter().map(|i| i.keyword()).collect::<Vec<_>>();
        // No FROM at all: an implicit stage starts at the first instruction, with that instruction's span.
        let p = prog("# c\nCOPY a /b\nEXPORT /");
        assert_eq!(kinds("COPY a /b\nEXPORT /"), ["FROM", "COPY", "EXPORT"]);
        match &p.instructions[0] {
            Instruction::From { span, base: FromBase::Scratch, alias: None, .. } => assert_eq!((span.line, span.column), (2, 1)),
            _ => panic!("expected implicit FROM scratch"),
        }
        assert_eq!(kinds("EXPORT /"), ["FROM", "EXPORT"]);
        // ARG may precede the implicit stage; it stays before it.
        assert_eq!(kinds("ARG X = 1\nARG Y = 2\nLET Z = 3\nCOPY a /b\nEXPORT /"), ["ARG", "ARG", "FROM", "LET", "COPY", "EXPORT"]);
        // Followed by named stages, and an explicit FROM scratch is unchanged.
        assert_eq!(kinds("COPY a /b\nFROM scratch AS two\nEXPORT /"), ["FROM", "COPY", "FROM", "EXPORT"]);
        assert_eq!(kinds("ARG X = 1\nFROM scratch\nEXPORT /"), ["ARG", "FROM", "EXPORT"]);
        // Still errors: nothing but ARGs, or no EXPORT.
        prog_err("ARG X", E_VALIDATION, 1, 1, "missing EXPORT");
        prog_err("COPY a /b", E_VALIDATION, 1, 1, "missing EXPORT");
        prog_err("COPY ./${X} /b\nEXPORT /", E_UNDEFINED, 1, 8, "${X}");
    }

    #[test]
    fn stage_keyword() {
        let kinds = |src: &str| prog(src).instructions.iter().map(|i| i.keyword()).collect::<Vec<_>>();
        // STAGE starts a stage: no implicit stage is added (also after ARG), and it mixes with FROM.
        assert_eq!(kinds("STAGE s\nCOPY a /b\nEXPORT /"), ["FROM", "COPY", "EXPORT"]);
        assert_eq!(kinds("ARG X = 1\nSTAGE s\nCOPY a /b\nEXPORT /"), ["ARG", "FROM", "COPY", "EXPORT"]);
        assert_eq!(kinds("FROM scratch AS a\nSTAGE b FROM a\nFROM b AS c\nSTAGE d\nEXPORT /"), ["FROM", "FROM", "FROM", "FROM", "EXPORT"]);
        // COPY --from a STAGE, and STAGE based on an earlier stage or a local directory.
        prog(&format!("STAGE a\nCOPY x /x\nSTAGE b FROM a\nSTAGE c FROM ./dir\nSTAGE d\nCOPY --from=a /x /y{TAIL}"));
        prog_err(&format!("STAGE a\nCOPY --from=a /x /y{TAIL}"), E_UNDEFINED, 2, 6, "not complete");
        // Same name rules and errors as FROM … AS.
        prog_err(&format!("STAGE a\nSTAGE a{TAIL}"), E_DUPLICATE, 2, 1, "'a' is already defined");
        prog_err(&format!("FROM scratch AS a\nSTAGE a{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("ARG a\nSTAGE a{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("STAGE a\nFROM scratch AS a{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("STAGE a FROM a{TAIL}"), E_UNDEFINED, 1, 14, "STAGE a FROM a: stage 'a' is not defined before this line");
        prog_err(&format!("STAGE a FROM b\nSTAGE b{TAIL}"), E_UNDEFINED, 1, 14, "STAGE a FROM b: stage 'b'");
        prog_err(&format!("FROM b\nSTAGE b{TAIL}"), E_UNDEFINED, 1, 6, "FROM b: stage 'b'");
        // a bare word naming no stage is a local directory
        prog(&format!("STAGE a FROM dir\nSTAGE b FROM ./b{TAIL}"));
    }

    #[test]
    fn export_rules() {
        prog_err("FROM scratch\nCOPY a /b", E_VALIDATION, 2, 1, "missing EXPORT: the last instruction must be EXPORT");
        prog_err("FROM scratch\nEXPORT /\nCOPY a /b", E_VALIDATION, 3, 1, "EXPORT must be the last instruction");
        prog_err("FROM scratch\nEXPORT /\nEXPORT /", E_VALIDATION, 3, 1, "");
    }

    #[test]
    fn variables_must_be_defined_earlier() {
        prog_err(&format!("{HEAD}COPY ./${{X}} /{TAIL}"), E_UNDEFINED, 2, 8, "undefined variable ${X}: declare it with ARG or LET on an earlier line");
        prog_err(&format!("{HEAD}LET X = ${{X}}{TAIL}"), E_UNDEFINED, 2, 9, "");
        prog_err(&format!("{HEAD}LET A = 1\nRENDER t TO \"/a/${{A}}/${{B}}\"{TAIL}"), E_UNDEFINED, 3, 22, "");
        prog_err(&format!("{HEAD}IMPORT d FROM ./${{U}}{TAIL}"), E_UNDEFINED, 2, 17, "");
        prog_err(&format!("{HEAD}EXPORT /${{OUT}}"), E_UNDEFINED, 2, 9, "");
        prog_err(&format!("{HEAD}COPY a /${{late}}\nLET late = x{TAIL}"), E_UNDEFINED, 2, 9, "");
        prog_err(&format!("{HEAD}IMPORT n FROM ./x\nCOPY a /${{n}}{TAIL}"), E_UNDEFINED, 3, 9, "('n' is an import; use it in a RENDER template)");
        prog_err(&format!("ARG A = ${{B}}\nFROM scratch{TAIL}"), E_UNDEFINED, 1, 9, "");
        prog_err(&format!("{HEAD}LET x = 1\nCOPY a /${{X}}{TAIL}"), E_UNDEFINED, 3, 9, "");
        prog_err("FROM scratch\nCOPY \\\n  ./${NOPE} \\\n  /b\nEXPORT /", E_UNDEFINED, 3, 5, "");
    }

    #[test]
    fn dotted_access() {
        prog_err("FROM scratch\nARG a = x\nCOPY ${a.b} /x\nEXPORT /", E_SYNTAX, 3, 6, "dotted access ${a.b} is only allowed in RENDER templates");
    }

    #[test]
    fn duplicate_names() {
        prog_err(&format!("{HEAD}ARG X\nARG X{TAIL}"), E_DUPLICATE, 3, 1, "'X' is already defined");
        prog_err(&format!("ARG X\n{HEAD}LET X = 1{TAIL}"), E_DUPLICATE, 3, 1, "");
        prog_err(&format!("{HEAD}LET X = 1\nIMPORT X FROM ./x{TAIL}"), E_DUPLICATE, 3, 1, "");
        prog_err(&format!("{HEAD}IMPORT a FROM ./x\nIMPORT a FROM ./y{TAIL}"), E_DUPLICATE, 3, 1, "");
        prog_err(&format!("{HEAD}IMPORT a FROM ./x\nLET a = 1{TAIL}"), E_DUPLICATE, 3, 1, "");
    }

    #[test]
    fn import_references() {
        prog_err(&format!("{HEAD}COPY site: /{TAIL}"), E_UNDEFINED, 2, 6, "unknown import 'site' (declare it with IMPORT on an earlier line, or write ./site: for a local path)");
        prog_err(&format!("{HEAD}RENDER tpl:/x.html TO /x{TAIL}"), E_UNDEFINED, 2, 8, "tpl");
        prog_err(&format!("{HEAD}COPY site:/a /\nIMPORT site FROM ./s{TAIL}"), E_UNDEFINED, 2, 6, "");
        prog(&format!("ARG P = x\n{HEAD}COPY ${{P}}: /\nCOPY ./a:b /\nCOPY a/b:c /{TAIL}"));
    }

    #[test]
    fn url_sources_rejected() {
        prog_err(&format!("{HEAD}COPY https://example.com/a.css /{TAIL}"), E_VALIDATION, 2, 6, "IMPORT then COPY");
        prog_err(&format!("{HEAD}RENDER \"http://x/t.html\" TO /t{TAIL}"), E_VALIDATION, 2, 8, "IMPORT");
    }

    #[test]
    fn stages() {
        let p = prog("ARG A = 1\nFROM scratch AS one\nLET B = 2\nFROM ./dir AS two\nIMPORT i FROM ./x\nCOPY --from=one /a /b\nFROM two\nCOPY ${A}${B} /c\nEXPORT /");
        assert_eq!(p.instructions.iter().filter(|i| matches!(i, Instruction::From { .. })).count(), 3);
        prog_err(&format!("FROM scratch AS a\nFROM scratch AS a{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("ARG a\nFROM scratch AS a{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("FROM scratch\nIMPORT a FROM ./x\nFROM scratch AS a{TAIL}"), E_DUPLICATE, 3, 1, "");
        prog_err(&format!("FROM scratch AS a\nLET a = 1{TAIL}"), E_DUPLICATE, 2, 1, "");
        prog_err(&format!("FROM scratch AS a\nIMPORT a FROM ./x{TAIL}"), E_DUPLICATE, 2, 1, "");
    }

    #[test]
    fn forward_and_self_stage_references() {
        prog_err(&format!("FROM scratch AS a\nCOPY --from=a /x /y{TAIL}"), E_UNDEFINED, 2, 6, "--from=a: stage 'a' is not complete before this line");
        prog_err(&format!("FROM scratch AS a\nCOPY --from=b /x /y\nFROM scratch AS b{TAIL}"), E_UNDEFINED, 2, 6, "not complete");
        prog_err(&format!("FROM scratch\nRENDER --from=nope /t TO /x{TAIL}"), E_UNDEFINED, 2, 8, "--from=nope: stage 'nope' is not defined");
        prog_err(&format!("FROM a AS a{TAIL}"), E_UNDEFINED, 1, 6, "FROM a: stage 'a' is not defined before this line (write ./a for a local directory)");
        prog_err(&format!("FROM b\nFROM scratch AS b{TAIL}"), E_UNDEFINED, 1, 6, "");
        prog(&format!("FROM dir\nFROM ./a AS a\nFROM ./a{TAIL}"));
        prog_err("FROM scratch AS a\nEXPORT /\nFROM scratch", E_VALIDATION, 3, 1, "EXPORT must be the last");
        prog_err("FROM scratch AS a\nFROM scratch", E_VALIDATION, 2, 1, "missing EXPORT");
    }

    #[test]
    fn sort_by_and_for() {
        let p = prog("FROM scratch\nIMPORT posts FROM ./p AS markdown SORT BY date DESC\nRENDER ./t TO /p/${post.slug}.html FOR post IN posts\nEXPORT /");
        assert!(matches!(&p.instructions[1], Instruction::Import { as_kind: Some(ImportKind::Markdown), sort: Some(s), .. } if s.key == "date" && s.descending));
        assert!(matches!(&p.instructions[2], Instruction::Render { each: Some(e), .. } if e.as_name == "post" && e.list == "posts"));
        prog_err("FROM scratch\nIMPORT d FROM ./d.txt AS text SORT BY x\nEXPORT /", E_VALIDATION, 2, 31, "IMPORT d: SORT BY applies to lists: AS markdown, json, yaml or csv");
        fails("FROM scratch\nIMPORT d FROM ./d SORT BY x\nEXPORT /", E_VALIDATION, 2);
        prog_err("FROM scratch\nRENDER ./t TO /a FOR p IN nothing\nEXPORT /", E_UNDEFINED, 2, 18, "FOR p IN nothing: 'nothing' is not an IMPORT declared on an earlier line");
        prog_err("FROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./t TO /a FOR posts IN posts\nEXPORT /", E_DUPLICATE, 3, 18, "FOR posts: 'posts' is already defined; pick another loop variable name");
        fails("FROM scratch AS s\nFROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./t TO /a FOR s IN posts\nEXPORT /", E_DUPLICATE, 4);
        fails("FROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./t TO /${post.slug} \nEXPORT /", E_SYNTAX, 3);
        fails("FROM scratch\nARG A = x\nCOPY ./a /${A.b}\nEXPORT /", E_SYNTAX, 3);
        // the template operand is not the destination
        fails("FROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./${post} TO /a FOR post IN posts\nEXPORT /", E_UNDEFINED, 3);
    }

    #[test]
    fn plugin_and_transform() {
        let p = prog("FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nIMPORT posts FROM ./p AS markdown\nTRANSFORM ./a.txt WITH up TO /a.txt OPTIONS x=1 y=\"two words\"\nTRANSFORM WITH up TO /c/${post.slug}.svg OPTIONS t=${post.title} FOR post IN posts\nEXPORT /");
        assert!(matches!(&p.instructions[3], Instruction::Transform { plugin, options, source: Some(_), .. } if plugin == "up" && options.len() == 2));
        assert!(matches!(&p.instructions[4], Instruction::Transform { source: None, each: Some(e), .. } if e.as_name == "post"));
        prog_err("FROM scratch\nTRANSFORM ./a WITH nope TO /a\nEXPORT /", E_UNDEFINED, 2, 20, "TRANSFORM … WITH nope: no PLUGIN or TOOL nope is declared on an earlier line");
        fails("FROM scratch\nARG p = x\nPLUGIN p FROM ./plugins/p.mjs\nEXPORT /", E_DUPLICATE, 3);
        fails("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a OPTIONS a=1 OPTIONS b=2\nEXPORT /", E_SYNTAX, 3);
        fails("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a OPTIONS a=1 a=2\nEXPORT /", E_SYNTAX, 3);
        fails("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /${x.y}\nEXPORT /", E_SYNTAX, 3);
        fails("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM WITH p TO /a FOR x IN nope\nEXPORT /", E_UNDEFINED, 3);
        fails("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM site: WITH p TO /a\nEXPORT /", E_UNDEFINED, 3);
        prog("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM WITH p TO /badge.svg OPTIONS label=hi\nEXPORT /");
    }

    #[test]
    fn tool_args() {
        let tool = |t: &str| format!("FROM scratch\nTOOL fake\n{t}\nEXPORT /");
        let p = prog(&tool("IMPORT xs FROM ./x.json AS json\nTRANSFORM ./a.txt WITH fake TO /a.out OPTIONS w=3 ARGS -f \"lavfi x\" --w={option.w} {input} {output}"));
        assert!(p.instructions.iter().any(|i| matches!(i, Instruction::Transform { args: Some(a), .. } if a.len() == 5)));
        prog_err("FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a\nEXPORT /", E_VALIDATION, 3, 1, "TRANSFORM … WITH t: a TOOL needs ARGS (the argument list, e.g. ARGS -i {input} {output})");
        prog_err("FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS -i {input}\nEXPORT /", E_VALIDATION, 3, 1, "TRANSFORM … WITH t: ARGS must contain {output}, the file the tool writes");
        prog_err("FROM scratch\nTOOL t\nIMPORT xs FROM ./x.json AS json\nTRANSFORM WITH t TO /a ARGS {input} {output} FOR x IN xs\nEXPORT /", E_VALIDATION, 4, 29, "{input} needs a TRANSFORM source file");
        prog_err("FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS {option.nope} {output}\nEXPORT /", E_UNDEFINED, 3, 33, "{option.nope}: no OPTIONS nope=… on this TRANSFORM");
        prog_err("FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a ARGS x\nEXPORT /", E_VALIDATION, 3, 1, "ARGS applies to a TOOL; p is a PLUGIN (pass values with OPTIONS)");
        fails("FROM scratch\nTOOL t\nTOOL t\nEXPORT /", E_DUPLICATE, 3);
        fails("FROM scratch\nARG t = 1\nTOOL t\nEXPORT /", E_DUPLICATE, 3);
        prog("FROM scratch\nTOOL t\nTRANSFORM ./a WITH t TO /a ARGS -vf drawtext=text=%{pts}{x} {output}\nEXPORT /");
    }

    #[test]
    fn placeholders() {
        let names = |s: &str| tool_placeholders(s).into_iter().map(|(_, _, n)| n.to_string()).collect::<Vec<_>>();
        assert_eq!(names("{input}{output}"), ["input", "output"]);
        assert_eq!(names("--w={option.w-1}x{option.}{option.9}{inputs}{{output}}"), ["option.w-1", "output"]);
        assert_eq!(tool_placeholders("a{output}"), [(1, 9, "output")]);
        assert!(names("%{pts}{x}").is_empty());
    }

    #[test]
    fn heredocs_validate() {
        let p = prog("FROM scratch\nRENDER \\\n  <<T \\\n  TO /i.html\nhello\nT\nCOPY <<C /c\nc\nC\nEXPORT /\n");
        assert_eq!(kinds(&p), ["FROM", "RENDER", "COPY", "EXPORT"]);
        assert_eq!(prog("FROM scratch\nCOPY \"<<EOF\" /a\nCOPY ./x<<y /b\nEXPORT /").instructions.len(), 4);
        // inline bodies are not checked for variables
        prog("FROM scratch\nRENDER <<T TO /a\n${MISSING}\nT\nEXPORT /");
    }
}
