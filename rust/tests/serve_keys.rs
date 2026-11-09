//! `serve` keys, `--open` and `--qr` (issue #224), driven through a pipe
//! (`SITEFILE_SERVE_KEYS=1`): the same session as test/serve-keys.test.ts.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join(format!("sitefile-keys-rs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    fs::canonicalize(&p).unwrap()
}

fn wait_for(path: &std::path::Path, what: &str) -> String {
    let start = Instant::now();
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(what) {
            return text;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "waiting for {what:?} in {text:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn keys_open_and_qr() {
    let d = tmp();
    fs::create_dir_all(d.join("site")).unwrap();
    fs::write(d.join("site/index.html"), "hi").unwrap();
    let browser = d.join("browser.sh");
    fs::write(&browser, format!("#!/bin/sh\necho \"$@\" >> {}/opened\n", d.display())).unwrap();
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o755)).unwrap();
    let log = d.join("out.txt");
    let mut child = Command::new(BIN)
        .args(["serve", "./site", "--open=/docs/", "--qr", "-p", "127.0.0.1:0"])
        .current_dir(&d)
        .env("SITEFILE_HOME", d.join("home"))
        .env("SITEFILE_SERVE_KEYS", "1")
        .env("BROWSER", &browser)
        .stdin(Stdio::piped())
        .stdout(fs::File::create(&log).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let text = wait_for(&log, "Press h for keys\n");
    let url = text
        .split("at ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .unwrap()
        .to_string();
    // --qr: the local URL's code, right after the Serving line.
    let qr = sitefile::qr::qr_terminal(&sitefile::qr::encode_qr(&url).unwrap());
    assert!(text.contains(&qr), "{text}");
    let opened = wait_for(&d.join("opened"), "\n");
    assert_eq!(opened, format!("{url}docs/\n"));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"u").unwrap();
    wait_for(&log, &format!("  Local:   {url}\n"));
    fs::write(d.join("site/new.html"), "new").unwrap();
    stdin.write_all(b"r").unwrap();
    wait_for(&log, "Re-read ");
    stdin.write_all(b"h").unwrap();
    wait_for(&log, "Keys: o open in the browser");
    stdin.write_all(b"q").unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "q did not stop the server");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    let mut s = String::new();
    fs::File::open(&log).unwrap().read_to_string(&mut s).unwrap();
    assert!(s.contains("Re-read ./site (2 files)\n") || s.contains("(2 files)"), "{s}");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn open_and_qr_are_foreground_only() {
    let d = tmp().join("fg");
    fs::create_dir_all(d.join("site")).unwrap();
    let r = Command::new(BIN)
        .args(["serve", "-d", "--qr", "./site"])
        .current_dir(&d)
        .env("SITEFILE_HOME", d.join("home"))
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&r.stderr).contains("--open and --qr are for a server in the foreground, not -d"));
}
