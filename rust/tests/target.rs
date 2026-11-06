//! `--target <stage>` (issue #83), ported from test/target.test.ts.

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
        let p = std::env::temp_dir().join(format!("sitefile-target-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
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

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sitefile(work: &Tmp, args: &[&str]) -> Out {
    let o = Command::new(BIN).args(args).current_dir(repo_root()).env("SITEFILE_HOME", work.0.join(".home")).output().expect("run sitefile");
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into(), stderr: String::from_utf8_lossy(&o.stderr).into() }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Paths of every file under `dir`, `/`-prefixed and sorted.
fn tree_paths(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, rel: &str, out: &mut Vec<String>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let r = format!("{rel}/{}", e.file_name().to_string_lossy());
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), &r, out);
            } else {
                out.push(r);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out.sort();
    out
}

fn multistage(work: &Tmp) -> PathBuf {
    let ctx = work.0.join("multistage");
    copy_dir(&repo_root().join("examples/multistage"), &ctx);
    let _ = fs::remove_dir_all(ctx.join("dist"));
    ctx
}

#[test]
fn build_target_exports_an_earlier_stage() {
    let work = Tmp::new();
    let ctx = multistage(&work);
    let out = work.0.join("out");
    let r = sitefile(&work, &["build", s(&ctx), "--target", "theme", "-o", s(&out), "--no-lockfile"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("Exported 2 files to "), "{}", r.stdout);
    assert_eq!(tree_paths(&out), ["/css/site.css", "/vendor/index.html"]);
    assert_eq!(fs::read_to_string(out.join("vendor/index.html")).unwrap(), "<h1>Home</h1>\n");
    let r = sitefile(&work, &["build", s(&ctx), "--target", "assets", "-o", s(&out), "--force", "--no-lockfile"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(tree_paths(&out), ["/static/site.css"]);
}

#[test]
fn build_target_keeps_the_export_format_but_not_its_path_or_site_files() {
    let work = Tmp::new();
    let ctx = work.0.join("ctx");
    fs::create_dir_all(&ctx).unwrap();
    fs::write(ctx.join("Sitefile"), "STAGE build\nCOPY <<X /out/a.txt\na\nX\nSTAGE site\nCOPY --from=build /out/ /site/\nREDIRECT /old /new\nEXPORT /site AS zip\n").unwrap();
    let r = sitefile(&work, &["build", s(&ctx), "--target", "build", "--no-lockfile"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("dist.zip (zip, "), "{}", r.stdout);
    let paths = |args: &[&str]| -> Vec<String> {
        let j: serde_json::Value = serde_json::from_str(&sitefile(&work, args).stdout).unwrap();
        j["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(paths(&["inspect", s(&ctx), "--target", "build", "--no-lockfile", "--json"]), ["/out/a.txt"]);
    assert_eq!(paths(&["inspect", s(&ctx), "--no-lockfile", "--json"]), ["/_redirects", "/a.txt"]);
}

#[test]
fn validate_target_and_unknown_targets() {
    let work = Tmp::new();
    let ctx = multistage(&work);
    let v = sitefile(&work, &["validate", s(&ctx), "--target", "assets"]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(v.stdout.ends_with("OK (0 imports, 2 operations, 2 stages, target assets)\n"), "{}", v.stdout);
    let j: serde_json::Value = serde_json::from_str(&sitefile(&work, &["validate", s(&ctx), "--target", "theme", "--json"]).stdout).unwrap();
    assert_eq!(j["target"], "theme");
    assert_eq!(j["export"], "/");
    assert_eq!(j["stages"].as_array().unwrap().len(), 1);
    let plain: serde_json::Value = serde_json::from_str(&sitefile(&work, &["validate", s(&ctx), "--json"]).stdout).unwrap();
    assert!(plain.get("target").is_none());

    for cmd in ["build", "validate", "inspect"] {
        let bad = sitefile(&work, &[cmd, s(&ctx), "--target", "nope", "--no-lockfile"]);
        assert_eq!(bad.code, 2, "{cmd}");
        assert_eq!(bad.stderr, "error: --target 'nope' does not name a stage (stages: theme, assets)\nRun 'sitefile --help' for usage.\n");
    }
    let serve = sitefile(&work, &["serve", "--target", "theme", s(&ctx)]);
    assert_eq!(serve.code, 2);
    assert!(serve.stderr.starts_with("error: --target needs --watch\n"), "{}", serve.stderr);
}
