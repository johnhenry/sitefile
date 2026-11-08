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
    /// Fall back to `/index.html` for unknown extension-less paths (single-page apps).
    pub spa: bool,
    /// One access-log line per request: `METHOD /path STATUS BYTES MSms`.
    pub log: Option<LogFn>,
    /// Sent as `x-sitefile-server` so `servers` can tell its own servers from a reused port.
    pub server_id: Option<String>,
    /// `serve --watch`: answer [`RELOAD_PATH`] with an event stream and [`RELOAD_SCRIPT_PATH`]
    /// with the client script, and add [`RELOAD_SCRIPT`] to HTML navigations. Never set
    /// otherwise, and never written to exports.
    pub live_reload: Option<Arc<LiveReload>>,
    /// Problems a person should know about (without the `warning: ` prefix), e.g. a CSP that
    /// blocks live reload.
    pub warn: Option<LogFn>,
}

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

/// Open live-reload streams; [`LiveReload::reload`] tells every page to reload.
#[derive(Default)]
pub struct LiveReload {
    generation: Mutex<u64>,
    changed: Condvar,
    closed: AtomicBool,
    clients: AtomicUsize,
    /// CSP values already warned about (one warning per policy text).
    warned_csp: Mutex<std::collections::HashSet<String>>,
}

impl LiveReload {
    pub fn new() -> Arc<LiveReload> {
        Arc::new(LiveReload::default())
    }

    pub fn reload(&self) {
        if let Ok(mut g) = self.generation.lock() {
            *g += 1;
        }
        self.changed.notify_all();
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
    fn stream(&self, out: &mut TcpStream, options: &SiteServerOptions) {
        let mut head = String::from(
            "HTTP/1.1 200 OK\r\nx-content-type-options: nosniff\r\ncache-control: no-cache\r\n",
        );
        if let Some(id) = options.server_id.as_deref().filter(|s| !s.is_empty()) {
            head.push_str(&format!("x-sitefile-server: {id}\r\n"));
        }
        head.push_str(
            "content-type: text/event-stream\r\nconnection: close\r\n\r\nretry: 1000\n\n",
        );
        if out
            .write_all(head.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            return;
        }
        self.clients.fetch_add(1, Ordering::SeqCst);
        let mut seen = self.generation.lock().map(|g| *g).unwrap_or(0);
        loop {
            let Ok(g) = self.generation.lock() else { break };
            let Ok((g, _)) = self
                .changed
                .wait_timeout_while(g, Duration::from_secs(15), |g| {
                    *g == seen && !self.closed.load(Ordering::SeqCst)
                })
            else {
                break;
            };
            let now = *g;
            drop(g);
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            // a comment line every 15 s finds clients that went away
            let msg: &[u8] = if now != seen {
                b"data: reload\n\n"
            } else {
                b": ping\n\n"
            };
            seen = now;
            if out.write_all(msg).and_then(|_| out.flush()).is_err() {
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

/// The request path: `url` up to `?` or `#`.
fn raw_path_of(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    &url[..end]
}

const NOT_FOUND_PAGE: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>404 Not Found</title><h1>404 Not Found</h1>\n";
const BAD_REQUEST_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>400 Bad Request</title><h1>400 Bad Request</h1>\n";
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
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "",
    }
}

// ---------------------------------------------------------------- _redirects and _headers
// (src/serve-rules.ts)

/// A `_redirects` rule `serve` understands (docs/CLI.md, "serve").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeRedirect {
    /// Decoded path without a trailing slash (`/` for the root); for a splat rule, the prefix before `/*` (`""` for `/*`).
    pub from: String,
    pub splat: bool,
    pub to: String,
    pub status: u16,
    /// `301!`: applies even when a file exists at the path.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeHeaders {
    pub pattern: String,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
}

/// Headers the server owns; `_headers` cannot set them.
const PROTECTED_HEADERS: [&str; 6] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "etag",
    "location",
];
const SERVE_STATUSES: [u16; 7] = [200, 301, 302, 303, 307, 308, 404];

/// `^https?://[^/]` (case-insensitive).
fn is_http_url(s: &str) -> bool {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    let n = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return false;
    };
    s.as_bytes().get(n).is_some_and(|b| *b != b'/')
}

/// JavaScript's `String.prototype.trim` whitespace (the characters that matter in these files).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Parse `_redirects`: `from to [status[!]]` per line, `#` comments. Supported: exact paths and a
/// trailing `/*` with `:splat` in the target; statuses 301, 302, 303, 307, 308 (redirect), 200
/// (rewrite) and 404. Other lines (placeholders, conditions, query matching) are ignored.
pub fn parse_redirect_rules(text: &str) -> Vec<ServeRedirect> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let t = js_trim(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = t.split([' ', '\t']).filter(|f| !f.is_empty()).collect();
        if fields.len() < 2 || fields.len() > 3 {
            continue;
        }
        let (from, to, st) = (
            fields[0],
            fields[1],
            fields.get(2).copied().unwrap_or("301"),
        );
        let force = st.ends_with('!');
        let code = if force { &st[..st.len() - 1] } else { st };
        let Some(status) = (code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()))
            .then(|| code.parse::<u16>().ok())
            .flatten()
        else {
            continue;
        };
        if !SERVE_STATUSES.contains(&status) {
            continue;
        }
        if to.is_empty()
            || !to.bytes().all(|b| (0x21..=0x7e).contains(&b))
            || !(to.starts_with('/') || is_http_url(to))
        {
            continue;
        }
        if (status == 200 || status == 404) && !to.starts_with('/') {
            continue;
        }
        if !from.starts_with('/') {
            continue;
        }
        let splat = from == "/*" || from.ends_with("/*");
        let prefix = if splat { &from[..from.len() - 2] } else { from };
        if prefix.contains('*') || prefix.split('/').any(|seg| seg.starts_with(':')) {
            continue;
        }
        let Some(segs) = safe_segments(if prefix.is_empty() { "/" } else { prefix }) else {
            continue;
        };
        let decoded = format!("/{}", segs.join("/"));
        let from = if splat && decoded == "/" {
            String::new()
        } else {
            decoded
        };
        out.push(ServeRedirect {
            from,
            splat,
            to: to.to_string(),
            status,
            force,
        });
    }
    out
}

/// Parse `_headers`: a path pattern line (`*` matches anything), then indented `Name: value` lines.
pub fn parse_header_rules(text: &str) -> Vec<ServeHeaders> {
    let mut out: Vec<ServeHeaders> = Vec::new();
    let mut cur = false;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let t = js_trim(line);
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let Some(i) = t.find(':').filter(|i| *i > 0) else {
                continue;
            };
            if !cur {
                continue;
            }
            let name = js_trim(&t[..i]).to_ascii_lowercase();
            let value = js_trim(&t[i + 1..]);
            if !crate::site::is_token(&name)
                || !value
                    .bytes()
                    .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
                || PROTECTED_HEADERS.contains(&name.as_str())
            {
                continue;
            }
            if let Some(last) = out.last_mut() {
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

/// The remainder a rule matches for `path` (`""` for the prefix itself), or `None`.
pub fn match_redirect<'p>(r: &ServeRedirect, path: &'p str) -> Option<&'p str> {
    if !r.splat {
        return (path == r.from).then_some("");
    }
    if r.from.is_empty() {
        return Some(&path[1..]);
    }
    if path == r.from {
        return Some("");
    }
    path.strip_prefix(r.from.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
}

/// The rules a tree's `/_redirects` and `/_headers` define (parsed once per server).
#[derive(Debug, Clone, Default)]
pub struct SiteRules {
    pub redirects: Vec<ServeRedirect>,
    pub headers: Vec<ServeHeaders>,
}

impl SiteRules {
    pub fn from_tree(tree: &Tree) -> SiteRules {
        let text = |p: &str| {
            tree.get(p)
                .map(|f| String::from_utf8_lossy(&f.bytes).into_owned())
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

fn rules_for(cache: &RulesCache, tree: &Arc<Tree>) -> Arc<SiteRules> {
    let mut slot = match cache.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((t, r)) = slot.as_ref() {
        if Arc::ptr_eq(t, tree) {
            return r.clone();
        }
    }
    let r = Arc::new(SiteRules::from_tree(tree));
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
        &SiteRules::from_tree(tree),
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
    let mut base: Vec<(String, String)> = vec![
        ("x-content-type-options".into(), "nosniff".into()),
        ("cache-control".into(), "no-cache".into()),
    ];
    if let Some(id) = options.server_id.as_deref().filter(|s| !s.is_empty()) {
        base.push(("x-sitefile-server".into(), id.to_string()));
    }
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

    if method != "GET" && method != "HEAD" {
        return page(405, METHOD_PAGE, owned(vec![("allow", "GET, HEAD".into())]));
    }
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
            owned(vec![("content-type", "text/event-stream".into())]),
            None,
        );
    }
    if options.live_reload.is_some() && raw_path == RELOAD_SCRIPT_PATH {
        let js = reload_client();
        return finish(
            200,
            owned(vec![
                ("content-type", "text/javascript; charset=utf-8".into()),
                ("content-length", js.len().to_string()),
            ]),
            Some(js.as_bytes().to_vec()),
        );
    }
    let Some(segs) = safe_segments(raw_path) else {
        return page(400, BAD_REQUEST_PAGE, vec![]);
    };
    let trailing = raw_path.ends_with('/');
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
        let body = if live {
            inject_reload_script(&file.bytes)
        } else {
            file.bytes.to_vec()
        };
        let mut h = owned(vec![
            ("content-type", mime.into()),
            ("content-length", body.len().to_string()),
        ]);
        h.extend(custom.iter().cloned());
        if status != 200 {
            return finish(status, h, Some(body));
        }
        // An injected page is a different representation of the file: give it its own ETag.
        let etag = format!(
            "\"{}{}\"",
            file.digest.strip_prefix("sha256:").unwrap_or(&file.digest),
            if live { "-live" } else { "" }
        );
        if etag_matches(headers.get("if-none-match").map(String::as_str), &etag) {
            let mut h: Vec<(String, String)> = custom.clone();
            h.push(("etag".into(), etag));
            return finish(304, h, None);
        }
        h.push(("etag".into(), etag));
        finish(200, h, Some(body))
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
    // Apply the first matching `_redirects` rule (forced ones, or the others).
    let redirect = |forced: bool| -> Option<Response> {
        for r in rules.redirects.iter().filter(|r| r.force == forced) {
            let Some(rest) = match_redirect(r, &vpath) else {
                continue;
            };
            let target = if r.splat {
                let enc: Vec<String> = rest.split('/').map(encode_uri_component).collect();
                r.to.replace(":splat", &enc.join("/"))
            } else {
                r.to.clone()
            };
            if r.status != 200 && r.status != 404 {
                let location = if target.contains('?') {
                    target
                } else {
                    format!("{target}{search}")
                };
                return Some(finish(
                    r.status,
                    owned(vec![("location", location), ("content-length", "0".into())]),
                    None,
                ));
            }
            // 200 (rewrite) and 404: serve the target path's file with that status.
            let tpath = target.split('?').next().unwrap_or("");
            let served = safe_segments(tpath)
                .and_then(|t| resolve(&format!("/{}", t.join("/")), tpath.ends_with('/')));
            if let Some(served) = served {
                return Some(send_file(r.status, &served));
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
        let location = format!("/{}/{search}", enc.join("/"));
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
    if served.is_none() && options.spa {
        let last = segs.last().map(String::as_str).unwrap_or("");
        let has_ext = last.rfind('.').is_some_and(|i| i + 1 < last.len());
        if !has_ext && is_file("/index.html") {
            served = Some("/index.html".into());
        }
    }
    if let Some(served) = served {
        return send_file(200, &served);
    }
    if is_file("/404.html") {
        return send_file(404, "/404.html");
    }
    page(404, NOT_FOUND_PAGE, custom.clone())
}

const MAX_HEAD: usize = 64 * 1024;
const MAX_DISCARD_BODY: u64 = 1 << 20;

struct Request {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    keep_alive: bool,
    body_len: u64,
    chunked: bool,
}

/// Read one request head. `Ok(None)` on a clean EOF before any byte.
fn read_request(
    r: &mut BufReader<TcpStream>,
) -> io::Result<Option<std::result::Result<Request, ()>>> {
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
        url: url.into(),
        headers,
        keep_alive,
        body_len,
        chunked,
    })))
}

fn serve_connection(
    stream: TcpStream,
    tree: &SharedTree,
    rules: &RulesCache,
    options: &SiteServerOptions,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
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
        let mut keep = req.keep_alive && !req.chunked && req.body_len <= MAX_DISCARD_BODY;
        if req.body_len > 0
            && keep
            && io::copy(&mut reader.by_ref().take(req.body_len), &mut io::sink()).is_err()
        {
            keep = false;
        }
        if let Some(live) = options
            .live_reload
            .as_ref()
            .filter(|_| req.method == "GET" && raw_path_of(&req.url) == RELOAD_PATH)
        {
            live.stream(&mut out, options); // not access-logged: one long request per open page
            break;
        }
        let current = match tree.read() {
            Ok(t) => t.clone(),
            Err(_) => break,
        };
        let site_rules = rules_for(rules, &current);
        let res = respond(
            &current,
            &site_rules,
            options,
            &req.method,
            &req.url,
            &req.headers,
        );
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
        let quiet = options.live_reload.is_some() && raw_path_of(&req.url) == RELOAD_SCRIPT_PATH;
        if !probe && !quiet {
            if let Some(log) = &options.log {
                log(&format!(
                    "{} {} {} {} {ms:.1}ms",
                    req.method,
                    printable(&req.url),
                    res.status,
                    res.body.len()
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
            let (tree, rules, options, conns) =
                (tree.clone(), rules.clone(), options.clone(), c2.clone());
            std::thread::spawn(move || {
                serve_connection(stream, &tree, &rules, &options);
                if let Ok(mut m) = conns.lock() {
                    m.remove(&id);
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
            warn: None,
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
    fn spa_fallback_only_for_extensionless_paths() {
        with_server(true, |port, _| {
            let r = get(port, "/some/client/route");
            assert_eq!((r.status, r.body.as_str()), (200, "<h1>home</h1>"));
            assert_eq!(get(port, "/missing.js").status, 404);
            assert_eq!(get(port, "/app.js").body, "console.log(1)");
        });
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
            warn: None,
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
        for path in ["/users/1", "/x", "/bad", "/_redirects", "/_headers"] {
            assert_eq!(at(&t, path).status, 404, "{path}");
        }
        assert_eq!(reason(308), "Permanent Redirect");
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
        assert!(js.starts_with("(function () {"), "{js}");
        assert!(js.ends_with("})();\n"), "{js}");
        assert!(!js.contains('`'));
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
        assert_eq!(js.body, reload_client().as_bytes());
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
        read_until(&mut c, &mut got, "data: reload\n\n");
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
        let r = respond(&one, &rules_for(&cache, &one), &opts, "GET", "/missing", &h);
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
            respond(&one, &rules_for(&cache, &one), &opts, "GET", "/a", &h).header("location"),
            Some("/b")
        );
        let two = mk(&[("/index.html", "v2"), ("/_redirects", "/a /c 301\n")]);
        let r = respond(&two, &rules_for(&cache, &two), &opts, "GET", "/a", &h);
        assert_eq!((r.status, r.header("location")), (301, Some("/c")));
    }
}
