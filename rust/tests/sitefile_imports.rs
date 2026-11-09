//! `IMPORT … AS sitefile` (issue #97), ported from test/sitefile-import.test.ts (local imports; the Git
//! ones are covered by the net-sitefile-import-* conformance cases).

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
            "sitefile-sfimport-rs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(sitefile::platform::canonicalize(&p).unwrap())
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
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
        .current_dir(&work.0)
        .env("SITEFILE_HOME", work.0.join(".home"))
        .output()
        .unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

const THEME: &str = "ARG ACCENT = \"teal\"\nSTAGE assets\nRENDER ./a.css TO /a.css\nSTAGE layout\nCOPY ./page.html /page.html\nFROM scratch\nEXPORT /\n";

fn theme(work: &Tmp) {
    work.write("theme/Sitefile", THEME);
    work.write("theme/a.css", "h1 { color: ${ACCENT} }\n");
    work.write("theme/page.html", "<h1>${TITLE}</h1>\n");
}

#[test]
fn stages_of_a_local_sitefile_with_its_own_args_and_origins() {
    let work = Tmp::new();
    theme(&work);
    work.write(
        "Sitefile",
        "ARG ACCENT = \"red\"\nARG TITLE = \"Site\"\nIMPORT theme FROM ./theme AS sitefile ARG ACCENT=${ACCENT}\nSTAGE site FROM theme+assets\nRENDER --escape=html --from=theme+layout /page.html TO /index.html\nEXPORT /\n",
    );
    let r = sitefile(&work, &["build", ".", "-o", "out"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        fs::read_to_string(work.0.join("out/a.css")).unwrap(),
        "h1 { color: red }\n"
    );
    assert_eq!(
        fs::read_to_string(work.0.join("out/index.html")).unwrap(),
        "<h1>Site</h1>\n"
    );
    let i = sitefile(&work, &["inspect", "."]);
    assert!(
        i.stdout.contains("/a.css  (theme/Sitefile:3 RENDER)"),
        "{}",
        i.stdout
    );
    // The theme's defaults apply without ARG clauses; --build-args never reach it.
    work.write(
        "Sitefile",
        "IMPORT theme FROM ./theme AS sitefile\nFROM theme+assets\nEXPORT /\n",
    );
    let r = sitefile(
        &work,
        &["build", ".", "-o", "out", "--build-arg", "ACCENT=blue"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        fs::read_to_string(work.0.join("out/a.css")).unwrap(),
        "h1 { color: teal }\n"
    );
    assert!(r
        .stderr
        .contains("--build-arg ACCENT does not match any ARG and was ignored"));
}

#[test]
fn unknown_stages_args_cycles_and_the_sandbox() {
    let work = Tmp::new();
    theme(&work);
    let err = |sitefile_text: &str| -> String {
        work.write("Sitefile", sitefile_text);
        let r = sitefile(&work, &["build", ".", "-o", "out"]);
        assert_ne!(r.code, 0);
        r.stderr
            .lines()
            .find(|l| l.starts_with("error"))
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(
        err("IMPORT t FROM ./theme AS sitefile\nFROM t+nope\nEXPORT /\n"),
        "error[E_UNDEFINED]: Sitefile:2:1: t+nope: theme/Sitefile has no stage 'nope' (stages: assets, layout)"
    );
    assert_eq!(
        err("IMPORT t FROM ./theme AS sitefile ARG NOPE=1\nFROM t+assets\nEXPORT /\n"),
        "error[E_UNDEFINED]: Sitefile:1:35: IMPORT t: theme/Sitefile has no ARG NOPE"
    );
    assert_eq!(
        err("IMPORT me FROM ./ AS sitefile\nFROM me+x\nEXPORT /\n"),
        "error[E_VALIDATION]: Sitefile:1:1: IMPORT me: Sitefile import cycle: Sitefile -> Sitefile"
    );
    work.write(
        "theme/Sitefile",
        "STAGE assets\nCOPY ../Sitefile /x\nEXPORT /\n",
    );
    assert_eq!(
        err("IMPORT t FROM ./theme AS sitefile\nFROM t+assets\nEXPORT /\n"),
        "error[E_PATH_ESCAPE]: theme/Sitefile:2:6: local path '../Sitefile' escapes the build context"
    );
}

#[test]
fn the_shared_stages_example_builds_and_validates_cleanly() {
    let work = Tmp::new();
    let ex = repo_root().join("examples/shared-stages");
    let r = sitefile(
        &work,
        &[
            "build",
            ex.to_str().unwrap(),
            "-o",
            work.0.join("out").to_str().unwrap(),
        ],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(fs::read_to_string(work.0.join("out/assets/theme.css"))
        .unwrap()
        .contains("--accent: rebeccapurple"));
    let v = sitefile(&work, &["validate", ex.to_str().unwrap()]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(v.stderr.is_empty(), "{}", v.stderr);
}
