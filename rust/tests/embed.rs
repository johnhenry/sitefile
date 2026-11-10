//! The library API for embedders (johnhenry/sitefile#153): what sitefile
//! Desktop's `sfd-local` calls instead of copying CLI code. Everything here
//! passes its store home explicitly; `SITEFILE_HOME` is pointed at an empty
//! directory first, so a test that fell back to the environment would fail
//! (and could never touch `~/.sitefile`).
// The detached-server and TOOL cp tests are Unix-only.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use sitefile::error::{ErrorCode, Needs, NeedsKind};
use sitefile::executor::{
    build, store_build_as_image, BuildOptions, CancelToken, ImageInfo, LockfileSetting,
};
use sitefile::plugins::cache::TransformCache;
use sitefile::servers::{
    is_running, parse_publish, pid_alive, resolve_source, restart_detached, serve_detached,
    stop_server, DetachedServe, ServerRegistry, ServerSource,
};
use sitefile::store::Store;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sitefile");

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Tmp {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "sitefile-embed-rs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(sitefile::platform::canonicalize(&p).unwrap())
    }
    fn write(&self, rel: &str, data: impl AsRef<[u8]>) {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `SITEFILE_HOME` -> an empty directory nobody passes as a store home.
fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let decoy =
            std::env::temp_dir().join(format!("sitefile-embed-decoy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&decoy);
        fs::create_dir_all(&decoy).unwrap();
        std::env::set_var("SITEFILE_HOME", &decoy);
    });
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn options(ctx: &Path, home: &Path) -> BuildOptions {
    BuildOptions {
        context_dir: ctx.display().to_string(),
        lockfile: LockfileSetting::Disabled,
        store_home: Some(home.to_path_buf()),
        ..Default::default()
    }
}

fn fail(o: &BuildOptions) -> sitefile::error::SitefileError {
    match build(o) {
        Ok(_) => panic!("expected the build to fail"),
        Err(e) => e,
    }
}

fn simple_site(t: &Tmp) -> PathBuf {
    t.write("ctx/a.txt", "hello");
    t.write(
        "ctx/Sitefile",
        "FROM scratch\nCOPY ./a.txt /a.txt\nEXPORT /\n",
    );
    t.0.join("ctx")
}

// ------------------------------------------------------------ 2. store a build as an image

#[test]
fn a_build_is_stored_as_a_tagged_image() {
    isolate();
    let t = Tmp::new();
    let ctx = simple_site(&t);
    let home = t.0.join("home");
    let o = options(&ctx, &home);
    let r = build(&o).unwrap();
    let store = Store::open(&home).unwrap();
    let info = ImageInfo::new(ctx.display().to_string());
    let s = store_build_as_image(
        &store,
        &r,
        &["site:1".into(), "site:latest".into()],
        &info,
        &o,
    )
    .unwrap();
    assert!(s.id.starts_with("sha256:"), "{}", s.id);
    assert_eq!(s.tags, ["site:1", "site:latest"]);
    assert_eq!(store.resolve("site:1").unwrap(), s.id);
    assert_eq!(store.load_tree(&s.id).unwrap().digest(), r.output.digest());
    // The same build stored again is the same image; no tags stores it untagged.
    let again = store_build_as_image(&store, &r, &[], &info, &o).unwrap();
    assert_eq!((again.id.as_str(), again.tags.len()), (s.id.as_str(), 0));
    // A digest is not a tag: refused before anything is stored.
    let e = store_build_as_image(
        &store,
        &r,
        &[format!("site@sha256:{}", "0".repeat(64))],
        &info,
        &o,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::E_USAGE);
    assert!(
        e.message().contains("a tag cannot include a digest"),
        "{}",
        e.message()
    );
    // Nothing went to the environment's store.
    assert!(Store::open_existing(sitefile::store::resolve_store_home())
        .map(|s| s.list().unwrap().is_empty())
        .unwrap_or(true));
}

// ------------------------------------------------------------ 3. the store home is explicit

#[test]
fn from_image_and_wasm_use_the_store_home_given() {
    isolate();
    let t = Tmp::new();
    let ctx = simple_site(&t);
    let home = t.0.join("home");
    let o = options(&ctx, &home);
    let r = build(&o).unwrap();
    let store = Store::open(&home).unwrap();
    store_build_as_image(
        &store,
        &r,
        &["base:1".into()],
        &ImageInfo::new(ctx.display().to_string()),
        &o,
    )
    .unwrap();

    // FROM <local image> reads the store home of the options, not $SITEFILE_HOME.
    t.write("on/b.txt", "b");
    t.write(
        "on/Sitefile",
        "FROM image base:1\nCOPY ./b.txt /b.txt\nEXPORT /\n",
    );
    let on = t.0.join("on");
    let r = build(&options(&on, &home)).unwrap();
    let paths: Vec<&str> = r.output.entries().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["/a.txt", "/b.txt"]);
    let elsewhere = t.0.join("other-home");
    let e = fail(&options(&on, &elsewhere));
    assert_eq!(e.code, ErrorCode::E_NOT_FOUND, "{}", e.message());
    let e = fail(&BuildOptions {
        store_home: None,
        ..options(&on, &home)
    });
    assert_eq!(
        e.code,
        ErrorCode::E_NOT_FOUND,
        "None falls back to $SITEFILE_HOME (the empty decoy)"
    );

    // The wasm compiled-module cache lands under <home>/cache/wasm. A custom
    // section makes these bytes unique, so no earlier compile in this process is reused.
    let mut wasm = fs::read(repo_root().join("test/fixtures/wasm/const.wasm")).unwrap();
    let name = format!(
        "embed-{}-{}",
        std::process::id(),
        t.0.display().to_string().len()
    );
    let payload_len = 1 + name.len();
    wasm.push(0);
    wasm.push(payload_len as u8);
    wasm.push(name.len() as u8);
    wasm.extend_from_slice(name.as_bytes());
    t.write("w/plugins/const.wasm", &wasm);
    t.write(
        "w/Sitefile",
        "FROM scratch\nPLUGIN c FROM ./plugins/const.wasm\nTRANSFORM WITH c TO /out.txt\nEXPORT /\n",
    );
    let wasm_home = t.0.join("wasm-home");
    let r = build(&options(&t.0.join("w"), &wasm_home)).unwrap();
    assert_eq!(r.output.len(), 1);
    // Windows has no wasm disk cache (docs/RUST.md, "Windows").
    if cfg!(windows) {
        return;
    }
    let cached: Vec<_> = fs::read_dir(wasm_home.join("cache/wasm"))
        .expect("the wasm cache is under the store home given")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cwasm"))
        .collect();
    assert_eq!(cached.len(), 1);
}

// ------------------------------------------------------------ 4. cancellation

#[test]
fn a_cancelled_build_fails_with_e_cancelled() {
    isolate();
    let t = Tmp::new();
    let ctx = simple_site(&t);
    let home = t.0.join("home");
    let cancel = CancelToken::new();
    cancel.cancel();
    let o = BuildOptions {
        cancel: Some(cancel.clone()),
        ..options(&ctx, &home)
    };
    let e = fail(&o);
    assert_eq!(e.code, ErrorCode::E_CANCELLED);
    assert_eq!(e.message(), "the build was cancelled");

    // Cancelled while it runs: the warning callback fires during planning,
    // before any stage starts; the build stops at the next check.
    let token = CancelToken::new();
    let t2 = token.clone();
    let o = BuildOptions {
        allow_plugin: vec!["nothing".into()],
        cancel: Some(token.clone()),
        on_warning: Some(Arc::new(move |_m: &str| t2.cancel())),
        ..options(&ctx, &home)
    };
    let e = fail(&o);
    assert!(token.is_cancelled());
    assert_eq!(e.code, ErrorCode::E_CANCELLED, "{}", e.message());

    // A finished build is not stored once cancelled.
    let o = options(&ctx, &home);
    let r = build(&o).unwrap();
    let late = CancelToken::new();
    late.cancel();
    let store = Store::open(&home).unwrap();
    let e = store_build_as_image(
        &store,
        &r,
        &["x:1".into()],
        &ImageInfo::new(ctx.display().to_string()),
        &BuildOptions {
            cancel: Some(late),
            ..o
        },
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::E_CANCELLED);
    assert!(store.list().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn cancelling_from_a_callback_stops_the_running_build() {
    isolate();
    let t = Tmp::new();
    for i in 0..4 {
        t.write(&format!("ctx/in/{i}.txt"), format!("{i}"));
    }
    // One TOOL run per file; the token is cancelled when the first run's notice
    // ("tool cp -> /bin/cp") is printed, so no run happens at all.
    t.write(
        "ctx/Sitefile",
        "FROM scratch\nTOOL cp\nTRANSFORM ./in/ WITH cp TO /out/* ARGS {input} {output}\nEXPORT /\n",
    );
    let ctx = t.0.join("ctx");
    let token = CancelToken::new();
    let t2 = token.clone();
    let o = BuildOptions {
        allow_exec: vec!["cp".into()],
        cancel: Some(token),
        on_notice: Some(Arc::new(move |_m: &str| t2.cancel())),
        ..options(&ctx, &t.0.join("home"))
    };
    let e = fail(&o);
    assert_eq!(e.code, ErrorCode::E_CANCELLED, "{}", e.message());
    // Without the token the same build works.
    let r = build(&BuildOptions {
        cancel: None,
        on_notice: None,
        ..o
    })
    .unwrap();
    assert_eq!(r.output.len(), 4);
}

// ------------------------------------------------------------ 5. per-plugin cache entries

#[cfg(unix)]
#[test]
fn cache_entries_record_their_plugin_and_can_be_pruned_per_plugin() {
    isolate();
    let t = Tmp::new();
    t.write("ctx/a.txt", "a");
    t.write("ctx/b.txt", "b");
    t.write(
        "ctx/Sitefile",
        "FROM scratch\nTOOL copya\nTOOL copyb\nTRANSFORM ./a.txt WITH copya TO /a.txt ARGS {input} {output}\nTRANSFORM ./b.txt WITH copyb TO /b.txt ARGS {input} {output}\nEXPORT /\n",
    );
    let cache = t.0.join("cache");
    let o = BuildOptions {
        allow_exec: vec!["copya=/bin/cp".into(), "copyb=/bin/cp".into()],
        cache_dir: Some(cache.clone()),
        ..options(&t.0.join("ctx"), &t.0.join("home"))
    };
    build(&o).unwrap();
    let tc = TransformCache::new(&cache);
    let usage = tc.usage_by_plugin();
    let names: Vec<_> = usage
        .iter()
        .map(|u| (u.plugin.as_deref(), u.entries, u.bytes))
        .collect();
    assert_eq!(names, [(Some("copya"), 1, 1), (Some("copyb"), 1, 1)]);

    let r = tc.prune_plugin("copya", Duration::ZERO);
    assert_eq!((r.entries, r.blobs, r.bytes), (1, 1, 1));
    let left: Vec<_> = tc.usage_by_plugin().into_iter().map(|u| u.plugin).collect();
    assert_eq!(left, [Some("copyb".to_string())]);
    // The rebuild misses only copya's result.
    let r = build(&o).unwrap();
    assert_eq!(
        (r.transform_stats.cache.hits, r.transform_stats.cache.misses),
        (1, 1)
    );
}

// ------------------------------------------------------------ 6. structured permission errors

#[test]
fn permission_errors_name_what_must_be_allowed() {
    isolate();
    let t = Tmp::new();
    t.write("ctx/plugins/shout.mjs", "export function transform() {}\n");
    t.write("ctx/a.txt", "a");
    t.write(
        "ctx/Sitefile",
        "FROM scratch\nPLUGIN shout FROM ./plugins/shout.mjs\nTRANSFORM ./a.txt WITH shout TO /a.txt\nEXPORT /\n",
    );
    let home = t.0.join("home");
    let e = fail(&options(&t.0.join("ctx"), &home));
    assert_eq!(e.code, ErrorCode::E_UNSUPPORTED);
    let n = e.needs.clone().expect("needs");
    assert_eq!(n, Needs::plugin("shout"));
    assert_eq!((n.kind, n.flag()), (NeedsKind::Plugin, "--allow-plugin"));
    assert_eq!(
        n.args(),
        ["--allow-plugin".to_string(), "shout".to_string()]
    );
    assert!(
        e.message().ends_with("PLUGIN shout runs JavaScript, which must be allowed explicitly: add --allow-plugin shout (or --allow-plugins to allow every PLUGIN); WebAssembly plugins need no flag"),
        "the message is unchanged: {}",
        e.message()
    );

    t.write(
        "ctx/Sitefile",
        "FROM scratch\nTOOL ffmpeg\nTRANSFORM ./a.txt WITH ffmpeg TO /a.txt ARGS {input} {output}\nEXPORT /\n",
    );
    let e = fail(&options(&t.0.join("ctx"), &home));
    assert_eq!(e.code, ErrorCode::E_UNSUPPORTED);
    assert_eq!(e.needs, Some(Needs::tool("ffmpeg")));
    assert_eq!(e.needs.as_ref().unwrap().flag(), "--allow-exec");
    assert!(
        e.message().contains("TOOL ffmpeg runs an external program"),
        "{}",
        e.message()
    );

    // Other errors carry nothing; secrets redaction keeps it.
    t.write(
        "ctx/Sitefile",
        "FROM scratch\nCOPY ./missing.txt /m.txt\nEXPORT /\n",
    );
    assert_eq!(fail(&options(&t.0.join("ctx"), &home)).needs, None);
}

// ------------------------------------------------------------ 1 + 7. detached serve and reaping

fn get(host: &str, port: u16, path: &str) -> String {
    let mut c = std::net::TcpStream::connect((host, port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        c,
        "GET {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n"
    )
    .unwrap();
    let mut s = String::new();
    c.read_to_string(&mut s).unwrap();
    s
}

#[cfg(unix)]
#[test]
fn detached_servers_start_restart_and_stop_from_a_long_lived_process() {
    isolate();
    let t = Tmp::new();
    t.write("site/index.html", "<p>hi</p>");
    let home = t.0.join("home");
    let store = Store::open(&home).unwrap();

    // Source resolution: directories by their spelling, images by reference.
    let dir = format!("{}/", t.0.join("site").display());
    let (label, source) = resolve_source(&store, &dir).unwrap();
    assert_eq!(label, t.0.join("site").display().to_string());
    assert!(matches!(source, ServerSource::Dir { .. }));
    assert_eq!(
        resolve_source(&store, &format!("{}/nope/", t.0.display()))
            .unwrap_err()
            .code,
        ErrorCode::E_NOT_FOUND
    );
    assert_eq!(
        resolve_source(&store, "missing:1").unwrap_err().code,
        ErrorCode::E_NOT_FOUND
    );

    let reg = ServerRegistry::open(Some(store)).unwrap();
    let mut serve = DetachedServe::resolve(&reg.store, &dir).unwrap();
    serve.name = Some("embedded".into());
    serve.bind = parse_publish(Some("127.0.0.1:0")).unwrap();
    let up = serve_detached(&reg, &serve, Some(Path::new(BIN))).unwrap();
    assert!(up.ready && up.port != 0 && up.pid > 0);
    assert_eq!(up.name, "embedded");
    assert!(is_running(&up));
    assert!(get(&up.host, up.port, "/").contains("<p>hi</p>"));
    // A second server with the same name is refused, and leaves no record.
    let n = reg.all().unwrap().len();
    assert_eq!(
        serve_detached(&reg, &serve, Some(Path::new(BIN)))
            .unwrap_err()
            .code,
        ErrorCode::E_SERVER
    );
    assert_eq!(reg.all().unwrap().len(), n);

    // Restart: same id and name, a new process.
    let again = restart_detached(&reg, &up, Duration::from_secs(5), Some(Path::new(BIN))).unwrap();
    assert_eq!(
        (again.id.as_str(), again.name.as_str()),
        (up.id.as_str(), "embedded")
    );
    assert_ne!(again.pid, up.pid);
    assert!(
        !pid_alive(up.pid),
        "the old worker was reaped, not left a zombie"
    );
    assert!(is_running(&again));

    // This process lives on, so the worker is our child: it must be reaped for
    // stop_server to see it go (a zombie answers kill -0), well within the grace.
    let t0 = Instant::now();
    assert!(stop_server(&reg, &again, Duration::from_secs(5)).unwrap());
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "stop waited out its grace: {:?}",
        t0.elapsed()
    );
    assert!(!pid_alive(again.pid));
    assert!(!reg.read(&again.id).unwrap().unwrap().ready);
    reg.remove(&again.id).unwrap();

    // A worker that fails to start is reported and its record removed.
    let mut bad = serve.clone();
    bad.name = None;
    bad.source = ServerSource::Dir {
        path: t.0.join("gone").display().to_string(),
    };
    assert_eq!(
        serve_detached(&reg, &bad, Some(Path::new(BIN)))
            .unwrap_err()
            .code,
        ErrorCode::E_NOT_FOUND
    );
    assert!(reg.all().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn detached_servers_serve_archive_files_and_reread_them_on_restart() {
    use sitefile::export::archive_writers::{write_zip, WriterOptions};
    use sitefile::tree::{make_file, Tree};
    isolate();
    let t = Tmp::new();
    let zip = |body: &str| {
        let tree = Tree::from_files([(
            "/index.html".to_string(),
            make_file(body.as_bytes().to_vec(), "t", 0o644),
        )])
        .unwrap();
        write_zip(&tree, &WriterOptions::default()).unwrap()
    };
    t.write("site.zip", zip("<p>one</p>"));
    let file = t.0.join("site.zip").display().to_string();
    let reg = ServerRegistry::open(Some(Store::open(t.0.join("home")).unwrap())).unwrap();

    let serve = DetachedServe::resolve(&reg.store, &file).unwrap();
    assert_eq!(serve.label, file, "the label is the file's path");
    assert_eq!(serve.source, ServerSource::Archive { path: file.clone() });
    let up = serve_detached(&reg, &serve, Some(Path::new(BIN))).unwrap();
    assert_eq!(up.image, file);
    assert!(get(&up.host, up.port, "/").contains("<p>one</p>"));

    // A new copy of the archive is picked up by a restart.
    t.write("site.zip", zip("<p>two</p>"));
    let again = restart_detached(&reg, &up, Duration::from_secs(5), Some(Path::new(BIN))).unwrap();
    assert!(get(&again.host, again.port, "/").contains("<p>two</p>"));
    assert!(stop_server(&reg, &again, Duration::from_secs(5)).unwrap());
    reg.remove(&again.id).unwrap();

    // A file that is not an archive is refused up front and leaves no record.
    t.write("notes.txt", "plain text");
    let bad =
        DetachedServe::resolve(&reg.store, &t.0.join("notes.txt").display().to_string()).unwrap();
    let e = serve_detached(&reg, &bad, Some(Path::new(BIN))).unwrap_err();
    assert_eq!(e.code, ErrorCode::E_TYPE);
    assert!(e.message().contains("notes.txt: not a recognized archive"));
    assert!(reg.all().unwrap().is_empty());
}
