//! A static HTTP server for an immutable Tree (port of `src/server.ts`), on
//! `std::net` with a thread per connection. GET and HEAD only; nothing outside
//! the tree is reachable because lookups are exact map keys built from
//! validated segments.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::wbn::{encode_uri_component, mime_type_for};
use crate::tree::Tree;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Default)]
pub struct SiteServerOptions {
    /// Single-page apps: answer a page navigation ([`is_navigation`]) for a path nothing else
    /// answers with the nearest app shell ([`spa_fallback`]). Never for subresources.
    pub spa: bool,
    /// `--spa-exclude`: request paths (`*` matches any characters) that never fall back.
    pub spa_exclude: Vec<String>,
    /// `--spa-fallback`: the one file every fallback serves, instead of the nearest `200.html` or `index.html`.
    pub spa_fallback: Option<String>,
    /// One access-log line per request: `METHOD /path STATUS BYTES MSms`.
    pub log: Option<LogFn>,
    /// Sent as `x-sitefile-server` so `servers` can tell its own servers from a reused port.
    pub server_id: Option<String>,
    /// `serve --watch`: answer [`RELOAD_PATH`] with an event stream and [`RELOAD_SCRIPT_PATH`]
    /// with the client script, and add [`RELOAD_SCRIPT`] to HTML navigations. Never set
    /// otherwise, and never written to exports.
    pub live_reload: Option<Arc<LiveReload>>,
    /// When the served files last changed (Unix seconds), read per request: sent as
    /// `Last-Modified` on file responses, and `If-Modified-Since` (without `If-None-Match`)
    /// gives `304`. `None`: neither.
    pub last_modified: Option<LastModifiedFn>,
    /// Problems a person should know about (without the `warning: ` prefix), e.g. a CSP that
    /// blocks live reload.
    pub warn: Option<LogFn>,
    /// DNS-rebinding guard: when set, a request whose `Host` is not `localhost`, a `*.localhost`
    /// name, an IP address or a name this list allows gets `403` ([`host_allowed`]). `*` allows
    /// every host; `None`: no check (the CLI always sets it).
    pub allowed_hosts: Option<Vec<String>>,
    /// `--cors`: `Access-Control-Allow-Origin: *` on every response (`_headers` still wins) and
    /// `OPTIONS` preflights answered with `204`.
    pub cors: bool,
    /// `--isolate`: cross-origin isolation on every response: `Cross-Origin-Opener-Policy:
    /// same-origin`, this `Cross-Origin-Embedder-Policy` (`require-corp` or `credentialless`) and
    /// `Cross-Origin-Resource-Policy: same-origin`. `_headers` still wins.
    pub isolate: Option<String>,
    /// `--max-age`, `--immutable`, `--max-age-html`: a production `Cache-Control` preview for
    /// files ([`cache_control_for`]).
    pub cache: Option<CachePreview>,
    /// `--log-format json`: access-log lines as JSON objects ([`access_log_line`]); else text.
    pub log_json: bool,
    /// `--log-ip`: add the client address to access-log lines.
    pub log_ip: bool,
    /// `--quiet`: no access-log lines (other log lines still go to `log`).
    pub quiet: bool,
    /// `--auth`, `--auth-file`: every request needs one of these credentials, else `401`.
    pub auth: Option<Arc<crate::serve_auth::BasicAuth>>,
    /// `--max-connections-per-ip`: open connections one non-loopback client may hold (0: no limit);
    /// a connection over it gets `503` with `Retry-After: 1` and is closed.
    pub max_connections_per_ip: usize,
    /// Count loopback clients against `max_connections_per_ip` too (tests only).
    pub limit_loopback: bool,
    /// `--random-route`: `base` is a secret, so `/` is a plain `404` instead of a redirect into it.
    pub hide_base: bool,
    /// `--allow-ip`, `--deny-ip`: a non-loopback client they refuse gets `403` before anything else.
    pub ip_rules: Option<IpRules>,
    /// `X-Robots-Tag: noindex` on every response (non-loopback binds).
    pub noindex: bool,
    /// Add the client address and device to access-log lines of non-loopback clients.
    pub client_info: bool,
    /// `--tls-cert`, `--tls-key`: serve HTTPS ([`tls_config`]).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// `--base /repo/`: serve the site under this path prefix (a GitHub Pages project site); `/`
    /// redirects to it, `/repo` gets its slash, everything else is 404. Read per request (a
    /// watched server takes it from the last good build); [`normalize_base`] makes the value.
    pub base: Option<BaseFn>,
    /// `--country`: what `_redirects` `Country=` conditions match (there is no geolocation locally).
    pub country: Option<String>,
    /// Where file contents come from when the tree holds only digests (a zip served in
    /// place, [`crate::resources::archive::open_zip_file`]).
    pub content: Option<Arc<dyn SiteContent>>,
}

/// Contents read on demand ([`SiteServerOptions::content`]).
pub trait SiteContent: Send + Sync {
    /// A file's bytes (`None`: use the node's).
    fn read(&self, path: &str) -> Option<Result<Vec<u8>>>;
    /// A deflated file's raw deflate stream, sent as `content-encoding: gzip` to clients that accept it.
    fn deflated(&self, path: &str) -> Option<crate::resources::archive::Deflated>;
}

impl SiteContent for crate::resources::archive::ZipFile {
    fn read(&self, path: &str) -> Option<Result<Vec<u8>>> {
        crate::resources::archive::ZipFile::read(self, path)
    }
    fn deflated(&self, path: &str) -> Option<crate::resources::archive::Deflated> {
        crate::resources::archive::ZipFile::deflated(self, path)
    }
}

/// Whether an `accept-encoding` header accepts gzip (`gzip` or `x-gzip` with a non-zero q).
pub fn accepts_gzip(header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    header.split(',').any(|part| {
        let mut it = part.trim().split(';');
        let name = it.next().unwrap_or("").trim().to_ascii_lowercase();
        if name != "gzip" && name != "x-gzip" {
            return false;
        }
        match it
            .map(str::trim)
            .find(|x| x.len() >= 2 && x[..2].eq_ignore_ascii_case("q="))
        {
            None => true,
            Some(q) => crate::cli::common::js_to_number(&q[2..]) > 0.0,
        }
    })
}

/// A gzip member around a raw deflate stream: fixed header (no name, mtime 0, OS unknown), CRC-32 and size.
pub fn gzip_member(deflate: &[u8], crc: u32, size: u64) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend_from_slice(deflate);
    out.extend(crc.to_le_bytes());
    out.extend(((size % (1u64 << 32)) as u32).to_le_bytes());
    out
}

pub type LastModifiedFn = Arc<dyn Fn() -> Option<i64> + Send + Sync>;

/// The tree a server answers from; `serve --watch` swaps in each good rebuild.
pub type SharedTree = Arc<RwLock<Arc<Tree>>>;

/// Reserved for the live-reload event stream while `serve --watch` runs.
pub const RELOAD_PATH: &str = "/__sitefile/reload";
/// Reserved for the live-reload client script ([`reload_client`]) while `serve --watch` runs.
pub const RELOAD_SCRIPT_PATH: &str = "/__sitefile/reload.js";
/// Added to navigated HTML (before the last `</body>`, else at the end) under `serve --watch`.
pub const RELOAD_SCRIPT: &str = "<script src=\"/__sitefile/reload.js\"></script>";

/// The TypeScript module that holds the client script; it is the single source of both servers' bytes.
const RELOAD_CLIENT_SOURCE: &str = include_str!("../../src/reload-client.ts");

/// The live-reload client script (JavaScript): the text between the `String.raw` backticks of
/// `src/reload-client.ts`.
pub fn reload_client() -> &'static str {
    static CLIENT: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let start = RELOAD_CLIENT_SOURCE
            .find("String.raw`")
            .map(|i| i + "String.raw`".len())
            .expect("src/reload-client.ts has a String.raw literal");
        let end = RELOAD_CLIENT_SOURCE
            .rfind('`')
            .filter(|e| *e >= start)
            .expect("src/reload-client.ts closes its String.raw literal");
        &RELOAD_CLIENT_SOURCE[start..end]
    })
}

/// Whether a request is a page navigation, which gets the reload script: `Sec-Fetch-Mode:
/// navigate` when the browser sends it, else `Sec-Fetch-Dest` document/iframe/frame, else an
/// `Accept` naming `text/html`. A `fetch()` of an HTML fragment is none of these.
pub fn is_navigation(headers: &HashMap<String, String>) -> bool {
    if let Some(mode) = headers.get("sec-fetch-mode") {
        return mode.trim().eq_ignore_ascii_case("navigate");
    }
    if let Some(dest) = headers.get("sec-fetch-dest") {
        let d = dest.trim().to_ascii_lowercase();
        return d == "document" || d == "iframe" || d == "frame";
    }
    headers
        .get("accept")
        .is_some_and(|a| a.to_ascii_lowercase().contains("text/html"))
}

/// The directive of a `Content-Security-Policy` value that blocks live reload: `script-src`
/// when a same-origin script cannot run, `connect-src` when the event stream cannot connect.
/// Same rules as `cspBlocksReload` in src/server.ts.
pub fn csp_blocks_reload(value: &str) -> Option<&'static str> {
    for policy in value.split(',') {
        let mut directives: Vec<(String, Vec<String>)> = Vec::new();
        for d in policy.split(';') {
            let mut tokens = d
                .split(['\t', '\n', '\x0c', '\r', ' '])
                .filter(|t| !t.is_empty());
            let Some(name) = tokens.next() else { continue };
            let name = name.to_lowercase();
            if !directives.iter().any(|(n, _)| *n == name) {
                directives.push((name, tokens.map(|t| t.to_lowercase()).collect()));
            }
        }
        let effective = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| directives.iter().find(|(d, _)| d == n).map(|(_, v)| v))
        };
        let allows = |sources: Option<&Vec<String>>, script: bool| match sources {
            None => true,
            Some(v) => {
                !(script && v.iter().any(|t| t == "'strict-dynamic'"))
                    && v.iter()
                        .any(|t| t == "'self'" || t == "*" || t == "http:" || t == "https:")
            }
        };
        if !allows(
            effective(&["script-src-elem", "script-src", "default-src"]),
            true,
        ) {
            return Some("script-src");
        }
        if !allows(effective(&["connect-src", "default-src"]), false) {
            return Some("connect-src");
        }
    }
    None
}

/// Headers of the event stream (also on HEAD): no proxy buffering (`X-Accel-Buffering`), no
/// compression or other transforms.
pub const STREAM_HEADERS: [(&str, &str); 3] = [
    ("content-type", "text/event-stream"),
    ("cache-control", "no-cache, no-transform"),
    ("x-accel-buffering", "no"),
];

/// What the client script does besides reloading (`serve --watch` flags).
#[derive(Debug, Clone)]
pub struct LiveReloadOptions {
    /// Unregister the page's service workers and empty its Cache Storage before each reload
    /// (`--keep-service-workers` turns it off).
    pub reset_service_workers: bool,
    /// Swap changed stylesheets in place instead of reloading when only CSS changed
    /// (`--no-css-inject` turns it off).
    pub css_inject: bool,
    /// Forward the page's `console.log`/`console.info` too, not only errors and warnings (`--console`).
    pub console: bool,
    /// Show a Rebuilding badge and a build-error overlay in the page (`--no-overlay` turns it off).
    pub overlay: bool,
    /// After a rebuild that changed exactly one page, go to that page (`--navigate-to-changed`).
    pub navigate_to_changed: bool,
    /// `--reload morph` (true, the default) updates a page in place when only HTML changed;
    /// `--reload full` (false) always reloads.
    pub morph: bool,
}

impl Default for LiveReloadOptions {
    fn default() -> Self {
        LiveReloadOptions {
            reset_service_workers: true,
            css_inject: true,
            console: false,
            overlay: true,
            navigate_to_changed: false,
            morph: true,
        }
    }
}

/// The script served at [`RELOAD_SCRIPT_PATH`]: [`reload_client`] called with `options` (keys in
/// the same fixed order as `clientScript` in src/server.ts).
pub fn client_script(options: &LiveReloadOptions) -> String {
    format!(
        "{}({{\"resetServiceWorkers\":{},\"cssInject\":{},\"console\":{},\"overlay\":{},\"navigateToChanged\":{},\"reload\":\"{}\"}});\n",
        reload_client(),
        options.reset_service_workers,
        options.css_inject,
        options.console,
        options.overlay,
        options.navigate_to_changed,
        if options.morph { "morph" } else { "full" }
    )
}

/// Reserved for browser console messages (POST) while `serve --watch` runs.
pub const CONSOLE_PATH: &str = "/__sitefile/console";
/// Largest console message body accepted (bytes).
pub const MAX_CONSOLE_BODY: u64 = 16 * 1024;
/// At most this many console messages are logged per [`CONSOLE_WINDOW`].
pub const CONSOLE_LIMIT: u32 = 50;
pub const CONSOLE_WINDOW: Duration = Duration::from_secs(10);

/// `s` with each run of control characters made one space, trimmed, and cut to `max` chars (then `...`).
fn clean_text(s: &str, max: usize) -> String {
    let mut t = String::with_capacity(s.len());
    let mut in_ctl = false;
    for c in s.chars() {
        if (c as u32) < 0x20 || c == '\u{7f}' {
            if !in_ctl {
                t.push(' ');
            }
            in_ctl = true;
        } else {
            t.push(c);
            in_ctl = false;
        }
    }
    let t = js_trim(&t);
    if t.chars().count() > max {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}...")
    } else {
        t.to_string()
    }
}

/// The server-log lines for one console message body; same rules as `consoleLines` in src/server.ts.
pub fn console_lines(body: &str, all: bool) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let o = v.as_object()?;
    let level = o.get("level").and_then(|l| l.as_str())?;
    if !["error", "warn", "log", "info"].contains(&level) {
        return None;
    }
    if (level == "log" || level == "info") && !all {
        return Some(Vec::new());
    }
    let message = clean_text(
        o.get("message").and_then(|m| m.as_str()).unwrap_or(""),
        1000,
    );
    let num = |k: &str| -> Option<u64> {
        let f = o.get(k)?.as_f64()?;
        (f.fract() == 0.0 && f >= 0.0 && f <= 9007199254740991.0).then_some(f as u64)
    };
    let url = o
        .get("url")
        .and_then(|u| u.as_str())
        .map(|u| clean_text(u, 300))
        .unwrap_or_default();
    let line = num("line");
    let col = line.and(num("col"));
    let at = if url.is_empty() {
        String::new()
    } else {
        format!(
            " ({url}{}{})",
            line.map(|l| format!(":{l}")).unwrap_or_default(),
            col.map(|c| format!(":{c}")).unwrap_or_default()
        )
    };
    let mut out = vec![format!("browser {level}: {message}{at}")];
    if let Some(stack) = o.get("stack").and_then(|s| s.as_str()) {
        out.extend(
            stack
                .split('\n')
                .map(|l| clean_text(l, 300))
                .filter(|l| !l.is_empty() && *l != message)
                .take(5)
                .map(|l| format!("    {l}")),
        );
    }
    Some(out)
}

/// A console POST comes from a page of this server; same rules as `isSameOriginPost` in src/server.ts.
pub fn is_same_origin_post(headers: &HashMap<String, String>) -> bool {
    let (Some(origin), Some(host)) = (headers.get("origin"), headers.get("host")) else {
        return false;
    };
    if host.is_empty() {
        return false;
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        if !site.trim().eq_ignore_ascii_case("same-origin") {
            return false;
        }
    }
    *origin == format!("http://{host}") || *origin == format!("https://{host}")
}

/// The event that tells every page to reload.
pub const RELOAD_EVENT: &str = "{\"type\":\"reload\"}";
/// Sent when a rebuild starts (pages show a Rebuilding badge).
pub const BUILDING_EVENT: &str = "{\"type\":\"building\"}";
/// Sent after a good rebuild that changed nothing (pages clear the badge and any error overlay).
pub const OK_EVENT: &str = "{\"type\":\"ok\"}";

/// The event for a failed rebuild; same shape as `errorEvent` in src/server.ts.
pub fn error_event(e: &SitefileError) -> String {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), "error".into());
    m.insert("code".into(), e.code.to_string().into());
    m.insert("message".into(), e.detail.clone().into());
    if let Some(span) = &e.span {
        m.insert("file".into(), span.file.clone().into());
        m.insert("line".into(), span.line.into());
        m.insert("column".into(), span.column.into());
    }
    serde_json::Value::Object(m).to_string()
}

/// The URL path that serves `file` (`/dir/` for `/dir/index.html`); same as `pageUrlFor` in src/server.ts.
pub fn page_url_for(file: &str) -> String {
    let mut segs: Vec<&str> = file.split('/').skip(1).collect();
    if segs.last() == Some(&"index.html") {
        let n = segs.len() - 1;
        segs[n] = "";
    }
    let enc: Vec<String> = segs.iter().map(|s| encode_uri_component(s)).collect();
    format!("/{}", enc.join("/"))
}

/// The live-reload event for a new tree (`None` when no file changed); same rules as
/// `reloadEventFor` in src/server.ts.
pub fn reload_event_for(
    prev: &Tree,
    next: &Tree,
    css_inject: bool,
    navigate: bool,
) -> Option<String> {
    let mut changed: Vec<String> = Vec::new();
    let mut pages: Vec<&String> = Vec::new();
    let mut css_only = css_inject;
    let mut html_only = true;
    for (p, f) in next.entries() {
        let old = prev.get(p);
        if old.is_some_and(|o| o.digest == f.digest && o.mode == f.mode) {
            continue;
        }
        changed.push(p.clone());
        let html = mime_type_for(p).starts_with("text/html");
        if html {
            pages.push(p);
        }
        if old.is_none() || !html {
            html_only = false;
        }
        if old.is_none() || !mime_type_for(p).starts_with("text/css") {
            css_only = false;
        }
    }
    for (p, _) in prev.entries() {
        if !next.is_file(p) {
            changed.push(p.clone());
            css_only = false;
            html_only = false;
        }
    }
    if changed.is_empty() {
        return None;
    }
    if !css_only {
        let path = (navigate && pages.len() == 1).then(|| page_url_for(pages[0]));
        if path.is_none() && !html_only {
            return Some(RELOAD_EVENT.to_string());
        }
        let mut m = serde_json::Map::new();
        m.insert("type".into(), "reload".into());
        if let Some(p) = path {
            m.insert("path".into(), p.into());
        }
        if html_only {
            m.insert("morph".into(), true.into());
        }
        return Some(serde_json::Value::Object(m).to_string());
    }
    changed.sort();
    Some(serde_json::json!({ "type": "css", "paths": changed }).to_string())
}

/// Events sent so far: `next` is the id the next one gets; the last [`KEPT_EVENTS`] are kept for
/// streams that have not written them yet.
#[derive(Default)]
struct Events {
    next: u64,
    recent: std::collections::VecDeque<(u64, String)>,
}
const KEPT_EVENTS: usize = 64;

/// Open live-reload streams; [`LiveReload::send`] gives every page an event ([`LiveReload::reload`]: reload).
#[derive(Default)]
pub struct LiveReload {
    /// The client script for this session ([`client_script`]); empty means the default options.
    script: String,
    events: Mutex<Events>,
    changed: Condvar,
    closed: AtomicBool,
    clients: AtomicUsize,
    /// CSP values already warned about (one warning per policy text).
    warned_csp: Mutex<std::collections::HashSet<String>>,
    /// `--console`: log and info messages are logged too.
    console: bool,
    /// The last error event, until a good build: sent to streams that open meanwhile.
    failure: Mutex<Option<String>>,
    /// Console rate limit: the current window's start and how many messages it has seen.
    console_window: Mutex<Option<(Instant, u32)>>,
}

impl LiveReload {
    pub fn new() -> Arc<LiveReload> {
        LiveReload::with_options(&LiveReloadOptions::default())
    }

    pub fn with_options(options: &LiveReloadOptions) -> Arc<LiveReload> {
        Arc::new(LiveReload {
            script: client_script(options),
            console: options.console,
            ..LiveReload::default()
        })
    }

    /// The client script for this session.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// Whether one more console message may be logged now (at most [`CONSOLE_LIMIT`] per
    /// [`CONSOLE_WINDOW`]), and the notice line the first refused one in a window prints.
    pub fn admit_console(&self, now: Instant) -> (bool, Option<String>) {
        let Ok(mut w) = self.console_window.lock() else {
            return (false, None);
        };
        let (start, count) = match *w {
            Some((start, count)) if now.duration_since(start) < CONSOLE_WINDOW => {
                (start, count + 1)
            }
            _ => (now, 1),
        };
        *w = Some((start, count));
        if count <= CONSOLE_LIMIT {
            return (true, None);
        }
        let notice = (count == CONSOLE_LIMIT + 1).then(|| {
            format!(
                "browser: more than {CONSOLE_LIMIT} console messages in 10 s; dropping the rest"
            )
        });
        (false, notice)
    }

    /// The answer to a request for [`CONSOLE_PATH`]: `body` is the POST body when it was read
    /// (same origin, a length within [`MAX_CONSOLE_BODY`]).
    fn console_response(
        &self,
        options: &SiteServerOptions,
        method: &str,
        headers: &HashMap<String, String>,
        body: Option<&[u8]>,
        body_len: u64,
        chunked: bool,
    ) -> Response {
        let h = base_headers(options);
        let res = |status: u16, mut h: Vec<(String, String)>, extra: &[(&str, &str)]| {
            h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
            Response {
                status,
                headers: h,
                body: Vec::new(),
                note: None,
            }
        };
        if method != "POST" {
            return res(405, h, &[("allow", "POST"), ("content-length", "0")]);
        }
        if !is_same_origin_post(headers) {
            return res(403, h, &[("content-length", "0")]);
        }
        if chunked || body_len > MAX_CONSOLE_BODY {
            return res(413, h, &[("content-length", "0")]);
        }
        let text = String::from_utf8_lossy(body.unwrap_or_default());
        let Some(lines) = console_lines(&text, self.console) else {
            return res(400, h, &[("content-length", "0")]);
        };
        if !lines.is_empty() {
            let (ok, notice) = self.admit_console(Instant::now());
            if let Some(log) = &options.log {
                if let Some(n) = notice {
                    log(&n);
                }
                if ok {
                    for l in &lines {
                        log(l);
                    }
                }
            }
        }
        res(204, h, &[])
    }

    /// Send `event` (one line of JSON) to every open page.
    pub fn send(&self, event: &str) {
        if let Ok(mut f) = self.failure.lock() {
            if event.starts_with("{\"type\":\"error\"") {
                *f = Some(event.to_string());
            } else if event != BUILDING_EVENT {
                *f = None;
            }
        }
        if let Ok(mut e) = self.events.lock() {
            let id = e.next;
            e.recent.push_back((id, event.to_string()));
            e.next += 1;
            while e.recent.len() > KEPT_EVENTS {
                e.recent.pop_front();
            }
        }
        self.changed.notify_all();
    }

    pub fn reload(&self) {
        self.send(RELOAD_EVENT);
    }

    /// End every open stream.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    /// Warn (once per policy text) when a CSP sent with an injected page blocks live reload.
    fn warn_csp(&self, warn: Option<&LogFn>, served: &str, policy: Option<&str>) {
        let (Some(warn), Some(policy)) = (warn, policy) else {
            return;
        };
        let Ok(mut seen) = self.warned_csp.lock() else {
            return;
        };
        if seen.contains(policy) {
            return;
        }
        let Some(blocked) = csp_blocks_reload(policy) else {
            return;
        };
        seen.insert(policy.to_string());
        let what = if blocked == "script-src" {
            format!("scripts from 'self' ({RELOAD_SCRIPT_PATH})")
        } else {
            format!("connections to 'self' ({RELOAD_PATH})")
        };
        warn(&format!("live reload: the Content-Security-Policy that _headers sets for {served} does not allow {what}, so open pages will not reload: {policy}"));
    }

    /// Open streams.
    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    /// Hold `out` open as a text/event-stream until the client goes away or [`LiveReload::close`].
    /// The headers keep proxies and tunnels from buffering or compressing it, and a comment every
    /// 15 s keeps idle connections from being cut (and finds clients that went away).
    fn stream(&self, out: &mut Conn, options: &SiteServerOptions) {
        let mut head = String::from("HTTP/1.1 200 OK\r\nx-content-type-options: nosniff\r\n");
        if let Some(id) = options.server_id.as_deref().filter(|s| !s.is_empty()) {
            head.push_str(&format!("x-sitefile-server: {id}\r\n"));
        }
        if options.cors {
            head.push_str("access-control-allow-origin: *\r\n");
        }
        for (k, v) in isolation_headers(options) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        for (k, v) in STREAM_HEADERS {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("connection: close\r\n\r\nretry: 1000\n\n");
        // A page opened while the last build is broken shows the error at once.
        if let Some(f) = self.failure.lock().ok().and_then(|f| f.clone()) {
            head.push_str(&format!("data: {f}\n\n"));
        }
        if out
            .write_all(head.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            return;
        }
        self.clients.fetch_add(1, Ordering::SeqCst);
        let mut seen = self.events.lock().map(|e| e.next).unwrap_or(0);
        loop {
            let Ok(e) = self.events.lock() else { break };
            let Ok((e, _)) = self
                .changed
                .wait_timeout_while(e, Duration::from_secs(15), |e| {
                    e.next == seen && !self.closed.load(Ordering::SeqCst)
                })
            else {
                break;
            };
            let mut msg = String::new();
            for (id, ev) in &e.recent {
                if *id >= seen {
                    msg.push_str(&format!("data: {ev}\n\n"));
                }
            }
            seen = e.next;
            drop(e);
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            // a comment line every 15 s finds clients that went away
            if msg.is_empty() {
                msg.push_str(": ping\n\n");
            }
            if out
                .write_all(msg.as_bytes())
                .and_then(|_| out.flush())
                .is_err()
            {
                break;
            }
        }
        self.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `html` with [`RELOAD_SCRIPT`] inserted before the last `</body` (ASCII case-insensitive), else appended.
pub fn inject_reload_script(html: &[u8]) -> Vec<u8> {
    let needle = b"</body";
    let at = (0..html.len().saturating_sub(needle.len() - 1))
        .rev()
        .find(|&i| html[i..i + needle.len()].eq_ignore_ascii_case(needle));
    let mut out = Vec::with_capacity(html.len() + RELOAD_SCRIPT.len());
    match at {
        Some(i) => {
            out.extend_from_slice(&html[..i]);
            out.extend_from_slice(RELOAD_SCRIPT.as_bytes());
            out.extend_from_slice(&html[i..]);
        }
        None => {
            out.extend_from_slice(html);
            out.extend_from_slice(RELOAD_SCRIPT.as_bytes());
        }
    }
    out
}

/// The origin form of a request target: an absolute-form target (`http://host/path?q`, RFC 9112
/// section 3.2.2) loses its scheme and authority (`/path?q`, or `/` for an empty path); any other
/// target is returned as it is. The authority is not checked.
/// Whether `s` is a dotted-decimal IPv4 address.
fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u32>().is_ok_and(|n| n <= 255)
        })
}

/// The DNS-rebinding guard of [`SiteServerOptions::allowed_hosts`] (`hostAllowed` in
/// src/server.ts): whether a `Host` header value may be answered.
pub fn host_allowed(allowed: Option<&[String]>, host: Option<&str>) -> bool {
    let (Some(allowed), Some(host)) = (allowed, host) else {
        return true;
    };
    if allowed.iter().any(|e| e == "*") {
        return true;
    }
    // Node keeps the first of repeated Host headers; ours are joined with ", ".
    let h = host.split(',').next().unwrap_or("").trim();
    if h.is_empty() {
        return true;
    }
    let port_ok = |p: &str| {
        p.is_empty() || (p.starts_with(':') && p[1..].bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(rest) = h.strip_prefix('[') {
        return match rest.find(']') {
            Some(close) => {
                close > 0
                    && rest[..close]
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
                    && port_ok(&rest[close + 1..])
            }
            None => false,
        };
    }
    // `^(.*?)(:[0-9]*)?$`: the port is the shortest `:digits` suffix that leaves the rest.
    let name = h
        .match_indices(':')
        .map(|(i, _)| i)
        .find(|&i| port_ok(&h[i..]))
        .map_or(h, |i| &h[..i]);
    let mut name = name.to_ascii_lowercase();
    if name.ends_with('.') {
        name.pop();
    }
    if is_ipv4(&name) || name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    allowed.iter().any(|e| match e.strip_prefix('.') {
        Some(base) => name == base || name.ends_with(e.as_str()),
        None => name == *e,
    })
}

/// An `--allowed-host` value, lowercased: `*`, or a host name (letters, digits, `-`, `.`; a
/// leading `.` for subdomains). `None` when invalid.
pub fn parse_allowed_host(value: &str) -> Option<String> {
    let v = value.trim().to_ascii_lowercase();
    if v == "*" {
        return Some(v);
    }
    let body = v.strip_prefix('.').unwrap_or(&v);
    let body = body.strip_suffix('.').unwrap_or(body);
    let ok = !body.is_empty()
        && body.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    ok.then(|| v.strip_suffix('.').unwrap_or(&v).to_string())
}

pub fn origin_form(target: &str) -> std::borrow::Cow<'_, str> {
    let b = target.as_bytes();
    let scheme = if b.len() >= 7 && b[..7].eq_ignore_ascii_case(b"http://") {
        7
    } else if b.len() >= 8 && b[..8].eq_ignore_ascii_case(b"https://") {
        8
    } else {
        return std::borrow::Cow::Borrowed(target);
    };
    let after = &target[scheme..];
    let rest = &after[after.find(['/', '?', '#']).unwrap_or(after.len())..];
    if rest.starts_with('/') {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Owned(format!("/{rest}"))
    }
}

/// The request path: `url` up to `?` or `#`.
fn raw_path_of(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    &url[..end]
}

const NOT_FOUND_PAGE: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>404 Not Found</title><h1>404 Not Found</h1>\n";
const BAD_REQUEST_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>400 Bad Request</title><h1>400 Bad Request</h1>\n";
/// A served connection: plain TCP, or a TLS session over it (`--tls-cert`). Clones share the
/// socket and the session; the connection loop reads and writes from one thread.
pub struct Conn {
    tcp: TcpStream,
    tls: Option<Arc<Mutex<rustls::ServerConnection>>>,
}

impl Conn {
    fn try_clone(&self) -> io::Result<Conn> {
        Ok(Conn {
            tcp: self.tcp.try_clone()?,
            tls: self.tls.clone(),
        })
    }
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.peer_addr()
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.tcp.set_read_timeout(d)
    }
    fn set_nodelay(&self, on: bool) -> io::Result<()> {
        self.tcp.set_nodelay(on)
    }
    fn shutdown(&mut self, how: Shutdown) -> io::Result<()> {
        if let Some(t) = &self.tls {
            if let Ok(mut c) = t.lock() {
                c.send_close_notify();
                let _ = c.complete_io(&mut self.tcp);
            }
        }
        self.tcp.shutdown(how)
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.tls {
            None => self.tcp.read(buf),
            Some(t) => {
                let mut c = t.lock().map_err(|_| io::Error::other("tls session"))?;
                rustls::Stream::new(&mut *c, &mut self.tcp).read(buf)
            }
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &self.tls {
            None => self.tcp.write(buf),
            Some(t) => {
                let mut c = t.lock().map_err(|_| io::Error::other("tls session"))?;
                rustls::Stream::new(&mut *c, &mut self.tcp).write(buf)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &self.tls {
            None => self.tcp.flush(),
            Some(t) => {
                let mut c = t.lock().map_err(|_| io::Error::other("tls session"))?;
                rustls::Stream::new(&mut *c, &mut self.tcp).flush()
            }
        }
    }
}

/// The TLS configuration of `--tls-cert`/`--tls-key`: a PEM certificate chain and private key.
pub fn tls_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let bad = |m: String| SitefileError::plain(E_USAGE, m);
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| bad(format!("--tls-cert: not a PEM certificate: {e}")))?;
    if certs.is_empty() {
        return Err(bad("--tls-cert: no certificate in the file".into()));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| bad(format!("--tls-key: not a PEM private key: {e}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad(format!("TLS: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| bad(format!("--tls-key does not match --tls-cert: {e}")))?;
    Ok(Arc::new(config))
}

/// A parsed CIDR block: 4 or 16 address bytes and a prefix length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    pub bytes: Vec<u8>,
    pub prefix: u32,
}

/// `--allow-ip` / `--deny-ip` blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpRules {
    pub allow: Vec<Cidr>,
    pub deny: Vec<Cidr>,
}

/// Address bytes of an IPv4 or IPv6 literal (`::ffff:a.b.c.d` becomes IPv4; `ipBytes` in
/// src/server.ts).
pub fn ip_bytes(text: &str) -> Option<Vec<u8>> {
    let t = match text.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("::ffff:") && text[7..].contains('.') => &text[7..],
        _ => text,
    };
    if is_ipv4(t) {
        return Some(t.split('.').map(|p| p.parse::<u8>().unwrap_or(0)).collect());
    }
    if t.is_empty() || !t.contains(':') || !t.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
        return None;
    }
    let halves: Vec<&str> = t.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let words = |h: &str| -> Option<Vec<u16>> {
        if h.is_empty() {
            return Some(Vec::new());
        }
        h.split(':')
            .map(|w| {
                if (1..=4).contains(&w.len()) {
                    u16::from_str_radix(w, 16).ok()
                } else {
                    None
                }
            })
            .collect()
    };
    let a = words(halves[0])?;
    let b = if halves.len() == 2 {
        words(halves[1])?
    } else {
        Vec::new()
    };
    let used = a.len() + b.len();
    let fill = if halves.len() == 2 {
        if used >= 8 {
            return None;
        }
        8 - used
    } else {
        0
    };
    if used + fill != 8 {
        return None;
    }
    let mut all = a;
    all.extend(std::iter::repeat_n(0u16, fill));
    all.extend(b);
    Some(all.iter().flat_map(|w| w.to_be_bytes()).collect())
}

/// `a.b.c.d/n`, `x::y/n` or a bare address (`parseCidr` in src/server.ts).
pub fn parse_cidr(text: &str) -> Option<Cidr> {
    let parts: Vec<&str> = text.trim().split('/').collect();
    if parts.len() > 2 {
        return None;
    }
    let bytes = ip_bytes(parts[0])?;
    let max = (bytes.len() * 8) as u32;
    let prefix = match parts.get(1) {
        None => max,
        Some(l) if (1..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_digit()) => {
            l.parse().ok()?
        }
        Some(_) => return None,
    };
    (prefix <= max).then_some(Cidr { bytes, prefix })
}

fn cidr_contains(c: &Cidr, ip: &[u8]) -> bool {
    c.bytes.len() == ip.len()
        && (0..c.prefix as usize).all(|bit| {
            let mask = 0x80u8 >> (bit % 8);
            c.bytes[bit / 8] & mask == ip[bit / 8] & mask
        })
}

/// Whether `--allow-ip`/`--deny-ip` let a client in (`ipAllowed` in src/server.ts).
pub fn ip_allowed(rules: Option<&IpRules>, remote: &str) -> bool {
    let Some(r) = rules else { return true };
    if is_loopback_text(remote) {
        return true;
    }
    let Some(ip) = ip_bytes(remote) else {
        return false;
    };
    if r.deny.iter().any(|c| cidr_contains(c, &ip)) {
        return false;
    }
    r.allow.is_empty() || r.allow.iter().any(|c| cidr_contains(c, &ip))
}

/// `isLoopbackAddress` in src/server.ts.
fn is_loopback_text(ip: &str) -> bool {
    let t = match ip.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("::ffff:") => &ip[7..],
        _ => ip,
    };
    ip == "::1" || t.starts_with("127.")
}

/// A short device name from a `User-Agent` (`deviceOf` in src/server.ts).
pub fn device_of(user_agent: Option<&str>) -> &'static str {
    let ua = user_agent.unwrap_or("");
    for (needle, name) in [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("Windows", "Windows"),
        ("Macintosh", "Mac"),
        ("Linux", "Linux"),
    ] {
        if ua.contains(needle) {
            return name;
        }
    }
    "other"
}

/// Loopback, also as an IPv4-mapped IPv6 address (`::ffff:127.0.0.1`).
fn is_loopback_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => a.is_loopback(),
        std::net::IpAddr::V6(a) => {
            a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// The first of repeated header values (ours are joined with `, `; Node keeps the first
/// `Authorization`).
fn first_value(v: Option<&String>) -> Option<&str> {
    v.map(|s| s.split(',').next().unwrap_or("").trim())
}

/// The headers every response starts with (`base` in src/server.ts).
fn base_headers(options: &SiteServerOptions) -> Vec<(String, String)> {
    let mut base: Vec<(String, String)> = vec![
        ("x-content-type-options".into(), "nosniff".into()),
        ("cache-control".into(), "no-cache".into()),
    ];
    if let Some(id) = options.server_id.as_deref().filter(|s| !s.is_empty()) {
        base.push(("x-sitefile-server".into(), id.to_string()));
    }
    if options.cors {
        base.push(("access-control-allow-origin".into(), "*".into()));
    }
    if options.noindex {
        base.push(("x-robots-tag".into(), "noindex".into()));
    }
    base.extend(isolation_headers(options));
    base
}

/// One access-log request, as [`access_log_line`] writes it.
pub struct AccessLogEntry<'a> {
    pub ts: &'a str,
    pub method: &'a str,
    /// The request target as received (made printable).
    pub target: &'a str,
    pub status: u16,
    pub bytes: usize,
    pub ms: f64,
    /// `Content-Range` of a `206`/`416`.
    pub range: Option<&'a str>,
    /// The client address (`--log-ip`, or a non-loopback client with client info).
    /// What answered, as the `_redirects` log note (` → /new (_redirects:3)`).
    pub note: Option<&'a str>,
    /// The client address (`--log-ip`).
    pub remote: Option<&'a str>,
    /// The client's device ([`device_of`]).
    pub device: Option<&'a str>,
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// An access-log line, text or JSON (`accessLogLine` in src/server.ts; the JSON form is a stable
/// contract, docs/CLI.md).
pub fn access_log_line(json: bool, e: &AccessLogEntry) -> String {
    let target = printable(e.target);
    if !json {
        return format!(
            "{} {target} {} {} {:.1}ms{}{}{}",
            e.method,
            e.status,
            e.bytes,
            e.ms,
            e.note.unwrap_or(""),
            e.remote.map(|r| format!(" {r}")).unwrap_or_default(),
            e.device.map(|d| format!(" {d}")).unwrap_or_default()
        );
    }
    let (path, query) = match target.find('?') {
        Some(q) => (&target[..q], &target[q + 1..]),
        None => (target.as_str(), ""),
    };
    format!(
        "{{\"type\":\"request\",\"ts\":{},\"method\":{},\"path\":{},\"query\":{},\"status\":{},\"bytes\":{},\"ms\":{:.1},\"encoding\":\"identity\",\"range\":{}{}{}{}}}",
        json_str(e.ts),
        json_str(e.method),
        json_str(path),
        json_str(query),
        e.status,
        e.bytes,
        e.ms,
        e.range.map(json_str).unwrap_or_else(|| "null".into()),
        e.note
            .map(|n| format!(",\"note\":{}", json_str(n.strip_prefix(" \u{2192} ").unwrap_or(n))))
            .unwrap_or_default(),
        e.remote
            .map(|r| format!(",\"remote\":{}", json_str(r)))
            .unwrap_or_default(),
        e.device
            .map(|d| format!(",\"device\":{}", json_str(d)))
            .unwrap_or_default()
    )
}

/// A log line that is not a request, as a JSON object (`jsonLogLine` in src/server.ts).
pub fn json_log_line(ts: &str, line: &str) -> String {
    let (kind, message) = if let Some(m) = line.strip_prefix("warning: ") {
        ("warning", m)
    } else if line.starts_with("error") {
        ("error", line)
    } else {
        ("message", line)
    };
    format!(
        "{{\"type\":{},\"ts\":{},\"message\":{}}}",
        json_str(kind),
        json_str(ts),
        json_str(message)
    )
}

/// `{"type":"started","ts","site","files","url"}` (`startedLogLine` in src/server.ts).
pub fn started_log_line(ts: &str, site: &str, files: usize, url: &str) -> String {
    format!(
        "{{\"type\":\"started\",\"ts\":{},\"site\":{},\"files\":{files},\"url\":{}}}",
        json_str(ts),
        json_str(site),
        json_str(url)
    )
}

/// `{"type":"stopped","ts","signal"}` (`stoppedLogLine` in src/server.ts).
pub fn stopped_log_line(ts: &str, signal: &str) -> String {
    format!(
        "{{\"type\":\"stopped\",\"ts\":{},\"signal\":{}}}",
        json_str(ts),
        json_str(signal)
    )
}

/// A detached server's log line as JSON (`logLineAsJson` in src/server.ts).
pub fn log_line_as_json(line: &str) -> String {
    if line.starts_with('{') {
        return line.to_string();
    }
    let is_ts = |t: &str| {
        let b = t.as_bytes();
        b.len() > 11
            && b[..4].iter().all(u8::is_ascii_digit)
            && b[4] == b'-'
            && b[5..7].iter().all(u8::is_ascii_digit)
            && b[7] == b'-'
            && b[8..10].iter().all(u8::is_ascii_digit)
            && b[10] == b'T'
            && b.ends_with(b"Z")
            && b.len() > 12
            && b[11..b.len() - 1]
                .iter()
                .all(|c| c.is_ascii_digit() || *c == b':' || *c == b'.')
    };
    let Some((ts, rest)) = line.split_once(' ').filter(|(t, _)| is_ts(t)) else {
        return json_log_line("", line);
    };
    if let Some(r) = parse_text_request(rest) {
        return access_log_line(
            true,
            &AccessLogEntry {
                ts,
                method: r.0,
                target: r.1,
                status: r.2,
                bytes: r.3,
                ms: r.4,
                range: None,
                note: None,
                remote: r.5,
                device: None,
            },
        );
    }
    if let Some(body) = rest.strip_prefix("serving ") {
        if let Some((site, tail)) = body.rsplit_once(" (") {
            if let Some((n, url)) = tail.split_once(" files) at ") {
                if !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_digit())
                    && !url.is_empty()
                    && !url.contains(char::is_whitespace)
                    && !site.is_empty()
                {
                    if let Ok(files) = n.parse() {
                        return started_log_line(ts, site, files, url);
                    }
                }
            }
        }
    }
    if let Some(sig) = rest
        .strip_prefix("received ")
        .and_then(|r| r.strip_suffix(", shutting down"))
        .filter(|s| {
            s.len() > 3 && s.starts_with("SIG") && s[3..].bytes().all(|b| b.is_ascii_uppercase())
        })
    {
        return stopped_log_line(ts, sig);
    }
    json_log_line(ts, rest)
}

/// `METHOD target STATUS BYTES MS.Dms[ remote]`, a text access-log line.
#[allow(clippy::type_complexity)]
fn parse_text_request(rest: &str) -> Option<(&str, &str, u16, usize, f64, Option<&str>)> {
    let parts: Vec<&str> = rest.split(' ').collect();
    if !(5..=6).contains(&parts.len()) || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let method = parts[0];
    let mb = method.as_bytes();
    if !mb[0].is_ascii_uppercase() || !mb.iter().all(|b| b.is_ascii_uppercase() || *b == b'-') {
        return None;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if parts[2].len() != 3 || !digits(parts[2]) || !digits(parts[3]) {
        return None;
    }
    let ms = parts[4].strip_suffix("ms")?;
    let (whole, frac) = ms.split_once('.')?;
    if !digits(whole) || frac.len() != 1 || !digits(frac) {
        return None;
    }
    if parts[1].contains(char::is_whitespace) {
        return None;
    }
    Some((
        method,
        parts[1],
        parts[2].parse().ok()?,
        parts[3].parse().ok()?,
        ms.parse().ok()?,
        parts.get(5).copied(),
    ))
}

/// Production cache headers to preview instead of `no-cache` (`serve --max-age`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePreview {
    /// Seconds for files other than HTML: `public, max-age=N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<u64>,
    /// Adds `immutable` to the non-HTML value (needs `max_age`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub immutable: bool,
    /// Seconds for HTML pages (`None`: they keep `no-cache`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_html: Option<u64>,
}

/// The `Cache-Control` a file of media type `mime` gets under `cache` (`cacheControlFor` in
/// src/server.ts), or `None` to keep `no-cache`.
pub fn cache_control_for(mime: &str, cache: Option<&CachePreview>) -> Option<String> {
    let c = cache?;
    if mime.starts_with("text/html") {
        return c.max_age_html.map(|n| format!("public, max-age={n}"));
    }
    c.max_age.map(|n| {
        format!(
            "public, max-age={n}{}",
            if c.immutable { ", immutable" } else { "" }
        )
    })
}

/// The `--isolate` headers ([`SiteServerOptions::isolate`]).
fn isolation_headers(options: &SiteServerOptions) -> Vec<(String, String)> {
    match &options.isolate {
        Some(coep) => vec![
            ("cross-origin-opener-policy".into(), "same-origin".into()),
            ("cross-origin-embedder-policy".into(), coep.clone()),
            ("cross-origin-resource-policy".into(), "same-origin".into()),
        ],
        None => Vec::new(),
    }
}

/// `Access-Control-Request-Headers` echoed in a preflight answer, when it is a plain list of
/// header names (`preflightHeaders` in src/server.ts).
fn preflight_headers(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    let ok = !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" \t,!#$%&'*+-.^_`|~".contains(&b));
    ok.then(|| v.to_string())
}

const FORBIDDEN_PAGE: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>403 Forbidden</title><h1>403 Forbidden</h1>\n";
const UNAUTHORIZED_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>401 Unauthorized</title><h1>401 Unauthorized</h1>\n";
const FORBIDDEN_HOST_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>403 Forbidden</title><h1>403 Forbidden</h1><p>This server answers requests for localhost and IP addresses only. To allow another host name, restart it with <code>--allowed-host &lt;name&gt;</code>.</p>\n";
const ERROR_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>500 Internal Server Error</title><h1>500 Internal Server Error</h1>\n";
const METHOD_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>405 Method Not Allowed</title><h1>405 Method Not Allowed</h1>\n";

/// `decodeURIComponent`, or `None` where it would throw.
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            let (h, l) = (
                b.get(i + 1).copied().and_then(hex)?,
                b.get(i + 2).copied().and_then(hex)?,
            );
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Decode a request path into safe segments, or `None` when it is malformed or unsafe.
pub fn safe_segments(raw_path: &str) -> Option<Vec<String>> {
    if !raw_path.starts_with('/') || raw_path.contains('\\') || raw_path.contains('\0') {
        return None;
    }
    let mut out = Vec::new();
    for raw in raw_path.split('/') {
        if raw.is_empty() {
            continue;
        }
        let seg = decode_uri_component(raw)?;
        // A decoded '/', '\', NUL, control character, '.' or '..' could change what is read.
        if seg == "."
            || seg == ".."
            || seg
                .chars()
                .any(|c| c == '\\' || c == '/' || (c as u32) < 0x20 || c == '\u{7f}')
        {
            return None;
        }
        // Tree paths are NFC; a request for an NFD spelling finds the same file.
        out.push(crate::paths::nfc(&seg).into_owned());
    }
    Some(out)
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Days since 1970-01-01 of a proleptic Gregorian date (month 1..=12).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`, RFC 9110 section 5.6.7) for Unix seconds.
pub fn http_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Unix seconds of an IMF-fixdate, or `None` for anything else (the obsolete formats too).
pub fn parse_http_date(value: &str) -> Option<i64> {
    let v = value.trim();
    let b = v.as_bytes();
    if b.len() != 29 || &v[3..5] != ", " || &v[25..] != " GMT" {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = v.get(r)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    if b[7] != b' ' || b[11] != b' ' || b[16] != b' ' || b[19] != b':' || b[22] != b':' {
        return None;
    }
    let wd = DAYS.iter().position(|d| *d == &v[0..3])?;
    let month = MONTHS.iter().position(|m| *m == &v[8..11])? as i64 + 1;
    let (day, year) = (num(5..7)?, num(12..16)?);
    let (h, mi, se) = (num(17..19)?, num(20..22)?, num(23..25)?);
    if day < 1 || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // The day must exist in that month and fall on the weekday given.
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_len = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day > month_len[(month - 1) as usize] || (days + 4).rem_euclid(7) as usize != wd {
        return None;
    }
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

/// One link-value as Node's `writeEarlyHints` accepts it (`LINK_VALUE` in src/server.ts):
/// `<uri>` then `; param[=value]` pairs, a quoted value without whitespace, `;` or `"`.
fn is_link_value(p: &str) -> bool {
    let ws =
        |c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\x0b' || c == '\x0c';
    let Some(rest) = p.strip_prefix('<') else {
        return false;
    };
    let Some(close) = rest.find('>') else {
        return false;
    };
    if rest[..close].contains(['\r', '\n']) {
        return false;
    }
    let mut it = rest[close + 1..].chars().peekable();
    loop {
        while it.peek().is_some_and(|c| ws(*c)) {
            it.next();
        }
        match it.next() {
            None => return true,
            Some(';') => {}
            Some(_) => return false,
        }
        while it.peek().is_some_and(|c| ws(*c)) {
            it.next();
        }
        let mut token = String::new();
        while let Some(&c) = it.peek() {
            if c == ';' || c == '"' || ws(c) {
                break;
            }
            token.push(c);
            it.next();
        }
        if token.is_empty() {
            return false;
        }
        if it.peek() == Some(&'"') {
            // `name="value"`: the quote must follow `=` after a non-empty name.
            if !(token.len() >= 2 && token.ends_with('=')) {
                return false;
            }
            it.next();
            loop {
                match it.next() {
                    Some('"') => break,
                    Some(c) if c == ';' || ws(c) => return false,
                    Some(_) => {}
                    None => return false,
                }
            }
            if it.peek().is_some_and(|c| !(ws(*c) || *c == ';')) {
                return false;
            }
        }
    }
}

/// The link-values of a `Link` header that become `103 Early Hints`: split at commas outside
/// `<…>` and quotes, kept when well formed and `rel=preload` or `rel=preconnect`.
pub fn early_hints(link: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut angle, mut quote) = (false, false);
    for c in link.chars() {
        if c == '<' && !quote {
            angle = true;
        } else if c == '>' && !quote {
            angle = false;
        } else if c == '"' && !angle {
            quote = !quote;
        }
        if c == ',' && !angle && !quote {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| {
            if !is_link_value(p) {
                return false;
            }
            let v: String = p
                .to_ascii_lowercase()
                .chars()
                .filter(|c| *c != ' ' && *c != '\t')
                .collect();
            [
                "rel=preload",
                "rel=\"preload\"",
                "rel=preconnect",
                "rel=\"preconnect\"",
            ]
            .iter()
            .any(|t| v.contains(t))
        })
        .map(str::to_string)
        .collect()
}

/// What a `Range` header asks of a body (see [`byte_range`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// First and last byte, inclusive.
    Bytes(usize, usize),
    /// `416 Range Not Satisfiable`.
    Unsatisfiable,
}

/// A `bytes=` value's number: digits only; more than 15 digits counts as larger than any file.
fn range_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(if s.len() > 15 {
        u64::MAX
    } else {
        s.parse().ok()?
    })
}

/// The byte range a `Range` header asks of a body of `size` bytes, or `None` when the header is
/// absent, malformed, not in bytes, or asks for several ranges (those are ignored: the whole body
/// is sent, as RFC 9110 allows). Same rules as `byteRange` in src/server.ts.
pub fn byte_range(header: Option<&str>, size: usize) -> Option<ByteRange> {
    let v = header?.trim();
    if v.len() < 6 || !v.as_bytes()[..6].eq_ignore_ascii_case(b"bytes=") {
        return None;
    }
    let spec = v[6..].trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    let size64 = size as u64;
    if first.is_empty() {
        let n = range_number(last)?;
        if n == 0 || size == 0 {
            return Some(ByteRange::Unsatisfiable);
        }
        return Some(ByteRange::Bytes(
            size64.saturating_sub(n) as usize,
            size - 1,
        ));
    }
    let start = range_number(first)?;
    let end = if last.is_empty() {
        u64::MAX
    } else {
        range_number(last)?
    };
    if end < start {
        return None;
    }
    if start >= size64 {
        return Some(ByteRange::Unsatisfiable);
    }
    Some(ByteRange::Bytes(
        start as usize,
        end.min(size64 - 1) as usize,
    ))
}

fn etag_matches(header: Option<&str>, etag: &str) -> bool {
    let Some(h) = header else { return false };
    if h.trim() == "*" {
        return true;
    }
    h.split(',').any(|t| {
        let t = t.trim();
        t.strip_prefix("W/").unwrap_or(t) == etag
    })
}

/// The app shell a `--spa` navigation to a missing path gets: `fallback` when given
/// (`--spa-fallback`), else the `200.html` (Surge) or `index.html` of the request's directory or
/// the closest parent that has one, so `/admin/users/5` gets `/admin/index.html` before
/// `/index.html` (port of `spaFallback`).
pub fn spa_fallback(
    segs: &[String],
    trailing: bool,
    is_file: &dyn Fn(&str) -> bool,
    fallback: Option<&str>,
) -> Option<String> {
    if let Some(f) = fallback {
        return is_file(f).then(|| f.to_string());
    }
    let dirs = if trailing {
        segs
    } else {
        &segs[..segs.len().saturating_sub(1)]
    };
    for n in (0..=dirs.len()).rev() {
        let dir: String = dirs[..n].iter().map(|d| format!("{d}/")).collect();
        for name in ["200.html", "index.html"] {
            let p = format!("/{dir}{name}");
            if is_file(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// The `--base` of a server, read per request.
pub type BaseFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// A `--base` value as `/a/b/` (segments percent-encoded like encodeURIComponent, NFC), `/` for
/// none, or `None` when it is not a safe absolute path. The path of a URL
/// (`https://user.github.io/repo/`) is taken as well (port of `normalizeBase`).
pub fn normalize_base(raw: &str) -> Option<String> {
    let n = http_prefix(raw);
    let p = if n > 0 {
        let rest = &raw[n..];
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let after = &rest[auth_end..];
        if after.starts_with('/') {
            &after[..after.find(['?', '#']).unwrap_or(after.len())]
        } else {
            "/"
        }
    } else {
        raw
    };
    let segs = if p.starts_with('/') {
        safe_segments(p)
    } else {
        safe_segments(&format!("/{p}"))
    }?;
    if segs.is_empty() {
        return Some("/".into());
    }
    let enc: Vec<String> = segs.iter().map(|s| encode_uri_component(s)).collect();
    Some(format!("/{}/", enc.join("/")))
}

/// A tree path with each segment percent-encoded like encodeURIComponent.
fn encode_path(p: &str) -> String {
    let enc: Vec<String> = p.split('/').map(encode_uri_component).collect();
    enc.join("/")
}

fn printable(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let n = c as u32;
        if (0x21..=0x7e).contains(&n) {
            out.push(c);
        } else {
            out.push_str(&format!("%{n:02x}"));
        }
    }
    out.chars().take(512).collect()
}

/// A response before it is written. `body` is empty for HEAD and 304.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Lowercase header names, in order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// What answered, for the access log (` → /new (_redirects:3)`), when a rule did.
    pub note: Option<String>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        500 => "Internal Server Error",
        416 => "Range Not Satisfiable",
        410 => "Gone",
        451 => "Unavailable For Legal Reasons",
        _ => "",
    }
}

// ---------------------------------------------------------------- _redirects and _headers
// (src/serve-rules.ts)

/// A segment of a `_redirects` source: a literal (decoded) or a `:placeholder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectSegment {
    Lit(String),
    Param(String),
}

/// A `_redirects` condition: `language`, `country` or `cookie` and its values (ASCII-lowercased
/// for language and country; cookie names as written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectCondition {
    pub key: &'static str,
    pub values: Vec<String>,
}

/// A `_redirects` rule `serve` understands (docs/CLI.md, "serve").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeRedirect {
    /// 1-based line in `_redirects` (for the access log).
    pub line: usize,
    /// Decoded path without a trailing slash (`/` for the root), placeholders written `:name`; for a
    /// splat rule, the prefix before `/*` (`""` for `/*`).
    pub from: String,
    pub segments: Vec<RedirectSegment>,
    pub splat: bool,
    /// Query parameters the request must have: `(name, value)` (decoded); a value `:name` captures it.
    pub query: Vec<(String, String)>,
    pub to: String,
    pub status: u16,
    /// `301!`: applies even when a file exists at the path.
    pub force: bool,
    pub conditions: Vec<RedirectCondition>,
}

impl ServeRedirect {
    /// A rule that depends on nothing but the path: no placeholders, query parameters or conditions.
    pub fn is_plain(&self) -> bool {
        self.query.is_empty()
            && self.conditions.is_empty()
            && self
                .segments
                .iter()
                .all(|s| matches!(s, RedirectSegment::Lit(_)))
    }
}

/// A line of `_redirects` or `_headers` that `serve` ignores, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredLine {
    pub line: usize,
    pub reason: String,
}

/// What a request offers a rule's query parameters and conditions.
#[derive(Debug, Clone, Default)]
pub struct RedirectRequest<'a> {
    /// The query string with its `?` (`""` when none).
    pub search: &'a str,
    pub accept_language: Option<&'a str>,
    pub cookie: Option<&'a str>,
    /// `serve --country`.
    pub country: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeHeaders {
    pub pattern: String,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
}

/// Headers the server owns; `_headers` cannot set them.
const PROTECTED_HEADERS: [&str; 10] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "etag",
    "location",
    "accept-ranges",
    "content-range",
    "last-modified",
    "www-authenticate",
];
pub const SERVE_STATUSES: [u16; 9] = [200, 301, 302, 303, 307, 308, 404, 410, 451];

/// Statuses that serve the target path's file with that status instead of redirecting.
pub fn is_rewrite_status(s: u16) -> bool {
    matches!(s, 200 | 404 | 410 | 451)
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`
fn is_placeholder(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// `^https?://[^/]` (case-insensitive).
fn is_http_url(s: &str) -> bool {
    let n = http_prefix(s);
    n > 0 && s.as_bytes().get(n).is_some_and(|b| *b != b'/')
}

/// The length of a leading `http://` or `https://` (any case), else 0.
fn http_prefix(s: &str) -> usize {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        0
    }
}

/// JavaScript's `String.prototype.trim` whitespace (the characters that matter in these files).
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `decodeURIComponent` after `+` becomes a space (form encoding), or `None` when malformed.
fn decode_query_part(s: &str) -> Option<String> {
    decode_uri_component(&s.replace('+', " "))
}

/// The decoded `name=value` pairs of a query string (without its `?`); malformed pairs are skipped.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in q.split('&').filter(|p| !p.is_empty()) {
        let (n, v) = part.split_once('=').unwrap_or((part, ""));
        if let (Some(n), Some(v)) = (decode_query_part(n), decode_query_part(v)) {
            out.push((n, v));
        }
    }
    out
}

fn is_target_field(f: &str) -> bool {
    f.starts_with('/') || http_prefix(f) > 0
}

/// `^[0-9]{3}!?$`
fn is_status_field(f: &str) -> bool {
    let d = f.strip_suffix('!').unwrap_or(f);
    d.len() == 3 && d.bytes().all(|b| b.is_ascii_digit())
}

/// Parse `_redirects` (Netlify / Cloudflare Pages): `from [name=value…] to [status[!]] [Key=values…]`
/// per line, `#` comments. Supported: literal segments, `:placeholders` (one segment each) and a
/// trailing `/*` (`:splat`); query parameters, literal or `:placeholder`; statuses 301, 302, 303,
/// 307, 308 (redirect), 200 (rewrite), 404, 410 and 451 (the target's file with that status);
/// conditions `Language=`, `Country=` and `Cookie=`. Every other line is in the second list, with why.
pub fn parse_redirects(text: &str) -> (Vec<ServeRedirect>, Vec<IgnoredLine>) {
    let mut rules = Vec::new();
    let mut ignored = Vec::new();
    for (index, raw) in text.split('\n').enumerate() {
        let t = js_trim(raw);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        match parse_redirect_line(t, index + 1) {
            Ok(r) => rules.push(r),
            Err(reason) => ignored.push(IgnoredLine {
                line: index + 1,
                reason,
            }),
        }
    }
    (rules, ignored)
}

/// One non-comment line: its rule, or why it is ignored.
fn parse_redirect_line(t: &str, line: usize) -> std::result::Result<ServeRedirect, String> {
    let fields: Vec<&str> = t.split([' ', '\t']).filter(|f| !f.is_empty()).collect();
    let from = fields[0];
    let mut k = 1;
    let mut query = Vec::new();
    while k < fields.len() && !is_target_field(fields[k]) {
        let f = fields[k];
        let Some(i) = f.find('=').filter(|i| *i > 0) else {
            break;
        };
        let (name, value) = (decode_query_part(&f[..i]), decode_query_part(&f[i + 1..]));
        match (name, value) {
            (Some(n), Some(v)) if !v.starts_with(':') || is_placeholder(&v[1..]) => {
                query.push((n, v))
            }
            _ => {
                return Err("bad query parameter (expected name=value or name=:placeholder)".into())
            }
        }
        k += 1;
    }
    if k >= fields.len() {
        return Err("expected 'from [name=value...] to [status] [conditions]'".into());
    }
    let to = fields[k];
    k += 1;
    let mut st = "301";
    if k < fields.len() && is_status_field(fields[k]) {
        st = fields[k];
        k += 1;
    }
    let mut conditions = Vec::new();
    for f in &fields[k..] {
        let Some(i) = f.find('=').filter(|i| *i > 0) else {
            return Err("unexpected field (expected a status, then Key=value conditions)".into());
        };
        let key = f[..i].to_ascii_lowercase();
        let values: Vec<&str> = f[i + 1..].split(',').filter(|x| !x.is_empty()).collect();
        let key: &'static str = match key.as_str() {
            "role" => {
                return Err("Role= conditions need Netlify Identity and are not supported".into())
            }
            "language" => "language",
            "country" => "country",
            "cookie" => "cookie",
            _ => {
                return Err(
                    "unsupported condition (supported: Language=, Country=, Cookie=)".into(),
                )
            }
        };
        if values.is_empty() {
            return Err("a condition needs at least one value".into());
        }
        conditions.push(RedirectCondition {
            key,
            values: values
                .iter()
                .map(|v| {
                    if key == "cookie" {
                        v.to_string()
                    } else {
                        v.to_ascii_lowercase()
                    }
                })
                .collect(),
        });
    }
    let force = st.ends_with('!');
    let status: u16 = st.trim_end_matches('!').parse().unwrap_or(0);
    if !SERVE_STATUSES.contains(&status) {
        let list: Vec<String> = SERVE_STATUSES.iter().map(|s| s.to_string()).collect();
        return Err(format!(
            "unsupported status (supported: {})",
            list.join(", ")
        ));
    }
    if !to.bytes().all(|b| (0x21..=0x7e).contains(&b)) || !(to.starts_with('/') || is_http_url(to))
    {
        return Err(
            "the target must be an absolute path or an http(s) URL, in printable ASCII".into(),
        );
    }
    if is_rewrite_status(status) && !to.starts_with('/') {
        return Err(format!(
            "status {status} needs a path target (proxying to a URL is not supported)"
        ));
    }
    if !from.starts_with('/') {
        return Err("the source must be a path starting with '/'".into());
    }
    let splat = from == "/*" || from.ends_with("/*");
    let prefix = if splat { &from[..from.len() - 2] } else { from };
    let mut segments = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    for raw in prefix.split('/').filter(|s| !s.is_empty()) {
        if raw.contains('*') {
            return Err("'*' is only supported as the last segment of the source".into());
        }
        if let Some(name) = raw.strip_prefix(':') {
            if !is_placeholder(name) {
                return Err("bad placeholder (expected :name of letters, digits and _)".into());
            }
            if name == "splat" {
                return Err(":splat is the value of a trailing '*', not a placeholder".into());
            }
            if names.contains(&name) {
                return Err("a placeholder is used twice in the source".into());
            }
            names.push(name);
            segments.push(RedirectSegment::Param(name.to_string()));
            continue;
        }
        let Some(mut seg) = safe_segments(&format!("/{raw}")) else {
            return Err("the source is not a valid path".into());
        };
        segments.push(RedirectSegment::Lit(seg.remove(0)));
    }
    let shown: Vec<String> = segments
        .iter()
        .map(|s| match s {
            RedirectSegment::Lit(l) => l.clone(),
            RedirectSegment::Param(p) => format!(":{p}"),
        })
        .collect();
    let shown = format!("/{}", shown.join("/"));
    Ok(ServeRedirect {
        line,
        from: if splat && shown == "/" {
            String::new()
        } else {
            shown
        },
        segments,
        splat,
        query,
        to: to.to_string(),
        status,
        force,
        conditions,
    })
}

/// [`parse_redirects`]' rules.
pub fn parse_redirect_rules(text: &str) -> Vec<ServeRedirect> {
    parse_redirects(text).0
}

/// Parse `_headers`: a path pattern line (`*` matches anything), then indented `Name: value` lines.
/// Every other line is in the second list, with why.
pub fn parse_headers(text: &str) -> (Vec<ServeHeaders>, Vec<IgnoredLine>) {
    let mut out: Vec<ServeHeaders> = Vec::new();
    let mut ignored = Vec::new();
    let mut cur = false;
    for (index, raw) in text.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let t = js_trim(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut skip = |reason: &str| {
            ignored.push(IgnoredLine {
                line: index + 1,
                reason: reason.to_string(),
            })
        };
        if line.starts_with([' ', '\t']) {
            if !cur {
                skip("a header line without a path line above it");
                continue;
            }
            if t.starts_with('!') {
                skip("'! Name' detach lines are not supported");
                continue;
            }
            let Some(i) = t.find(':').filter(|i| *i > 0) else {
                skip("expected 'Name: value'");
                continue;
            };
            let name = js_trim(&t[..i]).to_ascii_lowercase();
            let value = js_trim(&t[i + 1..]);
            if !crate::site::is_token(&name) {
                skip("invalid header name");
            } else if !value
                .bytes()
                .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
            {
                skip("the value must be printable ASCII");
            } else if PROTECTED_HEADERS.contains(&name.as_str()) {
                skip("the server sets this header itself");
            } else if let Some(last) = out.last_mut() {
                last.headers.push((name, value.to_string()));
            }
            continue;
        }
        cur = t.starts_with('/');
        if cur {
            out.push(ServeHeaders {
                pattern: t.to_string(),
                headers: Vec::new(),
            });
        } else {
            skip("a path pattern must start with '/' (indent header lines)");
        }
    }
    (out, ignored)
}

/// [`parse_headers`]' rules.
pub fn parse_header_rules(text: &str) -> Vec<ServeHeaders> {
    parse_headers(text).0
}

/// The warnings for a tree's `/_redirects` and `/_headers` lines `serve` ignores, one per line:
/// `_redirects:3: ignored: <reason>`.
/// `content` reads files a zip served in place holds only as digests.
pub fn site_file_warnings(tree: &Tree, content: Option<&dyn SiteContent>) -> Vec<String> {
    let mut out = Vec::new();
    let text = |p: &str| {
        tree.get(p).map(|f| match content.and_then(|c| c.read(p)) {
            Some(Ok(b)) => String::from_utf8_lossy(&b).into_owned(),
            Some(Err(_)) => String::new(),
            None => String::from_utf8_lossy(&f.bytes).into_owned(),
        })
    };
    if let Some(t) = text("/_redirects") {
        for i in parse_redirects(&t).1 {
            out.push(format!("_redirects:{}: ignored: {}", i.line, i.reason));
        }
    }
    if let Some(t) = text("/_headers") {
        for i in parse_headers(&t).1 {
            out.push(format!("_headers:{}: ignored: {}", i.line, i.reason));
        }
    }
    out
}

/// Glob match where `*` matches any run of characters (including `/`).
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = si;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Every matching block's headers; a name set by several blocks gets its values joined with `, `.
pub fn headers_for(rules: &[ServeHeaders], path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for r in rules.iter().filter(|r| glob_match(&r.pattern, path)) {
        for (k, v) in &r.headers {
            match out.iter_mut().find(|(n, _)| n == k) {
                Some((_, existing)) => *existing = format!("{existing}, {v}"),
                None => out.push((k.clone(), v.clone())),
            }
        }
    }
    out
}

/// Set `name` in captured values (replacing an earlier value).
fn set_capture(caps: &mut Vec<(String, String)>, name: &str, value: String) {
    match caps.iter_mut().find(|(n, _)| n == name) {
        Some(c) => c.1 = value,
        None => caps.push((name.to_string(), value)),
    }
}

/// The values a rule's source captures from `path` (decoded, no trailing slash): each placeholder,
/// and `splat` (the remainder, `""` for the prefix itself) for a splat rule; `None` when it does
/// not match.
pub fn match_redirect(r: &ServeRedirect, path: &str) -> Option<Vec<(String, String)>> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let n = r.segments.len();
    if if r.splat {
        segs.len() < n
    } else {
        segs.len() != n
    } {
        return None;
    }
    let mut caps = Vec::new();
    for (s, got) in r.segments.iter().zip(&segs) {
        match s {
            RedirectSegment::Param(p) => set_capture(&mut caps, p, got.to_string()),
            RedirectSegment::Lit(l) if l == got => {}
            RedirectSegment::Lit(_) => return None,
        }
    }
    if r.splat {
        set_capture(&mut caps, "splat", segs[n..].join("/"));
    }
    Some(caps)
}

/// Whether `Accept-Language` names a language a `Language=` value matches (`en` matches `en-GB`).
fn language_matches(values: &[String], header: &str) -> bool {
    for part in header.split(',') {
        let mut it = part.split(';');
        let tag = it.next().unwrap_or("");
        let q = it
            .map(js_trim)
            .find(|p| p.starts_with("q=") || p.starts_with("Q="));
        if let Some(q) = q {
            let v = &q[2..];
            let zero = v == "0"
                || v.strip_prefix("0.")
                    .is_some_and(|z| z.len() <= 3 && z.bytes().all(|b| b == b'0'));
            if zero {
                continue;
            }
        }
        let t = js_trim(tag).to_ascii_lowercase();
        if !t.is_empty()
            && values
                .iter()
                .any(|v| t == *v || t.starts_with(&format!("{v}-")))
        {
            return true;
        }
    }
    false
}

/// Whether a `Cookie` header has a cookie with one of these names.
fn cookie_matches(names: &[String], header: &str) -> bool {
    header.split(';').any(|c| {
        c.find('=')
            .filter(|i| *i > 0)
            .is_some_and(|i| names.iter().any(|n| n == js_trim(&c[..i])))
    })
}

/// [`match_redirect`], then the rule's query parameters (adding their captures) and conditions.
pub fn match_rule(
    r: &ServeRedirect,
    path: &str,
    req: &RedirectRequest,
) -> Option<Vec<(String, String)>> {
    let mut caps = match_redirect(r, path)?;
    if !r.query.is_empty() {
        let pairs = parse_query(req.search.get(1..).unwrap_or(""));
        for (name, value) in &r.query {
            let hit = pairs.iter().find(|(n, _)| n == name)?;
            if let Some(cap) = value.strip_prefix(':') {
                set_capture(&mut caps, cap, hit.1.clone());
            } else if hit.1 != *value {
                return None;
            }
        }
    }
    for c in &r.conditions {
        let ok = match c.key {
            "language" => language_matches(&c.values, req.accept_language.unwrap_or("")),
            "country" => req
                .country
                .is_some_and(|k| c.values.contains(&k.to_ascii_lowercase())),
            _ => cookie_matches(&c.values, req.cookie.unwrap_or("")),
        };
        if !ok {
            return None;
        }
    }
    Some(caps)
}

/// A rule's target with `:name` (letters, digits and `_`, the longest run) replaced by the captured
/// value, percent-encoded like encodeURIComponent; `:splat` keeps its slashes. Other names stay.
pub fn substitute_target(to: &str, caps: &[(String, String)]) -> String {
    let b = to.as_bytes();
    let mut out = String::with_capacity(to.len());
    let mut i = 0;
    while i < b.len() {
        let start_ok = b
            .get(i + 1)
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_');
        if b[i] != b':' || !start_ok {
            let ch = to[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        let name = &to[i + 1..j];
        match caps.iter().find(|(n, _)| n == name) {
            Some((_, v)) if name == "splat" => {
                let enc: Vec<String> = v.split('/').map(encode_uri_component).collect();
                out.push_str(&enc.join("/"));
            }
            Some((_, v)) => out.push_str(&encode_uri_component(v)),
            None => out.push_str(&to[i..j]),
        }
        i = j;
    }
    out
}

/// The rules a tree's `/_redirects` and `/_headers` define (parsed once per server).
#[derive(Debug, Clone, Default)]
pub struct SiteRules {
    pub redirects: Vec<ServeRedirect>,
    pub headers: Vec<ServeHeaders>,
}

impl SiteRules {
    pub fn from_tree(tree: &Tree) -> SiteRules {
        SiteRules::from_tree_with(tree, None)
    }
    /// The rules, reading `/_redirects` and `/_headers` through `content` when given.
    pub fn from_tree_with(tree: &Tree, content: Option<&dyn SiteContent>) -> SiteRules {
        let text = |p: &str| {
            tree.get(p)
                .map(|f| match content.and_then(|c| c.read(p)) {
                    Some(Ok(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Some(Err(_)) => String::new(),
                    None => String::from_utf8_lossy(&f.bytes).into_owned(),
                })
                .unwrap_or_default()
        };
        SiteRules {
            redirects: parse_redirect_rules(&text("/_redirects")),
            headers: parse_header_rules(&text("/_headers")),
        }
    }
}

/// The parsed rules of the tree a server last served (a watched server swaps trees).
type RulesCache = Arc<Mutex<Option<(Arc<Tree>, Arc<SiteRules>)>>>;

fn rules_for(
    cache: &RulesCache,
    tree: &Arc<Tree>,
    content: Option<&dyn SiteContent>,
) -> Arc<SiteRules> {
    let mut slot = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((t, r)) = slot.as_ref() {
        if Arc::ptr_eq(t, tree) {
            return r.clone();
        }
    }
    let r = Arc::new(SiteRules::from_tree_with(tree, content));
    *slot = Some((tree.clone(), r.clone()));
    r
}

/// Answer one request (no I/O). `headers` are lowercase names; repeated ones joined with `, `.
pub fn handle_request(
    tree: &Tree,
    options: &SiteServerOptions,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
) -> Response {
    respond(
        tree,
        &SiteRules::from_tree_with(tree, options.content.as_deref()),
        options,
        method,
        url,
        headers,
    )
}

/// [`handle_request`] with the tree's rules already parsed.
pub fn respond(
    tree: &Tree,
    rules: &SiteRules,
    options: &SiteServerOptions,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
) -> Response {
    let base = base_headers(options);
    let finish = |status: u16, extra: Vec<(String, String)>, body: Option<Vec<u8>>| -> Response {
        let mut headers = base.clone();
        for (k, v) in extra {
            headers.retain(|(h, _)| *h != k);
            headers.push((k, v));
        }
        let body = if method != "HEAD" && status != 304 {
            body.unwrap_or_default()
        } else {
            Vec::new()
        };
        Response {
            status,
            headers,
            body,
            note: None,
        }
    };
    let owned = |v: Vec<(&str, String)>| -> Vec<(String, String)> {
        v.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    };
    let page = |status: u16, html: &str, extra: Vec<(String, String)>| -> Response {
        let mut h = owned(vec![
            ("content-type", "text/html; charset=utf-8".to_string()),
            ("content-length", html.len().to_string()),
        ]);
        h.extend(extra);
        finish(status, h, Some(html.as_bytes().to_vec()))
    };

    // DNS rebinding: a page on another name must not read this server (before anything else).
    if !host_allowed(
        options.allowed_hosts.as_deref(),
        headers.get("host").map(String::as_str),
    ) {
        return page(403, FORBIDDEN_HOST_PAGE, vec![]);
    }
    // Basic authentication covers everything, the live-reload endpoints too.
    if let Some(a) = &options.auth {
        if !a.allows(first_value(headers.get("authorization"))) {
            return page(
                401,
                UNAUTHORIZED_PAGE,
                vec![(
                    "www-authenticate".into(),
                    "Basic realm=\"sitefile\", charset=\"UTF-8\"".into(),
                )],
            );
        }
    }
    if options.cors && method == "OPTIONS" {
        // A CORS preflight: any path, nothing is looked up.
        let mut h = owned(vec![("access-control-allow-methods", "GET, HEAD".into())]);
        if let Some(v) = preflight_headers(
            headers
                .get("access-control-request-headers")
                .map(String::as_str),
        ) {
            h.push(("access-control-allow-headers".into(), v));
        }
        h.push(("access-control-max-age".into(), "600".into()));
        return finish(204, h, None);
    }
    if method != "GET" && method != "HEAD" {
        let allow = if options.cors {
            "GET, HEAD, OPTIONS"
        } else {
            "GET, HEAD"
        };
        return page(405, METHOD_PAGE, owned(vec![("allow", allow.into())]));
    }
    let url = origin_form(url);
    let url = url.as_ref();
    let q = url.find('?');
    let hash = url.find('#');
    let end = q.unwrap_or(url.len()).min(hash.unwrap_or(url.len()));
    let raw_path = &url[..end];
    let search = match (q, hash) {
        (None, _) => "",
        (Some(q), Some(h)) if h > q => &url[q..h],
        (Some(q), _) => &url[q..],
    };
    if options.live_reload.is_some() && raw_path == RELOAD_PATH {
        // GET is streamed by the connection loop; HEAD only gets the headers.
        return finish(
            200,
            owned(
                STREAM_HEADERS
                    .iter()
                    .map(|(k, v)| (*k, v.to_string()))
                    .collect(),
            ),
            None,
        );
    }
    if let Some(lr) = options
        .live_reload
        .as_ref()
        .filter(|_| raw_path == RELOAD_SCRIPT_PATH)
    {
        let js = lr.script();
        return finish(
            200,
            owned(vec![
                ("content-type", "text/javascript; charset=utf-8".into()),
                ("content-length", js.len().to_string()),
            ]),
            Some(js.as_bytes().to_vec()),
        );
    }
    let Some(all_segs) = safe_segments(raw_path) else {
        return page(400, BAD_REQUEST_PAGE, vec![]);
    };
    let trailing = raw_path.ends_with('/');
    // `--base`: the site lives under `mount`; requests outside it are 404 (or redirected into it).
    let base_now = match &options.base {
        Some(b) => b(),
        None => None,
    };
    let base_segs = base_now
        .as_deref()
        .and_then(safe_segments)
        .unwrap_or_default();
    // The `--base` prefix of generated `Location`s (`""` without one), percent-encoded.
    let mount = if base_segs.is_empty() {
        String::new()
    } else {
        let enc: Vec<String> = base_segs.iter().map(|s| encode_uri_component(s)).collect();
        format!("/{}", enc.join("/"))
    };
    let mut segs = all_segs.clone();
    if !base_segs.is_empty() {
        let inside = all_segs.len() >= base_segs.len()
            && base_segs.iter().zip(&all_segs).all(|(b, a)| a == b);
        let to_mount = |status: u16| {
            finish(
                status,
                owned(vec![
                    ("location", format!("{mount}/{search}")),
                    ("content-length", "0".into()),
                ]),
                None,
            )
        };
        if inside && all_segs.len() == base_segs.len() && !trailing {
            return to_mount(301);
        }
        if !inside && all_segs.is_empty() && !options.hide_base {
            return to_mount(302);
        }
        if !inside {
            return page(404, NOT_FOUND_PAGE, vec![]);
        }
        segs = all_segs[base_segs.len()..].to_vec();
    }
    let vpath = format!("/{}", segs.join("/"));
    // `_headers` patterns match the request path (with its trailing slash).
    let custom = headers_for(
        &rules.headers,
        &format!(
            "{vpath}{}",
            if trailing && !segs.is_empty() {
                "/"
            } else {
                ""
            }
        ),
    );
    let is_file = |p: &str| tree.is_file(p) && p != "/_redirects" && p != "/_headers";

    let send_file = |status: u16, served: &str| -> Response {
        let file = tree.get(served).cloned().expect("resolved file");
        let mime = mime_type_for(served);
        let live = options.live_reload.is_some()
            && mime.starts_with("text/html")
            && is_navigation(headers);
        if let Some(lr) = options.live_reload.as_ref().filter(|_| live) {
            let csp = custom
                .iter()
                .find(|(k, _)| k == "content-security-policy")
                .map(|(_, v)| v.as_str());
            lr.warn_csp(options.warn.as_ref(), served, csp);
        }
        // A deflated zip entry goes to a gzip client as is, wrapped in a gzip header and trailer (not for a
        // range request: ranges are served from the identity body).
        let deflated = if !live
            && !headers.contains_key("range")
            && accepts_gzip(headers.get("accept-encoding").map(String::as_str))
        {
            options.content.as_ref().and_then(|c| c.deflated(served))
        } else {
            None
        };
        let read = match &deflated {
            Some(d) => Ok(gzip_member(&d.deflate, d.crc, d.size)),
            None => match options.content.as_ref().and_then(|c| c.read(served)) {
                Some(r) => r,
                None => Ok(file.bytes.to_vec()),
            },
        };
        let body = match read {
            Ok(b) => b,
            Err(e) => {
                if let Some(log) = &options.log {
                    log(&format!("error: {}", e.message()));
                }
                return page(500, ERROR_PAGE, vec![]);
            }
        };
        let body = if live {
            inject_reload_script(&body)
        } else {
            body
        };
        let encoding: Vec<(String, String)> = if deflated.is_some() {
            owned(vec![
                ("content-encoding", "gzip".into()),
                ("vary", "accept-encoding".into()),
            ])
        } else {
            vec![]
        };
        let cc: Vec<(String, String)> = if status == 200 {
            cache_control_for(mime, options.cache.as_ref())
                .map(|v| vec![("cache-control".to_string(), v)])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut h = owned(vec![
            ("content-type", mime.into()),
            ("content-length", body.len().to_string()),
        ]);
        h.extend(cc.iter().cloned());
        h.extend(encoding.iter().cloned());
        h.extend(custom.iter().cloned());
        if status != 200 {
            return finish(status, h, Some(body));
        }
        // An injected page and a gzip response are other representations of the file: they get their own ETags.
        let etag = format!(
            "\"{}{}{}\"",
            file.digest.strip_prefix("sha256:").unwrap_or(&file.digest),
            if live { "-live" } else { "" },
            if deflated.is_some() { "-gzip" } else { "" }
        );
        let modified = options.last_modified.as_ref().and_then(|f| f());
        let lm: Vec<(String, String)> = modified
            .map(|m| vec![("last-modified".to_string(), http_date(m))])
            .unwrap_or_default();
        let not_modified = || {
            let mut h: Vec<(String, String)> = encoding.clone();
            h.extend(cc.iter().cloned());
            h.extend(custom.iter().cloned());
            h.push(("etag".into(), etag.clone()));
            h.extend(lm.iter().cloned());
            finish(304, h, None)
        };
        if etag_matches(headers.get("if-none-match").map(String::as_str), &etag) {
            return not_modified();
        }
        // If-Modified-Since only counts without If-None-Match (RFC 9110 section 13.2.2).
        let since = if headers.contains_key("if-none-match") {
            None
        } else {
            headers
                .get("if-modified-since")
                .and_then(|v| parse_http_date(v))
        };
        if let (Some(m), Some(since)) = (modified, since) {
            if m <= since {
                return not_modified();
            }
        }
        h.push(("etag".into(), etag.clone()));
        h.extend(lm.iter().cloned());
        // A single byte range of a GET (`If-Range` must name this ETag); anything else gets the whole file.
        let if_range_ok = headers
            .get("if-range")
            .is_none_or(|v| v.trim() == etag.as_str());
        let range = if method == "GET" && if_range_ok {
            byte_range(headers.get("range").map(String::as_str), body.len())
        } else {
            None
        };
        h.push(("accept-ranges".into(), "bytes".into()));
        match range {
            Some(ByteRange::Unsatisfiable) => {
                let mut h: Vec<(String, String)> = custom.clone();
                h.push(("accept-ranges".into(), "bytes".into()));
                h.push(("content-range".into(), format!("bytes */{}", body.len())));
                h.push(("content-length".into(), "0".into()));
                finish(416, h, None)
            }
            Some(ByteRange::Bytes(start, end)) => {
                let part = body[start..=end].to_vec();
                h[1] = ("content-length".into(), part.len().to_string());
                h.push((
                    "content-range".into(),
                    format!("bytes {start}-{end}/{}", body.len()),
                ));
                finish(206, h, Some(part))
            }
            None => finish(200, h, Some(body)),
        }
    };
    // The file for a path: exact, `<path>.html` (clean URLs), or `<dir>/index.html` with a trailing slash.
    let resolve = |p: &str, slash: bool| -> Option<String> {
        if is_file(p) && !(slash && p != "/") {
            return Some(p.to_string());
        }
        if !slash && p != "/" && is_file(&format!("{p}.html")) {
            return Some(format!("{p}.html"));
        }
        if slash && tree.is_directory(p) {
            let index = if p == "/" {
                "/index.html".to_string()
            } else {
                format!("{p}/index.html")
            };
            if is_file(&index) {
                return Some(index);
            }
        }
        None
    };
    let rule_request = RedirectRequest {
        search,
        accept_language: headers.get("accept-language").map(String::as_str),
        cookie: headers.get("cookie").map(String::as_str),
        country: options.country.as_deref(),
    };
    // Apply the first matching `_redirects` rule (forced ones, or the others).
    let redirect = |forced: bool| -> Option<Response> {
        for r in rules.redirects.iter().filter(|r| r.force == forced) {
            let Some(caps) = match_rule(r, &vpath, &rule_request) else {
                continue;
            };
            let target = substitute_target(&r.to, &caps);
            if !is_rewrite_status(r.status) {
                // A rule that matches query parameters passes on only what its target names.
                let target = if target.starts_with('/') {
                    format!("{mount}{target}")
                } else {
                    target
                };
                let location = if target.contains('?') || !r.query.is_empty() {
                    target
                } else {
                    format!("{target}{search}")
                };
                let note = format!(" \u{2192} {location} (_redirects:{})", r.line);
                let mut res = finish(
                    r.status,
                    owned(vec![("location", location), ("content-length", "0".into())]),
                    None,
                );
                res.note = Some(note);
                return Some(res);
            }
            // 200 (rewrite), 404, 410 and 451: serve the target path's file with that status.
            let tpath = target.split('?').next().unwrap_or("");
            let served = safe_segments(tpath)
                .and_then(|t| resolve(&format!("/{}", t.join("/")), tpath.ends_with('/')));
            if let Some(served) = served {
                let mut res = send_file(r.status, &served);
                res.note = Some(format!(
                    " \u{2192} {} (_redirects:{})",
                    encode_path(&served),
                    r.line
                ));
                return Some(res);
            }
        }
        None
    };

    if let Some(res) = redirect(true) {
        return res;
    }
    let mut served = resolve(&vpath, trailing);
    if served.is_none() && !trailing && tree.is_directory(&vpath) {
        let enc: Vec<String> = segs.iter().map(|s| encode_uri_component(s)).collect();
        let location = format!("{mount}/{}/{search}", enc.join("/"));
        return finish(
            301,
            owned(vec![("location", location), ("content-length", "0".into())]),
            None,
        );
    }
    if served.is_none() {
        if let Some(res) = redirect(false) {
            return res;
        }
    }
    // `/about/` when only `/about.html` answers: redirect to the clean URL `/about` (relative links in
    // the page then resolve as they do at `/about`). `_redirects` rules for the path come first.
    if served.is_none() && trailing && !segs.is_empty() && is_file(&format!("{vpath}.html")) {
        let enc: Vec<String> = segs.iter().map(|s| encode_uri_component(s)).collect();
        let location = format!("{mount}/{}{search}", enc.join("/"));
        return finish(
            301,
            owned(vec![("location", location), ("content-length", "0".into())]),
            None,
        );
    }
    if served.is_none() && options.spa && is_navigation(headers) {
        let req_path = format!(
            "{vpath}{}",
            if trailing && !segs.is_empty() {
                "/"
            } else {
                ""
            }
        );
        if !options.spa_exclude.iter().any(|g| glob_match(g, &req_path)) {
            served = spa_fallback(&segs, trailing, &is_file, options.spa_fallback.as_deref());
        }
    }
    if let Some(served) = served {
        return send_file(200, &served);
    }
    // The nearest 404.html: in the request's directory, then each parent up to the root.
    let dirs = if trailing {
        &segs[..]
    } else {
        &segs[..segs.len().saturating_sub(1)]
    };
    for n in (0..=dirs.len()).rev() {
        let not_found: String = dirs[..n].iter().map(|d| format!("/{d}")).collect();
        let not_found = format!("{not_found}/404.html");
        if is_file(&not_found) {
            return send_file(404, &not_found);
        }
    }
    page(404, NOT_FOUND_PAGE, custom.clone())
}

const MAX_HEAD: usize = 64 * 1024;
const MAX_DISCARD_BODY: u64 = 1 << 20;

struct Request {
    method: String,
    /// HTTP/1.1 or later (`103 Early Hints` is only sent then).
    http11: bool,
    url: String,
    headers: HashMap<String, String>,
    keep_alive: bool,
    body_len: u64,
    chunked: bool,
}

/// Read one request head. `Ok(None)` on a clean EOF before any byte.
fn read_request(r: &mut BufReader<Conn>) -> io::Result<Option<std::result::Result<Request, ()>>> {
    let mut head: Vec<u8> = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r
            .by_ref()
            .take((MAX_HEAD - head.len() + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(if head.is_empty() { None } else { Some(Err(())) });
        }
        if head.is_empty() && (line == b"\r\n" || line == b"\n") {
            continue; // tolerate leading blank lines
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD || !line.ends_with(b"\n") {
            return Ok(Some(Err(())));
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    // latin1, like Node's parser
    let text: String = head.iter().map(|&b| b as char).collect();
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (Some(method), Some(url), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(Some(Err(())));
    };
    if method.is_empty()
        || !method.bytes().all(|c| c.is_ascii_uppercase() || c == b'-')
        || url.is_empty()
        || !version.starts_with("HTTP/1.")
    {
        return Ok(Some(Err(())));
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let Some((k, v)) = l.split_once(':') else {
            return Ok(Some(Err(())));
        };
        let k = k.trim().to_ascii_lowercase();
        if k.is_empty() || k.contains(' ') {
            return Ok(Some(Err(())));
        }
        let v = v.trim().to_string();
        headers
            .entry(k)
            .and_modify(|e| *e = format!("{e}, {v}"))
            .or_insert(v);
    }
    let conn = headers
        .get("connection")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let keep_alive = if version == "HTTP/1.0" {
        conn.contains("keep-alive")
    } else {
        !conn.contains("close")
    };
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let body_len = match headers.get("content-length") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return Ok(Some(Err(()))),
        },
        None => 0,
    };
    Ok(Some(Ok(Request {
        method: method.into(),
        http11: version != "HTTP/1.0",
        url: url.into(),
        headers,
        keep_alive,
        body_len,
        chunked,
    })))
}

fn serve_connection(
    stream: Conn,
    tree: &SharedTree,
    rules: &RulesCache,
    options: &SiteServerOptions,
    busy: bool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let peer = stream.peer_addr().ok().map(|a| a.ip().to_string());
    let mut reader = BufReader::new(stream);
    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(Ok(r))) => r,
            Ok(Some(Err(()))) => {
                let _ = out.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                );
                break;
            }
            _ => break,
        };
        let started = Instant::now();
        let mut keep = !busy && req.keep_alive && !req.chunked && req.body_len <= MAX_DISCARD_BODY;
        let ip_ok = ip_allowed(options.ip_rules.as_ref(), peer.as_deref().unwrap_or(""));
        let host_ok = ip_ok
            && !busy
            && host_allowed(
                options.allowed_hosts.as_deref(),
                req.headers.get("host").map(String::as_str),
            )
            && options
                .auth
                .as_ref()
                .is_none_or(|a| a.allows(first_value(req.headers.get("authorization"))));
        let console = options
            .live_reload
            .as_ref()
            .filter(|_| host_ok && raw_path_of(&req.url) == CONSOLE_PATH);
        let mut body: Option<Vec<u8>> = None;
        if console.is_some()
            && req.method == "POST"
            && is_same_origin_post(&req.headers)
            && !req.chunked
            && req.body_len <= MAX_CONSOLE_BODY
        {
            let mut buf = vec![0u8; req.body_len as usize];
            if reader.read_exact(&mut buf).is_err() {
                break;
            }
            body = Some(buf);
        } else if req.body_len > 0
            && keep
            && io::copy(&mut reader.by_ref().take(req.body_len), &mut io::sink()).is_err()
        {
            keep = false;
        }
        if console.is_some() && (req.chunked || req.body_len > MAX_CONSOLE_BODY) {
            keep = false;
        }
        if let Some(live) = options.live_reload.as_ref().filter(|_| {
            host_ok && req.method == "GET" && raw_path_of(&origin_form(&req.url)) == RELOAD_PATH
        }) {
            live.stream(&mut out, options); // not access-logged: one long request per open page
            break;
        }
        let current = match tree.read() {
            Ok(t) => t.clone(),
            Err(_) => break,
        };
        let site_rules = rules_for(rules, &current, options.content.as_deref());
        let res = match console {
            // --allow-ip / --deny-ip refuse this client (checked first).
            _ if !ip_ok => {
                let mut headers = base_headers(options);
                headers.push(("content-type".into(), "text/html; charset=utf-8".into()));
                headers.push(("content-length".into(), FORBIDDEN_PAGE.len().to_string()));
                Response {
                    status: 403,
                    headers,
                    body: if req.method == "HEAD" {
                        Vec::new()
                    } else {
                        FORBIDDEN_PAGE.as_bytes().to_vec()
                    },
                    note: None,
                }
            }
            // Over --max-connections-per-ip: one 503, then the connection closes.
            _ if busy => {
                let mut headers = base_headers(options);
                headers.push(("retry-after".into(), "1".into()));
                headers.push(("content-length".into(), "0".into()));
                Response {
                    status: 503,
                    headers,
                    body: Vec::new(),
                    note: None,
                }
            }
            Some(live) => live.console_response(
                options,
                &req.method,
                &req.headers,
                body.as_deref(),
                req.body_len,
                req.chunked,
            ),
            None => respond(
                &current,
                &site_rules,
                options,
                &req.method,
                &req.url,
                &req.headers,
            ),
        };
        // 103 Early Hints before an HTML page whose `Link` (from `_headers`) preloads or preconnects.
        let hints = match res.header("link") {
            Some(l)
                if res.status == 200
                    && req.method == "GET"
                    && req.http11
                    && res
                        .header("content-type")
                        .is_some_and(|t| t.starts_with("text/html")) =>
            {
                early_hints(l)
            }
            _ => Vec::new(),
        };
        if !hints.is_empty() {
            let _ = out.write_all(
                format!(
                    "HTTP/1.1 103 Early Hints\r\nlink: {}\r\n\r\n",
                    hints.join(", ")
                )
                .as_bytes(),
            );
        }
        let mut head = format!("HTTP/1.1 {} {}\r\n", res.status, reason(res.status));
        for (k, v) in &res.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(if keep {
            "connection: keep-alive\r\n\r\n"
        } else {
            "connection: close\r\n\r\n"
        });
        let ok = out
            .write_all(head.as_bytes())
            .and_then(|_| out.write_all(&res.body))
            .and_then(|_| out.flush())
            .is_ok();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        // `servers` probes carry the server's own id; they would only clutter the access log.
        let probe = options.server_id.is_some()
            && req.headers.get("x-sitefile-probe") == options.server_id.as_ref();
        // every page load fetches the reload script; it would only clutter the access log
        // nor do console messages (they are logged as `browser ...` lines)
        let quiet = options.live_reload.is_some()
            && [RELOAD_SCRIPT_PATH, CONSOLE_PATH].contains(&raw_path_of(&req.url));
        if !probe && !quiet && !options.quiet {
            if let Some(log) = &options.log {
                let shared = options.client_info && !peer.as_deref().is_some_and(is_loopback_text);
                let remote = if options.log_ip || shared {
                    peer.clone()
                } else {
                    None
                };
                log(&access_log_line(
                    options.log_json,
                    &AccessLogEntry {
                        ts: &crate::store::now_iso(),
                        method: &req.method,
                        target: &req.url,
                        status: res.status,
                        bytes: res.body.len(),
                        ms,
                        range: res.header("content-range"),
                        note: res.note.as_deref(),
                        remote: remote.as_deref(),
                        device: shared
                            .then(|| device_of(req.headers.get("user-agent").map(String::as_str))),
                    },
                ));
            }
        }
        if !ok || !keep {
            break;
        }
    }
    let _ = out.shutdown(Shutdown::Both);
}

/// A listening server; [`ListeningServer::close`] stops it.
pub struct ListeningServer {
    pub host: String,
    pub port: u16,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    accept: Option<JoinHandle<()>>,
}

/// Listen on host:port (port 0 = any free port) and serve `tree`.
/// Address in use and permission errors become E_SERVER.
pub fn listen(
    tree: Arc<Tree>,
    options: SiteServerOptions,
    host: &str,
    port: u16,
) -> Result<ListeningServer> {
    listen_shared(Arc::new(RwLock::new(tree)), options, host, port)
}

/// [`listen`] on a tree that can be replaced while the server runs (each request reads it once).
pub fn listen_shared(
    tree: SharedTree,
    options: SiteServerOptions,
    host: &str,
    port: u16,
) -> Result<ListeningServer> {
    let fail = |e: &io::Error| {
        let why = match e.kind() {
            io::ErrorKind::AddrInUse => format!("port {port} on {host} is already in use"),
            io::ErrorKind::PermissionDenied => format!("permission denied binding {host}:{port}"),
            _ => format!("cannot listen on {host}:{port}: {e}"),
        };
        SitefileError::plain(E_SERVER, why)
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare, port)
        .to_socket_addrs()
        .map_err(|e| fail(&e))?
        .collect();
    let mut last = io::Error::new(io::ErrorKind::AddrNotAvailable, "no address");
    let mut bound = None;
    for a in addrs {
        match TcpListener::bind(a) {
            Ok(l) => {
                bound = Some(l);
                break;
            }
            Err(e) => last = e,
        }
    }
    let listener = bound.ok_or_else(|| fail(&last))?;
    let addr = listener.local_addr().map_err(|e| fail(&e))?;
    let rules: RulesCache = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let conns: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
    let (s2, c2) = (stop.clone(), conns.clone());
    let per_ip: Arc<Mutex<HashMap<std::net::IpAddr, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let accept = std::thread::spawn(move || {
        let next = AtomicU64::new(0);
        for stream in listener.incoming() {
            if s2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let id = next.fetch_add(1, Ordering::Relaxed);
            if let Ok(c) = stream.try_clone() {
                if let Ok(mut m) = c2.lock() {
                    m.insert(id, c);
                }
            }
            // --max-connections-per-ip: count this client's open connections.
            let ip = stream.peer_addr().ok().map(|a| a.ip());
            let limited = ip.filter(|ip| {
                options.max_connections_per_ip > 0
                    && (options.limit_loopback || !is_loopback_ip(ip))
            });
            let busy = match limited {
                Some(ip) => {
                    let mut m = per_ip.lock().unwrap_or_else(|p| p.into_inner());
                    let n = m.entry(ip).or_insert(0);
                    *n += 1;
                    *n > options.max_connections_per_ip
                }
                None => false,
            };
            let (tree, rules, options, conns, per_ip) = (
                tree.clone(),
                rules.clone(),
                options.clone(),
                c2.clone(),
                per_ip.clone(),
            );
            std::thread::spawn(move || {
                let conn = Conn {
                    tls: options.tls.as_ref().and_then(|cfg| {
                        rustls::ServerConnection::new(cfg.clone())
                            .ok()
                            .map(|c| Arc::new(Mutex::new(c)))
                    }),
                    tcp: stream,
                };
                if options.tls.is_some() && conn.tls.is_none() {
                    return;
                }
                serve_connection(conn, &tree, &rules, &options, busy);
                if let Ok(mut m) = conns.lock() {
                    m.remove(&id);
                }
                if let Some(ip) = limited {
                    let mut m = per_ip.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(n) = m.get_mut(&ip) {
                        *n -= 1;
                        if *n == 0 {
                            m.remove(&ip);
                        }
                    }
                }
            });
        }
    });
    Ok(ListeningServer {
        host: host.to_string(),
        port: addr.port(),
        addr,
        stop,
        conns,
        accept: Some(accept),
    })
}

impl ListeningServer {
    pub fn url(&self) -> String {
        format_url(&self.host, self.port)
    }

    /// Block until the server is closed (foreground `serve`).
    pub fn wait(mut self) {
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }

    /// Stop accepting, close every open connection and wait for the accept loop.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut wake = self.addr;
        if wake.ip().is_unspecified() {
            wake.set_ip(if wake.is_ipv6() {
                "::1".parse().unwrap_or(wake.ip())
            } else {
                [127, 0, 0, 1].into()
            });
        }
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        if let Ok(mut m) = self.conns.lock() {
            for (_, c) in m.drain() {
                let _ = c.shutdown(Shutdown::Both);
            }
        }
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }

    /// A handle that can close the server from another thread (signal handlers, tests).
    pub fn stopper(&self) -> impl Fn() + Send + Sync + 'static {
        let (stop, conns, addr) = (self.stop.clone(), self.conns.clone(), self.addr);
        move || {
            if stop.swap(true, Ordering::SeqCst) {
                return;
            }
            let mut wake = addr;
            if wake.ip().is_unspecified() {
                wake.set_ip(if wake.is_ipv6() {
                    "::1".parse().unwrap_or(wake.ip())
                } else {
                    [127, 0, 0, 1].into()
                });
            }
            let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
            if let Ok(mut m) = conns.lock() {
                for (_, c) in m.drain() {
                    let _ = c.shutdown(Shutdown::Both);
                }
            }
        }
    }
}

impl Drop for ListeningServer {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.shutdown();
        }
    }
}

#[repr(C)]
struct IfAddrs {
    ifa_next: *mut IfAddrs,
    ifa_name: *mut std::ffi::c_char,
    ifa_flags: std::ffi::c_uint,
    ifa_addr: *mut u8,
}

extern "C" {
    fn getifaddrs(ifap: *mut *mut IfAddrs) -> std::ffi::c_int;
    fn freeifaddrs(ifa: *mut IfAddrs);
}

const IFF_UP: std::ffi::c_uint = 0x1;
const IFF_LOOPBACK: std::ffi::c_uint = 0x8;
const IFF_RUNNING: std::ffi::c_uint = 0x40;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const AF_INET6: u16 = if cfg!(target_os = "freebsd") { 28 } else { 30 };
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
const AF_INET6: u16 = 10;

/// The family of a `struct sockaddr` (BSDs: a length byte, then the family byte).
///
/// # Safety
/// `sa` points to a readable `struct sockaddr`.
unsafe fn sa_family(sa: *const u8) -> u16 {
    if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )) {
        u16::from(*sa.add(1))
    } else {
        u16::from_ne_bytes([*sa, *sa.add(1)])
    }
}

/// The addresses other machines can reach a server bound to all interfaces at
/// (`networkAddresses` in src/server.ts: up, non-loopback interfaces; IPv4 for `0.0.0.0`, IPv4
/// then IPv6 for `::`, no IPv6 link-local; each family sorted as text, without duplicates).
pub fn network_addresses(bind_host: &str) -> Vec<String> {
    if bind_host != "0.0.0.0" && bind_host != "::" {
        return Vec::new();
    }
    let mut v4: Vec<String> = Vec::new();
    let mut v6: Vec<String> = Vec::new();
    let mut head: *mut IfAddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list that freeifaddrs releases; every node and
    // address is read only while the list is alive, and only through the fields of the common
    // prefix of `struct ifaddrs` (identical on Linux and the BSDs).
    unsafe {
        if getifaddrs(&mut head) != 0 {
            return Vec::new();
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            let up = ifa.ifa_flags & (IFF_UP | IFF_RUNNING) == (IFF_UP | IFF_RUNNING);
            if up && ifa.ifa_flags & IFF_LOOPBACK == 0 && !ifa.ifa_addr.is_null() {
                let sa = ifa.ifa_addr as *const u8;
                let family = sa_family(sa);
                if family == 2 {
                    let b = std::slice::from_raw_parts(sa.add(4), 4);
                    v4.push(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string());
                } else if family == AF_INET6 && bind_host == "::" {
                    let mut b = [0u8; 16];
                    b.copy_from_slice(std::slice::from_raw_parts(sa.add(8), 16));
                    let a = std::net::Ipv6Addr::from(b);
                    if a.segments()[0] & 0xffc0 != 0xfe80 && !a.is_loopback() {
                        v6.push(a.to_string());
                    }
                }
            }
            cur = ifa.ifa_next;
        }
        freeifaddrs(head);
    }
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    v4.extend(v6);
    v4
}

/// [`network_addresses`] as URLs.
pub fn network_urls(bind_host: &str, port: u16) -> Vec<String> {
    network_addresses(bind_host)
        .iter()
        .map(|a| format_url(a, port))
        .collect()
}

/// `http://` URL made `https://` for a TLS server.
pub fn with_scheme(url: String, tls: bool) -> String {
    match url.strip_prefix("http://").filter(|_| tls) {
        Some(rest) => format!("https://{rest}"),
        None => url,
    }
}

pub fn format_url(host: &str, port: u16) -> String {
    let shown = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    if shown.contains(':') && !shown.starts_with('[') {
        format!("http://[{shown}]:{port}/")
    } else {
        format!("http://{shown}:{port}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::make_file;

    fn site() -> Arc<Tree> {
        let files = [
            ("/index.html", "<h1>home</h1>"),
            ("/about/index.html", "<h1>about</h1>"),
            ("/app.js", "console.log(1)"),
            ("/style.css", "a{}"),
            ("/data.json", "{}"),
            ("/logo.png", "PNG"),
            ("/blob.xyz", "bin"),
            ("/page.html", "page"),
        ];
        Arc::new(
            Tree::from_files(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
            )
            .unwrap(),
        )
    }

    struct Res {
        status: u16,
        headers: HashMap<String, String>,
        body: String,
    }

    /// Raw HTTP/1.1 client: the path is sent verbatim, so traversal attempts are not normalized away.
    fn req(port: u16, path: &str, method: &str, extra: &[(&str, &str)]) -> Res {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut r = format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n");
        for (k, v) in extra {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("\r\n");
        s.write_all(r.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let mut lines = head.split("\r\n");
        let status: u16 = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Res {
            status,
            headers,
            body: body.to_string(),
        }
    }
    fn get(port: u16, path: &str) -> Res {
        req(port, path, "GET", &[])
    }

    fn with_server(spa: bool, f: impl FnOnce(u16, &Arc<Mutex<Vec<String>>>)) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let opts = SiteServerOptions {
            spa,
            log: Some(Arc::new(move |line: &str| {
                l2.lock().unwrap().push(line.to_string())
            })),
            server_id: None,
            live_reload: None,
            last_modified: None,
            warn: None,
            ..Default::default()
        };
        let s = listen(site(), opts, "127.0.0.1", 0).unwrap();
        f(s.port, &log);
        s.close();
    }

    #[test]
    fn serves_indexes_and_files_with_headers() {
        with_server(false, |port, _| {
            let r = get(port, "/");
            assert_eq!(r.status, 200);
            assert_eq!(r.body, "<h1>home</h1>");
            assert_eq!(r.headers["content-type"], "text/html; charset=utf-8");
            assert_eq!(r.headers["cache-control"], "no-cache");
            assert_eq!(r.headers["x-content-type-options"], "nosniff");
            let e = &r.headers["etag"];
            assert!(e.len() == 66 && e.starts_with('"') && e.ends_with('"'));
            assert_eq!(get(port, "/about/").body, "<h1>about</h1>");
            assert_eq!(get(port, "/?x=1#frag").status, 200);
            assert_eq!(get(port, "/index.html").body, "<h1>home</h1>");
        });
    }

    #[test]
    fn directory_redirects_keep_query_no_open_redirect() {
        with_server(false, |port, _| {
            let r = get(port, "/about?x=1");
            assert_eq!(r.status, 301);
            assert_eq!(r.headers["location"], "/about/?x=1");
            let evil = get(port, "//about");
            assert_eq!(evil.status, 301);
            assert_eq!(evil.headers["location"], "/about/");
            assert_eq!(get(port, "/about/index.html/").status, 404);
        });
    }

    #[test]
    fn not_found_and_no_listings() {
        with_server(false, |port, _| {
            let r = get(port, "/missing");
            assert_eq!(r.status, 404);
            assert!(r.body.contains("404 Not Found"));
            assert_eq!(r.headers["content-type"], "text/html; charset=utf-8");
        });
        let t = Arc::new(
            Tree::from_files([(
                "/d/file.txt".to_string(),
                make_file(b"x".to_vec(), "t", 0o644),
            )])
            .unwrap(),
        );
        let s = listen(t, SiteServerOptions::default(), "127.0.0.1", 0).unwrap();
        assert_eq!(get(s.port, "/d/").status, 404);
        assert_eq!(get(s.port, "/").status, 404);
        s.close();
    }

    #[test]
    fn spa_fallback_by_navigation() {
        let doc = [
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
        ];
        with_server(true, |port, _| {
            let r = req(port, "/some/client/route", "GET", &doc);
            assert_eq!((r.status, r.body.as_str()), (200, "<h1>home</h1>"));
            assert_eq!(
                req(port, "/users/jane.doe", "GET", &doc).body,
                "<h1>home</h1>"
            );
            assert_eq!(
                req(
                    port,
                    "/users/jane.doe",
                    "GET",
                    &[("accept", "text/html,*/*")]
                )
                .status,
                200
            );
            let script = [
                ("sec-fetch-mode", "no-cors"),
                ("sec-fetch-dest", "script"),
                ("accept", "*/*"),
            ];
            assert_eq!(req(port, "/assets/app.js", "GET", &script).status, 404);
            assert_eq!(get(port, "/some/route").status, 404);
            assert_eq!(req(port, "/about/team", "GET", &doc).body, "<h1>about</h1>");
            assert_eq!(get(port, "/app.js").body, "console.log(1)");
        });
        // (johnhenry/sitefile#194) 200.html, nested app roots, exclusions and --spa-fallback.
        let t = Tree::from_files(
            [
                ("/index.html", "root"),
                ("/200.html", "surge shell"),
                ("/admin/index.html", "admin"),
                ("/api/x.json", "{}"),
                ("/app.html", "app"),
            ]
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap();
        let h: HashMap<String, String> = doc
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let opts = SiteServerOptions {
            spa: true,
            spa_exclude: vec!["/api/*".into(), "/admin/raw".into()],
            ..Default::default()
        };
        let nav = |o: &SiteServerOptions, u: &str| handle_request(&t, o, "GET", u, &h);
        assert_eq!(body(&nav(&opts, "/route")), "surge shell");
        assert_eq!(body(&nav(&opts, "/admin/users/5")), "admin");
        assert_eq!(nav(&opts, "/api/missing").status, 404);
        assert_eq!(nav(&opts, "/admin/raw").status, 404);
        let one = SiteServerOptions {
            spa: true,
            spa_fallback: Some("/app.html".into()),
            ..Default::default()
        };
        assert_eq!(body(&nav(&one, "/admin/users/5")), "app");
    }

    #[test]
    fn head_and_405() {
        with_server(false, |port, _| {
            let g = get(port, "/app.js");
            let h = req(port, "/app.js", "HEAD", &[]);
            assert_eq!(h.status, 200);
            assert_eq!(h.body, "");
            assert_eq!(h.headers["content-length"], g.headers["content-length"]);
            assert_eq!(h.headers["etag"], g.headers["etag"]);
            for m in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
                let r = req(port, "/", m, &[("content-length", "0")]);
                assert_eq!(r.status, 405, "{m}");
                assert_eq!(r.headers["allow"], "GET, HEAD");
            }
        });
    }

    #[test]
    fn etag_304() {
        with_server(false, |port, _| {
            let etag = get(port, "/style.css").headers["etag"].clone();
            for inm in [
                etag.clone(),
                format!("W/{etag}"),
                format!("\"nope\", {etag}"),
                "*".to_string(),
            ] {
                let r = req(port, "/style.css", "GET", &[("if-none-match", &inm)]);
                assert_eq!(r.status, 304, "{inm}");
                assert_eq!(r.body, "");
                assert_eq!(r.headers["etag"], etag);
            }
            assert_eq!(
                req(port, "/style.css", "GET", &[("if-none-match", "\"other\"")]).status,
                200
            );
        });
    }

    #[test]
    fn content_types() {
        with_server(false, |port, _| {
            assert!(get(port, "/app.js").headers["content-type"].starts_with("text/javascript"));
            assert!(get(port, "/style.css").headers["content-type"].starts_with("text/css"));
            assert!(get(port, "/data.json").headers["content-type"].starts_with("application/json"));
            assert_eq!(get(port, "/logo.png").headers["content-type"], "image/png");
            assert_eq!(
                get(port, "/blob.xyz").headers["content-type"],
                "application/octet-stream"
            );
        });
    }

    #[test]
    fn http_dates_and_early_hints() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date(&http_date(951_782_400)), Some(951_782_400));
        for bad in [
            "Mon, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 31 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
        assert_eq!(
            early_hints("</a.css>; rel=preload; as=style, </b.js>; rel=stylesheet, <https://c.example>; REL=\"preconnect\""),
            vec!["</a.css>; rel=preload; as=style", "<https://c.example>; REL=\"preconnect\""]
        );
        assert_eq!(
            early_hints("</a,b.css>; rel=preload"),
            vec!["</a,b.css>; rel=preload"]
        );
        assert!(early_hints("garbage rel=preload").is_empty());
        assert!(early_hints("</a>; title=\"a b\"; rel=preload").is_empty());

        let t = Tree::from_files([("/a.css".to_string(), make_file(b"a{}".to_vec(), "t", 0o644))])
            .unwrap();
        let opts = SiteServerOptions {
            last_modified: Some(Arc::new(|| Some(1_700_000_000))),
            ..Default::default()
        };
        let ask = |extra: &[(&str, &str)]| {
            let h: HashMap<String, String> = extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            handle_request(&t, &opts, "GET", "/a.css", &h)
        };
        assert_eq!(
            ask(&[]).header("last-modified"),
            Some("Tue, 14 Nov 2023 22:13:20 GMT")
        );
        let ims = ("if-modified-since", "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(ask(&[ims]).status, 304);
        assert_eq!(
            ask(&[("if-modified-since", "Tue, 14 Nov 2023 22:13:19 GMT")]).status,
            200
        );
        assert_eq!(ask(&[ims, ("if-none-match", "\"x\"")]).status, 200);
    }

    #[test]
    fn network_addresses_of_wildcard_binds() {
        assert!(network_addresses("127.0.0.1").is_empty());
        assert!(network_addresses("::1").is_empty());
        let v4 = network_addresses("0.0.0.0");
        assert!(v4.iter().all(|a| a
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| !ip.is_loopback())));
        let mut sorted = v4.clone();
        sorted.sort();
        assert_eq!(v4, sorted);
        let all = network_addresses("::");
        assert_eq!(&all[..v4.len()], &v4[..], "IPv4 first");
        assert!(all[v4.len()..]
            .iter()
            .all(|a| a.contains(':') && !a.starts_with("fe8") && a != "::1"));
        assert_eq!(
            network_urls("0.0.0.0", 8080),
            v4.iter()
                .map(|a| format!("http://{a}:8080/"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn max_connections_per_ip() {
        let opts = SiteServerOptions {
            max_connections_per_ip: 2,
            limit_loopback: true,
            ..Default::default()
        };
        let srv = listen(site(), opts, "127.0.0.1", 0).unwrap();
        let port = srv.port;
        let open = || TcpStream::connect(("127.0.0.1", port)).unwrap();
        let ask = |c: &mut TcpStream| {
            c.write_all(b"GET /app.js HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n")
                .unwrap();
            let mut buf = [0u8; 4096];
            let n = c.read(&mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        };
        let mut a = open();
        let b = open();
        let mut c = open();
        std::thread::sleep(Duration::from_millis(50));
        assert!(ask(&mut a).starts_with("HTTP/1.1 200"));
        let busy = ask(&mut c);
        assert!(busy.starts_with("HTTP/1.1 503"), "{busy}");
        assert!(busy.contains("retry-after: 1"));
        drop(b);
        drop(c);
        std::thread::sleep(Duration::from_millis(100));
        let mut d = open();
        assert!(ask(&mut d).starts_with("HTTP/1.1 200"));
        srv.close();
    }

    #[test]
    fn sharing_safety() {
        let opts = SiteServerOptions {
            base: Some(Arc::new(|| Some("/s3cr3t/".to_string()))),
            hide_base: true,
            noindex: true,
            ..Default::default()
        };
        let t = site();
        let h = HashMap::new();
        let get = |u: &str| handle_request(&t, &opts, "GET", u, &h);
        assert_eq!(get("/app.js").status, 404);
        assert_eq!(
            get("/").status,
            404,
            "the secret is not given away by a redirect"
        );
        assert_eq!(get("/s3cr3t/app.js").status, 200);
        let r = get("/s3cr3t?x=1");
        assert_eq!(
            (r.status, r.header("location")),
            (301, Some("/s3cr3t/?x=1"))
        );
        assert_eq!(
            get("/s3cr3t/about").header("location"),
            Some("/s3cr3t/about/")
        );
        assert_eq!(get("/s3cr3tx/app.js").status, 404);
        assert_eq!(r.header("x-robots-tag"), Some("noindex"));
        assert_eq!(ip_bytes("10.1.2.3"), Some(vec![10, 1, 2, 3]));
        assert_eq!(ip_bytes("::ffff:10.1.2.3"), Some(vec![10, 1, 2, 3]));
        let mut one = vec![0u8; 15];
        one.push(1);
        assert_eq!(ip_bytes("::1"), Some(one));
        for bad in ["1.2.3", "::1::2", "1:2:3:4:5:6:7:8:9", "g::1", ""] {
            assert_eq!(ip_bytes(bad), None, "{bad}");
        }
        let rules = IpRules {
            allow: vec![
                parse_cidr("192.168.0.0/16").unwrap(),
                parse_cidr("fd00::/8").unwrap(),
            ],
            deny: vec![parse_cidr("192.168.9.0/24").unwrap()],
        };
        assert!(ip_allowed(Some(&rules), "192.168.1.5"));
        assert!(ip_allowed(Some(&rules), "::ffff:192.168.1.5"));
        assert!(!ip_allowed(Some(&rules), "192.168.9.5"));
        assert!(!ip_allowed(Some(&rules), "10.0.0.1"));
        assert!(ip_allowed(Some(&rules), "fd12::1"));
        assert!(ip_allowed(Some(&rules), "127.0.0.1"));
        for bad in ["10.0.0.0/33", "10/8", "x", "1.2.3.4/a", "::/129"] {
            assert_eq!(parse_cidr(bad), None, "{bad}");
        }
        assert_eq!(
            device_of(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"
            )),
            "iPhone"
        );
        assert_eq!(
            device_of(Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)")),
            "Mac"
        );
        assert_eq!(device_of(None), "other");
    }

    #[test]
    fn tls_config_errors() {
        let e = tls_config(b"not pem", b"").err().unwrap();
        assert!(e.message().contains("--tls-cert"), "{}", e.message());
        let cert = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        assert!(tls_config(cert, b"no key").is_err());
        assert_eq!(
            with_scheme("http://127.0.0.1:1/".into(), true),
            "https://127.0.0.1:1/"
        );
        assert_eq!(with_scheme("http://a/".into(), false), "http://a/");
    }

    #[test]
    fn json_access_log() {
        let e = AccessLogEntry {
            ts: "2026-01-01T00:00:00.000Z",
            method: "GET",
            target: "/a?b",
            status: 206,
            bytes: 2,
            ms: 1.24,
            range: Some("bytes 0-1/10"),
            note: None,
            remote: None,
            device: None,
        };
        assert_eq!(
            access_log_line(true, &e),
            "{\"type\":\"request\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"method\":\"GET\",\"path\":\"/a\",\"query\":\"b\",\"status\":206,\"bytes\":2,\"ms\":1.2,\"encoding\":\"identity\",\"range\":\"bytes 0-1/10\"}"
        );
        let with_ip = AccessLogEntry {
            remote: Some("::1"),
            ..e
        };
        assert!(access_log_line(false, &with_ip).ends_with("ms ::1"));
        assert_eq!(
            log_line_as_json("2026-01-01T00:00:00.000Z GET /a?b 200 5 0.4ms"),
            "{\"type\":\"request\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"method\":\"GET\",\"path\":\"/a\",\"query\":\"b\",\"status\":200,\"bytes\":5,\"ms\":0.4,\"encoding\":\"identity\",\"range\":null}"
        );
        assert_eq!(
            log_line_as_json("2026-01-01T00:00:00.000Z serving ./site (3 files) at http://127.0.0.1:8080/"),
            "{\"type\":\"started\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"site\":\"./site\",\"files\":3,\"url\":\"http://127.0.0.1:8080/\"}"
        );
        assert_eq!(
            log_line_as_json("2026-01-01T00:00:00.000Z received SIGTERM, shutting down"),
            "{\"type\":\"stopped\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"signal\":\"SIGTERM\"}"
        );
        assert_eq!(
            log_line_as_json("2026-01-01T00:00:00.000Z warning: x \"y\""),
            "{\"type\":\"warning\",\"ts\":\"2026-01-01T00:00:00.000Z\",\"message\":\"x \\\"y\\\"\"}"
        );
        assert_eq!(
            log_line_as_json("    at stack"),
            "{\"type\":\"message\",\"ts\":\"\",\"message\":\"    at stack\"}"
        );
    }

    #[test]
    fn cache_preview() {
        let c = |max_age, immutable, max_age_html| CachePreview {
            max_age,
            immutable,
            max_age_html,
        };
        let html = "text/html; charset=utf-8";
        assert_eq!(
            cache_control_for(html, Some(&c(None, false, Some(0)))).as_deref(),
            Some("public, max-age=0")
        );
        assert_eq!(
            cache_control_for("text/css", Some(&c(None, false, Some(5)))),
            None
        );
        assert_eq!(
            cache_control_for("image/png", Some(&c(Some(5), true, None))).as_deref(),
            Some("public, max-age=5, immutable")
        );
        assert_eq!(cache_control_for(html, Some(&c(Some(5), true, None))), None);
        assert_eq!(cache_control_for("image/png", None), None);
        let t = Tree::from_files(
            [("/a.css", "a{}"), ("/index.html", "x")]
                .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap();
        let opts = SiteServerOptions {
            cache: Some(c(Some(600), false, None)),
            ..Default::default()
        };
        let h = HashMap::new();
        let r = handle_request(&t, &opts, "GET", "/a.css", &h);
        assert_eq!(r.header("cache-control"), Some("public, max-age=600"));
        let r = handle_request(&t, &opts, "GET", "/", &h);
        assert_eq!(r.header("cache-control"), Some("no-cache"));
    }

    #[test]
    fn isolate_flag() {
        let t = site();
        let h = HashMap::new();
        for coep in ["require-corp", "credentialless"] {
            let opts = SiteServerOptions {
                isolate: Some(coep.into()),
                ..Default::default()
            };
            for p in ["/", "/missing"] {
                let r = handle_request(&t, &opts, "GET", p, &h);
                assert_eq!(r.header("cross-origin-opener-policy"), Some("same-origin"));
                assert_eq!(r.header("cross-origin-embedder-policy"), Some(coep));
                assert_eq!(
                    r.header("cross-origin-resource-policy"),
                    Some("same-origin")
                );
            }
        }
        let r = handle_request(&t, &SiteServerOptions::default(), "GET", "/", &h);
        assert_eq!(r.header("cross-origin-embedder-policy"), None);
    }

    #[test]
    fn cors_flag() {
        let t = site();
        let opts = SiteServerOptions {
            cors: true,
            ..Default::default()
        };
        let mut h = HashMap::new();
        let r = handle_request(&t, &opts, "GET", "/", &h);
        assert_eq!(r.header("access-control-allow-origin"), Some("*"));
        h.insert(
            "access-control-request-headers".to_string(),
            "x-a, x-b".to_string(),
        );
        let p = handle_request(&t, &opts, "OPTIONS", "/x", &h);
        assert_eq!(p.status, 204);
        assert_eq!(p.header("access-control-allow-headers"), Some("x-a, x-b"));
        assert_eq!(p.header("content-length"), None);
        assert_eq!(
            handle_request(&t, &opts, "POST", "/", &h).header("allow"),
            Some("GET, HEAD, OPTIONS")
        );
        let plain = SiteServerOptions::default();
        assert_eq!(handle_request(&t, &plain, "OPTIONS", "/", &h).status, 405);
        assert_eq!(
            preflight_headers(Some(" x-a,x-b ")).as_deref(),
            Some("x-a,x-b")
        );
        assert_eq!(preflight_headers(Some("a\"b")), None);
    }

    #[test]
    fn host_allow_list() {
        let allowed: Vec<String> = vec!["example.com".into(), ".dev.test".into()];
        let a = Some(allowed.as_slice());
        for h in [
            None,
            Some(""),
            Some("localhost"),
            Some("LocalHost:80"),
            Some("x.localhost"),
            Some("localhost."),
            Some("127.0.0.1:8080"),
            Some("10.0.0.7"),
            Some("[::1]:8080"),
            Some("example.com"),
            Some("EXAMPLE.com.:1"),
            Some("dev.test"),
            Some("a.dev.test"),
            Some("localhost, evil.example"),
        ] {
            assert!(host_allowed(a, h), "{h:?}");
        }
        for h in [
            "evil.example",
            "www.example.com",
            "notdev.test",
            "localhost.evil",
            "1.2.3.4.evil",
            "999.1.1.1",
            "[::1",
            "[fe80::1%25en0]",
            "a:b:80",
        ] {
            assert!(!host_allowed(a, Some(h)), "{h}");
        }
        assert!(host_allowed(None, Some("evil.example")));
        assert!(host_allowed(Some(&["*".to_string()]), Some("evil.example")));
        assert_eq!(
            parse_allowed_host("Example.COM.").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            parse_allowed_host(".dev.test").as_deref(),
            Some(".dev.test")
        );
        assert_eq!(parse_allowed_host("*").as_deref(), Some("*"));
        for bad in ["", ".", "a..b", "a b", "a:80", "http://a", "*.a"] {
            assert_eq!(parse_allowed_host(bad), None, "{bad}");
        }
        let t = site();
        let opts = SiteServerOptions {
            allowed_hosts: Some(allowed.clone()),
            ..Default::default()
        };
        let mut h = HashMap::new();
        h.insert("host".to_string(), "evil.example".to_string());
        let r = handle_request(&t, &opts, "GET", "/", &h);
        assert_eq!(r.status, 403);
        assert!(body(&r).contains("--allowed-host"));
        h.insert("host".to_string(), "www.dev.test".to_string());
        assert_eq!(handle_request(&t, &opts, "GET", "/", &h).status, 200);
    }

    #[test]
    fn byte_ranges() {
        use ByteRange::*;
        assert_eq!(byte_range(Some("bytes=0-"), 5), Some(Bytes(0, 4)));
        assert_eq!(byte_range(Some(" BYTES= 1 - 2 "), 5), Some(Bytes(1, 2)));
        assert_eq!(
            byte_range(Some("bytes=0-99999999999999999999"), 5),
            Some(Bytes(0, 4))
        );
        assert_eq!(
            byte_range(Some("bytes=99999999999999999999-"), 5),
            Some(Unsatisfiable)
        );
        assert_eq!(byte_range(Some("bytes=-30"), 10), Some(Bytes(0, 9)));
        assert_eq!(byte_range(Some("bytes=-0"), 10), Some(Unsatisfiable));
        assert_eq!(byte_range(Some("bytes=0-"), 0), Some(Unsatisfiable));
        for ignored in [
            "bytes=0-1,4-5",
            "bytes=5-2",
            "items=0-1",
            "bytes=a-b",
            "bytes=1",
        ] {
            assert_eq!(byte_range(Some(ignored), 10), None, "{ignored}");
        }
        assert_eq!(byte_range(None, 5), None);
        let t = Tree::from_files([(
            "/v.txt".to_string(),
            make_file(b"0123456789".to_vec(), "t", 0o644),
        )])
        .unwrap();
        let opts = SiteServerOptions::default();
        let ask = |method: &str, extra: &[(&str, &str)]| {
            let h: HashMap<String, String> = extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            handle_request(&t, &opts, method, "/v.txt", &h)
        };
        let full = ask("GET", &[]);
        assert_eq!(full.header("accept-ranges"), Some("bytes"));
        let etag = full.header("etag").unwrap().to_string();
        let r = ask("GET", &[("range", "bytes=2-4")]);
        assert_eq!(r.status, 206);
        assert_eq!(r.body, b"234");
        assert_eq!(r.header("content-range"), Some("bytes 2-4/10"));
        assert_eq!(r.header("content-length"), Some("3"));
        let r = ask("GET", &[("range", "bytes=10-")]);
        assert_eq!(
            (r.status, r.header("content-range")),
            (416, Some("bytes */10"))
        );
        assert!(r.body.is_empty());
        let r = ask("GET", &[("range", "bytes=0-0"), ("if-range", &etag)]);
        assert_eq!(r.status, 206);
        assert_eq!(
            ask("GET", &[("range", "bytes=0-0"), ("if-range", "\"x\"")]).status,
            200
        );
        assert_eq!(
            ask("GET", &[("range", "bytes=0-0"), ("if-none-match", &etag)]).status,
            304
        );
        let h = ask("HEAD", &[("range", "bytes=0-1")]);
        assert_eq!((h.status, h.header("content-length")), (200, Some("10")));
    }

    #[test]
    fn absolute_form_targets() {
        with_server(false, |port, log| {
            let home = get(port, "http://example.com/index.html");
            assert_eq!(home.status, 200);
            assert_eq!(home.body, "<h1>home</h1>");
            assert_eq!(get(port, "http://example.com").body, "<h1>home</h1>");
            assert_eq!(
                get(port, "HTTPS://Example.COM:8443?x=1").body,
                "<h1>home</h1>"
            );
            assert_eq!(
                get(port, "http://example.com/about/").body,
                "<h1>about</h1>"
            );
            let dir = get(port, "http://example.com/about?x=1");
            assert_eq!(dir.status, 301);
            assert_eq!(dir.headers["location"], "/about/?x=1");
            assert_eq!(get(port, "http://example.com/..%2f").status, 400);
            assert_eq!(get(port, "ftp://example.com/index.html").status, 400);
            assert!(log.lock().unwrap()[0].starts_with("GET http://example.com/index.html 200 "));
        });
        assert_eq!(origin_form("http://h/a?b"), "/a?b");
        assert_eq!(origin_form("http://h?b"), "/?b");
        assert_eq!(origin_form("http://h"), "/");
        assert_eq!(origin_form("/a"), "/a");
        assert_eq!(origin_form("*"), "*");
    }

    #[test]
    fn traversal_and_encoded_separators_rejected() {
        with_server(false, |port, _| {
            for p in [
                "/..%2f",
                "/..%2F..%2Fetc/passwd",
                "/%2e%2e/secret",
                "/../etc/passwd",
                "/a/../index.html",
                "/%00",
                "/index.html%00.png",
                "/..\\x",
                "/%5cwindows",
                "/a%2fb",
                "/%",
                "/%zz",
                "/.%2e/x",
                "/%c0%af",
            ] {
                let r = get(port, p);
                assert!(r.status == 400 || r.status == 404, "{p} -> {}", r.status);
                assert!(!r.body.contains("root:"), "{p}");
            }
            for p in [
                "/..%2f",
                "/%2e%2e/secret",
                "/../etc/passwd",
                "/%00",
                "/..\\x",
                "/a%2fb",
                "/%zz",
            ] {
                assert_eq!(get(port, p).status, 400, "{p}");
            }
        });
        assert_eq!(safe_segments("/a/b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(
            safe_segments("/a%20b/c"),
            Some(vec!["a b".into(), "c".into()])
        );
        assert_eq!(safe_segments("relative"), None);
    }

    #[test]
    fn access_log_lines() {
        with_server(false, |port, log| {
            get(port, "/app.js");
            get(port, "/nope");
            req(port, "/", "POST", &[("content-length", "0")]);
            let l = log.lock().unwrap().clone();
            let shape = |s: &str, prefix: &str| {
                s.starts_with(prefix)
                    && s.ends_with("ms")
                    && s.rsplit(' ')
                        .next()
                        .unwrap()
                        .trim_end_matches("ms")
                        .contains('.')
            };
            assert!(shape(&l[0], "GET /app.js 200 14 "), "{}", l[0]);
            assert!(shape(&l[1], "GET /nope 404 "), "{}", l[1]);
            assert!(l[2].starts_with("POST / 405 "), "{}", l[2]);
        });
        assert_eq!(printable("/a b\u{e9}"), "/a%20b%e9");
    }

    #[test]
    fn keep_alive_serves_several_requests_and_probes_are_not_logged() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let opts = SiteServerOptions {
            spa: false,
            log: Some(Arc::new(move |l: &str| {
                l2.lock().unwrap().push(l.to_string())
            })),
            server_id: Some("abcdef012345".into()),
            live_reload: None,
            last_modified: None,
            warn: None,
            ..Default::default()
        };
        let s = listen(site(), opts, "127.0.0.1", 0).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        c.write_all(b"GET /app.js HTTP/1.1\r\nhost: x\r\n\r\nHEAD / HTTP/1.1\r\nhost: x\r\nx-sitefile-probe: abcdef012345\r\nconnection: close\r\n\r\n").unwrap();
        let mut buf = String::new();
        c.read_to_string(&mut buf).unwrap();
        assert_eq!(buf.matches("HTTP/1.1 200 OK").count(), 2);
        assert!(buf.contains("x-sitefile-server: abcdef012345"));
        assert_eq!(log.lock().unwrap().len(), 1);
        let mut bad = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        bad.write_all(b"garbage\r\n\r\n").unwrap();
        let mut b2 = String::new();
        bad.read_to_string(&mut b2).unwrap();
        assert!(b2.starts_with("HTTP/1.1 400"));
        s.close();
    }

    #[test]
    fn port_in_use_is_e_server() {
        let blocker = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = blocker.local_addr().unwrap().port();
        let e = listen(site(), SiteServerOptions::default(), "127.0.0.1", port)
            .err()
            .unwrap();
        assert_eq!(e.code, E_SERVER);
        assert!(e.detail.contains("already in use"), "{}", e.detail);
        assert_eq!(format_url("0.0.0.0", 8080), "http://127.0.0.1:8080/");
        assert_eq!(format_url("::1", 1), "http://[::1]:1/");
    }

    // ---- 404.html, clean URLs, _redirects, _headers (test/server.test.ts)

    fn rules_site() -> Tree {
        let redirects = [
            "# comments and blank lines are skipped",
            "",
            "/old /new 301",
            "/temp /new 302",
            "/blog/* https://blog.example/:splat",
            "/app/* /app/index.html 200",
            "/gone /404.html 404",
            "/about /elsewhere 301",
            "/docs /forced 308!",
            "/users/:id /u/:id 301",
            "/x /y 301 Country=nz",
            "/bad /bad\u{1} 301",
        ]
        .join("\n");
        let headers = [
            "/assets/*",
            "  Cache-Control: public, max-age=60",
            "  Content-Length: 1",
            "/*",
            "  X-Frame-Options: DENY",
            "  X-Bad: a\u{1}b",
            "  Cache-Control: immutable",
            "https://other/*",
            "  X-Other: 1",
        ]
        .join("\n");
        let files: Vec<(&str, String)> = vec![
            ("/index.html", "home".into()),
            ("/404.html", "custom 404".into()),
            ("/about.html", "about page".into()),
            ("/docs/index.html", "docs".into()),
            ("/new.html", "new".into()),
            ("/app/index.html", "app shell".into()),
            ("/assets/a.css", "a{}".into()),
            ("/_redirects", redirects),
            ("/_headers", headers),
        ];
        Tree::from_files(
            files
                .into_iter()
                .map(|(p, c)| (p.to_string(), make_file(c.into_bytes(), "t", 0o644))),
        )
        .unwrap()
    }

    fn at(tree: &Tree, url: &str) -> Response {
        handle_request(
            tree,
            &SiteServerOptions::default(),
            "GET",
            url,
            &HashMap::new(),
        )
    }

    fn body(r: &Response) -> &str {
        std::str::from_utf8(&r.body).unwrap()
    }

    #[test]
    fn not_found_page_is_404_html() {
        let t = rules_site();
        let r = at(&t, "/nope");
        assert_eq!((r.status, body(&r)), (404, "custom 404"));
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(r.header("etag"), None);
        let plain = Tree::from_files([(
            "/index.html".to_string(),
            make_file(b"x".to_vec(), "t", 0o644),
        )])
        .unwrap();
        assert!(body(&at(&plain, "/nope")).contains("404 Not Found"));
    }

    #[test]
    fn clean_urls_and_directory_redirects() {
        let t = rules_site();
        let r = at(&t, "/about");
        assert_eq!((r.status, body(&r)), (200, "about page"));
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(body(&at(&t, "/about.html")), "about page");
        assert_eq!(
            at(&t, "/about/").header("location"),
            Some("/elsewhere"),
            "no file at /about/, so the rule applies"
        );
        assert_eq!(
            at(&t, "/docs?q=1").status,
            308,
            "a forced rule wins over the directory"
        );
        let d = Tree::from_files([(
            "/docs/index.html".to_string(),
            make_file(b"docs".to_vec(), "t", 0o644),
        )])
        .unwrap();
        let r = at(&d, "/docs");
        assert_eq!((r.status, r.header("location")), (301, Some("/docs/")));
        // johnhenry/sitefile#188: `/about/` with only `/about.html` redirects to the clean URL.
        let c = Tree::from_files(
            [
                ("/about.html", "about page"),
                ("/a b.html", "space"),
                ("/x/index.html", "x"),
                ("/x.html", "x file"),
            ]
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap();
        let spa = SiteServerOptions {
            spa: true,
            ..Default::default()
        };
        let get = |u: &str| handle_request(&c, &spa, "GET", u, &HashMap::new());
        let r = get("/about/?q=1");
        assert_eq!((r.status, r.header("location")), (301, Some("/about?q=1")));
        assert!(r.body.is_empty());
        assert_eq!(get("/a%20b/").header("location"), Some("/a%20b"));
        assert_eq!(body(&get("/about")), "about page");
        assert_eq!(body(&get("/about.html")), "about page");
        assert_eq!(body(&get("/x/")), "x");
        assert_eq!(body(&get("/x")), "x file");
        assert_eq!(get("/about.html/").status, 404);
    }

    #[test]
    fn redirects_file_rules() {
        let t = rules_site();
        let r = at(&t, "/old?a=1");
        assert_eq!((r.status, r.header("location")), (301, Some("/new?a=1")));
        assert_eq!(
            at(&t, "/old/").header("location"),
            Some("/new"),
            "a trailing slash matches too"
        );
        assert_eq!(at(&t, "/temp").status, 302);
        let r = at(&t, "/blog/2024/a%20b");
        assert_eq!(
            (r.status, r.header("location")),
            (301, Some("https://blog.example/2024/a%20b"))
        );
        assert_eq!(
            at(&t, "/blog").header("location"),
            Some("https://blog.example/")
        );
        let r = at(&t, "/app/settings/profile");
        assert_eq!((r.status, body(&r)), (200, "app shell"));
        let r = at(&t, "/gone");
        assert_eq!((r.status, body(&r)), (404, "custom 404"));
        assert_eq!(
            body(&at(&t, "/about")),
            "about page",
            "a file shadows a rule without !"
        );
        assert_eq!(
            at(&t, "/users/a%20b").header("location"),
            Some("/u/a%20b"),
            "a placeholder matches one segment"
        );
        for path in ["/users/1/2", "/x", "/bad", "/_redirects", "/_headers"] {
            assert_eq!(at(&t, path).status, 404, "{path}");
        }
        assert_eq!(reason(308), "Permanent Redirect");
    }

    // johnhenry/sitefile#195 (test/server.test.ts)
    #[test]
    fn base_path_mount() {
        assert_eq!(normalize_base("repo").as_deref(), Some("/repo/"));
        assert_eq!(normalize_base("/a b//c/").as_deref(), Some("/a%20b/c/"));
        assert_eq!(
            normalize_base("https://user.github.io/repo/?x").as_deref(),
            Some("/repo/")
        );
        assert_eq!(normalize_base("https://example.com").as_deref(), Some("/"));
        assert_eq!(normalize_base("/../x"), None);
        let t = Tree::from_files(
            [
                ("/index.html", "home"),
                ("/docs/index.html", "docs"),
                ("/about.html", "about"),
                ("/404.html", "site 404"),
                ("/_redirects", "/old /about 301\n/ext https://e.x/ 302\n"),
            ]
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap();
        let opts = SiteServerOptions {
            base: Some(Arc::new(|| Some("/repo/".to_string()))),
            ..Default::default()
        };
        let get = |u: &str| handle_request(&t, &opts, "GET", u, &HashMap::new());
        let r = get("/?q=1");
        assert_eq!((r.status, r.header("location")), (302, Some("/repo/?q=1")));
        let r = get("/repo");
        assert_eq!((r.status, r.header("location")), (301, Some("/repo/")));
        assert_eq!(body(&get("/repo/")), "home");
        assert_eq!(body(&get("/repo/about")), "about");
        assert_eq!(get("/repo/docs").header("location"), Some("/repo/docs/"));
        assert_eq!(get("/repo/about/").header("location"), Some("/repo/about"));
        assert_eq!(get("/repo/old").header("location"), Some("/repo/about"));
        assert_eq!(get("/repo/ext").header("location"), Some("https://e.x/"));
        let r = get("/repo/missing");
        assert_eq!((r.status, body(&r)), (404, "site 404"));
        let r = get("/about");
        assert_eq!(r.status, 404);
        assert!(body(&r).contains("404 Not Found"));
        assert_eq!(get("/repository/").status, 404);
    }

    // johnhenry/sitefile#193 (the same site as test/server.test.ts)
    #[test]
    fn redirects_placeholders_queries_conditions_and_warnings() {
        let redirects = [
            "/blog/:year/:slug /posts/:year/:slug 301",
            "/store id=:id /items/:id 302",
            "/search q=cats /cats 302",
            "/old-store id=:id /new?item=:id 301",
            "/lang / 302 Language=es,pt-BR",
            "/ab /b-variant 302 Cookie=ab_test",
            "/geo /nz 302! Country=nz,au",
            "/removed /gone.html 410",
            "/legal /gone.html 451",
            "/files/* /f/:splat/:nope 301",
            "/a/*/b /c",
            "/x /y 999",
            "/admin /secret 200! Role=admin",
            "/q bad=%zz /y",
            "/z /y 301 Planet=mars",
        ]
        .join("\n");
        let headers =
            "  X-Orphan: 1\n/*\n  ! X-Detach\n  Content-Length: 1\n  Bad Name: x\nnot-a-path\n";
        let files: Vec<(&str, String)> = vec![
            ("/index.html", "home".into()),
            ("/gone.html", "gone".into()),
            ("/es/index.html", "hola".into()),
            ("/docs/404.html", "docs 404".into()),
            ("/docs/a/index.html", "a".into()),
            ("/_redirects", redirects),
            ("/_headers", headers.into()),
        ];
        let t = Tree::from_files(
            files
                .into_iter()
                .map(|(p, c)| (p.to_string(), make_file(c.into_bytes(), "t", 0o644))),
        )
        .unwrap();
        assert_eq!(
            site_file_warnings(&t, None),
            [
                "_redirects:11: ignored: '*' is only supported as the last segment of the source",
                "_redirects:12: ignored: unsupported status (supported: 200, 301, 302, 303, 307, 308, 404, 410, 451)",
                "_redirects:13: ignored: Role= conditions need Netlify Identity and are not supported",
                "_redirects:14: ignored: bad query parameter (expected name=value or name=:placeholder)",
                "_redirects:15: ignored: unsupported condition (supported: Language=, Country=, Cookie=)",
                "_headers:1: ignored: a header line without a path line above it",
                "_headers:3: ignored: '! Name' detach lines are not supported",
                "_headers:4: ignored: the server sets this header itself",
                "_headers:5: ignored: invalid header name",
                "_headers:6: ignored: a path pattern must start with '/' (indent header lines)",
            ]
        );
        let opts = SiteServerOptions {
            country: Some("NZ".into()),
            ..Default::default()
        };
        let get = |u: &str, h: &[(&str, &str)]| {
            let h: HashMap<String, String> = h
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            handle_request(&t, &opts, "GET", u, &h)
        };
        let loc = |u: &str| get(u, &[]).header("location").map(str::to_string);
        let r = get("/blog/2024/hello", &[]);
        assert_eq!(r.header("location"), Some("/posts/2024/hello"));
        assert_eq!(
            r.note.as_deref(),
            Some(" \u{2192} /posts/2024/hello (_redirects:1)")
        );
        assert_eq!(loc("/store?id=7&x=1").as_deref(), Some("/items/7"));
        assert_eq!(get("/store", &[]).status, 404);
        assert_eq!(get("/store", &[]).note, None);
        assert_eq!(loc("/search?q=cats").as_deref(), Some("/cats"));
        assert_eq!(get("/search?q=dogs", &[]).status, 404);
        assert_eq!(
            loc("/old-store?id=a%20b").as_deref(),
            Some("/new?item=a%20b")
        );
        let al = |v: &str| get("/lang", &[("accept-language", v)]).status;
        assert_eq!(al("fr;q=1, es-MX;q=0.8"), 302);
        assert_eq!(al("pt-br"), 302);
        assert_eq!(al("es;q=0, pt"), 404);
        assert_eq!(get("/ab", &[("cookie", "x=1; ab_test=b")]).status, 302);
        assert_eq!(get("/ab", &[("cookie", "ab_test_2=1")]).status, 404);
        assert_eq!(loc("/geo").as_deref(), Some("/nz"));
        let r = get("/removed", &[]);
        assert_eq!((r.status, body(&r)), (410, "gone"));
        assert_eq!(
            r.note.as_deref(),
            Some(" \u{2192} /gone.html (_redirects:8)")
        );
        assert_eq!(get("/legal", &[]).status, 451);
        assert_eq!(loc("/files/a/b%20c").as_deref(), Some("/f/a/b%20c/:nope"));
        let r = get("/docs/a/missing", &[]);
        assert_eq!((r.status, body(&r)), (404, "docs 404"));
        assert_eq!(body(&get("/docs/missing/", &[])), "docs 404");
        assert!(body(&get("/elsewhere", &[])).contains("404 Not Found"));
        assert_eq!(at(&t, "/geo").status, 404, "Country= needs --country");
    }

    #[test]
    fn headers_file_rules() {
        let t = rules_site();
        let r = at(&t, "/assets/a.css");
        assert_eq!(r.status, 200);
        assert_eq!(
            r.header("cache-control"),
            Some("public, max-age=60, immutable")
        );
        assert_eq!(r.header("x-frame-options"), Some("DENY"));
        assert_eq!(r.header("content-length"), Some("3"));
        assert_eq!((r.header("x-bad"), r.header("x-other")), (None, None));
        assert_eq!(
            r.headers
                .iter()
                .filter(|(k, _)| k == "cache-control")
                .count(),
            1
        );
        assert_eq!(at(&t, "/").header("cache-control"), Some("immutable"));
        assert_eq!(
            at(&t, "/nope").header("x-frame-options"),
            Some("DENY"),
            "the 404 page gets headers too"
        );
        assert_eq!(
            at(&t, "/old").header("x-frame-options"),
            None,
            "redirects do not"
        );
        let etag = r.header("etag").unwrap().to_string();
        let h: HashMap<String, String> =
            [("if-none-match".to_string(), etag)].into_iter().collect();
        let r = handle_request(
            &t,
            &SiteServerOptions::default(),
            "GET",
            "/assets/a.css",
            &h,
        );
        assert_eq!(
            (r.status, r.header("cache-control")),
            (304, Some("public, max-age=60, immutable"))
        );
    }

    #[test]
    fn rule_parsers_and_glob() {
        assert!(
            glob_match("/*.css", "/a/b.css")
                && glob_match("/*", "/")
                && glob_match("/a*b*c", "/aXbYc")
        );
        assert!(!glob_match("/*.css", "/a.js") && !glob_match("/a", "/ab"));
        let r = parse_redirect_rules("/a /b\n/c/* /d/:splat 302!\n/e\n/f /g 418\n/h ftp://x\n");
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[1].from.as_str(), r[1].splat, r[1].status, r[1].force),
            ("/c", true, 302, true)
        );
        let h = parse_header_rules("  X-Orphan: 1\n/a\r\n  X-A: 1\r\n  bad name: 2\n");
        assert_eq!(
            h,
            vec![ServeHeaders {
                pattern: "/a".into(),
                headers: vec![("x-a".into(), "1".into())]
            }]
        );
    }

    #[test]
    fn inject_reload_script_before_last_body() {
        let inj = |s: &str| String::from_utf8(inject_reload_script(s.as_bytes())).unwrap();
        assert_eq!(
            inj("<p>a</p></BODY></html>"),
            format!("<p>a</p>{RELOAD_SCRIPT}</BODY></html>")
        );
        assert_eq!(
            inj("<body></body><!-- </body> --></body>"),
            format!("<body></body><!-- </body> -->{RELOAD_SCRIPT}</body>")
        );
        assert_eq!(inj("<h1>x</h1>"), format!("<h1>x</h1>{RELOAD_SCRIPT}"));
        assert_eq!(inj(""), RELOAD_SCRIPT);
        assert_eq!(
            RELOAD_SCRIPT,
            format!("<script src=\"{RELOAD_SCRIPT_PATH}\"></script>")
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// What a browser sends for a page navigation (only those get the reload script).
    fn nav() -> HashMap<String, String> {
        headers(&[
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "document"),
        ])
    }

    #[test]
    fn reload_client_is_the_typescript_literal() {
        let js = reload_client();
        assert!(js.starts_with("(function (options) {"), "{js}");
        assert!(js.ends_with("})"), "{js}");
        assert!(!js.contains('`'));
        assert_eq!(
            client_script(&LiveReloadOptions::default()),
            format!("{js}({{\"resetServiceWorkers\":true,\"cssInject\":true,\"console\":false,\"overlay\":true,\"navigateToChanged\":false,\"reload\":\"morph\"}});\n")
        );
        let keep = LiveReload::with_options(&LiveReloadOptions {
            reset_service_workers: false,
            css_inject: false,
            console: true,
            overlay: false,
            navigate_to_changed: true,
            morph: false,
        });
        assert_eq!(
            keep.script(),
            format!(
                "{js}({{\"resetServiceWorkers\":false,\"cssInject\":false,\"console\":true,\"overlay\":false,\"navigateToChanged\":true,\"reload\":\"full\"}});\n"
            )
        );
        assert!(js.contains("attachShadow"));
        assert!(js.contains("getRegistrations()") && js.contains("caches.delete(k)"));
    }

    #[test]
    fn reload_event_for_nothing_css_or_reload() {
        let t = |files: &[(&str, &str)]| {
            Tree::from_files(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
            )
            .unwrap()
        };
        let a = t(&[("/index.html", "i"), ("/a.css", "a"), ("/z/b.css", "b")]);
        let e = |next: &Tree, css: bool| reload_event_for(&a, next, css, false);
        assert_eq!(
            e(
                &t(&[("/index.html", "i"), ("/a.css", "a"), ("/z/b.css", "b")]),
                true
            ),
            None
        );
        assert_eq!(
            e(
                &t(&[("/index.html", "i"), ("/a.css", "A"), ("/z/b.css", "B")]),
                true
            )
            .as_deref(),
            Some("{\"type\":\"css\",\"paths\":[\"/a.css\",\"/z/b.css\"]}")
        );
        let reload = Some(RELOAD_EVENT);
        assert_eq!(
            e(
                &t(&[("/index.html", "i"), ("/a.css", "A"), ("/z/b.css", "b")]),
                false
            )
            .as_deref(),
            reload
        );
        assert_eq!(
            e(
                &t(&[("/index.html", "I"), ("/a.css", "A"), ("/z/b.css", "b")]),
                true
            )
            .as_deref(),
            reload
        );
        assert_eq!(
            e(
                &t(&[
                    ("/index.html", "i"),
                    ("/a.css", "a"),
                    ("/z/b.css", "b"),
                    ("/c.css", "c")
                ]),
                true
            )
            .as_deref(),
            reload
        );
        assert_eq!(
            e(&t(&[("/index.html", "i"), ("/a.css", "a")]), true).as_deref(),
            reload
        );
        assert_eq!(RELOAD_EVENT, "{\"type\":\"reload\"}");
    }

    #[test]
    fn console_lines_levels_cleaning_location_stack() {
        let l = |b: &str, all: bool| console_lines(b, all);
        assert_eq!(
            l("{\"level\":\"error\",\"message\":\"Uncaught TypeError: x\\nis undefined\",\"url\":\"http://h/app.js\",\"line\":3,\"col\":5}", false),
            Some(vec!["browser error: Uncaught TypeError: x is undefined (http://h/app.js:3:5)".to_string()])
        );
        assert_eq!(
            l(
                "{\"level\":\"warn\",\"message\":\"w\",\"url\":\"http://h/\",\"line\":2}",
                false
            ),
            Some(vec!["browser warn: w (http://h/:2)".to_string()])
        );
        assert_eq!(
            l(
                "{\"level\":\"warn\",\"message\":\"w\",\"url\":\"http://h/\",\"col\":2}",
                false
            ),
            Some(vec!["browser warn: w (http://h/)".to_string()])
        );
        assert_eq!(
            l(
                "{\"level\":\"error\",\"message\":\"m\",\"line\":1.5,\"url\":\"u\"}",
                false
            ),
            Some(vec!["browser error: m (u)".to_string()])
        );
        assert_eq!(
            l("{\"level\":\"log\",\"message\":\"hi\"}", false),
            Some(vec![])
        );
        assert_eq!(
            l("{\"level\":\"info\",\"message\":\"hi\"}", true),
            Some(vec!["browser info: hi".to_string()])
        );
        assert_eq!(
            l("{\"level\":\"error\",\"message\":\"E: m\",\"stack\":\"E: m\\n    at f (a.js:1:2)\\n\\n    at g (b.js:3:4)\"}", false),
            Some(vec![
                "browser error: E: m".to_string(),
                "    at f (a.js:1:2)".to_string(),
                "    at g (b.js:3:4)".to_string()
            ])
        );
        let long =
            serde_json::json!({"level": "error", "message": "\u{1F600}".repeat(1200)}).to_string();
        assert_eq!(
            l(&long, false).unwrap()[0],
            format!("browser error: {}...", "\u{1F600}".repeat(1000))
        );
        let stack: Vec<String> = (0..9).map(|i| format!("at f{i}")).collect();
        let frames =
            serde_json::json!({"level": "error", "message": "m", "stack": stack.join("\n")})
                .to_string();
        assert_eq!(l(&frames, false).unwrap().len(), 6);
        for bad in [
            "",
            "x",
            "[]",
            "null",
            "{\"level\":\"debug\",\"message\":\"m\"}",
            "{\"message\":\"m\"}",
        ] {
            assert_eq!(l(bad, true), None, "{bad}");
        }
    }

    #[test]
    fn is_same_origin_post_rules() {
        let o = |p: &[(&str, &str)]| is_same_origin_post(&headers(p));
        assert!(o(&[
            ("host", "127.0.0.1:8080"),
            ("origin", "http://127.0.0.1:8080")
        ]));
        assert!(o(&[
            ("host", "x.example"),
            ("origin", "https://x.example"),
            ("sec-fetch-site", "same-origin")
        ]));
        assert!(!o(&[
            ("host", "127.0.0.1:8080"),
            ("origin", "http://evil.example")
        ]));
        assert!(!o(&[("host", "127.0.0.1:8080")]));
        assert!(!o(&[
            ("host", "127.0.0.1:8080"),
            ("origin", "http://127.0.0.1:8080"),
            ("sec-fetch-site", "cross-site")
        ]));
        assert!(!o(&[("host", "127.0.0.1:8080"), ("origin", "null")]));
    }

    #[test]
    fn console_endpoint_over_a_socket() {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = log.clone();
        let opts = SiteServerOptions {
            live_reload: Some(LiveReload::new()),
            log: Some(Arc::new(move |m: &str| {
                l2.lock().unwrap().push(m.to_string())
            })),
            ..Default::default()
        };
        let srv = listen(site(), opts, "127.0.0.1", 0).unwrap();
        let plain = listen(site(), SiteServerOptions::default(), "127.0.0.1", 0).unwrap();
        let post = |port: u16, body: &str, origin: Option<&str>| -> u16 {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let host = format!("127.0.0.1:{port}");
            let o = origin
                .map(|o| {
                    format!(
                        "origin: {}\r\n",
                        o.replace("SELF", &format!("http://{host}"))
                    )
                })
                .unwrap_or_default();
            write!(
                c,
                "POST /__sitefile/console HTTP/1.1\r\nhost: {host}\r\n{o}content-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut buf = String::new();
            let _ = c.read_to_string(&mut buf);
            buf.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0)
        };
        let me = Some("SELF");
        assert_eq!(
            post(srv.port, "{\"level\":\"error\",\"message\":\"Uncaught Error: boom\",\"url\":\"http://127.0.0.1/app.js\",\"line\":1,\"col\":7}", me),
            204
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec!["browser error: Uncaught Error: boom (http://127.0.0.1/app.js:1:7)".to_string()]
        );
        assert_eq!(
            post(
                srv.port,
                "{\"level\":\"error\",\"message\":\"x\"}",
                Some("http://evil.example")
            ),
            403
        );
        assert_eq!(
            post(srv.port, "{\"level\":\"error\",\"message\":\"x\"}", None),
            403
        );
        assert_eq!(post(srv.port, "not json", me), 400);
        let big = serde_json::json!({"level": "error", "message": "x".repeat(20000)}).to_string();
        assert_eq!(post(srv.port, &big, me), 413);
        assert_eq!(get(srv.port, "/__sitefile/console").status, 405);
        assert_eq!(
            post(plain.port, "{\"level\":\"error\",\"message\":\"x\"}", me),
            405
        );
        log.lock().unwrap().clear();
        for i in 0..CONSOLE_LIMIT + 5 {
            post(
                srv.port,
                &format!("{{\"level\":\"warn\",\"message\":\"w{i}\"}}"),
                me,
            );
        }
        let got = log.lock().unwrap().clone();
        assert_eq!(got.len(), CONSOLE_LIMIT as usize);
        assert_eq!(
            got.last().unwrap(),
            "browser: more than 50 console messages in 10 s; dropping the rest"
        );
        srv.close();
        plain.close();
    }

    #[test]
    fn error_event_shape() {
        let e = SitefileError::new(
            E_SYNTAX,
            "unknown instruction \"X\"",
            Some(crate::error::SourceSpan::new("/c/Sitefile", 3, 1)),
        );
        assert_eq!(
            error_event(&e),
            "{\"type\":\"error\",\"code\":\"E_SYNTAX\",\"message\":\"unknown instruction \\\"X\\\"\",\"file\":\"/c/Sitefile\",\"line\":3,\"column\":1}"
        );
        assert_eq!(
            error_event(&SitefileError::plain(E_NOT_FOUND, "no such file")),
            "{\"type\":\"error\",\"code\":\"E_NOT_FOUND\",\"message\":\"no such file\"}"
        );
        assert_eq!(BUILDING_EVENT, "{\"type\":\"building\"}");
        assert_eq!(OK_EVENT, "{\"type\":\"ok\"}");
    }

    #[test]
    fn navigate_to_changed_names_the_one_changed_page() {
        let t = |files: &[(&str, &str)]| {
            Tree::from_files(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
            )
            .unwrap()
        };
        let base: Vec<(&str, &str)> = vec![
            ("/index.html", "i"),
            ("/posts/a/index.html", "a"),
            ("/posts/b/index.html", "b"),
            ("/x.html", "x"),
            ("/s.css", "s"),
            ("/app.js", "1"),
        ];
        let a = t(&base);
        let with = |edits: &[(&'static str, &'static str)]| {
            let mut v = base.clone();
            for (p, c) in edits {
                match v.iter_mut().find(|(q, _)| q == p) {
                    Some(e) => e.1 = c,
                    None => v.push((p, c)),
                }
            }
            t(&v)
        };
        let e = |next: &Tree, nav: bool| reload_event_for(&a, next, true, nav);
        assert_eq!(
            e(&with(&[("/posts/a/index.html", "A")]), true).as_deref(),
            Some("{\"type\":\"reload\",\"path\":\"/posts/a/\",\"morph\":true}")
        );
        assert_eq!(
            e(&with(&[("/posts/a/index.html", "A")]), false).as_deref(),
            Some("{\"type\":\"reload\",\"morph\":true}")
        );
        assert_eq!(
            e(&with(&[("/x.html", "X"), ("/app.js", "2")]), true).as_deref(),
            Some("{\"type\":\"reload\",\"path\":\"/x.html\"}")
        );
        assert_eq!(
            e(
                &with(&[("/index.html", "I"), ("/posts/a/index.html", "A")]),
                true
            )
            .as_deref(),
            Some("{\"type\":\"reload\",\"morph\":true}")
        );
        assert_eq!(
            e(&with(&[("/app.js", "2")]), true).as_deref(),
            Some(RELOAD_EVENT)
        );
        assert_eq!(
            e(&with(&[("/s.css", "S")]), true).as_deref(),
            Some("{\"type\":\"css\",\"paths\":[\"/s.css\"]}")
        );
        assert_eq!(
            e(&with(&[("/new/index.html", "n")]), true).as_deref(),
            Some("{\"type\":\"reload\",\"path\":\"/new/\"}")
        );
        assert_eq!(page_url_for("/index.html"), "/");
        assert_eq!(page_url_for("/a b/index.html"), "/a%20b/");
        assert_eq!(page_url_for("/caf\u{e9}.html"), "/caf%C3%A9.html");
    }

    #[test]
    fn morph_marks_reloads_where_only_existing_html_changed() {
        let t = |files: &[(&str, &str)]| {
            Tree::from_files(
                files
                    .iter()
                    .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
            )
            .unwrap()
        };
        let base = [
            ("/index.html", "i"),
            ("/b.html", "b"),
            ("/app.js", "1"),
            ("/s.css", "s"),
        ];
        let a = t(&base);
        let m = |edits: &[(&'static str, &'static str)]| {
            let mut v: Vec<(&str, &str)> = base.to_vec();
            for (p, c) in edits {
                match v.iter_mut().find(|(q, _)| q == p) {
                    Some(e) => e.1 = c,
                    None => v.push((p, c)),
                }
            }
            reload_event_for(&a, &t(&v), true, false)
        };
        let morph = Some("{\"type\":\"reload\",\"morph\":true}");
        assert_eq!(m(&[("/index.html", "I")]).as_deref(), morph);
        assert_eq!(
            m(&[("/index.html", "I"), ("/b.html", "B")]).as_deref(),
            morph
        );
        assert_eq!(
            m(&[("/index.html", "I"), ("/app.js", "2")]).as_deref(),
            Some(RELOAD_EVENT)
        );
        assert_eq!(
            m(&[("/index.html", "I"), ("/s.css", "S")]).as_deref(),
            Some(RELOAD_EVENT)
        );
        assert_eq!(m(&[("/c.html", "c")]).as_deref(), Some(RELOAD_EVENT));
        let removed = t(&[("/index.html", "I"), ("/app.js", "1"), ("/s.css", "s")]);
        assert_eq!(
            reload_event_for(&a, &removed, true, false).as_deref(),
            Some(RELOAD_EVENT)
        );
        let js = reload_client();
        for needle in [
            "scriptsOf(doc) !== scriptsOf(document)",
            "document.activeElement",
            "sessionStorage.setItem(scrollKey",
            "options.reload !== \"morph\"",
        ] {
            assert!(js.contains(needle), "{needle}");
        }
    }

    #[test]
    fn is_navigation_mode_then_dest_then_accept() {
        let n = |p: &[(&str, &str)]| is_navigation(&headers(p));
        assert!(n(&[("sec-fetch-mode", "navigate")]));
        assert!(!n(&[
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-dest", "document"),
            ("accept", "text/html")
        ]));
        assert!(!n(&[
            ("sec-fetch-mode", "same-origin"),
            ("accept", "text/html")
        ]));
        assert!(n(&[("sec-fetch-dest", "document")]));
        assert!(n(&[("sec-fetch-dest", "iframe")]));
        assert!(!n(&[("sec-fetch-dest", "empty"), ("accept", "text/html")]));
        assert!(n(&[("accept", "Text/HTML,application/xhtml+xml")]));
        assert!(!n(&[("accept", "*/*")]));
        assert!(!n(&[]));
    }

    #[test]
    fn csp_blocks_reload_directives() {
        let c = csp_blocks_reload;
        assert_eq!(c("script-src 'self'"), None);
        assert_eq!(c("default-src 'self'"), None);
        assert_eq!(
            c("default-src 'none'; script-src 'self'"),
            Some("connect-src")
        );
        assert_eq!(
            c("default-src 'none'; script-src 'self'; connect-src 'self'"),
            None
        );
        assert_eq!(c("script-src 'nonce-abc'"), Some("script-src"));
        assert_eq!(c("script-src 'self' 'strict-dynamic'"), Some("script-src"));
        assert_eq!(
            c("script-src-elem 'none'; script-src 'self'"),
            Some("script-src")
        );
        assert_eq!(c("SCRIPT-SRC 'SELF'; script-src 'none'"), None);
        assert_eq!(c("script-src https:; connect-src *"), None);
        assert_eq!(c("img-src 'none'"), None);
        assert_eq!(
            c("script-src 'self', script-src 'none'"),
            Some("script-src")
        );
    }

    #[test]
    fn live_reload_serves_the_script_and_warns_about_a_blocking_csp() {
        let warnings = Arc::new(Mutex::new(Vec::<String>::new()));
        let w2 = warnings.clone();
        let opts = SiteServerOptions {
            live_reload: Some(LiveReload::new()),
            warn: Some(Arc::new(move |m: &str| {
                w2.lock().unwrap().push(m.to_string())
            })),
            ..Default::default()
        };
        let tree = Tree::from_files(
            [
                ("/index.html", "home"),
                ("/a.html", "a"),
                (
                    "/_headers",
                    "/*\n  Content-Security-Policy: script-src 'nonce-x'\n",
                ),
            ]
            .iter()
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap();
        let none = HashMap::new();
        let js = handle_request(&tree, &opts, "GET", RELOAD_SCRIPT_PATH, &none);
        assert_eq!(js.status, 200);
        assert_eq!(
            js.body,
            client_script(&LiveReloadOptions::default()).as_bytes()
        );
        assert_eq!(
            js.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(js.header("cache-control"), Some("no-cache"));
        assert_eq!(
            handle_request(
                &tree,
                &SiteServerOptions::default(),
                "GET",
                RELOAD_SCRIPT_PATH,
                &none
            )
            .status,
            404
        );
        let cors = headers(&[("sec-fetch-mode", "cors")]);
        let r = handle_request(&tree, &opts, "GET", "/", &cors);
        assert_eq!(r.body, b"home", "a fetch() of HTML is untouched");
        assert!(
            warnings.lock().unwrap().is_empty(),
            "only navigations are checked"
        );
        handle_request(&tree, &opts, "GET", "/", &nav());
        handle_request(&tree, &opts, "GET", "/a", &nav());
        assert_eq!(*warnings.lock().unwrap(), vec![
            "live reload: the Content-Security-Policy that _headers sets for /index.html does not allow scripts from 'self' (/__sitefile/reload.js), so open pages will not reload: script-src 'nonce-x'".to_string()
        ]);
    }

    #[test]
    fn live_reload_injects_html_only_and_swaps_trees() {
        let live = LiveReload::new();
        let opts = SiteServerOptions {
            live_reload: Some(live.clone()),
            ..Default::default()
        };
        let h = nav();
        let tree = site();
        let r = handle_request(&tree, &opts, "GET", "/", &h);
        assert_eq!(
            String::from_utf8(r.body.clone()).unwrap(),
            format!("<h1>home</h1>{RELOAD_SCRIPT}")
        );
        assert_eq!(
            r.header("content-length"),
            Some(r.body.len().to_string().as_str())
        );
        let etag = r.header("etag").unwrap().to_string();
        assert!(etag.ends_with("-live\""));
        let mut inm = nav();
        inm.insert("if-none-match".to_string(), etag);
        assert_eq!(handle_request(&tree, &opts, "GET", "/", &inm).status, 304);
        assert_eq!(
            handle_request(&tree, &opts, "GET", "/style.css", &h).body,
            b"a{}"
        );
        assert_eq!(
            handle_request(&tree, &opts, "HEAD", RELOAD_PATH, &h).header("content-type"),
            Some("text/event-stream")
        );
        let head = handle_request(&tree, &opts, "HEAD", RELOAD_PATH, &h);
        assert_eq!(
            (
                head.header("cache-control"),
                head.header("x-accel-buffering")
            ),
            (Some("no-cache, no-transform"), Some("no"))
        );
        // without --watch: no script, and the path is an ordinary (missing) file
        let plain = SiteServerOptions::default();
        assert_eq!(
            handle_request(&tree, &plain, "GET", "/", &h).body,
            b"<h1>home</h1>"
        );
        assert_eq!(
            handle_request(&tree, &plain, "GET", RELOAD_PATH, &h).status,
            404
        );

        // over a socket: the stream gets `data: reload`, and a swapped tree is served
        let shared: SharedTree = Arc::new(RwLock::new(tree));
        let srv = listen_shared(shared.clone(), opts, "127.0.0.1", 0).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.write_all(format!("GET {RELOAD_PATH} HTTP/1.1\r\nhost: x\r\n\r\n").as_bytes())
            .unwrap();
        let mut got = String::new();
        let read_until = |c: &mut TcpStream, got: &mut String, want: &str| {
            while !got.contains(want) {
                let mut b = [0u8; 512];
                let n = c.read(&mut b).unwrap();
                assert!(n > 0);
                got.push_str(&String::from_utf8_lossy(&b[..n]));
            }
        };
        read_until(&mut c, &mut got, "retry: 1000\n\n");
        assert!(got.contains("content-type: text/event-stream"));
        assert!(
            got.contains("cache-control: no-cache, no-transform\r\n"),
            "{got}"
        );
        assert!(got.contains("x-accel-buffering: no\r\n"), "{got}");
        for _ in 0..200 {
            if live.clients() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        *shared.write().unwrap() = Arc::new(
            Tree::from_files([(
                "/index.html".to_string(),
                make_file(b"<h1>v2</h1>".to_vec(), "t", 0o644),
            )])
            .unwrap(),
        );
        live.reload();
        read_until(&mut c, &mut got, "data: {\"type\":\"reload\"}\n\n");
        // events sent back to back all arrive, in order
        live.send("{\"type\":\"css\",\"paths\":[\"/a.css\"]}");
        live.reload();
        read_until(
            &mut c,
            &mut got,
            "data: {\"type\":\"css\",\"paths\":[\"/a.css\"]}\n\ndata: {\"type\":\"reload\"}\n\n",
        );
        assert_eq!(
            req(
                srv.port,
                "/",
                "GET",
                &[
                    ("sec-fetch-mode", "navigate"),
                    ("sec-fetch-dest", "document")
                ]
            )
            .body,
            format!("<h1>v2</h1>{RELOAD_SCRIPT}")
        );
        live.close();
        srv.close();
    }

    #[test]
    fn live_reload_injects_404_html_and_rules_follow_swapped_trees() {
        let mk =
            |files: &[(&str, &str)]| {
                Arc::new(
                    Tree::from_files(files.iter().map(|(p, c)| {
                        (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))
                    }))
                    .unwrap(),
                )
            };
        let opts = SiteServerOptions {
            live_reload: Some(LiveReload::new()),
            ..Default::default()
        };
        let h = nav();
        let cache: RulesCache = Arc::new(Mutex::new(None));
        let one = mk(&[
            ("/index.html", "home"),
            ("/404.html", "<p>gone</p></body>"),
            ("/_redirects", "/a /b 302\n"),
        ]);
        let r = respond(
            &one,
            &rules_for(&cache, &one, None),
            &opts,
            "GET",
            "/missing",
            &h,
        );
        assert_eq!(r.status, 404);
        assert_eq!(
            String::from_utf8(r.body.clone()).unwrap(),
            format!("<p>gone</p>{RELOAD_SCRIPT}</body>")
        );
        assert_eq!(
            r.header("content-length"),
            Some(r.body.len().to_string().as_str())
        );
        assert_eq!(
            respond(&one, &rules_for(&cache, &one, None), &opts, "GET", "/a", &h)
                .header("location"),
            Some("/b")
        );
        let two = mk(&[("/index.html", "v2"), ("/_redirects", "/a /c 301\n")]);
        let r = respond(&two, &rules_for(&cache, &two, None), &opts, "GET", "/a", &h);
        assert_eq!((r.status, r.header("location")), (301, Some("/c")));
    }
}
