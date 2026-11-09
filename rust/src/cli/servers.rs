//! `serve`, `servers`, `server stop|rm|logs|restart` and the hidden detached
//! worker (port of `src/cli/servers.ts` and `src/cli/serve-worker.ts`).
//!
//! `serve -d` re-runs this executable as `sitefile __serve-worker <id>`; the
//! worker shares the TypeScript version's state files in `$SITEFILE_HOME/servers`.

use super::args::{flag, flag_s, string, string_s, strings, OptSpec, Parsed};
use super::common::{err, js_to_number, out, pretty_json, require_args, table, usage, Command};
use crate::build_args::collect_build_args;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::executor::{build, BuildOptions, Callback, LockfileSetting};
use crate::server::{format_url, listen, listen_shared, LiveReload, SiteServerOptions};
use crate::servers::{
    check_site_files, is_running, load_served_site, resolve_source, restart_detached,
    serve_detached_with, served_last_modified, DetachedServe, ServedTreeOptions, ServerRegistry,
    ServerState,
};
pub use crate::servers::{parse_publish, stop_server};
use crate::store::{now_iso, Store};
use crate::tree::Tree;
use crate::watch::{watch_loop, BuildOutcome, WatchOptions};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ------------------------------------------------------------ signals

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: i32) {
    RECEIVED.store(sig, Ordering::SeqCst);
}

/// Catch SIGINT and SIGTERM; [`received`] then returns the signal's name.
pub(crate) fn catch_signals() {
    // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
    unsafe {
        signal(SIGINT, on_signal);
        signal(SIGTERM, on_signal);
    }
}

pub(crate) fn received() -> Option<&'static str> {
    match RECEIVED.load(Ordering::SeqCst) {
        SIGINT => Some("SIGINT"),
        SIGTERM => Some("SIGTERM"),
        _ => None,
    }
}

fn wait_for_signal() -> &'static str {
    loop {
        if let Some(s) = received() {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------ helpers

fn is_wildcard(h: &str) -> bool {
    h == "0.0.0.0" || h == "::"
}

/// `--country`: an ISO 3166-1 alpha-2 code (any case).
fn parse_country(v: Option<&str>) -> Result<Option<String>> {
    match v {
        None => Ok(None),
        Some(c) if c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Ok(Some(c.to_string()))
        }
        Some(c) => usage(format!(
            "--country expects a two-letter country code such as nz, got '{c}'"
        )),
    }
}

// ------------------------------------------------------------ serve

const SERVE_OPTS: &[OptSpec] = &[
    string("name"),
    string_s("publish", 'p'),
    flag_s("detach", 'd'),
    flag("spa"),
    string("country"),
    flag("strict-site-files"),
    flag("keep-os-metadata"),
    flag("watch"),
    string_s("file", 'f'),
    string("target"),
    strings("build-arg"),
    strings("build-arg-file"),
    flag("no-cache"),
    flag("no-lockfile"),
    flag("keep-service-workers"),
    flag("poll"),
    string("poll-interval"),
    flag("no-css-inject"),
    flag("console"),
    flag("no-overlay"),
    flag("navigate-to-changed"),
    string("reload"),
];
/// serve flags that only mean something with --watch.
const WATCH_ONLY: &[&str] = &[
    "file",
    "target",
    "build-arg",
    "build-arg-file",
    "no-cache",
    "no-lockfile",
    "keep-service-workers",
    "poll",
    "poll-interval",
    "no-css-inject",
    "console",
    "no-overlay",
    "navigate-to-changed",
    "reload",
];
pub const SERVE: Command = Command {
    name: "serve",
    summary: "serve an image, directory or archive file over HTTP",
    usage: "serve [--name n] [-p [host:]port] [-d] [--spa] <ref|id|./dir|file>  |  serve --watch [options] [context]",
    description: "Serve an image from the local store, a directory (paths starting with ./, ../ or /), or an archive file over HTTP:\nzip, tar, tar.gz, wbn, or an OCI archive as build -o x.oci.tar and save write it (the format is read from the\nbytes). A directory or archive is a snapshot read at start. GET and HEAD only; no directory listings.\nBinds 127.0.0.1 on a free port unless -p is given;\n0.0.0.0 is bound only when you pass it explicitly. Foreground by default (Ctrl-C stops it); -d detaches and\nrecords the server for servers, server stop, rm, logs and restart.\n\nWith --watch the argument is a build context (default .): its Sitefile is built in memory and served, rebuilt\nwhen the context, the Sitefile or a --build-arg-file changes, and open pages reload after each good rebuild\n(page loads get a small script, /__sitefile/reload.js). A failed rebuild keeps serving the last\ngood build. Nothing is written to the context except Sitefile.lock. A directory with no Sitefile (and no -f,\n--target or --build-arg*) is served as it is and re-read on change. When only stylesheets changed, pages\nswap them in place.",
    flags: &[
        "--name <n>        name for the server (-d only; default site-<6 hex>)",
        "-p, --publish <[host:]port>  address to bind (default 127.0.0.1, a free port)",
        "-d, --detach      run in the background and print the server ID",
        "--spa             serve /index.html for unknown paths without a file extension",
        "--country <code>  the country _redirects Country= conditions match (there is no geolocation locally)",
        "--strict-site-files  refuse to serve when a _redirects or _headers line would be ignored",
        "--keep-os-metadata  serve __MACOSX, ._*, Thumbs.db and desktop.ini of a directory or archive (default: skipped)",
        "--watch           build the context, serve it, rebuild on change and live-reload pages (foreground only)",
        "-f, --file <path>            Sitefile path (--watch; default <context>/Sitefile)",
        "--target <stage>             build up to this stage and serve its whole tree (--watch)",
        "--build-arg <NAME=value>     set an ARG (--watch; repeatable)",
        "--build-arg-file <path>      read ARGs from a .json, .yaml/.yml or .env file (--watch; repeatable)",
        "--no-cache        do not use the TRANSFORM result cache (--watch)",
        "--no-lockfile     do not read or write Sitefile.lock (--watch)",
        "--keep-service-workers       do not unregister service workers and clear caches before a reload (--watch)",
        "--poll            poll for changes (every 1000 ms) instead of file system events (--watch)",
        "--poll-interval <ms>         poll every <ms> milliseconds (--watch; implies --poll)",
        "--no-css-inject   reload the page when only stylesheets changed, instead of swapping them (--watch)",
        "--console         also forward console.log/info from pages to the log, not only errors and warnings (--watch)",
        "--no-overlay      no Rebuilding badge or build-error overlay in pages (--watch)",
        "--navigate-to-changed        after a rebuild that changed one page, open pages go to it (--watch)",
        "--reload <full|morph>        morph (default): update a page in place when only HTML changed; full: always reload (--watch)",
    ],
    options: SERVE_OPTS,
    run: run_serve,
};

fn run_serve(v: &Parsed) -> Result<i32> {
    if v.flag("watch") {
        return serve_watch(v);
    }
    for f in WATCH_ONLY {
        if v.has(f) {
            return usage(format!("--{f} needs --watch"));
        }
    }
    require_args(
        &v.positionals,
        1,
        1,
        "an image reference, ./directory or archive file",
    )?;
    let bind = parse_publish(v.str("publish"))?;
    let country = parse_country(v.str("country"))?;
    let strict_site_files = v.flag("strict-site-files");
    let name = v.str("name");
    if name.is_some() && !v.flag("detach") {
        return usage("--name needs -d (only detached servers are tracked)");
    }
    let store = Store::open_default()?;
    let (label, source) = resolve_source(&store, &v.positionals[0])?;
    if is_wildcard(&bind.host) {
        err(&format!(
            "warning: binding {}: the site will be reachable from other machines\n",
            bind.host
        ));
    }

    let keep_os_metadata = v.flag("keep-os-metadata");
    let warn = |m: &str| err(&format!("warning: {m}\n"));
    if !v.flag("detach") {
        let site = load_served_site(
            &store,
            &source,
            &ServedTreeOptions {
                keep_os_metadata,
                warn: Some(&warn),
            },
        )?;
        let tree = site.tree;
        check_site_files(&tree, strict_site_files, &warn, site.content.as_deref())?;
        let modified = served_last_modified(&store, &source);
        let n = tree.len();
        catch_signals();
        let log: crate::server::LogFn = Arc::new(|l: &str| out(&format!("{l}\n")));
        let server = listen(
            Arc::new(tree),
            SiteServerOptions {
                spa: v.flag("spa"),
                country: country.clone(),
                log: Some(log),
                server_id: None,
                live_reload: None,
                last_modified: Some(Arc::new(move || modified)),
                warn: None,
                content: site.content,
                ..Default::default()
            },
            &bind.host,
            bind.port,
        )?;
        out(&format!(
            "Serving {label} ({n} files) at {}  (Ctrl-C to stop)\n",
            format_url(&server.host, server.port)
        ));
        wait_for_signal();
        server.close();
        return Ok(0);
    }

    let reg = ServerRegistry::open(Some(store))?;
    let up = serve_detached_with(
        &reg,
        &DetachedServe {
            label: label.clone(),
            source,
            name: name.map(str::to_string),
            bind,
            spa: v.flag("spa"),
            keep_os_metadata,
            country,
            strict_site_files,
            project: None,
        },
        None,
        &warn,
    )?;
    out(&format!(
        "Started {} ({}) serving {label} at {}\n",
        up.name,
        up.id,
        format_url(&up.host, up.port)
    ));
    Ok(0)
}

/// `serve --watch [context]`: build in memory, serve, rebuild on change, live reload.
fn serve_watch(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 1, "a build context directory")?;
    if v.flag("detach") {
        return usage("--watch cannot be combined with -d");
    }
    if v.has("name") {
        return usage("--name needs -d (only detached servers are tracked)");
    }
    let bind = parse_publish(v.str("publish"))?;
    let context = v.positionals.first().cloned().unwrap_or_else(|| ".".into());
    let file = v.str("file").filter(|s| !s.is_empty()).map(str::to_string);
    let arg_files = v.strs("build-arg-file");
    let arg_flags = v.strs("build-arg");
    let poll = crate::watch::poll_option(v.flag("poll"), v.str("poll-interval"))?;
    // A directory without a Sitefile, given no build flags, is served as it is (re-read on change).
    let build_flags =
        file.is_some() || v.has("target") || !arg_files.is_empty() || !arg_flags.is_empty();
    let plain = !build_flags && !std::path::Path::new(&context).join("Sitefile").is_file();
    let css_inject = !v.flag("no-css-inject");
    let navigate = v.flag("navigate-to-changed");
    let morph = match v.str("reload").unwrap_or("morph") {
        "morph" => true,
        "full" => false,
        other => return usage(format!("--reload expects full or morph, got '{other}'")),
    };
    let country = parse_country(v.str("country"))?;
    let strict_site_files = v.flag("strict-site-files");
    // The site-file warnings last printed: a rebuild repeats them only when they change.
    let last_site_warnings = std::cell::RefCell::new(String::new());
    let site_files = |tree: &Tree| -> Result<()> {
        let shown = crate::server::site_file_warnings(tree, None);
        let text = shown.join("\n");
        if text != *last_site_warnings.borrow() {
            for m in &shown {
                err(&format!("warning: {m}\n"));
            }
        }
        *last_site_warnings.borrow_mut() = text;
        check_site_files(tree, strict_site_files, &|_| {}, None)
    };
    if is_wildcard(&bind.host) {
        err(&format!(
            "warning: binding {}: the site will be reachable from other machines\n",
            bind.host
        ));
    }
    catch_signals();

    let current: crate::server::SharedTree = Arc::new(RwLock::new(Arc::new(Tree::empty())));
    // Last-Modified: the time of the build being served.
    let built_at: Arc<RwLock<Option<i64>>> = Arc::new(RwLock::new(None));
    let live = LiveReload::with_options(&crate::server::LiveReloadOptions {
        reset_service_workers: !v.flag("keep-service-workers"),
        css_inject,
        console: v.flag("console"),
        overlay: !v.flag("no-overlay"),
        navigate_to_changed: navigate,
        morph,
    });
    let mut server: Option<crate::server::ListeningServer> = None;
    let started = std::cell::Cell::new(false);
    let memo = crate::memo::watch_memo();
    // Serve `next` and tell open pages what changed.
    let swap = |next: Tree| {
        let Ok(mut t) = current.write() else { return };
        let event = if started.get() {
            Some(
                crate::server::reload_event_for(&t, &next, css_inject, navigate)
                    .unwrap_or_else(|| crate::server::OK_EVENT.to_string()),
            )
        } else {
            None
        };
        *t = Arc::new(next);
        drop(t);
        if let Ok(mut b) = built_at.write() {
            *b = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64);
        }
        if let Some(e) = event {
            live.send(&e);
        }
    };
    // A failed build or read: print it, and tell the pages (and pages opened later).
    let failed = |e: &SitefileError| -> BuildOutcome {
        let code = super::common::report(e);
        if e.code != E_USAGE {
            live.send(&crate::server::error_event(e));
        }
        BuildOutcome {
            code,
            wrote: Vec::new(),
        }
    };
    let mut run_build = || -> BuildOutcome {
        if started.get() {
            live.send(crate::server::BUILDING_EVENT);
        }
        if plain {
            return match crate::servers::load_directory_tree(std::path::Path::new(&context)) {
                Ok(tree) => {
                    let n = tree.len();
                    out(&format!(
                        "Read {n} file{} ({})\n",
                        if n == 1 { "" } else { "s" },
                        tree.digest()
                    ));
                    if let Err(e) = site_files(&tree) {
                        return failed(&e);
                    }
                    swap(tree);
                    BuildOutcome {
                        code: 0,
                        wrote: Vec::new(),
                    }
                }
                Err(e) => failed(&e),
            };
        }
        let attempt = || -> Result<Tree> {
            let mut options = BuildOptions {
                context_dir: context.clone(),
                sitefile: file.clone(),
                build_args: collect_build_args(&arg_files, &arg_flags)?,
                ..Default::default()
            };
            options.target = v.str("target").map(str::to_string);
            if v.flag("no-lockfile") {
                options.lockfile = LockfileSetting::Disabled;
            }
            if !v.flag("no-cache") {
                options.cache_dir = Some(super::cache::transform_cache_dir());
            }
            options.skip_base = true;
            options.keep_os_metadata = v.flag("keep-os-metadata");
            options.memo = memo.clone();
            let notice: Callback = Arc::new(|m: &str| err(&format!("{m}\n")));
            options.on_notice = Some(notice);
            let warning: Callback = Arc::new(|m: &str| err(&format!("warning: {m}\n")));
            options.on_warning = Some(warning);
            Ok(build(&options)?.output)
        };
        match attempt() {
            Ok(tree) => {
                let n = tree.len();
                out(&format!(
                    "Built {n} file{} ({})\n",
                    if n == 1 { "" } else { "s" },
                    tree.digest()
                ));
                if let Err(e) = site_files(&tree) {
                    return failed(&e);
                }
                swap(tree);
                BuildOutcome {
                    code: 0,
                    wrote: Vec::new(),
                }
            }
            Err(e) => failed(&e),
        }
    };
    let mut on_started = || -> Result<()> {
        let log: crate::server::LogFn = Arc::new(|l: &str| out(&format!("{l}\n")));
        let opts = SiteServerOptions {
            spa: v.flag("spa"),
            country: country.clone(),
            log: Some(log),
            server_id: None,
            live_reload: Some(live.clone()),
            last_modified: Some(Arc::new({
                let built_at = built_at.clone();
                move || built_at.read().ok().and_then(|t| *t)
            })),
            warn: Some(Arc::new(|m: &str| err(&format!("warning: {m}\n")))),
            content: None,
        };
        let s = listen_shared(current.clone(), opts, &bind.host, bind.port)?;
        let n = current.read().map(|t| t.len()).unwrap_or(0);
        out(&format!(
            "Serving {context} ({n} files) at {}  (Ctrl-C to stop)\n",
            format_url(&s.host, s.port)
        ));
        server = Some(s);
        started.set(true);
        Ok(())
    };
    let code = watch_loop(
        &WatchOptions {
            context: &context,
            sitefile: file.as_deref(),
            arg_files: &arg_files,
            poll,
            plain,
        },
        &mut run_build,
        &mut on_started,
        &|| received().is_some(),
    )?;
    live.close();
    if let Some(s) = server {
        s.close();
    }
    Ok(code)
}

/// `sitefile __serve-worker <id>`: the detached server process.
pub fn serve_worker(id: &str) -> i32 {
    let stamp = |m: &str| out(&format!("{} {m}\n", now_iso()));
    let reg = match ServerRegistry::open(None) {
        Ok(r) => r,
        Err(e) => {
            err(&format!("{e}\n"));
            return 1;
        }
    };
    let state = match reg.read(id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            err(&format!("error[E_SERVER]: no state for server '{id}'\n"));
            return 1;
        }
        Err(e) => {
            err(&format!("{e}\n"));
            return 1;
        }
    };
    catch_signals();
    let started = (|| -> Result<crate::server::ListeningServer> {
        let warn = |m: &str| stamp(&format!("warning: {m}"));
        let site = load_served_site(
            &reg.store,
            &state.source,
            &ServedTreeOptions {
                keep_os_metadata: state.keep_os_metadata,
                warn: Some(&warn),
            },
        )?;
        let tree = site.tree;
        check_site_files(
            &tree,
            state.strict_site_files,
            &warn,
            site.content.as_deref(),
        )?;
        let modified = served_last_modified(&reg.store, &state.source);
        let n = tree.len();
        let log: crate::server::LogFn = Arc::new(|l: &str| out(&format!("{} {l}\n", now_iso())));
        let server = listen(
            Arc::new(tree),
            SiteServerOptions {
                spa: state.spa,
                country: state.country.clone(),
                log: Some(log),
                server_id: Some(id.to_string()),
                live_reload: None,
                last_modified: Some(Arc::new(move || modified)),
                warn: None,
                content: site.content,
                ..Default::default()
            },
            &state.bind.host,
            state.bind.port,
        )?;
        let (host, port) = (server.host.clone(), server.port);
        reg.update(id, |s| {
            s.pid = std::process::id() as i64;
            s.host = host.clone();
            s.port = port;
            s.ready = true;
            s.error = None;
        })?;
        stamp(&format!(
            "serving {} ({n} files) at {}",
            state.image,
            format_url(&host, port)
        ));
        Ok(server)
    })();
    match started {
        Ok(server) => {
            let sig = wait_for_signal();
            stamp(&format!("received {sig}, shutting down"));
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                server.close();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(3));
            let _ = reg.update(id, |s| s.ready = false);
            0
        }
        Err(e) => {
            let message = format!("error[{}]: {}", e.code, e.message());
            err(&format!("{} {message}\n", now_iso()));
            let _ = reg.update(id, |s| {
                s.pid = 0;
                s.ready = false;
                s.error = Some(message.clone());
            });
            1
        }
    }
}

// ------------------------------------------------------------ servers

const PS_OPTS: &[OptSpec] = &[flag_s("all", 'a'), flag("json")];
pub const PS: Command = Command {
    name: "servers",
    summary: "list site servers (also: server ls)",
    usage: "servers [-a] [--json]",
    description: "List running site servers (-a: also exited ones). A server is running only if its process is alive\nand its port answers; stale entries show as exited.",
    flags: &["-a, --all   include exited servers", "--json      machine-readable output"],
    options: PS_OPTS,
    run: run_ps,
};

fn run_ps(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 0, 0, "no arguments")?;
    let reg = ServerRegistry::open(None)?;
    let views: Vec<_> = reg
        .all()?
        .iter()
        .map(|s| reg.view(s))
        .filter(|s| v.flag("all") || s.status == "running")
        .collect();
    if v.flag("json") {
        // 'site' is the image reference or directory being served.
        let arr: Vec<Value> = views
            .iter()
            .map(|s| {
                let mut m = Map::new();
                if let Ok(Value::Object(st)) = serde_json::to_value(&s.state) {
                    for (k, val) in st {
                        if k != "image" {
                            m.insert(k, val);
                        }
                    }
                }
                m.insert("status".into(), Value::String(s.status.into()));
                m.insert("url".into(), Value::String(s.url.clone()));
                m.insert("site".into(), Value::String(s.state.image.clone()));
                Value::Object(m)
            })
            .collect();
        out(&(pretty_json(&Value::Array(arr)) + "\n"));
        return Ok(0);
    }
    let mut rows = vec![["NAME", "SITE", "URL", "STATUS", "ID"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()];
    for s in &views {
        rows.push(vec![
            s.state.name.clone(),
            s.state.image.clone(),
            if s.url.is_empty() {
                "-".into()
            } else {
                s.url.clone()
            },
            s.status.to_string(),
            s.state.id.clone(),
        ]);
    }
    out(&table(&rows));
    Ok(0)
}

pub fn grace_of(v: &Parsed) -> Result<Duration> {
    let Some(t) = v.str("time") else {
        return Ok(Duration::from_secs(5));
    };
    let n = js_to_number(t);
    if !n.is_finite() || n < 0.0 {
        return usage("--time expects a number of seconds");
    }
    Ok(Duration::from_secs_f64(n))
}

/// Run `f` for each target; print each failure and keep going.
fn each_target(
    reg: &ServerRegistry,
    keys: &[String],
    f: impl Fn(&ServerState) -> Result<String>,
) -> Result<i32> {
    let mut failed = false;
    for key in keys {
        match reg.find(key).and_then(|s| f(&s)) {
            Ok(line) => out(&format!("{line}\n")),
            Err(e) => {
                err(&format!("error[{}]: {}\n", e.code, e.message()));
                failed = true;
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

const TIME_OPTS: &[OptSpec] = &[string("time")];
pub const STOP: Command = Command {
    name: "stop",
    summary: "stop running site servers",
    usage: "server stop [--time secs] <name|id>...",
    description: "Send SIGTERM, wait up to --time seconds (default 5), then SIGKILL. Stopping an exited server is a no-op.",
    flags: &["--time <secs>   grace period before SIGKILL (default 5)"],
    options: TIME_OPTS,
    run: run_stop,
};

fn run_stop(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, usize::MAX, "a server id or name")?;
    let reg = ServerRegistry::open(None)?;
    let grace = grace_of(v)?;
    each_target(&reg, &v.positionals, |s| {
        stop_server(&reg, s, grace)?;
        Ok(s.name.clone())
    })
}

const RM_OPTS: &[OptSpec] = &[flag_s("force", 'f')];
pub const RM: Command = Command {
    name: "rm",
    summary: "remove site servers",
    usage: "server rm [-f] <name|id>...",
    description: "Delete the record and log of exited servers. Running servers are refused unless -f, which stops them first.",
    flags: &["-f, --force   stop and remove running servers"],
    options: RM_OPTS,
    run: run_rm,
};

fn run_rm(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, usize::MAX, "a server id or name")?;
    let reg = ServerRegistry::open(None)?;
    let force = v.flag("force");
    each_target(&reg, &v.positionals, |s| {
        if is_running(s) {
            if !force {
                return Err(SitefileError::plain(
                    E_SERVER,
                    format!("{} is running; stop it first or use -f", s.name),
                ));
            }
            stop_server(&reg, s, Duration::from_secs(5))?;
        }
        reg.remove(&s.id)?;
        Ok(s.name.clone())
    })
}

const LOGS_OPTS: &[OptSpec] = &[flag_s("follow", 'f'), string_s("tail", 'n')];
pub const LOGS: Command = Command {
    name: "logs",
    summary: "show a server's access log",
    usage: "server logs [-f] [-n lines] <name|id>",
    description: "Print the log of a detached server: one line per request (\"METHOD /path STATUS BYTES MSms\").",
    flags: &["-f, --follow    keep printing new lines until Ctrl-C", "-n, --tail <n>  only the last n lines"],
    options: LOGS_OPTS,
    run: run_logs,
};

/// The last n lines of text, each newline-terminated; a trailing newline is not an extra empty line.
pub fn tail_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let keep = if n == 0 {
        &[][..]
    } else {
        &lines[lines.len().saturating_sub(n)..]
    };
    keep.iter().map(|l| format!("{l}\n")).collect()
}

fn run_logs(v: &Parsed) -> Result<i32> {
    use std::io::{Read, Seek, SeekFrom};
    require_args(&v.positionals, 1, 1, "a server id or name")?;
    let reg = ServerRegistry::open(None)?;
    let s = reg.find(&v.positionals[0])?;
    let raw = std::fs::read(&s.log_file).unwrap_or_default();
    let offset = raw.len() as u64;
    let mut text = String::from_utf8_lossy(&raw).into_owned();
    if let Some(tail) = v.str("tail") {
        let n = js_to_number(tail);
        if !super::common::is_safe_integer(n) || n < 0.0 {
            return usage("--tail expects a non-negative integer");
        }
        text = tail_lines(&text, n as usize);
    }
    out(&text);
    if !v.flag("follow") {
        return Ok(0);
    }
    catch_signals();
    let mut pos = offset;
    while received().is_none() {
        std::thread::sleep(Duration::from_millis(150));
        let Ok(mut f) = std::fs::File::open(&s.log_file) else {
            continue;
        };
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        if size < pos {
            pos = 0;
        }
        if size > pos {
            let mut buf = vec![0u8; (size - pos) as usize];
            if f.seek(SeekFrom::Start(pos)).is_ok() && f.read_exact(&mut buf).is_ok() {
                pos = size;
                use std::io::Write;
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(&buf);
                let _ = o.flush();
            }
        }
    }
    Ok(0)
}

pub const RESTART: Command = Command {
    name: "restart",
    summary: "restart a site server",
    usage: "server restart [--time secs] <name|id>",
    description: "Stop the server if it is running, then start it again with the same name, image and bind address.\nImages are served by the same image ID; a directory or archive file is re-read.",
    flags: &["--time <secs>   grace period before SIGKILL (default 5)"],
    options: TIME_OPTS,
    run: run_restart,
};

fn run_restart(v: &Parsed) -> Result<i32> {
    require_args(&v.positionals, 1, 1, "a server id or name")?;
    let reg = ServerRegistry::open(None)?;
    let s = reg.find(&v.positionals[0])?;
    let up = restart_detached(&reg, &s, grace_of(v)?, None)?;
    out(&format!(
        "Restarted {} ({}) serving {} at {}\n",
        up.name,
        up.id,
        up.image,
        format_url(&up.host, up.port)
    ));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::Bind;

    #[test]
    fn publish_specs() {
        assert_eq!(
            parse_publish(None).unwrap(),
            Bind {
                host: "127.0.0.1".into(),
                port: 0
            }
        );
        assert_eq!(parse_publish(Some("8080")).unwrap().port, 8080);
        assert_eq!(
            parse_publish(Some("localhost:80")).unwrap().host,
            "127.0.0.1"
        );
        assert_eq!(parse_publish(Some("[::1]:80")).unwrap().host, "::1");
        assert_eq!(parse_publish(Some("0.0.0.0:1")).unwrap().host, "0.0.0.0");
        assert!(parse_publish(Some("99999"))
            .unwrap_err()
            .detail
            .contains("out of range"));
        assert!(parse_publish(Some("example.com:80"))
            .unwrap_err()
            .detail
            .contains("must be an IP"));
        assert!(parse_publish(Some("::1:80")).is_err());
    }
}

#[cfg(test)]
mod tail_tests {
    use super::tail_lines;

    #[test]
    fn tail_with_and_without_trailing_newline() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc\n");
        assert_eq!(tail_lines("a\nb\nc", 10), "a\nb\nc\n");
        assert_eq!(tail_lines("a\nb\nc\n", 0), "");
        assert_eq!(tail_lines("", 3), "");
        assert_eq!(tail_lines("a\n\n", 1), "\n");
    }
}
