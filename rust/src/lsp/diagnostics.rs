//! Diagnostics from the real pipeline: scan → parse → validate → plan → lint, plus the
//! template parser for RENDER heredoc bodies. Nothing is executed: planning
//! binds variables and normalizes paths without IO (no reads, fetches,
//! plugins or writes). `schema_diagnostics` is the exception: it reads the
//! local imports that have a SCHEMA and their schema files (no fetches).

use crate::build_args::BuildArgs;
use crate::error::{ErrorCode, SitefileError};
use crate::language::ast::{Instruction, Source, ValueType};
use crate::language::parser::parse_sitefile_all;
use crate::language::validate::validate;
use crate::lsp::text::{byte_to_utf16, utf16_len, utf16_to_byte, Lines};
use crate::planner::{plan, Plan};
use crate::render::parse_render_template;
use serde_json::{json, Value};

pub const ERROR: u8 = 1;
pub const WARNING: u8 = 2;
pub const INFORMATION: u8 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub line: u32,
    pub start: u32,
    pub end: u32,
    pub severity: u8,
    pub code: Option<&'static str>,
    pub message: String,
}

impl Diagnostic {
    pub fn to_json(&self) -> Value {
        let mut d = json!({
            "range": crate::lsp::text::range(self.line, self.start, self.line, self.end),
            "severity": self.severity,
            "source": "sitefile",
            "message": self.message,
        });
        if let Some(c) = self.code {
            d["code"] = json!(c);
        }
        d
    }
}

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    /// The plan, when the document plans (with placeholders for ARGs without defaults).
    pub plan: Option<Plan>,
    /// ARGs that have no default (their plan value is a placeholder).
    pub required_args: Vec<String>,
}

/// The range of the word starting at a 1-based span position: to the next
/// whitespace, at least one unit wide where the line allows it.
fn word_range(lines: &Lines, line1: u32, col1: u32) -> (u32, u32, u32) {
    let line = line1.saturating_sub(1);
    let text = lines.get(line);
    let width = utf16_len(text);
    let start = col1.saturating_sub(1).min(width);
    let mut col = 0u32;
    let mut end = start;
    for c in text.chars() {
        let w = c.len_utf16() as u32;
        if col >= start {
            if crate::language::scanner::is_js_space(c) {
                break;
            }
            end = col + w;
        }
        col += w;
    }
    if end == start {
        // At the end of the line (e.g. "expected X"): mark the last character instead.
        if start > 0 && start >= width {
            return (line, start - 1, start);
        }
        end = (start + 1).min(width.max(start));
    }
    (line, start, end)
}

fn from_error(e: &SitefileError, lines: &Lines, note: &str) -> Diagnostic {
    let (line, start, end) = match &e.span {
        Some(s) => word_range(lines, s.line, s.column),
        None => (0, 0, lines.width(0)),
    };
    Diagnostic {
        line,
        start,
        end,
        severity: ERROR,
        code: Some(e.code.as_str()),
        message: format!("{}{note}", e.detail),
    }
}

/// `file:line: message` (planner warnings) → a whole-line warning.
fn from_warning(w: &str, file: &str, lines: &Lines) -> Diagnostic {
    let parsed = w
        .strip_prefix(file)
        .and_then(|r| r.strip_prefix(':'))
        .and_then(|r| {
            let (n, msg) = r.split_once(':')?;
            Some((n.trim().parse::<u32>().ok()?, msg.trim_start()))
        });
    let (line, message) = match parsed {
        Some((n, msg)) => (n.saturating_sub(1), msg.to_string()),
        None => (0, w.to_string()),
    };
    // Lint warnings start with their rule id: `[render-escape] …`.
    let code = message
        .strip_prefix('[')
        .and_then(|r| r.split_once("] "))
        .and_then(|(id, _)| {
            crate::lint::LINT_RULES
                .iter()
                .copied()
                .chain(["lint-comment"])
                .find(|r| *r == id)
        });
    Diagnostic {
        line,
        start: 0,
        end: lines.width(line),
        severity: WARNING,
        code,
        message,
    }
}

/// A placeholder for an ARG without a default: valid for its type and as a path segment.
fn placeholder(ty: ValueType) -> &'static str {
    match ty {
        ValueType::String => "placeholder",
        ValueType::Number => "0",
        ValueType::Boolean => "false",
    }
}

/// Analyze a document. `file` is the label used in spans and warnings.
pub fn analyze(text: &str, file: &str) -> Analysis {
    let lines = Lines::new(text);
    let mut out = Analysis::default();
    let instructions = match parse_sitefile_all(text, file) {
        Ok(i) => i,
        Err(errors) => {
            out.diagnostics = errors.iter().map(|e| from_error(e, &lines, "")).collect();
            return out;
        }
    };
    let program = match validate(instructions, file) {
        Ok(p) => p,
        Err(e) => {
            out.diagnostics.push(from_error(&e, &lines, ""));
            return out;
        }
    };

    // An ARG without a default only fails the build when --build-arg is missing:
    // say so (Information) and plan the rest with a placeholder value.
    let mut args = BuildArgs::new();
    for ins in &program.instructions {
        if let Instruction::Arg {
            name,
            ty,
            default_value: None,
            span,
        } = ins
        {
            args.set(name.clone(), placeholder(*ty).to_string());
            out.required_args.push(name.clone());
            let (line, start, _) = word_range(&lines, span.line, span.column);
            // The name follows the keyword (unless it is on a continuation line).
            let lt = lines.get(line);
            let from = utf16_to_byte(lt, start) + 3;
            let s = lt
                .get(from..)
                .and_then(|r| r.find(name.as_str()))
                .map(|b| byte_to_utf16(lt, from + b))
                .unwrap_or(start);
            out.diagnostics.push(Diagnostic {
                line,
                start: s,
                end: s + utf16_len(name),
                severity: INFORMATION,
                code: Some(ErrorCode::E_MISSING_ARG.as_str()),
                message: format!(
                    "ARG {name} has no default: a build needs --build-arg {name}=<value>"
                ),
            });
        }
    }
    let note = if out.required_args.is_empty() {
        String::new()
    } else {
        format!(
            " (checked with placeholder values for {})",
            out.required_args.join(", ")
        )
    };
    match plan(&program, &args) {
        Ok(p) => {
            for w in &p.warnings {
                out.diagnostics.push(from_warning(w, file, &lines));
            }
            // `sitefile validate`'s lint warnings (rust/src/lint.rs).
            for w in crate::lint::lint(&program, &p, text, &[]) {
                out.diagnostics.push(from_warning(&w, file, &lines));
            }
            out.plan = Some(p);
        }
        Err(e) => {
            out.diagnostics.push(from_error(&e, &lines, &note));
            return out;
        }
    }

    // RENDER heredoc bodies are templates; the executor parses them the same way.
    for ins in &program.instructions {
        if let Instruction::Render {
            template: Source::Inline(body),
            ..
        } = ins
        {
            if let Err(e) = parse_render_template(&body.text, file, body.body_line) {
                out.diagnostics.push(from_error(&e, &lines, ""));
            }
        }
    }
    out
}

/// `IMPORT … SCHEMA` checks, which read files (the server runs them when a document is opened or saved,
/// docs/LSP.md): every local import with a SCHEMA is read from `context_dir` and checked as a build would,
/// so a frontmatter typo shows at its IMPORT line without a build. Nothing is checked unless the document
/// plans, and not while an ARG lacks a default (its placeholder value could name the wrong files).
pub fn schema_diagnostics(
    analysis: &Analysis,
    text: &str,
    file: &str,
    context_dir: &std::path::Path,
) -> Vec<Diagnostic> {
    let Some(plan) = &analysis.plan else {
        return Vec::new();
    };
    if !analysis.required_args.is_empty()
        || analysis.diagnostics.iter().any(|d| d.severity == ERROR)
    {
        return Vec::new();
    }
    let lines = Lines::new(text);
    crate::executor::check_local_schemas(plan, context_dir, file)
        .iter()
        .map(|e| from_error(e, &lines, ""))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Row = (u32, u32, u32, u8, Option<&'static str>, String);

    fn diags(src: &str) -> Vec<Row> {
        analyze(src, "Sitefile")
            .diagnostics
            .into_iter()
            .map(|d| (d.line, d.start, d.end, d.severity, d.code, d.message))
            .collect()
    }

    #[test]
    fn clean_document() {
        assert!(diags("FROM scratch\nCOPY ./a /b\nEXPORT /").is_empty());
        assert!(analyze("FROM scratch\nEXPORT /", "Sitefile").plan.is_some());
    }

    #[test]
    fn syntax_errors_one_per_bad_line() {
        let d = diags("FROM scratch\nCOPY ./a /b extra\nBOGUS\nEXPORT /");
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            (
                1,
                12,
                17,
                ERROR,
                Some("E_SYNTAX"),
                "unexpected 'extra'".into()
            )
        );
        assert_eq!(
            (d[1].0, d[1].1, d[1].2, d[1].4),
            (2, 0, 5, Some("E_UNKNOWN_INSTRUCTION"))
        );
    }

    #[test]
    fn syntax_error_columns_are_utf16() {
        // é is one UTF-16 unit, 😀 two: 'extra' starts at UTF-16 column 14.
        let d = diags("COPY ./é😀 /b extra\nEXPORT /");
        assert_eq!((d[0].0, d[0].1, d[0].2), (0, 14, 19));
    }

    #[test]
    fn expected_at_end_of_line() {
        let d = diags("FROM scratch\nRENDER ./t\nEXPORT /");
        assert_eq!((d[0].0, d[0].1, d[0].2), (1, 9, 10));
        assert!(d[0].5.contains("expected TO"), "{d:?}");
    }

    #[test]
    fn validation_error() {
        let d = diags("FROM scratch\nCOPY ${NOPE} /x\nEXPORT /");
        assert_eq!(d.len(), 1);
        assert_eq!(
            (d[0].0, d[0].1, d[0].3, d[0].4),
            (1, 5, ERROR, Some("E_UNDEFINED"))
        );
        assert!(d[0].5.starts_with("undefined variable ${NOPE}"));
    }

    #[test]
    fn lint_warning_has_its_rule_id() {
        let d = diags("ARG A = 1\nEXPORT /");
        assert_eq!(
            d,
            [(
                0,
                0,
                9,
                WARNING,
                Some("unused-name"),
                "[unused-name] ARG A is never used".into()
            )]
        );
    }

    #[test]
    fn planner_warning_is_whole_line() {
        let d = diags("FROM scratch\nCOPY ./a /a\nFROM scratch\nEXPORT /");
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0, d[0].1, d[0].2, d[0].3), (0, 0, 12, WARNING));
        assert!(
            d[0].5.starts_with("unnamed stage is not the last one"),
            "{d:?}"
        );
    }

    #[test]
    fn planner_error() {
        let d = diags("FROM scratch\nCOPY ./a ../x\nEXPORT /");
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0, d[0].3), (1, ERROR));
    }

    #[test]
    fn arg_without_default_is_information() {
        let a = analyze(
            "ARG SITE_URL\nARG N: number\nCOPY ./${SITE_URL}/${N} /\nEXPORT /",
            "Sitefile",
        );
        let d: Vec<_> = a
            .diagnostics
            .iter()
            .map(|d| (d.line, d.start, d.end, d.severity, d.code))
            .collect();
        assert_eq!(
            d,
            [
                (0, 4, 12, INFORMATION, Some("E_MISSING_ARG")),
                (1, 4, 5, INFORMATION, Some("E_MISSING_ARG"))
            ]
        );
        assert_eq!(a.required_args, ["SITE_URL", "N"]);
        assert!(a.plan.is_some());
    }

    #[test]
    fn template_errors_in_heredoc_bodies() {
        let d = diags("FROM scratch\nRENDER --escape=html <<HTML TO /i.html\n<p>ok</p>\n  ${#if x}\nHTML\nEXPORT /");
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!((d[0].0, d[0].1, d[0].4), (3, 2, Some("E_TEMPLATE")));
    }

    #[test]
    fn schema_errors_read_the_files() {
        let dir = std::env::temp_dir().join(format!("sitefile-lsp-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("posts")).unwrap();
        std::fs::write(
            dir.join("s.yaml"),
            "required: [title]\nproperties:\n  tags: {type: list, items: string}\n",
        )
        .unwrap();
        std::fs::write(dir.join("bad.yaml"), "properties:\n  a: str\n").unwrap();
        std::fs::write(dir.join("posts/a.md"), "---\ntitle: A\n---\nx\n").unwrap();
        std::fs::write(dir.join("posts/b.md"), "---\ntags: [x]\n---\nx\n").unwrap();
        std::fs::write(dir.join("posts/c.md"), "---\ndraft: true\n---\nx\n").unwrap();
        let check = |src: &str| {
            let a = analyze(src, "Sitefile");
            schema_diagnostics(&a, src, "Sitefile", &dir)
                .into_iter()
                .map(|d| (d.line, d.start, d.code, d.message))
                .collect::<Vec<_>>()
        };
        let src = "IMPORT posts FROM ./posts AS markdown SCHEMA ./s.yaml DRAFTS false\nIMPORT p2 FROM ./posts AS markdown SCHEMA ./bad.yaml\nIMPORT r FROM https://example.com/x.json AS json SCHEMA ./s.yaml\nEXPORT /";
        assert_eq!(
            check(src),
            [
                (0, 0, Some("E_SCHEMA"), "IMPORT posts: document '/b.md' has no 'title' (required by schema './s.yaml')".to_string()),
                (1, 35, Some("E_SCHEMA"), "IMPORT p2: schema './bad.yaml' property 'a': unknown type 'str' (expected string, number, boolean, date or list)".to_string()),
            ]
        );
        // Fixed documents: nothing to report. A document that does not plan, or an ARG without a default: no check.
        std::fs::write(dir.join("posts/b.md"), "---\ntitle: B\n---\nx\n").unwrap();
        assert!(check(
            "IMPORT posts FROM ./posts AS markdown SCHEMA ./s.yaml DRAFTS false\nEXPORT /"
        )
        .is_empty());
        assert!(check(
            "IMPORT posts FROM ./posts AS markdown SCHEMA ./missing.yaml\nCOPY ./a ../x\nEXPORT /"
        )
        .is_empty());
        assert!(
            check("ARG S\nIMPORT posts FROM ./posts AS markdown SCHEMA ./${S}.yaml\nEXPORT /")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanner_error() {
        let d = diags("FROM scratch\nCOPY <<A /a\nx");
        assert_eq!((d.len(), d[0].0, d[0].4), (1, 1, Some("E_SYNTAX")));
    }
}
