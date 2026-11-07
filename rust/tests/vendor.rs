//! `sitefile vendor` and vendored builds (port of test/vendor.test.ts, issue #96): the built binary
//! against a small loopback HTTP server.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-vendor-rs-{}-{}",
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

/// Serves /lib.js, /moved (302 to /lib.js) and /other.txt; counts requests.
fn serve() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            c.fetch_add(1, Ordering::SeqCst);
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
            let (status, extra, body) = match path.as_str() {
                "/lib.js" => ("200 OK", String::new(), "console.log(1)"),
                "/moved" => ("302 Found", "location: /lib.js\r\n".to_string(), ""),
                "/other.txt" => ("200 OK", String::new(), "other"),
                _ => ("404 Not Found", String::new(), ""),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, count)
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sitefile(home: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .args(args)
        .env("SITEFILE_HOME", home)
        .output()
        .unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into(),
        stderr: String::from_utf8_lossy(&o.stderr).into(),
    }
}

fn project(t: &Tmp, sitefile: &str) -> PathBuf {
    let ctx = t.0.join("site");
    fs::create_dir_all(ctx.join("public")).unwrap();
    fs::write(ctx.join("public/index.html"), "<h1>hi</h1>").unwrap();
    fs::write(ctx.join("Sitefile"), sitefile).unwrap();
    ctx
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

const NET: [&str; 2] = ["--allow-http", "--allow-private-network"];

#[test]
fn vendor_then_build_offline_without_network_flags() {
    let (base, count) = serve();
    let t = Tmp::new();
    let home = t.0.join("home");
    let ctx = project(&t, &format!("FROM scratch\nCOPY ./public/ /\nCOPY ./ /src/\nIMPORT lib FROM {base}/moved\nCOPY lib: /lib.js\nEXPORT /\n"));
    let v = sitefile(&home, &["vendor", s(&ctx), NET[0], NET[1]]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(
        v.stdout
            .starts_with("Vendored 1 resource and 0 images (1 blob, 14 bytes) to "),
        "{}",
        v.stdout
    );
    assert!(
        v.stdout
            .ends_with("vendor/sitefile\nUpdated Sitefile.lock\n"),
        "{}",
        v.stdout
    );
    let manifest = fs::read_to_string(ctx.join("vendor/sitefile/manifest.json")).unwrap();
    assert!(
        manifest.contains(&format!("\"resolved\": \"{base}/lib.js\"")),
        "{manifest}"
    );
    let digest = sitefile::digest::sha256(b"console.log(1)");
    let blob = ctx.join("vendor/sitefile/blobs/sha256").join(&digest[7..]);
    assert_eq!(fs::read_to_string(&blob).unwrap(), "console.log(1)");

    let n = count.load(Ordering::SeqCst);
    let out = ctx.join("dist");
    let b = sitefile(
        &home,
        &["build", s(&ctx), "-o", s(&out), "--frozen-lockfile"],
    );
    assert_eq!(b.code, 0, "{}", b.stderr);
    assert_eq!(b.stderr, "");
    assert_eq!(
        count.load(Ordering::SeqCst),
        n,
        "the build did not fetch anything"
    );
    assert_eq!(
        fs::read_to_string(out.join("lib.js")).unwrap(),
        "console.log(1)"
    );
    assert!(out.join("src/Sitefile").exists());
    assert!(
        !out.join("src/vendor").exists(),
        "the vendor directory is never copied"
    );

    // a corrupt blob fails the build
    fs::write(&blob, "tampered").unwrap();
    let c = sitefile(&home, &["build", s(&ctx), "-o", s(&out), "--force"]);
    assert_eq!(c.code, 1);
    assert!(
        c.stderr.contains("error[E_INTEGRITY]: ")
            && c.stderr.contains("Sitefile:4:1: vendored copy of '")
            && c.stderr.contains("is corrupt"),
        "{}",
        c.stderr
    );
}

#[test]
fn unvendored_urls_are_fetched_with_a_warning() {
    let (base, count) = serve();
    let t = Tmp::new();
    let home = t.0.join("home");
    let ctx = project(
        &t,
        &format!("FROM scratch\nIMPORT lib FROM {base}/lib.js\nCOPY lib: /lib.js\nEXPORT /\n"),
    );
    assert_eq!(
        sitefile(&home, &["vendor", s(&ctx), NET[0], NET[1]]).code,
        0
    );
    fs::write(ctx.join("Sitefile"), format!("FROM scratch\nIMPORT lib FROM {base}/lib.js\nIMPORT other FROM {base}/other.txt\nCOPY lib: /lib.js\nEXPORT /\n")).unwrap();
    let n = count.load(Ordering::SeqCst);
    let b = sitefile(
        &home,
        &["build", s(&ctx), "-o", s(&ctx.join("dist")), NET[0], NET[1]],
    );
    assert_eq!(b.code, 0, "{}", b.stderr);
    assert_eq!(count.load(Ordering::SeqCst), n + 1);
    assert!(
        b.stderr.contains(&format!("Sitefile:3: IMPORT other: '{base}/other.txt' is not vendored in vendor/sitefile; fetching it (run 'sitefile vendor' to add it)")),
        "{}",
        b.stderr
    );
}

#[test]
fn vendor_dir_validation_and_refusals() {
    let (base, _) = serve();
    let t = Tmp::new();
    let home = t.0.join("home");
    let ctx = project(
        &t,
        &format!("FROM scratch\nIMPORT lib FROM {base}/lib.js\nCOPY lib: /lib.js\nEXPORT /\n"),
    );
    fs::create_dir_all(ctx.join("third_party")).unwrap();
    fs::write(ctx.join("third_party/README"), "not ours").unwrap();
    for bad in ["../x", "/abs", ".", ""] {
        let r = sitefile(&home, &["vendor", s(&ctx), "--vendor-dir", bad]);
        assert_eq!(r.code, 2, "{bad}");
        assert!(
            r.stderr
                .contains("--vendor-dir expects a relative path inside the build context"),
            "{}",
            r.stderr
        );
    }
    let missing = sitefile(
        &home,
        &[
            "build",
            s(&ctx),
            "-o",
            s(&ctx.join("dist")),
            "--vendor-dir",
            "nope",
        ],
    );
    assert_eq!(missing.code, 1);
    assert!(missing.stderr.contains("error[E_NOT_FOUND]: vendor directory 'nope' has no manifest.json; run 'sitefile vendor --vendor-dir nope' first"), "{}", missing.stderr);
    let other = sitefile(
        &home,
        &[
            "vendor",
            s(&ctx),
            NET[0],
            NET[1],
            "--vendor-dir",
            "third_party",
        ],
    );
    assert_eq!(other.code, 1);
    assert!(
        other.stderr.contains(
            "error[E_EXPORT_UNSAFE]: 'third_party' exists and is not a sitefile vendor directory"
        ),
        "{}",
        other.stderr
    );
    assert_eq!(
        fs::read_to_string(ctx.join("third_party/README")).unwrap(),
        "not ours"
    );
    let outside = t.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, ctx.join("escape")).unwrap();
    let esc = sitefile(
        &home,
        &[
            "vendor",
            s(&ctx),
            NET[0],
            NET[1],
            "--vendor-dir",
            "escape/v",
        ],
    );
    assert_eq!(esc.code, 1);
    assert!(esc.stderr.contains("E_PATH_ESCAPE"), "{}", esc.stderr);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    // a custom directory is used when named
    assert_eq!(
        sitefile(
            &home,
            &[
                "vendor",
                s(&ctx),
                NET[0],
                NET[1],
                "--vendor-dir",
                "deps/offline"
            ]
        )
        .code,
        0
    );
    let b = sitefile(
        &home,
        &[
            "build",
            s(&ctx),
            "-o",
            s(&ctx.join("dist")),
            "--vendor-dir",
            "deps/offline",
        ],
    );
    assert_eq!(b.code, 0, "{}", b.stderr);
}
