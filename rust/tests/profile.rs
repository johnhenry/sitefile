//! `--profile` tests, ported from test/profile.test.ts: the trace's events,
//! names, args and order (never its timings).

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("sitefile-profile-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(fs::canonicalize(&p).unwrap())
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            fs::copy(e.path(), &t).unwrap();
        }
    }
}

fn run(work: &Tmp, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(BIN).args(args).current_dir(repo_root()).env("SITEFILE_HOME", work.0.join(".home")).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

fn trace(file: &Path) -> Vec<Value> {
    let doc: Value = serde_json::from_str(&fs::read_to_string(file).unwrap()).unwrap();
    assert_eq!(doc["displayTimeUnit"], "ms");
    let ev = doc["traceEvents"].as_array().unwrap().clone();
    for e in &ev {
        assert_eq!(e["pid"], 1);
        match e["ph"].as_str().unwrap() {
            "X" => assert!(e["ts"].is_u64() && e["dur"].is_u64(), "{e}"),
            ph => assert_eq!(ph, "M"),
        }
    }
    ev
}

fn spans(ev: &[Value]) -> Vec<String> {
    ev.iter().filter(|e| e["ph"] == "X").map(|e| format!("{} {}", e["cat"].as_str().unwrap(), e["name"].as_str().unwrap())).collect()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn build_profile_lists_steps_in_sitefile_order() {
    let work = Tmp::new();
    let ctx = work.0.join("blog");
    copy_dir(&repo_root().join("examples/blog"), &ctx);
    let _ = fs::remove_dir_all(ctx.join("dist"));
    let prof = work.0.join("trace.json");
    let (code, _, err) = run(&work, &["build", s(&ctx), "-o", s(&work.0.join("out")), "--no-lockfile", "--profile", s(&prof)]);
    assert_eq!(code, 0, "{err}");
    let ev = trace(&prof);
    let short: Vec<String> = spans(&ev).iter().map(|n| match n.split_once(' ') {
        Some((cat, name)) => format!("{cat} {}", name.rsplit_once("Sitefile:").map(|(_, r)| r).unwrap_or(name)),
        None => n.clone(),
    }).collect();
    assert_eq!(
        short,
        ["command build", "build parse", "import import posts", "stage stage #1", "operation 17 COPY",
            "operation 19 RENDER",
            "operation 20 RENDER",
            "operation 22 RENDER",
            "operation 26 FEED",
            "operation 27 SITEMAP",
            "operation 31 FINGERPRINT",
            "operation 34 CHECKLINKS",
            "export export"]
    );
    let label_of = |n: &str| n.rsplit_once(':').map(|(l, _)| l.to_string()).unwrap_or_default();
    let ops: Vec<_> = ev.iter().filter(|e| e["cat"] == "operation").map(|e| label_of(e["name"].as_str().unwrap())).collect();
    assert!(ops.iter().all(|l| l.ends_with("blog/Sitefile") && l == &ops[0]), "{ops:?}");
    let imp = ev.iter().find(|e| e["name"] == "import posts").unwrap();
    assert_eq!(imp["tid"], 2);
    assert_eq!(imp["args"], serde_json::json!({ "uri": "./posts", "as": "markdown", "kind": "value" }));
    assert!(ev.iter().any(|e| e["name"] == "thread_name" && e["tid"] == 2 && e["args"]["name"] == "import posts"));
    let export = ev.iter().find(|e| e["name"] == "export").unwrap();
    assert_eq!(export["args"], serde_json::json!({ "format": "directory" }));
}

#[test]
fn transform_calls_record_cache_state() {
    let work = Tmp::new();
    let ctx = work.0.join("wasm");
    copy_dir(&repo_root().join("examples/wasm"), &ctx);
    let prof = work.0.join("trace.json");
    let out = work.0.join("out");
    let args = ["build", s(&ctx), "-o", s(&out), "--force", "--no-lockfile", "--profile", s(&prof)];
    let state = |ev: &[Value]| -> Vec<String> { ev.iter().filter(|e| e["cat"] == "transform").map(|e| e["args"]["cache"].as_str().unwrap().to_string()).collect() };
    assert_eq!(run(&work, &args).0, 0);
    let first = state(&trace(&prof));
    assert!(!first.is_empty() && first.iter().all(|c| c == "miss"), "{first:?}");
    assert_eq!(run(&work, &args).0, 0);
    assert!(state(&trace(&prof)).iter().all(|c| c == "hit"));
    let mut no_cache = args.to_vec();
    no_cache.push("--no-cache");
    assert_eq!(run(&work, &no_cache).0, 0);
    assert!(state(&trace(&prof)).iter().all(|c| c == "off"));
}

#[test]
fn failed_build_writes_trace_with_incomplete_steps() {
    let work = Tmp::new();
    fs::write(work.0.join("a.txt"), "a").unwrap();
    fs::write(work.0.join("b.txt"), "b").unwrap();
    fs::write(work.0.join("Sitefile"), "FROM scratch\nCOPY ./a.txt /x.txt\nCOPY ./b.txt /x.txt\nEXPORT /\n").unwrap();
    let prof = work.0.join("trace.json");
    let (code, _, err) = run(&work, &["build", s(&work.0), "-o", s(&work.0.join("out")), "--no-lockfile", "--profile", s(&prof)]);
    assert_eq!(code, 1);
    assert!(err.contains("error[E_CONFLICT]"), "{err}");
    let got: Vec<(String, bool)> = trace(&prof)
        .iter()
        .filter(|e| e["ph"] == "X")
        .map(|e| {
            let n = e["name"].as_str().unwrap();
            (n.rsplit_once("Sitefile:").map(|(_, r)| r).unwrap_or(n).to_string(), e["args"]["incomplete"] == true)
        })
        .collect();
    let want: Vec<(String, bool)> = [("build", false), ("parse", false), ("stage #1", true), ("2 COPY", false), ("3 COPY", true)].iter().map(|(a, b)| (a.to_string(), *b)).collect();
    assert_eq!(got, want);
}

#[test]
fn trace_inside_context_is_not_an_input_and_other_commands() {
    let work = Tmp::new();
    fs::create_dir_all(work.0.join("src")).unwrap();
    fs::write(work.0.join("src/a.txt"), "a").unwrap();
    fs::write(work.0.join("Sitefile"), "FROM scratch\nCOPY ./ /\nEXPORT /\n").unwrap();
    let prof = work.0.join("src/trace.json");
    let out = work.0.join("out");
    for _ in 0..2 {
        let (code, stdout, err) = run(&work, &["build", s(&work.0), "-o", s(&out), "--force", "--no-lockfile", "--profile", s(&prof)]);
        assert_eq!(code, 0, "{err}");
        assert!(stdout.contains("Exported 2 files"), "{stdout}");
    }
    assert_eq!(run(&work, &["validate", s(&work.0), "--profile", s(&prof)]).0, 0);
    assert_eq!(spans(&trace(&prof)), ["command validate", "build parse"]);
    assert_eq!(run(&work, &["inspect", s(&work.0), "--profile", s(&prof)]).0, 0);
    let sp = spans(&trace(&prof));
    assert_eq!(&sp[..2], ["command inspect", "build parse"]);
    assert!(!sp.contains(&"export export".to_string()));
    let missing = work.0.join("missing/trace.json");
    let (code, _, err) = run(&work, &["build", s(&work.0), "-o", s(&out), "--force", "--no-lockfile", "--profile", s(&missing)]);
    assert_eq!(code, 1);
    assert!(err.contains("error[E_EXPORT]: cannot write the --profile file '") && err.contains("missing/trace.json'"), "{err}");
    let (code, _, err) = run(&work, &["build", s(&work.0), "--profile", ""]);
    assert_eq!(code, 2);
    assert!(err.contains("--profile expects a file path"), "{err}");
}

#[test]
fn target_why_and_graph_are_traced() {
    let work = Tmp::new();
    fs::write(work.0.join("Sitefile"), "STAGE a FROM scratch\nCOPY <<T /a.txt\nhi\nT\nSTAGE b FROM a\nCOPY --from=a /a.txt /b.txt\nEXPORT /\n").unwrap();
    let prof = work.0.join("trace.json");
    let short = || -> Vec<String> {
        spans(&trace(&prof))
            .iter()
            .map(|n| {
                let (cat, name) = n.split_once(' ').unwrap();
                format!("{cat} {}", name.rsplit_once("Sitefile:").map(|(_, r)| r).unwrap_or(name))
            })
            .collect()
    };
    let (code, _, err) = run(&work, &["build", s(&work.0), "-o", s(&work.0.join("out")), "--no-lockfile", "--target", "a", "--profile", s(&prof)]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(short(), ["command build", "build parse", "stage stage a", "operation 2 COPY", "export export"]);
    assert_eq!(trace(&prof).iter().find(|e| e["name"] == "parse").unwrap()["args"]["target"], "a");
    let (code, out, err) = run(&work, &["why", "/b.txt", s(&work.0), "--no-lockfile", "--profile", s(&prof)]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("b.txt"), "{out}");
    assert_eq!(short(), ["command why", "build parse", "stage stage a", "operation 2 COPY", "stage stage b", "operation 6 COPY"]);
    let (code, _, err) = run(&work, &["graph", s(&work.0), "--profile", s(&prof)]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(short(), ["command graph", "build parse"]);
}
