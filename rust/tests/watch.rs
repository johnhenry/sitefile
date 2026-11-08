//! Watch mode, ported from test/watch.test.ts: `build --watch` and
//! `serve --watch` run as child processes on temporary contexts with a
//! temporary SITEFILE_HOME.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");
const RELOAD_SCRIPT: &str = "<script src=\"/__sitefile/reload.js\"></script>";

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-watch-rs-{}-{}",
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

/// A small site like test/helpers/cli.ts makeSite().
fn make_site(t: &Tmp, extra: &[(&str, &str)], sitefile: &str) -> PathBuf {
    let dir = t.0.join("site");
    let mut files = vec![
        (
            "public/index.html",
            "<!doctype html><title>Home</title><link rel=stylesheet href=style.css>",
        ),
        ("public/style.css", "body{margin:0}"),
        ("public/about/index.html", "<h1>About</h1>"),
    ];
    for (p, c) in extra {
        files.retain(|(q, _)| q != p);
        files.push((p, c));
    }
    for (p, c) in files {
        let f = dir.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }
    fs::write(dir.join("Sitefile"), sitefile).unwrap();
    dir
}

const DEFAULT_SITEFILE: &str = "FROM scratch\nCOPY ./public/ /\nEXPORT /\n";

struct Proc {
    child: Child,
    out: Arc<Mutex<String>>,
    err: Arc<Mutex<String>>,
}

impl Proc {
    fn start(home: &Path, args: &[&str]) -> Proc {
        let mut child = Command::new(BIN)
            .args(args)
            .env("SITEFILE_HOME", home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pump = |mut r: Box<dyn Read + Send>| {
            let buf = Arc::new(Mutex::new(String::new()));
            let b2 = buf.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = r.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    b2.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..n]));
                }
            });
            buf
        };
        let out = pump(Box::new(child.stdout.take().unwrap()));
        let err = pump(Box::new(child.stderr.take().unwrap()));
        Proc { child, out, err }
    }
    fn stdout(&self) -> String {
        self.out.lock().unwrap().clone()
    }
    fn stderr(&self) -> String {
        self.err.lock().unwrap().clone()
    }
    /// Wait until stdout+stderr contain `count` lines starting with `prefix`.
    fn wait_lines(&self, prefix: &str, count: usize) {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let all = self.stdout() + &self.stderr();
            if all.lines().filter(|l| l.starts_with(prefix)).count() >= count {
                return;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {count} x '{prefix}'\n--- stdout\n{}\n--- stderr\n{}",
                self.stdout(),
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn count(&self, prefix: &str) -> usize {
        self.stdout()
            .lines()
            .filter(|l| l.starts_with(prefix))
            .count()
    }
    fn stop(mut self) -> i32 {
        unsafe {
            kill(self.child.id() as i32, 2);
        }
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code().unwrap_or(-1);
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                return -1;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(BIN)
        .args(args)
        .env("SITEFILE_HOME", home)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Write with a fresh mtime even on coarse clocks (the poller compares size, mtime, ctime and inode).
fn write(p: impl AsRef<Path>, c: &str) {
    fs::write(p, c).unwrap();
}

#[test]
fn build_watch_rebuilds_and_keeps_the_last_good_output() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[("templates/index.html", "<title>${TITLE}</title>")], "ARG TITLE = \"Home\"\nCOPY ./public/ /\nRENDER ./templates/index.html TO /title.html\nEXPORT /\n");
    let args = site.join("args.env");
    write(&args, "TITLE=One\n");
    let w = Proc::start(
        &home,
        &[
            "build",
            "--watch",
            s(&site),
            "--build-arg-file",
            s(&args),
            "--no-cache",
        ],
    );
    w.wait_lines("Watching ", 1);
    assert!(
        w.stdout().starts_with("Exported 4 files to "),
        "{}",
        w.stdout()
    );
    assert!(w.stdout().contains(&format!(
        "Watching {} for changes (Ctrl-C to stop)\n",
        s(&site)
    )));
    let title = || fs::read_to_string(site.join("dist/title.html")).unwrap();
    assert_eq!(title(), "<title>One</title>");

    write(site.join("public/style.css"), "body{margin:1px}");
    w.wait_lines("Exported ", 2);
    assert_eq!(
        fs::read_to_string(site.join("dist/style.css")).unwrap(),
        "body{margin:1px}"
    );

    write(&args, "TITLE=Two\n");
    w.wait_lines("Exported ", 3);
    assert_eq!(title(), "<title>Two</title>");

    let sitefile = site.join("Sitefile");
    let good = fs::read_to_string(&sitefile).unwrap();
    write(&sitefile, &(good.clone() + "BOGUS instruction\n"));
    w.wait_lines("Build failed; keeping the last good output", 1);
    assert!(
        w.stderr()
            .lines()
            .any(|l| l.starts_with("error[E_") && l.contains("Sitefile:5:1")),
        "{}",
        w.stderr()
    );
    assert_eq!(title(), "<title>Two</title>");
    let temps: Vec<_> = fs::read_dir(&site)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("sitefile-"))
        .collect();
    assert!(temps.is_empty(), "no temp siblings left");

    write(&sitefile, &good.replace("COPY", "# changed\nCOPY"));
    w.wait_lines("Exported ", 4);

    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(w.count("Exported "), 4, "no rebuild loop:\n{}", w.stdout());
    assert_eq!(w.count("Change detected, rebuilding"), 4);
    fs::create_dir_all(site.join(".git")).unwrap();
    write(site.join(".git/HEAD"), "ref: x\n");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(w.count("Exported "), 4, ".git changes are ignored");
    let err = w.stderr();
    assert_eq!(w.stop(), 0, "exit code after Ctrl-C\n{err}");
}

#[test]
fn build_watch_a_burst_is_one_rebuild() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[], DEFAULT_SITEFILE);
    let w = Proc::start(&home, &["build", "--watch", s(&site)]);
    w.wait_lines("Watching ", 1);
    for i in 0..5 {
        write(site.join(format!("public/f{i}.txt")), &i.to_string());
    }
    w.wait_lines("Exported 8 files", 1);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(w.count("Exported "), 2, "{}", w.stdout());
    w.stop();
}

#[test]
fn watch_usage_errors() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[], DEFAULT_SITEFILE);
    let (code, _, err) = run(&home, &["validate", "--watch", s(&site)]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("error: --watch is only for build (and serve --watch), not validate\n"),
        "{err}"
    );
    let (code, _, err) = run(&home, &["build", "--watch", "-t", "x", s(&site)]);
    assert_eq!(code, 2);
    assert!(
        err.starts_with("error: --watch cannot be combined with -t\n"),
        "{err}"
    );
    let missing = site.join("nope");
    let (code, _, err) = run(&home, &["build", "--watch", s(&missing)]);
    assert_eq!(code, 1);
    assert_eq!(
        err,
        format!("error[E_NOT_FOUND]: '{}' is not a directory\n", s(&missing))
    );
    let (code, _, err) = run(&home, &["build", "--watch", s(&site), "--build-arg", "=x"]);
    assert_eq!(code, 2, "a usage error in the first build ends the watch");
    assert!(
        err.starts_with("error: --build-arg expects NAME=value, got '=x'\n"),
        "{err}"
    );
    let (code, _, err) = run(&home, &["serve", "--watch", "-d", s(&site)]);
    assert_eq!(code, 2);
    assert!(err.contains("--watch cannot be combined with -d"), "{err}");
    let (code, _, err) = run(&home, &["serve", "--build-arg", "A=1", "./x/"]);
    assert_eq!(code, 2);
    assert!(err.contains("--build-arg needs --watch"), "{err}");
    let (code, _, err) = run(&home, &["serve", "--keep-service-workers", "./x/"]);
    assert_eq!(code, 2);
    assert!(
        err.contains("--keep-service-workers needs --watch"),
        "{err}"
    );
}

/// GET `path` as a page navigation; returns (status, headers lowercased, body).
fn get(port: u16, path: &str) -> (u16, String, String) {
    get_with(
        port,
        path,
        "sec-fetch-mode: navigate\r\nsec-fetch-dest: document\r\n",
    )
}

/// GET `path` with `extra` header lines (each ending in CRLF).
fn get_with(port: u16, path: &str, extra: &str) -> (u16, String, String) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        c,
        "GET {path} HTTP/1.1\r\nhost: x\r\n{extra}connection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = String::new();
    c.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head.to_ascii_lowercase(), body.to_string())
}

/// Open the event stream; the returned reader yields events (text before a blank line).
struct Events(TcpStream, String);
impl Events {
    fn open(port: u16) -> Events {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(c, "GET /__sitefile/reload HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut e = Events(c, String::new());
        let head = e.until("\r\n\r\n");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "{head}"
        );
        e
    }
    fn until(&mut self, sep: &str) -> String {
        loop {
            if let Some(i) = self.1.find(sep) {
                let ev = self.1[..i].to_string();
                self.1 = self.1[i + sep.len()..].to_string();
                return ev;
            }
            let mut chunk = [0u8; 1024];
            let n = self.0.read(&mut chunk).expect("event stream read");
            assert!(n > 0, "event stream closed");
            self.1.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }
    fn next(&mut self) -> String {
        self.until("\n\n")
    }
}

#[test]
fn serve_watch_live_reloads_and_keeps_serving_on_failure() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(
        &t,
        &[("public/index.html", "<html><body><h1>v1</h1></body></html>")],
        DEFAULT_SITEFILE,
    );
    let w = Proc::start(&home, &["serve", "--watch", s(&site)]);
    w.wait_lines("Watching ", 1);
    let stdout = w.stdout();
    let mut lines = stdout.lines();
    let built = lines.next().unwrap();
    assert!(
        built.starts_with("Built 3 files (sha256:") && built.ends_with(')'),
        "{stdout}"
    );
    let serving = lines.next().unwrap();
    let prefix = format!("Serving {} (3 files) at http://127.0.0.1:", s(&site));
    assert!(
        serving.starts_with(&prefix) && serving.ends_with("/  (Ctrl-C to stop)"),
        "{stdout}"
    );
    let port: u16 = serving[prefix.len()..]
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    let (status, head, body) = get(port, "/");
    assert_eq!(status, 200);
    assert_eq!(
        body,
        format!("<html><body><h1>v1</h1>{RELOAD_SCRIPT}</body></html>")
    );
    assert!(head.contains(&format!("content-length: {}", body.len())));
    assert!(head.contains("-live\""), "{head}");
    assert_eq!(get(port, "/style.css").2, "body{margin:0}");
    assert_eq!(
        get_with(port, "/", "sec-fetch-mode: cors\r\naccept: */*\r\n").2,
        "<html><body><h1>v1</h1></body></html>",
        "a fetch() of HTML is untouched"
    );
    let (status, head, js) = get_with(port, "/__sitefile/reload.js", "");
    assert_eq!(status, 200);
    assert!(
        head.contains("content-type: text/javascript; charset=utf-8"),
        "{head}"
    );
    assert!(js.contains("new EventSource("), "{js}");

    let mut ev = Events::open(port);
    assert_eq!(ev.next(), "retry: 1000");
    write(site.join("public/index.html"), "<h1>v2</h1>");
    assert_eq!(ev.next(), "data: {\"type\":\"building\"}");
    assert_eq!(ev.next(), "data: {\"type\":\"reload\"}");
    assert_eq!(get(port, "/").2, format!("<h1>v2</h1>{RELOAD_SCRIPT}"));
    // only a stylesheet of the build changed: pages swap it in place
    write(site.join("public/style.css"), "body{margin:1px}");
    assert_eq!(ev.next(), "data: {\"type\":\"building\"}");
    assert_eq!(
        ev.next(),
        "data: {\"type\":\"css\",\"paths\":[\"/style.css\"]}"
    );

    write(
        site.join("Sitefile"),
        "FROM scratch\nCOPY ./missing/ /\nEXPORT /\n",
    );
    w.wait_lines("Build failed; keeping the last good output", 1);
    assert!(w.stderr().contains("error[E_"), "{}", w.stderr());
    assert_eq!(get(port, "/").2, format!("<h1>v2</h1>{RELOAD_SCRIPT}"));
    assert!(
        !site.join("dist").exists(),
        "serve --watch writes no export"
    );
    assert!(
        !w.stdout().contains("/__sitefile/"),
        "the event stream and the script are not access-logged"
    );
    drop(ev);
    assert_eq!(w.stop(), 0);
}

/// The port from a `Serving ... at http://127.0.0.1:PORT/` line.
fn port_of(stdout: &str) -> u16 {
    let at =
        stdout.find("at http://127.0.0.1:").expect("Serving line") + "at http://127.0.0.1:".len();
    stdout[at..].split('/').next().unwrap().parse().unwrap()
}

#[test]
fn serve_watch_plain_directory_is_reread_and_swaps_css() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let dir = t.0.join("plain");
    fs::create_dir_all(&dir).unwrap();
    write(
        dir.join("index.html"),
        "<link rel=\"stylesheet\" href=\"/s.css\"><h1>p1</h1>",
    );
    write(dir.join("s.css"), "a{}");
    write(dir.join(".env"), "SECRET=1");
    let w = Proc::start(&home, &["serve", "--watch", s(&dir)]);
    w.wait_lines("Watching ", 1);
    let stdout = w.stdout();
    assert!(stdout.starts_with("Read 2 files (sha256:"), "{stdout}");
    assert!(stdout.contains(" (2 files) at "), "{stdout}");
    let port = port_of(&stdout);
    assert_eq!(
        get(port, "/").2,
        format!("<link rel=\"stylesheet\" href=\"/s.css\"><h1>p1</h1>{RELOAD_SCRIPT}")
    );
    assert_eq!(get(port, "/.env").0, 404);
    let mut ev = Events::open(port);
    assert_eq!(ev.next(), "retry: 1000");
    write(dir.join("s.css"), "a{color:red}");
    assert_eq!(ev.next(), "data: {\"type\":\"building\"}");
    assert_eq!(ev.next(), "data: {\"type\":\"css\",\"paths\":[\"/s.css\"]}");
    write(dir.join("index.html"), "<h1>p2</h1>");
    assert_eq!(ev.next(), "data: {\"type\":\"building\"}");
    assert_eq!(ev.next(), "data: {\"type\":\"reload\"}");
    w.wait_lines("Change detected, re-reading", 2);
    assert_eq!(get(port, "/").2, format!("<h1>p2</h1>{RELOAD_SCRIPT}"));
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [".env", "index.html", "s.css"]);
    drop(ev);
    assert_eq!(w.stop(), 0);
}

#[test]
fn serve_watch_no_css_inject_reloads() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[("public/index.html", "<h1>x</h1>")], DEFAULT_SITEFILE);
    let w = Proc::start(&home, &["serve", "--watch", "--no-css-inject", s(&site)]);
    w.wait_lines("Watching ", 1);
    let port = port_of(&w.stdout());
    assert!(get(port, "/__sitefile/reload.js")
        .2
        .ends_with("\"cssInject\":false,\"console\":false,\"overlay\":true});\n"));
    let mut ev = Events::open(port);
    assert_eq!(ev.next(), "retry: 1000");
    write(site.join("public/style.css"), "body{margin:3px}");
    assert_eq!(ev.next(), "data: {\"type\":\"building\"}");
    assert_eq!(ev.next(), "data: {\"type\":\"reload\"}");
    drop(ev);
    assert_eq!(w.stop(), 0);
}

#[test]
fn build_watch_a_failing_first_build_keeps_watching() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[], "FROM scratch\nCOPY ./nope/ /\nEXPORT /\n");
    let w = Proc::start(&home, &["build", "--watch", s(&site)]);
    w.wait_lines("Watching ", 1);
    assert!(w.stderr().starts_with("error[E_"), "{}", w.stderr());
    write(site.join("Sitefile"), DEFAULT_SITEFILE);
    w.wait_lines("Exported 3 files", 1);
    assert_eq!(w.stop(), 0);
}

#[test]
fn build_watch_poll_interval_sees_changes_and_poll_needs_watch() {
    let t = Tmp::new();
    let home = t.0.join("home");
    let site = make_site(&t, &[], DEFAULT_SITEFILE);
    let w = Proc::start(
        &home,
        &["build", "--watch", "--poll-interval", "50", s(&site)],
    );
    w.wait_lines("Watching ", 1);
    write(site.join("public/style.css"), "body{margin:2px}");
    w.wait_lines("Exported ", 2);
    assert_eq!(
        fs::read_to_string(site.join("dist/style.css")).unwrap(),
        "body{margin:2px}"
    );
    assert_eq!(w.stop(), 0);
    let (code, _, err) = run(&home, &["build", "--poll", s(&site)]);
    assert_eq!(code, 2);
    assert!(err.contains("--poll needs --watch"), "{err}");
    let (code, _, err) = run(
        &home,
        &["build", "--watch", "--poll-interval", "soon", s(&site)],
    );
    assert_eq!(code, 2);
    assert!(
        err.contains(
            "--poll-interval expects a whole number of milliseconds (10 or more), got 'soon'"
        ),
        "{err}"
    );
    let (code, _, err) = run(&home, &["serve", "--poll", "./x/"]);
    assert_eq!(code, 2);
    assert!(err.contains("--poll needs --watch"), "{err}");
}

#[test]
fn build_watch_rebuilds_incrementally() {
    let t = Tmp::new();
    let home = t.0.join("home");
    fs::create_dir_all(&home).unwrap();
    let site = make_site(
        &t,
        &[("templates/t.html", "<p>${TITLE}</p>")],
        "ARG TITLE = \"T\"\nCOPY ./public/ /\nRENDER ./templates/t.html TO /t.html\nEXPORT /\n",
    );
    let trace = home.join("trace.json");
    let memo = || -> Vec<String> {
        let Ok(text) = fs::read_to_string(&trace) else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["cat"] == "operation")
            .map(|e| e["args"]["memo"].as_str().unwrap_or("-").to_string())
            .collect()
    };
    let w = Proc::start(
        &home,
        &["build", "--watch", s(&site), "--profile", s(&trace)],
    );
    w.wait_lines("Watching ", 1);
    assert_eq!(memo(), ["miss", "miss"]);
    write(site.join("public/style.css"), "body{margin:2px}");
    w.wait_lines("Exported ", 2);
    // COPY read a changed file; RENDER read nothing that changed, so its file is reused.
    // (The trace is written just after the Exported line.)
    let end = Instant::now() + Duration::from_secs(10);
    while memo() != ["miss", "files"] && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(memo(), ["miss", "files"]);
    assert_eq!(
        fs::read_to_string(site.join("dist/t.html")).unwrap(),
        "<p>T</p>"
    );
    assert_eq!(
        fs::read_to_string(site.join("dist/style.css")).unwrap(),
        "body{margin:2px}"
    );
    assert_eq!(w.stop(), 0);
}
