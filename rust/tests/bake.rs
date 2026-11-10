//! `sitefile bake` (issue #92), ported from the CLI tests in test/bake.test.ts.

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
            "sitefile-bake-rs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
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

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_name() == "dist" {
            continue;
        }
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

fn sitefile(work: &Tmp, cwd: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("SITEFILE_HOME", work.0.join(".home"))
        .output()
        .expect("run sitefile");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn example(work: &Tmp) -> PathBuf {
    let dir = work.0.join("bake");
    copy_dir(&repo_root().join("examples").join("bake"), &dir);
    dir
}

fn strip_digest(line: &str) -> &str {
    line.split(" (sha256:").next().unwrap_or(line)
}

#[test]
fn bakes_every_variant_in_plan_order() {
    let work = Tmp::new();
    let dir = example(&work);
    let r = sitefile(&work, &dir, &["bake", "-j", "3"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines: Vec<&str> = r.stdout.trim_end().split('\n').map(strip_digest).collect();
    assert_eq!(
        lines,
        [
            "[site-en-staging] Exported 2 files to dist/staging/en",
            "[site-en-production] Exported 2 files to dist/production/en",
            "[site-fr-staging] Exported 2 files to dist/staging/fr",
            "[site-fr-production] Exported 2 files to dist/production/fr",
            "Baked 4 targets",
        ]
    );
    let fr = fs::read_to_string(dir.join("dist/production/fr/index.html")).unwrap();
    assert!(fr.contains("<html lang=\"fr\">"));
    assert!(fr.contains("href=\"https://example.com/fr/\""));
}

#[test]
fn a_failing_target_fails_the_bake_after_the_others() {
    let work = Tmp::new();
    let dir = example(&work);
    fs::write(dir.join("sitefile.bake.yaml"), "targets:\n  good:\n    args: {LOCALE: fr}\n  bad:\n    args: {LOCALE: de}\n  also:\n    output: out/also\n").unwrap();
    let r = sitefile(&work, &dir, &["bake", "--build-arg", "ENV=ci"]);
    assert_eq!(r.code, 1);
    assert!(
        r.stdout.contains("[good] Exported 2 files to dist/good "),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("[also] Exported 2 files to out/also "),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("Baked"));
    assert!(
        r.stderr.contains("[bad] error[E_NOT_FOUND]: "),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .lines()
            .any(|l| l == "error[E_BAKE]: 1 of 3 targets failed: bad"),
        "{}",
        r.stderr
    );
    assert!(fs::read_to_string(dir.join("out/also/index.html"))
        .unwrap()
        .contains("<p class=\"env\">ci</p>"));
}

#[test]
fn print_builds_nothing_and_usage_errors() {
    let work = Tmp::new();
    let dir = example(&work);
    let r = sitefile(&work, &dir, &["bake", "--print", "--json", "en"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let names: Vec<&str> = v["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["site-en-staging", "site-en-production"]);
    assert!(!dir.join("dist").exists());
    for (args, message) in [
        (vec!["--json"], "bake: --json is only for --print"),
        (vec!["-j", "0"], "--jobs expects a positive integer"),
        (
            vec!["nope"],
            "bake: no target or group named 'nope' (targets: common, site; groups: default, en)",
        ),
        (
            vec!["-f", "bake.toml"],
            "bake file 'bake.toml': unknown format (use a .yaml, .yml or .json file)",
        ),
    ] {
        let mut a = vec!["bake"];
        a.extend(args.iter());
        let u = sitefile(&work, &dir, &a);
        assert_eq!(u.code, 2, "{args:?}: {}", u.stderr);
        assert_eq!(
            u.stderr.lines().next().unwrap(),
            format!("error: {message}")
        );
    }
    let none = sitefile(&work, &work.0, &["bake"]);
    assert_eq!(none.code, 1);
    assert!(none.stderr.starts_with("error[E_NOT_FOUND]: no bake file: sitefile.bake.yaml, sitefile.bake.yml, sitefile.bake.json not found"));
}

#[test]
fn profile_dir_gets_one_trace_per_target() {
    let work = Tmp::new();
    let dir = example(&work);
    let r = sitefile(&work, &dir, &["bake", "en", "--profile", "traces"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    for name in ["site-en-staging", "site-en-production"] {
        assert!(
            fs::read_to_string(dir.join("traces").join(format!("{name}.json")))
                .unwrap()
                .contains("traceEvents")
        );
    }
}

#[test]
fn a_gzip_wbn_target_defaults_to_dot_wbn_dot_gz_like_build() {
    // #151: the same rule as `build`'s default name; the case directory is the conformance case's.
    let work = Tmp::new();
    let dir = work.0.join("bake");
    copy_dir(
        &repo_root()
            .join("conformance")
            .join("cases")
            .join("bake-wbn-gzip"),
        &dir,
    );
    let printed = sitefile(&work, &dir, &["bake", "--print", "--json"]);
    assert_eq!(printed.code, 0, "{}", printed.stderr);
    let plan: serde_json::Value = serde_json::from_str(&printed.stdout).unwrap();
    let outputs: Vec<(String, String)> = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["output"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want = [
        ("gz", "dist/gz.wbn.gz"),
        ("named", "dist/named.wbn"),
        ("plain", "dist/plain.wbn"),
        ("zip", "dist/zip.zip"),
    ];
    assert_eq!(
        outputs,
        want.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<Vec<_>>()
    );
    let r = sitefile(&work, &dir, &["bake"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout
            .lines()
            .any(|l| l.starts_with("[gz] Exported 1 file to dist/gz.wbn.gz (wbn, ")),
        "{}",
        r.stdout
    );
    let gz = fs::read(dir.join("dist").join("gz.wbn.gz")).unwrap();
    assert_eq!(
        &gz[..2],
        &[0x1f, 0x8b],
        "the bytes are gzip, as the name says"
    );
    let plain = fs::read(dir.join("dist").join("plain.wbn")).unwrap();
    assert_ne!(&plain[..2], &[0x1f, 0x8b]);
}
