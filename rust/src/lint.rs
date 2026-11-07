//! Lint rules for `sitefile validate` and the language server (src/lint.ts).
//!
//! Each rule has a stable id and reports a likely mistake in a Sitefile that is otherwise valid.
//! Rules look at the Sitefile only (no IO): the validated program, its plan, and its comments.
//! A warning is `<file>:<line>: [<id>] <message>`, the line being the instruction it is about.
//!
//! Disabling: `# sitefile-lint: disable=<id>[,<id>…]` anywhere turns rules off for the whole file,
//! `# sitefile-lint: disable-next-line=<id>[,…]` for the next instruction only, and the CLI's
//! `--lint-disable <id>[,<id>…]` for one run; `all` names every rule.

use crate::executor::template_names;
use crate::language::ast::ExportFormat;
use crate::language::ast::{EscapeMode, Instruction, Operand, Program, Source, TemplatePart};
use crate::language::scanner::{
    is_js_space, js_trim, logical_lines_with_comments, CommentLine, LogicalLine,
};
use crate::language::validate::{operands_of, parse_fragment_body};
use crate::planner::{imported_stage_refs, ImportLocation, Operation, Plan, SourceRef, StageBase};
use crate::refs::parse_ref;
use crate::render::parse_render_template;
use std::collections::{HashMap, HashSet};

/// Every rule, in the order warnings of one line are reported.
pub const LINT_RULES: [&str; 6] = [
    "unused-name",
    "unpinned-import",
    "unpinned-image",
    "render-escape",
    "sitemap-order",
    "shadowed-write",
];

/// Import kinds a RENDER template can read (so a template file may use them).
const VALUE_KINDS: [&str; 7] = ["text", "json", "yaml", "csv", "markdown", "wbn", "html"];

struct Found {
    line: u32,
    rule: &'static str,
    message: String,
}

/// Text of an operand as written (`${…}` kept), for messages and extension checks.
pub fn written(op: &Operand) -> String {
    op.parts
        .iter()
        .map(|p| match p {
            TemplatePart::Text(t) => t.clone(),
            TemplatePart::Var { name, path, .. } => {
                let mut s = format!("${{{name}");
                for k in path {
                    s.push('.');
                    s.push_str(k);
                }
                s.push('}');
                s
            }
        })
        .collect()
}

/// Every operand of an instruction, EXPORT's BASE and AT included.
fn operands(ins: &Instruction) -> Vec<&Operand> {
    let mut ops: Vec<&Operand> = operands_of(ins).into_iter().map(|(o, _)| o).collect();
    if let Instruction::Export { base, at, .. } = ins {
        ops.extend(base.iter().chain(at.iter()));
    }
    ops
}

/// `#[ \t]*sitefile-lint[ \t]*:` (name ASCII case-insensitive) on a comment from its `#`: the rest.
pub fn lint_comment(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('#')?.trim_start_matches([' ', '\t']);
    let name = rest.get(..13)?;
    if !name.eq_ignore_ascii_case("sitefile-lint") {
        return None;
    }
    rest[13..].trim_start_matches([' ', '\t']).strip_prefix(':')
}

/// `(disable|disable-next-line)[ \t]*=[ \t]*(.+)` on a trimmed comment body.
fn lint_body(body: &str) -> Option<(&'static str, &str)> {
    for verb in ["disable", "disable-next-line"] {
        if let Some(r) = body.strip_prefix(verb) {
            if let Some(r) = r.trim_start_matches([' ', '\t']).strip_prefix('=') {
                let ids = r.trim_start_matches([' ', '\t']);
                if !ids.is_empty() {
                    return Some((verb, ids));
                }
            }
        }
    }
    None
}

type Directives = (HashSet<String>, HashMap<u32, HashSet<String>>);

/// Parse `# sitefile-lint:` comments: rules off for the file, rules off per line, and problems.
fn directives(
    comments: &[CommentLine],
    lines: &[LogicalLine],
    found: &mut Vec<Found>,
) -> Directives {
    let mut file = HashSet::new();
    let mut per_line: HashMap<u32, HashSet<String>> = HashMap::new();
    for c in comments {
        let Some(rest) = lint_comment(&c.text) else {
            continue;
        };
        let Some((verb, ids)) = lint_body(js_trim(rest)) else {
            found.push(Found {
                line: c.line,
                rule: "lint-comment",
                message: "unrecognized # sitefile-lint comment (expected disable=<rule>[,<rule>…] or disable-next-line=<rule>[,<rule>…])".into(),
            });
            continue;
        };
        let mut known = Vec::new();
        for id in ids.split(',').map(js_trim) {
            if id == "all" || LINT_RULES.contains(&id) {
                known.push(id.to_string());
            } else {
                found.push(Found {
                    line: c.line,
                    rule: "lint-comment",
                    message: format!(
                        "unknown lint rule '{id}' (rules: {}, or all)",
                        LINT_RULES.join(", ")
                    ),
                });
            }
        }
        if verb == "disable" {
            file.extend(known);
        } else if let Some(next) = lines
            .iter()
            .find(|l| l.positions.first().is_some_and(|p| p.line > c.line))
        {
            per_line
                .entry(next.positions[0].line)
                .or_default()
                .extend(known);
        }
    }
    (file, per_line)
}

/// Whether a RENDER line sets `--escape=` explicitly (before its template).
fn explicit_escape(line: Option<&LogicalLine>) -> bool {
    let Some(line) = line else { return false };
    let text = line.text();
    for w in text.split(is_js_space).filter(|w| !w.is_empty()).skip(1) {
        if !w.starts_with("--") {
            break;
        }
        if w.starts_with("--escape=") {
            return true;
        }
    }
    false
}

/// The file path a RENDER, COPY or TRANSFORM writes, when it is known before the build.
fn written_path(op: &Operation) -> Option<String> {
    match op {
        Operation::Render { each: Some(e), .. } => Some(written(&e.dest)),
        Operation::Render { dest, .. } => Some(dest.clone()),
        Operation::Copy {
            dest,
            dest_is_dir: false,
            ..
        } => Some(dest.clone()),
        Operation::Transform { dest, .. } => Some(written(dest)),
        _ => None,
    }
}

fn is_html_page(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".html") || p.ends_with(".htm")
}

/// Lint a validated program and its plan. `source` is the Sitefile text (for comments),
/// `disabled` the rule ids turned off by the caller (`--lint-disable`).
pub fn lint(program: &Program, plan: &Plan, source: &str, disabled: &[String]) -> Vec<String> {
    let file = program.file.as_str();
    let mut comments = Vec::new();
    let lines = logical_lines_with_comments(source, file, &mut comments).unwrap_or_default();
    let by_start: HashMap<(u32, u32), &LogicalLine> = lines
        .iter()
        .filter_map(|l| l.positions.first().map(|p| ((p.line, p.column), l)))
        .collect();
    let mut found = Vec::new();
    let (off_in_file, off_per_line) = directives(&comments, &lines, &mut found);
    let off: HashSet<String> = off_in_file
        .into_iter()
        .chain(disabled.iter().cloned())
        .collect();
    let mut report = |line: u32, rule: &'static str, message: String| {
        found.push(Found {
            line,
            rule,
            message,
        })
    };

    // ---- unused-name: a declaration nothing refers to.
    let mut used: HashSet<String> = HashSet::new();
    // A template file (or one from an import or stage) can read any ARG, LET or value import.
    let mut opaque_templates = false;
    for ins in &program.instructions {
        for op in operands(ins) {
            for p in &op.parts {
                if let TemplatePart::Var { name, .. } = p {
                    used.insert(name.clone());
                }
            }
        }
        match ins {
            Instruction::Render { template, each, .. } => {
                match template {
                    Source::Inline(body) => {
                        match parse_render_template(&body.text, file, body.body_line) {
                            Ok(nodes) => used.extend(template_names(&nodes)),
                            Err(_) => opaque_templates = true,
                        }
                    }
                    Source::Operand(_) => opaque_templates = true,
                }
                if let Some(e) = each {
                    used.insert(e.list.clone());
                }
            }
            Instruction::Transform { each, plugin, .. } => {
                if let Some(e) = each {
                    used.insert(e.list.clone());
                }
                used.insert(plugin.clone());
            }
            Instruction::Feed { list, .. } => {
                used.insert(list.clone());
            }
            // An html LET's template names what it uses (validate already parsed it).
            Instruction::Fragment { body, .. } => {
                if let Ok(nodes) = parse_fragment_body(body, file) {
                    used.extend(template_names(&nodes));
                }
            }
            _ => {}
        }
    }
    for stage in &plan.stages {
        for op in &stage.operations {
            let src = match op {
                Operation::Copy { source, .. } => Some(source),
                Operation::Render { template, .. } => Some(template),
                Operation::Transform { source, .. } => source.as_ref(),
                _ => None,
            };
            if let Some(SourceRef::Import { name, .. }) = src {
                used.insert(name.clone());
            }
        }
    }
    // An IMPORT … AS sitefile is used by naming one of its stages (`FROM theme+assets`, `--from=theme+assets`).
    for r in imported_stage_refs(&plan.stages) {
        used.insert(r.import);
    }
    for ins in &program.instructions {
        let decl = match ins {
            Instruction::Arg { name, .. } if !opaque_templates => Some(("ARG", name)),
            Instruction::Let { name, .. } | Instruction::Fragment { name, .. }
                if !opaque_templates =>
            {
                Some(("LET", name))
            }
            Instruction::Import { name, as_kind, .. } => {
                let value = as_kind.is_some_and(|k| VALUE_KINDS.contains(&k.as_str()));
                (!opaque_templates || !value).then_some(("IMPORT", name))
            }
            Instruction::Plugin { name, .. } => Some(("PLUGIN", name)),
            Instruction::Tool { name, .. } => Some(("TOOL", name)),
            _ => None,
        };
        if let Some((what, name)) = decl {
            if !used.contains(name) {
                report(
                    ins.span().line,
                    "unused-name",
                    format!("{what} {name} is never used"),
                );
            }
        }
    }

    // ---- unpinned-import: a remote IMPORT without INTEGRITY.
    for spec in &plan.imports {
        if let (ImportLocation::Remote { url }, None) = (&spec.location, &spec.integrity) {
            report(
                spec.span.line,
                "unpinned-import",
                format!("IMPORT {} has no INTEGRITY, so {url} is trusted on first use (pin it with INTEGRITY sha256:…)", spec.name),
            );
        }
    }

    // ---- unpinned-image: a registry image (FROM image, an oci EXPORT BASE) without a digest.
    let pinned = |r: &str| parse_ref(r).ok().map(|r| r.digest.is_some());
    let unpinned_image = |what: &str, r: &str| {
        format!("{what} {r} has no @sha256: digest, so the tag is trusted on first use (pin it as {r}@sha256:…)")
    };
    for stage in &plan.stages {
        if let StageBase::Image {
            reference,
            local: false,
            lead,
            ..
        } = &stage.base
        {
            if pinned(reference) == Some(false) {
                report(
                    stage.span.line,
                    "unpinned-image",
                    unpinned_image(&format!("{lead} image"), reference),
                );
            }
        }
    }
    let exp = program
        .instructions
        .iter()
        .find(|i| matches!(i, Instruction::Export { .. }));
    if let (Some(exp), ExportFormat::Oci, Some(base)) = (exp, plan.export_format, &plan.export_base)
    {
        if pinned(base) == Some(false) {
            report(
                exp.span().line,
                "unpinned-image",
                unpinned_image("EXPORT … AS oci BASE", base),
            );
        }
    }

    for stage in &plan.stages {
        let ops = &stage.operations;
        // ---- render-escape: an HTML page rendered without escaping.
        for op in ops {
            let Operation::Render {
                escape: EscapeMode::None,
                span,
                ..
            } = op
            else {
                continue;
            };
            let dest = written_path(op).unwrap_or_default();
            if !is_html_page(&dest)
                || explicit_escape(by_start.get(&(span.line, span.column)).copied())
            {
                continue;
            }
            report(
                span.line,
                "render-escape",
                format!("RENDER writes {dest} without --escape=html, so substituted values are not HTML-escaped (add --escape=html, or --escape=none if that is intended)"),
            );
        }
        // ---- sitemap-order: a page written after the SITEMAP that should list it.
        for (i, op) in ops.iter().enumerate() {
            let Operation::Sitemap { span, .. } = op else {
                continue;
            };
            if let Some(later) = ops[i + 1..]
                .iter()
                .find(|o| written_path(o).is_some_and(|p| p.ends_with(".html")))
            {
                report(
                    span.line,
                    "sitemap-order",
                    format!(
                        "SITEMAP lists the .html files written before it, but line {} writes {} after it (move SITEMAP after the pages)",
                        later.span().line,
                        written_path(later).unwrap_or_default()
                    ),
                );
            }
        }
        // ---- shadowed-write: a file replaced by a later --overwrite before anything reads it.
        let mut last: HashMap<String, u32> = HashMap::new();
        for op in ops {
            let (path, overwrite) = match op {
                Operation::Render {
                    each: None,
                    dest,
                    overwrite,
                    ..
                } => (dest, *overwrite),
                Operation::Copy {
                    source: SourceRef::Inline { .. },
                    dest,
                    overwrite,
                    ..
                } => (dest, *overwrite),
                _ => continue,
            };
            if let Some(&before) = last.get(path) {
                if overwrite {
                    report(
                        before,
                        "shadowed-write",
                        format!("{path} written here is replaced by line {} (--overwrite), so this write has no effect", op.span().line),
                    );
                }
            }
            last.insert(path.clone(), op.span().line);
        }
    }

    let is_off = |f: &Found| {
        let line = off_per_line.get(&f.line);
        off.contains("all")
            || off.contains(f.rule)
            || line.is_some_and(|s| s.contains("all") || s.contains(f.rule))
    };
    let mut kept: Vec<Found> = found
        .into_iter()
        .filter(|f| f.rule == "lint-comment" || !is_off(f))
        .collect();
    // Stable: by line, then in the order the rules ran.
    kept.sort_by_key(|f| f.line);
    kept.into_iter()
        .map(|f| format!("{file}:{}: [{}] {}", f.line, f.rule, f.message))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_args::BuildArgs;
    use crate::language::validate::parse_program;
    use crate::planner::plan;

    fn check(src: &str, disabled: &[&str]) -> Vec<String> {
        let program = parse_program(src, "Sitefile").unwrap();
        let p = plan(&program, &BuildArgs::new()).unwrap();
        lint(
            &program,
            &p,
            src,
            &disabled.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    // Every rule's text is checked against TypeScript by the differential fixtures; these cover each rule once.
    #[test]
    fn each_rule() {
        let src = "ARG A = 1\nIMPORT r FROM https://e.x/a.json AS json\nFROM image ghcr.io/a/b:1 AS i\nFROM scratch\nCOPY --from=i / /\nSITEMAP BASE https://e.x\nRENDER <<H TO /a.html\n${r.x}\nH\nRENDER --escape=html --overwrite <<H TO /a.html\nx\nH\nEXPORT /";
        let ws = check(src, &[]);
        let ids: Vec<String> = ws
            .iter()
            .map(|w| {
                w.split('[')
                    .nth(1)
                    .unwrap()
                    .split(']')
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            ids,
            [
                "unused-name",
                "unpinned-import",
                "unpinned-image",
                "sitemap-order",
                "render-escape",
                "shadowed-write"
            ],
            "{ws:#?}"
        );
        assert_eq!(ws[0], "Sitefile:1: [unused-name] ARG A is never used");
        assert!(check(src, &["all"]).is_empty());
    }

    #[test]
    fn comments_disable_rules() {
        let src = "# sitefile-lint: disable-next-line=unused-name\nARG A = 1\nARG B = 1\n# sitefile-lint: disable=bogus\nEXPORT /";
        assert_eq!(
            check(src, &[]),
            [
                "Sitefile:3: [unused-name] ARG B is never used",
                "Sitefile:4: [lint-comment] unknown lint rule 'bogus' (rules: unused-name, unpinned-import, unpinned-image, render-escape, sitemap-order, shadowed-write, or all)",
            ]
        );
        assert_eq!(lint_comment("#  SITEFILE-LINT : x"), Some(" x"));
        assert_eq!(lint_comment("# sitefile-lints: x"), None);
    }
}
