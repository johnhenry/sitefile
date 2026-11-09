//! `sitefile outdated` for remote imports (port of test/outdated.test.ts, issue #104): the built binary
//! against a small loopback HTTP server whose bodies change between runs.

use std::collections::HashMap;
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
            "sitefile-outdated-rs-{}-{}",
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

type Bodies = Arc<Mutex<HashMap<String, String>>>;

fn serve(bodies: Bodies) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let body = bodies.lock().unwrap().get(&path).cloned();
            let (status, body) = match body {
                Some(b) => ("200 OK", b),
                None => ("404 Not Found", String::new()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

fn sitefile(home: &Path, args: &[&str]) -> (i32, String, String) {
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

fn sha(s: &str) -> String {
    sitefile::digest::sha256(s.as_bytes())
}

#[test]
fn imports_current_changed_unpinned_and_errors_without_writing() {
    let bodies: Bodies = Arc::new(Mutex::new(
        [("/a.js", "a1"), ("/b.js", "b1"), ("/c.js", "c1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    let base = serve(bodies.clone());
    let t = Tmp::new();
    let home = t.0.join("home");
    let ctx = t.0.join("site");
    fs::create_dir_all(&ctx).unwrap();
    fs::write(
        ctx.join("Sitefile"),
        format!("FROM scratch\nIMPORT a FROM {base}/a.js INTEGRITY {}\nIMPORT b FROM {base}/b.js\nIMPORT c FROM {base}/c.js\nIMPORT gone FROM {base}/gone.js\nEXPORT /\n", sha("a1")),
    )
    .unwrap();
    let lock = format!("{{\"version\":1,\"resources\":{{\"{base}/b.js\":{{\"integrity\":\"{}\",\"size\":2,\"resolved\":\"{base}/b.js\"}}}}}}", sha("b1"));
    fs::write(ctx.join("Sitefile.lock"), &lock).unwrap();
    let net = ["--allow-http", "--allow-private-network"];
    let c = ctx.to_str().unwrap();

    let (code, out, err) = sitefile(&home, &["outdated", c, net[0], net[1]]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.ends_with("4 remote inputs: 1 unpinned, 1 error, 2 current\n"),
        "{out}"
    );

    bodies.lock().unwrap().insert("/a.js".into(), "a2".into());
    bodies.lock().unwrap().insert("/b.js".into(), "b2".into());
    let (code, out, _) = sitefile(&home, &["outdated", c, net[0], net[1]]);
    assert_eq!(code, 0);
    let h = |s: &str| sha(s)[7..19].to_string();
    let pad = " ".repeat(base.len() + 8 - 6);
    let want = [
        format!(
            "KIND    NAME  SOURCE{pad}  PINNED        BY         LATEST        STATUS    NEWER TAG"
        ),
        format!(
            "import  a     {base}/a.js     {}  integrity  {}  changed   -",
            h("a1"),
            h("a2")
        ),
        format!(
            "import  b     {base}/b.js     {}  lockfile   {}  changed   -",
            h("b1"),
            h("b2")
        ),
        format!(
            "import  c     {base}/c.js     -             -          {}  unpinned  -",
            h("c1")
        ),
        format!("import  gone  {base}/gone.js  -             -          -             error     -"),
        format!("gone: error[E_FETCH_STATUS]: GET {base}/gone.js returned HTTP 404"),
        "4 remote inputs: 2 changed, 1 unpinned, 1 error".to_string(),
        String::new(),
    ]
    .join("\n");
    assert_eq!(out, want);

    let (_, json, _) = sitefile(&home, &["outdated", c, net[0], net[1], "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["inputs"][1]["status"], "changed");
    assert_eq!(v["inputs"][1]["latest"], sha("b2"));
    assert_eq!(v["inputs"][3]["error"]["code"], "E_FETCH_STATUS");
    assert_eq!(v["summary"]["changed"], 2);
    let keys: Vec<&String> = v["inputs"][0].as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["kind", "name", "source", "pin", "pinnedBy", "latest", "status", "newerTag", "error"]
    );

    // never writes: the lockfile is unchanged and no store was created
    assert_eq!(fs::read_to_string(ctx.join("Sitefile.lock")).unwrap(), lock);
    assert!(!home.join("index.json").exists());

    // no remote inputs
    fs::write(ctx.join("Sitefile"), "FROM scratch\nEXPORT /\n").unwrap();
    let (_, out, _) = sitefile(&home, &["outdated", c]);
    assert_eq!(out, "No remote inputs: the Sitefile has no remote IMPORT, no registry FROM image and no BASE image\n");
}
