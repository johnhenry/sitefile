//! CHECKLINKS (port of test/checklinks.test.ts, issue #99): the built binary on small sites, with a
//! loopback HTTP server for `--external`. Messages are the TypeScript implementation's, byte for byte.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-links-rs-{}-{}",
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

/// Serves /ok (200), /moved (302 to /ok) and 404 otherwise; records each request line.
fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or("").to_string(),
                parts.next().unwrap_or("/").to_string(),
            );
            l.lock().unwrap().push(format!("{method} {path}"));
            let (status, extra, body) = match path.as_str() {
                "/ok" => ("200 OK", "", "fine"),
                "/moved" => ("302 Found", "location: /ok\r\n", ""),
                _ => ("404 Not Found", "", ""),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, log)
}

fn build(home: &Path, ctx: &Path, extra: &[&str]) -> (i32, String) {
    let out = ctx.join("dist");
    let mut args = vec![
        "build",
        ctx.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--no-lockfile",
    ];
    args.extend_from_slice(extra);
    let o = Command::new(BIN)
        .args(&args)
        .current_dir(ctx)
        .env("SITEFILE_HOME", home)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

fn site(t: &Tmp, files: &[(&str, &str)]) -> PathBuf {
    let ctx = t.0.join("site");
    for (p, c) in files {
        let f = ctx.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }
    ctx
}

fn warnings(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .map(str::to_string)
        .collect()
}

const INDEX: &str = "<link rel=stylesheet href=\"css/site.css\">\n<a href=\"about\">About</a> <a href=\"docs/\">Docs</a> <a href=\"blog/\">Blog</a>\n<img src=\"/img/missing.png\"> <a href=\"#top\">t</a> <a href=\"mailto:a@b\">m</a>\n<a href=\"old/page\">old</a> <a href=\"https://example.com/\">ext</a> <a href=\"nope.html?x#y\">nope</a>\n";

fn files(sitefile: &str) -> Vec<(&str, &str)> {
    vec![
        ("public/index.html", INDEX),
        ("public/about.html", "about"),
        (
            "public/docs/index.html",
            "<a href=\"../index.html\">home</a>",
        ),
        ("public/blog/post.html", ""),
        (
            "public/css/site.css",
            "a { background: url(../img/bg.png) } @import \"gone.css\";",
        ),
        ("Sitefile", sitefile),
    ]
}

#[test]
fn broken_internal_links_fail_the_build_with_file_and_line() {
    let t = Tmp::new();
    let ctx = site(
        &t,
        &files("COPY ./public/ /\nCHECKLINKS\nREDIRECT /old/* /new/:splat\nEXPORT /\n"),
    );
    let (code, stderr) = build(&t.0, &ctx, &[]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains(
            "error[E_BROKEN_LINK]: Sitefile:2:1: CHECKLINKS: 5 broken links:\n  /css/site.css:1: ../img/bg.png (not found: /img/bg.png)\n  /css/site.css:1: gone.css (not found: /css/gone.css)\n  /index.html:2: blog/ (not found: /blog/)\n  /index.html:3: /img/missing.png (not found: /img/missing.png)\n  /index.html:4: nope.html?x#y (not found: /nope.html)"
        ),
        "{stderr}"
    );
}

#[test]
fn warn_prints_each_broken_link_and_keeps_building() {
    let t = Tmp::new();
    let ctx = site(
        &t,
        &files("COPY ./public/ /\nCHECKLINKS --warn\nEXPORT /\n"),
    );
    let (code, stderr) = build(&t.0, &ctx, &[]);
    assert_eq!(code, 0, "{stderr}");
    let w = warnings(&stderr);
    assert_eq!(w.len(), 6, "{w:?}");
    assert_eq!(
        w[4],
        "Sitefile:2 CHECKLINKS: broken link /index.html:4: old/page (not found: /old/page)"
    );
}

#[test]
fn external_links_are_requested_once_under_the_network_policy() {
    let (base, log) = serve();
    let t = Tmp::new();
    let page = format!("<a href=\"{base}/ok\">a</a><a href=\"{base}/moved\">b</a><a href=\"{base}/gone\">c</a><a href=\"{base}/ok\">d</a>");
    let ctx = site(
        &t,
        &[
            ("index.html", page.as_str()),
            (
                "Sitefile",
                "COPY ./index.html /\nCHECKLINKS --warn --external\nEXPORT /\n",
            ),
        ],
    );
    let (code, stderr) = build(&t.0, &ctx, &[]);
    assert_eq!(code, 0, "{stderr}");
    let blocked: Vec<String> = ["ok", "moved", "gone", "ok"]
        .iter()
        .map(|p| format!("Sitefile:2 CHECKLINKS: broken link /index.html:1: {base}/{p} (plain http is checked only with --allow-http)"))
        .collect();
    assert_eq!(warnings(&stderr), blocked);
    assert!(log.lock().unwrap().is_empty());
    let (code, stderr) = build(&t.0, &ctx, &["--allow-http", "--allow-private-network"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        warnings(&stderr),
        [format!(
            "Sitefile:2 CHECKLINKS: broken link /index.html:1: {base}/gone (HTTP 404)"
        )]
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["GET /ok", "GET /moved", "GET /ok", "GET /gone"]
    );
}
