//! OCI distribution client, registry credentials and push/pull (port of
//! `src/registry.ts`, `src/registry-auth.ts` and `src/registry-ops.ts`).
//!
//! # Client
//!
//! Distribution-spec v1.1: `/v2/` probe, Bearer-token and Basic auth, manifest
//! and blob GET/HEAD, monolithic blob upload, cross-repository mount, manifest
//! PUT. Transport policy (the registry host is named by the user, so its
//! address may be private):
//!
//! - https, except for loopback registries (`localhost`, 127.0.0.0/8, ::1) and
//!   hosts passed as insecure registries, which try https first and fall back
//!   to http;
//! - redirects are followed manually (GET/HEAD only), at most 5, every hop
//!   re-validated. Registries redirect blob downloads to a CDN: a target on
//!   another origin must be https (unless both the registry and the target are
//!   loopback), must not carry credentials in the URL, and must resolve to
//!   public addresses (checked at connect time by the guarded resolver) unless
//!   `allow_private_network`;
//! - the Authorization header is sent to the registry's own origin only, never
//!   to a redirect target; token endpoints (the `realm` of a Bearer challenge)
//!   obey the same cross-origin rules;
//! - bodies are never decompressed; responses are bounded by [`RegistryLimits`].
//!
//! Tokens and passwords live in memory only and never appear in an error
//! message ([`Credentials`]' `Debug` is redacted).
//!
//! # Credentials
//!
//! Sources, first match wins:
//!
//! 1. explicit (`--username` + `--password-stdin`; only for the registry the command names);
//! 2. `$SITEFILE_REGISTRY_AUTH`: JSON `{ "<host>": "user:password" | {"username","password"} | {"identitytoken"} }`;
//! 3. Docker config (`$DOCKER_CONFIG/config.json` or `~/.docker/config.json`):
//!    `credHelpers[<host>]` -> `docker-credential-<name> get`, then
//!    `auths[<host>].auth|username/password|identitytoken`, then
//!    `credsStore` -> `docker-credential-<name> get`.
//!
//! A credential helper is run only when the config names one; the name must be
//! a plain word, it runs without a shell, the server URL goes to its stdin and
//! nothing secret is ever on argv.
//!
//! The environment is passed explicitly ([`Env`]) rather than read from the
//! process, so callers (and parallel tests) control `PATH`, `DOCKER_CONFIG`,
//! `HOME` and `SITEFILE_REGISTRY_AUTH`.

use crate::digest::{is_integrity, sha256};
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::oci::{media, parse_config, parse_manifest, parse_platform, Descriptor, ParsedManifest};
use crate::refs::{canonical_ref, is_component, is_tag, parse_ref, ref_key, with_default_registry, ImageRef};
use crate::resources::https::{redact_url, tls_config, GuardedResolver};
use crate::resources::netpolicy::is_public_address;
use crate::resources::types::{FetchPolicy, DEFAULT_FETCH_POLICY};
use crate::store::{short_id, Store};
use base64::Engine;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ================================================================ environment

/// An environment snapshot: what credential resolution and helpers see.
pub type Env = BTreeMap<String, String>;

/// The current process environment (non-UTF-8 entries are skipped).
pub fn process_env() -> Env {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
}

/// Registries reachable over plain http: `--insecure-registry` flags plus
/// `$SITEFILE_INSECURE_REGISTRIES` (comma-separated), deduplicated in order.
pub fn insecure_registries_from_env(extra: &[String], env: &Env) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let from_env = env.get("SITEFILE_INSECURE_REGISTRIES").map(String::as_str).unwrap_or("");
    for s in extra.iter().map(String::as_str).chain(from_env.split(',').map(str::trim).filter(|s| !s.is_empty())) {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    }
    out
}

// ================================================================ credentials

/// Registry credentials. `Debug` never prints the secrets.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: Option<String>,
    /// A Docker "identity token" (OAuth2 refresh token), exchanged at the token endpoint.
    pub identity_token: Option<String>,
}

impl Credentials {
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Credentials {
        Credentials { username: Some(username.into()), password: Some(password.into()), identity_token: None }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("identity_token", &self.identity_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

const DOCKER_HUB_KEYS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];
const DEFAULT_HELPER_TIMEOUT_MS: u64 = 15_000;

/// `https://index.docker.io/v1/`, `registry.example.com:5000/x` -> host key.
fn host_key(k: &str) -> String {
    let b = k.as_bytes();
    let alpha = b.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    let no_scheme = if alpha > 0 && k[alpha..].starts_with("://") { &k[alpha + 3..] } else { k };
    let host = no_scheme.split('/').next().unwrap_or("").to_ascii_lowercase();
    if DOCKER_HUB_KEYS.contains(&host.as_str()) {
        "docker.io".into()
    } else {
        host
    }
}

fn key_matches(config_key: &str, registry: &str) -> bool {
    host_key(config_key) == host_key(registry)
}

/// `$DOCKER_CONFIG/config.json`, else `$HOME/.docker/config.json`.
pub fn docker_config_path(env: &Env) -> PathBuf {
    match env.get("DOCKER_CONFIG").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("config.json"),
        None => {
            let home = env.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).unwrap_or_else(|| "/".into());
            PathBuf::from(home).join(".docker").join("config.json")
        }
    }
}

fn nonempty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn split_user_pass(text: &str) -> Option<Credentials> {
    match text.find(':') {
        Some(i) if i > 0 => Some(Credentials::basic(&text[..i], &text[i + 1..])),
        _ => None,
    }
}

/// Node's lenient `Buffer.from(s, 'base64')`: whitespace and padding are optional.
fn lenient_base64(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace() && *c != '=').map(|c| match c {
        '-' => '+',
        '_' => '/',
        c => c,
    }).collect();
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(clean.as_bytes()).unwrap_or_default()
}

fn from_entry(e: &Value) -> Option<Credentials> {
    if let Some(s) = e.as_str() {
        return split_user_pass(s);
    }
    let o = e.as_object()?;
    if let Some(auth) = nonempty(o.get("auth")) {
        if let Some(c) = split_user_pass(&String::from_utf8_lossy(&lenient_base64(&auth))) {
            return Some(c);
        }
    }
    if let Some(tok) = nonempty(o.get("identitytoken")).or_else(|| nonempty(o.get("identityToken"))) {
        return Some(Credentials { identity_token: Some(tok), username: nonempty(o.get("username")), password: None });
    }
    match (nonempty(o.get("username")), nonempty(o.get("password"))) {
        (Some(u), Some(p)) => Some(Credentials::basic(u, p)),
        _ => None,
    }
}

fn valid_helper_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphanumeric() && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Docker's `docker-credential-<name> get`: the server URL on stdin, JSON on
/// stdout. Runs without a shell, with exactly `env` as its environment (the
/// program is looked up on `env`'s `PATH`). Its output is never put in an error.
pub fn run_credential_helper(name: &str, server_url: &str, env: &Env, timeout_ms: u64) -> Result<Option<Credentials>> {
    if !valid_helper_name(name) {
        let shown: String = name.chars().take(40).collect();
        return Err(SitefileError::plain(E_AUTH, format!("refusing to run credential helper with unusual name '{shown}'")));
    }
    let program = format!("docker-credential-{name}");
    let failed = || SitefileError::plain(E_AUTH, format!("credential helper {program} failed"));
    let mut child = match Command::new(&program)
        .arg("get")
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(SitefileError::plain(E_AUTH, format!("credential helper {program} (named in your Docker config) was not found on PATH")));
        }
        Err(_) => return Err(failed()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(server_url.as_bytes());
    }
    let mut stdout = child.stdout.take().ok_or_else(failed)?;
    let reader = std::thread::spawn(move || -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => return Some(out),
                Ok(n) => {
                    if out.len() + n > 1 << 20 {
                        return None;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
    });
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed());
            }
        }
    };
    let Some(out) = reader.join().ok().flatten() else {
        return Err(failed());
    };
    let text = String::from_utf8_lossy(&out);
    if !status.success() {
        // The helper protocol's "no entry for this server" answer means anonymous access.
        if text.to_ascii_lowercase().contains("credentials not found") {
            return Ok(None);
        }
        return Err(failed());
    }
    let j: Value = serde_json::from_str(&text)
        .map_err(|_| SitefileError::plain(E_AUTH, format!("credential helper {program} returned unreadable output")))?;
    let user = nonempty(j.get("Username"));
    let secret = nonempty(j.get("Secret"));
    Ok(match (user, secret) {
        (Some(u), Some(s)) if u == "<token>" => Some(Credentials { identity_token: Some(s), ..Default::default() }),
        (Some(u), Some(s)) => Some(Credentials::basic(u, s)),
        _ => None,
    })
}

fn read_json_file(file: &std::path::Path) -> Result<Option<Value>> {
    let text = match std::fs::read(file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SitefileError::plain(E_AUTH, format!("cannot read {}: {e}", file.display()))),
    };
    serde_json::from_str(&text).map(Some).map_err(|_| SitefileError::plain(E_AUTH, format!("{} is not valid JSON", file.display())))
}

/// Resolve credentials for `registry` (canonical host, e.g. `docker.io` or
/// `localhost:5000`); `None` means anonymous access.
pub fn resolve_credentials(registry: &str, explicit: Option<&Credentials>, env: &Env) -> Result<Option<Credentials>> {
    resolve_credentials_with_timeout(registry, explicit, env, DEFAULT_HELPER_TIMEOUT_MS)
}

pub fn resolve_credentials_with_timeout(registry: &str, explicit: Option<&Credentials>, env: &Env, helper_timeout_ms: u64) -> Result<Option<Credentials>> {
    if let Some(c) = explicit {
        return Ok(Some(c.clone()));
    }
    if let Some(raw) = env.get("SITEFILE_REGISTRY_AUTH").filter(|s| !s.is_empty()) {
        let bad = || SitefileError::plain(E_AUTH, "SITEFILE_REGISTRY_AUTH must be a JSON object mapping registry hosts to \"user:password\"");
        let j: Value = serde_json::from_str(raw).map_err(|_| bad())?;
        let o = j.as_object().ok_or_else(bad)?;
        for (k, v) in o {
            if key_matches(k, registry) {
                if let Some(c) = from_entry(v) {
                    return Ok(Some(c));
                }
            }
        }
    }
    let Some(cfg) = read_json_file(&docker_config_path(env))? else { return Ok(None) };
    let Some(cfg) = cfg.as_object() else { return Ok(None) };
    let server_url = if host_key(registry) == "docker.io" { "https://index.docker.io/v1/".to_string() } else { host_key(registry) };
    if let Some(helpers) = cfg.get("credHelpers").and_then(Value::as_object) {
        for (k, v) in helpers {
            if let (true, Some(name)) = (key_matches(k, registry), nonempty(Some(v))) {
                return run_credential_helper(&name, &server_url, env, helper_timeout_ms);
            }
        }
    }
    if let Some(auths) = cfg.get("auths").and_then(Value::as_object) {
        for (k, v) in auths {
            if key_matches(k, registry) {
                if let Some(c) = from_entry(v) {
                    return Ok(Some(c));
                }
            }
        }
    }
    if let Some(store) = nonempty(cfg.get("credsStore")) {
        return run_credential_helper(&store, &server_url, env, helper_timeout_ms);
    }
    Ok(None)
}

// ================================================================ client options

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryLimits {
    /// Largest blob accepted (bytes).
    pub max_blob_bytes: u64,
    /// Largest total of all blobs of one image (bytes).
    pub max_total_bytes: u64,
    /// Largest manifest / token response (bytes).
    pub max_manifest_bytes: u64,
    /// Idle/connect timeout per request (ms).
    pub timeout_ms: u64,
    /// Time budget for one blob transfer (ms).
    pub blob_deadline_ms: u64,
}

pub const DEFAULT_REGISTRY_LIMITS: RegistryLimits = RegistryLimits {
    max_blob_bytes: 2 * 1024 * 1024 * 1024,
    max_total_bytes: 8 * 1024 * 1024 * 1024,
    max_manifest_bytes: 4 * 1024 * 1024,
    timeout_ms: 30_000,
    blob_deadline_ms: 30 * 60_000,
};

impl Default for RegistryLimits {
    fn default() -> Self {
        DEFAULT_REGISTRY_LIMITS
    }
}

/// Asked lazily, only when the registry demands authentication.
pub type CredentialProvider = Arc<dyn Fn() -> Result<Option<Credentials>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct RegistryOptions {
    /// Registries (`host[:port]`) that may be reached over plain http.
    pub insecure_registries: Vec<String>,
    /// Let redirects and token endpoints on other origins resolve to private addresses.
    pub allow_private_network: bool,
    /// Credentials source; `None` means anonymous unless built by [`make_client`].
    pub credentials: Option<CredentialProvider>,
    pub limits: RegistryLimits,
}

impl fmt::Debug for RegistryOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryOptions")
            .field("insecure_registries", &self.insecure_registries)
            .field("allow_private_network", &self.allow_private_network)
            .field("credentials", &self.credentials.as_ref().map(|_| "<provider>"))
            .field("limits", &self.limits)
            .finish()
    }
}

/// Build a client for a (canonical) registry name, with credentials resolved
/// lazily from `explicit`, then `env` ($SITEFILE_REGISTRY_AUTH, Docker config),
/// unless `options.credentials` is already set.
pub fn make_client(registry: &str, options: &RegistryOptions, explicit: Option<Credentials>, env: &Env) -> RegistryClient {
    let mut o = options.clone();
    if o.credentials.is_none() {
        let (reg, env) = (registry.to_string(), env.clone());
        o.credentials = Some(Arc::new(move || resolve_credentials(&reg, explicit.as_ref(), &env)));
    }
    RegistryClient::new(registry, o)
}

// ================================================================ URLs

/// A parsed URL (WHATWG, via ureq's parser).
#[derive(Debug, Clone)]
struct Url {
    href: String,
    scheme: String,
    /// `URL.host`: hostname (IPv6 in brackets) plus a non-default port.
    host: String,
    /// Without IPv6 brackets.
    hostname: String,
    has_userinfo: bool,
}

fn url_agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| ureq::AgentBuilder::new().build())
}

fn parse_url(s: &str) -> Option<Url> {
    let req = url_agent().get(s);
    let ru = req.request_url().ok()?;
    let u = ru.as_url();
    let host_str = u.host_str().unwrap_or("");
    let host = match u.port() {
        Some(p) => format!("{host_str}:{p}"),
        None => host_str.to_string(),
    };
    Some(Url {
        href: u.as_str().to_string(),
        scheme: u.scheme().to_string(),
        hostname: host_str.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host_str).to_string(),
        host,
        has_userinfo: !u.username().is_empty() || u.password().is_some_and(|p| !p.is_empty()),
    })
}

fn join_url(base: &str, rel: &str) -> Option<String> {
    let req = url_agent().get(base);
    let ru = req.request_url().ok()?;
    ru.as_url().join(rel).ok().map(|u| u.to_string())
}

/// Rewrite the query: drop `remove` keys, then append `add` (URLSearchParams semantics).
fn edit_query(url: &str, remove: &[&str], add: &[(&str, &str)], clear: bool) -> Option<String> {
    let req = url_agent().get(url);
    let ru = req.request_url().ok()?;
    let mut u = ru.as_url().clone();
    let kept: Vec<(String, String)> =
        if clear { vec![] } else { u.query_pairs().filter(|(k, _)| !remove.contains(&k.as_ref())).map(|(k, v)| (k.into_owned(), v.into_owned())).collect() };
    if kept.is_empty() && add.is_empty() {
        u.set_query(None);
    } else {
        let mut q = u.query_pairs_mut();
        q.clear();
        for (k, v) in &kept {
            q.append_pair(k, v);
        }
        for (k, v) in add {
            q.append_pair(k, v);
        }
    }
    Some(u.to_string())
}

/// `application/x-www-form-urlencoded` serialization.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    edit_query("http://x/", &[], pairs, true).and_then(|s| s.split_once('?').map(|(_, q)| q.to_string())).unwrap_or_default()
}

fn hostname_of(host_port: &str) -> &str {
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host_port.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host_port,
    }
}

pub fn is_loopback_host(hostname: &str) -> bool {
    let h = hostname.to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h == "::1" {
        return true;
    }
    let parts: Vec<&str> = h.split('.').collect();
    parts.len() == 4 && parts[0] == "127" && parts[1..].iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Control characters to spaces, at most 200 characters.
fn clean(s: &str) -> String {
    s.chars().map(|c| if (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c }).take(200).collect()
}

// ================================================================ client

const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
const MAX_REDIRECTS: u32 = 5;
const USER_AGENT: &str = "sitefile/0.1";

pub const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

fn valid_repo(repo: &str) -> bool {
    !repo.is_empty() && repo.split('/').all(is_component)
}

/// The `/v2/...` target of a `Link: </v2/...>; rel="next"` header (same-registry pages only).
fn next_link(link: &str) -> Option<String> {
    let start = link.find("</v2/")?;
    let rest = &link[start + 1..];
    let end = rest.find('>')?;
    let after = rest[end + 1..].trim_start();
    let params = after.strip_prefix(';')?.trim_start();
    let rel = params.strip_prefix("rel=")?;
    let rel = rel.strip_prefix('"').unwrap_or(rel);
    rel.starts_with("next").then(|| rest[..end].to_string())
}

fn valid_reference(r: &str) -> bool {
    is_tag(r) || is_integrity(r)
}

/// A manifest as fetched: its bytes, their digest and the media type.
#[derive(Debug, Clone)]
pub struct ManifestResponse {
    pub bytes: Vec<u8>,
    pub digest: String,
    pub media_type: String,
}

/// A request body, reopened for each attempt (a 401 retry sends it again).
pub enum Body<'a> {
    Bytes(&'a [u8]),
    Stream { length: u64, open: &'a dyn Fn() -> Result<Box<dyn Read + Send>> },
}

impl Body<'_> {
    fn length(&self) -> u64 {
        match self {
            Body::Bytes(b) => b.len() as u64,
            Body::Stream { length, .. } => *length,
        }
    }
}

#[derive(Debug, Clone)]
struct Challenge {
    scheme: String,
    params: HashMap<String, String>,
}

fn parse_challenge(h: Option<&str>) -> Option<Challenge> {
    let h = h?.trim_start();
    let n = h.bytes().take_while(u8::is_ascii_alphabetic).count();
    if n == 0 {
        return None;
    }
    let scheme = h[..n].to_ascii_lowercase();
    let rest = &h[n..];
    let b = rest.as_bytes();
    let is_key = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let mut params = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        if !is_key(b[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < b.len() && is_key(b[i]) {
            i += 1;
        }
        if rest[i..].starts_with("=\"") {
            let vs = i + 2;
            match rest[vs..].find('"') {
                Some(e) => {
                    params.insert(rest[s..i].to_ascii_lowercase(), rest[vs..vs + e].to_string());
                    i = vs + e + 1;
                }
                None => break,
            }
        }
    }
    Some(Challenge { scheme, params })
}

fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    for _ in 0..8 {
        let Some(err) = cur else { break };
        if let Some(io) = err.downcast_ref::<io::Error>() {
            if matches!(io.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

/// A [`GuardedResolver`] that keeps the policy error behind a failed lookup.
#[derive(Clone)]
struct Guard {
    inner: GuardedResolver,
    last: Arc<Mutex<Option<SitefileError>>>,
}

impl ureq::Resolver for Guard {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        self.inner.resolve_checked(netloc).map_err(|e| {
            let msg = e.message();
            if let Ok(mut g) = self.last.lock() {
                *g = Some(e);
            }
            io::Error::other(msg)
        })
    }
}

struct Transport {
    agent: ureq::Agent,
    last: Arc<Mutex<Option<SitefileError>>>,
}

impl Transport {
    fn new(allow_private: bool, timeout: Duration) -> Transport {
        let policy = FetchPolicy { allow_private_network: allow_private, ..DEFAULT_FETCH_POLICY };
        let last = Arc::new(Mutex::new(None));
        let guard = Guard { inner: GuardedResolver::new(&policy, None), last: last.clone() };
        let mut builder = ureq::AgentBuilder::new()
            .redirects(0)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .try_proxy_from_env(false)
            .user_agent(USER_AGENT)
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .timeout_write(timeout)
            .resolver(guard);
        // Trust NODE_EXTRA_CA_CERTS as Node's https (the TypeScript registry client)
        // does. Without a `ca` argument this cannot fail.
        if let Ok(Some(config)) = tls_config(None, None) {
            builder = builder.tls_config(config);
        }
        Transport { agent: builder.build(), last }
    }
    fn take_error(&self) -> Option<SitefileError> {
        self.last.lock().ok().and_then(|mut g| g.take())
    }
}

/// Client for one registry. Thread-safe; tokens are cached per scope.
pub struct RegistryClient {
    /// The canonical registry name (`docker.io`, `ghcr.io`, `localhost:5000`).
    pub registry: String,
    pub limits: RegistryLimits,
    options: RegistryOptions,
    host: String,
    insecure: bool,
    loopback: bool,
    origin: Mutex<Option<Url>>,
    tokens: Mutex<HashMap<String, String>>,
    creds: Mutex<Option<Option<Credentials>>>,
    /// Same origin, loopback pairs, or --allow-private-network.
    private: Transport,
    /// Other origins: public addresses only.
    public: Transport,
}

/// A streaming blob download. Keeps the policy error behind a failed read for
/// [`RegistryClient::get_blob`]'s caller (see [`BlobReader::take_error`]).
pub struct BlobReader {
    inner: Box<dyn Read + Send + Sync>,
    deadline: Instant,
    deadline_ms: u64,
    digest: String,
    url: String,
    timeout_ms: u64,
    error: Option<SitefileError>,
}

impl BlobReader {
    /// The error that made the last read fail, with its proper code.
    pub fn take_error(&mut self) -> Option<SitefileError> {
        self.error.take()
    }
}

impl Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() > self.deadline {
            let e = SitefileError::plain(E_FETCH_TIMEOUT, format!("downloading blob {} exceeded {} ms", self.digest, self.deadline_ms));
            let msg = e.message();
            self.error = Some(e);
            return Err(io::Error::new(io::ErrorKind::TimedOut, msg));
        }
        match self.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(e),
            Err(e) => {
                let se = if is_timeout(&e) {
                    SitefileError::plain(E_FETCH_TIMEOUT, format!("GET {} timed out after {} ms", self.url, self.timeout_ms))
                } else {
                    SitefileError::plain(E_REGISTRY, format!("error downloading blob {}: {e}", self.digest))
                };
                let msg = se.message();
                self.error = Some(se);
                Err(io::Error::other(msg))
            }
        }
    }
}

/// Outcome of [`RegistryClient::push_blob`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobPush {
    Mounted,
    Uploaded,
}

impl RegistryClient {
    /// `registry` is the canonical registry name (`docker.io`, `ghcr.io`, `localhost:5000`).
    pub fn new(registry: &str, options: RegistryOptions) -> RegistryClient {
        let host = crate::refs::registry_api_host(registry).to_string();
        let loopback = is_loopback_host(hostname_of(&host));
        let insecure = loopback
            || options.insecure_registries.iter().any(|h| h.eq_ignore_ascii_case(&host) || h.eq_ignore_ascii_case(registry));
        let timeout = Duration::from_millis(options.limits.timeout_ms.max(1));
        RegistryClient {
            registry: registry.to_string(),
            limits: options.limits,
            private: Transport::new(true, timeout),
            public: Transport::new(options.allow_private_network, timeout),
            options,
            host,
            insecure,
            loopback,
            origin: Mutex::new(None),
            tokens: Mutex::new(HashMap::new()),
            creds: Mutex::new(None),
        }
    }

    // ------------------------------------------------------------ transport

    fn origin(&self) -> Option<Url> {
        self.origin.lock().ok().and_then(|g| g.clone())
    }

    fn same_origin(&self, u: &Url) -> bool {
        self.origin().is_some_and(|o| o.scheme == u.scheme && o.host == u.host)
    }

    /// Refuse hops that leave the registry's origin unless they are safe.
    fn check_target(&self, u: &Url) -> Result<()> {
        if self.same_origin(u) {
            return Ok(());
        }
        let shown = redact_url(&u.href);
        if u.has_userinfo {
            return Err(SitefileError::plain(E_PROTOCOL, format!("refusing a URL with credentials ({shown})")));
        }
        let loopback_pair = self.loopback && is_loopback_host(&u.hostname);
        if u.scheme != "https" && !(u.scheme == "http" && loopback_pair) {
            return Err(SitefileError::plain(
                E_PROTOCOL,
                format!("{}: refusing to follow a redirect or token realm to {shown}: it must use https", self.registry),
            ));
        }
        if u.hostname.parse::<IpAddr>().is_ok() && !is_public_address(&u.hostname) && !self.options.allow_private_network && !loopback_pair {
            return Err(SitefileError::plain(
                E_SSRF_BLOCKED,
                format!(
                    "{}: refusing to connect to non-public address {} ({shown}); use --allow-private-network to permit it",
                    self.registry, u.hostname
                ),
            ));
        }
        Ok(())
    }

    fn send_once(&self, method: &str, url: &Url, headers: &[(String, String)], body: Option<&Body<'_>>, timeout: Option<Duration>) -> Result<ureq::Response> {
        let loopback_pair = self.loopback && is_loopback_host(&url.hostname);
        let t = if self.same_origin(url) || loopback_pair { &self.private } else { &self.public };
        let shown = redact_url(&url.href);
        let mut req = t.agent.request(method, &url.href).set("accept-encoding", "identity").set("user-agent", USER_AGENT);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        if let Some(d) = timeout {
            req = req.timeout(d);
        }
        let ms = timeout.map(|d| d.as_millis() as u64).unwrap_or(self.limits.timeout_ms);
        let result = match body {
            None if method == "POST" || method == "PUT" => req.send_bytes(&[]),
            None => req.call(),
            Some(b) => {
                let req = req.set("content-length", &b.length().to_string());
                match b {
                    Body::Bytes(bytes) => req.send_bytes(bytes),
                    Body::Stream { open, .. } => req.send(open()?),
                }
            }
        };
        match result {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(_, r)) => Ok(r),
            Err(ureq::Error::Transport(tr)) => {
                if let Some(e) = t.take_error() {
                    if e.code == E_SSRF_BLOCKED {
                        return Err(e);
                    }
                    return Err(SitefileError::plain(E_REGISTRY, format!("{method} {shown} failed: {}", e.detail)));
                }
                if is_timeout(&tr) {
                    return Err(SitefileError::plain(E_FETCH_TIMEOUT, format!("{method} {shown} timed out after {ms} ms")));
                }
                let why = tr.message().map(str::to_string).unwrap_or_else(|| tr.kind().to_string());
                let src = std::error::Error::source(&tr).map(|s| format!(": {s}")).unwrap_or_default();
                Err(SitefileError::plain(E_REGISTRY, format!("{method} {shown} failed: {why}{src}")))
            }
        }
    }

    /// One logical request: follows redirects (GET/HEAD) re-validating each hop; auth only at the registry origin.
    fn follow(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&Body<'_>>,
        auth: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<ureq::Response> {
        let first = redact_url(url);
        let mut cur = parse_url(url).ok_or_else(|| SitefileError::plain(E_REGISTRY, format!("cannot request {first}: invalid URL")))?;
        let mut hops = 0;
        loop {
            self.check_target(&cur)?;
            let mut h: Vec<(String, String)> = headers.to_vec();
            if let Some(a) = auth.filter(|_| self.same_origin(&cur)) {
                h.push(("authorization".into(), a.to_string()));
            }
            let res = self.send_once(method, &cur, &h, body, timeout)?;
            let status = res.status();
            if !REDIRECTS.contains(&status) || (method != "GET" && method != "HEAD") {
                return Ok(res);
            }
            let shown = redact_url(&cur.href);
            let Some(loc) = res.header("location").map(str::to_string) else {
                return Err(SitefileError::plain(E_REDIRECT, format!("redirect {status} from {shown} has no Location header")));
            };
            drop(res);
            if hops >= MAX_REDIRECTS {
                return Err(SitefileError::plain(E_REDIRECT, format!("too many redirects from {first} (limit {MAX_REDIRECTS})")));
            }
            hops += 1;
            cur = join_url(&cur.href, &loc)
                .and_then(|u| parse_url(&u))
                .ok_or_else(|| SitefileError::plain(E_REDIRECT, format!("invalid redirect Location from {shown}")))?;
        }
    }

    /// Find the base URL: https, or (insecure registries only) http when https is unreachable.
    fn init(&self) -> Result<Url> {
        if let Some(o) = self.origin() {
            return Ok(o);
        }
        let bad = || SitefileError::plain(E_USAGE, format!("invalid registry host '{}'", self.host));
        let https = parse_url(&format!("https://{}/", self.host)).ok_or_else(bad)?;
        *self.origin.lock().unwrap() = Some(https.clone());
        if !self.insecure {
            return Ok(https);
        }
        let probe = parse_url(&format!("{}v2/", https.href)).ok_or_else(bad)?;
        match self.send_once("GET", &probe, &[], None, Some(Duration::from_millis(self.limits.timeout_ms.min(5000)))) {
            Ok(_) => Ok(https),
            Err(e) if e.code != E_REGISTRY && e.code != E_FETCH_TIMEOUT => Err(e),
            Err(_) => {
                let http = parse_url(&format!("http://{}/", self.host)).ok_or_else(bad)?;
                *self.origin.lock().unwrap() = Some(http.clone());
                Ok(http)
            }
        }
    }

    fn get_creds(&self) -> Result<Option<Credentials>> {
        if let Some(c) = self.creds.lock().unwrap().clone() {
            return Ok(c);
        }
        let v = match &self.options.credentials {
            Some(f) => f()?,
            None => None,
        };
        *self.creds.lock().unwrap() = Some(v.clone());
        Ok(v)
    }

    // ------------------------------------------------------------ auth

    fn basic_header(c: &Credentials) -> String {
        let raw = format!("{}:{}", c.username.as_deref().unwrap_or(""), c.password.as_deref().unwrap_or(""));
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()))
    }

    fn fetch_token(&self, chal: &Challenge, scope: &str, creds: Option<&Credentials>) -> Result<String> {
        let no_realm = || SitefileError::plain(E_AUTH, format!("{}: the registry's token challenge has no usable realm", self.registry));
        let realm = chal.params.get("realm").ok_or_else(no_realm)?;
        let realm_url = parse_url(realm).filter(|u| !u.scheme.is_empty() && realm.contains(':')).ok_or_else(no_realm)?;
        let scopes: Vec<(&str, &str)> = scope.split(' ').filter(|s| !s.is_empty()).map(|s| ("scope", s)).collect();
        let mut add: Vec<(&str, &str)> = Vec::new();
        if let Some(s) = chal.params.get("service") {
            add.push(("service", s));
        }
        add.extend(scopes);
        let remove: &[&str] = if chal.params.contains_key("service") { &["service"] } else { &[] };
        let mut url = edit_query(&realm_url.href, remove, &add, false).ok_or_else(no_realm)?;
        let mut headers: Vec<(String, String)> = vec![("accept".into(), "application/json".into())];
        let mut method = "GET";
        let form;
        let mut body = None;
        if let Some(tok) = creds.and_then(|c| c.identity_token.as_deref()) {
            method = "POST";
            let service = chal.params.get("service").map(String::as_str).unwrap_or("");
            form = form_encode(&[("grant_type", "refresh_token"), ("service", service), ("scope", scope), ("client_id", "sitefile"), ("refresh_token", tok)]);
            url = edit_query(&url, &[], &[], true).ok_or_else(no_realm)?;
            headers.push(("content-type".into(), "application/x-www-form-urlencoded".into()));
            body = Some(Body::Bytes(form.as_bytes()));
        } else if let Some(c) = creds.filter(|c| c.username.as_deref().is_some_and(|u| !u.is_empty()) && c.password.is_some()) {
            headers.push(("authorization".into(), Self::basic_header(c)));
        }
        let res = self.follow(method, &url, &headers, body.as_ref(), None, None)?;
        let status = res.status();
        if status != 200 {
            let hint = if status == 401 || status == 403 { " (credentials rejected or missing)" } else { "" };
            return Err(SitefileError::plain(
                E_AUTH,
                format!("{}: the token endpoint {} answered HTTP {status}{hint}", self.registry, redact_url(&url)),
            ));
        }
        let bytes = read_body(res, self.limits.max_manifest_bytes)?;
        let j: Value = serde_json::from_slice(&bytes)
            .map_err(|_| SitefileError::plain(E_AUTH, format!("{}: the token endpoint returned unreadable output", self.registry)))?;
        let pick = |k: &str| j.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        pick("token")
            .or_else(|| pick("access_token"))
            .ok_or_else(|| SitefileError::plain(E_AUTH, format!("{}: the token endpoint returned no token", self.registry)))
    }

    fn authenticate(&self, chal: &Challenge, scope: &str, use_creds: bool) -> Result<String> {
        let need = use_creds || chal.scheme == "basic";
        let creds = if need { self.get_creds()? } else { None };
        if need && creds.is_none() {
            return Err(SitefileError::plain(
                E_AUTH,
                format!(
                    "{r}: authentication required but no credentials were found (use \"docker login {r}\", $SITEFILE_REGISTRY_AUTH, or --username with --password-stdin)",
                    r = self.registry
                ),
            ));
        }
        if chal.scheme == "basic" {
            let c = creds.as_ref().filter(|c| c.username.as_deref().is_some_and(|u| !u.is_empty()) && c.password.is_some());
            let Some(c) = c else {
                return Err(SitefileError::plain(E_AUTH, format!("{}: Basic authentication needs a username and password", self.registry)));
            };
            return Ok(Self::basic_header(c));
        }
        if chal.scheme == "bearer" {
            let s = chal.params.get("scope").filter(|s| !s.is_empty()).map(String::as_str).unwrap_or(scope);
            return Ok(format!("Bearer {}", self.fetch_token(chal, s, creds.as_ref())?));
        }
        Err(SitefileError::plain(E_AUTH, format!("{}: unsupported authentication scheme '{}'", self.registry, clean(&chal.scheme))))
    }

    /// Send a request, answering a 401 challenge (anonymous token first, then credentials).
    fn call(&self, method: &str, url: &str, scope: &str, headers: &[(String, String)], body: Option<&Body<'_>>, timeout: Option<Duration>) -> Result<ureq::Response> {
        self.init()?;
        let mut used_creds = false;
        let mut attempt = 0;
        loop {
            let token = self.tokens.lock().unwrap().get(scope).cloned();
            let res = self.follow(method, url, headers, body, token.as_deref(), timeout)?;
            if res.status() != 401 || attempt >= 2 {
                return Ok(res);
            }
            let Some(chal) = parse_challenge(res.header("www-authenticate")) else { return Ok(res) };
            drop(res);
            // Pushing needs credentials from the start; for pulls try an anonymous token before them.
            let mut want_creds = scope.split(' ').any(|x| x.ends_with(",push") || x.ends_with(":push"));
            if attempt == 1 {
                if used_creds {
                    return Err(SitefileError::plain(
                        E_AUTH,
                        format!("{}: access denied (the credentials were rejected or lack permission)", self.registry),
                    ));
                }
                want_creds = true;
            }
            let tok = self.authenticate(&chal, scope, want_creds)?;
            self.tokens.lock().unwrap().insert(scope.to_string(), tok);
            used_creds = want_creds || chal.scheme == "basic";
            attempt += 1;
        }
    }

    fn v2(&self, p: &str) -> Result<String> {
        Ok(format!("{}v2/{p}", self.init()?.href))
    }

    fn fail<T>(&self, res: ureq::Response, what: &str) -> Result<T> {
        let status = res.status();
        let mut detail = String::new();
        if let Ok(b) = read_body(res, 64 * 1024) {
            if let Ok(j) = serde_json::from_slice::<Value>(&b) {
                let errs = j.get("errors").and_then(Value::as_array).cloned().unwrap_or_default();
                detail = errs
                    .iter()
                    .take(3)
                    .map(|e| {
                        [e.get("code"), e.get("message")].iter().filter_map(|x| x.and_then(Value::as_str)).map(clean).collect::<Vec<_>>().join(": ")
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
            }
        }
        let msg = format!("{}: {what} failed: HTTP {status}{}", self.registry, if detail.is_empty() { String::new() } else { format!(" ({detail})") });
        Err(match status {
            401 | 403 => SitefileError::plain(E_AUTH, format!("{msg}; check your credentials and permissions")),
            404 => SitefileError::plain(E_NOT_FOUND, msg),
            _ => SitefileError::plain(E_REGISTRY, msg),
        })
    }

    fn repo_path<'r>(&self, repo: &'r str) -> Result<&'r str> {
        if !valid_repo(repo) {
            return Err(SitefileError::plain(E_USAGE, format!("invalid repository name '{repo}'")));
        }
        Ok(repo)
    }

    // ------------------------------------------------------------ API

    /// `GET /v2/`: confirms the endpoint speaks the distribution API (authenticating if it demands it).
    pub fn ping(&self) -> Result<()> {
        let url = self.v2("")?;
        let res = self.call("GET", &url, "", &[], None, None)?;
        if res.status() == 200 {
            return Ok(());
        }
        self.fail(res, "GET /v2/")
    }

    pub fn get_manifest(&self, repo: &str, reference: &str) -> Result<ManifestResponse> {
        if !valid_reference(reference) {
            return Err(SitefileError::plain(E_USAGE, format!("invalid reference '{reference}'")));
        }
        let url = self.v2(&format!("{}/manifests/{reference}", self.repo_path(repo)?))?;
        let headers = [("accept".to_string(), MANIFEST_ACCEPT.to_string())];
        let res = self.call("GET", &url, &format!("repository:{repo}:pull"), &headers, None, None)?;
        if res.status() != 200 {
            return self.fail(res, &format!("GET manifest {repo}:{}", clean(reference)));
        }
        let claimed = res.header("docker-content-digest").map(str::to_string);
        let content_type = res.header("content-type").unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let bytes = read_body(res, self.limits.max_manifest_bytes)?;
        let digest = sha256(&bytes);
        if reference.starts_with("sha256:") && digest != reference {
            return Err(SitefileError::plain(
                E_INTEGRITY,
                format!("{}/{repo}: manifest content does not match the requested digest {reference}", self.registry),
            ));
        }
        if let Some(c) = claimed.filter(|c| *c != digest) {
            return Err(SitefileError::plain(
                E_INTEGRITY,
                format!("{}/{repo}: manifest does not match the digest the registry reported ({})", self.registry, clean(&c)),
            ));
        }
        let mut media_type = content_type;
        if media_type.is_empty() || media_type == "application/json" || media_type == "text/plain" {
            if let Some(mt) = serde_json::from_slice::<Value>(&bytes).ok().and_then(|j| j.get("mediaType").and_then(Value::as_str).map(str::to_string)) {
                media_type = mt;
            }
        }
        Ok(ManifestResponse { bytes, digest, media_type })
    }

    /// `GET /v2/<repo>/tags/list`, following `Link: <...>; rel="next"` pages on this registry (at most
    /// 10 pages of 1000). Tags that are not valid tag names are dropped.
    pub fn list_tags(&self, repo: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut url = self.v2(&format!("{}/tags/list?n=1000", self.repo_path(repo)?))?;
        for _ in 0..10 {
            let res = self.call("GET", &url, &format!("repository:{repo}:pull"), &[], None, None)?;
            if res.status() != 200 {
                return self.fail(res, &format!("GET tags {repo}"));
            }
            let link = res.header("link").map(str::to_string);
            let bytes = read_body(res, self.limits.max_manifest_bytes)?;
            let j: Value = serde_json::from_slice(&bytes)
                .map_err(|_| SitefileError::plain(E_REGISTRY, format!("{}/{repo}: the tag list is not valid JSON", self.registry)))?;
            for t in j.get("tags").and_then(Value::as_array).into_iter().flatten() {
                if let Some(t) = t.as_str().filter(|t| is_tag(t)) {
                    out.push(t.to_string());
                }
            }
            let Some(next) = link.as_deref().and_then(next_link) else { break };
            url = format!("{}{next}", self.init()?.href.trim_end_matches('/'));
        }
        Ok(out)
    }

    pub fn has_blob(&self, repo: &str, digest: &str) -> Result<bool> {
        let url = self.v2(&format!("{}/blobs/{digest}", self.repo_path(repo)?))?;
        let res = self.call("HEAD", &url, &format!("repository:{repo}:pull,push"), &[], None, None)?;
        match res.status() {
            200 => Ok(true),
            404 => Ok(false),
            _ => self.fail(res, &format!("HEAD blob {digest}")),
        }
    }

    /// Stream a blob's bytes (redirects followed under the policy above). The caller verifies size and digest.
    pub fn get_blob(&self, repo: &str, desc: &Descriptor) -> Result<BlobReader> {
        if desc.size > self.limits.max_blob_bytes {
            return Err(SitefileError::plain(
                E_TOO_LARGE,
                format!("blob {} is {} bytes, exceeding the limit of {} bytes", desc.digest, desc.size, self.limits.max_blob_bytes),
            ));
        }
        let url = self.v2(&format!("{}/blobs/{}", self.repo_path(repo)?, desc.digest))?;
        let started = Instant::now();
        let res = self.call("GET", &url, &format!("repository:{repo}:pull"), &[], None, None)?;
        if res.status() != 200 {
            return self.fail(res, &format!("GET blob {}", desc.digest));
        }
        let enc = res.header("content-encoding").unwrap_or("").trim().to_ascii_lowercase();
        if !enc.is_empty() && enc != "identity" {
            return Err(SitefileError::plain(
                E_REGISTRY,
                format!("{}: blob {} came with content-encoding '{}'; only identity is accepted", self.registry, desc.digest, clean(&enc)),
            ));
        }
        if let Some(declared) = res.header("content-length").and_then(|h| h.trim().parse::<f64>().ok()).filter(|d| d.is_finite()) {
            if declared > desc.size as f64 {
                return Err(SitefileError::plain(
                    E_TOO_LARGE,
                    format!("blob {}: the server sent {} bytes for a {}-byte blob", desc.digest, crate::values::js_number(declared), desc.size),
                ));
            }
        }
        let shown = redact_url(res.get_url());
        Ok(BlobReader {
            inner: res.into_reader(),
            deadline: started + Duration::from_millis(self.limits.blob_deadline_ms),
            deadline_ms: self.limits.blob_deadline_ms,
            digest: desc.digest.clone(),
            url: shown,
            timeout_ms: self.limits.timeout_ms,
            error: None,
        })
    }

    /// Make a blob exist in `repo`: tries a cross-repository mount from
    /// `mount_from` (same registry) when given, else a monolithic upload (POST then PUT).
    pub fn push_blob(&self, repo: &str, desc: &Descriptor, body: &Body<'_>, mount_from: Option<&str>) -> Result<BlobPush> {
        let scope = match mount_from {
            Some(m) => format!("repository:{repo}:pull,push repository:{m}:pull"),
            None => format!("repository:{repo}:pull,push"),
        };
        let mut start = self.v2(&format!("{}/blobs/uploads/", self.repo_path(repo)?))?;
        if let Some(m) = mount_from {
            let from = self.repo_path(m)?;
            start = edit_query(&start, &["mount", "from"], &[("mount", &desc.digest), ("from", from)], false)
                .ok_or_else(|| SitefileError::plain(E_REGISTRY, format!("{}: invalid upload URL", self.registry)))?;
        }
        let post = self.call("POST", &start, &scope, &[], None, None)?;
        match post.status() {
            201 => return Ok(BlobPush::Mounted),
            202 => {}
            _ => return self.fail(post, &format!("start upload of {}", desc.digest)),
        }
        let Some(loc) = post.header("location").map(str::to_string) else {
            return Err(SitefileError::plain(E_REGISTRY, format!("{}: upload response has no Location header", self.registry)));
        };
        drop(post);
        let target = join_url(&start, &loc)
            .and_then(|t| edit_query(&t, &["digest"], &[("digest", &desc.digest)], false))
            .ok_or_else(|| SitefileError::plain(E_REGISTRY, format!("{}: invalid upload Location", self.registry)))?;
        let headers = [("content-type".to_string(), "application/octet-stream".to_string())];
        let put = self.call("PUT", &target, &scope, &headers, Some(body), Some(Duration::from_millis(self.limits.blob_deadline_ms)))?;
        match put.status() {
            201 | 204 => Ok(BlobPush::Uploaded),
            _ => self.fail(put, &format!("upload of {}", desc.digest)),
        }
    }

    /// `PUT` a manifest at a tag or digest; returns the digest the registry confirms.
    pub fn put_manifest(&self, repo: &str, reference: &str, bytes: &[u8], media_type: &str) -> Result<String> {
        if !valid_reference(reference) {
            return Err(SitefileError::plain(E_USAGE, format!("invalid reference '{reference}'")));
        }
        let url = self.v2(&format!("{}/manifests/{reference}", self.repo_path(repo)?))?;
        let headers = [("content-type".to_string(), media_type.to_string())];
        let res = self.call("PUT", &url, &format!("repository:{repo}:pull,push"), &headers, Some(&Body::Bytes(bytes)), None)?;
        if res.status() != 201 && res.status() != 200 {
            return self.fail(res, &format!("PUT manifest {repo}:{}", clean(reference)));
        }
        let digest = sha256(bytes);
        if let Some(c) = res.header("docker-content-digest").filter(|c| *c != digest) {
            return Err(SitefileError::plain(
                E_INTEGRITY,
                format!("{}/{repo}: the registry stored the manifest as {}, expected {digest}", self.registry, clean(c)),
            ));
        }
        Ok(digest)
    }
}

fn read_body(res: ureq::Response, max: u64) -> Result<Vec<u8>> {
    let mut r = res.into_reader();
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if (out.len() + n) as u64 > max {
                    return Err(SitefileError::plain(E_TOO_LARGE, format!("registry response exceeded {max} bytes")));
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let code = if is_timeout(&e) { E_FETCH_TIMEOUT } else { E_REGISTRY };
                return Err(SitefileError::plain(code, format!("error reading registry response: {e}")));
            }
        }
    }
}

// ================================================================ operations

fn is_config_type(t: &str) -> bool {
    t == media::CONFIG || t == media::DOCKER_CONFIG
}
fn is_index_type(t: &str) -> bool {
    t == media::INDEX || t == media::DOCKER_LIST
}
fn is_manifest_type(t: &str) -> bool {
    t == media::MANIFEST || t == media::DOCKER_MANIFEST
}

/// Pick the manifest descriptor for `os/arch` from an image index / manifest list.
pub fn select_platform(index_bytes: &[u8], platform: &str) -> Result<Descriptor> {
    let (os, arch) = parse_platform(Some(platform))?;
    let j: Value = serde_json::from_slice(index_bytes)
        .map_err(|_| SitefileError::plain(E_REGISTRY, "the registry returned an unreadable image index"))?;
    let list = j.get("manifests").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut have: Vec<String> = Vec::new();
    let mut hits: Vec<(Descriptor, String)> = Vec::new();
    let js = |v: Option<&Value>| match v {
        Some(Value::String(s)) => s.clone(),
        None => "undefined".into(),
        Some(v) => v.to_string(),
    };
    for m in &list {
        let Some(p) = m.get("platform").filter(|p| p.is_object()) else { continue };
        let digest = m.get("digest").and_then(Value::as_str).filter(|d| is_integrity(d));
        let size = m.get("size").and_then(Value::as_u64);
        let (Some(digest), Some(size)) = (digest, size) else { continue };
        if p.get("architecture").and_then(Value::as_str) == Some("unknown") {
            continue;
        }
        let variant = p.get("variant").and_then(Value::as_str).unwrap_or("").to_string();
        let (p_os, p_arch) = (js(p.get("os")), js(p.get("architecture")));
        have.push(if variant.is_empty() { format!("{p_os}/{p_arch}") } else { format!("{p_os}/{p_arch}/{variant}") });
        if p_os == os && p_arch == arch {
            let mt = match m.get("mediaType") {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => media::MANIFEST.to_string(),
                Some(v) => v.to_string(),
            };
            hits.push((Descriptor { media_type: mt, digest: digest.to_string(), size, annotations: None }, variant));
        }
    }
    let best = hits.iter().find(|h| h.1.is_empty()).or(hits.first());
    match best {
        Some((d, _)) => Ok(d.clone()),
        None => {
            let mut uniq: Vec<String> = Vec::new();
            for h in have {
                if !uniq.contains(&h) {
                    uniq.push(h);
                }
            }
            let avail = if uniq.is_empty() { "none".to_string() } else { uniq.join(", ") };
            Err(SitefileError::plain(E_REGISTRY, format!("the image has no {platform} variant (available: {avail}); pass --platform")))
        }
    }
}

fn sniff_media_type(bytes: &[u8]) -> String {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(j) => {
            if let Some(s) = j.get("mediaType").and_then(Value::as_str) {
                return s.to_string();
            }
            if j.get("manifests").is_some_and(Value::is_array) {
                media::INDEX.into()
            } else {
                media::MANIFEST.into()
            }
        }
        Err(_) => String::new(),
    }
}

fn fetch_manifest_cached(store: &Store, client: &RegistryClient, repo: &str, reference: &str) -> Result<ManifestResponse> {
    if reference.starts_with("sha256:") && store.has_blob(reference, None) {
        if let Ok(bytes) = store.read_blob(reference, None) {
            let media_type = sniff_media_type(&bytes);
            return Ok(ManifestResponse { bytes, digest: reference.to_string(), media_type });
        }
        // corrupt cache entry: fetch again
    }
    let mut m = client.get_manifest(repo, reference)?;
    store.put_small_blob(&m.bytes)?;
    if m.media_type.is_empty() {
        m.media_type = sniff_media_type(&m.bytes);
    }
    Ok(m)
}

/// Resolve a tag to the digest it currently points at (one manifest GET), caching the manifest.
pub fn resolve_digest(store: &Store, client: &RegistryClient, r: &ImageRef) -> Result<String> {
    Ok(fetch_manifest_cached(store, client, &r.name, r.tag.as_deref().unwrap_or("latest"))?.digest)
}

#[derive(Default)]
pub struct PullImageOptions<'a> {
    /// `os/arch` used to pick from an index (default linux/amd64).
    pub platform: Option<String>,
    /// Fail when the image's own platform differs from `platform` (BASE does; `pull` only with --platform).
    pub enforce_platform: bool,
    /// Fetch this digest (an index or a manifest) instead of resolving the reference's tag.
    pub root_digest: Option<String>,
    /// Tag the image locally as the reference (registry/name:tag).
    pub tag: bool,
    pub on_event: Option<&'a dyn Fn(&str)>,
}

#[derive(Debug, Clone)]
pub struct PullResult {
    /// The platform manifest digest: the local image ID.
    pub id: String,
    /// What the reference resolved to (an index digest for multi-platform images, else `id`).
    pub root_digest: String,
    pub manifest: ParsedManifest,
    pub config: Map<String, Value>,
}

/// Pull an image into the store: verifies every blob's sha256 while streaming, then registers the manifest.
pub fn pull_image(store: &Store, client: &RegistryClient, r: &ImageRef, o: &PullImageOptions<'_>) -> Result<PullResult> {
    let Some(registry) = r.registry.clone() else {
        return Err(SitefileError::plain(E_USAGE, "internal: pullImage needs a registry reference"));
    };
    let platform = o.platform.clone().unwrap_or_else(|| "linux/amd64".into());
    let say = |m: &str| {
        if let Some(f) = o.on_event {
            f(m)
        }
    };
    let repo = r.name.as_str();
    let reference = o.root_digest.clone().or_else(|| r.digest.clone()).or_else(|| r.tag.clone()).unwrap_or_else(|| "latest".into());
    let limits = client.limits;
    let reg = &client.registry;

    let root = fetch_manifest_cached(store, client, repo, &reference)?;
    let manifest = if is_index_type(&root.media_type) {
        let d = select_platform(&root.bytes, &platform)?;
        fetch_manifest_cached(store, client, repo, &d.digest)?
    } else if is_manifest_type(&root.media_type) {
        root.clone()
    } else {
        let mt = if root.media_type.is_empty() { "unknown" } else { &root.media_type };
        return Err(SitefileError::plain(
            E_UNSUPPORTED,
            format!("{reg}/{repo}: unsupported manifest type '{mt}' (expected an OCI or Docker v2 image)"),
        ));
    };

    let parsed = parse_manifest(&manifest.bytes).map_err(|e| SitefileError::plain(E_REGISTRY, format!("{reg}/{repo}: {e}")))?;
    if !is_config_type(&parsed.config.media_type) {
        return Err(SitefileError::plain(
            E_UNSUPPORTED,
            format!("{reg}/{repo}: the artifact is not a container image (config type '{}')", parsed.config.media_type),
        ));
    }
    let blobs: Vec<&Descriptor> = std::iter::once(&parsed.config).chain(parsed.layers.iter()).collect();
    let mut total: u64 = 0;
    for d in &blobs {
        if d.size > limits.max_blob_bytes {
            return Err(SitefileError::plain(
                E_TOO_LARGE,
                format!("blob {} is {} bytes, exceeding the limit of {} bytes (--max-download-bytes)", d.digest, d.size, limits.max_blob_bytes),
            ));
        }
        total = total.saturating_add(d.size);
    }
    if total > limits.max_total_bytes {
        return Err(SitefileError::plain(
            E_TOO_LARGE,
            format!("{reg}/{repo} totals {total} bytes, exceeding the limit of {} bytes", limits.max_total_bytes),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for d in blobs.into_iter().filter(|d| seen.insert(d.digest.clone())) {
        if store.has_blob(&d.digest, Some(d.size)) {
            store.touch_blob(&d.digest); // recently used: protected from the cache size limit
            say(&format!("{}: already exists", short_id(&d.digest)));
            continue;
        }
        let mut reader = client.get_blob(repo, d)?;
        if let Err(e) = store.import_blob(&d.digest, d.size, &mut reader, Some(limits.max_blob_bytes)) {
            return Err(reader.take_error().unwrap_or(e));
        }
        say(&format!("{}: pulled ({} bytes)", short_id(&d.digest), d.size));
    }

    let config_bytes = store.read_blob(&parsed.config.digest, Some(parsed.config.size))?;
    let config: Map<String, Value> = serde_json::from_slice::<Value>(&config_bytes)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .ok_or_else(|| SitefileError::plain(E_REGISTRY, format!("{reg}/{repo}: the image config is not a JSON object")))?;
    if o.enforce_platform {
        let (os, arch) = parse_platform(Some(&platform))?;
        let cfg = parse_config(&config_bytes).map_err(|e| SitefileError::plain(E_REGISTRY, format!("{reg}/{repo}: invalid image config: {e}")))?;
        if cfg.os != os || cfg.architecture != arch {
            return Err(SitefileError::plain(
                E_REGISTRY,
                format!("{reg}/{repo} is {}/{}, not {platform}; pass --platform", cfg.os, cfg.architecture),
            ));
        }
    }
    let tag_ref = o.tag.then(|| ImageRef {
        registry: Some(registry.clone()),
        name: r.name.clone(),
        tag: Some(r.tag.clone().unwrap_or_else(|| "latest".into())),
        digest: None,
    });
    let id = store.add_manifest(&manifest.bytes, tag_ref.as_ref())?;
    Ok(PullResult { id, root_digest: root.digest, manifest: parsed, config })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PushResult {
    pub uploaded: usize,
    pub mounted: usize,
    pub existing: usize,
}

/// Push a stored image `id` to `r` (a registry reference): blobs that are
/// missing, then the manifest. Returns the confirmed digest, the manifest size and counts.
pub fn push_image(
    store: &Store,
    client: &RegistryClient,
    id: &str,
    r: &ImageRef,
    on_event: Option<&dyn Fn(&str)>,
) -> Result<(String, u64, PushResult)> {
    let say = |m: &str| {
        if let Some(f) = on_event {
            f(m)
        }
    };
    let repo = r.name.as_str();
    if let Some(d) = r.digest.as_deref().filter(|d| *d != id) {
        return Err(SitefileError::plain(E_USAGE, format!("the reference pins {d} but the local image is {id}")));
    }
    let manifest_bytes = store.read_blob(id, None)?;
    let parsed = parse_manifest(&manifest_bytes).map_err(|e| SitefileError::plain(E_STORE, format!("image {id}: {e}")))?;
    // Layers of the BASE image can be mounted from its repository when it lives on this registry.
    let mut mount_from: Option<String> = None;
    let mut base_layers: Vec<String> = Vec::new();
    if let Some(a) = parsed.annotations.get(crate::export::oci::ann::BASE_NAME) {
        if let Ok(b) = parse_ref(a).map(|b| with_default_registry(&b)) {
            if b.registry.as_deref() == Some(client.registry.as_str()) && b.name != repo {
                mount_from = Some(b.name);
                let n = parsed.layers.len().saturating_sub(1);
                base_layers = parsed.layers[..n].iter().map(|l| l.digest.clone()).collect();
            }
        }
    }
    client.ping()?;
    let mut counts = PushResult::default();
    let mut seen = std::collections::HashSet::new();
    for d in std::iter::once(&parsed.config).chain(parsed.layers.iter()) {
        if !seen.insert(d.digest.clone()) {
            continue;
        }
        if client.has_blob(repo, &d.digest)? {
            counts.existing += 1;
            say(&format!("{}: layer already exists", short_id(&d.digest)));
            continue;
        }
        if !store.has_blob(&d.digest, Some(d.size)) {
            return Err(SitefileError::plain(E_STORE, format!("blob {} is missing from the local store; cannot push", d.digest)));
        }
        let digest = d.digest.clone();
        let open = move || -> Result<Box<dyn Read + Send>> { Ok(Box::new(store.blob_reader(&digest)?)) };
        let body = Body::Stream { length: d.size, open: &open };
        let mount = if base_layers.contains(&d.digest) { mount_from.as_deref() } else { None };
        match client.push_blob(repo, d, &body, mount)? {
            BlobPush::Mounted => {
                counts.mounted += 1;
                say(&format!("{}: mounted from {} ({} bytes)", short_id(&d.digest), mount.unwrap_or(""), d.size));
            }
            BlobPush::Uploaded => {
                counts.uploaded += 1;
                say(&format!("{}: pushed ({} bytes)", short_id(&d.digest), d.size));
            }
        }
    }
    let media_type = serde_json::from_slice::<Value>(&manifest_bytes)
        .ok()
        .and_then(|j| j.get("mediaType").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| media::MANIFEST.to_string());
    let reference = r.tag.clone().or_else(|| r.digest.clone()).unwrap_or_else(|| "latest".into());
    let digest = client.put_manifest(repo, &reference, &manifest_bytes, &media_type)?;
    if digest != id {
        return Err(SitefileError::plain(E_INTEGRITY, format!("the registry reports digest {digest}, expected {id}")));
    }
    Ok((digest, manifest_bytes.len() as u64, counts))
}

// ================================================================ commands

/// Options for [`push`] (the `push` command). The CLI reads `--password-stdin` and passes the result as `explicit_credentials`.
#[derive(Default)]
pub struct PushOptions<'a> {
    pub registry: RegistryOptions,
    /// `--username` + `--password-stdin`.
    pub explicit_credentials: Option<Credentials>,
    /// Environment for credentials and `$SITEFILE_INSECURE_REGISTRIES` (default: the process environment).
    pub env: Option<Env>,
    /// Called with every output line as it happens (the same lines end up in the report).
    pub on_event: Option<&'a dyn Fn(&str)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReport {
    pub digest: String,
    pub size: u64,
    pub uploaded: usize,
    pub mounted: usize,
    pub existing: usize,
    /// Output lines, exactly as the TypeScript CLI prints them on stdout.
    pub lines: Vec<String>,
}

/// Options for [`pull`] (the `pull` command).
#[derive(Default)]
pub struct PullOptions<'a> {
    pub registry: RegistryOptions,
    /// `--platform`: also enforces that the image is that platform.
    pub platform: Option<String>,
    pub explicit_credentials: Option<Credentials>,
    pub env: Option<Env>,
    pub on_event: Option<&'a dyn Fn(&str)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullReport {
    pub id: String,
    pub root_digest: String,
    pub lines: Vec<String>,
}

struct Lines<'a> {
    lines: Vec<String>,
    on_event: Option<&'a dyn Fn(&str)>,
}

impl Lines<'_> {
    fn say(&mut self, s: String) {
        if let Some(f) = self.on_event {
            f(&s);
        }
        self.lines.push(s);
    }
}

fn effective(options: &RegistryOptions, env: &Env) -> RegistryOptions {
    RegistryOptions { insecure_registries: insecure_registries_from_env(&options.insecure_registries, env), ..options.clone() }
}

/// `sitefile push <registry/name[:tag]>`. Prints (via `on_event` and the report):
/// `The push refers to repository [reg/name]`, one line per blob, then `<tag>: digest: <d> size: <n>`.
pub fn push(store: &Store, ref_str: &str, o: &PushOptions<'_>) -> Result<PushReport> {
    let ref0 = parse_ref(ref_str)?;
    if ref0.registry.is_none() {
        return Err(SitefileError::plain(
            E_USAGE,
            format!("'{ref_str}' has no registry host; tag the image first, e.g. sitefile tag {ref_str} localhost:5000/{ref_str}"),
        ));
    }
    let r = canonical_ref(&ref0);
    let key = ref_key(&ImageRef { digest: None, ..r.clone() });
    let id = if r.digest.is_some() && r.tag.is_none() { r.digest.clone() } else { store.refs()?.get(&key).cloned() };
    let id = match id {
        Some(id) if store.has_image(&id)? => id,
        _ => return Err(SitefileError::plain(E_NOT_FOUND, format!("no local image {key}; tag one first: sitefile tag <image> {key}"))),
    };
    let env = o.env.clone().unwrap_or_else(process_env);
    let registry = r.registry.clone().unwrap_or_default();
    let client = make_client(&registry, &effective(&o.registry, &env), o.explicit_credentials.clone(), &env);
    let out = std::cell::RefCell::new(Lines { lines: vec![], on_event: o.on_event });
    out.borrow_mut().say(format!("The push refers to repository [{registry}/{}]", r.name));
    let say = |m: &str| out.borrow_mut().say(m.to_string());
    let (digest, size, c) = push_image(store, &client, &id, &r, Some(&say))?;
    out.borrow_mut().say(format!("{}: digest: {digest} size: {size}", r.tag.as_deref().unwrap_or("digest")));
    let lines = out.into_inner().lines;
    Ok(PushReport { digest, size, uploaded: c.uploaded, mounted: c.mounted, existing: c.existing, lines })
}

/// `sitefile pull <[registry/]name[:tag][@sha256:...]>`. A bare name means
/// Docker Hub. Prints one line per blob, `Digest: <root>`, then
/// `Pulled <ref> (image <short id>)`.
pub fn pull(store: &Store, ref_str: &str, o: &PullOptions<'_>) -> Result<PullReport> {
    let r = with_default_registry(&parse_ref(ref_str)?);
    let env = o.env.clone().unwrap_or_else(process_env);
    let registry = r.registry.clone().unwrap_or_default();
    let client = make_client(&registry, &effective(&o.registry, &env), o.explicit_credentials.clone(), &env);
    let out = std::cell::RefCell::new(Lines { lines: vec![], on_event: o.on_event });
    let say = |m: &str| out.borrow_mut().say(m.to_string());
    let res = pull_image(
        store,
        &client,
        &r,
        &PullImageOptions {
            platform: o.platform.clone(),
            enforce_platform: o.platform.is_some(),
            root_digest: None,
            tag: r.digest.is_none(),
            on_event: Some(&say),
        },
    )?;
    out.borrow_mut().say(format!("Digest: {}", res.root_digest));
    let shown = match &r.digest {
        None => ref_key(&r),
        Some(d) => format!("{registry}/{}@{d}", r.name),
    };
    out.borrow_mut().say(format!("Pulled {shown} (image {})", short_id(&res.id)));
    let lines = out.into_inner().lines;
    Ok(PullReport { id: res.id, root_digest: res.root_digest, lines })
}

// ================================================================ test support

/// An in-memory OCI distribution registry on a loopback TcpListener (thread
/// per connection), just enough for push/pull/BASE tests. Port of
/// `test/helpers/mock-registry.ts`.
#[cfg(test)]
pub(crate) mod mock {
    use crate::digest::sha256;
    use crate::export::archive_writers::{gzip_tar, write_tar, WriterOptions};
    use crate::export::oci::canonical_json;
    use crate::tree::{make_file, Tree};
    use base64::Engine;
    use serde_json::{json, Value};
    use std::collections::{HashMap, HashSet};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Auth {
        #[default]
        None,
        Basic,
        /// Token endpoint on the same server.
        Bearer,
    }

    #[derive(Debug, Clone)]
    pub struct MockOptions {
        pub auth: Auth,
        pub user: String,
        pub pass: String,
        /// Bearer: allow anonymous pull tokens.
        pub anonymous_pull: bool,
        /// Serve blob GETs through a redirect to this base URL (a CDN).
        pub redirect_blobs_to: Option<String>,
        /// Serve different bytes for this blob digest.
        pub corrupt: Option<String>,
        /// Answer GET manifest by digest with these bytes.
        pub swap_manifest: Option<Vec<u8>>,
        /// Largest page of tags/list, whatever ?n= asks for (pagination test).
        pub tag_page_size: Option<usize>,
    }

    impl Default for MockOptions {
        fn default() -> Self {
            MockOptions {
                auth: Auth::None,
                user: "u".into(),
                pass: "p".into(),
                anonymous_pull: true,
                redirect_blobs_to: None,
                corrupt: None,
                swap_manifest: None,
                tag_page_size: None,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct MockRequest {
        pub method: String,
        pub url: String,
        pub authorization: Option<String>,
    }

    #[derive(Default)]
    pub struct State {
        pub blobs: HashMap<String, Vec<u8>>,
        repo_blobs: HashMap<String, HashSet<String>>,
        /// `repo@digest` and `repo:tag` -> manifest bytes.
        pub manifests: HashMap<String, Vec<u8>>,
        uploads: HashMap<String, String>,
        pub requests: Vec<MockRequest>,
        pub token_calls: Vec<String>,
        pub options: MockOptions,
        counter: u64,
    }

    impl State {
        fn has_blob(&self, repo: &str, d: &str) -> bool {
            self.repo_blobs.get(repo).is_some_and(|s| s.contains(d)) && self.blobs.contains_key(d)
        }
        fn add_blob(&mut self, repo: &str, d: &str, b: Vec<u8>) {
            self.blobs.insert(d.to_string(), b);
            self.repo_blobs.entry(repo.to_string()).or_default().insert(d.to_string());
        }
    }

    pub struct SeedImage<'a> {
        /// Files per layer, bottom first.
        pub layers: Vec<Vec<(&'a str, &'a str)>>,
        pub config: Value,
        pub arch: &'a str,
        /// Docker v2 media types instead of OCI.
        pub docker: bool,
    }

    impl Default for SeedImage<'_> {
        fn default() -> Self {
            SeedImage { layers: vec![], config: json!({}), arch: "amd64", docker: false }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Seeded {
        pub manifest_digest: String,
        pub manifest_bytes: Vec<u8>,
        pub layer_digests: Vec<String>,
        pub diff_ids: Vec<String>,
    }

    pub struct HttpReq {
        pub method: String,
        /// Request target as sent (path + query).
        pub target: String,
        pub headers: HashMap<String, String>,
        pub body: Vec<u8>,
    }

    pub fn pct_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'+' {
                out.push(b' ');
            } else if b[i] == b'%' && i + 3 <= b.len() {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            } else {
                out.push(b[i]);
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub fn query_params(target: &str) -> Vec<(String, String)> {
        let q = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        q.split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (pct_decode(k), pct_decode(v))
            })
            .collect()
    }

    fn read_request(stream: &TcpStream) -> Option<HttpReq> {
        // Only plain HTTP: a TLS ClientHello (the https probe) is closed at once.
        let mut first = [0u8; 1];
        if stream.peek(&mut first).ok()? == 0 || !first[0].is_ascii_uppercase() {
            return None;
        }
        let mut r = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        r.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();
        let mut headers = HashMap::new();
        loop {
            let mut h = String::new();
            r.read_line(&mut h).ok()?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let mut body = Vec::new();
        if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
            loop {
                let mut sz = String::new();
                r.read_line(&mut sz).ok()?;
                let n = usize::from_str_radix(sz.trim().split(';').next()?, 16).ok()?;
                let mut chunk = vec![0u8; n + 2];
                r.read_exact(&mut chunk).ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
        } else if let Some(n) = headers.get("content-length").and_then(|n| n.parse::<usize>().ok()) {
            body = vec![0u8; n];
            r.read_exact(&mut body).ok()?;
        }
        Some(HttpReq { method, target, headers, body })
    }

    pub fn respond(stream: &mut TcpStream, status: u16, body: &[u8], headers: &[(&str, String)], head: bool) {
        let mut out = format!("HTTP/1.1 {status} X\r\nconnection: close\r\n");
        let mut has_len = false;
        for (k, v) in headers {
            has_len |= k.eq_ignore_ascii_case("content-length");
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if !has_len {
            out.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        out.push_str("\r\n");
        let _ = stream.write_all(out.as_bytes());
        if !head {
            let _ = stream.write_all(body);
        }
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    /// Serve `handler` on 127.0.0.1 (thread per connection); returns the port.
    pub fn serve(handler: impl Fn(HttpReq, &mut TcpStream) + Send + Sync + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let h = handler.clone();
                std::thread::spawn(move || {
                    if let Some(req) = read_request(&s) {
                        h(req, &mut s);
                    }
                });
            }
        });
        port
    }

    pub fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
    }

    pub struct MockRegistry {
        /// `127.0.0.1:<port>`: loopback, so plain http is allowed.
        pub host: String,
        pub state: Arc<Mutex<State>>,
    }

    impl MockRegistry {
        pub fn start(options: MockOptions) -> MockRegistry {
            let state = Arc::new(Mutex::new(State { options, ..Default::default() }));
            let st = state.clone();
            let port_cell = Arc::new(Mutex::new(0u16));
            let pc = port_cell.clone();
            let port = serve(move |req, s| {
                let port = *pc.lock().unwrap();
                handle(&st, port, req, s)
            });
            *port_cell.lock().unwrap() = port;
            MockRegistry { host: format!("127.0.0.1:{port}"), state }
        }
        pub fn set_options(&self, f: impl FnOnce(&mut MockOptions)) {
            f(&mut self.state.lock().unwrap().options);
        }
        pub fn requests(&self) -> Vec<MockRequest> {
            self.state.lock().unwrap().requests.clone()
        }
        pub fn token_calls(&self) -> Vec<String> {
            self.state.lock().unwrap().token_calls.clone()
        }
        pub fn has_manifest(&self, key: &str) -> bool {
            self.state.lock().unwrap().manifests.contains_key(key)
        }
        pub fn set_manifest(&self, key: &str, bytes: &[u8]) {
            self.state.lock().unwrap().manifests.insert(key.to_string(), bytes.to_vec());
        }
        pub fn has_blob(&self, d: &str) -> bool {
            self.state.lock().unwrap().blobs.contains_key(d)
        }

        pub fn seed_image(&self, repo: &str, tag: &str, img: SeedImage<'_>) -> Seeded {
            let mut st = self.state.lock().unwrap();
            let mut layer_digests = Vec::new();
            let mut diff_ids = Vec::new();
            for files in &img.layers {
                let tree = Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "seed", 0o644)))).unwrap();
                let tar = write_tar(&tree, &WriterOptions { epoch: Some(0), dirs: vec![] }).unwrap();
                let gz = gzip_tar(&tar);
                let d = sha256(&gz);
                st.add_blob(repo, &d, gz);
                layer_digests.push(d);
                diff_ids.push(sha256(&tar));
            }
            let history: Vec<Value> =
                (0..diff_ids.len()).map(|i| json!({"created": "2024-01-02T03:04:05Z", "created_by": format!("seed layer {i}")})).collect();
            let config = canonical_json(&json!({
                "architecture": img.arch,
                "os": "linux",
                "created": "2024-01-02T03:04:05Z",
                "config": img.config,
                "rootfs": { "type": "layers", "diff_ids": diff_ids },
                "history": history,
            }))
            .into_bytes();
            let config_digest = sha256(&config);
            let config_len = config.len();
            st.add_blob(repo, &config_digest, config);
            let (mt, ct, lt) = if img.docker {
                (
                    "application/vnd.docker.distribution.manifest.v2+json",
                    "application/vnd.docker.container.image.v1+json",
                    "application/vnd.docker.image.rootfs.diff.tar.gzip",
                )
            } else {
                ("application/vnd.oci.image.manifest.v1+json", "application/vnd.oci.image.config.v1+json", "application/vnd.oci.image.layer.v1.tar+gzip")
            };
            let layers: Vec<Value> = layer_digests.iter().map(|d| json!({"mediaType": lt, "digest": d, "size": st.blobs[d].len()})).collect();
            let manifest_bytes = canonical_json(&json!({
                "schemaVersion": 2,
                "mediaType": mt,
                "config": { "mediaType": ct, "digest": config_digest, "size": config_len },
                "layers": layers,
            }))
            .into_bytes();
            let md = sha256(&manifest_bytes);
            st.manifests.insert(format!("{repo}@{md}"), manifest_bytes.clone());
            st.manifests.insert(format!("{repo}:{tag}"), manifest_bytes.clone());
            Seeded { manifest_digest: md, manifest_bytes, layer_digests, diff_ids }
        }
    }

    fn handle(state: &Arc<Mutex<State>>, port: u16, req: HttpReq, s: &mut TcpStream) {
        let mut st = state.lock().unwrap();
        let path = req.target.split('?').next().unwrap_or("").to_string();
        let auth = req.headers.get("authorization").cloned();
        st.requests.push(MockRequest { method: req.method.clone(), url: req.target.clone(), authorization: auth.clone() });
        let o = st.options.clone();
        let head = req.method == "HEAD";
        let json_h = || vec![("content-type", "application/json".to_string())];
        let params = query_params(&req.target);
        let param = |k: &str| params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());

        if path == "/token" {
            let search = req.target.find('?').map(|i| req.target[i..].to_string()).unwrap_or_default();
            st.token_calls.push(search);
            drop(st);
            let wants_push = params.iter().any(|(k, v)| k == "scope" && v.contains("push"));
            let ok = auth.as_deref() == Some(basic(&o.user, &o.pass).as_str());
            if ok {
                return respond(s, 200, br#"{"token":"tok-full"}"#, &json_h(), false);
            }
            if auth.is_none() && !wants_push && o.anonymous_pull {
                return respond(s, 200, br#"{"token":"tok-anon"}"#, &json_h(), false);
            }
            if auth.is_some() {
                return respond(s, 401, br#"{"errors":[{"code":"UNAUTHORIZED","message":"bad credentials"}]}"#, &[], false);
            }
            // like Docker Hub: a token without the push scope
            return respond(s, 200, br#"{"token":"tok-denied"}"#, &json_h(), false);
        }

        let writes = req.method == "POST" || req.method == "PUT";
        // /v2/<repo>/(blobs/uploads|blobs|manifests)/<rest>
        let parsed = path.strip_prefix("/v2/").and_then(|p| {
            for kind in ["/blobs/uploads", "/blobs/", "/manifests/"] {
                if let Some(i) = p.find(kind) {
                    let rest = p[i + kind.len()..].trim_start_matches('/');
                    return Some((p[..i].to_string(), kind.trim_matches('/').to_string(), rest.to_string()));
                }
            }
            None
        });
        let repo = parsed.as_ref().map(|p| p.0.clone()).unwrap_or_default();
        let unauthorized = br#"{"errors":[{"code":"UNAUTHORIZED","message":"auth"}]}"#;
        if o.auth == Auth::Basic && auth.as_deref() != Some(basic(&o.user, &o.pass).as_str()) {
            drop(st);
            return respond(s, 401, unauthorized, &[("www-authenticate", "Basic realm=\"mock\"".into())], head);
        }
        if o.auth == Auth::Bearer {
            let tok = auth.as_deref().and_then(|a| a.strip_prefix("Bearer "));
            let good = tok == Some("tok-full") || (tok == Some("tok-anon") && !writes);
            if !good {
                let scope = if parsed.is_some() {
                    format!(",scope=\"repository:{repo}:{}\"", if writes { "pull,push" } else { "pull" })
                } else {
                    String::new()
                };
                drop(st);
                let chal = format!("Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\"{scope}");
                return respond(s, 401, unauthorized, &[("www-authenticate", chal)], head);
            }
        }
        if path == "/v2/" || path == "/v2" {
            drop(st);
            return respond(s, 200, b"{}", &[], head);
        }
        // Tag listing, paginated like the distribution spec: ?n=<count>&last=<tag>, with a Link header to the next page.
        if let Some(name) = path.strip_prefix("/v2/").and_then(|p| p.strip_suffix("/tags/list")) {
            let prefix = format!("{name}:");
            let mut all: Vec<String> = st.manifests.keys().filter_map(|k| k.strip_prefix(&prefix).map(str::to_string)).collect();
            all.sort();
            drop(st);
            if all.is_empty() {
                return respond(s, 404, br#"{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known to registry"}]}"#, &[], head);
            }
            let from: Vec<String> = match param("last") {
                Some(last) => all.into_iter().filter(|t| *t > last).collect(),
                None => all,
            };
            let n = param("n").and_then(|n| n.parse::<usize>().ok()).unwrap_or(from.len()).min(o.tag_page_size.unwrap_or(usize::MAX));
            let page: Vec<String> = from.iter().take(n).cloned().collect();
            let mut headers = json_h();
            if from.len() > page.len() {
                headers.push(("link", format!("</v2/{name}/tags/list?n={n}&last={}>; rel=\"next\"", page.last().cloned().unwrap_or_default())));
            }
            let body = serde_json::to_vec(&json!({ "name": name, "tags": page })).unwrap_or_default();
            return respond(s, 200, &body, &headers, head);
        }
        let Some((_, kind, rest)) = parsed else {
            drop(st);
            return respond(s, 404, br#"{"errors":[{"code":"NAME_UNKNOWN","message":"nope"}]}"#, &[], head);
        };
        let m = req.method.as_str();
        if kind == "blobs" && (m == "GET" || m == "HEAD") {
            let Some(b) = st.blobs.get(&rest).cloned().filter(|_| st.has_blob(&repo, &rest)) else {
                drop(st);
                return respond(s, 404, br#"{"errors":[{"code":"BLOB_UNKNOWN","message":"no"}]}"#, &[], head);
            };
            drop(st);
            if head {
                return respond(s, 200, b"", &[("content-length", b.len().to_string())], true);
            }
            if let Some(to) = &o.redirect_blobs_to {
                return respond(s, 307, b"", &[("location", format!("{to}/blob/{}", &rest[7..]))], false);
            }
            let out = if o.corrupt.as_deref() == Some(rest.as_str()) { vec![0x41u8; b.len()] } else { b };
            return respond(s, 200, &out, &[], false);
        }
        if kind == "blobs/uploads" && m == "POST" {
            if let (Some(mount), Some(from)) = (param("mount"), param("from")) {
                if st.has_blob(&from, &mount) {
                    let b = st.blobs[&mount].clone();
                    st.add_blob(&repo, &mount, b);
                    drop(st);
                    return respond(s, 201, b"", &[("location", format!("/v2/{repo}/blobs/{mount}"))], false);
                }
            }
            st.counter += 1;
            let id = format!("upload-{}", st.counter);
            st.uploads.insert(id.clone(), repo.clone());
            drop(st);
            return respond(s, 202, b"", &[("location", format!("/v2/{repo}/blobs/uploads/{id}?_state=x"))], false);
        }
        if kind == "blobs/uploads" && m == "PUT" {
            let d = param("digest").unwrap_or_default();
            if !st.uploads.contains_key(&rest) || sha256(&req.body) != d {
                drop(st);
                return respond(s, 400, br#"{"errors":[{"code":"DIGEST_INVALID","message":"bad digest"}]}"#, &[], false);
            }
            st.uploads.remove(&rest);
            st.add_blob(&repo, &d, req.body.clone());
            drop(st);
            return respond(s, 201, b"", &[("location", format!("/v2/{repo}/blobs/{d}")), ("docker-content-digest", d.clone())], false);
        }
        if kind == "manifests" && m == "PUT" {
            let d = sha256(&req.body);
            st.manifests.insert(format!("{repo}@{d}"), req.body.clone());
            if !rest.starts_with("sha256:") {
                st.manifests.insert(format!("{repo}:{rest}"), req.body.clone());
            }
            drop(st);
            return respond(s, 201, b"", &[("docker-content-digest", d)], false);
        }
        if kind == "manifests" && (m == "GET" || m == "HEAD") {
            let key = if rest.starts_with("sha256:") { format!("{repo}@{rest}") } else { format!("{repo}:{rest}") };
            let Some(mut b) = st.manifests.get(&key).cloned() else {
                drop(st);
                return respond(s, 404, br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"no such manifest"}]}"#, &[], head);
            };
            drop(st);
            if let (Some(sw), true) = (&o.swap_manifest, rest.starts_with("sha256:")) {
                b = sw.clone();
            }
            let mt = serde_json::from_slice::<Value>(&b)
                .ok()
                .and_then(|j| j.get("mediaType").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());
            let d = sha256(&b);
            return respond(s, 200, &b, &[("content-type", mt), ("docker-content-digest", d)], head);
        }
        drop(st);
        respond(s, 405, b"", &[], head)
    }

    /// A tiny "CDN" serving the registry's blobs without authentication; records what it saw.
    pub struct MockCdn {
        pub url: String,
        pub requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    pub fn start_cdn(reg: &MockRegistry) -> MockCdn {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (rq, st) = (requests.clone(), reg.state.clone());
        let port = serve(move |req, s| {
            let authorization = req.headers.get("authorization").cloned();
            rq.lock().unwrap().push(MockRequest { method: req.method.clone(), url: req.target.clone(), authorization });
            let b = req.target.strip_prefix("/blob/").and_then(|hex| st.lock().unwrap().blobs.get(&format!("sha256:{hex}")).cloned());
            match b {
                Some(b) => respond(s, 200, &b, &[], false),
                None => respond(s, 404, b"", &[], false),
            }
        });
        MockCdn { url: format!("http://127.0.0.1:{port}"), requests }
    }

    /// A fresh temp directory (never ~/.sitefile or ~/.docker).
    pub fn tmp(prefix: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let d = std::env::temp_dir().join(format!("sitefile-reg-{prefix}{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An environment that names an empty Docker config dir and nothing else secret.
    pub fn clean_env() -> crate::registry::Env {
        let mut e = crate::registry::Env::new();
        e.insert("DOCKER_CONFIG".into(), tmp("docker-empty-").display().to_string());
        e.insert("HOME".into(), tmp("home-").display().to_string());
        e.insert("PATH".into(), "/usr/bin:/bin".into());
        e
    }
}

#[cfg(test)]
mod tests {
    use super::mock::*;
    use super::*;
    use crate::error::ErrorCode;
    use crate::export::oci::{canonical_json, OciOptions};
    use crate::refs::parse_local_ref;
    use crate::tree::{make_file, Tree};
    use serde_json::json;
    use std::path::Path;

    const SECRET: &str = "s3cr3t-pw-do-not-print";

    fn blog_tree() -> Tree {
        let files = [("/index.html", "<h1>Home</h1>"), ("/style.css", "body{}"), ("/about/index.html", "<h1>About</h1>")];
        Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "test", 0o644)))).unwrap()
    }

    /// A store with the blog site built and tagged `r`.
    fn build_blog(store: &Store, r: &str) -> String {
        let id = store.add_image(&blog_tree(), &OciOptions::default()).unwrap().id;
        store.tag(&id, &parse_local_ref(r).unwrap()).unwrap();
        id
    }

    fn new_store() -> Store {
        Store::open(tmp("home-")).unwrap()
    }

    fn docker_config(config: Value) -> String {
        let dir = tmp("docker-cfg-");
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        dir.display().to_string()
    }

    fn env_with(pairs: &[(&str, &str)]) -> Env {
        let mut e = clean_env();
        for (k, v) in pairs {
            e.insert(k.to_string(), v.to_string());
        }
        e
    }

    fn push_with(store: &Store, r: &str, env: Env) -> Result<PushReport> {
        push(store, r, &PushOptions { env: Some(env), ..Default::default() })
    }

    fn pull_with(store: &Store, r: &str, env: Env) -> Result<PullReport> {
        pull(store, r, &PullOptions { env: Some(env), ..Default::default() })
    }

    fn client(host: &str) -> RegistryClient {
        RegistryClient::new(host, RegistryOptions::default())
    }

    fn code(r: Result<impl fmt::Debug>) -> (ErrorCode, String) {
        let e = r.expect_err("expected an error");
        (e.code, e.message())
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    // ------------------------------------------------------------ push / pull

    #[test]
    fn push_then_pull_round_trips_with_the_same_digest() {
        let reg = MockRegistry::start(MockOptions::default());
        let store1 = new_store();
        let id = build_blog(&store1, "blog:1");
        let remote = format!("{}/team/blog:1", reg.host);
        store1.tag(&id, &parse_local_ref(&remote).unwrap()).unwrap();
        let p = push_with(&store1, &remote, clean_env()).unwrap();
        assert_eq!(p.lines[0], format!("The push refers to repository [{}/team/blog]", reg.host));
        let last = p.lines.last().unwrap();
        assert!(last.starts_with(&format!("1: digest: {id} size: ")), "{last}");
        assert_eq!(p.uploaded, 2);
        assert!(reg.has_manifest(&format!("team/blog@{id}")));
        // a second push uploads nothing new: only the manifest is PUT again
        let puts = || reg.requests().iter().filter(|r| r.method == "PUT").count();
        let before = puts();
        let p2 = push_with(&store1, &remote, clean_env()).unwrap();
        assert!(p2.lines.iter().any(|l| l.ends_with(": layer already exists")));
        assert_eq!(p2.existing, 2);
        assert_eq!(puts() - before, 1);

        // a fresh store pulls it back: same ID, tagged as written, files intact
        let store2 = new_store();
        let events = std::cell::RefCell::new(Vec::new());
        let on = |m: &str| events.borrow_mut().push(m.to_string());
        let pl = pull(&store2, &remote, &PullOptions { env: Some(clean_env()), on_event: Some(&on), ..Default::default() }).unwrap();
        assert_eq!(*events.borrow(), pl.lines, "on_event sees exactly the report lines");
        assert_eq!(pl.lines[pl.lines.len() - 2], format!("Digest: {id}"));
        assert_eq!(pl.lines.last().unwrap(), &format!("Pulled {remote} (image {})", short_id(&id)));
        assert_eq!(store2.resolve(&remote).unwrap(), id);
        assert!(store2.load_tree(&id).unwrap().get("/index.html").is_some());

        // delete locally, pull again by digest
        store2.remove_image(&id, true).unwrap();
        let by_digest = format!("{}/team/blog@{id}", reg.host);
        let pd = pull_with(&store2, &by_digest, clean_env()).unwrap();
        assert_eq!(pd.lines.last().unwrap(), &format!("Pulled {by_digest} (image {})", short_id(&id)));
        assert!(store2.has_image(&id).unwrap());
    }

    #[test]
    fn uncompressed_images_push_and_pull() {
        use crate::language::ast::OciCompression;
        let reg = MockRegistry::start(MockOptions::default());
        let store1 = new_store();
        let id = store1.add_image(&blog_tree(), &OciOptions { compression: OciCompression::None, ..Default::default() }).unwrap().id;
        let remote = format!("{}/team/plain:1", reg.host);
        store1.tag(&id, &parse_local_ref(&remote).unwrap()).unwrap();
        push_with(&store1, &remote, clean_env()).unwrap();
        let store2 = new_store();
        pull_with(&store2, &remote, clean_env()).unwrap();
        assert_eq!(store2.resolve(&remote).unwrap(), id);
        let d = store2.details(&id).unwrap();
        assert_eq!(d.manifest.layers[0].media_type, media::LAYER_TAR);
        assert!(store2.load_tree(&id).unwrap().get("/index.html").is_some());
    }

    #[test]
    fn push_needs_a_registry_host_and_an_existing_local_image() {
        let store = new_store();
        build_blog(&store, "blog:1");
        let (c, m) = code(push_with(&store, "blog:1", clean_env()));
        assert_eq!(c, E_USAGE);
        assert!(m.contains("no registry host"), "{m}");
        let (c, m) = code(push_with(&store, "localhost:1/none:1", clean_env()));
        assert_eq!(c, E_NOT_FOUND, "{m}");
    }

    #[test]
    fn pull_verifies_blob_digests_and_keeps_nothing_on_corruption() {
        let reg = MockRegistry::start(MockOptions::default());
        let seeded = reg.seed_image("lib/app", "1", SeedImage { layers: vec![vec![("/a.txt", "aaa")], vec![("/b.txt", "bbb")]], ..Default::default() });
        let bad = seeded.layer_digests[1].clone();
        reg.set_options(|o| o.corrupt = Some(bad.clone()));
        let store = new_store();
        let (c, _) = code(pull_with(&store, &format!("{}/lib/app:1", reg.host), clean_env()));
        assert_eq!(c, E_INTEGRITY);
        assert!(store.list().unwrap().is_empty());
        let names: Vec<String> =
            std::fs::read_dir(store.home.join("blobs/sha256")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert!(!names.contains(&bad[7..].to_string()), "the corrupt blob is not stored");
        assert!(names.iter().all(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())), "no temp files left: {names:?}");
    }

    #[test]
    fn a_manifest_fetched_by_digest_must_hash_to_that_digest() {
        let reg = MockRegistry::start(MockOptions::default());
        let a = reg.seed_image("lib/app", "1", SeedImage { layers: vec![vec![("/a", "a")]], ..Default::default() });
        let b = reg.seed_image("lib/other", "1", SeedImage { layers: vec![vec![("/b", "b")]], ..Default::default() });
        reg.set_options(|o| o.swap_manifest = Some(b.manifest_bytes.clone()));
        let (c, _) = code(pull_with(&new_store(), &format!("{}/lib/app@{}", reg.host, a.manifest_digest), clean_env()));
        assert_eq!(c, E_INTEGRITY);
    }

    #[test]
    fn size_limits_max_download_bytes() {
        let reg = MockRegistry::start(MockOptions::default());
        let big = "x".repeat(5000);
        reg.seed_image("lib/app", "1", SeedImage { layers: vec![vec![("/big", big.as_str())]], ..Default::default() });
        let o = PullOptions {
            env: Some(clean_env()),
            registry: RegistryOptions { limits: RegistryLimits { max_blob_bytes: 100, ..Default::default() }, ..Default::default() },
            ..Default::default()
        };
        let (c, m) = code(pull(&new_store(), &format!("{}/lib/app:1", reg.host), &o));
        assert_eq!(c, E_TOO_LARGE, "{m}");
    }

    #[test]
    fn pull_picks_the_platform_variant_from_an_index() {
        let reg = MockRegistry::start(MockOptions::default());
        let amd = reg.seed_image("lib/multi", "amd", SeedImage { layers: vec![vec![("/f", "amd")]], docker: true, ..Default::default() });
        let arm = reg.seed_image("lib/multi", "arm", SeedImage { layers: vec![vec![("/f", "arm")]], arch: "arm64", docker: true, ..Default::default() });
        let index = canonical_json(&json!({
            "schemaVersion": 2,
            "mediaType": media::DOCKER_LIST,
            "manifests": [
                { "mediaType": media::DOCKER_MANIFEST, "digest": amd.manifest_digest, "size": amd.manifest_bytes.len(), "platform": { "os": "linux", "architecture": "amd64" } },
                { "mediaType": media::DOCKER_MANIFEST, "digest": arm.manifest_digest, "size": arm.manifest_bytes.len(), "platform": { "os": "linux", "architecture": "arm64", "variant": "v8" } },
                { "mediaType": media::MANIFEST, "digest": arm.manifest_digest, "size": 1, "platform": { "os": "unknown", "architecture": "unknown" } },
            ],
        }))
        .into_bytes();
        reg.set_manifest("lib/multi:latest", &index);
        reg.set_manifest(&format!("lib/multi@{}", sha256(&index)), &index);
        let remote = format!("{}/lib/multi", reg.host);

        let s1 = new_store();
        let r1 = pull_with(&s1, &remote, clean_env()).unwrap();
        assert!(r1.lines.contains(&format!("Digest: {}", sha256(&index))));
        assert_eq!(s1.resolve(&remote).unwrap(), amd.manifest_digest, "default platform is linux/amd64");

        let s2 = new_store();
        pull(&s2, &remote, &PullOptions { env: Some(clean_env()), platform: Some("linux/arm64".into()), ..Default::default() }).unwrap();
        let d = s2.details(&arm.manifest_digest).unwrap();
        assert_eq!(d.config.architecture, "arm64");
        assert_eq!(d.manifest.layers[0].media_type, media::DOCKER_LAYER);

        let o = PullOptions { env: Some(clean_env()), platform: Some("linux/s390x".into()), ..Default::default() };
        let (c, m) = code(pull(&new_store(), &remote, &o));
        assert_eq!(c, E_REGISTRY);
        assert!(m.contains("no linux/s390x variant (available: linux/amd64, linux/arm64/v8)"), "{m}");
        assert_eq!(select_platform(br#"{"manifests":[]}"#, "linux/amd64").unwrap_err().code, E_REGISTRY);
    }

    #[test]
    fn registry_errors_name_the_server_problem() {
        let reg = MockRegistry::start(MockOptions::default());
        let (c, m) = code(pull_with(&new_store(), &format!("{}/lib/missing:1", reg.host), clean_env()));
        assert_eq!(c, E_NOT_FOUND);
        assert!(m.contains("404") && m.contains("MANIFEST_UNKNOWN"), "{m}");
    }

    // ------------------------------------------------------------ auth

    #[test]
    fn bearer_auth_anonymous_pull_credentials_for_push_no_secrets() {
        let reg = MockRegistry::start(MockOptions { auth: Auth::Bearer, user: "alice".into(), pass: SECRET.into(), ..Default::default() });
        reg.seed_image("lib/pub", "1", SeedImage { layers: vec![vec![("/f", "f")]], ..Default::default() });
        let store = new_store();
        let anon = pull_with(&store, &format!("{}/lib/pub:1", reg.host), clean_env()).unwrap();
        let scopes: Vec<String> = reg.token_calls().iter().filter_map(|c| c.split("scope=").nth(1).map(|s| s.split('&').next().unwrap().to_string())).collect();
        assert_eq!(scopes, vec!["repository%3Alib%2Fpub%3Apull"]);
        assert!(reg.requests().iter().any(|r| r.authorization.as_deref() == Some("Bearer tok-anon")));

        let id = build_blog(&store, "blog:1");
        let remote = format!("{}/alice/blog:1", reg.host);
        store.tag(&id, &parse_local_ref(&remote).unwrap()).unwrap();
        let denied = push_with(&store, &remote, clean_env()).unwrap_err();
        assert_eq!(denied.code, E_AUTH);
        assert!(denied.message().contains("no credentials"), "{}", denied.message());

        // credentials from the Docker config `auths`
        let cfg = docker_config(json!({ "auths": { reg.host.clone(): { "auth": b64(&format!("alice:{SECRET}")) } } }));
        let ok = push_with(&store, &remote, env_with(&[("DOCKER_CONFIG", &cfg)])).unwrap();
        assert!(reg.has_manifest(&format!("alice/blog@{id}")));
        let mut outputs: Vec<String> = ok.lines.clone();
        outputs.extend(anon.lines.clone());
        outputs.push(denied.message());
        outputs.push(format!("{denied:?}"));
        for out in &outputs {
            assert!(!out.contains("tok-") && !out.contains(SECRET), "no token or password in output: {out}");
        }
        // Basic credentials only go to the token endpoint, never to /v2/ with a bearer challenge
        assert!(reg.requests().iter().filter(|r| r.url.starts_with("/v2/")).all(|r| !r.authorization.as_deref().unwrap_or("").starts_with("Basic ")));

        // wrong password: rejected, secret not echoed
        let bad = docker_config(json!({ "auths": { reg.host.clone(): { "auth": b64(&format!("alice:wrong-{SECRET}")) } } }));
        let no = push_with(&store, &remote, env_with(&[("DOCKER_CONFIG", &bad)])).unwrap_err();
        assert_eq!(no.code, E_AUTH);
        assert!(!no.message().contains(SECRET) && !format!("{no:?}").contains(SECRET));
    }

    #[test]
    fn basic_auth_via_env_and_via_the_client_api() {
        let reg = MockRegistry::start(MockOptions { auth: Auth::Basic, user: "bob".into(), pass: SECRET.into(), ..Default::default() });
        reg.seed_image("lib/priv", "1", SeedImage { layers: vec![vec![("/f", "f")]], ..Default::default() });
        let store = new_store();
        let r = format!("{}/lib/priv:1", reg.host);
        assert_eq!(pull_with(&store, &r, clean_env()).unwrap_err().code, E_AUTH);
        let auth = json!({ reg.host.clone(): format!("bob:{SECRET}") }).to_string();
        let ok = pull_with(&store, &r, env_with(&[("SITEFILE_REGISTRY_AUTH", &auth)])).unwrap();
        assert!(ok.lines.iter().all(|l| !l.contains(SECRET)));

        let s2 = new_store();
        let provider: CredentialProvider = Arc::new(|| Ok(Some(Credentials::basic("bob", SECRET))));
        let c = RegistryClient::new(&reg.host, RegistryOptions { credentials: Some(provider), ..Default::default() });
        let res = pull_image(&s2, &c, &parse_ref(&r).unwrap(), &PullImageOptions { tag: true, ..Default::default() }).unwrap();
        assert!(res.id.starts_with("sha256:"));
        let wrong: CredentialProvider = Arc::new(|| Ok(Some(Credentials::basic("bob", "nope"))));
        let c = RegistryClient::new(&reg.host, RegistryOptions { credentials: Some(wrong), ..Default::default() });
        let e = pull_image(&s2, &c, &parse_ref(&format!("{}/lib/priv:2", reg.host)).unwrap(), &PullImageOptions::default()).unwrap_err();
        assert_eq!(e.code, E_AUTH);
        assert!(!e.message().contains("nope"));
    }

    fn fake_helper(bin: &Path, log: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            "#!/bin/sh\nread server\necho \"args=$@ server=$server\" > \"{}\"\necho '{{\"ServerURL\":\"x\",\"Username\":\"carol\",\"Secret\":\"{SECRET}\"}}'\n",
            log.display()
        );
        let p = bin.join("docker-credential-fake");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn credential_helpers_only_the_named_one_url_on_stdin_nothing_on_argv() {
        let reg = MockRegistry::start(MockOptions { auth: Auth::Bearer, user: "carol".into(), pass: SECRET.into(), ..Default::default() });
        let bin = tmp("helper-bin-");
        let log = bin.join("log.txt");
        fake_helper(&bin, &log);
        let store = new_store();
        let id = build_blog(&store, "blog:1");
        let remote = format!("{}/carol/blog:1", reg.host);
        store.tag(&id, &parse_local_ref(&remote).unwrap()).unwrap();
        let path = format!("{}:/usr/bin:/bin", bin.display());

        // credHelpers[host]
        let cfg = docker_config(json!({ "credHelpers": { reg.host.clone(): "fake" } }));
        let r = push_with(&store, &remote, env_with(&[("PATH", &path), ("DOCKER_CONFIG", &cfg)])).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), format!("args=get server={}", reg.host));
        assert!(r.lines.iter().all(|l| !l.contains(SECRET)));

        // credsStore, with an empty auths entry as `docker login` writes it
        std::fs::remove_file(&log).unwrap();
        let cfg2 = docker_config(json!({ "auths": { reg.host.clone(): {} }, "credsStore": "fake" }));
        push_with(&store, &remote, env_with(&[("PATH", &path), ("DOCKER_CONFIG", &cfg2)])).unwrap();
        assert!(log.exists());

        // a config that names no helper never runs one, even if one is on PATH
        std::fs::remove_file(&log).unwrap();
        let cfg3 = docker_config(json!({ "auths": {} }));
        assert!(push_with(&store, &remote, env_with(&[("PATH", &path), ("DOCKER_CONFIG", &cfg3)])).is_err());
        assert!(!log.exists());
    }

    #[test]
    fn credential_helper_names_are_restricted_and_a_missing_helper_is_clear() {
        let env = Env::new();
        assert_eq!(run_credential_helper("a b; rm -rf /", "x", &env, 1000).unwrap_err().code, E_AUTH);
        assert_eq!(run_credential_helper("../evil", "x", &env, 1000).unwrap_err().code, E_AUTH);
        let cfg = docker_config(json!({ "credHelpers": { "registry.example.com": "no-such-helper-xyz" } }));
        let env = env_with(&[("DOCKER_CONFIG", &cfg), ("PATH", "/nonexistent")]);
        let e = resolve_credentials("registry.example.com", None, &env).unwrap_err();
        assert_eq!(e.code, E_AUTH);
        assert!(e.message().contains("not found on PATH"), "{}", e.message());
    }

    #[test]
    fn docker_config_auth_forms_and_docker_hub_aliases() {
        let cfg = docker_config(json!({
            "auths": {
                "https://index.docker.io/v1/": { "auth": b64("hubuser:hubpass") },
                "ghcr.io": { "username": "g", "password": "gp" },
                "https://reg.example.com:5000/v2/": { "auth": b64("u:p:with:colons") },
                "tok.example.com": { "identitytoken": "refresh-me" },
            }
        }));
        let env = env_with(&[("DOCKER_CONFIG", &cfg)]);
        let get = |r: &str| resolve_credentials(r, None, &env).unwrap();
        assert_eq!(get("docker.io"), Some(Credentials::basic("hubuser", "hubpass")));
        assert_eq!(get("ghcr.io"), Some(Credentials::basic("g", "gp")));
        assert_eq!(get("reg.example.com:5000"), Some(Credentials::basic("u", "p:with:colons")));
        assert_eq!(get("tok.example.com"), Some(Credentials { identity_token: Some("refresh-me".into()), ..Default::default() }));
        assert_eq!(get("other.example.com"), None);
        // explicit beats everything; $SITEFILE_REGISTRY_AUTH beats the config
        let x = Credentials::basic("x", "y");
        assert_eq!(resolve_credentials("ghcr.io", Some(&x), &env).unwrap(), Some(x.clone()));
        let e2 = env_with(&[("DOCKER_CONFIG", &cfg), ("SITEFILE_REGISTRY_AUTH", r#"{"ghcr.io":"e:f"}"#)]);
        assert_eq!(resolve_credentials("ghcr.io", None, &e2).unwrap(), Some(Credentials::basic("e", "f")));
        let e3 = env_with(&[("DOCKER_CONFIG", &cfg), ("SITEFILE_REGISTRY_AUTH", "nope")]);
        assert_eq!(resolve_credentials("ghcr.io", None, &e3).unwrap_err().code, E_AUTH);
        // secrets never in Debug output
        let dbg = format!("{:?}", Credentials::basic("user", SECRET));
        assert!(!dbg.contains(SECRET) && dbg.contains("user"));
    }

    #[test]
    fn insecure_registries_merge_flags_and_env() {
        let env = env_with(&[("SITEFILE_INSECURE_REGISTRIES", " a:5000, b ,,a:5000")]);
        assert_eq!(insecure_registries_from_env(&["c".into(), "a:5000".into()], &env), vec!["c", "a:5000", "b"]);
    }

    // ------------------------------------------------------------ transport policy

    #[test]
    fn blob_redirects_to_a_cdn_are_followed_without_authorization() {
        let reg = MockRegistry::start(MockOptions { auth: Auth::Basic, ..Default::default() });
        let cdn = start_cdn(&reg);
        reg.seed_image("lib/app", "1", SeedImage { layers: vec![vec![("/f", "f")]], ..Default::default() });
        reg.set_options(|o| o.redirect_blobs_to = Some(cdn.url.clone()));
        let auth = json!({ reg.host.clone(): "u:p" }).to_string();
        pull_with(&new_store(), &format!("{}/lib/app:1", reg.host), env_with(&[("SITEFILE_REGISTRY_AUTH", &auth)])).unwrap();
        let reqs = cdn.requests.lock().unwrap().clone();
        assert!(!reqs.is_empty());
        assert!(reqs.iter().all(|q| q.authorization.is_none()), "credentials are not forwarded");
    }

    #[test]
    fn redirects_are_revalidated() {
        let reg = MockRegistry::start(MockOptions::default());
        reg.seed_image("lib/app", "1", SeedImage { layers: vec![vec![("/f", "f")]], ..Default::default() });
        let r = parse_ref(&format!("{}/lib/app:1", reg.host)).unwrap();
        for (target, want) in [("http://203.0.113.9", E_PROTOCOL), ("https://10.0.0.5", E_SSRF_BLOCKED), ("https://user:pw@example.com", E_PROTOCOL)] {
            reg.set_options(|o| o.redirect_blobs_to = Some(target.into()));
            let e = pull_image(&new_store(), &client(&reg.host), &r, &PullImageOptions::default()).unwrap_err();
            assert_eq!(e.code, want, "{target}: {}", e.message());
            assert!(!e.message().contains("pw@"));
        }
    }

    #[test]
    fn a_non_loopback_registry_is_reached_over_https_only() {
        // 192.0.2.1 (TEST-NET-1) never answers; no plain-http attempt is made.
        let c = RegistryClient::new("192.0.2.1:5000", RegistryOptions { limits: RegistryLimits { timeout_ms: 300, ..Default::default() }, ..Default::default() });
        let e = c.ping().unwrap_err();
        assert!(e.message().contains("https://192.0.2.1:5000"), "{}", e.message());
    }

    #[test]
    fn insecure_registries_fall_back_to_http_and_others_do_not() {
        // loopback registries (and --insecure-registry hosts) probe https first, then use http
        let reg = MockRegistry::start(MockOptions::default());
        let c = client(&reg.host);
        c.ping().unwrap();
        assert_eq!(c.init().unwrap().scheme, "http");
        assert!(!client("192.0.2.1:5000").insecure);
        assert!(RegistryClient::new("reg.example:5000", RegistryOptions { insecure_registries: vec!["REG.example:5000".into()], ..Default::default() }).insecure);
    }

    #[test]
    fn challenge_parsing() {
        let c = parse_challenge(Some(r#"Bearer realm="https://auth.example/token",service="reg",scope="repository:a/b:pull""#)).unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.params["realm"], "https://auth.example/token");
        assert_eq!(c.params["scope"], "repository:a/b:pull");
        assert!(parse_challenge(Some("   ")).is_none());
        assert!(parse_challenge(None).is_none());
    }

    #[test]
    fn push_image_rejects_a_digest_that_does_not_match() {
        let reg = MockRegistry::start(MockOptions::default());
        let store = new_store();
        let id = build_blog(&store, "blog:1");
        let r = parse_ref(&format!("{}/x/y@sha256:{}", reg.host, "0".repeat(64))).unwrap();
        assert_eq!(push_image(&store, &client(&reg.host), &id, &r, None).unwrap_err().code, E_USAGE);
    }
}
