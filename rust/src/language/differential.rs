//! Differential parser fuzzing against the TypeScript reference
//! (`tests/fixtures/sitefile/generate.mjs`, format in `record.mjs`).
//!
//! For every input: the validated IR or the error (code, line, column,
//! message) must equal TypeScript's, and so must `sitefile validate` (the
//! planner, with placeholder values for ARGs without a default). The same
//! input also goes through `parse_sitefile_all`, the language server's
//! diagnostics and its editor features, which must not panic and must agree
//! with the CLI: the first error diagnostic is the error `sitefile validate`
//! reports, and the warnings are its warnings.

use crate::build_args::BuildArgs;
use crate::error::{SitefileError, SourceSpan};
use crate::language::ast::{
    FragmentBody, FromBase, InlineBody, Instruction, Operand, Program, Source, TemplatePart,
};
use crate::language::parser::{parse_sitefile, parse_sitefile_all};
use crate::language::validate::validate;
use crate::lsp::diagnostics::{analyze, ERROR, WARNING};
use crate::lsp::text::Lines;
use crate::lsp::{features, index};
use crate::planner::plan;
use serde_json::{json, Map, Value};
use std::panic::{catch_unwind, AssertUnwindSafe};

const FILE: &str = "Sitefile";

// ------------------------------------------------------------ IR as JSON (TypeScript's shape)

fn span(s: &SourceSpan) -> Value {
    json!([s.line, s.column])
}

fn operand(o: &Operand) -> Value {
    let parts: Vec<Value> = o
        .parts
        .iter()
        .map(|p| match p {
            TemplatePart::Text(t) => json!({ "kind": "text", "value": t }),
            TemplatePart::Var {
                name,
                path,
                span: s,
            } => json!({ "kind": "var", "name": name, "path": path, "span": span(s) }),
        })
        .collect();
    json!({ "parts": parts, "span": span(&o.span) })
}

fn inline(b: &InlineBody) -> Value {
    json!({ "kind": "inline", "delimiter": b.delimiter, "text": b.text, "span": span(&b.span), "bodyLine": b.body_line })
}

fn source(s: &Source) -> Value {
    match s {
        Source::Operand(o) => operand(o),
        Source::Inline(b) => inline(b),
    }
}

/// An object from `(key, value)` pairs, leaving out `None`s (TypeScript's optional fields).
fn obj(fields: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in fields {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn each(e: &Option<crate::language::ast::ForEach>) -> Option<Value> {
    e.as_ref().map(|e| {
        obj(vec![
            ("as", Some(json!(e.as_name))),
            ("list", Some(json!(e.list))),
            ("span", Some(span(&e.span))),
            (
                "by",
                e.by.as_ref()
                    .map(|b| json!({ "key": b.key, "span": span(&b.span) })),
            ),
            (
                "page",
                e.page.as_ref().map(|p| {
                    obj(vec![
                        ("size", Some(operand(&p.size))),
                        ("first", p.first.as_ref().map(operand)),
                        ("span", Some(span(&p.span))),
                    ])
                }),
            ),
        ])
    })
}

fn stage_ref(f: &Option<crate::language::ast::StageRef>) -> Option<Value> {
    f.as_ref()
        .map(|f| json!({ "name": f.name, "span": span(&f.span) }))
}

pub(crate) fn instruction(ins: &Instruction) -> Value {
    let s = |x: &str| Some(json!(x));
    match ins {
        Instruction::From { span: sp, base, alias, stage_kw } => obj(vec![
            ("kind", s("FROM")),
            ("span", Some(span(sp))),
            ("base", Some(match base {
                FromBase::Scratch => json!("scratch"),
                FromBase::Operand(o) => operand(o),
                FromBase::Image { reference, at } => obj(vec![("kind", s("image")), ("ref", Some(operand(reference))), ("at", at.as_ref().map(operand))]),
            })),
            ("alias", alias.as_ref().map(|a| json!(a))),
            ("keyword", stage_kw.then(|| json!("STAGE"))),
        ]),
        Instruction::Arg { span: sp, name, ty, default_value } => obj(vec![
            ("kind", s("ARG")),
            ("span", Some(span(sp))),
            ("name", Some(json!(name))),
            ("type", s(ty.as_str())),
            ("defaultValue", default_value.as_ref().map(operand)),
        ]),
        Instruction::Let { span: sp, name, ty, value } => obj(vec![
            ("kind", s("LET")),
            ("span", Some(span(sp))),
            ("name", Some(json!(name))),
            ("type", s(ty.as_str())),
            ("value", Some(operand(value))),
        ]),
        Instruction::Fragment { span: sp, name, body } => obj(vec![
            ("kind", s("LET")),
            ("span", Some(span(sp))),
            ("name", Some(json!(name))),
            ("type", s("html")),
            ("body", Some(match body {
                FragmentBody::Line { text, span: at } => json!({ "kind": "line", "text": text, "span": span(at) }),
                FragmentBody::Inline(b) => inline(b),
            })),
        ]),
        Instruction::Import { span: sp, name, uri, as_kind, integrity, sort, schema, drafts, secrets, args } => obj(vec![
            ("kind", s("IMPORT")),
            ("span", Some(span(sp))),
            ("name", Some(json!(name))),
            ("uri", Some(operand(uri))),
            ("as", as_kind.map(|k| json!(k.as_str()))),
            ("integrity", integrity.as_ref().map(|i| json!(i))),
            ("sort", sort.as_ref().map(|so| json!({ "key": so.key, "descending": so.descending, "span": span(&so.span) }))),
            ("schema", schema.as_ref().map(|sc| json!({ "path": operand(&sc.path), "span": span(&sc.span) }))),
            ("drafts", drafts.as_ref().map(|d| json!({ "value": operand(&d.value), "span": span(&d.span) }))),
            (
                "secrets",
                (!secrets.is_empty()).then(|| {
                    Value::Array(secrets.iter().map(|x| json!({ "id": x.id, "header": x.header, "bearer": x.bearer, "span": span(&x.span) })).collect())
                }),
            ),
            (
                "args",
                (!args.is_empty()).then(|| Value::Array(args.iter().map(|a| json!({ "name": a.name, "value": operand(&a.value), "span": span(&a.span) })).collect())),
            ),
        ]),
        Instruction::Copy { span: sp, source: src, dest, from, overwrite, exclude, chmod, parents } => obj(vec![
            ("kind", s("COPY")),
            ("span", Some(span(sp))),
            ("source", Some(source(src))),
            ("dest", Some(operand(dest))),
            ("overwrite", Some(json!(overwrite))),
            ("from", stage_ref(from)),
            ("exclude", (!exclude.is_empty()).then(|| json!(exclude))),
            ("chmod", chmod.map(|m| json!(m))),
            ("parents", parents.then(|| json!(true))),
        ]),
        Instruction::Render { span: sp, template, dest, from, escape, each: e, overwrite, inline_svg } => obj(vec![
            ("kind", s("RENDER")),
            ("span", Some(span(sp))),
            ("template", Some(source(template))),
            ("dest", Some(operand(dest))),
            ("escape", s(escape.as_str())),
            ("overwrite", Some(json!(overwrite))),
            ("from", stage_ref(from)),
            ("each", each(e)),
            ("inlineSvg", inline_svg.then(|| json!(true))),
        ]),
        Instruction::Plugin { span: sp, name, uri, integrity } => obj(vec![
            ("kind", s("PLUGIN")),
            ("span", Some(span(sp))),
            ("name", Some(json!(name))),
            ("uri", Some(operand(uri))),
            ("integrity", integrity.as_ref().map(|i| json!(i))),
        ]),
        Instruction::Tool { span: sp, name } => obj(vec![("kind", s("TOOL")), ("span", Some(span(sp))), ("name", Some(json!(name)))]),
        Instruction::Transform { span: sp, source: src, from, plugin, plugin_span, dest, options, each: e, args, include, exclude, overwrite } => obj(vec![
            ("kind", s("TRANSFORM")),
            ("span", Some(span(sp))),
            ("source", src.as_ref().map(source)),
            ("from", stage_ref(from)),
            ("plugin", Some(json!(plugin))),
            ("pluginSpan", Some(span(plugin_span))),
            ("dest", Some(operand(dest))),
            (
                "options",
                Some(Value::Array(
                    options.iter().map(|o| json!({ "key": o.key, "value": operand(&o.value), "span": span(&o.span) })).collect(),
                )),
            ),
            ("each", each(e)),
            ("args", args.as_ref().map(|a| Value::Array(a.iter().map(operand).collect()))),
            ("include", (!include.is_empty()).then(|| json!(include))),
            ("exclude", (!exclude.is_empty()).then(|| json!(exclude))),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::CheckLinks { span: sp, warn, external } => obj(vec![
            ("kind", s("CHECKLINKS")),
            ("span", Some(span(sp))),
            ("warn", Some(json!(warn))),
            ("external", Some(json!(external))),
        ]),
        Instruction::Fingerprint { span: sp, patterns, manifest, integrity, no_cache_headers, overwrite } => obj(vec![
            ("kind", s("FINGERPRINT")),
            ("span", Some(span(sp))),
            ("patterns", Some(Value::Array(patterns.iter().map(operand).collect()))),
            ("manifest", manifest.as_ref().map(operand)),
            ("integrity", integrity.then(|| json!(true))),
            ("noCacheHeaders", no_cache_headers.then(|| json!(true))),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Canonical { span: sp, base, pretty } => obj(vec![
            ("kind", s("CANONICAL")),
            ("span", Some(span(sp))),
            ("base", Some(operand(base))),
            ("pretty", Some(json!(pretty))),
        ]),
        Instruction::Precompress { span: sp, formats, patterns, min, overwrite } => obj(vec![
            ("kind", s("PRECOMPRESS")),
            ("span", Some(span(sp))),
            ("formats", Some(json!(formats))),
            ("patterns", Some(Value::Array(patterns.iter().map(operand).collect()))),
            ("min", min.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Robots { span: sp, rules, sitemap, dest, overwrite } => obj(vec![
            ("kind", s("ROBOTS")),
            ("span", Some(span(sp))),
            (
                "rules",
                Some(Value::Array(
                    rules.iter().map(|(a, p, r)| json!({ "allow": a, "path": operand(p), "span": span(r) })).collect(),
                )),
            ),
            ("sitemap", Some(json!(sitemap))),
            ("dest", dest.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::SecurityTxt { span: sp, contact, expires, encryption, policy, dest, overwrite } => obj(vec![
            ("kind", s("SECURITY_TXT")),
            ("span", Some(span(sp))),
            ("contact", Some(Value::Array(contact.iter().map(operand).collect()))),
            ("expires", Some(operand(expires))),
            ("encryption", Some(Value::Array(encryption.iter().map(operand).collect()))),
            ("policy", Some(Value::Array(policy.iter().map(operand).collect()))),
            ("dest", dest.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Precache { span: sp, patterns, dest, into, overwrite } => obj(vec![
            ("kind", s("PRECACHE")),
            ("span", Some(span(sp))),
            ("patterns", Some(Value::Array(patterns.iter().map(operand).collect()))),
            ("dest", dest.as_ref().map(operand)),
            ("into", into.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Csp { span: sp, policy, meta } => obj(vec![
            ("kind", s("CSP")),
            ("span", Some(span(sp))),
            ("policy", policy.as_ref().map(operand)),
            ("meta", Some(json!(meta))),
        ]),
        Instruction::Sitemap { span: sp, base, dest, overwrite } => obj(vec![
            ("kind", s("SITEMAP")),
            ("span", Some(span(sp))),
            ("base", Some(operand(base))),
            ("dest", dest.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Feed { span: sp, list, list_span, base, link, dest, title, overwrite } => obj(vec![
            ("kind", s("FEED")),
            ("span", Some(span(sp))),
            ("list", Some(json!(list))),
            ("listSpan", Some(span(list_span))),
            ("base", Some(operand(base))),
            ("link", Some(operand(link))),
            ("dest", dest.as_ref().map(operand)),
            ("title", title.as_ref().map(operand)),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Redirect { span: sp, from, to, status, overwrite } => obj(vec![
            ("kind", s("REDIRECT")),
            ("span", Some(span(sp))),
            ("from", Some(operand(from))),
            ("to", Some(operand(to))),
            ("status", Some(json!(status))),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Header { span: sp, pattern, name, value, overwrite } => obj(vec![
            ("kind", s("HEADER")),
            ("span", Some(span(sp))),
            ("pattern", Some(operand(pattern))),
            ("name", Some(operand(name))),
            ("value", Some(Value::Array(value.iter().map(operand).collect()))),
            ("overwrite", Some(json!(overwrite))),
        ]),
        Instruction::Export { span: sp, path, format, base, at, compression, key } => obj(vec![
            ("kind", s("EXPORT")),
            ("span", Some(span(sp))),
            ("path", Some(operand(path))),
            ("format", s(format.as_str())),
            ("base", base.as_ref().map(operand)),
            ("at", at.as_ref().map(operand)),
            ("compression", compression.as_ref().map(|(v, cs)| json!({ "value": v.as_str(), "span": span(cs) }))),
            ("key", key.as_ref().map(|(id, ks)| json!({ "id": id, "span": span(ks) }))),
        ]),
    }
}

/// `{file, line, column}` objects become `[line, column]` (record.mjs `compact`).
fn compact(v: Value) -> Value {
    match v {
        Value::Object(m)
            if m.len() == 3
                && m.contains_key("file")
                && m.contains_key("line")
                && m.contains_key("column") =>
        {
            json!([m["line"], m["column"]])
        }
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, compact(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(compact).collect()),
        v => v,
    }
}

fn error(e: &SitefileError) -> Value {
    json!({ "error": [e.code.as_str(), e.span.as_ref().map(|s| s.line), e.span.as_ref().map(|s| s.column), e.detail] })
}

/// The value an ARG without a default gets (as in `lsp::diagnostics`).
fn placeholders(p: &Program) -> BuildArgs {
    let mut args = BuildArgs::new();
    for ins in &p.instructions {
        if let Instruction::Arg {
            name,
            ty,
            default_value: None,
            ..
        } = ins
        {
            let v = match ty {
                crate::language::ast::ValueType::Number => "0",
                crate::language::ast::ValueType::Boolean => "false",
                crate::language::ast::ValueType::String => "placeholder",
            };
            args.set(name.clone(), v);
        }
    }
    args
}

// ------------------------------------------------------------ one case

fn panics<T>(what: &str, src: &str, f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        format!("{what} panicked on {src:?}: {msg}")
    })
}

/// Every disagreement or panic for one recorded case.
fn check(case: &Value, positions: bool) -> Vec<String> {
    let src = case["input"].as_str().expect("input");
    let mut bad = Vec::new();
    let mut fail = |what: &str, want: &Value, got: &Value| {
        bad.push(format!("{what} {src:?}\n  want {want}\n  got  {got}"))
    };

    // Parser and validator.
    let parsed = match panics("parse_sitefile", src, || parse_sitefile(src, FILE)) {
        Ok(p) => p,
        Err(m) => return vec![m],
    };
    let program = parsed.clone().and_then(|ins| validate(ins, FILE));
    let got = match &program {
        Ok(p) => json!({ "ok": p.instructions.iter().map(instruction).collect::<Vec<_>>() }),
        Err(e) => error(e),
    };
    if got != case["parse"] {
        fail("parse", &case["parse"], &got);
    }

    // `sitefile validate`: the planner.
    let mut validate_result = got.clone();
    if let Ok(p) = &program {
        let planned = match panics("plan", src, || plan(p, &placeholders(p))) {
            Ok(r) => r,
            Err(m) => return vec![m],
        };
        validate_result = match &planned {
            Ok(pl) => {
                json!({ "ok": pl.warnings, "json": compact(pl.validate_json()), "lint": crate::lint::lint(p, pl, src, &[]) })
            }
            Err(e) => error(e),
        };
        // As text, the way `validate --json` prints it: JavaScript number formatting and key order.
        let text = |v: &Value| crate::values::js_json_stringify(v, false);
        if case.get("plan").map(text) != Some(text(&validate_result)) {
            fail("plan", &case["plan"], &validate_result);
        }
    }

    // The error-tolerant parser: the same instructions, or the same first error.
    match panics("parse_sitefile_all", src, || parse_sitefile_all(src, FILE)) {
        Err(m) => bad.push(m),
        Ok(all) => match (&parsed, &all) {
            (Ok(a), Ok(b)) if a == b => {}
            (Err(e), Err(es))
                if es.first().is_some_and(|f| {
                    f.code == e.code && f.span == e.span && f.detail == e.detail
                }) => {}
            _ => bad.push(format!(
                "parse_sitefile_all disagrees with parse_sitefile on {src:?}: {all:?}"
            )),
        },
    }

    // Language server diagnostics agree with `sitefile validate`.
    match panics("lsp analyze", src, || analyze(src, FILE)) {
        Err(m) => bad.push(m),
        Ok(analysis) => {
            let lines = Lines::new(src);
            let errors: Vec<_> = analysis
                .diagnostics
                .iter()
                .filter(|d| d.severity == ERROR)
                .collect();
            let warnings: Vec<String> = analysis
                .diagnostics
                .iter()
                .filter(|d| d.severity == WARNING)
                .map(|d| {
                    if d.message.starts_with("--build-arg") {
                        d.message.clone()
                    } else {
                        format!("{FILE}:{}: {}", d.line + 1, d.message)
                    }
                })
                .collect();
            let note = format!(
                " (checked with placeholder values for {})",
                analysis.required_args.join(", ")
            );
            if let Some(e) = validate_result["error"].as_array() {
                let (code, line, col, detail) = (
                    e[0].as_str().unwrap(),
                    e[1].as_u64(),
                    e[2].as_u64(),
                    e[3].as_str().unwrap(),
                );
                match errors.first() {
                    None => bad.push(format!(
                        "lsp: no error diagnostic for {src:?}, validate says {e:?}"
                    )),
                    Some(d) => {
                        let message = d.message.strip_suffix(note.as_str()).unwrap_or(&d.message);
                        let want_line = line.map_or(0, |l| l as u32 - 1);
                        // The range covers the word at the error (the last character for an error at the end of a line).
                        let c0 = col.map_or(0, |c| (c as u32 - 1).min(lines.width(want_line)));
                        let at = d.start == c0 || (d.end == c0 && d.start + 1 == c0);
                        if d.code != Some(code) || d.line != want_line || !at || message != detail {
                            bad.push(format!(
                                "lsp diagnostic for {src:?}\n  want {e:?}\n  got  {d:?}"
                            ));
                        }
                    }
                }
            } else {
                // Only template syntax in RENDER heredoc bodies is checked by the LSP and not by `validate`.
                if let Some(d) = errors.iter().find(|d| d.code != Some("E_TEMPLATE")) {
                    bad.push(format!(
                        "lsp: unexpected error diagnostic for {src:?}: {d:?}"
                    ));
                }
                if let Some(w) = validate_result["ok"].as_array() {
                    // The planner's warnings, then the lint warnings, as `validate` prints them.
                    let lint = validate_result["lint"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let want: Vec<String> = w
                        .iter()
                        .chain(lint.iter())
                        .map(|w| w.as_str().unwrap().to_string())
                        .collect();
                    if want != warnings {
                        bad.push(format!(
                            "lsp warnings for {src:?}\n  want {want:?}\n  got  {warnings:?}"
                        ));
                    }
                }
            }
            // Editor features on the same text: no panics anywhere.
            if let Err(m) = panics("lsp features", src, || {
                editor_features(src, Some(&analysis), positions)
            }) {
                bad.push(m);
            }
        }
    }
    bad
}

/// Index the document, list its symbols and (with `positions`) ask for every
/// position-based feature at a spread of positions.
fn editor_features(
    src: &str,
    analysis: Option<&crate::lsp::diagnostics::Analysis>,
    positions: bool,
) {
    let lines = Lines::new(src);
    let ix = index::build(&lines);
    let _ = features::document_symbols(&lines, &ix);
    if !positions {
        return;
    }
    let uri = "file:///p/Sitefile";
    for (l, text) in lines.iter().enumerate().take(16) {
        let l = l as u32;
        let w = crate::lsp::text::utf16_len(text);
        // The ends, both sides of the first few spaces, and a spread in between (and past the end).
        let mut cols = vec![
            0,
            1,
            w / 4,
            w / 2,
            (3 * w) / 4,
            w.saturating_sub(1),
            w,
            w + 1,
        ];
        let mut c = 0;
        for ch in text.chars().take(64) {
            if crate::language::scanner::is_js_space(ch) && cols.len() < 16 {
                cols.extend([c, c + 1]);
            }
            c += ch.len_utf16() as u32;
        }
        cols.sort_unstable();
        cols.dedup();
        for col in cols {
            let c = features::completion(&lines, &ix, l, col);
            let _ = features::completion_json(&c, l, col);
            let _ = features::hover(&ix, analysis, l, col);
            let _ = features::definition(&ix, uri, l, col);
            let _ = features::references(&ix, uri, l, col, true);
            if features::prepare_rename(&ix, l, col).is_some() {
                let _ = features::rename(&ix, uri, l, col, "renamed");
            }
        }
    }
}

/// `every`: position-based editor features run on every n-th input (all of them get the index and symbols).
fn run(cases: &[Value], every: usize) {
    let mut failures = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        failures.extend(check(case, i % every == 0));
    }
    let shown: Vec<_> = failures.iter().take(15).cloned().collect();
    assert!(
        failures.is_empty(),
        "{} disagreements in {} inputs:\n{}",
        failures.len(),
        cases.len(),
        shown.join("\n")
    );
}

fn load(path: &std::path::Path) -> Vec<Value> {
    serde_json::from_str(
        &std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// The committed fixture (`tests/fixtures/sitefile/fuzz.json`).
#[test]
fn differential_fixtures() {
    let cases = load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sitefile/fuzz.json"),
    );
    assert!(cases.len() >= 300, "only {} cases", cases.len());
    let parsed = cases
        .iter()
        .filter(|c| c["parse"].get("ok").is_some())
        .count();
    assert!(parsed >= 50, "only {parsed} inputs parse");
    // The position-based features build JSON for every call, which is slow in a debug build.
    run(&cases, 8);
}

/// A larger, locally generated fuzz file (see generate.mjs).
#[test]
#[ignore]
fn differential_external() {
    let Ok(path) = std::env::var("SITEFILE_PARSER_FUZZ") else {
        return;
    };
    let cases = load(std::path::Path::new(&path));
    run(&cases, 1);
    eprintln!("{} inputs agree", cases.len());
}

/// Hand-picked inputs for the panic checks alone (no TypeScript result needed).
#[test]
fn tolerant_paths_do_not_panic() {
    for src in [
        "",
        "\r",
        "\r\n\r\n",
        "COPY <<A",
        "COPY <<-A /a\n\tA",
        "\u{2028}",
        "😀\n😀",
        "COPY \\\n\\\n\\",
        "RENDER <<H TO /x\n{{#each}}\nH",
    ] {
        let _ = parse_sitefile_all(src, FILE);
        let a = analyze(src, FILE);
        editor_features(src, Some(&a), true);
    }
}
