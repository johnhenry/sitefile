//! TRANSFORM over a directory (issue #127), ported from test/transform-dir.test.ts:
//! one plugin or tool call per file, in path order, cached and memoized per file.

use sitefile::error::ErrorCode;
use sitefile::executor::{build, BuildOptions, LockfileSetting};
use sitefile::memo::BuildMemo;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

/// One test at a time: the image module is compiled once and reused from the in-process
/// cache; the disk cache is off so nothing lands under the real $SITEFILE_HOME.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    static ONCE: Once = Once::new();
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ONCE.call_once(|| std::env::set_var("SITEFILE_NO_WASM_CACHE", "1"));
    g
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}
fn testdata(name: &str) -> Vec<u8> {
    fs::read(repo_root().join("plugins/image/testdata").join(name)).unwrap()
}

struct Tmp(PathBuf);
impl Tmp {
    fn new(files: &[(&str, Vec<u8>)]) -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("sitefile-tdir-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        let t = Tmp(fs::canonicalize(&p).unwrap());
        t.write(&[("plugins/image.wasm", fs::read(repo_root().join("plugins/image/image.wasm")).unwrap())]);
        t.write(files);
        t
    }
    fn write(&self, files: &[(&str, Vec<u8>)]) {
        for (rel, bytes) in files {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, bytes).unwrap();
        }
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn options(dir: &Path) -> BuildOptions {
    BuildOptions { context_dir: dir.to_string_lossy().into_owned(), lockfile: LockfileSetting::Disabled, ..Default::default() }
}
fn paths(t: &sitefile::tree::Tree) -> Vec<String> {
    t.entries().map(|(p, _)| p.clone()).collect()
}
fn images() -> Vec<(&'static str, Vec<u8>)> {
    vec![("in/a.png", testdata("tiny.png")), ("in/b.jpg", testdata("tiny.jpg")), ("in/c/d.gif", testdata("tiny.gif")), ("in/c/r.jpg", testdata("rotated.jpg"))]
}
fn sitefile(body: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![("Sitefile", format!("PLUGIN image FROM ./plugins/image.wasm\n{body}\nEXPORT /\n").into_bytes())]
}
fn error(t: &Tmp, o: BuildOptions, code: ErrorCode) -> String {
    let e = build(&o).err().expect("expected an error");
    assert_eq!(e.code, code, "{}", e.message());
    let _ = t;
    e.message()
}

#[test]
fn the_image_plugin_over_a_directory_writes_what_one_transform_per_file_writes() {
    let _g = serial();
    let each = Tmp::new(&images());
    each.write(&sitefile(
        &[
            "TRANSFORM ./in/a.png WITH image TO /img/ OPTIONS widths=4,8 format=png,webp",
            "TRANSFORM ./in/b.jpg WITH image TO /img/ OPTIONS widths=4,8 format=png,webp",
            "TRANSFORM ./in/c/d.gif WITH image TO /img/c/ OPTIONS widths=4,8 format=png,webp",
            "TRANSFORM ./in/c/r.jpg WITH image TO /img/c/ OPTIONS widths=4,8 format=png,webp",
            "TRANSFORM ./in/b.jpg WITH image TO /t/b.jpg OPTIONS widths=5",
            "TRANSFORM ./in/c/r.jpg WITH image TO /t/c/r.jpg OPTIONS widths=5",
        ]
        .join("\n"),
    ));
    let dir = Tmp::new(&images());
    dir.write(&sitefile(
        "TRANSFORM ./in WITH image TO /img/ OPTIONS widths=4,8 format=png,webp\nTRANSFORM --include=*.jpg ./in/ WITH image TO /t/* OPTIONS widths=5",
    ));
    let a = build(&options(&each.0)).unwrap();
    let b = build(&options(&dir.0)).unwrap();
    assert_eq!(paths(&b.output), paths(&a.output));
    assert_eq!(b.output.digest(), a.output.digest());
    assert_eq!(b.transform_stats.plugins_spawned, 1);
    assert_eq!(b.transform_stats.plugin_calls, 6);
    let origin = &b.output.get("/t/c/r.jpg").unwrap().origin;
    assert!(origin.ends_with("Sitefile:3 TRANSFORM ./in/c/r.jpg"), "{origin}");
}

#[test]
fn destinations_filters_and_stage_sources() {
    let _g = serial();
    let t = Tmp::new(&images());
    t.write(&[("in/notes.txt", b"not an image".to_vec()), (".sitefileignore", b"in/b.jpg\n".to_vec())]);
    t.write(&sitefile(
        &[
            "STAGE s",
            "TRANSFORM --exclude=*.txt --exclude=c/ ./in WITH image TO /t/*.png OPTIONS widths=3",
            "FROM scratch",
            "TRANSFORM --include=c/** --exclude=*.gif ./in/ WITH image TO /webp/*.webp OPTIONS widths=3",
            "TRANSFORM --from=s / WITH image TO /again/*.jpg OPTIONS widths=2",
        ]
        .join("\n"),
    ));
    let r = build(&options(&t.0)).unwrap();
    assert_eq!(paths(&r.output), ["/again/t/a.jpg", "/webp/c/r.webp"]);
}

#[test]
fn errors_name_the_file_and_conflicts_name_both_inputs() {
    let _g = serial();
    let bad = Tmp::new(&[("in/a.png", testdata("tiny.png")), ("in/notes.txt", b"x".to_vec()), ("in/z.txt", b"y".to_vec())]);
    bad.write(&sitefile("TRANSFORM ./in/ WITH image TO /img/"));
    let m = error(&bad, options(&bad.0), ErrorCode::E_PLUGIN);
    assert!(m.contains("Sitefile:2:1: ./in/notes.txt: PLUGIN image: "), "{m}");

    let clash = Tmp::new(&[("in/x.png", testdata("tiny.png")), ("in/x.jpg", testdata("tiny.jpg"))]);
    clash.write(&sitefile("TRANSFORM --overwrite ./in/ WITH image TO /img/"));
    let m = error(&clash, options(&clash.0), ErrorCode::E_CONFLICT);
    assert!(m.contains("'/img/x.json' is written for two inputs with different content ("), "{m}");
    assert!(m.contains("Sitefile:2 TRANSFORM ./in/x.jpg and ") && m.contains("Sitefile:2 TRANSFORM ./in/x.png)"), "{m}");

    let t = Tmp::new(&images());
    for (body, code, want) in [
        ("TRANSFORM --include=*.png ./in/a.png WITH image TO /o/", ErrorCode::E_TYPE, "--include and --exclude filter a directory, but './in/a.png' is a file"),
        ("TRANSFORM ./in/a.png WITH image TO /o/*.webp", ErrorCode::E_TYPE, "a '*' name stands for each file of a directory source"),
        ("IMPORT l FROM ./l.json AS json\nTRANSFORM ./in WITH image TO /o/${i}/ FOR i IN l", ErrorCode::E_TYPE, "'./in' is a directory, which runs the plugin once per file"),
        ("TRANSFORM --exclude=x ./in WITH image TO /o/ FOR i IN l", ErrorCode::E_SYNTAX, "--exclude filters a directory source, which cannot be combined with FOR"),
        ("TRANSFORM --include=#x ./in WITH image TO /o/", ErrorCode::E_SYNTAX, "--include=#x is not a pattern"),
    ] {
        t.write(&[("l.json", b"[\"x\"]".to_vec())]);
        t.write(&sitefile(body));
        let m = error(&t, options(&t.0), code);
        assert!(m.contains(want), "{body}: {m}");
    }
}

#[test]
fn incremental_rebuilds_rerun_only_changed_files() {
    let _g = serial();
    let t = Tmp::new(&images());
    t.write(&sitefile("TRANSFORM ./in/ WITH image TO /img/ OPTIONS widths=4"));
    let memo = BuildMemo::shared();
    let with_memo = || {
        let mut o = options(&t.0);
        o.memo = Some(memo.clone());
        o
    };
    assert_eq!(build(&with_memo()).unwrap().transform_stats.plugin_calls, 4);
    assert_eq!(build(&with_memo()).unwrap().transform_stats.plugin_calls, 0);
    t.write(&[("in/b.jpg", testdata("rotated.jpg"))]);
    let edited = build(&with_memo()).unwrap();
    assert_eq!(edited.transform_stats.plugin_calls, 1);
    assert_eq!(edited.output.digest(), build(&options(&t.0)).unwrap().output.digest());
}

#[cfg(unix)]
#[test]
fn a_tool_over_a_directory() {
    let _g = serial();
    let t = Tmp::new(&[("src/a.txt", b"a\n".to_vec()), ("src/c/d.txt", b"d\n".to_vec()), ("src/e.md", b"e\n".to_vec())]);
    t.write(&[("Sitefile", b"TOOL cp\nTRANSFORM --exclude=*.md ./src/ WITH cp TO /out/*.up ARGS {input} {output}\nEXPORT /\n".to_vec())]);
    let mut o = options(&t.0);
    o.allow_exec = vec!["cp".into()];
    let r = build(&o).unwrap();
    assert_eq!(paths(&r.output), ["/out/a.up", "/out/c/d.up"]);
    assert_eq!(r.transform_stats.tool_runs, 2);
    t.write(&[("Sitefile", b"TOOL cp\nTRANSFORM ./src WITH cp TO /out ARGS {input} {output}\nEXPORT /\n".to_vec())]);
    let m = error(&t, o, ErrorCode::E_PLUGIN);
    assert!(m.contains("TOOL cp writes one file per input, but '/out' is a directory destination: use '/out/*'"), "{m}");
}
