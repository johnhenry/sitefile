//! CLI integration tests, ported from test/cli.test.ts: they run the built
//! `sitefile` binary on the examples in a temporary copy.

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
        let p = std::env::temp_dir().join(format!("sitefile-cli-rs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(fs::canonicalize(&p).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
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

/// A temporary copy of `examples/<name>`.
fn example(work: &Tmp, name: &str) -> PathBuf {
    let ctx = work.path().join(name);
    copy_dir(&repo_root().join("examples").join(name), &ctx);
    let _ = fs::remove_dir_all(ctx.join("dist"));
    ctx
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sitefile(work: &Tmp, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .args(args)
        .current_dir(repo_root())
        .env("SITEFILE_HOME", work.path().join(".home"))
        .output()
        .expect("run sitefile");
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into(), stderr: String::from_utf8_lossy(&o.stderr).into() }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn read(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).unwrap()
}

#[test]
fn mvp_acceptance_hello_builds_dist_with_title_and_css() {
    let work = Tmp::new();
    let ctx = example(&work, "hello");
    let r = sitefile(&work, &["build", s(&ctx)]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Exported 2 files"), "{}", r.stdout);
    let html = read(ctx.join("dist/index.html"));
    assert!(html.contains("<title>Hello</title>"));
    assert!(html.contains("Welcome to Hello."));
    assert_eq!(read(ctx.join("dist/style.css")), read(repo_root().join("examples/hello/public/style.css")));

    let again = sitefile(&work, &["build", s(&ctx), "--build-arg", "TITLE=<Sitefile>"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert!(read(ctx.join("dist/index.html")).contains("<title>&lt;Sitefile&gt;</title>"));
}

#[test]
fn version_and_help() {
    let work = Tmp::new();
    let v = sitefile(&work, &["--version"]);
    assert_eq!((v.code, v.stdout.as_str()), (0, "0.1.0\n"));
    let h = sitefile(&work, &["--help"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.starts_with("sitefile 0.1.0 — Docker-inspired static artifact builder"));
    let none = sitefile(&work, &[]);
    assert_eq!(none.code, 2);
}

#[test]
fn validate_and_inspect() {
    let work = Tmp::new();
    let hello = example(&work, "hello");
    let v = sitefile(&work, &["validate", s(&hello)]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(v.stdout.contains("OK (0 imports, 2 operations)"), "{}", v.stdout);

    let i = sitefile(&work, &["inspect", s(&hello), "--json"]);
    assert_eq!(i.code, 0, "{}", i.stderr);
    let data: serde_json::Value = serde_json::from_str(&i.stdout).unwrap();
    let paths: Vec<&str> = data["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["/index.html", "/style.css"]);
    assert!(!hello.join("dist").exists(), "inspect must not write output");
}

#[test]
fn errors_print_code_and_location_usage_errors_exit_2() {
    let work = Tmp::new();
    let dir = work.path().join("bad");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Sitefile"), "FROM scratch\nRUN rm -rf /\nEXPORT /\n").unwrap();
    let r = sitefile(&work, &["validate", s(&dir)]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.starts_with("error[E_UNSUPPORTED]: ") && r.stderr.contains("Sitefile:2:1:"), "{}", r.stderr);

    let u = sitefile(&work, &["build", s(&dir), "--bogus"]);
    assert_eq!(u.code, 2);
    assert!(u.stderr.contains("Unknown option '--bogus'") && u.stderr.contains("Run 'sitefile --help' for usage."), "{}", u.stderr);
    assert_eq!(sitefile(&work, &["frobnicate"]).code, 2);
    assert_eq!(sitefile(&work, &["build", "--frozen-lockfile", "--update-lock"]).code, 2);
}

#[test]
fn refuses_to_replace_a_non_empty_directory_outside_the_context_without_force() {
    let work = Tmp::new();
    let ctx = example(&work, "hello");
    let outside = work.path().join("precious");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep").unwrap();

    let r = sitefile(&work, &["build", s(&ctx), "-o", s(&outside)]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("E_EXPORT_UNSAFE"), "{}", r.stderr);
    assert_eq!(read(outside.join("keep.txt")), "keep");

    let forced = sitefile(&work, &["build", s(&ctx), "-o", s(&outside), "--force"]);
    assert_eq!(forced.code, 0, "{}", forced.stderr);
    assert!(!outside.join("keep.txt").exists());

    let unsafe_ = sitefile(&work, &["build", s(&ctx), "-o", s(work.path()), "--force"]);
    assert_eq!(unsafe_.code, 1);
    assert!(unsafe_.stderr.contains("E_EXPORT_UNSAFE"), "{}", unsafe_.stderr);
}

#[test]
fn archive_outputs_by_extension() {
    let work = Tmp::new();
    let ctx = example(&work, "hello");
    let zip = work.path().join("site.zip");
    let r = sitefile(&work, &["build", s(&ctx), "-o", s(&zip), "--no-cache"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("(zip, ") && r.stdout.contains(" bytes, sha256:"), "{}", r.stdout);
    assert_eq!(&fs::read(&zip).unwrap()[..2], b"PK");
    let oci = work.path().join("site.oci.tar");
    let r = sitefile(&work, &["build", s(&ctx), "-o", s(&oci), "--no-cache"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("(oci image sha256:"), "{}", r.stdout);
}

#[test]
fn tag_store_and_image_commands() {
    let work = Tmp::new();
    let ctx = example(&work, "hello");
    let r = sitefile(&work, &["build", s(&ctx), "-t", "site:1", "--no-cache"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.starts_with("Tagged site:1 ") && r.stdout.ends_with("(2 files)\n"), "{}", r.stdout);
    assert!(!ctx.join("dist").exists(), "-t without -o writes nothing to the context");
    let images = sitefile(&work, &["images"]);
    assert!(images.stdout.starts_with("REPOSITORY  TAG  IMAGE ID"), "{}", images.stdout);
    let out = work.path().join("saved");
    let save = sitefile(&work, &["save", "site:1", "-o", s(&out)]);
    assert_eq!(save.code, 0, "{}", save.stderr);
    assert!(read(out.join("index.html")).contains("<title>Hello</title>"));
    let rmi = sitefile(&work, &["rmi", "site:1"]);
    assert!(rmi.stdout.starts_with("Untagged: site:1\nDeleted: sha256:"), "{}", rmi.stdout);
    assert_eq!(sitefile(&work, &["rmi", "site:1"]).code, 1);
}

#[test]
fn multistage_builds_from_a_local_stage_and_copy_from() {
    let work = Tmp::new();
    let ctx = example(&work, "multistage");
    let v = sitefile(&work, &["validate", s(&ctx)]);
    assert_eq!(v.code, 0, "{}", v.stderr);
    assert!(v.stdout.contains("3 stages"));
    let r = sitefile(&work, &["build", s(&ctx), "--build-arg", "TITLE=Staged"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Exported 3 files"));
    assert_eq!(read(ctx.join("dist/static/site.css")), "body { font-family: sans-serif; }\n");
    assert_eq!(read(ctx.join("dist/raw-index.html")), "<h1>Home</h1>\n");
    assert!(read(ctx.join("dist/about.html")).contains("<title>Staged</title>"));
}

#[test]
fn inline_template_and_css_live_in_the_sitefile() {
    let work = Tmp::new();
    let ctx = example(&work, "inline");
    let r = sitefile(&work, &["build", s(&ctx), "--build-arg", "TITLE=A & B"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let html = read(ctx.join("dist/index.html"));
    assert!(html.contains("<title>A &amp; B</title>"));
    assert!(html.contains("<code>${TITLE}</code>"));
    assert!(read(ctx.join("dist/style.css")).contains("${not substituted}"));
}

#[test]
fn blog_builds_an_index_one_page_per_post_and_a_feed() {
    let work = Tmp::new();
    let ctx = example(&work, "blog");
    let r = sitefile(&work, &["build", s(&ctx)]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let out = ctx.join("dist");
    let index = read(out.join("index.html"));
    assert!(index.find("Markdown that stays safe").unwrap() < index.find("Hello, world").unwrap(), "newest first");
    let post = read(out.join("posts/markdown-that-stays-safe/index.html"));
    assert!(post.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!post.to_lowercase().contains("href=\"javascript:"));
    assert!(post.contains("<li>security</li>"));
    let feed = read(out.join("feed.xml"));
    assert!(feed.contains("<updated>2026-03-01T00:00:00Z</updated>"));
    assert_eq!(feed.matches("<entry>").count(), 3);
}

#[test]
fn data_reads_yaml_csv_and_a_web_bundle() {
    let work = Tmp::new();
    let ctx = example(&work, "data");
    let r = sitefile(&work, &["build", s(&ctx)]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let out = ctx.join("dist");
    let index = read(out.join("index.html"));
    assert!(index.contains("<a href=\"/hello/\">Unpacked bundle</a>"));
    let (a, t, g) = (index.find("Ada Lovelace").unwrap(), index.find("Alan Turing").unwrap(), index.find("Grace Hopper").unwrap());
    assert!(a < t && t < g, "oldest first by the numeric 'joined' column");
    assert!(index.contains("Computability &quot;and&quot; cryptanalysis"));
    assert!(index.contains("version b2"));
    assert!(index.contains("<td>https://hello.example/style.css</td><td>text/css; charset=utf-8</td>"));
    assert!(read(out.join("team/ada/index.html")).contains("<h1>Ada Lovelace</h1>"));
    assert!(read(out.join("hello/index.html")).contains("<title>Hello</title>"));
}

#[test]
fn motion_renders_animated_svgs_through_a_sandboxed_plugin() {
    let work = Tmp::new();
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: node is not installed");
        return;
    }
    let ctx = example(&work, "motion");
    let r = sitefile(&work, &["build", s(&ctx)]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let title = read(ctx.join("dist/title.svg"));
    assert!(title.starts_with("<?xml") && title.contains("<svg") && title.contains("@keyframes") && title.trim_end().ends_with("</svg>"));
    assert!(title.contains("prefers-reduced-motion"));
    let index = read(ctx.join("dist/index.html"));
    for part in index.split("src=\"/cards/").skip(1) {
        let name = &part[..part.find(".svg\"").unwrap()];
        assert!(read(ctx.join(format!("dist/cards/{name}.svg"))).contains("<svg"));
    }
}

#[test]
fn wasm_draws_badges_with_a_webassembly_plugin() {
    let work = Tmp::new();
    let ctx = example(&work, "wasm");
    let r = sitefile(&work, &["build", s(&ctx)]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let svg = read(ctx.join("dist/badges/why-wasm.svg"));
    assert!(svg.starts_with("<svg ") && svg.contains("aria-label=\"reading time: 1 min · "), "{svg}");
    assert!(read(ctx.join("dist/badges/built-with.svg")).contains("sitefile + wasm"));
}

#[test]
fn tool_needs_allow_exec() {
    let work = Tmp::new();
    let dir = work.path().join("tool");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "x\n").unwrap();
    fs::write(dir.join("Sitefile"), "FROM scratch\nTOOL cp\nTRANSFORM ./a.txt WITH cp TO /b.txt ARGS {input} {output}\nEXPORT /\n").unwrap();
    let denied = sitefile(&work, &["build", s(&dir)]);
    assert_eq!(denied.code, 1);
    assert!(denied.stderr.contains("add --allow-exec cp"), "{}", denied.stderr);
    let ok = sitefile(&work, &["build", s(&dir), "--allow-exec", "cp"]);
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert!(ok.stderr.starts_with("tool cp -> /"), "{}", ok.stderr);
    assert_eq!(read(dir.join("dist/b.txt")), "x\n");
}

/// DER blocks of one PEM label (`CERTIFICATE`, `PRIVATE KEY`, ...).
fn pem_blocks(pem: &str, label: &str) -> Vec<Vec<u8>> {
    use base64::Engine;
    let (begin, end) = (format!("-----BEGIN {label}-----"), format!("-----END {label}-----"));
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(b) = rest.find(&begin) {
        let after = &rest[b + begin.len()..];
        let e = after.find(&end).unwrap();
        let body: String = after[..e].chars().filter(|c| !c.is_whitespace()).collect();
        out.push(base64::engine::general_purpose::STANDARD.decode(body).unwrap());
        rest = &after[e + end.len()..];
    }
    out
}

/// The TypeScript CLI trusts NODE_EXTRA_CA_CERTS through Node's default CA
/// store; the Rust CLI must trust the same file (the network conformance cases
/// rely on it). Needs `openssl` on PATH for a throwaway CA.
#[test]
fn node_extra_ca_certs_is_trusted_for_remote_imports() {
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use ureq::rustls;
    if Command::new("openssl").arg("version").output().map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("skipped: openssl not found on PATH");
        return;
    }
    let work = Tmp::new();
    let d = work.path();
    let ossl = |args: &[&str]| {
        let o = Command::new("openssl").args(args).current_dir(d).output().unwrap();
        assert!(o.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    };
    ossl(&[
        "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "2", "-subj", "/CN=Sitefile Test CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    fs::write(d.join("srv.ext"), "subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n").unwrap();
    ossl(&["req", "-newkey", "rsa:2048", "-nodes", "-keyout", "srv.key", "-out", "srv.csr", "-subj", "/CN=sitefile-test-server"]);
    ossl(&["x509", "-req", "-in", "srv.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", "srv.pem", "-days", "2", "-extfile", "srv.ext"]);

    let certs = pem_blocks(&read(d.join("srv.pem")), "CERTIFICATE").into_iter().map(rustls::pki_types::CertificateDer::from).collect::<Vec<_>>();
    let key_pem = read(d.join("srv.key"));
    let key: rustls::pki_types::PrivateKeyDer<'static> = match pem_blocks(&key_pem, "PRIVATE KEY").pop() {
        Some(k) => rustls::pki_types::PrivatePkcs8KeyDer::from(k).into(),
        None => rustls::pki_types::PrivatePkcs1KeyDer::from(pem_blocks(&key_pem, "RSA PRIVATE KEY").pop().unwrap()).into(),
    };
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let Ok(conn) = rustls::ServerConnection::new(config.clone()) else { continue };
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut buf) {
                    Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            let body = "body { color: teal; }\n";
            let _ = write!(tls, "HTTP/1.1 200 OK\r\ncontent-type: text/css\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });

    let ctx = d.join("site");
    fs::create_dir_all(&ctx).unwrap();
    fs::write(ctx.join("Sitefile"), format!("FROM scratch\nIMPORT css FROM https://127.0.0.1:{port}/style.css\nCOPY css: /style.css\nEXPORT /\n")).unwrap();
    let build = |extra_ca: Option<&Path>| {
        let mut cmd = Command::new(BIN);
        cmd.args(["build", s(&ctx), "--allow-private-network", "--no-lockfile", "--force"]).env("SITEFILE_HOME", d.join(".home")).env_remove("NODE_EXTRA_CA_CERTS");
        if let Some(ca) = extra_ca {
            cmd.env("NODE_EXTRA_CA_CERTS", ca);
        }
        cmd.output().unwrap()
    };
    let text = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).into_owned();
    let untrusted = build(None);
    assert_eq!(untrusted.status.code(), Some(1));
    assert!(text(&untrusted).contains("error[E_FETCH]"), "{}", text(&untrusted));
    let trusted = build(Some(&d.join("ca.pem")));
    assert_eq!(trusted.status.code(), Some(0), "{}", text(&trusted));
    assert_eq!(read(ctx.join("dist/style.css")), "body { color: teal; }\n");
    // An unreadable file is ignored with Node's warning, not fatal.
    let missing = build(Some(&d.join("no-such-ca.pem")));
    assert_eq!(missing.status.code(), Some(1));
    assert!(text(&missing).contains("Warning: Ignoring extra certs from"), "{}", text(&missing));
}

// ---- SITEMAP, FEED, REDIRECT and HEADER (test/site.test.ts)

fn site_project(work: &Tmp, files: &[(&str, &str)]) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let ctx = work.path().join(format!("site-{}", N.fetch_add(1, Ordering::SeqCst)));
    for (rel, content) in files {
        let p = ctx.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    ctx
}

const POSTS: [(&str, &str); 2] = [
    ("posts/2026-01-05-first.md", "---\ntitle: First & <one>\nauthor: Ann\n---\nHello *there*.\n"),
    ("posts/2026-02-01-second.md", "---\ntitle: Second\ndate: 2026-02-01T09:30:00+01:00\n---\nTwo.\n"),
];

#[test]
fn site_files_sitemap_feed_redirects_headers() {
    let work = Tmp::new();
    let src = [
        "ARG URL = \"https://blog.example\"",
        "IMPORT posts FROM ./posts AS markdown SORT BY date DESC",
        "COPY <<X /index.html\nX",
        "COPY <<X /404.html\nX",
        "COPY <<X /a&b.html\nX",
        "SITEMAP BASE ${URL}/",
        "FEED posts BASE ${URL} LINK /p/${item.slug}/index.html TO /atom.xml TITLE \"Ann's & notes\"",
        "REDIRECT /blog/* https://blog.example/:splat STATUS 302",
        "HEADER /* X-Frame-Options: DENY",
        "HEADER /assets/* Cache-Control: public, max-age=60",
        "HEADER /* Referrer-Policy: no-referrer",
        "EXPORT /",
    ]
    .join("\n");
    let ctx = site_project(&work, &[POSTS[0], POSTS[1], ("Sitefile", &src)]);
    let out = work.path().join("out");
    let r = sitefile(&work, &["build", s(&ctx), "-o", s(&out), "--no-lockfile"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        read(out.join("sitemap.xml")),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n  <url><loc>https://blog.example/a%26b.html</loc></url>\n  <url><loc>https://blog.example/</loc></url>\n</urlset>\n"
    );
    let feed = read(out.join("atom.xml"));
    assert!(feed.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n  <title>Ann&apos;s &amp; notes</title>\n  <link href=\"https://blog.example/\"/>\n  <link rel=\"self\" href=\"https://blog.example/atom.xml\"/>\n  <id>https://blog.example/atom.xml</id>\n  <updated>2026-02-01T09:30:00+01:00</updated>\n"), "{feed}");
    assert!(feed.contains("    <title>First &amp; &lt;one&gt;</title>\n    <link href=\"https://blog.example/p/first/\"/>\n    <id>https://blog.example/p/first/</id>\n    <updated>2026-01-05T00:00:00Z</updated>\n    <author><name>Ann</name></author>\n    <summary>Hello there.</summary>\n    <content type=\"html\">&lt;p&gt;Hello &lt;em&gt;there&lt;/em&gt;.&lt;/p&gt;\n</content>\n"), "{feed}");
    assert_eq!(read(out.join("_redirects")), "/blog/* https://blog.example/:splat 302\n");
    assert_eq!(read(out.join("_headers")), "/*\n  X-Frame-Options: DENY\n  Referrer-Policy: no-referrer\n/assets/*\n  Cache-Control: public, max-age=60\n");
}

#[test]
fn site_file_errors() {
    let work = Tmp::new();
    let cases: [(&[(&str, &str)], &str); 5] = [
        (&[("posts/x.md", "---\ntitle: X\n---\nbody\n"), ("Sitefile", "IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /")], "FEED posts: document '/x.md' has no 'date' (add it to its frontmatter)"),
        (&[("posts/x.md", "---\ntitle: X\ndate: soon\n---\n"), ("Sitefile", "IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /")], "has date 'soon'; expected YYYY-MM-DD or an RFC 3339 date-time"),
        (&[("one.md", "---\ntitle: X\n---\n"), ("Sitefile", "IMPORT posts FROM ./one.md AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /")], "error[E_TYPE]"),
        (&[("_redirects", "/x /y 301\n"), ("Sitefile", "COPY ./_redirects /_redirects\nREDIRECT /a /b\nEXPORT /")], "'/_redirects' was already written with different content by "),
        (&[("Sitefile", "HEADER /* X-A: \"1\\nSet-Cookie: a=b\"\nEXPORT /")], "HEADER X-A: the value must not contain control characters"),
    ];
    for (files, expected) in cases {
        let ctx = site_project(&work, files);
        let r = sitefile(&work, &["build", s(&ctx), "-o", s(&work.path().join("o")), "--no-lockfile", "--force"]);
        assert_eq!(r.code, 1, "{expected}");
        assert!(r.stderr.contains(expected), "{} lacks {expected}", r.stderr);
    }
    // identical content is fine; --overwrite replaces
    let ctx = site_project(&work, &[("_headers", "x"), ("Sitefile", "COPY ./_headers /_headers\nHEADER --overwrite /* A: 1\nEXPORT /")]);
    let out = work.path().join("ov");
    assert_eq!(sitefile(&work, &["build", s(&ctx), "-o", s(&out), "--no-lockfile"]).code, 0);
    assert_eq!(read(out.join("_headers")), "/*\n  A: 1\n");
}
