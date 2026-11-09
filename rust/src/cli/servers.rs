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
    is_running, load_served_tree_with, resolve_source, restart_detached, serve_detached_with,
    served_last_modified, DetachedServe, ServedTreeOptions, ServerRegistry, ServerState,
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

/// [`wait_for_signal`], or `None` once `deadline` passes.
fn wait_for_signal_until(deadline: Option<std::time::Instant>) -> Option<&'static str> {
    loop {
        if let Some(s) = received() {
            return Some(s);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------ helpers

fn is_wildcard(h: &str) -> bool {
    h == "0.0.0.0" || h == "::"
}

// ------------------------------------------------------------ serve

/// `--allowed-host` values, validated and lowercased (the server always adds localhost names and
/// IP addresses).
fn isolate_of(v: &Parsed) -> Result<Option<String>> {
    let coep = v.str("coep");
    if coep.is_some() && !v.flag("isolate") {
        return usage("--coep needs --isolate");
    }
    if let Some(c) = coep.filter(|c| *c != "require-corp" && *c != "credentialless") {
        return usage(format!(
            "--coep must be require-corp or credentialless, not '{c}'"
        ));
    }
    Ok(v.flag("isolate")
        .then(|| coep.unwrap_or("require-corp").to_string()))
}

/// `--max-age`, `--immutable`, `--max-age-html`, or `None` when none is given.
fn cache_of(v: &Parsed) -> Result<Option<crate::server::CachePreview>> {
    let secs = |flag: &str| -> Result<Option<u64>> {
        let Some(raw) = v.str(flag) else {
            return Ok(None);
        };
        match raw.parse::<u64>() {
            Ok(n)
                if (1..=10).contains(&raw.len())
                    && raw.bytes().all(|b| b.is_ascii_digit())
                    && n <= 2_147_483_647 =>
            {
                Ok(Some(n))
            }
            _ => usage(format!(
                "--{flag} must be a number of seconds (0 to 2147483647), not '{raw}'"
            )),
        }
    };
    let max_age = secs("max-age")?;
    let max_age_html = secs("max-age-html")?;
    if v.flag("immutable") && max_age.is_none() {
        return usage("--immutable needs --max-age");
    }
    if max_age.is_none() && max_age_html.is_none() {
        return Ok(None);
    }
    Ok(Some(crate::server::CachePreview {
        max_age,
        immutable: v.flag("immutable"),
        max_age_html,
    }))
}

/// What `--auth`, `--auth-file` and `SITEFILE_SERVE_AUTH` give: the server's checker, the verifiers
/// and the absolute htpasswd path the record keeps (never a password).
type AuthSetup = (
    Option<Arc<crate::serve_auth::BasicAuth>>,
    Vec<crate::serve_auth::AuthEntry>,
    Option<String>,
);

fn auth_of(v: &Parsed, bind_host: &str) -> Result<AuthSetup> {
    use crate::serve_auth::{
        make_verifier, parse_auth_pair, parse_htpasswd, AuthEntry, BasicAuth, AUTH_ITERATIONS,
    };
    let mut pairs = Vec::new();
    for a in v.strs("auth") {
        pairs.push(parse_auth_pair(&a, "--auth")?);
    }
    if let Ok(env) = std::env::var("SITEFILE_SERVE_AUTH") {
        if !env.is_empty() {
            pairs.push(parse_auth_pair(&env, "SITEFILE_SERVE_AUTH")?);
        }
    }
    let entries: Vec<AuthEntry> = pairs
        .into_iter()
        .map(|(user, pw)| AuthEntry {
            user,
            verifier: make_verifier(&pw, None, AUTH_ITERATIONS),
        })
        .collect();
    let auth_file = v.str("auth-file").map(str::to_string);
    let from_file = match &auth_file {
        Some(f) => {
            let Ok(text) = std::fs::read_to_string(f) else {
                return usage(format!("--auth-file: cannot read {f}"));
            };
            parse_htpasswd(&text, f)?
        }
        None => Vec::new(),
    };
    if entries.is_empty() && auth_file.is_none() {
        return Ok((None, Vec::new(), None));
    }
    let loopback = bind_host == "localhost" || bind_host == "::1" || bind_host.starts_with("127.");
    if !loopback {
        err(&format!("warning: basic authentication over plain HTTP on {bind_host}: the password crosses the network in the clear\n"));
    }
    let abs = auth_file.map(|f| {
        std::path::absolute(&f)
            .map(|p| p.display().to_string())
            .unwrap_or(f)
    });
    let mut all = entries.clone();
    all.extend(from_file);
    Ok((Some(Arc::new(BasicAuth::new(all))), entries, abs))
}

/// `--max-connections-per-ip <n>` (0 when absent: no limit).
fn per_ip_of(v: &Parsed) -> Result<usize> {
    let Some(raw) = v.str("max-connections-per-ip") else {
        return Ok(0);
    };
    if !(1..=6).contains(&raw.len()) || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return usage(format!(
            "--max-connections-per-ip must be a number (0 to 999999), not '{raw}'"
        ));
    }
    Ok(raw.parse().unwrap_or(0))
}

/// `  Network: <url>` lines for a server bound to all interfaces (none otherwise).
fn network_lines(host: &str, port: u16, base_path: Option<&str>, tls: bool) -> String {
    crate::server::network_urls(host, port)
        .into_iter()
        .map(|u| {
            format!(
                "  Network: {}\n",
                with_base(&crate::server::with_scheme(u, tls), base_path)
            )
        })
        .collect()
}

/// `--tls-cert`, `--tls-key` (or `SITEFILE_TLS_CERT`, `SITEFILE_TLS_KEY`): the server's TLS
/// configuration and the absolute paths the record keeps (`tlsOf` in src/cli/servers.ts).
#[allow(clippy::type_complexity)]
fn tls_of(v: &Parsed) -> Result<(Option<Arc<rustls::ServerConfig>>, Option<(String, String)>)> {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let cert = v
        .str("tls-cert")
        .map(str::to_string)
        .or_else(|| env("SITEFILE_TLS_CERT"));
    let key = v
        .str("tls-key")
        .map(str::to_string)
        .or_else(|| env("SITEFILE_TLS_KEY"));
    let (cert, key) = match (cert, key) {
        (None, None) => return Ok((None, None)),
        (Some(c), Some(k)) => (c, k),
        _ => return usage("--tls-cert and --tls-key go together"),
    };
    let read = |f: &str, flag: &str| -> Result<Vec<u8>> {
        std::fs::read(f)
            .map_err(|_| SitefileError::plain(E_USAGE, format!("--{flag}: cannot read {f}")))
    };
    let config = crate::server::tls_config(&read(&cert, "tls-cert")?, &read(&key, "tls-key")?)?;
    let abs = |f: String| {
        std::path::absolute(&f)
            .map(|p| p.display().to_string())
            .unwrap_or(f)
    };
    Ok((Some(config), Some((abs(cert), abs(key)))))
}

/// A server URL (`http://host:port/`) under a --random-route prefix.
fn with_base(url: &str, base_path: Option<&str>) -> String {
    match base_path {
        Some(p) => format!("{}{p}/", &url[..url.len() - 1]),
        None => url.to_string(),
    }
}

/// The URL a server is reached at locally (its --random-route prefix included).
fn site_url(host: &str, port: u16, base_path: Option<&str>, tls: bool) -> String {
    with_base(
        &crate::server::with_scheme(format_url(host, port), tls),
        base_path,
    )
}

/// `--random-route`, `--allow-ip`, `--deny-ip`, `--stop-after` (`sharingOf` in src/cli/servers.ts).
struct Sharing {
    base_path: Option<String>,
    ip_rules: Option<crate::server::IpRules>,
    noindex: bool,
    allow_text: Vec<String>,
    deny_text: Vec<String>,
    stop_after: Option<(u64, String)>,
}

/// `--throttle`: the spec as given and its limits.
fn throttle_of(v: &Parsed) -> Result<Option<(String, crate::server::Throttle)>> {
    let Some(spec) = v.str("throttle") else {
        return Ok(None);
    };
    match crate::server::parse_throttle(spec) {
        Some(t) => Ok(Some((spec.to_string(), t))),
        None => usage(format!(
            "--throttle expects 3g, slow-3g or <kbit/s>[/<latency ms>], not '{spec}'"
        )),
    }
}

/// `--mime-file` entries, then `--mime` values, in order (`mimeOf` in src/cli/servers.ts).
fn mime_of(v: &Parsed) -> Result<Option<Vec<(String, String)>>> {
    let file = v.str("mime-file");
    let flags = v.strs("mime");
    if file.is_none() && flags.is_empty() {
        return Ok(None);
    }
    let mut out = match file {
        Some(f) => {
            let Ok(text) = std::fs::read_to_string(f) else {
                return usage(format!("--mime-file: cannot read {f}"));
            };
            crate::server::parse_mime_types(&text, f)?
        }
        None => Vec::new(),
    };
    for f in flags {
        let Some((ext, t)) = crate::server::parse_mime_flag(&f) else {
            return usage(format!("--mime expects ext=type/subtype, not '{f}'"));
        };
        match out.iter_mut().find(|(k, _)| *k == ext) {
            Some(slot) => slot.1 = t,
            None => out.push((ext, t)),
        }
    }
    Ok(Some(out))
}

fn mime_map(pairs: &[(String, String)]) -> Arc<std::collections::HashMap<String, String>> {
    Arc::new(pairs.iter().cloned().collect())
}

/// The start message's `  Throttle: …` line (none without --throttle).
fn throttle_line(spec: Option<&str>) -> String {
    match spec.and_then(crate::server::parse_throttle) {
        Some(t) => format!(
            "  Throttle: {} ({} kbit/s, {} ms latency)\n",
            spec.unwrap_or(""),
            t.bytes_per_second / 125,
            t.latency_ms
        ),
        None => String::new(),
    }
}

fn sharing_of(v: &Parsed, bind_host: &str) -> Result<Sharing> {
    let cidrs = |flag: &str| -> Result<(Vec<String>, Vec<crate::server::Cidr>)> {
        let text = v.strs(flag);
        let mut parsed = Vec::new();
        for t in &text {
            match crate::server::parse_cidr(t) {
                Some(c) => parsed.push(c),
                None => return usage(format!("--{flag}: invalid address or CIDR block '{t}'")),
            }
        }
        Ok((text, parsed))
    };
    let (allow_text, allow) = cidrs("allow-ip")?;
    let (deny_text, deny) = cidrs("deny-ip")?;
    let base_path = v
        .flag("random-route")
        .then(|| format!("/{}", crate::export::exporter::random_hex(12)));
    let stop_after = match v.str("stop-after") {
        None => None,
        Some(raw) => {
            let (num, unit) = raw.split_at(raw.len().saturating_sub(1));
            let mult = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 3600,
                "d" => 86400,
                _ => 0,
            };
            let n: u64 = if (1..=6).contains(&num.len()) && num.bytes().all(|b| b.is_ascii_digit())
            {
                num.parse().unwrap_or(0)
            } else {
                0
            };
            if mult == 0 || n == 0 {
                return usage(format!(
                    "--stop-after expects a duration like 90s, 30m, 2h or 1d, not '{raw}'"
                ));
            }
            Some((n * mult, raw.to_string()))
        }
    };
    let loopback = bind_host == "localhost" || bind_host == "::1" || bind_host.starts_with("127.");
    let ip_rules =
        (!allow.is_empty() || !deny.is_empty()).then_some(crate::server::IpRules { allow, deny });
    Ok(Sharing {
        base_path,
        ip_rules,
        noindex: !loopback,
        allow_text,
        deny_text,
        stop_after,
    })
}

/// `--log-format`, `--log-ip`, `--quiet`: (json, log_ip, quiet).
fn logging_of(v: &Parsed) -> Result<(bool, bool, bool)> {
    let format = v.str("log-format");
    if let Some(f) = format.filter(|f| *f != "text" && *f != "json") {
        return usage(format!("--log-format must be text or json, not '{f}'"));
    }
    Ok((format == Some("json"), v.flag("log-ip"), v.flag("quiet")))
}

/// Where a foreground server's log lines go: stdout, other lines wrapped as JSON objects under
/// `--log-format json`.
fn log_writer(json: bool) -> crate::server::LogFn {
    Arc::new(move |l: &str| {
        if json && !l.starts_with('{') {
            out(&format!(
                "{}\n",
                crate::server::json_log_line(&now_iso(), l)
            ));
        } else {
            out(&format!("{l}\n"));
        }
    })
}

fn allowed_hosts_of(v: &Parsed) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in v.strs("allowed-host") {
        let Some(h) = crate::server::parse_allowed_host(&raw) else {
            return usage(format!("--allowed-host: invalid host name '{raw}'"));
        };
        if !out.contains(&h) {
            out.push(h);
        }
    }
    Ok(out)
}

const SERVE_OPTS: &[OptSpec] = &[
    string("name"),
    string_s("publish", 'p'),
    flag_s("detach", 'd'),
    flag("spa"),
    flag("keep-os-metadata"),
    strings("allowed-host"),
    flag("cors"),
    flag("isolate"),
    string("coep"),
    string("max-age"),
    flag("immutable"),
    string("max-age-html"),
    string("log-format"),
    flag("log-ip"),
    flag("quiet"),
    strings("auth"),
    string("auth-file"),
    string("max-connections-per-ip"),
    flag("random-route"),
    strings("allow-ip"),
    strings("deny-ip"),
    string("stop-after"),
    string("tls-cert"),
    flag("compress"),
    string("throttle"),
    strings("mime"),
    string("mime-file"),
    string("tls-key"),
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
        "--cors            Access-Control-Allow-Origin: * on every response and OPTIONS preflights (204)",
        "--isolate         cross-origin isolation headers (COOP same-origin, COEP, CORP same-origin) on every response",
        "--coep <require-corp|credentialless>  the Cross-Origin-Embedder-Policy of --isolate (default require-corp)",
        "--max-age <secs>  preview production caching: Cache-Control: public, max-age=<secs> on files other than HTML",
        "--immutable       add immutable to the --max-age value (fingerprinted assets)",
        "--max-age-html <secs>        Cache-Control: public, max-age=<secs> on HTML pages too (default: they keep no-cache)",
        "--log-format <text|json>     access-log lines as text (default) or one JSON object per line (a stable format; docs/CLI.md)",
        "--log-ip          add the client address to access-log lines",
        "--quiet           no access-log lines",
        "--auth <user:password>       require HTTP basic authentication (repeatable; or SITEFILE_SERVE_AUTH); only a hash is stored",
        "--auth-file <htpasswd>       users from an htpasswd file ({SHA} entries, htpasswd -s)",
        "--max-connections-per-ip <n> open connections one non-loopback client may hold (default 0: no limit; 503 over it)",
        "--random-route    serve the site only under a random secret path (printed in the URL)",
        "--allow-ip <cidr>            only answer clients in this block (repeatable; loopback is always allowed)",
        "--deny-ip <cidr>             refuse clients in this block (repeatable; checked first)",
        "--stop-after <n>[s|m|h|d]    stop the server after this long (not with --watch)",
        "--tls-cert <pem>  serve HTTPS with this certificate chain (with --tls-key; or SITEFILE_TLS_CERT)",
        "--tls-key <pem>   the private key of --tls-cert (or SITEFILE_TLS_KEY); read at start, never copied",
        "--compress        gzip text, JSON, SVG, wasm ... files (1 KiB or more) on the fly (precompressed .br/.gz are always used)",
        "--throttle <3g|slow-3g|kbit[/ms]>  limit response bandwidth and add latency (e.g. 400/400)",
        "--mime <ext=type>            serve files with this extension as this content type (repeatable; before the built-in table)",
        "--mime-file <mime.types>     content types from an nginx or Apache mime.types file (--mime wins)",
        "--allowed-host <name>        also answer requests for this Host (repeatable; .example.com: and subdomains; *: any)",
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
    let allowed_hosts = allowed_hosts_of(v)?;
    let isolate = isolate_of(v)?;
    let cache = cache_of(v)?;
    let (log_json, log_ip, quiet) = logging_of(v)?;
    let (auth, auth_entries, auth_file) = auth_of(v, &bind.host)?;
    let per_ip = per_ip_of(v)?;
    let sharing = sharing_of(v, &bind.host)?;
    let (tls, tls_paths) = tls_of(v)?;
    let throttle = throttle_of(v)?;
    let mime = mime_of(v)?;
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
        let tree = load_served_tree_with(
            &store,
            &source,
            &ServedTreeOptions {
                keep_os_metadata,
                warn: Some(&warn),
            },
        )?;
        let modified = served_last_modified(&store, &source);
        let n = tree.len();
        catch_signals();
        let server = listen(
            Arc::new(tree),
            SiteServerOptions {
                spa: v.flag("spa"),
                log: Some(log_writer(log_json)),
                server_id: None,
                live_reload: None,
                last_modified: Some(Arc::new(move || modified)),
                warn: None,
                allowed_hosts: Some(allowed_hosts.clone()),
                cors: v.flag("cors"),
                isolate: isolate.clone(),
                cache: cache.clone(),
                log_json,
                log_ip,
                quiet,
                auth: auth.clone(),
                max_connections_per_ip: per_ip,
                limit_loopback: false,
                base_path: sharing.base_path.clone(),
                ip_rules: sharing.ip_rules.clone(),
                noindex: sharing.noindex,
                client_info: true,
                tls: tls.clone(),
                compress: v.flag("compress"),
                gzip_cache: Default::default(),
                throttle: throttle.as_ref().map(|t| t.1),
                mime: mime.as_deref().map(mime_map),
            },
            &bind.host,
            bind.port,
        )?;
        let bp = sharing.base_path.as_deref();
        if log_json {
            out(&format!(
                "{}\n",
                crate::server::started_log_line(
                    &now_iso(),
                    &label,
                    n,
                    &site_url(&server.host, server.port, bp, tls.is_some())
                )
            ));
        } else {
            out(&format!(
                "Serving {label} ({n} files) at {}  (Ctrl-C to stop)\n{}{}",
                site_url(&server.host, server.port, bp, tls.is_some()),
                network_lines(&server.host, server.port, bp, tls.is_some()),
                throttle_line(throttle.as_ref().map(|t| t.0.as_str()))
            ));
        }
        let deadline = sharing
            .stop_after
            .as_ref()
            .map(|(secs, _)| std::time::Instant::now() + Duration::from_secs(*secs));
        if wait_for_signal_until(deadline).is_none() {
            if let Some((_, text)) = &sharing.stop_after {
                out(&format!("Stopping: --stop-after {text} reached\n"));
            }
        }
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
            allowed_hosts,
            cors: v.flag("cors"),
            isolate,
            cache,
            log_format: log_json.then(|| "json".to_string()),
            log_ip,
            quiet,
            auth: auth_entries,
            auth_file,
            max_connections_per_ip: per_ip,
            base_path: sharing.base_path.clone(),
            allow_ip: (!sharing.allow_text.is_empty()).then(|| sharing.allow_text.clone()),
            deny_ip: (!sharing.deny_text.is_empty()).then(|| sharing.deny_text.clone()),
            tls_cert: tls_paths.as_ref().map(|p| p.0.clone()),
            tls_key: tls_paths.as_ref().map(|p| p.1.clone()),
            compress: v.flag("compress"),
            throttle: throttle.as_ref().map(|t| t.0.clone()),
            mime: mime.as_ref().map(|m| {
                m.iter()
                    .map(|(k, t)| (k.clone(), Value::String(t.clone())))
                    .collect()
            }),
            expires: sharing.stop_after.as_ref().map(|(secs, _)| {
                crate::store::iso_of_time(std::time::SystemTime::now() + Duration::from_secs(*secs))
            }),
            project: None,
        },
        None,
        &warn,
    )?;
    out(&format!(
        "Started {} ({}) serving {label} at {}\n{}{}",
        up.name,
        up.id,
        site_url(&up.host, up.port, up.base_path.as_deref(), up.tls_cert.is_some()),
        network_lines(&up.host, up.port, up.base_path.as_deref(), up.tls_cert.is_some()),
        throttle_line(up.throttle.as_deref())
        site_url(
            &up.host,
            up.port,
            up.base_path.as_deref(),
            up.tls_cert.is_some()
        ),
        network_lines(
            &up.host,
            up.port,
            up.base_path.as_deref(),
            up.tls_cert.is_some()
        )
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
    let allowed_hosts = allowed_hosts_of(v)?;
    let isolate = isolate_of(v)?;
    let cache = cache_of(v)?;
    let (log_json, log_ip, quiet) = logging_of(v)?;
    let (auth, _, _) = auth_of(v, &bind.host)?;
    let per_ip = per_ip_of(v)?;
    if v.has("stop-after") {
        return usage("--stop-after does not apply to --watch");
    }
    let sharing = sharing_of(v, &bind.host)?;
    let (tls, _) = tls_of(v)?;
    let throttle = throttle_of(v)?;
    let mime = mime_of(v)?;
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
        let opts = SiteServerOptions {
            spa: v.flag("spa"),
            log: Some(log_writer(log_json)),
            server_id: None,
            live_reload: Some(live.clone()),
            last_modified: Some(Arc::new({
                let built_at = built_at.clone();
                move || built_at.read().ok().and_then(|t| *t)
            })),
            warn: Some(Arc::new(|m: &str| err(&format!("warning: {m}\n")))),
            allowed_hosts: Some(allowed_hosts.clone()),
            cors: v.flag("cors"),
            isolate: isolate.clone(),
            cache: cache.clone(),
            log_json,
            log_ip,
            quiet,
            auth: auth.clone(),
            max_connections_per_ip: per_ip,
            limit_loopback: false,
            base_path: sharing.base_path.clone(),
            ip_rules: sharing.ip_rules.clone(),
            noindex: sharing.noindex,
            client_info: true,
            tls: tls.clone(),
            compress: v.flag("compress"),
            gzip_cache: Default::default(),
            throttle: throttle.as_ref().map(|t| t.1),
            mime: mime.as_deref().map(mime_map),
        };
        let s = listen_shared(current.clone(), opts, &bind.host, bind.port)?;
        let n = current.read().map(|t| t.len()).unwrap_or(0);
        if log_json {
            out(&format!(
                "{}\n",
                crate::server::started_log_line(
                    &now_iso(),
                    &context,
                    n,
                    &site_url(&s.host, s.port, sharing.base_path.as_deref(), tls.is_some())
                )
            ));
        } else {
            out(&format!(
                "Serving {context} ({n} files) at {}  (Ctrl-C to stop)\n{}{}",
                site_url(&s.host, s.port, sharing.base_path.as_deref(), tls.is_some()),
                network_lines(&s.host, s.port, sharing.base_path.as_deref(), tls.is_some()),
                throttle_line(throttle.as_ref().map(|t| t.0.as_str()))
            ));
        }
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

/// A detached server's TLS configuration, from the PEM files its record names (read at every start).
fn worker_tls(state: &ServerState) -> Result<Option<Arc<rustls::ServerConfig>>> {
    let (Some(c), Some(k)) = (&state.tls_cert, &state.tls_key) else {
        return Ok(None);
    };
    let read = |f: &String| {
        std::fs::read(f).map_err(|e| SitefileError::plain(E_SERVER, format!("{f}: {e}")))
    };
    Ok(Some(crate::server::tls_config(&read(c)?, &read(k)?)?))
}

/// A detached server's checker: its verifiers and its htpasswd file, read again at every start.
fn worker_auth(state: &ServerState) -> Result<Option<Arc<crate::serve_auth::BasicAuth>>> {
    let mut all = state.auth.clone();
    if let Some(f) = &state.auth_file {
        let text = std::fs::read_to_string(f)
            .map_err(|e| SitefileError::plain(E_SERVER, format!("--auth-file {f}: {e}")))?;
        all.extend(crate::serve_auth::parse_htpasswd(&text, f)?);
    }
    Ok((!all.is_empty() || state.auth_file.is_some())
        .then(|| Arc::new(crate::serve_auth::BasicAuth::new(all))))
}

/// `sitefile __serve-worker <id>`: the detached server process.
pub fn serve_worker(id: &str) -> i32 {
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
    // `--log-format json`: every line is a JSON object.
    let json = state.log_format.as_deref() == Some("json");
    let stamp = move |m: &str| {
        let ts = now_iso();
        if json {
            let line = if m.starts_with('{') {
                m.to_string()
            } else {
                crate::server::json_log_line(&ts, m)
            };
            out(&format!("{line}\n"));
        } else {
            out(&format!("{ts} {m}\n"));
        }
    };
    catch_signals();
    // --stop-after: the record's expiry.
    let left_ms = state
        .expires
        .as_deref()
        .and_then(super::common::parse_iso)
        .map(|ms| {
            ms - std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        });
    let deadline = left_ms
        .filter(|l| *l > 0)
        .map(|l| std::time::Instant::now() + Duration::from_millis(l as u64));
    let started = (|| -> Result<crate::server::ListeningServer> {
        if left_ms.is_some_and(|l| l <= 0) {
            return Err(SitefileError::plain(
                E_SERVER,
                format!(
                    "server expired at {} (--stop-after)",
                    state.expires.as_deref().unwrap_or("")
                ),
            ));
        }
        let warn = |m: &str| stamp(&format!("warning: {m}"));
        let tree = load_served_tree_with(
            &reg.store,
            &state.source,
            &ServedTreeOptions {
                keep_os_metadata: state.keep_os_metadata,
                warn: Some(&warn),
            },
        )?;
        let modified = served_last_modified(&reg.store, &state.source);
        let n = tree.len();
        let log: crate::server::LogFn = Arc::new(stamp);
        let server = listen(
            Arc::new(tree),
            SiteServerOptions {
                spa: state.spa,
                log: Some(log),
                server_id: Some(id.to_string()),
                live_reload: None,
                last_modified: Some(Arc::new(move || modified)),
                warn: None,
                allowed_hosts: Some(state.allowed_hosts.clone()),
                cors: state.cors,
                isolate: state.isolate.clone(),
                cache: state.cache.clone(),
                log_json: json,
                log_ip: state.log_ip,
                quiet: state.quiet,
                auth: worker_auth(&state)?,
                max_connections_per_ip: state.max_connections_per_ip,
                limit_loopback: false,
                base_path: state.base_path.clone(),
                ip_rules: (state.allow_ip.is_some() || state.deny_ip.is_some()).then(|| {
                    let parse = |v: &Option<Vec<String>>| -> Vec<crate::server::Cidr> {
                        v.iter()
                            .flatten()
                            .filter_map(|t| crate::server::parse_cidr(t))
                            .collect()
                    };
                    crate::server::IpRules {
                        allow: parse(&state.allow_ip),
                        deny: parse(&state.deny_ip),
                    }
                }),
                noindex: !(state.bind.host == "localhost"
                    || state.bind.host == "::1"
                    || state.bind.host.starts_with("127.")),
                client_info: true,
                tls: worker_tls(&state)?,
                compress: state.compress,
                gzip_cache: Default::default(),
                throttle: state
                    .throttle
                    .as_deref()
                    .and_then(crate::server::parse_throttle),
                mime: state.mime.as_ref().map(|m| {
                    Arc::new(
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|t| (k.clone(), t.to_string())))
                            .collect(),
                    )
                }),
                throttle: state
                    .throttle
                    .as_deref()
                    .and_then(crate::server::parse_throttle),
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
        if json {
            stamp(&crate::server::started_log_line(
                &now_iso(),
                &state.image,
                n,
                &site_url(
                    &host,
                    port,
                    state.base_path.as_deref(),
                    state.tls_cert.is_some(),
                ),
            ));
        } else {
            stamp(&format!(
                "serving {} ({n} files) at {}",
                state.image,
                site_url(
                    &host,
                    port,
                    state.base_path.as_deref(),
                    state.tls_cert.is_some()
                )
            ));
        }
        Ok(server)
    })();
    match started {
        Ok(server) => {
            match wait_for_signal_until(deadline) {
                Some(sig) if json => stamp(&crate::server::stopped_log_line(&now_iso(), sig)),
                Some(sig) => stamp(&format!("received {sig}, shutting down")),
                None if json => stamp(&crate::server::stopped_log_line(&now_iso(), "expired")),
                None => stamp("--stop-after reached, shutting down"),
            }
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
            let ts = now_iso();
            if json {
                err(&format!(
                    "{}\n",
                    crate::server::json_log_line(&ts, &message)
                ));
            } else {
                err(&format!("{ts} {message}\n"));
            }
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
                        // Credentials never leave the record: a server that needs them shows auth: true.
                        if k != "image" && k != "auth" {
                            m.insert(k, val);
                        }
                    }
                }
                m.insert("status".into(), Value::String(s.status.into()));
                m.insert("url".into(), Value::String(s.url.clone()));
                // A server bound to all interfaces also lists the URLs other machines use.
                let net = if s.status == "running" {
                    crate::server::network_urls(&s.state.host, s.state.port)
                        .into_iter()
                        .map(|u| crate::server::with_scheme(u, s.state.tls_cert.is_some()))
                        .collect()
                } else {
                    Vec::new()
                };
                if !net.is_empty() {
                    m.insert(
                        "networkUrls".into(),
                        Value::Array(net.into_iter().map(Value::String).collect()),
                    );
                }
                if !s.state.auth.is_empty() || s.state.auth_file.is_some() {
                    m.insert("auth".into(), Value::Bool(true));
                }
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

const LOGS_OPTS: &[OptSpec] = &[flag_s("follow", 'f'), string_s("tail", 'n'), flag("json")];
pub const LOGS: Command = Command {
    name: "logs",
    summary: "show a server's access log",
    usage: "server logs [-f] [-n lines] [--json] <name|id>",
    description: "Print the log of a detached server: one line per request (\"METHOD /path STATUS BYTES MSms\").",
    flags: &["-f, --follow    keep printing new lines until Ctrl-C", "-n, --tail <n>  only the last n lines", "--json          one JSON object per line (text lines of older servers are converted)"],
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
    let json = v.flag("json");
    let mut carry = String::new();
    // Write log bytes; with --json, whole lines only, each converted.
    let mut emit = |chunk: &str| {
        if !json {
            out(chunk);
            return;
        }
        carry.push_str(chunk);
        let Some(last) = carry.rfind('\n') else {
            return;
        };
        let whole: String = carry[..last]
            .split('\n')
            .map(|l| format!("{}\n", crate::server::log_line_as_json(l)))
            .collect();
        carry = carry[last + 1..].to_string();
        out(&whole);
    };
    emit(&text);
    if !v.flag("follow") {
        drop(emit);
        if json && !carry.is_empty() {
            out(&format!("{}\n", crate::server::log_line_as_json(&carry)));
        }
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
                emit(&String::from_utf8_lossy(&buf));
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
        "Restarted {} ({}) serving {} at {}\n{}",
        up.name,
        up.id,
        up.image,
        site_url(
            &up.host,
            up.port,
            up.base_path.as_deref(),
            up.tls_cert.is_some()
        ),
        network_lines(
            &up.host,
            up.port,
            up.base_path.as_deref(),
            up.tls_cert.is_some()
        )
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
