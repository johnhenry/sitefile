//! `sitefile import-config` (port of `src/host-config.ts`, issue #204):
//! translate a host's config file into `_redirects` and `_headers` that
//! `sitefile serve` understands today. Every rule written is one `serve`
//! applies as written; anything else is listed as unsupported with the reason.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::server::{parse_header_rules, parse_redirect_rules};
use crate::toml::parse_toml;
use serde_json::{Map, Value};

pub const HOST_FORMATS: [&str; 6] = [
    "netlify", "vercel", "firebase", "azure", "wrangler", "htaccess",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderBlock {
    pub pattern: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// Where in the file: `redirects[2]`, `hosting.cleanUrls`, `line 7`.
    pub entry: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedConfig {
    pub format: String,
    /// `_redirects` lines (`from to status[!]`).
    pub redirects: Vec<String>,
    pub headers: Vec<HeaderBlock>,
    pub unsupported: Vec<Unsupported>,
}

/// The format a file name implies, if any.
pub fn format_of(file: &str) -> Option<&'static str> {
    let base = file.rsplit('/').next().unwrap_or(file).to_lowercase();
    match base.as_str() {
        "netlify.toml" => Some("netlify"),
        "vercel.json" => Some("vercel"),
        "firebase.json" => Some("firebase"),
        "staticwebapp.config.json" => Some("azure"),
        "wrangler.toml" | "wrangler.json" => Some("wrangler"),
        ".htaccess" => Some("htaccess"),
        _ => None,
    }
}

const PLACEHOLDERS: &str = ":placeholders are not supported yet (johnhenry/sitefile#193)";
const MIDDLE_STAR: &str = "* is supported only as a trailing /*";
const PROXY: &str = "proxying to another host is not supported (johnhenry/sitefile#172)";
const CONDITIONS: &str = "conditions are not supported";
const CLEAN_URLS: &str =
    "serve always answers /about with about.html; canonical redirects are johnhenry/sitefile#192";
const TRAILING: &str = "trailing-slash modes are johnhenry/sitefile#192";

fn as_int(v: &Value) -> Option<i64> {
    let f = v.as_f64()?;
    (f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0).then_some(f as i64)
}

fn is_http(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http:") || l.starts_with("https:")
}

/// JavaScript's own-key order: integer-like keys ascending first, then the rest as written.
fn keys_of(o: &Map<String, Value>) -> Vec<&String> {
    let mut keys: Vec<&String> = o.keys().collect();
    keys.sort_by_key(|k| crate::values::array_index(k).map_or((1, 0), |i| (0, i)));
    keys
}

#[derive(Default)]
struct Out {
    redirects: Vec<String>,
    fallbacks: Vec<String>,
    headers: Vec<HeaderBlock>,
    unsupported: Vec<Unsupported>,
}

impl Out {
    fn skip(&mut self, entry: &str, reason: impl Into<String>) {
        self.unsupported.push(Unsupported {
            entry: entry.to_string(),
            reason: reason.into(),
        });
    }

    fn redirect(
        &mut self,
        entry: &str,
        from: &str,
        to: &str,
        status: i64,
        force: bool,
        fallback: bool,
    ) -> bool {
        let line = format!("{from} {to} {status}{}", if force { "!" } else { "" });
        if parse_redirect_rules(&line).len() == 1 && !has_placeholder(to) {
            if fallback {
                self.fallbacks.push(line);
            } else {
                self.redirects.push(line);
            }
            return true;
        }
        self.skip(entry, redirect_reason(from, to, status));
        false
    }

    fn header(&mut self, entry: &str, pattern: &str, headers: Vec<(String, String)>) {
        if headers.is_empty() {
            return;
        }
        let mut text = format!("{pattern}\n");
        for (i, (n, v)) in headers.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&format!("  {n}: {v}"));
        }
        text.push('\n');
        let parsed = parse_header_rules(&text);
        if parsed.len() != 1
            || parsed[0].pattern != pattern
            || parsed[0].headers.len() != headers.len()
            || pattern.chars().any(is_ascii_space)
        {
            self.skip(entry, header_reason(pattern, &headers));
            return;
        }
        match self.headers.last_mut() {
            Some(last) if last.pattern == pattern => last.headers.extend(headers),
            _ => self.headers.push(HeaderBlock {
                pattern: pattern.to_string(),
                headers,
            }),
        }
    }
}

fn is_ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}' | '\u{b}')
}

/// `/(^|\/):(?!splat(\/|$|[?#]))[A-Za-z_]/` on the target without its `https://host`.
fn has_placeholder(to: &str) -> bool {
    let mut path = to;
    let lower = to.to_ascii_lowercase();
    for scheme in ["http://", "https://"] {
        if lower.starts_with(scheme) {
            let rest = &to[scheme.len()..];
            let end = rest.find('/').unwrap_or(rest.len());
            path = &rest[end..];
            break;
        }
    }
    let b = path.as_bytes();
    for i in 0..b.len() {
        if b[i] != b':' || !(i == 0 || b[i - 1] == b'/') {
            continue;
        }
        let after = &path[i + 1..];
        if after.starts_with("splat") {
            let n = after.as_bytes().get(5);
            if n.is_none() || matches!(n, Some(b'/') | Some(b'?') | Some(b'#')) {
                continue;
            }
        }
        if after
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            return true;
        }
    }
    false
}

fn redirect_reason(from: &str, to: &str, status: i64) -> String {
    if status == 410 {
        return "status 410 is not supported yet (johnhenry/sitefile#193)".into();
    }
    if ![200, 301, 302, 303, 307, 308, 404].contains(&status) {
        return format!("status {status} is not supported");
    }
    if from.split('/').any(|s| s.starts_with(':')) || has_placeholder(to) {
        return PLACEHOLDERS.into();
    }
    let head = from.strip_suffix("/*").unwrap_or(from);
    if head.contains('*') {
        return MIDDLE_STAR.into();
    }
    if !from.starts_with('/') {
        return format!("'{from}' is not a path");
    }
    if (status == 200 || status == 404) && is_http(to) {
        return PROXY.into();
    }
    format!("sitefile serve cannot express '{from} {to} {status}'")
}

fn is_token(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn header_reason(pattern: &str, headers: &[(String, String)]) -> String {
    if !pattern.starts_with('/') || pattern.chars().any(is_ascii_space) {
        return format!("'{pattern}' is not a path pattern");
    }
    for (n, v) in headers {
        if !is_token(n) {
            return format!("'{n}' is not a header name");
        }
        if [
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "etag",
            "location",
        ]
        .contains(&n.to_ascii_lowercase().as_str())
        {
            return format!("{n} cannot be set by _headers");
        }
        if !v.chars().all(|c| c == '\t' || (' '..='~').contains(&c)) {
            return format!("the value of {n} has characters _headers cannot carry");
        }
    }
    "sitefile serve cannot express these headers".into()
}

fn str_of<'v>(m: &'v Map<String, Value>, k: &str) -> Option<&'v str> {
    m.get(k).and_then(Value::as_str)
}

fn header_pairs(list: &[Value]) -> Option<Vec<(String, String)>> {
    let mut hs = Vec::new();
    for h in list {
        let m = h.as_object()?;
        hs.push((
            str_of(m, "key")?.to_string(),
            str_of(m, "value")?.to_string(),
        ));
    }
    Some(hs)
}

/// A mapping of names to strings, in JavaScript key order.
fn string_map(v: &Value) -> Option<Vec<(String, String)>> {
    let m = v.as_object()?;
    let mut out = Vec::new();
    for k in keys_of(m) {
        out.push((k.clone(), m[k].as_str()?.to_string()));
    }
    Some(out)
}

// ------------------------------------------------------------------ netlify.toml

fn netlify(doc: &Map<String, Value>, out: &mut Out) {
    if let Some(redirects) = doc.get("redirects") {
        match redirects.as_array() {
            None => out.skip("redirects", "expected [[redirects]] tables"),
            Some(list) => {
                for (i, r) in list.iter().enumerate() {
                    let entry = format!("redirects[{i}]");
                    let Some(m) = r.as_object() else {
                        out.skip(&entry, "expected from and to");
                        continue;
                    };
                    let (Some(from), Some(to)) = (str_of(m, "from"), str_of(m, "to")) else {
                        out.skip(&entry, "expected from and to");
                        continue;
                    };
                    if let Some(k) = ["query", "conditions", "headers", "signed"]
                        .iter()
                        .find(|k| m.contains_key(**k))
                    {
                        out.skip(
                            &entry,
                            if *k == "query" {
                                "query matching is not supported"
                            } else {
                                CONDITIONS
                            },
                        );
                        continue;
                    }
                    let status = match m.get("status") {
                        None => Some(301),
                        Some(v) => as_int(v),
                    };
                    let Some(status) = status else {
                        out.skip(&entry, "status must be a number");
                        continue;
                    };
                    let force = match m.get("force") {
                        None => false,
                        Some(Value::Bool(b)) => *b,
                        Some(_) => {
                            out.skip(&entry, "force must be true or false");
                            continue;
                        }
                    };
                    out.redirect(&entry, from, to, status, force, false);
                }
            }
        }
    }
    if let Some(headers) = doc.get("headers") {
        match headers.as_array() {
            None => out.skip("headers", "expected [[headers]] tables"),
            Some(list) => {
                'blocks: for (i, h) in list.iter().enumerate() {
                    let entry = format!("headers[{i}]");
                    let m = h.as_object();
                    let (Some(pattern), Some(values)) = (
                        m.and_then(|m| str_of(m, "for")),
                        m.and_then(|m| m.get("values")).and_then(Value::as_object),
                    ) else {
                        out.skip(&entry, "expected for and values");
                        continue;
                    };
                    let mut hs = Vec::new();
                    for k in keys_of(values) {
                        match &values[k] {
                            Value::String(s) => hs.push((k.clone(), s.clone())),
                            Value::Array(a) if a.iter().all(Value::is_string) => hs.push((
                                k.clone(),
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            )),
                            _ => {
                                out.skip(&entry, format!("the value of {k} must be a string"));
                                continue 'blocks;
                            }
                        }
                    }
                    out.header(&entry, pattern, hs);
                }
            }
        }
    }
    for k in ["edge_functions", "functions", "plugins"] {
        if doc.contains_key(k) {
            out.skip(k, "not part of serving files");
        }
    }
}

// ------------------------------------------------------------------ Vercel sources

fn has_any(s: &str, set: &str) -> bool {
    s.chars().any(|c| set.contains(c))
}

const VERCEL_SPECIAL: &str = ":*()?+[]{}\\";

/// A Vercel `source` as a `_redirects` path (see the TypeScript `vercelSource`).
fn vercel_source(source: &str) -> Option<(String, Vec<String>)> {
    if source.starts_with('/') && !has_any(&source[1..], VERCEL_SPECIAL) {
        return Some((source.to_string(), Vec::new()));
    }
    let slash = source.rfind('/')?;
    let (prefix, last) = (&source[..=slash], &source[slash + 1..]);
    if !prefix.starts_with('/') {
        return None;
    }
    if prefix.len() > 1
        && prefix[1..prefix.len() - 1]
            .split('/')
            .any(|seg| seg.is_empty() || has_any(seg, VERCEL_SPECIAL))
    {
        return None;
    }
    if last == "(.*)" {
        return Some((format!("{prefix}*"), vec!["$1".into()]));
    }
    let name = last.strip_prefix(':')?.strip_suffix('*')?;
    let mut cs = name.chars();
    let ok = cs
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| {
        (
            format!("{prefix}*"),
            vec![format!(":{name}*"), format!(":{name}")],
        )
    })
}

fn vercel_target(dest: &str, params: &[String]) -> String {
    let mut out = dest.to_string();
    for p in params {
        out = out.split(p.as_str()).collect::<Vec<_>>().join(":splat");
    }
    out
}

fn vercel_source_reason(source: &str) -> &'static str {
    if source.contains('(') || source.contains(')') {
        "regular expressions are not supported"
    } else if source.contains(':') {
        PLACEHOLDERS
    } else {
        MIDDLE_STAR
    }
}

fn each(
    doc: &Map<String, Value>,
    key: &str,
    at: &str,
    out: &mut Out,
    conditions: bool,
    mut f: impl FnMut(&Map<String, Value>, &str, &mut Out),
) {
    let Some(v) = doc.get(key) else { return };
    let name = if at.is_empty() {
        key.to_string()
    } else {
        format!("{at}.{key}")
    };
    let Some(list) = v.as_array() else {
        out.skip(&name, "expected a list");
        return;
    };
    for (i, r) in list.iter().enumerate() {
        let entry = format!("{name}[{i}]");
        let Some(m) = r.as_object() else {
            out.skip(&entry, "expected an object");
            continue;
        };
        if conditions && (m.contains_key("has") || m.contains_key("missing")) {
            out.skip(&entry, CONDITIONS);
            continue;
        }
        f(m, &entry, out);
    }
}

fn vercel(doc: &Map<String, Value>, out: &mut Out) {
    each(doc, "redirects", "", out, true, |r, entry, out| {
        let (Some(source), Some(dest)) = (str_of(r, "source"), str_of(r, "destination")) else {
            return out.skip(entry, "expected source and destination");
        };
        let Some((from, params)) = vercel_source(source) else {
            return out.skip(entry, vercel_source_reason(source));
        };
        let status = match r.get("statusCode") {
            Some(v) => as_int(v),
            None => Some(if r.get("permanent") == Some(&Value::Bool(false)) {
                307
            } else {
                308
            }),
        };
        let Some(status) = status else {
            return out.skip(entry, "statusCode must be a number");
        };
        out.redirect(
            entry,
            &from,
            &vercel_target(dest, &params),
            status,
            false,
            false,
        );
    });
    each(doc, "rewrites", "", out, true, |r, entry, out| {
        let (Some(source), Some(dest)) = (str_of(r, "source"), str_of(r, "destination")) else {
            return out.skip(entry, "expected source and destination");
        };
        if is_http(dest) {
            return out.skip(entry, PROXY);
        }
        let Some((from, params)) = vercel_source(source) else {
            return out.skip(entry, vercel_source_reason(source));
        };
        out.redirect(
            entry,
            &from,
            &vercel_target(dest, &params),
            200,
            false,
            false,
        );
    });
    each(doc, "headers", "", out, true, |r, entry, out| {
        let (Some(source), Some(list)) = (
            str_of(r, "source"),
            r.get("headers").and_then(Value::as_array),
        ) else {
            return out.skip(entry, "expected source and headers");
        };
        let Some((from, _)) = vercel_source(source) else {
            return out.skip(entry, vercel_source_reason(source));
        };
        let Some(hs) = header_pairs(list) else {
            return out.skip(entry, "expected headers of key and value");
        };
        out.header(entry, &from, hs);
    });
    if doc.contains_key("cleanUrls") {
        out.skip("cleanUrls", CLEAN_URLS);
    }
    if doc.contains_key("trailingSlash") {
        out.skip("trailingSlash", TRAILING);
    }
}

// ------------------------------------------------------------------ firebase.json

const FIREBASE_SPECIAL: &str = "*?{}[]!:";

fn firebase_source(glob: &str) -> Option<String> {
    if glob.starts_with('/') && !has_any(&glob[1..], FIREBASE_SPECIAL) {
        return Some(glob.to_string());
    }
    if glob == "**" {
        return Some("/*".into());
    }
    let prefix = glob.strip_suffix("/**")?;
    if prefix.is_empty() {
        return Some("/*".into());
    }
    let rest = prefix.strip_prefix('/')?;
    if rest
        .split('/')
        .any(|seg| seg.is_empty() || has_any(seg, FIREBASE_SPECIAL))
    {
        return None;
    }
    Some(format!("{prefix}/*"))
}

fn firebase_reason(r: &Map<String, Value>) -> &'static str {
    if r.contains_key("regex") {
        return "regular expressions are not supported";
    }
    let s = r.get("source").or_else(|| r.get("glob"));
    let s = match s {
        Some(Value::String(x)) => x.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    if s.contains(':') {
        PLACEHOLDERS
    } else {
        "only exact paths and a trailing /** are supported"
    }
}

fn firebase_from(r: &Map<String, Value>) -> Option<String> {
    if r.contains_key("regex") {
        return None;
    }
    let s = match r.get("source") {
        Some(v) => v,
        None => r.get("glob")?,
    };
    firebase_source(s.as_str()?)
}

fn firebase(doc: &Map<String, Value>, out: &mut Out) {
    let mut hosting = doc.get("hosting");
    let mut at = "hosting";
    if let Some(Value::Array(list)) = hosting {
        if list.len() != 1 {
            return out.skip("hosting", "several sites: import one site at a time");
        }
        hosting = list.first();
        at = "hosting[0]";
    }
    let h = match hosting {
        Some(Value::Object(m)) => m,
        None => return,
        Some(_) => return out.skip("hosting", "expected an object"),
    };
    each(h, "redirects", at, out, false, |r, entry, out| {
        let Some(from) = firebase_from(r) else {
            return out.skip(entry, firebase_reason(r));
        };
        let Some(dest) = str_of(r, "destination") else {
            return out.skip(entry, "expected a destination");
        };
        let status = match r.get("type") {
            None => Some(301),
            Some(v) => as_int(v),
        };
        let Some(status) = status else {
            return out.skip(entry, "type must be 301 or 302");
        };
        out.redirect(entry, &from, dest, status, false, false);
    });
    each(h, "rewrites", at, out, false, |r, entry, out| {
        if let Some(k) = ["function", "run", "dynamicLinks"]
            .iter()
            .find(|k| r.contains_key(**k))
        {
            return out.skip(entry, format!("{k} rewrites are not supported"));
        }
        let Some(from) = firebase_from(r) else {
            return out.skip(entry, firebase_reason(r));
        };
        let Some(dest) = str_of(r, "destination") else {
            return out.skip(entry, "expected a destination");
        };
        if is_http(dest) {
            return out.skip(entry, PROXY);
        }
        out.redirect(entry, &from, dest, 200, false, false);
    });
    each(h, "headers", at, out, false, |r, entry, out| {
        let Some(from) = firebase_from(r) else {
            return out.skip(entry, firebase_reason(r));
        };
        let Some(list) = r.get("headers").and_then(Value::as_array) else {
            return out.skip(entry, "expected headers");
        };
        let Some(hs) = header_pairs(list) else {
            return out.skip(entry, "expected headers of key and value");
        };
        out.header(entry, &from, hs);
    });
    if h.contains_key("cleanUrls") {
        out.skip(&format!("{at}.cleanUrls"), CLEAN_URLS);
    }
    if h.contains_key("trailingSlash") {
        out.skip(&format!("{at}.trailingSlash"), TRAILING);
    }
    if h.contains_key("i18n") {
        out.skip(
            &format!("{at}.i18n"),
            "language and country rewrites are not supported",
        );
    }
}

// ------------------------------------------------------------------ staticwebapp.config.json

fn azure_route(route: &str) -> Option<&str> {
    if route == "/*" {
        return Some(route);
    }
    let base = route
        .strip_suffix("/*")
        .filter(|b| b.starts_with('/') && !has_any(b, "*?{}[]:"))
        .map(|_| route);
    base.or_else(|| (route.starts_with('/') && !has_any(route, "*?{}[]:")).then_some(route))
}

fn azure(doc: &Map<String, Value>, out: &mut Out) {
    if let Some(routes) = doc.get("routes") {
        match routes.as_array() {
            None => out.skip("routes", "expected a list"),
            Some(list) => {
                for (i, r) in list.iter().enumerate() {
                    let entry = format!("routes[{i}]");
                    let Some((m, route)) = r
                        .as_object()
                        .and_then(|m| str_of(m, "route").map(|s| (m, s)))
                    else {
                        out.skip(&entry, "expected a route");
                        continue;
                    };
                    if m.contains_key("allowedRoles") || m.contains_key("methods") {
                        out.skip(&entry, "allowedRoles and methods are not supported");
                        continue;
                    }
                    let Some(from) = azure_route(route) else {
                        out.skip(&entry, "only exact routes and a trailing /* are supported");
                        continue;
                    };
                    if let Some(hv) = m.get("headers") {
                        match string_map(hv) {
                            None => out.skip(&entry, "headers must map names to strings"),
                            Some(hs) => out.header(&entry, from, hs),
                        }
                    }
                    if let Some(to) = str_of(m, "redirect") {
                        let status = match m.get("statusCode") {
                            None => Some(302),
                            Some(v) => as_int(v),
                        };
                        let Some(status) = status else {
                            out.skip(&entry, "statusCode must be a number");
                            continue;
                        };
                        out.redirect(&entry, from, to, status, false, false);
                    } else if let Some(to) = str_of(m, "rewrite") {
                        if m.get("statusCode").is_some_and(|v| as_int(v) != Some(200)) {
                            out.skip(&entry, "a rewrite with a statusCode is not supported");
                            continue;
                        }
                        out.redirect(&entry, from, to, 200, false, false);
                    } else if m.contains_key("statusCode") {
                        out.skip(
                            &entry,
                            "a statusCode without a redirect or rewrite is not supported",
                        );
                    }
                }
            }
        }
    }
    if let Some(nav) = doc.get("navigationFallback") {
        match nav
            .as_object()
            .and_then(|m| str_of(m, "rewrite").map(|s| (m, s)))
        {
            None => out.skip("navigationFallback", "expected a rewrite"),
            Some((m, to)) => {
                out.redirect("navigationFallback", "/*", to, 200, false, true);
                if m.contains_key("exclude") {
                    out.skip("navigationFallback.exclude", "not translated: the fallback answers only paths with no file, so excluded paths that are missing get it too");
                }
            }
        }
    }
    if let Some(o) = doc.get("responseOverrides") {
        match o.as_object() {
            None => out.skip("responseOverrides", "expected an object"),
            Some(m) => {
                for code in keys_of(m) {
                    let entry = format!("responseOverrides.{code}");
                    if code != "404" {
                        out.skip(&entry, "only 404 can be overridden");
                        continue;
                    }
                    match m[code].as_object() {
                        Some(x)
                            if !x.contains_key("redirect") && str_of(x, "rewrite").is_some() =>
                        {
                            let to = str_of(x, "rewrite").unwrap_or_default();
                            out.redirect(&entry, "/*", to, 404, false, true);
                        }
                        _ => out.skip(&entry, "only a rewrite is supported"),
                    }
                }
            }
        }
    }
    if let Some(g) = doc.get("globalHeaders") {
        match string_map(g) {
            None => out.skip("globalHeaders", "globalHeaders must map names to strings"),
            Some(hs) => out.header("globalHeaders", "/*", hs),
        }
    }
    if doc.contains_key("mimeTypes") {
        out.skip(
            "mimeTypes",
            "custom content types are johnhenry/sitefile#220",
        );
    }
}

// ------------------------------------------------------------------ wrangler [assets]

fn wrangler(doc: &Map<String, Value>, out: &mut Out) {
    let Some(a) = doc.get("assets") else { return };
    let Some(a) = a.as_object() else {
        return out.skip("assets", "expected a table");
    };
    match a.get("not_found_handling") {
        Some(Value::String(s)) if s == "single-page-application" => {
            out.redirect(
                "assets.not_found_handling",
                "/*",
                "/index.html",
                200,
                false,
                true,
            );
        }
        Some(Value::String(s)) if s == "404-page" || s == "none" => {}
        Some(Value::String(s)) => {
            out.skip("assets.not_found_handling", format!("unknown value '{s}'"))
        }
        Some(_) => out.skip("assets.not_found_handling", "expected a string"),
        None => {}
    }
    if a.get("html_handling")
        .is_some_and(|v| v.as_str() != Some("auto-trailing-slash"))
    {
        out.skip("assets.html_handling", TRAILING);
    }
    if a.contains_key("run_worker_first") {
        out.skip("assets.run_worker_first", "Workers are not run");
    }
}

// ------------------------------------------------------------------ .htaccess

/// Split a directive line into words, honouring "double quotes" (a backslash escapes the next character).
fn words(line: &str) -> Option<Vec<String>> {
    let c: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < c.len() && (c[i] == ' ' || c[i] == '\t') {
            i += 1;
        }
        if i >= c.len() {
            return Some(out);
        }
        if c[i] == '"' {
            let mut w = String::new();
            i += 1;
            loop {
                if i >= c.len() {
                    return None;
                }
                if c[i] == '\\' && i + 1 < c.len() {
                    w.push(c[i + 1]);
                    i += 2;
                } else if c[i] == '"' {
                    i += 1;
                    break;
                } else {
                    w.push(c[i]);
                    i += 1;
                }
            }
            out.push(w);
        } else {
            let start = i;
            while i < c.len() && c[i] != ' ' && c[i] != '\t' {
                i += 1;
            }
            out.push(c[start..i].iter().collect());
        }
    }
}

fn apache_status(s: &str) -> Option<i64> {
    match s {
        "permanent" => Some(301),
        "temp" => Some(302),
        "seeother" => Some(303),
        _ => None,
    }
}

fn three_digits(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit())
}

/// The status word of `Redirect`/`RedirectMatch`, if `first` is one: Ok(status), or Err(reason).
fn status_word(first: &str) -> Option<std::result::Result<i64, String>> {
    if apache_status(first).is_none() && first != "gone" && !three_digits(first) {
        return None;
    }
    if first == "gone" || first == "410" {
        return Some(Err(
            "status 410 is not supported yet (johnhenry/sitefile#193)".into(),
        ));
    }
    if let Some(s) = apache_status(first) {
        return Some(Ok(s));
    }
    if first.starts_with('3') {
        return Some(Ok(first.parse().unwrap_or(0)));
    }
    Some(Err(format!("status {first} is not supported")))
}

fn alpha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn path_chars(s: &str, slash: bool) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b) || (slash && b == b'/'))
}

fn htaccess(text: &str, out: &mut Out) {
    let mut containers: Vec<String> = Vec::new();
    for (n, raw) in text.split('\n').enumerate() {
        let entry = format!("line {}", n + 1);
        let t = crate::server::js_trim(raw.strip_suffix('\r').unwrap_or(raw));
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // </Name>
        if let Some(inner) = t.strip_prefix("</").and_then(|x| x.strip_suffix('>')) {
            if alpha(inner.trim_end_matches([' ', '\t'])) {
                containers.pop();
                continue;
            }
        }
        // <Name ...>
        if let Some(inner) = t.strip_prefix('<').and_then(|x| x.strip_suffix('>')) {
            let end = inner.find([' ', '\t']).unwrap_or(inner.len());
            let (name, rest) = (&inner[..end], &inner[end..]);
            if alpha(name) && !rest.contains('>') {
                containers.push(name.to_ascii_lowercase());
                if name.to_ascii_lowercase() != "ifmodule" {
                    out.skip(
                        &entry,
                        format!(
                            "<{name}> sections are not supported; their directives are skipped"
                        ),
                    );
                }
                continue;
            }
        }
        if containers.iter().any(|c| c != "ifmodule") {
            continue;
        }
        let w = match words(t) {
            Some(w) if !w.is_empty() => w,
            _ => {
                out.skip(&entry, "unbalanced quotes");
                continue;
            }
        };
        let d = w[0].to_lowercase();
        if d == "redirect" || d == "redirectpermanent" || d == "redirecttemp" {
            let mut args = &w[1..];
            let mut status = if d == "redirectpermanent" { 301 } else { 302 };
            if d == "redirect" {
                let first = args.first().map(|s| s.to_lowercase()).unwrap_or_default();
                match status_word(&first) {
                    Some(Err(reason)) => {
                        out.skip(&entry, reason);
                        continue;
                    }
                    Some(Ok(s)) => {
                        status = s;
                        args = &args[1..];
                    }
                    None => {}
                }
            }
            if args.len() != 2 {
                out.skip(&entry, "expected a path and a URL");
                continue;
            }
            let (from, to) = (&args[0], &args[1]);
            if out.redirect(&entry, from, to, status, false, false) && from != "/" {
                let f = format!("{}/*", from.strip_suffix('/').unwrap_or(from));
                let t2 = format!("{}/:splat", to.strip_suffix('/').unwrap_or(to));
                out.redirect(&entry, &f, &t2, status, false, false);
            }
            continue;
        }
        if d == "redirectmatch" {
            let mut args = &w[1..];
            let mut status = 302;
            let first = args.first().map(|s| s.to_lowercase()).unwrap_or_default();
            match status_word(&first) {
                Some(Err(reason)) => {
                    out.skip(&entry, reason);
                    continue;
                }
                Some(Ok(s)) => {
                    status = s;
                    args = &args[1..];
                }
                None => {}
            }
            if args.len() != 2 {
                out.skip(&entry, "expected a pattern and a URL");
                continue;
            }
            let (re, to) = (args[0].as_str(), args[1].as_str());
            // ^/path$
            if let Some(p) = re.strip_prefix('^').and_then(|x| x.strip_suffix('$')) {
                if p.starts_with('/') && path_chars(p, true) {
                    out.redirect(&entry, p, to, status, false, false);
                    continue;
                }
            }
            // ^/prefix/(.*)$
            let body = re
                .strip_prefix('^')
                .map(|x| x.strip_suffix('$').unwrap_or(x));
            if let Some(prefix) = body.and_then(|b| b.strip_suffix("(.*)")) {
                let segs_ok = prefix.starts_with('/')
                    && prefix.ends_with('/')
                    && (prefix == "/"
                        || prefix[1..prefix.len() - 1]
                            .split('/')
                            .all(|s| !s.is_empty() && path_chars(s, false)));
                let only_one = to
                    .match_indices('$')
                    .all(|(i, _)| to.as_bytes().get(i + 1) == Some(&b'1'));
                if segs_ok && only_one {
                    out.redirect(
                        &entry,
                        &format!("{prefix}*"),
                        &to.split("$1").collect::<Vec<_>>().join(":splat"),
                        status,
                        false,
                        false,
                    );
                    continue;
                }
            }
            out.skip(
                &entry,
                "only ^/path$ and ^/prefix/(.*)$ patterns are supported",
            );
            continue;
        }
        if d == "errordocument" {
            if w.len() == 3 && w[1] == "404" && w[2].starts_with('/') {
                out.redirect(&entry, "/*", &w[2], 404, false, true);
            } else if w.get(1).map(String::as_str) == Some("404") {
                out.skip(&entry, "only a local page is supported");
            } else {
                out.skip(
                    &entry,
                    format!(
                        "ErrorDocument {} is not supported",
                        w.get(1).map(String::as_str).unwrap_or("undefined")
                    ),
                );
            }
            continue;
        }
        if d == "directoryindex" {
            if !(w.len() == 2 && w[1] == "index.html") {
                out.skip(&entry, "serve always uses index.html");
            }
            continue;
        }
        if d == "header" {
            let mut args = &w[1..];
            if args.first().is_some_and(|a| a.to_lowercase() == "always") {
                args = &args[1..];
            }
            if args.len() != 3 || args[0].to_lowercase() != "set" {
                out.skip(&entry, "only Header set <name> <value> is supported");
                continue;
            }
            out.header(&entry, "/*", vec![(args[1].clone(), args[2].clone())]);
            continue;
        }
        if d == "addtype" {
            out.skip(&entry, "custom content types are johnhenry/sitefile#220");
            continue;
        }
        out.skip(&entry, format!("{} is not translated", w[0]));
    }
}

// ------------------------------------------------------------------ entry point

/// Translate the text of a host config file. `file` names it in messages.
pub fn import_host_config(text: &str, file: &str, format: &str) -> Result<ImportedConfig> {
    let mut out = Out::default();
    if format == "htaccess" {
        htaccess(text, &mut out);
    } else {
        let doc = if format == "netlify"
            || (format == "wrangler" && !file.to_lowercase().ends_with(".json"))
        {
            parse_toml(text, file)?
        } else {
            serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(text)).map_err(
                |e| SitefileError::plain(E_DATA, format!("{file} is not valid JSON: {e}")),
            )?
        };
        let Value::Object(doc) = doc else {
            return Err(SitefileError::plain(
                E_DATA,
                format!("{file}: expected an object at the top level"),
            ));
        };
        match format {
            "netlify" => netlify(&doc, &mut out),
            "vercel" => vercel(&doc, &mut out),
            "firebase" => firebase(&doc, &mut out),
            "azure" => azure(&doc, &mut out),
            _ => wrangler(&doc, &mut out),
        }
    }
    let mut redirects = out.redirects;
    redirects.extend(out.fallbacks);
    Ok(ImportedConfig {
        format: format.to_string(),
        redirects,
        headers: out.headers,
        unsupported: out.unsupported,
    })
}

/// `_redirects` text (empty when there are no rules).
pub fn redirects_text(c: &ImportedConfig) -> String {
    c.redirects.iter().map(|l| format!("{l}\n")).collect()
}

/// `_headers` text (empty when there are no blocks).
pub fn headers_text(c: &ImportedConfig) -> String {
    let mut s = String::new();
    for b in &c.headers {
        s.push_str(&b.pattern);
        s.push('\n');
        for (n, v) in &b.headers {
            s.push_str(&format!("  {n}: {v}\n"));
        }
    }
    s
}
