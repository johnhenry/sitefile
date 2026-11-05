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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
}

const NOT_FOUND_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>404 Not Found</title><h1>404 Not Found</h1>\n";
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
            let (h, l) = (b.get(i + 1).copied().and_then(hex)?, b.get(i + 2).copied().and_then(hex)?);
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
        if seg == "." || seg == ".." || seg.chars().any(|c| c == '\\' || c == '/' || (c as u32) < 0x20 || c == '\u{7f}') {
            return None;
        }
        out.push(seg);
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
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "",
    }
}

/// Answer one request (no I/O). `headers` are lowercase names; repeated ones joined with `, `.
pub fn handle_request(tree: &Tree, options: &SiteServerOptions, method: &str, url: &str, headers: &HashMap<String, String>) -> Response {
    let mut base: Vec<(String, String)> = vec![("x-content-type-options".into(), "nosniff".into()), ("cache-control".into(), "no-cache".into())];
    if let Some(id) = options.server_id.as_deref().filter(|s| !s.is_empty()) {
        base.push(("x-sitefile-server".into(), id.to_string()));
    }
    let finish = |status: u16, extra: Vec<(&str, String)>, body: Option<Vec<u8>>| -> Response {
        let mut headers = base.clone();
        for (k, v) in extra {
            headers.retain(|(h, _)| h != k);
            headers.push((k.to_string(), v));
        }
        let body = if method != "HEAD" && status != 304 { body.unwrap_or_default() } else { Vec::new() };
        Response { status, headers, body }
    };
    let page = |status: u16, html: &str, mut extra: Vec<(&str, String)>| -> Response {
        let mut h = vec![("content-type", "text/html; charset=utf-8".to_string()), ("content-length", html.len().to_string())];
        h.append(&mut extra);
        finish(status, h, Some(html.as_bytes().to_vec()))
    };

    if method != "GET" && method != "HEAD" {
        return page(405, METHOD_PAGE, vec![("allow", "GET, HEAD".into())]);
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
    let Some(segs) = safe_segments(raw_path) else { return page(400, BAD_REQUEST_PAGE, vec![]) };
    let trailing = raw_path.ends_with('/');
    let vpath = format!("/{}", segs.join("/"));

    let mut served: Option<String> = if tree.is_file(&vpath) && !(trailing && vpath != "/") { Some(vpath.clone()) } else { None };
    if served.is_none() && tree.is_directory(&vpath) {
        if !trailing {
            let enc: Vec<String> = segs.iter().map(|s| encode_uri_component(s)).collect();
            let location = format!("/{}/{search}", enc.join("/"));
            return finish(301, vec![("location", location), ("content-length", "0".into())], None);
        }
        let index = if vpath == "/" { "/index.html".to_string() } else { format!("{vpath}/index.html") };
        if tree.is_file(&index) {
            served = Some(index);
        }
    }
    if served.is_none() && options.spa {
        let last = segs.last().map(String::as_str).unwrap_or("");
        let has_ext = last.rfind('.').is_some_and(|i| i + 1 < last.len());
        if !has_ext && tree.is_file("/index.html") {
            served = Some("/index.html".into());
        }
    }
    let Some((served, file)) = served.and_then(|s| tree.get(&s).cloned().map(|f| (s, f))) else {
        return page(404, NOT_FOUND_PAGE, vec![]);
    };
    let etag = format!("\"{}\"", file.digest.strip_prefix("sha256:").unwrap_or(&file.digest));
    if etag_matches(headers.get("if-none-match").map(String::as_str), &etag) {
        return finish(304, vec![("etag", etag)], None);
    }
    finish(
        200,
        vec![("etag", etag), ("content-type", mime_type_for(&served).into()), ("content-length", file.bytes.len().to_string())],
        Some(file.bytes.to_vec()),
    )
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
fn read_request(r: &mut BufReader<TcpStream>) -> io::Result<Option<std::result::Result<Request, ()>>> {
    let mut head: Vec<u8> = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r.by_ref().take((MAX_HEAD - head.len() + 1) as u64).read_until(b'\n', &mut line)?;
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
    let (Some(method), Some(url), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Ok(Some(Err(())));
    };
    if method.is_empty() || !method.bytes().all(|c| c.is_ascii_uppercase() || c == b'-') || url.is_empty() || !version.starts_with("HTTP/1.") {
        return Ok(Some(Err(())));
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let Some((k, v)) = l.split_once(':') else { return Ok(Some(Err(()))) };
        let k = k.trim().to_ascii_lowercase();
        if k.is_empty() || k.contains(' ') {
            return Ok(Some(Err(())));
        }
        let v = v.trim().to_string();
        headers.entry(k).and_modify(|e| *e = format!("{e}, {v}")).or_insert(v);
    }
    let conn = headers.get("connection").map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    let keep_alive = if version == "HTTP/1.0" { conn.contains("keep-alive") } else { !conn.contains("close") };
    let chunked = headers.get("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let body_len = match headers.get("content-length") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return Ok(Some(Err(()))),
        },
        None => 0,
    };
    Ok(Some(Ok(Request { method: method.into(), url: url.into(), headers, keep_alive, body_len, chunked })))
}

fn serve_connection(stream: TcpStream, tree: &Tree, options: &SiteServerOptions) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let Ok(mut out) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(Ok(r))) => r,
            Ok(Some(Err(()))) => {
                let _ = out.write_all(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n");
                break;
            }
            _ => break,
        };
        let started = Instant::now();
        let mut keep = req.keep_alive && !req.chunked && req.body_len <= MAX_DISCARD_BODY;
        if req.body_len > 0 && keep && io::copy(&mut reader.by_ref().take(req.body_len), &mut io::sink()).is_err() {
            keep = false;
        }
        let res = handle_request(tree, options, &req.method, &req.url, &req.headers);
        let mut head = format!("HTTP/1.1 {} {}\r\n", res.status, reason(res.status));
        for (k, v) in &res.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(if keep { "connection: keep-alive\r\n\r\n" } else { "connection: close\r\n\r\n" });
        let ok = out.write_all(head.as_bytes()).and_then(|_| out.write_all(&res.body)).and_then(|_| out.flush()).is_ok();
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        // `servers` probes carry the server's own id; they would only clutter the access log.
        let probe = options.server_id.is_some() && req.headers.get("x-sitefile-probe") == options.server_id.as_ref();
        if !probe {
            if let Some(log) = &options.log {
                log(&format!("{} {} {} {} {ms:.1}ms", req.method, printable(&req.url), res.status, res.body.len()));
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
pub fn listen(tree: Arc<Tree>, options: SiteServerOptions, host: &str, port: u16) -> Result<ListeningServer> {
    let fail = |e: &io::Error| {
        let why = match e.kind() {
            io::ErrorKind::AddrInUse => format!("port {port} on {host} is already in use"),
            io::ErrorKind::PermissionDenied => format!("permission denied binding {host}:{port}"),
            _ => format!("cannot listen on {host}:{port}: {e}"),
        };
        SitefileError::plain(E_SERVER, why)
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare, port).to_socket_addrs().map_err(|e| fail(&e))?.collect();
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
            let (tree, options, conns) = (tree.clone(), options.clone(), c2.clone());
            std::thread::spawn(move || {
                serve_connection(stream, &tree, &options);
                if let Ok(mut m) = conns.lock() {
                    m.remove(&id);
                }
            });
        }
    });
    Ok(ListeningServer { host: host.to_string(), port: addr.port(), addr, stop, conns, accept: Some(accept) })
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
            wake.set_ip(if wake.is_ipv6() { "::1".parse().unwrap_or(wake.ip()) } else { [127, 0, 0, 1].into() });
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
                wake.set_ip(if wake.is_ipv6() { "::1".parse().unwrap_or(wake.ip()) } else { [127, 0, 0, 1].into() });
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
    let shown = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
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
        Arc::new(Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644)))).unwrap())
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
        let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let headers = lines.filter_map(|l| l.split_once(": ")).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Res { status, headers, body: body.to_string() }
    }
    fn get(port: u16, path: &str) -> Res {
        req(port, path, "GET", &[])
    }

    fn with_server(spa: bool, f: impl FnOnce(u16, &Arc<Mutex<Vec<String>>>)) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let opts = SiteServerOptions { spa, log: Some(Arc::new(move |line: &str| l2.lock().unwrap().push(line.to_string()))), server_id: None };
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
        let t = Arc::new(Tree::from_files([("/d/file.txt".to_string(), make_file(b"x".to_vec(), "t", 0o644))]).unwrap());
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
            for inm in [etag.clone(), format!("W/{etag}"), format!("\"nope\", {etag}"), "*".to_string()] {
                let r = req(port, "/style.css", "GET", &[("if-none-match", &inm)]);
                assert_eq!(r.status, 304, "{inm}");
                assert_eq!(r.body, "");
                assert_eq!(r.headers["etag"], etag);
            }
            assert_eq!(req(port, "/style.css", "GET", &[("if-none-match", "\"other\"")]).status, 200);
        });
    }

    #[test]
    fn content_types() {
        with_server(false, |port, _| {
            assert!(get(port, "/app.js").headers["content-type"].starts_with("text/javascript"));
            assert!(get(port, "/style.css").headers["content-type"].starts_with("text/css"));
            assert!(get(port, "/data.json").headers["content-type"].starts_with("application/json"));
            assert_eq!(get(port, "/logo.png").headers["content-type"], "image/png");
            assert_eq!(get(port, "/blob.xyz").headers["content-type"], "application/octet-stream");
        });
    }

    #[test]
    fn traversal_and_encoded_separators_rejected() {
        with_server(false, |port, _| {
            for p in [
                "/..%2f", "/..%2F..%2Fetc/passwd", "/%2e%2e/secret", "/../etc/passwd", "/a/../index.html", "/%00", "/index.html%00.png", "/..\\x",
                "/%5cwindows", "/a%2fb", "/%", "/%zz", "/.%2e/x", "/%c0%af",
            ] {
                let r = get(port, p);
                assert!(r.status == 400 || r.status == 404, "{p} -> {}", r.status);
                assert!(!r.body.contains("root:"), "{p}");
            }
            for p in ["/..%2f", "/%2e%2e/secret", "/../etc/passwd", "/%00", "/..\\x", "/a%2fb", "/%zz"] {
                assert_eq!(get(port, p).status, 400, "{p}");
            }
        });
        assert_eq!(safe_segments("/a/b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(safe_segments("/a%20b/c"), Some(vec!["a b".into(), "c".into()]));
        assert_eq!(safe_segments("relative"), None);
    }

    #[test]
    fn access_log_lines() {
        with_server(false, |port, log| {
            get(port, "/app.js");
            get(port, "/nope");
            req(port, "/", "POST", &[("content-length", "0")]);
            let l = log.lock().unwrap().clone();
            let shape = |s: &str, prefix: &str| s.starts_with(prefix) && s.ends_with("ms") && s.rsplit(' ').next().unwrap().trim_end_matches("ms").contains('.');
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
        let opts = SiteServerOptions { spa: false, log: Some(Arc::new(move |l: &str| l2.lock().unwrap().push(l.to_string()))), server_id: Some("abcdef012345".into()) };
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
        let e = listen(site(), SiteServerOptions::default(), "127.0.0.1", port).err().unwrap();
        assert_eq!(e.code, E_SERVER);
        assert!(e.detail.contains("already in use"), "{}", e.detail);
        assert_eq!(format_url("0.0.0.0", 8080), "http://127.0.0.1:8080/");
        assert_eq!(format_url("::1", 1), "http://[::1]:1/");
    }
}
