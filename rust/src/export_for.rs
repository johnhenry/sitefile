//! `EXPORT … FOR <host>` (port of `src/export-for.ts`, issue #205): write
//! what each host reads for the REDIRECT and HEADER rules (and FINGERPRINT's
//! cache headers). A rule the host cannot express as `sitefile serve` (and
//! Netlify) apply it is an E_EXPORT error naming the rule, or with `--lossy` a
//! warning and the rule is left out.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::server::glob_match;
use crate::tree::{make_file, ConflictPolicy, Tree};
use crate::values::js_json_pretty;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRedirect {
    pub from: String,
    pub to: String,
    pub status: u16,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHeader {
    pub pattern: String,
    pub name: String,
    pub value: String,
    pub span: SourceSpan,
    /// Written by FINGERPRINT, not a HEADER line.
    pub auto: bool,
}

pub struct HostOptions<'a> {
    pub label: &'a str,
    pub lossy: bool,
    pub warn: &'a dyn Fn(&str),
    /// EXPORT's BASE (GitHub Pages: the custom domain for CNAME).
    pub base: Option<&'a str>,
    pub span: &'a SourceSpan,
}

pub struct HostResult {
    /// Indexes of the rules that stay (Cloudflare: written as `_redirects` and `_headers`).
    pub redirects: Vec<usize>,
    pub headers: Vec<usize>,
    /// The exported files, moved and with the host's files added (unused for Cloudflare).
    pub output: Tree,
}

fn is_placeholder_seg(s: &str) -> bool {
    let Some(name) = s.strip_prefix(':') else {
        return false;
    };
    let mut cs = name.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

enum Shape {
    Exact(String),
    Splat(String),
    Placeholders(Vec<String>, bool),
    Other,
}

fn shape_of(from: &str) -> Shape {
    let splat = from == "/*" || from.ends_with("/*");
    let head = if splat { &from[..from.len() - 2] } else { from };
    if head.contains('*') {
        return Shape::Other;
    }
    let segments: Vec<String> = head.split('/').skip(1).map(String::from).collect();
    if segments.iter().any(|s| s.starts_with(':')) {
        if !segments
            .iter()
            .all(|s| !s.starts_with(':') || is_placeholder_seg(s))
        {
            return Shape::Other;
        }
        return Shape::Placeholders(segments, splat);
    }
    if splat {
        return Shape::Splat(head.to_string());
    }
    Shape::Exact(from.to_string())
}

fn trim_slash(p: &str) -> &str {
    if p.len() > 1 && p.ends_with('/') {
        &p[..p.len() - 1]
    } else {
        p
    }
}

/// The path a file in `tree` answers for `from` (Netlify serves a file before a rule), if any.
fn shadowing_file(tree: &Tree, from: &str) -> Option<String> {
    let prefix = match shape_of(from) {
        Shape::Exact(path) => {
            let p = trim_slash(&path);
            let candidates = if p == "/" {
                vec!["/index.html".to_string()]
            } else {
                vec![
                    p.to_string(),
                    format!("{p}/index.html"),
                    format!("{p}.html"),
                ]
            };
            return candidates.into_iter().find(|c| tree.is_file(c));
        }
        Shape::Splat(prefix) => prefix,
        Shape::Placeholders(segs, _) => {
            let k = segs.iter().position(|s| s.starts_with(':')).unwrap_or(0);
            format!("/{}", segs[..k].join("/"))
        }
        Shape::Other => String::new(),
    };
    let dir = if prefix == "/" || prefix.is_empty() {
        "/".to_string()
    } else {
        format!("{prefix}/")
    };
    tree.entries()
        .map(|(p, _)| p)
        .find(|f| f.starts_with(&dir))
        .cloned()
}

struct Ctx<'a> {
    host: &'a str,
    o: &'a HostOptions<'a>,
}

impl Ctx<'_> {
    fn refuse(&self, r: &HostRedirect, reason: &str) -> Result<()> {
        let what = format!("REDIRECT {} {} {}", r.from, r.to, r.status);
        if self.o.lossy {
            (self.o.warn)(&format!(
                "{}:{} {what}: {reason}; left out (--lossy)",
                self.o.label, r.span.line
            ));
            return Ok(());
        }
        Err(SitefileError::at(
            E_EXPORT,
            format!(
                "EXPORT … FOR {}: {what}: {reason} (build with --lossy to leave it out)",
                self.host
            ),
            &r.span,
        ))
    }

    fn refuse_header(&self, h: &HostHeader, reason: &str) -> Result<()> {
        if h.auto {
            (self.o.warn)(&format!(
                "{}:{} FINGERPRINT: the {} header for {} is left out: {reason}",
                self.o.label, h.span.line, h.name, h.pattern
            ));
            return Ok(());
        }
        let what = format!("HEADER {} {}", h.pattern, h.name);
        if self.o.lossy {
            (self.o.warn)(&format!(
                "{}:{} {what}: {reason}; left out (--lossy)",
                self.o.label, h.span.line
            ));
            return Ok(());
        }
        Err(SitefileError::at(
            E_EXPORT,
            format!(
                "EXPORT … FOR {}: {what}: {reason} (build with --lossy to leave it out)",
                self.host
            ),
            &h.span,
        ))
    }

    /// Ok(false) (and refused) when a file in the export answers the rule's path first on Netlify.
    fn unshadowed(&self, tree: &Tree, r: &HostRedirect, host_name: &str) -> Result<bool> {
        match shadowing_file(tree, &r.from) {
            None => Ok(true),
            Some(f) => {
                self.refuse(r, &format!("{f} is a file in the export: Netlify and sitefile serve answer with the file, {host_name} applies the redirect first"))?;
                Ok(false)
            }
        }
    }
}

/// Header rules grouped by pattern, in order of first use.
fn blocks(headers: &[HostHeader]) -> Vec<(String, Vec<&HostHeader>)> {
    let mut out: Vec<(String, Vec<&HostHeader>)> = Vec::new();
    for h in headers {
        match out.iter_mut().find(|(p, _)| *p == h.pattern) {
            Some((_, v)) => v.push(h),
            None => out.push((h.pattern.clone(), vec![h])),
        }
    }
    out
}

fn json_bytes(v: &Value) -> Vec<u8> {
    (js_json_pretty(v) + "\n").into_bytes()
}

/// Insert `name: value`, joining with ", " when the name is there already.
fn join_into(m: &mut Map<String, Value>, name: &str, value: &str) {
    let v = match m.get(name).and_then(Value::as_str) {
        Some(prev) => format!("{prev}, {value}"),
        None => value.to_string(),
    };
    m.insert(name.to_string(), Value::String(v));
}

fn moved(output: &Tree, dir: &str) -> Result<Tree> {
    Tree::from_files(
        output
            .entries()
            .map(|(p, n)| (format!("{dir}{p}"), n.clone())),
    )
}

fn add(output: &Tree, files: Vec<(&str, Vec<u8>)>, o: &HostOptions, origin: &str) -> Result<Tree> {
    output.with_files(
        files.into_iter().map(|(p, b)| {
            (
                p.to_string(),
                make_file(b, format!("{}:{} {origin}", o.label, o.span.line), 0o644),
            )
        }),
        ConflictPolicy::Error,
        Some(o.span),
    )
}

// ------------------------------------------------------------------ Cloudflare Pages

const CF_STATIC_MAX: usize = 2000;
const CF_DYNAMIC_MAX: usize = 100;
const CF_HEADER_RULES_MAX: usize = 100;

fn cloudflare(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
) -> Result<HostResult> {
    let mut redirects = Vec::new();
    let (mut statics, mut dynamics) = (0, 0);
    for (i, r) in rs.iter().enumerate() {
        if !c.unshadowed(output, r, "Cloudflare Pages")? {
            continue;
        }
        let dynamic = !matches!(shape_of(&r.from), Shape::Exact(_));
        if if dynamic {
            dynamics >= CF_DYNAMIC_MAX
        } else {
            statics >= CF_STATIC_MAX
        } {
            c.refuse(
                r,
                &if dynamic {
                    format!("Cloudflare Pages takes at most {CF_DYNAMIC_MAX} dynamic redirects")
                } else {
                    format!("Cloudflare Pages takes at most {CF_STATIC_MAX} static redirects")
                },
            )?;
            continue;
        }
        if dynamic {
            dynamics += 1;
        } else {
            statics += 1;
        }
        redirects.push(i);
    }
    let mut headers = Vec::new();
    let mut patterns: Vec<&str> = Vec::new();
    for (i, h) in hs.iter().enumerate() {
        if !patterns.contains(&h.pattern.as_str()) {
            if patterns.len() >= CF_HEADER_RULES_MAX {
                c.refuse_header(
                    h,
                    &format!("Cloudflare Pages takes at most {CF_HEADER_RULES_MAX} header rules"),
                )?;
                continue;
            }
            patterns.push(&h.pattern);
        }
        headers.push(i);
    }
    Ok(HostResult {
        redirects,
        headers,
        output: output.clone(),
    })
}

// ------------------------------------------------------------------ GitHub Pages

fn github_pages(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
    o: &HostOptions,
) -> Result<HostResult> {
    for r in rs {
        c.refuse(r, "GitHub Pages has no redirects")?;
    }
    for h in hs {
        c.refuse_header(h, "GitHub Pages cannot set response headers")?;
    }
    let mut files: Vec<(&str, Vec<u8>)> = vec![("/.nojekyll", Vec::new())];
    if let Some(base) = o.base {
        let lower = base.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("EXPORT … FOR github-pages: BASE must be an http(s) URL, got '{base}'"),
                o.span,
            ));
        }
        let Some((host, path, query, fragment)) = crate::resources::https::url_site_parts(base)
        else {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("EXPORT … FOR github-pages: BASE '{base}' is not a URL"),
                o.span,
            ));
        };
        if !host.ends_with(".github.io") {
            if path != "/" || query || fragment {
                return Err(SitefileError::at(E_VALIDATION, format!("EXPORT … FOR github-pages: a custom domain serves the site at its root, so BASE '{base}' cannot have a path"), o.span));
            }
            files.push(("/CNAME", format!("{host}\n").into_bytes()));
        }
    }
    Ok(HostResult {
        redirects: Vec::new(),
        headers: Vec::new(),
        output: add(output, files, o, "EXPORT FOR github-pages")?,
    })
}

// ------------------------------------------------------------------ Vercel (Build Output API v3)

fn re_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ".+?^$()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A `_headers` pattern as a Vercel `src`: `*` is any run of characters, `:name` one segment.
fn header_regex(pattern: &str) -> String {
    let segs: Vec<String> = pattern
        .split('/')
        .map(|seg| {
            if is_placeholder_seg(seg) {
                "[^/]+".to_string()
            } else {
                seg.split('*').map(re_escape).collect::<Vec<_>>().join(".*")
            }
        })
        .collect();
    format!("^{}$", segs.join("/"))
}

/// The origin (`https://host`) of an http(s) target, case-insensitive, and the rest.
fn split_origin(to: &str) -> (&str, &str) {
    let lower = to.to_ascii_lowercase();
    for scheme in ["http://", "https://"] {
        if lower.starts_with(scheme) {
            let rest = &to[scheme.len()..];
            let end = scheme.len() + rest.find('/').unwrap_or(rest.len());
            return (&to[..end], &to[end..]);
        }
    }
    ("", to)
}

/// The names a target uses (`:splat`, `:name` at the start of a path segment), each rewritten by `sigil`.
fn target_with(to: &str, sigil: &dyn Fn(&str) -> String) -> (String, Vec<String>) {
    let (origin, path) = split_origin(to);
    let b = path.as_bytes();
    let mut out = String::from(origin);
    let mut names = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let at_seg = i == 0 || b[i - 1] == b'/';
        if at_seg
            && b[i] == b':'
            && b.get(i + 1)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let name = &path[i + 1..j];
            names.push(name.to_string());
            out.push_str(&sigil(name));
            i = j;
            continue;
        }
        let ch = path[i..].chars().next().unwrap_or_default();
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, names)
}

fn vercel(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
    o: &HostOptions,
) -> Result<HostResult> {
    let mut routes: Vec<Value> = Vec::new();
    for (pattern, list) in blocks(hs) {
        let mut headers = Map::new();
        for h in list {
            join_into(&mut headers, &h.name, &h.value);
        }
        routes.push(json!({ "src": header_regex(&pattern), "headers": headers, "continue": true }));
    }
    routes.push(json!({ "handle": "filesystem" }));
    for r in rs {
        let mut captures: Vec<String> = Vec::new();
        let src = match shape_of(&r.from) {
            Shape::Exact(path) => {
                let p = trim_slash(&path);
                if p == "/" {
                    "^/$".to_string()
                } else {
                    format!("^{}/?$", re_escape(p))
                }
            }
            Shape::Splat(prefix) => {
                captures.push("splat".into());
                format!("^{}/(?<splat>.*)$", re_escape(&prefix))
            }
            Shape::Placeholders(segs, splat) => {
                let parts: Vec<String> = segs
                    .iter()
                    .map(|seg| {
                        if is_placeholder_seg(seg) {
                            captures.push(seg[1..].to_string());
                            format!("(?<{}>[^/]+)", &seg[1..])
                        } else {
                            re_escape(seg)
                        }
                    })
                    .collect();
                if splat {
                    captures.push("splat".into());
                }
                format!(
                    "^/{}{}$",
                    parts.join("/"),
                    if splat { "/(?<splat>.*)" } else { "" }
                )
            }
            Shape::Other => {
                c.refuse(r, "* is supported only as a trailing /*")?;
                continue;
            }
        };
        let (to, names) = target_with(&r.to, &|n| format!("${n}"));
        if let Some(unknown) = names.iter().find(|n| !captures.contains(n)) {
            c.refuse(r, &format!(":{unknown} in the target is not in the source"))?;
            continue;
        }
        routes.push(json!({ "src": src, "headers": { "Location": to }, "status": r.status }));
    }
    if output.is_file("/404.html") {
        routes.push(json!({ "handle": "error" }));
        routes.push(json!({ "status": 404, "src": "^/.*$", "dest": "/404.html" }));
    }
    let out = add(
        &moved(output, "/.vercel/output/static")?,
        vec![(
            "/.vercel/output/config.json",
            json_bytes(&json!({ "version": 3, "routes": routes })),
        )],
        o,
        "EXPORT FOR vercel",
    )?;
    Ok(HostResult {
        redirects: Vec::new(),
        headers: Vec::new(),
        output: out,
    })
}

// ------------------------------------------------------------------ Firebase Hosting

fn firebase(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
    o: &HostOptions,
) -> Result<HostResult> {
    let mut redirects: Vec<Value> = Vec::new();
    for r in rs {
        if r.status != 301 && r.status != 302 {
            c.refuse(r, "Firebase Hosting redirects are 301 or 302")?;
            continue;
        }
        let source = match shape_of(&r.from) {
            Shape::Exact(path) => path,
            Shape::Splat(prefix) => format!("{prefix}/:splat*"),
            Shape::Placeholders(segs, splat) => {
                format!("/{}{}", segs.join("/"), if splat { "/:splat*" } else { "" })
            }
            Shape::Other => {
                c.refuse(r, "* is supported only as a trailing /*")?;
                continue;
            }
        };
        if !c.unshadowed(output, r, "Firebase Hosting")? {
            continue;
        }
        redirects.push(json!({ "source": source, "destination": r.to, "type": r.status }));
    }
    let mut headers: Vec<Value> = Vec::new();
    for (pattern, list) in blocks(hs) {
        if pattern.split('/').any(|s| s.starts_with(':')) {
            for h in list {
                c.refuse_header(
                    h,
                    "Firebase Hosting header sources are globs, without :placeholders",
                )?;
            }
            continue;
        }
        let mut glob = String::new();
        let mut prev_star = false;
        for ch in pattern.chars() {
            if ch == '*' {
                if !prev_star {
                    glob.push_str("**");
                }
                prev_star = true;
            } else {
                glob.push(ch);
                prev_star = false;
            }
        }
        let hs: Vec<Value> = list
            .iter()
            .map(|h| json!({ "key": h.name, "value": h.value }))
            .collect();
        headers.push(json!({ "source": glob, "headers": hs }));
    }
    let mut hosting = Map::new();
    hosting.insert("public".into(), json!("public"));
    if !redirects.is_empty() {
        hosting.insert("redirects".into(), Value::Array(redirects));
    }
    if !headers.is_empty() {
        hosting.insert("headers".into(), Value::Array(headers));
    }
    let out = add(
        &moved(output, "/public")?,
        vec![("/firebase.json", json_bytes(&json!({ "hosting": hosting })))],
        o,
        "EXPORT FOR firebase",
    )?;
    Ok(HostResult {
        redirects: Vec::new(),
        headers: Vec::new(),
        output: out,
    })
}

// ------------------------------------------------------------------ Azure Static Web Apps

#[derive(Clone)]
struct Route {
    prefix: String,
    below: bool,
}

fn route_of(p: &str) -> Option<Route> {
    match shape_of(p) {
        Shape::Exact(path) => Some(Route {
            prefix: path,
            below: false,
        }),
        Shape::Splat(prefix) => Some(Route {
            prefix,
            below: true,
        }),
        _ => None,
    }
}

fn overlap(a: &Route, b: &Route) -> bool {
    let under = |x: &str, dir: &str| dir.is_empty() || x.starts_with(&format!("{dir}/"));
    match (a.below, b.below) {
        (false, false) => a.prefix == b.prefix,
        (true, true) => {
            a.prefix == b.prefix || under(&a.prefix, &b.prefix) || under(&b.prefix, &a.prefix)
        }
        (true, false) => under(&b.prefix, &a.prefix),
        (false, true) => under(&a.prefix, &b.prefix),
    }
}

fn azure(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
    o: &HostOptions,
) -> Result<HostResult> {
    let mut routes: Vec<Value> = Vec::new();
    let mut taken: Vec<(Route, String)> = Vec::new();
    for r in rs {
        if r.status != 301 && r.status != 302 {
            c.refuse(r, "Azure Static Web Apps redirects are 301 or 302")?;
            continue;
        }
        let Some(route) = route_of(&r.from) else {
            let reason = if matches!(shape_of(&r.from), Shape::Placeholders(..)) {
                "Azure Static Web Apps routes have no :placeholders"
            } else {
                "* is supported only as a trailing /*"
            };
            c.refuse(r, reason)?;
            continue;
        };
        if !target_with(&r.to, &|n| n.to_string()).1.is_empty() {
            c.refuse(
                r,
                "Azure Static Web Apps redirects cannot use :splat or :placeholders in the target",
            )?;
            continue;
        }
        if !c.unshadowed(output, r, "Azure Static Web Apps")? {
            continue;
        }
        routes.push(json!({ "route": r.from, "redirect": r.to, "statusCode": r.status }));
        taken.push((route, format!("REDIRECT {}", r.from)));
    }
    let mut global = Map::new();
    let all = blocks(hs);
    if let Some((_, list)) = all.iter().find(|(p, _)| p == "/*") {
        for h in list {
            join_into(&mut global, &h.name, &h.value);
        }
    }
    let mut route_headers: Vec<(String, Map<String, Value>)> = Vec::new();
    for (pattern, list) in &all {
        if pattern == "/*" {
            continue;
        }
        let Some(route) = route_of(pattern) else {
            for h in list {
                c.refuse_header(
                    h,
                    "Azure Static Web Apps routes are exact paths or end in /*",
                )?;
            }
            continue;
        };
        if let Some((_, what)) = taken.iter().find(|(t, _)| overlap(t, &route)) {
            let reason = format!("on Azure Static Web Apps only the first matching route applies, and {what} matches the same paths");
            for h in list {
                c.refuse_header(h, &reason)?;
            }
            continue;
        }
        let mut headers = Map::new();
        for h in list {
            if global.keys().any(|g| g.eq_ignore_ascii_case(&h.name)) {
                c.refuse_header(h, &format!("on Azure Static Web Apps a route's {} replaces the one for /* instead of adding to it", h.name))?;
                continue;
            }
            join_into(&mut headers, &h.name, &h.value);
        }
        if headers.is_empty() {
            continue;
        }
        route_headers.push((pattern.clone(), headers));
        taken.push((route, format!("HEADER {pattern}")));
    }
    for (pattern, headers) in route_headers {
        routes.push(json!({ "route": pattern, "headers": headers }));
    }
    let mut config = Map::new();
    if !routes.is_empty() {
        config.insert("routes".into(), Value::Array(routes));
    }
    if !global.is_empty() {
        config.insert("globalHeaders".into(), Value::Object(global));
    }
    if output.is_file("/404.html") {
        config.insert(
            "responseOverrides".into(),
            json!({ "404": { "rewrite": "/404.html" } }),
        );
    }
    let out = add(
        output,
        vec![(
            "/staticwebapp.config.json",
            json_bytes(&Value::Object(config)),
        )],
        o,
        "EXPORT FOR azure",
    )?;
    Ok(HostResult {
        redirects: Vec::new(),
        headers: Vec::new(),
        output: out,
    })
}

// ------------------------------------------------------------------ S3 website hosting

fn s3_field(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "cache-control" => Some("CacheControl"),
        "content-type" => Some("ContentType"),
        "content-disposition" => Some("ContentDisposition"),
        "content-encoding" => Some("ContentEncoding"),
        "content-language" => Some("ContentLanguage"),
        "expires" => Some("Expires"),
        _ => None,
    }
}

fn s3(
    output: &Tree,
    rs: &[HostRedirect],
    hs: &[HostHeader],
    c: &Ctx,
    o: &HostOptions,
) -> Result<HostResult> {
    // (key, fields in insertion order)
    let mut objects: Vec<(String, Map<String, Value>)> = Vec::new();
    fn object<'m>(
        objects: &'m mut Vec<(String, Map<String, Value>)>,
        key: &str,
    ) -> &'m mut Map<String, Value> {
        if let Some(i) = objects.iter().position(|(k, _)| k == key) {
            return &mut objects[i].1;
        }
        let mut m = Map::new();
        m.insert("Key".into(), Value::String(key.to_string()));
        objects.push((key.to_string(), m));
        &mut objects.last_mut().expect("just pushed").1
    }
    let mut rules: Vec<Value> = Vec::new();
    for r in rs {
        let shape = shape_of(&r.from);
        if let Shape::Exact(path) = &shape {
            if r.status != 301 {
                c.refuse(r, "S3 object redirects are always 301")?;
                continue;
            }
            if path == "/" {
                c.refuse(r, "S3 cannot redirect the root object")?;
                continue;
            }
            let key = if path.ends_with('/') {
                format!("{}index.html", &path[1..])
            } else {
                path[1..].to_string()
            };
            if output.is_file(&format!("/{key}")) {
                c.refuse(r, &format!("/{key} is a file in the export, and an S3 object is either a file or a redirect"))?;
                continue;
            }
            object(&mut objects, &key).insert(
                "WebsiteRedirectLocation".into(),
                Value::String(r.to.clone()),
            );
            continue;
        }
        let prefix = match &shape {
            Shape::Splat(p) if !p.is_empty() => p.clone(),
            Shape::Placeholders(..) => {
                c.refuse(r, "S3 routing rules have no :placeholders")?;
                continue;
            }
            Shape::Splat(_) => {
                c.refuse(r, "S3 cannot redirect every key")?;
                continue;
            }
            _ => {
                c.refuse(r, "* is supported only as a trailing /*")?;
                continue;
            }
        };
        let (t, names) = target_with(&r.to, &|n| format!(":{n}"));
        let splat_at_end = names.len() == 1 && names[0] == "splat" && t.ends_with(":splat");
        if !names.is_empty() && !splat_at_end {
            c.refuse(r, "S3 routing rules can only keep the rest of the path at the end of the target (…/:splat)")?;
            continue;
        }
        if !c.unshadowed(output, r, "S3")? {
            continue;
        }
        let (origin, rest) = split_origin(&r.to);
        let mut redirect = Map::new();
        let path_part =
            if !origin.is_empty() && origin.len() > origin.find("://").map_or(0, |i| i + 3) {
                let i = origin.find("://").unwrap_or(0);
                redirect.insert(
                    "Protocol".into(),
                    Value::String(origin[..i].to_ascii_lowercase()),
                );
                redirect.insert(
                    "HostName".into(),
                    Value::String(origin[i + 3..].to_string()),
                );
                rest
            } else {
                &r.to[..]
            };
        let p = path_part.strip_suffix(":splat").unwrap_or(path_part);
        let p = p.strip_prefix('/').unwrap_or(p);
        if splat_at_end {
            redirect.insert("ReplaceKeyPrefixWith".into(), Value::String(p.to_string()));
        } else if !p.is_empty() {
            redirect.insert("ReplaceKeyWith".into(), Value::String(p.to_string()));
        }
        redirect.insert(
            "HttpRedirectCode".into(),
            Value::String(r.status.to_string()),
        );
        rules.push(json!({ "Condition": { "KeyPrefixEquals": format!("{}/", &prefix[1..]) }, "Redirect": redirect }));
    }
    let mut valid: Vec<&HostHeader> = Vec::new();
    for h in hs {
        if s3_field(&h.name).is_none() {
            c.refuse_header(h, "S3 serves only Cache-Control, Content-Type, Content-Disposition, Content-Encoding, Content-Language and Expires from object metadata")?;
        } else if h.pattern.split('/').any(|s| s.starts_with(':')) {
            c.refuse_header(
                h,
                "S3 object metadata is set per file, so the pattern cannot have :placeholders",
            )?;
        } else {
            valid.push(h);
        }
    }
    let files: Vec<String> = output.entries().map(|(p, _)| p.clone()).collect();
    for f in &files {
        let mut urls = vec![f.clone()];
        if let Some(dir) = f
            .strip_suffix("index.html")
            .filter(|_| f.ends_with("/index.html"))
        {
            urls.push(dir.to_string());
        }
        for h in &valid {
            if !urls.iter().any(|u| glob_match(&h.pattern, u)) {
                continue;
            }
            let field = s3_field(&h.name).expect("checked");
            join_into(object(&mut objects, &f[1..]), field, &h.value);
        }
    }
    let mut website = Map::new();
    website.insert("IndexDocument".into(), json!({ "Suffix": "index.html" }));
    if output.is_file("/404.html") {
        website.insert("ErrorDocument".into(), json!({ "Key": "404.html" }));
    }
    if !rules.is_empty() {
        website.insert("RoutingRules".into(), Value::Array(rules));
    }
    objects.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let list = Value::Array(objects.into_iter().map(|(_, m)| Value::Object(m)).collect());
    let out = add(
        output,
        vec![
            ("/_s3/website.json", json_bytes(&Value::Object(website))),
            ("/_s3/objects.json", json_bytes(&list)),
        ],
        o,
        "EXPORT FOR s3",
    )?;
    Ok(HostResult {
        redirects: Vec::new(),
        headers: Vec::new(),
        output: out,
    })
}

/// Apply `EXPORT … FOR <host>` (not `netlify`, which is `_redirects` and `_headers` as they are).
pub fn export_for_host(
    host: &str,
    output: &Tree,
    redirects: &[HostRedirect],
    headers: &[HostHeader],
    o: &HostOptions,
) -> Result<HostResult> {
    let c = Ctx { host, o };
    match host {
        "cloudflare" => cloudflare(output, redirects, headers, &c),
        "github-pages" => github_pages(output, redirects, headers, &c, o),
        "vercel" => vercel(output, redirects, headers, &c, o),
        "firebase" => firebase(output, redirects, headers, &c, o),
        "azure" => azure(output, redirects, headers, &c, o),
        _ => s3(output, redirects, headers, &c, o),
    }
}
