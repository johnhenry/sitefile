//! Remote resources over HTTPS (port of `src/resources/https.ts`).
//!
//! Redirects are followed manually and every hop is re-validated (scheme,
//! credentials, IP-literal hosts); hostnames are checked at connect time by a
//! guarded [`ureq::Resolver`], so DNS rebinding between validation and
//! connection cannot bypass the SSRF policy. One deadline covers the whole
//! fetch, including redirects and the body.
//!
//! URLs are parsed with ureq's parser (the WHATWG-conformant `url` crate), so
//! hosts such as `2130706433` or `0x7f.1` are canonicalized exactly as the
//! TypeScript version's `new URL()` does before the address check.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::resources::netpolicy::{is_public_address, is_public_ip};
use crate::resources::types::FetchPolicy;
use crate::resources::wbn_reader::split_scheme;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
const USER_AGENT: &str = "sitefile/0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub bytes: Vec<u8>,
    /// URL of the final hop after redirects.
    pub final_url: String,
    /// Media type essence from `content-type` (lowercase, parameters removed).
    pub media_type: Option<String>,
}

/// A parsed http(s) URL.
#[derive(Debug, Clone)]
struct Hop {
    href: String,
    redacted: String,
    /// Lowercase, without the colon.
    scheme: String,
    /// Without IPv6 brackets.
    host: String,
    has_credentials: bool,
    /// scheme://host[:port] (WHATWG origin serialization).
    origin: String,
}

/// What to send. `headers` go to every hop; `origin_headers` (credentials) only to hops on the
/// first URL's origin (scheme, host and port), so a redirect elsewhere never receives them.
/// A request with a body (POST) never follows redirects (E_REDIRECT).
#[derive(Debug, Clone, Default)]
pub struct FetchRequest {
    /// "GET" (default) or "POST".
    pub method: Option<&'static str>,
    pub body: Option<Vec<u8>>,
    pub headers: Vec<(String, String)>,
    pub origin_headers: Vec<(String, String)>,
}

/// Parse with ureq's (WHATWG) parser. Fails for anything without a host.
fn parse_hop(url: &str) -> Option<Hop> {
    let req = ureq::get(url);
    let ru = req.request_url().ok()?;
    let u = ru.as_url();
    let mut clean = u.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    let host = u.host_str().unwrap_or("");
    Some(Hop {
        href: u.as_str().to_string(),
        redacted: clean.as_str().to_string(),
        scheme: u.scheme().to_string(),
        host: strip_brackets(host).to_string(),
        has_credentials: !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()),
        origin: u.origin().ascii_serialization(),
    })
}

/// What a URL parsed with the WHATWG parser carries (Git URLs: credentials and query are refused).
pub(crate) struct UrlParts {
    pub has_credentials: bool,
    /// A non-empty query (`new URL(…).search !== ''`).
    pub has_query: bool,
}

/// Parse an http(s) URL; None when it does not parse.
pub(crate) fn url_parts(url: &str) -> Option<UrlParts> {
    let req = ureq::get(url);
    let ru = req.request_url().ok()?;
    let u = ru.as_url();
    Some(UrlParts {
        has_credentials: !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()),
        has_query: u.query().is_some_and(|q| !q.is_empty()),
    })
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// A URL rendered without any userinfo, safe for error messages.
pub fn redact_url(url: &str) -> String {
    if let Some(h) = parse_hop(url) {
        return h.redacted;
    }
    // Hostless or non-http URLs: drop anything that looks like userinfo.
    match split_scheme(url) {
        Some((scheme, rest)) => match rest.strip_prefix("//") {
            Some(r) => {
                let end = r.find(['/', '?', '#']).unwrap_or(r.len());
                let auth = &r[..end];
                let host = auth.rsplit('@').next().unwrap_or(auth);
                format!("{}://{host}{}", scheme.to_ascii_lowercase(), &r[end..])
            }
            None => url.to_string(),
        },
        None => "<invalid url>".into(),
    }
}

fn protocol_error(scheme: &str, url: &str, span: Option<&SourceSpan>) -> SitefileError {
    let hint = if scheme == "http" { " (plain http requires --allow-http)" } else { "" };
    SitefileError::new(E_PROTOCOL, format!("unsupported protocol '{scheme}:' for {}{hint}", redact_url(url)), span.cloned())
}

/// Parse and validate one hop before connecting.
fn checked_hop(url: &str, policy: &FetchPolicy, span: Option<&SourceSpan>) -> Result<Hop> {
    let Some((scheme, _)) = split_scheme(url.trim()) else {
        return Err(SitefileError::new(E_PROTOCOL, format!("invalid URL '{}'", redact_url(url)), span.cloned()));
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && !(scheme == "http" && policy.allow_http) {
        return Err(protocol_error(&scheme, url, span));
    }
    let hop = parse_hop(url).ok_or_else(|| SitefileError::new(E_PROTOCOL, format!("invalid URL '{}'", redact_url(url)), span.cloned()))?;
    validate(&hop, policy, span)?;
    Ok(hop)
}

fn validate(url: &Hop, policy: &FetchPolicy, span: Option<&SourceSpan>) -> Result<()> {
    if url.scheme != "https" && !(url.scheme == "http" && policy.allow_http) {
        return Err(protocol_error(&url.scheme, &url.href, span));
    }
    if url.has_credentials {
        return Err(SitefileError::new(
            E_PROTOCOL,
            format!("URL {} contains credentials; credentials in URLs are not allowed", url.redacted),
            span.cloned(),
        ));
    }
    if url.host.is_empty() {
        return Err(SitefileError::new(E_PROTOCOL, format!("URL {} has no host", url.redacted), span.cloned()));
    }
    if url.host.parse::<IpAddr>().is_ok() && !policy.allow_private_network && !is_public_address(&url.host) {
        return Err(SitefileError::new(
            E_SSRF_BLOCKED,
            format!(
                "refusing to connect to non-public address {} ({}); use --allow-private-network to permit it",
                url.host, url.redacted
            ),
            span.cloned(),
        ));
    }
    Ok(())
}

/// Validate one hop before connecting: scheme, credentials and IP-literal
/// hosts. Hostnames are checked at connect time by [`GuardedResolver`].
pub fn validate_hop(url: &str, policy: &FetchPolicy, span: Option<&SourceSpan>) -> Result<()> {
    checked_hop(url, policy, span).map(|_| ())
}

/// A resolver that looks up every address and fails with E_SSRF_BLOCKED if any
/// is non-public. ureq calls it when it connects, so DNS rebinding between
/// validation and connection cannot bypass it. The SitefileError behind a
/// failed lookup is kept in `last_error` for the fetch to report.
#[derive(Clone)]
pub struct GuardedResolver {
    allow_private_network: bool,
    span: Option<SourceSpan>,
    last_error: Arc<Mutex<Option<SitefileError>>>,
    /// Bounds the DNS lookup: the fetch deadline and the E_FETCH_TIMEOUT message.
    deadline: Option<(Instant, String)>,
    lookup: Lookup,
}

/// Name lookup for `host:port`; swappable so tests can simulate a slow resolver.
type Lookup = fn(&str) -> io::Result<Vec<SocketAddr>>;

fn system_lookup(netloc: &str) -> io::Result<Vec<SocketAddr>> {
    netloc.to_socket_addrs().map(|a| a.collect())
}

impl GuardedResolver {
    pub fn new(policy: &FetchPolicy, span: Option<&SourceSpan>) -> GuardedResolver {
        GuardedResolver {
            allow_private_network: policy.allow_private_network,
            span: span.cloned(),
            last_error: Arc::new(Mutex::new(None)),
            deadline: None,
            lookup: system_lookup,
        }
    }

    /// Make lookups give up at `deadline` with E_FETCH_TIMEOUT carrying `message`.
    pub fn with_deadline(mut self, deadline: Instant, message: String) -> GuardedResolver {
        self.deadline = Some((deadline, message));
        self
    }

    /// `getaddrinfo` cannot be interrupted, so with a deadline the lookup runs
    /// on a helper thread and is abandoned (it finishes on its own) on timeout.
    fn lookup_bounded(&self, netloc: &str, hostname: &str) -> Result<Vec<SocketAddr>> {
        let cannot = |e: io::Error| SitefileError::new(E_FETCH, format!("cannot resolve {hostname}: {e}"), self.span.clone());
        let Some((deadline, message)) = &self.deadline else {
            return (self.lookup)(netloc).map_err(cannot);
        };
        let timeout = || SitefileError::new(E_FETCH_TIMEOUT, message.clone(), self.span.clone());
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let lookup = self.lookup;
        let owned = netloc.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(lookup(&owned));
        });
        match rx.recv_timeout(remaining) {
            Ok(r) => r.map_err(cannot),
            Err(_) => Err(timeout()),
        }
    }

    /// Resolve `host:port` (`[v6]:port` for IPv6) and apply the policy.
    pub fn resolve_checked(&self, netloc: &str) -> Result<Vec<SocketAddr>> {
        let hostname = match netloc.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => strip_brackets(h),
            _ => strip_brackets(netloc),
        };
        let addrs = self.lookup_bounded(netloc, &hostname)?;
        if addrs.is_empty() {
            return Err(SitefileError::new(E_FETCH, format!("no addresses found for {hostname}"), self.span.clone()));
        }
        if !self.allow_private_network {
            if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
                return Err(SitefileError::new(
                    E_SSRF_BLOCKED,
                    format!(
                        "refusing to connect to {hostname}: it resolves to non-public address {}; use --allow-private-network to permit it",
                        bad.ip()
                    ),
                    self.span.clone(),
                ));
            }
        }
        Ok(addrs)
    }

    fn take_error(&self) -> Option<SitefileError> {
        self.last_error.lock().ok().and_then(|mut g| g.take())
    }
}

impl ureq::Resolver for GuardedResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        self.resolve_checked(netloc).map_err(|e| {
            let msg = e.message();
            if let Ok(mut g) = self.last_error.lock() {
                *g = Some(e);
            }
            io::Error::other(msg)
        })
    }
}

// ------------------------------------------------------------------ TLS

/// DER certificates from PEM text (`-----BEGIN CERTIFICATE-----` blocks).
pub(crate) fn pem_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    use base64::Engine;
    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut rest = text.as_ref();
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    while let Some(b) = rest.find(BEGIN) {
        let after = &rest[b + BEGIN.len()..];
        let Some(e) = after.find(END) else { break };
        let body: String = after[..e].chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body) {
            out.push(der);
        }
        rest = &after[e + END.len()..];
    }
    out
}

/// Certificates from the PEM file named by `NODE_EXTRA_CA_CERTS`, read once.
///
/// The TypeScript CLI trusts them because Node adds that file to its default
/// CA store at startup; honoring the same variable keeps the two CLIs' trust
/// identical (enterprise private CAs, the conformance suite's test CA). Like
/// Node, a file that cannot be read is ignored with a warning.
pub(crate) fn node_extra_ca_certs() -> &'static [Vec<u8>] {
    static EXTRA: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    EXTRA.get_or_init(|| {
        let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS").filter(|p| !p.is_empty()) else { return Vec::new() };
        match std::fs::read(&path) {
            Ok(pem) => pem_certificates(&pem),
            Err(e) => {
                eprintln!("Warning: Ignoring extra certs from `{}`, load failed: {e}", path.to_string_lossy());
                Vec::new()
            }
        }
    })
}

/// TLS settings for a client: `None` (ureq's defaults) unless extra CAs are
/// configured through `NODE_EXTRA_CA_CERTS` or `ca`.
pub(crate) fn tls_config(ca: Option<&[u8]>, span: Option<&SourceSpan>) -> Result<Option<Arc<ureq::rustls::ClientConfig>>> {
    tls_config_from(node_extra_ca_certs(), ca, span)
}

/// The default roots plus `env_extra` (DER) and `ca` (PEM). The defaults are
/// the same Mozilla roots (webpki-roots) ureq uses when no extra CA is set, so
/// the extras are purely additive, as in Node (default store plus extra).
fn tls_config_from(env_extra: &[Vec<u8>], ca: Option<&[u8]>, span: Option<&SourceSpan>) -> Result<Option<Arc<ureq::rustls::ClientConfig>>> {
    use ureq::rustls;
    if ca.is_none() && env_extra.is_empty() {
        return Ok(None);
    }
    let fail = |why: String| SitefileError::new(E_FETCH, why, span.cloned());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots.add_parsable_certificates(env_extra.iter().cloned().map(rustls::pki_types::CertificateDer::from));
    if let Some(ca) = ca {
        let extra = pem_certificates(ca);
        if extra.is_empty() {
            return Err(fail("the extra CA certificate is not PEM (no 'BEGIN CERTIFICATE' block found)".into()));
        }
        for der in extra {
            roots.add(rustls::pki_types::CertificateDer::from(der)).map_err(|e| fail(format!("invalid extra CA certificate: {e}")))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| fail(format!("TLS setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Arc::new(config)))
}

// ------------------------------------------------------------------ fetch

fn source_is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    let mut depth = 0;
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            if matches!(io.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        depth += 1;
        if depth > 8 {
            break;
        }
        cur = err.source();
    }
    false
}

/// Fetch a remote resource under the given network policy. Redirects are
/// followed manually and every hop is re-validated; one deadline covers the
/// whole fetch.
pub fn fetch_remote(url: &str, policy: &FetchPolicy, span: Option<&SourceSpan>) -> Result<FetchResult> {
    fetch_remote_with(url, policy, span, &FetchRequest::default())
}

/// [`fetch_remote`] with a method, body and extra headers (see [`FetchRequest`]).
pub fn fetch_remote_with(url: &str, policy: &FetchPolicy, span: Option<&SourceSpan>, request: &FetchRequest) -> Result<FetchResult> {
    let method = request.method.unwrap_or("GET");
    let started = Instant::now();
    let budget = Duration::from_millis(policy.timeout_ms);
    let deadline = started + budget;
    let timeout_error =
        || SitefileError::new(E_FETCH_TIMEOUT, format!("fetching {} timed out after {} ms", redact_url(url), policy.timeout_ms), span.cloned());

    let mut current = checked_hop(url, policy, span)?;
    let origin = current.origin.clone();
    let resolver = GuardedResolver::new(policy, span).with_deadline(deadline, timeout_error().detail);
    let mut builder = ureq::AgentBuilder::new()
        .redirects(0)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .try_proxy_from_env(false)
        .user_agent(USER_AGENT)
        .resolver(resolver.clone());
    if let Some(config) = tls_config(policy.ca.as_deref(), span)? {
        builder = builder.tls_config(config);
    }
    let agent = builder.build();

    let mut redirects: u32 = 0;
    loop {
        validate(&current, policy, span)?;
        let now = Instant::now();
        if now >= deadline {
            return Err(timeout_error());
        }
        let mut req = agent
            .request(method, &current.href)
            .timeout(deadline - now)
            .set("accept-encoding", "identity")
            .set("user-agent", USER_AGENT)
            .set("accept", "*/*");
        for (k, v) in &request.headers {
            req = req.set(k, v);
        }
        // Credentials only ever go to the origin they were given for (not after a redirect elsewhere).
        if current.origin == origin {
            for (k, v) in &request.origin_headers {
                req = req.set(k, v);
            }
        }
        let sent = match &request.body {
            Some(b) => req.send_bytes(b),
            None => req.call(),
        };
        let res = match sent {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => {
                if let Some(e) = resolver.take_error() {
                    return Err(e);
                }
                if Instant::now() >= deadline || source_is_timeout(&t) {
                    return Err(timeout_error());
                }
                return Err(SitefileError::new(E_FETCH, format!("fetching {} failed: {t}", current.redacted), span.cloned()));
            }
        };
        let status = res.status();

        if REDIRECT_STATUSES.contains(&status) && method != "GET" {
            return Err(SitefileError::new(
                E_REDIRECT,
                format!("{method} {} was redirected (HTTP {status}); redirects are only followed for GET", current.redacted),
                span.cloned(),
            ));
        }
        if REDIRECT_STATUSES.contains(&status) {
            let location = res.header("location").map(str::to_string);
            drop(res);
            let Some(location) = location else {
                return Err(SitefileError::new(
                    E_REDIRECT,
                    format!("redirect {status} from {} has no Location header", current.redacted),
                    span.cloned(),
                ));
            };
            redirects += 1;
            if redirects > policy.max_redirects {
                return Err(SitefileError::new(
                    E_REDIRECT,
                    format!("too many redirects fetching {} (limit {})", redact_url(url), policy.max_redirects),
                    span.cloned(),
                ));
            }
            let base = ureq::get(&current.href);
            let joined = base.request_url().ok().and_then(|b| b.as_url().join(&location).ok()).map(|u| u.to_string());
            let Some(next) = joined else {
                return Err(SitefileError::new(E_REDIRECT, format!("invalid redirect Location from {}", current.redacted), span.cloned()));
            };
            current = checked_hop(&next, policy, span)?;
            continue;
        }

        if status != 200 {
            return Err(SitefileError::new(E_FETCH_STATUS, format!("{method} {} returned HTTP {status}", current.redacted), span.cloned()));
        }

        let encoding = res.header("content-encoding").unwrap_or("").trim().to_ascii_lowercase();
        if !encoding.is_empty() && encoding != "identity" {
            return Err(SitefileError::new(
                E_FETCH,
                format!("{} responded with content-encoding '{encoding}'; only identity is accepted", current.redacted),
                span.cloned(),
            ));
        }

        if let Some(h) = res.header("content-length") {
            // Number(header): blank is 0, junk is NaN (ignored).
            let t = h.trim();
            let declared = if t.is_empty() { Some(0.0) } else { t.parse::<f64>().ok() };
            if let Some(d) = declared.filter(|d| d.is_finite()) {
                if d > policy.max_bytes as f64 {
                    return Err(SitefileError::new(
                        E_TOO_LARGE,
                        format!("{} is {} bytes, exceeding the limit of {} bytes", current.redacted, crate::values::js_number(d), policy.max_bytes),
                        span.cloned(),
                    ));
                }
            }
        }

        let content_type = res.header("content-type").map(str::to_string);
        let mut reader = res.into_reader();
        let mut bytes: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if (bytes.len() + n) as u64 > policy.max_bytes {
                        return Err(SitefileError::new(
                            E_TOO_LARGE,
                            format!("{} exceeded the limit of {} bytes", current.redacted, policy.max_bytes),
                            span.cloned(),
                        ));
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if Instant::now() >= deadline || source_is_timeout(&e) {
                        return Err(timeout_error());
                    }
                    return Err(SitefileError::new(E_FETCH, format!("error reading {}: {e}", current.redacted), span.cloned()));
                }
            }
        }
        if Instant::now() > deadline {
            return Err(timeout_error());
        }
        let media_type = content_type
            .and_then(|ct| ct.split(';').next().map(|s| s.trim().to_ascii_lowercase()))
            .filter(|s| !s.is_empty());
        return Ok(FetchResult { bytes, final_url: current.href, media_type });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::resources::types::DEFAULT_FETCH_POLICY;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn local() -> FetchPolicy {
        FetchPolicy { allow_http: true, allow_private_network: true, ..DEFAULT_FETCH_POLICY }
    }

    #[derive(Debug, Clone, Default)]
    struct Req {
        path: String,
        headers: HashMap<String, String>,
    }

    type Handler = dyn Fn(&Req, &mut TcpStream, &str) + Send + Sync;

    /// A one-thread-per-connection HTTP/1.1 server on 127.0.0.1. The handler
    /// writes the raw response; returning without writing keeps nothing open
    /// unless the handler blocks.
    fn serve(handler: impl Fn(&Req, &mut TcpStream, &str) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let h: Arc<Handler> = Arc::new(handler);
        let b = base.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let h = h.clone();
                let b = b.clone();
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut headers = HashMap::new();
                    loop {
                        let mut l = String::new();
                        if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = l.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    h(&Req { path, headers }, &mut stream, &b);
                });
            }
        });
        base
    }

    fn respond(s: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &str) {
        let mut out = format!("HTTP/1.1 {status} X\r\nconnection: close\r\ncontent-length: {}\r\n", body.len());
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(body);
        let _ = s.write_all(out.as_bytes());
    }

    fn redirect(s: &mut TcpStream, status: u16, location: &str) {
        respond(s, status, &[("location", location)], "");
    }

    fn port_of(base: &str) -> &str {
        base.rsplit(':').next().unwrap()
    }

    fn code(r: Result<FetchResult>) -> ErrorCode {
        let e = r.unwrap_err();
        eprintln!("{}: {}", e.code, e.message());
        e.code
    }

    #[test]
    fn fetches_a_body_with_headers_and_media_type() {
        let seen: Arc<Mutex<Option<Req>>> = Arc::new(Mutex::new(None));
        let s2 = seen.clone();
        let base = serve(move |req, s, _| {
            *s2.lock().unwrap() = Some(req.clone());
            respond(s, 200, &[("content-type", "Text/Plain; charset=utf-8")], "hello");
        });
        let r = fetch_remote(&format!("{base}/a.txt"), &local(), None).unwrap();
        assert_eq!(r.bytes, b"hello");
        assert_eq!(r.final_url, format!("{base}/a.txt"));
        assert_eq!(r.media_type.as_deref(), Some("text/plain"));
        let req = seen.lock().unwrap().clone().unwrap();
        assert_eq!(req.headers.get("accept-encoding").map(String::as_str), Some("identity"));
        assert_eq!(req.headers.get("user-agent").map(String::as_str), Some("sitefile/0.1"));
        assert!(!req.headers.contains_key("cookie"));
        assert!(!req.headers.contains_key("authorization"));
    }

    #[test]
    fn http_is_rejected_by_default() {
        let base = serve(|_, s, _| respond(s, 200, &[], "x"));
        assert_eq!(code(fetch_remote(&format!("{base}/"), &DEFAULT_FETCH_POLICY, None)), E_PROTOCOL);
    }

    #[test]
    fn unsupported_schemes_are_rejected() {
        assert_eq!(code(fetch_remote("ftp://example.com/x", &local(), None)), E_PROTOCOL);
        assert_eq!(code(fetch_remote("file:///etc/passwd", &local(), None)), E_PROTOCOL);
        assert_eq!(code(fetch_remote("not a url", &local(), None)), E_PROTOCOL);
        assert_eq!(code(fetch_remote("https://", &local(), None)), E_PROTOCOL);
        let e = fetch_remote("http://example.com/", &DEFAULT_FETCH_POLICY, None).unwrap_err();
        assert!(e.message().contains("requires --allow-http"), "{}", e.message());
    }

    #[test]
    fn loopback_is_blocked_by_default_even_with_allow_http() {
        let base = serve(|_, s, _| respond(s, 200, &[], "x"));
        let port = port_of(&base);
        let policy = FetchPolicy { allow_http: true, ..DEFAULT_FETCH_POLICY };
        assert_eq!(code(fetch_remote(&format!("http://127.0.0.1:{port}/"), &policy, None)), E_SSRF_BLOCKED);
        assert_eq!(code(fetch_remote(&format!("http://localhost:{port}/"), &policy, None)), E_SSRF_BLOCKED);
        assert_eq!(code(fetch_remote(&format!("http://[::1]:{port}/"), &policy, None)), E_SSRF_BLOCKED);
        assert_eq!(code(fetch_remote(&format!("https://localhost:{port}/"), &DEFAULT_FETCH_POLICY, None)), E_SSRF_BLOCKED);
    }

    #[test]
    fn localhost_by_name_works_when_private_network_is_allowed() {
        let base = serve(|_, s, _| respond(s, 200, &[], "named"));
        // localhost may resolve to ::1 first; only assert we did not get an SSRF block.
        match fetch_remote(&format!("http://localhost:{}/", port_of(&base)), &local(), None) {
            Ok(r) => assert_eq!(r.bytes, b"named"),
            Err(e) => assert_ne!(e.code, E_SSRF_BLOCKED, "{}", e.message()),
        }
    }

    #[test]
    fn credentials_in_urls_are_rejected_without_leaking_them() {
        let base = serve(|_, s, _| respond(s, 200, &[], "x"));
        let url = base.replace("http://", "http://user:hunter2@");
        let e = fetch_remote(&url, &local(), None).unwrap_err();
        assert_eq!(e.code, E_PROTOCOL);
        assert!(!e.message().contains("hunter2"), "{}", e.message());
        assert!(!redact_url(&url).contains("hunter2"));
    }

    #[test]
    fn redirects_are_followed_and_final_url_reported() {
        let base = serve(|req, s, b| match req.path.as_str() {
            "/start" => redirect(s, 302, "/middle"),
            "/middle" => redirect(s, 308, &format!("{b}/end")),
            _ => respond(s, 200, &[], "done"),
        });
        let r = fetch_remote(&format!("{base}/start"), &local(), None).unwrap();
        assert_eq!(r.bytes, b"done");
        assert_eq!(r.final_url, format!("{base}/end"));
    }

    #[test]
    fn redirect_loops_and_too_many_redirects_fail() {
        let base = serve(|req, s, _| {
            if req.path == "/loop" {
                return redirect(s, 301, "/loop");
            }
            let n: u32 = req.path[1..].parse().unwrap_or(0);
            if n > 0 {
                redirect(s, 307, &format!("/{}", n - 1))
            } else {
                respond(s, 200, &[], "ok")
            }
        });
        assert_eq!(code(fetch_remote(&format!("{base}/loop"), &local(), None)), E_REDIRECT);
        let two = FetchPolicy { max_redirects: 2, ..local() };
        assert_eq!(code(fetch_remote(&format!("{base}/3"), &two, None)), E_REDIRECT);
        assert_eq!(fetch_remote(&format!("{base}/2"), &two, None).unwrap().bytes, b"ok");
    }

    #[test]
    fn redirect_without_location_fails() {
        let base = serve(|_, s, _| respond(s, 302, &[], ""));
        assert_eq!(code(fetch_remote(&format!("{base}/"), &local(), None)), E_REDIRECT);
    }

    #[test]
    fn every_redirect_hop_is_revalidated() {
        let base = serve(|req, s, _| {
            let target = match req.path.as_str() {
                "/ftp" => "ftp://example.com/file",
                "/creds" => "http://u:p@127.0.0.1/",
                _ => "/",
            };
            redirect(s, 302, target)
        });
        assert_eq!(code(fetch_remote(&format!("{base}/ftp"), &local(), None)), E_PROTOCOL);
        assert_eq!(code(fetch_remote(&format!("{base}/creds"), &local(), None)), E_PROTOCOL);
    }

    #[test]
    fn validate_hop_private_targets_and_downgrades() {
        let p = &DEFAULT_FETCH_POLICY;
        let c = |u: &str, p: &FetchPolicy| validate_hop(u, p, None).map_err(|e| e.code);
        assert_eq!(c("http://example.com/", p), Err(E_PROTOCOL));
        assert_eq!(c("https://10.255.255.1/", p), Err(E_SSRF_BLOCKED));
        assert_eq!(c("https://169.254.169.254/", p), Err(E_SSRF_BLOCKED));
        assert_eq!(c("https://[::ffff:127.0.0.1]/", p), Err(E_SSRF_BLOCKED));
        assert_eq!(c("https://[fd00::1]/", p), Err(E_SSRF_BLOCKED));
        // Decimal / hex IPv4 forms are canonicalized by the URL parser.
        assert_eq!(c("https://2130706433/", p), Err(E_SSRF_BLOCKED));
        assert_eq!(c("https://0x7f.1/", p), Err(E_SSRF_BLOCKED));
        assert_eq!(c("https://93.184.216.34/", p), Ok(()));
        assert_eq!(c("https://example.com/", p), Ok(()));
        assert_eq!(c("https://10.255.255.1/", &FetchPolicy { allow_private_network: true, ..DEFAULT_FETCH_POLICY }), Ok(()));
    }

    #[test]
    fn dns_lookup_is_bounded_by_the_fetch_timeout() {
        fn slow(_: &str) -> io::Result<Vec<SocketAddr>> {
            std::thread::sleep(Duration::from_secs(3));
            Ok(vec!["93.184.216.34:443".parse().unwrap()])
        }
        let msg = "fetching https://slow.test/ timed out after 150 ms".to_string();
        let mut r = GuardedResolver::new(&DEFAULT_FETCH_POLICY, None).with_deadline(Instant::now() + Duration::from_millis(150), msg.clone());
        r.lookup = slow;
        let t = Instant::now();
        let e = r.resolve_checked("slow.test:443").unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(2), "lookup was not abandoned");
        assert_eq!(e.code, E_FETCH_TIMEOUT);
        assert_eq!(e.detail, msg);
        // Through the ureq trait the error is kept for the fetch to report.
        assert!(ureq::Resolver::resolve(&r, "slow.test:443").is_err());
        assert_eq!(r.take_error().map(|e| e.code), Some(E_FETCH_TIMEOUT));
    }

    #[test]
    fn guarded_resolver_blocks_names_resolving_to_private_addresses() {
        let guarded = GuardedResolver::new(&DEFAULT_FETCH_POLICY, None);
        assert_eq!(guarded.resolve_checked("localhost:80").unwrap_err().code, E_SSRF_BLOCKED);
        // Through the ureq trait the error is kept for the fetch to report.
        assert!(ureq::Resolver::resolve(&guarded, "localhost:80").is_err());
        assert_eq!(guarded.take_error().map(|e| e.code), Some(E_SSRF_BLOCKED));
        let open = GuardedResolver::new(&local(), None);
        let addrs = open.resolve_checked("localhost:80").unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.port() == 80));
    }

    #[test]
    fn non_200_status_fails() {
        let base = serve(|_, s, _| respond(s, 404, &[], "nope"));
        assert_eq!(code(fetch_remote(&format!("{base}/missing"), &local(), None)), E_FETCH_STATUS);
        let base = serve(|_, s, _| respond(s, 204, &[], ""));
        assert_eq!(code(fetch_remote(&format!("{base}/"), &local(), None)), E_FETCH_STATUS);
    }

    #[test]
    fn content_length_over_the_cap_fails_before_reading() {
        let base = serve(|_, s, _| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\nxxxxxxxxxx");
            thread::sleep(Duration::from_secs(3)); // never finish
        });
        let t0 = Instant::now();
        let p = FetchPolicy { max_bytes: 100, timeout_ms: 5000, ..local() };
        assert_eq!(code(fetch_remote(&format!("{base}/"), &p, None)), E_TOO_LARGE);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    fn chunked(s: &mut TcpStream, chunks: &[String], finish: bool) {
        let mut out = String::from("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
        for c in chunks {
            out.push_str(&format!("{:x}\r\n{c}\r\n", c.len()));
        }
        if finish {
            out.push_str("0\r\n\r\n");
        }
        let _ = s.write_all(out.as_bytes());
    }

    #[test]
    fn streamed_body_over_the_cap_fails() {
        let base = serve(|_, s, _| chunked(s, &["x".repeat(64), "x".repeat(64), "x".repeat(64)], true));
        assert_eq!(code(fetch_remote(&format!("{base}/"), &FetchPolicy { max_bytes: 100, ..local() }, None)), E_TOO_LARGE);
        let r = fetch_remote(&format!("{base}/"), &FetchPolicy { max_bytes: 192, ..local() }, None).unwrap();
        assert_eq!(r.bytes.len(), 192);
    }

    #[test]
    fn overall_timeout_fires_for_a_hanging_server() {
        let base = serve(|_, _, _| thread::sleep(Duration::from_secs(3)));
        let t0 = Instant::now();
        assert_eq!(code(fetch_remote(&format!("{base}/"), &FetchPolicy { timeout_ms: 150, ..local() }, None)), E_FETCH_TIMEOUT);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn timeout_also_covers_a_stalled_body() {
        let base = serve(|_, s, _| {
            chunked(s, &["partial".to_string()], false);
            thread::sleep(Duration::from_secs(3));
        });
        let t0 = Instant::now();
        assert_eq!(code(fetch_remote(&format!("{base}/"), &FetchPolicy { timeout_ms: 150, ..local() }, None)), E_FETCH_TIMEOUT);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn timeout_covers_redirect_chains() {
        let base = serve(|req, s, _| {
            thread::sleep(Duration::from_millis(80));
            let n: u32 = req.path[1..].parse().unwrap_or(0);
            redirect(s, 302, &format!("/{}", n + 1))
        });
        let p = FetchPolicy { timeout_ms: 200, max_redirects: 50, ..local() };
        assert_eq!(code(fetch_remote(&format!("{base}/0"), &p, None)), E_FETCH_TIMEOUT);
    }

    #[test]
    fn compressed_content_encoding_is_rejected() {
        let base = serve(|_, s, _| respond(s, 200, &[("content-encoding", "gzip")], "not really gzip"));
        assert_eq!(code(fetch_remote(&format!("{base}/"), &local(), None)), E_FETCH);
        let base = serve(|_, s, _| respond(s, 200, &[("content-encoding", " Identity ")], "plain"));
        assert_eq!(fetch_remote(&format!("{base}/"), &local(), None).unwrap().bytes, b"plain");
    }

    #[test]
    fn connection_errors_become_e_fetch() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert_eq!(code(fetch_remote(&format!("http://127.0.0.1:{port}/"), &local(), None)), E_FETCH);
    }

    #[test]
    fn extra_ca_must_be_pem() {
        let p = FetchPolicy { ca: Some(b"not a certificate".to_vec()), ..DEFAULT_FETCH_POLICY };
        assert_eq!(code(fetch_remote("https://example.com/", &p, None)), E_FETCH);
        assert!(pem_certificates(b"x\n-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n").first() == Some(&vec![0, 1, 2]));
    }

    #[test]
    fn tls_defaults_unless_an_extra_ca_is_configured() {
        assert!(tls_config_from(&[], None, None).unwrap().is_none());
        // NODE_EXTRA_CA_CERTS entries that do not parse are skipped (as Node does), not fatal.
        assert!(tls_config_from(&[vec![0, 1, 2]], None, None).unwrap().is_some());
        assert_eq!(tls_config_from(&[vec![0, 1, 2]], Some(b"not pem"), None).err().map(|e| e.code), Some(E_FETCH));
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_url("https://u:p@example.com/a?b"), "https://example.com/a?b");
        assert_eq!(redact_url("ftp://u:p@example.com/a"), "ftp://example.com/a");
        assert_eq!(redact_url("not a url"), "<invalid url>");
    }
}
