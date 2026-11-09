//! Git sources (issue #91, port of test/git.test.ts): IMPORT from `git+http://…` and Git URL build
//! contexts, with the built binary against a minimal in-test Git smart-HTTP (protocol v2) server.

use flate2::{write::ZlibEncoder, Compression};
use sitefile::resources::git::{git_object_id, ObjType};
use sitefile::sha1::{hex, Sha1};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-repo-rs-{}-{}",
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

#[derive(Default, Clone)]
struct Repo {
    objects: HashMap<String, (ObjType, Vec<u8>)>,
    refs: Vec<(String, String, Option<String>)>, // name, id, peeled
}

impl Repo {
    fn put(&mut self, kind: ObjType, data: Vec<u8>) -> String {
        let id = git_object_id(kind, &data);
        self.objects.insert(id.clone(), (kind, data));
        id
    }
    /// A tree from (mode, name, content-or-subtree-id) entries; blobs are written.
    fn tree(&mut self, entries: &[(&str, &str, &str)]) -> String {
        let mut out = Vec::new();
        let mut sorted: Vec<(String, String, String)> = entries
            .iter()
            .map(|(mode, name, v)| {
                let id = if *mode == "40000" || *mode == "160000" {
                    v.to_string()
                } else {
                    self.put(ObjType::Blob, v.as_bytes().to_vec())
                };
                (mode.to_string(), name.to_string(), id)
            })
            .collect();
        sorted.sort_by_key(|(m, n, _)| {
            if m == "40000" {
                format!("{n}/")
            } else {
                n.clone()
            }
        });
        for (mode, name, id) in sorted {
            out.extend(format!("{mode} {name}\0").as_bytes());
            out.extend((0..20).map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).unwrap()));
        }
        self.put(ObjType::Tree, out)
    }
    fn commit(&mut self, tree: &str, message: &str) -> String {
        self.put(
            ObjType::Commit,
            format!(
                "tree {tree}\nauthor T <t@e> 0 +0000\ncommitter T <t@e> 0 +0000\n\n{message}\n"
            )
            .into_bytes(),
        )
    }
    fn reachable(&self, commit: &str) -> Vec<String> {
        let mut out = vec![commit.to_string()];
        let text = String::from_utf8_lossy(&self.objects[commit].1).into_owned();
        let tree = text[5..45].to_string();
        fn walk(r: &Repo, id: &str, out: &mut Vec<String>) {
            out.push(id.to_string());
            let d = &r.objects[id].1;
            let mut i = 0;
            while i < d.len() {
                let sp = i + d[i..].iter().position(|&b| b == b' ').unwrap();
                let nul = sp + d[sp..].iter().position(|&b| b == 0).unwrap();
                let mode = std::str::from_utf8(&d[i..sp]).unwrap();
                let child = hex(&d[nul + 1..nul + 21]);
                if mode == "40000" {
                    walk(r, &child, out);
                } else if mode != "160000" && !out.contains(&child) {
                    out.push(child);
                }
                i = nul + 21;
            }
        }
        walk(self, &tree, &mut out);
        out
    }
}

fn zlib(d: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(d).unwrap();
    e.finish().unwrap()
}

fn obj_header(code: u8, size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut c = (code << 4) | (size & 15) as u8;
    let mut s = size >> 4;
    while s > 0 {
        out.push(c | 0x80);
        c = (s & 0x7f) as u8;
        s >>= 7;
    }
    out.push(c);
    out
}

/// A pack of `ids`; a blob that extends the previous blob is sent as an OFS_DELTA against it.
fn pack(repo: &Repo, ids: &[String]) -> Vec<u8> {
    let mut out = b"PACK".to_vec();
    out.extend(2u32.to_be_bytes());
    out.extend((ids.len() as u32).to_be_bytes());
    let mut prev_blob: Option<(usize, Vec<u8>)> = None;
    for id in ids {
        let (kind, data) = &repo.objects[id];
        let off = out.len();
        let code = match kind {
            ObjType::Commit => 1,
            ObjType::Tree => 2,
            ObjType::Blob => 3,
            ObjType::Tag => 4,
        };
        match &prev_blob {
            Some((boff, base))
                if *kind == ObjType::Blob
                    && data.len() > base.len()
                    && data.starts_with(base)
                    && base.len() > 8
                    && base.len() < 0x10000 =>
            {
                let tail = &data[base.len()..];
                let varint = |mut n: usize| {
                    let mut v = Vec::new();
                    loop {
                        let mut c = (n & 0x7f) as u8;
                        n >>= 7;
                        if n > 0 {
                            c |= 0x80;
                        }
                        v.push(c);
                        if n == 0 {
                            return v;
                        }
                    }
                };
                let mut d = varint(base.len());
                d.extend(varint(data.len()));
                d.extend([
                    0x80 | 0x10 | 0x20,
                    (base.len() & 0xff) as u8,
                    (base.len() >> 8) as u8,
                    tail.len() as u8,
                ]);
                d.extend_from_slice(tail);
                out.extend(obj_header(6, d.len()));
                let rel = off - boff;
                let mut enc = vec![(rel & 0x7f) as u8];
                let mut r = rel >> 7;
                while r > 0 {
                    r -= 1;
                    enc.insert(0, 0x80 | (r & 0x7f) as u8);
                    r >>= 7;
                }
                out.extend(enc);
                out.extend(zlib(&d));
            }
            _ => {
                out.extend(obj_header(code, data.len()));
                out.extend(zlib(data));
            }
        }
        if *kind == ObjType::Blob {
            prev_blob = Some((off, data.clone()));
        }
    }
    let mut h = Sha1::new();
    h.update(&out);
    out.extend(h.finish());
    out
}

fn pkt(s: &[u8]) -> Vec<u8> {
    let mut v = format!("{:04x}", s.len() + 4).into_bytes();
    v.extend_from_slice(s);
    v
}

fn pkt_lines(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let n = usize::from_str_radix(std::str::from_utf8(&body[i..i + 4]).unwrap(), 16).unwrap();
        if n < 4 {
            i += 4;
            continue;
        }
        out.push(
            String::from_utf8_lossy(&body[i + 4..i + n])
                .trim_end_matches('\n')
                .to_string(),
        );
        i += n;
    }
    out
}

type Shared = Arc<Mutex<Repo>>;

/// Serve `/repo.git`; returns the base URL and a request counter.
fn serve(repo: Shared) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = count.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            c2.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let repo = repo.lock().unwrap().clone();
            let (ct, resp): (&str, Vec<u8>) =
                if path == "/repo.git/info/refs?service=git-upload-pack" {
                    let mut v = pkt(b"# service=git-upload-pack\n");
                    v.extend(b"0000");
                    for l in [
                        "version 2\n",
                        "ls-refs\n",
                        "fetch=shallow\n",
                        "object-format=sha1\n",
                    ] {
                        v.extend(pkt(l.as_bytes()));
                    }
                    v.extend(b"0000");
                    ("application/x-git-upload-pack-advertisement", v)
                } else if path == "/repo.git/git-upload-pack" {
                    let lines = pkt_lines(&body);
                    let mut v = Vec::new();
                    if lines.first().map(String::as_str) == Some("command=ls-refs") {
                        let prefixes: Vec<&str> = lines
                            .iter()
                            .filter_map(|l| l.strip_prefix("ref-prefix "))
                            .collect();
                        for (name, id, peeled) in &repo.refs {
                            if prefixes.iter().any(|p| name.starts_with(p)) {
                                let p = peeled
                                    .as_ref()
                                    .map(|p| format!(" peeled:{p}"))
                                    .unwrap_or_default();
                                v.extend(pkt(format!("{id} {name}{p}\n").as_bytes()));
                            }
                        }
                        v.extend(b"0000");
                    } else {
                        let want = lines
                            .iter()
                            .find_map(|l| l.strip_prefix("want "))
                            .unwrap()
                            .to_string();
                        let p = pack(&repo, &repo.reachable(&want));
                        v.extend(pkt(b"shallow-info\n"));
                        v.extend(pkt(format!("shallow {want}\n").as_bytes()));
                        v.extend(b"0001");
                        v.extend(pkt(b"packfile\n"));
                        for chunk in p.chunks(8000) {
                            let mut c = vec![1u8];
                            c.extend_from_slice(chunk);
                            v.extend(pkt(&c));
                        }
                        v.extend(b"0000");
                    }
                    ("application/x-git-upload-pack-result", v)
                } else {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    continue;
                };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", resp.len());
            let _ = stream.write_all(&resp);
        }
    });
    (base, count)
}

fn sitefile(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let o = Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("SITEFILE_HOME", home)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into(),
        String::from_utf8_lossy(&o.stderr).into(),
    )
}

const NET: [&str; 2] = ["--allow-http", "--allow-private-network"];

/// Two commits on main; v1 is an annotated tag of the first.
fn fixture() -> (Shared, String, String) {
    let mut r = Repo::default();
    let big = "shared prefix line\n".repeat(20);
    let docs = r.tree(&[
        ("100644", "a.md", "---\ntitle: A\n---\n# A\n"),
        ("120000", "link.md", "a.md"),
        ("100755", "run.sh", "#!/bin/sh\n"),
        ("100644", "big.txt", &big),
        ("100644", "big2.txt", &format!("{big}two\n")),
    ]);
    let site1 = r.tree(&[("100644", "index.html", "<h1>v1</h1>\n")]);
    let t1 = r.tree(&[
        ("40000", "docs", &docs),
        ("40000", "site", &site1),
        (
            "100644",
            "Sitefile",
            "FROM scratch\nCOPY ./site/ /\nEXPORT /\n",
        ),
        ("160000", "mod", &"a".repeat(40)),
    ]);
    let c1 = r.commit(&t1, "one");
    let site2 = r.tree(&[("100644", "index.html", "<h1>v2</h1>\n")]);
    let t2 = r.tree(&[
        ("40000", "site", &site2),
        (
            "100644",
            "Sitefile",
            "FROM scratch\nCOPY ./site/ /\nEXPORT /\n",
        ),
    ]);
    let c2 = r.commit(&t2, "two");
    let tag = r.put(
        ObjType::Tag,
        format!("object {c1}\ntype commit\ntag v1\ntagger T <t@e> 0 +0000\n\nv1\n").into_bytes(),
    );
    r.refs = vec![
        ("HEAD".into(), c2.clone(), None),
        ("refs/heads/main".into(), c2.clone(), None),
        ("refs/tags/v1".into(), tag, Some(c1.clone())),
    ];
    (Arc::new(Mutex::new(r)), c1, c2)
}

#[test]
fn imports_lock_vendor_outdated_and_contexts() {
    let (repo, c1, c2) = fixture();
    let (base, count) = serve(repo);
    let t = Tmp::new();
    let home = t.0.join("home");
    let ctx = t.0.join("site");
    fs::create_dir_all(&ctx).unwrap();
    let c = ctx.to_str().unwrap();
    fs::write(ctx.join("Sitefile"), format!("FROM scratch\nIMPORT docs FROM git+{base}/repo.git#v1:docs\nIMPORT site FROM git+{base}/repo.git#main:site\nCOPY docs: /d/\nCOPY site: /\nEXPORT /\n")).unwrap();

    // validate --json: the location shape
    let (code, out, err) = sitefile(&home, &t.0, &["validate", c, "--json"]);
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let loc = &v["imports"][0]["location"];
    let keys: Vec<&String> = loc.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["kind", "url", "repo", "ref", "path"]);
    assert_eq!(loc["ref"], "v1");

    // --strict-pins refuses the unpinned refs before fetching
    let (code, _, err) = sitefile(&home, &t.0, &["build", c, NET[0], NET[1], "--strict-pins"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("error[E_UNPINNED]") && err.contains("name a commit (#<40 hex>[:path])"),
        "{err}"
    );

    let (code, out, err) = sitefile(&home, &t.0, &["build", c, NET[0], NET[1]]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(&format!(
            "IMPORT docs is not pinned; pin commit {c1} (#{c1}:docs) for a reproducible build"
        )),
        "{err}"
    );
    assert!(out.contains("Updated Sitefile.lock"), "{out}");
    let dist = ctx.join("dist");
    assert_eq!(
        fs::read_to_string(dist.join("d/link.md")).unwrap(),
        "---\ntitle: A\n---\n# A\n"
    );
    assert_eq!(
        fs::read_to_string(dist.join("index.html")).unwrap(),
        "<h1>v2</h1>\n"
    );
    assert!(fs::read_to_string(dist.join("d/big2.txt"))
        .unwrap()
        .ends_with("two\n"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dist.join("d/run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    let lock = fs::read_to_string(ctx.join("Sitefile.lock")).unwrap();
    assert_eq!(
        lock,
        format!("{{\n  \"version\": 1,\n  \"resources\": {{}},\n  \"git\": {{\n    \"git+{base}/repo.git#main:site\": {{\n      \"commit\": \"{c2}\"\n    }},\n    \"git+{base}/repo.git#v1:docs\": {{\n      \"commit\": \"{c1}\"\n    }}\n  }}\n}}\n")
    );
    let (code, _, err) = sitefile(
        &home,
        &t.0,
        &[
            "build",
            c,
            NET[0],
            NET[1],
            "--strict-pins",
            "--frozen-lockfile",
        ],
    );
    assert_eq!(code, 0, "{err}");

    // vendor, then an offline build that makes no request
    let (code, out, err) = sitefile(&home, &t.0, &["vendor", c, NET[0], NET[1]]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with("Vendored 2 resources and 0 images (2 blobs"),
        "{out}"
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ctx.join("vendor/sitefile/manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["git"][format!("git+{base}/repo.git#v1:docs")]["kind"],
        "tree"
    );
    let before = count.load(Ordering::SeqCst);
    let (code, _, err) = sitefile(
        &home,
        &t.0,
        &[
            "build",
            c,
            "--frozen-lockfile",
            "-o",
            t.0.join("out").to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        count.load(Ordering::SeqCst),
        before,
        "no request with a vendored copy"
    );
    fs::remove_dir_all(ctx.join("vendor")).unwrap();

    // outdated: a locked ref that moved is changed (lock docs at c2 by hand), a commit pin is digest-only
    fs::write(
        ctx.join("Sitefile"),
        format!("FROM scratch\nIMPORT a FROM git+{base}/repo.git#main:site\nIMPORT b FROM git+{base}/repo.git#{c1}\nIMPORT c FROM git+{base}/repo.git#v1\nIMPORT d FROM git+{base}/repo.git#gone\nEXPORT /\n"),
    )
    .unwrap();
    fs::write(ctx.join("Sitefile.lock"), format!("{{\"version\":1,\"resources\":{{}},\"git\":{{\"git+{base}/repo.git#main:site\":{{\"commit\":\"{c1}\"}}}}}}")).unwrap();
    let (code, out, err) = sitefile(&home, &t.0, &["outdated", c, NET[0], NET[1], "--json"]);
    assert_eq!(code, 0, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let rows: Vec<(String, String, String)> = v["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["status"].as_str().unwrap().into(),
                i["pinnedBy"].as_str().unwrap_or("-").into(),
                i["latest"].as_str().unwrap_or("-").into(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("changed".to_string(), "lockfile".to_string(), c2.clone()),
            ("digest-only".into(), "commit".into(), "-".into()),
            ("unpinned".into(), "-".into(), c1.clone()),
            ("error".into(), "-".into(), "-".into())
        ]
    );
    assert_eq!(v["inputs"][3]["error"]["code"], "E_NOT_FOUND");

    // INTEGRITY on a Git import, a submodule
    fs::write(
        ctx.join("Sitefile"),
        format!(
            "FROM scratch\nIMPORT a FROM git+{base}/repo.git#main INTEGRITY sha256:{}\nEXPORT /\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    let (code, _, err) = sitefile(&home, &t.0, &["validate", c]);
    assert_eq!(code, 1);
    assert!(
        err.contains("error[E_VALIDATION]")
            && err.contains("INTEGRITY does not apply to Git imports"),
        "{err}"
    );
    fs::write(
        ctx.join("Sitefile"),
        format!("FROM scratch\nIMPORT a FROM git+{base}/repo.git#v1\nEXPORT /\n"),
    )
    .unwrap();
    let (code, _, err) = sitefile(&home, &t.0, &["build", c, NET[0], NET[1], "--no-lockfile"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("error[E_UNSUPPORTED_FILE]") && err.contains("'mod' is a Git submodule"),
        "{err}"
    );

    // a Git URL context: fetched, built into ./dist of the working directory, removed
    let cwd = t.0.join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    let url = format!("{base}/repo.git");
    let (code, out, err) = sitefile(&home, &cwd, &["build", &url, NET[0], NET[1]]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.starts_with(&format!("context: {url} at commit {c2}\n")),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(cwd.join("dist/index.html")).unwrap(),
        "<h1>v2</h1>\n"
    );
    assert!(out.starts_with("Exported 1 file to dist ("), "{out}");
    let (code, _, err) = sitefile(&home, &cwd, &["build", &url, "--watch"]);
    assert_eq!(code, 2, "{err}");
    let (code, _, _) = sitefile(&home, &cwd, &["build", &url, "-f", "../x"]);
    assert_eq!(code, 2);
}
