//! Build secrets (issue #85) end to end with the CLI: `--secret` + `IMPORT … SECRET`. The value must
//! reach only the import's origin, and never the output, Sitefile.lock, the vendor directory,
//! provenance, the --profile trace, SITEFILE_HOME, stdout/stderr or error messages.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");
const SECRET: &str = "tok-91b2-SECRET-VALUE";

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("sitefile-secret-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
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

type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>)>>>; // server, path, headers

/// A tiny HTTP/1.1 server; `respond(path, headers)` returns (status line, extra headers, body).
fn serve(name: &'static str, seen: Seen, respond: impl Fn(&str, &[(String, String)]) -> (String, Vec<(String, String)>, String) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut headers = Vec::new();
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
                if let Some((k, v)) = l.trim_end().split_once(':') {
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            seen.lock().unwrap().push((name.to_string(), path.clone(), headers.clone()));
            let (status, extra, body) = respond(&path, &headers);
            let mut head = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
            for (k, v) in extra {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            let _ = write!(stream, "{head}\r\n{body}");
        }
    });
    base
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
    h.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(BIN).args(args).env("SITEFILE_HOME", home).env("API_TOKEN", SECRET).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

fn grep(dir: &Path, needle: &[u8], hits: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            grep(&p, needle, hits);
        } else if fs::read(&p).map(|b| b.windows(needle.len()).any(|w| w == needle)).unwrap_or(false) {
            hits.push(p);
        }
    }
}

#[test]
fn secrets_reach_only_their_origin_and_are_never_recorded() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let other = serve("other", seen.clone(), |_, _| ("200 OK".into(), vec![("content-type".into(), "text/plain".into())], "other origin\n".into()));
    let other2 = other.clone();
    let main = serve("main", seen.clone(), move |path, h| {
        let ok = header(h, "x-api-key") == Some(SECRET) || header(h, "authorization") == Some(&format!("Bearer {SECRET}"));
        match path {
            "/same" => ("302 Found".into(), vec![("location".into(), "/data.json".into())], String::new()),
            "/away" => ("302 Found".into(), vec![("location".into(), format!("{other2}/landing"))], String::new()),
            "/leak" => ("302 Found".into(), vec![("location".into(), format!("/missing?k={}", header(h, "x-api-key").unwrap_or("")))], String::new()),
            "/data.json" if ok => ("200 OK".into(), vec![("content-type".into(), "application/json".into())], "{\"title\":\"private data\"}".into()),
            "/data.json" => ("401 Unauthorized".into(), vec![], String::new()),
            _ => ("404 Not Found".into(), vec![], String::new()),
        }
    });
    let ctx = Tmp::new();
    let home = Tmp::new();
    let out = Tmp::new();
    let tok = Tmp::new();
    let tok_file = tok.0.join("tok");
    fs::write(&tok_file, format!("{SECRET}\n")).unwrap();
    fs::write(
        ctx.0.join("Sitefile"),
        format!(
            "FROM scratch\nIMPORT d FROM {main}/same SECRET k AS HEADER X-Api-Key AS json\nIMPORT b FROM {main}/data.json AS json SECRET t AS BEARER\nIMPORT o FROM {main}/away SECRET k AS HEADER X-Api-Key AS text\nRENDER <<T TO /i.txt\n${{d.title}} ${{b.title}} ${{o}}\nT\nEXPORT / AS oci\n"
        ),
    )
    .unwrap();
    let c = ctx.0.to_str().unwrap();
    let src = format!("id=t,src={}", tok_file.display());
    let sec = ["--secret", "id=k,env=API_TOKEN", "--secret", src.as_str()];
    let o = |p: &str| out.0.join(p).to_string_lossy().into_owned();
    let (tar, trace, dir) = (o("site.oci.tar"), o("trace.json"), o("dir"));
    let runs: Vec<Vec<&str>> = vec![
        [&["vendor", c, "--allow-http", "--allow-private-network"][..], &sec].concat(),
        [&["build", c, "--allow-http", "--allow-private-network", "-o", &tar, "--provenance", "--sbom", "--profile", &trace, "--update-lock"][..], &sec].concat(),
        [&["build", c, "--allow-http", "--allow-private-network", "-o", &dir, "--format", "directory", "--secret", "id=k,env=API_TOKEN", "--secret", &src, "--secret", "id=extra,env=API_TOKEN"][..]].concat(),
        [&["outdated", c, "--allow-http", "--allow-private-network", "--json"][..], &sec].concat(),
        vec!["validate", c, "--json"],
    ];
    for args in &runs {
        let (code, stdout, stderr) = run(&home.0, args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET), "{stdout}{stderr}");
        if args[0] == "build" && args.contains(&"id=extra,env=API_TOKEN") {
            assert!(stderr.contains("warning: --secret extra is not used by any IMPORT … SECRET"), "{stderr}");
        }
    }
    assert_eq!(fs::read_to_string(out.0.join("dir/i.txt")).unwrap(), "private data private data other origin\n\n");
    // Same-origin hops carry the secret; the other origin never sees it.
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().any(|(s, p, h)| s == "main" && p == "/data.json" && header(h, "x-api-key") == Some(SECRET)));
    let landings: Vec<_> = seen.iter().filter(|(s, _, _)| s == "other").collect();
    assert!(!landings.is_empty());
    for (_, _, h) in landings {
        assert!(header(h, "x-api-key").is_none() && header(h, "authorization").is_none(), "{h:?}");
    }
    for d in [&ctx.0, &out.0, &home.0] {
        let mut hits = Vec::new();
        grep(d, SECRET.as_bytes(), &mut hits);
        assert!(hits.is_empty(), "secret found in {hits:?}");
    }
    // A missing secret is E_SECRET; a reflected secret is redacted from the error.
    fs::write(ctx.0.join("Sitefile"), format!("FROM scratch\nIMPORT d FROM {main}/leak SECRET k AS HEADER X-Api-Key\nEXPORT /\n")).unwrap();
    let (code, _, stderr) = run(&home.0, &["build", c, "--allow-http", "--allow-private-network", "-o", &o("x")]);
    assert_eq!(code, 1);
    assert!(stderr.contains("error[E_SECRET]") && stderr.contains("secret 'k' was not given"), "{stderr}");
    let (code, stdout, stderr) = run(&home.0, &["build", c, "--allow-http", "--allow-private-network", "-o", &o("x"), "--secret", "id=k,env=API_TOKEN"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("error[E_FETCH_STATUS]") && stderr.contains("/missing?k=***"), "{stderr}");
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
    // A malformed flag is a usage error that does not echo it.
    let (code, _, stderr) = run(&home.0, &["build", c, "--secret", SECRET]);
    assert_eq!(code, 2);
    assert!(!stderr.contains(SECRET), "{stderr}");
}
