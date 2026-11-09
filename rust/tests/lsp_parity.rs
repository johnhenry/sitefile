//! The language server's diagnostics agree with `sitefile validate`.
//!
//! For every Sitefile under conformance/cases and examples, the Rust CLI's
//! `validate` output equals the TypeScript CLI's (recorded by
//! tests/fixtures/sitefile/generate.mjs in conformance.json), and the LSP's
//! diagnostics for the same text say the same: its first error is the error
//! `validate` prints (code, line, column, message) and its warnings are the
//! warnings. The same check runs the Rust CLI on a sample of the parser fuzz
//! inputs (fuzz.json). ARGs without a default get the LSP's placeholder values
//! on the command line (the LSP assumes them; `validate` would stop with
//! E_MISSING_ARG). Intentional differences are listed in docs/LSP.md.

use serde_json::Value;
use sitefile::language::ast::{Instruction, ValueType};
use sitefile::language::validate::parse_program;
use sitefile::lsp::diagnostics::{analyze, ERROR, WARNING};
use sitefile::lsp::text::Lines;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn fixture(name: &str) -> Vec<Value> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sitefile")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p =
            std::env::temp_dir().join(format!("sitefile-lsp-parity-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(sitefile::platform::canonicalize(&p).unwrap())
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn validate(cwd: &Path, home: &Path, args: &[String]) -> Out {
    let o = Command::new(BIN)
        .arg("validate")
        .args(args)
        .current_dir(cwd)
        .env("SITEFILE_HOME", home)
        .output()
        .expect("run sitefile");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

/// `--build-arg NAME=<placeholder>` for every ARG without a default (as the LSP assumes).
fn placeholder_args(text: &str, label: &str) -> Vec<String> {
    let Ok(p) = parse_program(text, label) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ins in &p.instructions {
        if let Instruction::Arg {
            name,
            ty,
            default_value: None,
            ..
        } = ins
        {
            let v = match ty {
                ValueType::String => "placeholder",
                ValueType::Number => "0",
                ValueType::Boolean => "false",
            };
            out.extend(["--build-arg".to_string(), format!("{name}={v}")]);
        }
    }
    out
}

/// `error[CODE]: label:line:col: detail` → (code, line, col, detail).
fn parse_error_line(line: &str, label: &str) -> Option<(String, Option<u32>, Option<u32>, String)> {
    let rest = line.strip_prefix("error[")?;
    let (code, rest) = rest.split_once("]: ")?;
    let located = rest
        .strip_prefix(label)
        .and_then(|r| r.strip_prefix(':'))
        .and_then(|r| {
            let mut it = r.splitn(3, ':');
            let l = it.next()?.parse().ok()?;
            let c = it.next()?.parse().ok()?;
            Some((l, c, it.next()?.strip_prefix(' ')?.to_string()))
        });
    Some(match located {
        Some((l, c, d)) => (code.to_string(), Some(l), Some(c), d),
        None => (code.to_string(), None, None, rest.to_string()),
    })
}

/// Compare the LSP's diagnostics for `text` with what `validate` printed.
fn lsp_agrees(text: &str, label: &str, code: i32, stderr: &str) -> Result<(), String> {
    let analysis = analyze(text, label);
    let lines = Lines::new(text);
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
                format!("{label}:{}: {}", d.line + 1, d.message)
            }
        })
        .collect();
    let want_warnings: Vec<String> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .map(String::from)
        .collect();
    let note = format!(
        " (checked with placeholder values for {})",
        analysis.required_args.join(", ")
    );
    if code == 0 {
        if let Some(d) = errors.iter().find(|d| d.code != Some("E_TEMPLATE")) {
            return Err(format!("validate passes, LSP reports {d:?}"));
        }
        if warnings != want_warnings {
            return Err(format!(
                "warnings: validate {want_warnings:?}, LSP {warnings:?}"
            ));
        }
        return Ok(());
    }
    let line = stderr
        .lines()
        .find(|l| l.starts_with("error["))
        .ok_or_else(|| format!("no error line in {stderr:?}"))?;
    let (ecode, eline, ecol, detail) =
        parse_error_line(line, label).ok_or_else(|| format!("unparsed {line:?}"))?;
    let d = errors
        .first()
        .ok_or_else(|| format!("validate: {line:?}, LSP: no error"))?;
    let message = d.message.strip_suffix(note.as_str()).unwrap_or(&d.message);
    let want_line = eline.map_or(0, |l| l - 1);
    let c0 = ecol.map_or(0, |c| (c - 1).min(lines.width(want_line)));
    let at = d.start == c0 || (d.end == c0 && d.start + 1 == c0);
    if d.code != Some(ecode.as_str()) || d.line != want_line || !at || message != detail {
        return Err(format!("validate: {line:?}\n    LSP: {d:?}"));
    }
    Ok(())
}

#[test]
fn conformance_sitefiles_cli_and_lsp_agree() {
    let home = Tmp::new("home");
    let root = repo_root();
    let cases = fixture("conformance.json");
    assert!(cases.len() >= 60, "only {} Sitefiles", cases.len());
    let mut bad = Vec::new();
    for case in &cases {
        let dir = root.join(case["dir"].as_str().unwrap());
        let text = fs::read_to_string(dir.join("Sitefile")).unwrap();
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            args,
            placeholder_args(&text, "Sitefile"),
            "{}: regenerate conformance.json",
            dir.display()
        );
        let rust = validate(&dir, &home.0, &args);
        let ts = (
            case["code"].as_i64().unwrap() as i32,
            case["stdout"].as_str().unwrap(),
            case["stderr"].as_str().unwrap(),
        );
        if (rust.code, rust.stdout.as_str(), rust.stderr.as_str()) != ts {
            bad.push(format!(
                "{}: Rust CLI {:?} / {:?} / {:?}, TypeScript CLI {ts:?}",
                case["dir"], rust.code, rust.stdout, rust.stderr
            ));
            continue;
        }
        if let Err(e) = lsp_agrees(&text, "Sitefile", rust.code, &rust.stderr) {
            bad.push(format!("{}: {e}", case["dir"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn fuzz_inputs_cli_and_lsp_agree() {
    let work = Tmp::new("fuzz");
    let home = work.0.join(".home");
    let cases = fixture("fuzz.json");
    let mut bad = Vec::new();
    let mut checked = 0;
    // Every third input keeps the run to a couple of seconds (each one starts the CLI).
    for (i, case) in cases.iter().enumerate().step_by(3) {
        let text = case["input"].as_str().unwrap();
        let label = format!("case-{i}.sitefile");
        fs::write(work.0.join(&label), text).unwrap();
        let mut args = vec!["-f".to_string(), label.clone()];
        args.extend(placeholder_args(text, &label));
        let rust = validate(&work.0, &home, &args);
        // The TypeScript result for the same text (record.mjs): the parse error, else the plan's.
        let want = case["plan"]
            .get("error")
            .or_else(|| case["parse"].get("error"));
        let got = rust
            .stderr
            .lines()
            .find(|l| l.starts_with("error["))
            .and_then(|l| parse_error_line(l, &label));
        let same = match (want, &got) {
            (None, None) => rust.code == 0,
            (Some(w), Some((c, l, col, d))) => {
                w[0].as_str() == Some(c.as_str())
                    && w[1].as_u64() == l.map(u64::from)
                    && w[2].as_u64() == col.map(u64::from)
                    && w[3].as_str() == Some(d.as_str())
            }
            _ => false,
        };
        if !same {
            bad.push(format!(
                "{text:?}: TypeScript {want:?}, Rust CLI {:?}",
                rust.stderr
            ));
        } else if let Err(e) = lsp_agrees(text, &label, rust.code, &rust.stderr) {
            bad.push(format!("{text:?}: {e}"));
        }
        checked += 1;
    }
    assert!(checked >= 100, "only {checked} inputs");
    assert!(
        bad.is_empty(),
        "{} of {checked}:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
