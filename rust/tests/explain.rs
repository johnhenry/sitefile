//! `sitefile graph` and `sitefile why <path>` (issue #94), ported from test/explain.test.ts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-explain-rs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
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

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sitefile(work: &Tmp, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .env("SITEFILE_HOME", work.0.join(".home"))
        .output()
        .expect("run sitefile");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

const SITEFILE: &str = "STAGE build\nIMPORT theme FROM ./theme AS tree\nCOPY ./src/ /out/\nCOPY --overwrite ./override.html /out/index.html\nSTAGE more FROM build\nCOPY <<X /out/extra.txt\nextra\nX\nSTAGE site\nCOPY --from=more /out/ /\nCOPY --overwrite theme: /\nREDIRECT /old /new\nEXPORT /\n";

fn project(work: &Tmp) -> String {
    let ctx = work.0.join("ctx");
    fs::create_dir_all(ctx.join("src")).unwrap();
    fs::create_dir_all(ctx.join("theme")).unwrap();
    fs::write(ctx.join("Sitefile"), SITEFILE).unwrap();
    fs::write(ctx.join("src/index.html"), "first\n").unwrap();
    fs::write(ctx.join("src/style.css"), "a{}\n").unwrap();
    fs::write(ctx.join("override.html"), "second\n").unwrap();
    fs::write(ctx.join("theme/style.css"), "b{}\n").unwrap();
    ctx.to_str().unwrap().to_string()
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}: {s}"))
}

fn keys(v: &serde_json::Value) -> Vec<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

#[test]
fn graph_formats() {
    let work = Tmp::new();
    let ctx = project(&work);
    let g = sitefile(&work, &["graph", &ctx, "--json"]);
    assert_eq!(g.code, 0, "{}", g.stderr);
    let v = json(&g.stdout);
    assert_eq!(keys(&v), ["file", "nodes", "edges"]);
    assert_eq!(keys(&v["nodes"][0]), ["id", "kind", "label", "line"]);
    assert!(sitefile(&work, &["graph", &ctx, "--dot"])
        .stdout
        .starts_with("digraph sitefile {"));
    assert!(sitefile(&work, &["graph", &ctx])
        .stdout
        .contains("\nexport  EXPORT / AS directory\n"));
    let t = json(&sitefile(&work, &["graph", &ctx, "--json", "--target", "build"]).stdout);
    assert_eq!(t["target"], "build");
}

#[test]
fn why_reports_writes_overwrites_and_inputs() {
    let work = Tmp::new();
    let ctx = project(&work);
    let w = sitefile(
        &work,
        &["why", "/index.html", &ctx, "--no-lockfile", "--json"],
    );
    assert_eq!(w.code, 0, "{}", w.stderr);
    let r = json(&w.stdout);
    assert_eq!(
        keys(&r),
        ["path", "mode", "size", "digest", "origin", "history"]
    );
    assert_eq!(
        keys(&r["history"][0]),
        [
            "location",
            "instruction",
            "stage",
            "path",
            "action",
            "digest",
            "inputs"
        ]
    );
    let h: Vec<(String, String, String, String)> = r["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["stage"].as_str().unwrap().into(),
                e["action"].as_str().unwrap().into(),
                e["path"].as_str().unwrap().into(),
                e["inputs"][0].as_str().unwrap().into(),
            )
        })
        .collect();
    let want = [
        ("build", "write", "/out/index.html", "./src/index.html"),
        ("build", "overwrite", "/out/index.html", "./override.html"),
        ("site", "write", "/index.html", "stage more:/out/index.html"),
    ];
    assert_eq!(
        h,
        want.map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
    );

    let css = sitefile(&work, &["why", "style.css", &ctx, "--no-lockfile"]);
    assert_eq!(css.code, 0, "{}", css.stderr);
    let lines: Vec<&str> = css.stdout.lines().collect();
    assert!(
        lines[0].starts_with("/style.css  644  4 bytes  sha256:"),
        "{}",
        css.stdout
    );
    assert!(
        lines[1].ends_with("Sitefile:3 COPY ./src /out/  [stage build]  wrote /out/style.css"),
        "{}",
        css.stdout
    );
    assert_eq!(lines[2], "       from ./src/style.css");
    assert!(
        lines[5].ends_with("Sitefile:11 COPY theme: /  [stage site]  overwrote /style.css"),
        "{}",
        css.stdout
    );
    assert_eq!(lines[6], "       from import theme:/style.css");

    let more = json(
        &sitefile(
            &work,
            &[
                "why",
                "/out/index.html",
                &ctx,
                "--no-lockfile",
                "--json",
                "--target",
                "more",
            ],
        )
        .stdout,
    );
    let stages: Vec<&str> = more["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["stage"].as_str().unwrap())
        .collect();
    assert_eq!(stages, ["build", "build"]);
    let redirects = json(
        &sitefile(
            &work,
            &["why", "/_redirects", &ctx, "--no-lockfile", "--json"],
        )
        .stdout,
    );
    assert_eq!(redirects["history"][0]["instruction"], "REDIRECT");
}

#[test]
fn why_and_graph_errors() {
    let work = Tmp::new();
    let ctx = project(&work);
    let missing = sitefile(&work, &["why", "/nope", &ctx, "--no-lockfile"]);
    assert_eq!(missing.code, 1);
    assert_eq!(
        missing.stderr,
        "error[E_NOT_FOUND]: '/nope' is not in the build output\n"
    );
    let dir = sitefile(&work, &["why", "/", &ctx, "--no-lockfile"]);
    assert_eq!(
        dir.stderr,
        "error[E_NOT_FOUND]: '/' is a directory in the build output; name a file\n"
    );
    let cases: [(&[&str], &str); 4] = [
        (
            &["why"],
            "why expects a path in the build output: sitefile why <path> [context]",
        ),
        (
            &["graph", &ctx, "--dot", "--json"],
            "--dot and --json conflict",
        ),
        (&["inspect", &ctx, "--dot"], "--dot is only for graph"),
        (&["why", "/a", &ctx, "extra"], "unexpected arguments: extra"),
    ];
    for (args, message) in cases {
        let r = sitefile(&work, args);
        assert_eq!(r.code, 2, "{args:?}");
        assert_eq!(
            r.stderr,
            format!("error: {message}\nRun 'sitefile --help' for usage.\n")
        );
    }
}
