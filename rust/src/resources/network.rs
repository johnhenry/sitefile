//! Explicit network settings: proxy, host rules, connect/read timeouts (port of
//! `src/resources/network.ts`).
//!
//! Nothing here is read from the environment implicitly: `HTTP(S)_PROXY` stays
//! ignored, and the CLI reads `SITEFILE_PROXY` and friends through
//! [`network_from_env`]. Embedders set [`NetworkSettings`] on
//! `FetchPolicy::network` and `RegistryOptions::network`.

use crate::error::{ErrorCode::*, Needs, Result, SitefileError, SourceSpan};
use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Proxy, host rules and connect/read timeouts for everything sitefile fetches.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct NetworkSettings {
    /// `http://[user:password@]host[:port]`: a CONNECT tunnel for https targets,
    /// absolute-form requests for http.
    pub proxy: Option<String>,
    /// Hosts reached directly, never through the proxy (`NO_PROXY` syntax, see [`host_matches`]).
    pub no_proxy: Vec<String>,
    /// When non-empty, only hosts matching one of these patterns may be contacted.
    pub allow_hosts: Vec<String>,
    /// Hosts never contacted; a deny rule beats an allow rule.
    pub deny_hosts: Vec<String>,
    /// Strict network mode (`--network strict`, `SITEFILE_NETWORK=strict`):
    /// every host must match an allow-hosts rule, also when there is none (so
    /// nothing is contacted), and an `IMPORT … SECRET` needs a
    /// [`secret_hosts`](Self::secret_hosts) rule for its host.
    pub strict: bool,
    /// `<secret id>=<host pattern>` (`--allow-secret-host`): where, in strict
    /// mode, that secret may be sent.
    pub secret_hosts: Vec<String>,
    /// Time limit for establishing the TCP connection (to the target, or to the proxy), ms.
    pub connect_timeout_ms: Option<u64>,
    /// Longest wait for data once connected (idle time), ms.
    pub read_timeout_ms: Option<u64>,
}

impl NetworkSettings {
    /// No proxy, no host rules, no connect/read timeouts (the default).
    pub const NONE: NetworkSettings = NetworkSettings {
        proxy: None,
        no_proxy: Vec::new(),
        allow_hosts: Vec::new(),
        deny_hosts: Vec::new(),
        strict: false,
        secret_hosts: Vec::new(),
        connect_timeout_ms: None,
        read_timeout_ms: None,
    };

    /// True when any setting is present.
    pub fn is_set(&self) -> bool {
        self != &NetworkSettings::NONE
    }
}

impl fmt::Debug for NetworkSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print proxy credentials.
        f.debug_struct("NetworkSettings")
            .field("proxy", &self.proxy.as_deref().map(shown_proxy))
            .field("no_proxy", &self.no_proxy)
            .field("allow_hosts", &self.allow_hosts)
            .field("deny_hosts", &self.deny_hosts)
            .field("strict", &self.strict)
            .field("secret_hosts", &self.secret_hosts)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("read_timeout_ms", &self.read_timeout_ms)
            .finish()
    }
}

/// A configured HTTP proxy, parsed.
#[derive(Clone)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    /// The proxy URL without credentials, for messages.
    pub shown: String,
}

impl ProxyConfig {
    /// `Basic …` when the URL carries credentials. ureq sends it itself only on
    /// CONNECT; absolute-form (http) requests need it set as a header.
    pub fn authorization(&self) -> Option<String> {
        use base64::Engine;
        if self.user.is_none() && self.password.is_none() {
            return None;
        }
        let pair = format!(
            "{}:{}",
            self.user.as_deref().unwrap_or(""),
            self.password.as_deref().unwrap_or("")
        );
        Some(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        ))
    }

    /// The proxy for ureq (credentials decoded, as the TypeScript client sends them).
    pub(crate) fn ureq(&self) -> Result<ureq::Proxy> {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let creds = match (&self.user, &self.password) {
            (None, None) => String::new(),
            (u, p) => format!(
                "{}:{}@",
                u.as_deref().unwrap_or(""),
                p.as_deref().unwrap_or("")
            ),
        };
        ureq::Proxy::new(format!("http://{creds}{host}:{}", self.port)).map_err(|_| {
            SitefileError::plain(
                E_USAGE,
                format!(
                    "invalid proxy URL '{}': expected http://[user:password@]host[:port]",
                    self.shown
                ),
            )
        })
    }
}

/// Split a list given as one string (commas and/or whitespace).
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The parts of a URL a proxy setting needs (WHATWG parser, via ureq).
struct ProxyUrl {
    scheme: String,
    host: String,
    port: Option<u16>,
    username: String,
    password: String,
    path: String,
    query: String,
    fragment: String,
    /// Without credentials or a trailing `/`.
    shown: String,
}

fn parse_url(raw: &str) -> Option<ProxyUrl> {
    let req = ureq::get(raw);
    let ru = req.request_url().ok()?;
    let u = ru.as_url();
    let mut clean = u.clone();
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    let s = clean.to_string();
    Some(ProxyUrl {
        scheme: u.scheme().to_string(),
        host: u.host_str().unwrap_or("").to_string(),
        port: u.port(),
        username: u.username().to_string(),
        password: u.password().unwrap_or("").to_string(),
        path: u.path().to_string(),
        query: u.query().unwrap_or("").to_string(),
        fragment: u.fragment().unwrap_or("").to_string(),
        shown: s.strip_suffix('/').map(str::to_string).unwrap_or(s),
    })
}

/// A proxy URL with any credentials removed (`<invalid url>` unless it is an http(s) URL).
fn shown_proxy(raw: &str) -> String {
    parse_url(raw)
        .map(|u| u.shown)
        .unwrap_or_else(|| "<invalid url>".into())
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// `decodeURIComponent`, keeping invalid escapes as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a proxy URL, or fail with E_USAGE. Credentials never appear in the message.
pub fn parse_proxy(raw: &str) -> Result<ProxyConfig> {
    let shown = shown_proxy(raw);
    let bad = || {
        SitefileError::plain(
            E_USAGE,
            format!("invalid proxy URL '{shown}': expected http://[user:password@]host[:port]"),
        )
    };
    let u = parse_url(raw).ok_or_else(bad)?;
    if u.scheme != "http"
        || u.host.is_empty()
        || (u.path != "/" && !u.path.is_empty())
        || !u.query.is_empty()
        || !u.fragment.is_empty()
    {
        return Err(bad());
    }
    let host = u
        .host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&u.host)
        .to_string();
    let user = percent_decode(&u.username);
    let password = percent_decode(&u.password);
    let has = !user.is_empty() || !password.is_empty();
    Ok(ProxyConfig {
        host,
        port: u.port.unwrap_or(80),
        user: has.then(|| user.clone()),
        password: has.then(|| password.clone()),
        shown,
    })
}

// ------------------------------------------------------------------ host patterns

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

#[derive(Debug, Clone)]
enum Kind {
    Any,
    Domain(String),
    Net(Family, u128, u32),
}

#[derive(Debug, Clone)]
struct Pattern {
    kind: Kind,
    port: Option<u16>,
}

fn ip_to_big(ip: &str) -> Option<(Family, u128)> {
    if let Ok(a) = ip.parse::<Ipv4Addr>() {
        return Some((Family::V4, u32::from(a) as u128));
    }
    if let Ok(a) = ip.parse::<Ipv6Addr>() {
        return Some((Family::V6, u128::from(a)));
    }
    None
}

fn hostname_ok(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
}

/// Parse one pattern; None when it is not valid.
fn parse_pattern(raw: &str) -> Option<Pattern> {
    let mut p = raw.trim().to_ascii_lowercase();
    if p.is_empty() {
        return None;
    }
    if p == "*" {
        return Some(Pattern {
            kind: Kind::Any,
            port: None,
        });
    }
    let mut port: Option<u32> = None;
    if let Some(rest) = p.strip_prefix('[') {
        let close = rest.find(']')?;
        let inner = rest[..close].to_string();
        let after = &rest[close + 1..];
        if !after.is_empty() {
            let ps = after.strip_prefix(':')?;
            if ps.is_empty() || !ps.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            port = Some(ps.parse().ok()?);
        }
        p = inner;
    } else if p.matches(':').count() == 1 {
        let i = p.find(':')?;
        let ps = p[i + 1..].to_string();
        if ps.is_empty() || !ps.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        port = Some(ps.parse().unwrap_or(u32::MAX));
        p.truncate(i);
    }
    if let Some(n) = port {
        if !(1..=65535).contains(&n) {
            return None;
        }
    }
    let port = port.map(|n| n as u16);
    if let Some(slash) = p.find('/') {
        let (fam, addr) = ip_to_big(&p[..slash])?;
        let ps = &p[slash + 1..];
        if ps.is_empty() || !ps.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let prefix: u32 = ps.parse().ok()?;
        if prefix > if fam == Family::V4 { 32 } else { 128 } {
            return None;
        }
        return Some(Pattern {
            kind: Kind::Net(fam, addr, prefix),
            port,
        });
    }
    if let Some((fam, addr)) = ip_to_big(&p) {
        let prefix = if fam == Family::V4 { 32 } else { 128 };
        return Some(Pattern {
            kind: Kind::Net(fam, addr, prefix),
            port,
        });
    }
    let mut d = p.as_str();
    if let Some(r) = d.strip_prefix("*.") {
        d = r;
    } else if let Some(r) = d.strip_prefix('.') {
        d = r;
    }
    let d = d.strip_suffix('.').unwrap_or(d);
    if !hostname_ok(d) {
        return None;
    }
    Some(Pattern {
        kind: Kind::Domain(d.to_string()),
        port,
    })
}

fn normalize_host(host: &str) -> String {
    let h = host.to_ascii_lowercase();
    let h = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .map(str::to_string)
        .unwrap_or(h);
    h.strip_suffix('.').map(str::to_string).unwrap_or(h)
}

fn matches(host: &str, port: u16, p: &Pattern) -> bool {
    if let Kind::Any = p.kind {
        return true;
    }
    if p.port.is_some_and(|q| q != port) {
        return false;
    }
    match &p.kind {
        Kind::Any => true,
        Kind::Domain(d) => host == d || host.ends_with(&format!(".{d}")),
        Kind::Net(fam, addr, prefix) => {
            let Some((f, a)) = ip_to_big(host) else {
                return false;
            };
            if f != *fam {
                return false;
            }
            let bits = if f == Family::V4 { 32 } else { 128 };
            let shift = bits - prefix;
            if shift >= 128 {
                return true;
            }
            (a >> shift) == (addr >> shift)
        }
    }
}

/// True when `host` (as in a URL, IPv6 without brackets) on `port` matches `pattern`:
/// `*` (every host); `example.com` (it and every subdomain; `.example.com` and
/// `*.example.com` too); `10.0.0.0/8`, `fd00::/8` (IP-literal hosts in that
/// network) or `192.0.2.1`, `::1` (that address); any of these plus `:port`
/// (`[::1]:8080` for IPv6). A name pattern never matches an IP literal and an IP
/// pattern never matches a name, whatever it resolves to.
pub fn host_matches(host: &str, port: u16, pattern: &str) -> bool {
    parse_pattern(pattern).is_some_and(|p| matches(&normalize_host(host), port, &p))
}

fn first_match<'a>(host: &str, port: u16, list: &'a [String]) -> Option<&'a str> {
    let h = normalize_host(host);
    list.iter()
        .find(|raw| parse_pattern(raw).is_some_and(|p| matches(&h, port, &p)))
        .map(|s| s.trim())
}

/// Apply the allow/deny host rules to one hop (`host` without brackets).
/// E_HOST_BLOCKED names the rule; a deny rule beats an allow rule.
pub fn check_host_rules(
    host: &str,
    port: u16,
    network: &NetworkSettings,
    shown: &str,
    span: Option<&SourceSpan>,
) -> Result<()> {
    let host = normalize_host(host);
    if let Some(rule) = first_match(&host, port, &network.deny_hosts) {
        return Err(SitefileError::new(
            E_HOST_BLOCKED,
            format!("refusing to connect to {host}: it matches deny-hosts rule '{rule}' ({shown})"),
            span.cloned(),
        ));
    }
    if (network.strict || !network.allow_hosts.is_empty())
        && first_match(&host, port, &network.allow_hosts).is_none()
    {
        let mode = if network.strict {
            " (strict network mode)"
        } else {
            ""
        };
        return Err(SitefileError::new(
            E_HOST_BLOCKED,
            format!("refusing to connect to {host}: it matches no allow-hosts rule{mode} ({shown}); add --allow-hosts {host}"),
            span.cloned(),
        )
        .with_needs(Needs::host(host)));
    }
    Ok(())
}

/// True when `host:port` may be contacted under `network`'s host rules (see [`check_host_rules`]).
pub fn host_allowed(host: &str, port: u16, network: &NetworkSettings) -> bool {
    let h = normalize_host(host);
    if first_match(&h, port, &network.deny_hosts).is_some() {
        return false;
    }
    !(network.strict || !network.allow_hosts.is_empty())
        || first_match(&h, port, &network.allow_hosts).is_some()
}

/// Split `<id>=<pattern>`; None when malformed.
fn secret_host_rule(raw: &str) -> Option<(String, String)> {
    let (id, pattern) = raw.split_once('=')?;
    let (id, pattern) = (id.trim(), pattern.trim());
    let mut chars = id.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    parse_pattern(pattern)?;
    Some((id.to_string(), pattern.to_string()))
}

/// Before an `IMPORT … SECRET` request to `target` (the import's own origin,
/// the only one its secrets are sent to): in strict network mode each secret
/// needs an `--allow-secret-host <id>=<pattern>` that matches the host.
/// E_HOST_BLOCKED with a `needs`. Nothing is checked outside strict mode, and
/// a host the host rules refuse is left to the request itself (its
/// E_HOST_BLOCKED asks for `--allow-hosts` first).
pub fn check_secret_hosts(
    import_name: &str,
    secret_ids: &[&str],
    target: &str,
    network: &NetworkSettings,
    span: Option<&SourceSpan>,
) -> Result<()> {
    if !network.strict || secret_ids.is_empty() {
        return Ok(());
    }
    let Some((host, port)) = crate::resources::https::host_port(target) else {
        return Ok(()); // the request itself reports the bad URL
    };
    let host = normalize_host(&host);
    if !host_allowed(&host, port, network) {
        return Ok(());
    }
    let rules: Vec<(String, String)> = network
        .secret_hosts
        .iter()
        .filter_map(|r| secret_host_rule(r))
        .collect();
    for id in secret_ids {
        if rules
            .iter()
            .any(|(rid, p)| rid == id && host_matches(&host, port, p))
        {
            continue;
        }
        return Err(SitefileError::new(
            E_HOST_BLOCKED,
            format!("IMPORT {import_name}: refusing to send secret '{id}' to {host} (strict network mode); add --allow-secret-host {id}={host}"),
            span.cloned(),
        )
        .with_needs(Needs::secret_host(id, &host)));
    }
    Ok(())
}

/// The proxy for `host:port`, or None for a direct connection (no proxy, or a no-proxy match).
pub fn proxy_for(host: &str, port: u16, network: &NetworkSettings) -> Result<Option<ProxyConfig>> {
    let Some(proxy) = network.proxy.as_deref().filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if first_match(host, port, &network.no_proxy).is_some() {
        return Ok(None);
    }
    parse_proxy(proxy).map(Some)
}

// ------------------------------------------------------------------ configuration

/// A usage problem in `network` (bad proxy URL, host pattern or timeout), or None.
pub fn network_problem(network: &NetworkSettings) -> Option<String> {
    if let Some(p) = network.proxy.as_deref().filter(|p| !p.is_empty()) {
        if let Err(e) = parse_proxy(p) {
            return Some(e.detail);
        }
    }
    for (name, list) in [
        ("no-proxy", &network.no_proxy),
        ("allow-hosts", &network.allow_hosts),
        ("deny-hosts", &network.deny_hosts),
    ] {
        for p in list {
            if parse_pattern(p).is_none() {
                return Some(format!("invalid {name} pattern '{p}': expected *, a host name (example.com, .example.com, *.example.com), an IP address or a CIDR network, optionally with :port"));
            }
        }
    }
    for r in &network.secret_hosts {
        if secret_host_rule(r).is_none() {
            return Some(format!("invalid --allow-secret-host '{r}': expected <secret id>=<host pattern> (a pattern as for --allow-hosts)"));
        }
    }
    for (name, v) in [
        ("connect timeout", network.connect_timeout_ms),
        ("read timeout", network.read_timeout_ms),
    ] {
        if v == Some(0) {
            return Some(format!("the {name} must be a positive integer (ms)"));
        }
    }
    None
}

fn positive_ms(raw: &str, what: &str) -> Result<u64> {
    let ok = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
    match raw.parse::<u64>() {
        Ok(n) if ok && n > 0 && n <= 9_007_199_254_740_991 => Ok(n),
        _ => Err(SitefileError::plain(
            E_USAGE,
            format!("{what} expects a positive integer (ms), got '{raw}'"),
        )),
    }
}

/// Parse `SITEFILE_FETCH_TIMEOUT`: `<ms>` (connect and read) or
/// `connect=<ms>,read=<ms>` (either part optional). Returns (connect, read).
pub fn parse_fetch_timeout(raw: &str) -> Result<(Option<u64>, Option<u64>)> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok((None, None));
    }
    if s.bytes().all(|b| b.is_ascii_digit()) {
        let ms = positive_ms(s, "SITEFILE_FETCH_TIMEOUT")?;
        return Ok((Some(ms), Some(ms)));
    }
    let (mut connect, mut read) = (None, None);
    for part in s.split(',') {
        let bad = || {
            SitefileError::plain(
                E_USAGE,
                format!(
                    "SITEFILE_FETCH_TIMEOUT expects <ms> or connect=<ms>,read=<ms>, got '{raw}'"
                ),
            )
        };
        let (k, v) = part.split_once('=').ok_or_else(bad)?;
        let (k, v) = (k.trim(), v.trim());
        if v.chars().any(char::is_whitespace) {
            return Err(bad());
        }
        match k {
            "connect" => connect = Some(positive_ms(v, "SITEFILE_FETCH_TIMEOUT connect")?),
            "read" => read = Some(positive_ms(v, "SITEFILE_FETCH_TIMEOUT read")?),
            _ => return Err(bad()),
        }
    }
    Ok((connect, read))
}

/// Parse a network mode (`open`: false, `strict`: true; empty: None); `what`
/// names the source in the E_USAGE message.
pub fn parse_network_mode(raw: &str, what: &str) -> Result<Option<bool>> {
    match raw.trim() {
        "" => Ok(None),
        "open" => Ok(Some(false)),
        "strict" => Ok(Some(true)),
        _ => Err(SitefileError::plain(
            E_USAGE,
            format!("{what} expects open or strict, got '{raw}'"),
        )),
    }
}

/// Network settings from `SITEFILE_PROXY`, `SITEFILE_NO_PROXY`,
/// `SITEFILE_ALLOW_HOSTS`, `SITEFILE_DENY_HOSTS` (comma- or space-separated),
/// `SITEFILE_FETCH_TIMEOUT` and `SITEFILE_NETWORK` (`open` or `strict`).
/// Empty values are unset. `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` are never read.
pub fn network_from_env(env: &BTreeMap<String, String>) -> Result<NetworkSettings> {
    let get = |k: &str| env.get(k).map(String::as_str).unwrap_or("");
    let proxy = get("SITEFILE_PROXY").trim();
    let (connect, read) = parse_fetch_timeout(get("SITEFILE_FETCH_TIMEOUT"))?;
    let strict = parse_network_mode(get("SITEFILE_NETWORK"), "SITEFILE_NETWORK")?.unwrap_or(false);
    Ok(NetworkSettings {
        proxy: (!proxy.is_empty()).then(|| proxy.to_string()),
        no_proxy: split_list(get("SITEFILE_NO_PROXY")),
        allow_hosts: split_list(get("SITEFILE_ALLOW_HOSTS")),
        deny_hosts: split_list(get("SITEFILE_DENY_HOSTS")),
        strict,
        secret_hosts: Vec::new(),
        connect_timeout_ms: connect,
        read_timeout_ms: read,
    })
}

/// `over` on top of `base`: the proxy and timeouts are replaced, the host lists
/// are joined (duplicates dropped, first occurrence kept), and strict mode is
/// on when either is strict.
pub fn merge_network(base: &NetworkSettings, over: &NetworkSettings) -> NetworkSettings {
    let join = |a: &[String], b: &[String]| {
        let mut out: Vec<String> = Vec::new();
        for s in a.iter().chain(b) {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
        out
    };
    NetworkSettings {
        proxy: over.proxy.clone().or_else(|| base.proxy.clone()),
        no_proxy: join(&base.no_proxy, &over.no_proxy),
        allow_hosts: join(&base.allow_hosts, &over.allow_hosts),
        deny_hosts: join(&base.deny_hosts, &over.deny_hosts),
        strict: base.strict || over.strict,
        secret_hosts: join(&base.secret_hosts, &over.secret_hosts),
        connect_timeout_ms: over.connect_timeout_ms.or(base.connect_timeout_ms),
        read_timeout_ms: over.read_timeout_ms.or(base.read_timeout_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(host: &str, port: u16, p: &str) -> bool {
        host_matches(host, port, p)
    }

    #[test]
    fn domain_patterns_match_the_host_and_subdomains() {
        for p in [
            "example.com",
            ".example.com",
            "*.example.com",
            "EXAMPLE.com.",
        ] {
            assert!(m("example.com", 443, p), "{p}");
            assert!(m("a.b.example.com", 443, p), "{p}");
            assert!(!m("badexample.com", 443, p), "{p}");
            assert!(!m("example.org", 443, p), "{p}");
        }
        assert!(m("anything.at.all", 1, "*"));
    }

    #[test]
    fn ports_ip_and_cidr_patterns() {
        assert!(m("example.com", 8443, "example.com:8443"));
        assert!(!m("example.com", 443, "example.com:8443"));
        assert!(m("10.1.2.3", 443, "10.0.0.0/8"));
        assert!(!m("11.1.2.3", 443, "10.0.0.0/8"));
        assert!(m("192.0.2.7", 80, "192.0.2.7"));
        assert!(m("::1", 443, "::1"));
        assert!(m("::1", 8080, "[::1]:8080"));
        assert!(!m("::1", 443, "[::1]:8080"));
        assert!(m("fd12::1", 443, "fd00::/8"));
        assert!(m("1.2.3.4", 443, "0.0.0.0/0"));
        // Names and IP literals never match each other.
        assert!(!m("10.0.0.1", 443, "example.com"));
        assert!(!m("localhost", 443, "127.0.0.1"));
        // An IPv4 pattern does not match an IPv6 host.
        assert!(!m("::ffff:10.0.0.1", 443, "10.0.0.0/8"));
    }

    #[test]
    fn invalid_patterns() {
        for p in [
            "",
            "a b",
            "exa!mple.com",
            "10.0.0.0/33",
            "host:0",
            "host:99999",
            "[::1",
            "x:y",
        ] {
            assert!(parse_pattern(p).is_none(), "{p}");
        }
    }

    #[test]
    fn deny_beats_allow_and_names_the_rule() {
        let n = NetworkSettings {
            allow_hosts: vec!["example.com".into()],
            deny_hosts: vec!["evil.example.com".into()],
            ..NetworkSettings::NONE
        };
        assert!(check_host_rules("cdn.example.com", 443, &n, "u", None).is_ok());
        let e = check_host_rules("evil.example.com", 443, &n, "u", None).unwrap_err();
        assert_eq!(e.code, E_HOST_BLOCKED);
        assert_eq!(
            e.detail,
            "refusing to connect to evil.example.com: it matches deny-hosts rule 'evil.example.com' (u)"
        );
        let e = check_host_rules("example.org", 443, &n, "u", None).unwrap_err();
        assert_eq!(
            e.detail,
            "refusing to connect to example.org: it matches no allow-hosts rule (u); add --allow-hosts example.org"
        );
    }

    #[test]
    fn proxy_parsing_and_bypass() {
        let p = parse_proxy("http://user:p%40ss@proxy.local:3128").unwrap();
        assert_eq!(p.host, "proxy.local");
        assert_eq!(p.port, 3128);
        assert_eq!(p.password.as_deref(), Some("p@ss"));
        assert_eq!(p.shown, "http://proxy.local:3128");
        assert_eq!(parse_proxy("http://proxy").unwrap().port, 80);
        for bad in [
            "https://proxy:1",
            "socks5://p:1",
            "http://p:1/path",
            "proxy:1",
            "http://",
        ] {
            let e = parse_proxy(bad).err().expect(bad);
            assert_eq!(e.code, E_USAGE);
            assert!(!e.detail.contains("p%40ss"));
        }
        let e = parse_proxy("https://u:secret@p:1").err().unwrap();
        assert!(!e.detail.contains("secret"), "{}", e.detail);
        let n = NetworkSettings {
            proxy: Some("http://proxy:3128".into()),
            no_proxy: vec!["internal.corp".into(), "10.0.0.0/8".into()],
            ..NetworkSettings::NONE
        };
        assert!(proxy_for("a.internal.corp", 443, &n).unwrap().is_none());
        assert!(proxy_for("10.2.3.4", 443, &n).unwrap().is_none());
        assert!(proxy_for("example.com", 443, &n).unwrap().is_some());
        assert!(format!(
            "{:?}",
            NetworkSettings {
                proxy: Some("http://u:secret@p:1".into()),
                ..NetworkSettings::NONE
            }
        )
        .contains("http://p:1"));
    }

    #[test]
    fn env_and_merge() {
        let env: BTreeMap<String, String> = [
            ("SITEFILE_PROXY", "http://p:1"),
            ("SITEFILE_NO_PROXY", "localhost, 127.0.0.1"),
            ("SITEFILE_DENY_HOSTS", "a.com b.com"),
            ("SITEFILE_FETCH_TIMEOUT", "connect=500, read=2000"),
            ("HTTPS_PROXY", "http://ignored:9"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let n = network_from_env(&env).unwrap();
        assert_eq!(n.proxy.as_deref(), Some("http://p:1"));
        assert_eq!(n.no_proxy, ["localhost", "127.0.0.1"]);
        assert_eq!(n.deny_hosts, ["a.com", "b.com"]);
        assert_eq!(
            (n.connect_timeout_ms, n.read_timeout_ms),
            (Some(500), Some(2000))
        );
        assert_eq!(parse_fetch_timeout("750").unwrap(), (Some(750), Some(750)));
        assert!(parse_fetch_timeout("fast").is_err());
        assert!(parse_fetch_timeout("connect=0").is_err());
        let over = NetworkSettings {
            deny_hosts: vec!["b.com".into(), "c.com".into()],
            read_timeout_ms: Some(9),
            ..NetworkSettings::NONE
        };
        let m = merge_network(&n, &over);
        assert_eq!(m.deny_hosts, ["a.com", "b.com", "c.com"]);
        assert_eq!(
            (m.connect_timeout_ms, m.read_timeout_ms),
            (Some(500), Some(9))
        );
        assert!(network_from_env(&BTreeMap::new()).unwrap() == NetworkSettings::NONE);
        // --proxy '' turns SITEFILE_PROXY off.
        let off = merge_network(
            &NetworkSettings {
                proxy: Some("http://p:1".into()),
                ..NetworkSettings::NONE
            },
            &NetworkSettings {
                proxy: Some(String::new()),
                ..NetworkSettings::NONE
            },
        );
        assert!(network_problem(&off).is_none());
        assert!(proxy_for("example.com", 443, &off).unwrap().is_none());
    }

    fn span() -> SourceSpan {
        SourceSpan::new("Sitefile", 3, 1)
    }

    #[test]
    fn a_host_outside_the_allow_list_carries_needs_host() {
        use crate::error::NeedsKind;
        let n = NetworkSettings {
            allow_hosts: vec!["example.org".into()],
            ..NetworkSettings::NONE
        };
        let e = check_host_rules(
            "cdn.example.com",
            443,
            &n,
            "https://cdn.example.com/a.css",
            None,
        )
        .unwrap_err();
        assert_eq!(e.detail, "refusing to connect to cdn.example.com: it matches no allow-hosts rule (https://cdn.example.com/a.css); add --allow-hosts cdn.example.com");
        let needs = e.needs.unwrap();
        assert_eq!(
            (needs.kind, needs.name.as_str(), needs.flag()),
            (NeedsKind::Host, "cdn.example.com", "--allow-hosts")
        );
        assert_eq!(needs.kind.as_str(), "host");
        assert_eq!(
            needs.args(),
            ["--allow-hosts".to_string(), "cdn.example.com".into()]
        );
        let e = check_host_rules("CDN.Example.com.", 443, &n, "u", None).unwrap_err();
        assert_eq!(e.needs.unwrap().name, "cdn.example.com");
        // A deny-hosts match carries none, also in strict mode.
        let d = NetworkSettings {
            allow_hosts: vec!["a.test".into()],
            deny_hosts: vec!["a.test".into()],
            strict: true,
            ..NetworkSettings::NONE
        };
        let e = check_host_rules("a.test", 443, &d, "u", None).unwrap_err();
        assert_eq!(e.code, E_HOST_BLOCKED);
        assert!(e.needs.is_none());
    }

    #[test]
    fn strict_mode_allows_only_listed_hosts() {
        let strict = NetworkSettings {
            strict: true,
            ..NetworkSettings::NONE
        };
        assert!(strict.is_set());
        let e = check_host_rules(
            "cdn.example.com",
            443,
            &strict,
            "https://cdn.example.com/x",
            None,
        )
        .unwrap_err();
        assert_eq!(e.detail, "refusing to connect to cdn.example.com: it matches no allow-hosts rule (strict network mode) (https://cdn.example.com/x); add --allow-hosts cdn.example.com");
        for allow in ["example.com", "*"] {
            let n = NetworkSettings {
                allow_hosts: vec![allow.into()],
                ..strict.clone()
            };
            assert!(check_host_rules("cdn.example.com", 443, &n, "u", None).is_ok());
        }
        let port = NetworkSettings {
            allow_hosts: vec!["a.test:443".into()],
            ..strict.clone()
        };
        assert!(check_host_rules("a.test", 8443, &port, "u", None).is_err());
        assert!(
            check_host_rules("cdn.example.com", 443, &NetworkSettings::NONE, "u", None).is_ok()
        );
        // host_allowed agrees with check_host_rules.
        for (n, h, want) in [
            (NetworkSettings::NONE, "a.test", true),
            (strict.clone(), "a.test", false),
            (port.clone(), "a.test", true),
            (
                NetworkSettings {
                    deny_hosts: vec!["a.test".into()],
                    ..NetworkSettings::NONE
                },
                "a.test",
                false,
            ),
        ] {
            assert_eq!(host_allowed(h, 443, &n), want);
            assert_eq!(check_host_rules(h, 443, &n, "u", None).is_ok(), want);
        }
    }

    #[test]
    fn network_mode_from_the_environment() {
        let env = |v: &str| BTreeMap::from([("SITEFILE_NETWORK".to_string(), v.to_string())]);
        assert!(network_from_env(&env("strict")).unwrap().strict);
        assert!(!network_from_env(&env(" open ")).unwrap().strict);
        assert!(!network_from_env(&env("")).unwrap().strict);
        let e = network_from_env(&env("ask")).unwrap_err();
        assert_eq!(
            (e.code, e.detail.as_str()),
            (
                E_USAGE,
                "SITEFILE_NETWORK expects open or strict, got 'ask'"
            )
        );
        assert_eq!(
            parse_network_mode("strict", "--network").unwrap(),
            Some(true)
        );
        assert!(parse_network_mode("Strict", "--network").is_err());
        let m = merge_network(
            &NetworkSettings {
                strict: true,
                secret_hosts: vec!["a=x.test".into()],
                ..NetworkSettings::NONE
            },
            &NetworkSettings {
                secret_hosts: vec!["a=x.test".into(), "b=y.test".into()],
                ..NetworkSettings::NONE
            },
        );
        assert!(m.strict);
        assert_eq!(m.secret_hosts, ["a=x.test", "b=y.test"]);
    }

    #[test]
    fn allow_secret_host_is_id_equals_pattern() {
        let ok = NetworkSettings {
            secret_hosts: vec![
                "tok=api.example.com".into(),
                "_a1=*.example.com:8443".into(),
                "k=10.0.0.0/8".into(),
            ],
            ..NetworkSettings::NONE
        };
        assert!(network_problem(&ok).is_none());
        for bad in [
            "tok",
            "=api.example.com",
            "tok=",
            "1tok=a.test",
            "to-k=a.test",
            "tok=a b",
            "tok=a.test/x",
        ] {
            let n = NetworkSettings {
                secret_hosts: vec![bad.into()],
                ..NetworkSettings::NONE
            };
            assert_eq!(
                network_problem(&n).as_deref(),
                Some(format!("invalid --allow-secret-host '{bad}': expected <secret id>=<host pattern> (a pattern as for --allow-hosts)").as_str()),
                "{bad}"
            );
        }
    }

    #[test]
    fn strict_mode_sends_a_secret_only_where_a_rule_allows() {
        use crate::error::NeedsKind;
        let strict = NetworkSettings {
            strict: true,
            allow_hosts: vec!["example.com".into()],
            ..NetworkSettings::NONE
        };
        let url = "https://api.example.com/v1";
        let e = check_secret_hosts("data", &["tok"], url, &strict, Some(&span())).unwrap_err();
        assert_eq!(e.code, E_HOST_BLOCKED);
        assert_eq!(e.message(), "Sitefile:3:1: IMPORT data: refusing to send secret 'tok' to api.example.com (strict network mode); add --allow-secret-host tok=api.example.com");
        let needs = e.needs.unwrap();
        assert_eq!(
            (needs.kind, needs.name.as_str(), needs.flag()),
            (
                NeedsKind::SecretHost,
                "tok=api.example.com",
                "--allow-secret-host"
            )
        );
        assert_eq!(needs.kind.as_str(), "secret-host");
        let with = |rules: &[&str]| NetworkSettings {
            secret_hosts: rules.iter().map(|s| s.to_string()).collect(),
            ..strict.clone()
        };
        assert!(
            check_secret_hosts("data", &["tok"], url, &with(&["tok=api.example.com"]), None)
                .is_ok()
        );
        assert!(
            check_secret_hosts("data", &["tok"], url, &with(&["tok=example.com"]), None).is_ok()
        );
        assert!(check_secret_hosts(
            "data",
            &["tok"],
            url,
            &with(&["other=api.example.com"]),
            None
        )
        .is_err());
        assert!(
            check_secret_hosts("data", &["tok"], url, &with(&["tok=evil.test"]), None).is_err()
        );
        let e = check_secret_hosts(
            "data",
            &["a", "b"],
            url,
            &with(&["a=api.example.com"]),
            None,
        )
        .unwrap_err();
        assert_eq!(e.needs.unwrap().name, "b=api.example.com");
        // Open mode needs no rule; a host the host rules refuse is left to the request.
        let open = NetworkSettings {
            strict: false,
            ..strict.clone()
        };
        assert!(check_secret_hosts("data", &["tok"], url, &open, None).is_ok());
        assert!(check_secret_hosts("data", &["tok"], url, &NetworkSettings::NONE, None).is_ok());
        assert!(
            check_secret_hosts("data", &["tok"], "https://api.example.org/", &strict, None).is_ok()
        );
    }

    #[test]
    fn error_json_has_code_message_span_and_needs() {
        let e = SitefileError::new(E_HOST_BLOCKED, "refusing", Some(span()))
            .with_needs(Needs::host("a.test"));
        assert_eq!(
            serde_json::to_string(&e.to_json()).unwrap(),
            r#"{"error":{"code":"E_HOST_BLOCKED","message":"Sitefile:3:1: refusing","span":{"file":"Sitefile","line":3,"column":1},"needs":{"kind":"host","name":"a.test","flag":"--allow-hosts"}}}"#
        );
        let plain = SitefileError::plain(E_FETCH, "x");
        assert_eq!(
            serde_json::to_string(&plain.to_json()).unwrap(),
            r#"{"error":{"code":"E_FETCH","message":"x","span":null,"needs":null}}"#
        );
    }
}
