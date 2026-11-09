//! Site files: sitemap.xml, an Atom feed, `_redirects` and `_headers` (port of
//! `src/site.ts`; docs/LANGUAGE.md "Site files"). Output bytes and messages
//! match the TypeScript implementation.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::export::wbn::encode_uri_component;
use crate::tree::Tree;

/// ASCII whitespace, other control characters, and DEL (`[\u0000- \u007f]`).
fn space_or_control(s: &str) -> bool {
    s.chars().any(|c| (c as u32) <= 0x20 || c == '\u{7f}')
}

/// Control characters other than tab.
fn control(s: &str) -> bool {
    s.chars()
        .any(|c| ((c as u32) < 0x20 && c != '\t') || c == '\u{7f}')
}

/// An RFC 9110 token (header names).
pub fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `^https?://` (case-insensitive): the length of the scheme and `://`.
fn http_prefix(s: &str) -> Option<usize> {
    let lower = s
        .get(..8)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| s.to_ascii_lowercase());
    if lower.starts_with("https://") {
        Some(8)
    } else if lower.starts_with("http://") {
        Some(7)
    } else {
        None
    }
}

/// Check a SITEMAP/FEED `BASE`: an absolute http(s) URL with a host and no credentials,
/// query, fragment, whitespace or control characters. Returns it without trailing slashes.
pub fn check_base_url(raw: &str, what: &str, span: &SourceSpan) -> Result<String> {
    let ok = http_prefix(raw).is_some_and(|n| {
        let host = raw[n..].split('/').next().unwrap_or("");
        !host.is_empty() && !host.contains('@')
    }) && !raw
        .chars()
        .any(|c| (c as u32) <= 0x20 || c == '\u{7f}' || c == '?' || c == '#');
    if !ok {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("{what} '{raw}' must be an absolute http(s) URL such as https://example.com (no credentials, query or fragment)"),
            span,
        ));
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// `<base><path>` with each segment percent-encoded; `…/index.html` becomes its directory URL `…/`.
pub fn page_url(base: &str, path: &str) -> String {
    let p = path
        .strip_suffix("index.html")
        .filter(|_| path.ends_with("/index.html"))
        .unwrap_or(path);
    let encoded: Vec<String> = p.split('/').map(encode_uri_component).collect();
    format!("{base}{}", encoded.join("/"))
}

/// Escape text for XML content and attribute values; characters XML 1.0 forbids become U+FFFD.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// sitemaps.org XML for every `.html` file in the tree except `/404.html`, sorted by path (UTF-8 bytes).
pub fn sitemap_xml(tree: &Tree, base: &str) -> String {
    format!(
        "{SITEMAP_HEAD}{}{SITEMAP_TAIL}",
        sitemap_entries(tree, base).concat()
    )
}

const SITEMAP_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n";
const SITEMAP_TAIL: &str = "</urlset>\n";

/// The sitemaps.org limits of one sitemap file: 50,000 URLs and 50 MiB (52,428,800 bytes) uncompressed.
pub const SITEMAP_LIMITS: (usize, usize) = (50_000, 52_428_800);

fn sitemap_entries(tree: &Tree, base: &str) -> Vec<String> {
    let mut paths: Vec<&String> = tree
        .entries()
        .map(|(p, _)| p)
        .filter(|p| p.ends_with(".html") && p.as_str() != "/404.html")
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            format!(
                "  <url><loc>{}</loc></url>\n",
                xml_escape(&page_url(base, p))
            )
        })
        .collect()
}

/// `/sitemap.xml` -> `/sitemap-2.xml`: the n-th part of a split sitemap (`sitemapPart` in src/site.ts).
pub fn sitemap_part(dest: &str, n: usize) -> String {
    let slash = dest.rfind('/').map_or(0, |i| i + 1);
    match dest.rfind('.') {
        Some(dot) if dot > slash => format!("{}-{n}{}", &dest[..dot], &dest[dot..]),
        _ => format!("{dest}-{n}"),
    }
}

/// The sitemap files for the tree (`sitemapFiles` in src/site.ts): one at `dest` within `limits`
/// (URLs, bytes), or else a sitemap index at `dest` and the parts it lists.
pub fn sitemap_files(
    tree: &Tree,
    base: &str,
    dest: &str,
    limits: (usize, usize),
) -> Vec<(String, String)> {
    let entries = sitemap_entries(tree, base);
    let frame = SITEMAP_HEAD.len() + SITEMAP_TAIL.len();
    let total = frame + entries.iter().map(String::len).sum::<usize>();
    if entries.len() <= limits.0 && total <= limits.1 {
        return vec![(
            dest.to_string(),
            format!("{SITEMAP_HEAD}{}{SITEMAP_TAIL}", entries.concat()),
        )];
    }
    let mut parts: Vec<Vec<&String>> = Vec::new();
    let mut cur: Vec<&String> = Vec::new();
    let mut bytes = frame;
    for e in &entries {
        if !cur.is_empty() && (cur.len() >= limits.0 || bytes + e.len() > limits.1) {
            parts.push(std::mem::take(&mut cur));
            bytes = frame;
        }
        cur.push(e);
        bytes += e.len();
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    let files: Vec<(String, String)> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let body: String = p.iter().map(|s| s.as_str()).collect();
            (
                sitemap_part(dest, i + 1),
                format!("{SITEMAP_HEAD}{body}{SITEMAP_TAIL}"),
            )
        })
        .collect();
    let mut index = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for (p, _) in &files {
        index.push_str(&format!(
            "  <sitemap><loc>{}</loc></sitemap>\n",
            xml_escape(&page_url(base, p))
        ));
    }
    index.push_str("</sitemapindex>\n");
    let mut out = vec![(dest.to_string(), index)];
    out.extend(files);
    out
}

// ---------------------------------------------------------------- dates

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedDate {
    /// RFC 3339 form written to the feed.
    pub text: String,
    /// Seconds since the epoch (UTC), for picking the newest entry.
    pub seconds: i64,
    /// Fractional seconds as 9 digits.
    pub fraction: String,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn digits(s: &str, n: usize) -> Option<i64> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// `YYYY-MM-DD` (midnight UTC) or an RFC 3339 date-time; `None` when it is neither.
pub fn parse_feed_date(s: &str) -> Option<FeedDate> {
    if !s.is_ascii() || s.len() < 10 {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let b = date.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (
        digits(&date[..4], 4)?,
        digits(&date[5..7], 2)?,
        digits(&date[8..], 2)?,
    );
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    let day = days_from_civil(y, mo, d) * 86400;
    if rest.is_empty() {
        return Some(FeedDate {
            text: format!("{date}T00:00:00Z"),
            seconds: day,
            fraction: "000000000".into(),
        });
    }
    // [Tt ]HH:MM[:SS[.frac]](Z|z|±HH:MM)
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    if rest.len() < 6 || rest.as_bytes()[2] != b':' {
        return None;
    }
    let (hh, mm) = (&rest[..2], &rest[3..5]);
    let (h, mi) = (digits(hh, 2)?, digits(mm, 2)?);
    let mut tail = &rest[5..];
    let mut ss = None;
    let mut frac = "";
    if let Some(t) = tail.strip_prefix(':') {
        ss = Some(t.get(..2)?);
        digits(ss?, 2)?;
        tail = &t[2..];
        if let Some(f) = tail.strip_prefix('.') {
            let n = f.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return None;
            }
            frac = &tail[..n + 1];
            tail = &f[n..];
        }
    }
    let sec = ss.map(|v| v.parse::<i64>().unwrap_or(0)).unwrap_or(0);
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let zone = tail.to_ascii_uppercase();
    let offset = if zone == "Z" {
        0
    } else {
        let zb = zone.as_bytes();
        if zone.len() != 6 || !(zb[0] == b'+' || zb[0] == b'-') || zb[3] != b':' {
            return None;
        }
        let (oh, om) = (digits(&zone[1..3], 2)?, digits(&zone[4..], 2)?);
        if oh > 23 || om > 59 {
            return None;
        }
        (if zb[0] == b'-' { -1 } else { 1 }) * (oh * 3600 + om * 60)
    };
    let text = format!("{date}T{hh}:{mm}:{}{frac}{zone}", ss.unwrap_or("00"));
    let fraction = format!("{}000000000", frac.get(1..).unwrap_or(""))[..9].to_string();
    Some(FeedDate {
        text,
        seconds: day + h * 3600 + mi * 60 + sec - offset,
        fraction,
    })
}

fn newer(a: &FeedDate, b: &FeedDate) -> bool {
    a.seconds > b.seconds || (a.seconds == b.seconds && a.fraction > b.fraction)
}

// ---------------------------------------------------------------- feed

#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub title: String,
    pub url: String,
    pub date: FeedDate,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
}

/// Atom 1.0. The feed's id is its own URL; each entry's id is its link.
pub fn atom_xml(title: &str, site_url: &str, self_url: &str, entries: &[FeedEntry]) -> String {
    let mut updated: Option<&FeedDate> = None;
    for e in entries {
        if updated.is_none_or(|u| newer(&e.date, u)) {
            updated = Some(&e.date);
        }
    }
    let x = xml_escape;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    out.push_str(&format!("  <title>{}</title>\n", x(title)));
    out.push_str(&format!("  <link href=\"{}\"/>\n", x(site_url)));
    out.push_str(&format!(
        "  <link rel=\"self\" href=\"{}\"/>\n",
        x(self_url)
    ));
    out.push_str(&format!("  <id>{}</id>\n", x(self_url)));
    out.push_str(&format!(
        "  <updated>{}</updated>\n",
        updated
            .map(|u| u.text.as_str())
            .unwrap_or("1970-01-01T00:00:00Z")
    ));
    for e in entries {
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <title>{}</title>\n", x(&e.title)));
        out.push_str(&format!("    <link href=\"{}\"/>\n", x(&e.url)));
        out.push_str(&format!("    <id>{}</id>\n", x(&e.url)));
        out.push_str(&format!("    <updated>{}</updated>\n", e.date.text));
        if let Some(a) = &e.author {
            out.push_str(&format!("    <author><name>{}</name></author>\n", x(a)));
        }
        if let Some(s) = &e.summary {
            out.push_str(&format!("    <summary>{}</summary>\n", x(s)));
        }
        if let Some(c) = &e.content {
            out.push_str(&format!("    <content type=\"html\">{}</content>\n", x(c)));
        }
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

// ---------------------------------------------------------------- _redirects and _headers

/// Check a REDIRECT source or HEADER pattern: an absolute path with no spaces or control characters.
pub fn check_site_path(raw: &str, what: &str, span: &SourceSpan) -> Result<String> {
    if space_or_control(raw) {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("{what} must not contain spaces or control characters"),
            span,
        ));
    }
    if !raw.starts_with('/') {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("{what} '{raw}' must be an absolute path starting with '/'"),
            span,
        ));
    }
    Ok(raw.to_string())
}

/// Check a REDIRECT target: an absolute path or an http(s) URL.
pub fn check_redirect_target(raw: &str, span: &SourceSpan) -> Result<String> {
    if space_or_control(raw) {
        return Err(SitefileError::at(
            E_VALIDATION,
            "REDIRECT target must not contain spaces or control characters",
            span,
        ));
    }
    let url = http_prefix(raw).is_some_and(|n| raw[n..].chars().next().is_some_and(|c| c != '/'));
    if !raw.starts_with('/') && !url {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("REDIRECT target '{raw}' must be an absolute path or an http(s) URL"),
            span,
        ));
    }
    Ok(raw.to_string())
}

pub fn check_header_name(raw: &str, span: &SourceSpan) -> Result<String> {
    if space_or_control(raw) {
        return Err(SitefileError::at(
            E_VALIDATION,
            "HEADER name must not contain spaces or control characters",
            span,
        ));
    }
    if !is_token(raw) {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("HEADER name '{raw}' is not a valid header name (letters, digits and !#$%&'*+-.^_`|~)"),
            span,
        ));
    }
    Ok(raw.to_string())
}

/// The header value: trimmed of spaces and tabs; non-empty, no control characters other than tab.
pub fn check_header_value(raw: &str, name: &str, span: &SourceSpan) -> Result<String> {
    if control(raw) {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("HEADER {name}: the value must not contain control characters (CR, LF, …)"),
            span,
        ));
    }
    let v = raw.trim_matches(|c| c == ' ' || c == '\t');
    if v.is_empty() {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("HEADER {name}: the value is empty"),
            span,
        ));
    }
    Ok(v.to_string())
}

/// `/_redirects` in the Netlify / Cloudflare Pages format: `from to status`, one per line, in source order.
pub fn redirects_file<'a>(rules: impl IntoIterator<Item = (&'a str, &'a str, u16)>) -> String {
    rules
        .into_iter()
        .map(|(from, to, status)| format!("{from} {to} {status}\n"))
        .collect()
}

/// `/_headers`: one block per path pattern (in order of first use), its headers indented in source order.
pub fn headers_file<'a>(rules: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> String {
    let mut blocks: Vec<(&str, String)> = Vec::new();
    for (pattern, name, value) in rules {
        let line = format!("  {name}: {value}\n");
        match blocks.iter_mut().find(|(p, _)| *p == pattern) {
            Some((_, lines)) => lines.push_str(&line),
            None => blocks.push((pattern, line)),
        }
    }
    blocks
        .into_iter()
        .map(|(p, lines)| format!("{p}\n{lines}"))
        .collect()
}

// ---------------------------------------------------------------- FINGERPRINT cache headers

/// The `Cache-Control` of a fingerprinted file: its name changes with its content, so it never goes stale.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// The `Cache-Control` of a page: revalidate on every request, so it always names the current assets.
pub const NO_CACHE: &str = "no-cache";

/// The `/_headers` rules (pattern, value of `Cache-Control`) EXPORT adds for FINGERPRINT (`cacheHeaderRules`
/// in src/site.ts): `files` are the URL paths of every file in the exported directory, `hashed` the
/// fingerprinted ones. A fingerprinted file gets [`IMMUTABLE`] through a `/dir/*` rule for the topmost
/// directory (below the root) that holds only fingerprinted files, or else through its own path; pages get
/// [`NO_CACHE`] through `/`, `/*/` and `/*.html` (each only when a page it matches exists). Paths with spaces
/// or control characters are left out. Empty when nothing is fingerprinted.
pub fn cache_header_rules(
    files: &[String],
    hashed: &std::collections::HashSet<String>,
) -> Vec<(String, &'static str)> {
    use std::collections::HashMap;
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    let dirs = |f: &str| -> Vec<String> {
        let segs: Vec<&str> = f.split('/').collect();
        (2..segs.len()).map(|k| segs[..k].join("/")).collect()
    };
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for f in &sorted {
        for d in dirs(f) {
            let c = counts.entry(d).or_insert((0, 0));
            c.0 += 1;
            if hashed.contains(*f) {
                c.1 += 1;
            }
        }
    }
    let mut out: Vec<(String, &'static str)> = Vec::new();
    fn add(out: &mut Vec<(String, &'static str)>, pattern: String, value: &'static str) {
        if !space_or_control(&pattern) && !out.iter().any(|(p, _)| *p == pattern) {
            out.push((pattern, value));
        }
    }
    for f in &sorted {
        if !hashed.contains(*f) {
            continue;
        }
        let pattern = dirs(f)
            .into_iter()
            .find(|d| {
                let c = counts[d];
                c.0 == c.1
            })
            .map_or_else(|| (*f).clone(), |d| format!("{d}/*"));
        add(&mut out, pattern, IMMUTABLE);
    }
    if out.is_empty() {
        return out;
    }
    let pages: Vec<&&String> = sorted
        .iter()
        .filter(|f| f.to_ascii_lowercase().ends_with(".html"))
        .collect();
    if pages.iter().any(|f| f.as_str() == "/index.html") {
        add(&mut out, "/".into(), NO_CACHE);
    }
    if pages
        .iter()
        .any(|f| f.as_str() != "/index.html" && f.to_ascii_lowercase().ends_with("/index.html"))
    {
        add(&mut out, "/*/".into(), NO_CACHE);
    }
    if !pages.is_empty() {
        add(&mut out, "/*.html".into(), NO_CACHE);
    }
    out
}

// ---------------------------------------------------------------- robots.txt and security.txt

/// `/robots.txt` (RFC 9309) for every user agent (`robotsTxt` in src/site.ts).
pub fn robots_txt(rules: &[(bool, String)], sitemaps: &[String]) -> String {
    let mut out = String::from("User-agent: *\n");
    if rules.is_empty() {
        out.push_str("Disallow:\n");
    }
    for (allow, path) in rules {
        out.push_str(&format!(
            "{}: {path}\n",
            if *allow { "Allow" } else { "Disallow" }
        ));
    }
    if !sitemaps.is_empty() {
        out.push('\n');
        for u in sitemaps {
            out.push_str(&format!("Sitemap: {u}\n"));
        }
    }
    out
}

/// A security.txt URI field: one of `schemes` (lowercase, with their `:` or `://`), no spaces or control characters.
pub fn check_field_uri(
    raw: &str,
    field: &str,
    schemes: &[&str],
    span: &SourceSpan,
) -> Result<String> {
    let lower = raw.to_lowercase();
    if space_or_control(raw)
        || !schemes
            .iter()
            .any(|s| lower.starts_with(s) && raw.chars().count() > s.chars().count())
    {
        let list: Vec<String> = schemes
            .iter()
            .map(|s| match s.strip_suffix("//") {
                Some(p) => format!("{p}//…"),
                None => s.to_string(),
            })
            .collect();
        return Err(SitefileError::at(
            E_VALIDATION,
            format!(
                "SECURITY_TXT {field} '{raw}' must be a {} URI with no spaces",
                list.join(", ")
            ),
            span,
        ));
    }
    Ok(raw.to_string())
}

/// RFC 9116 `security.txt`: Contact lines, Expires, then Encryption and Policy lines, in source order.
pub fn security_txt(
    contact: &[String],
    expires: &str,
    encryption: &[String],
    policy: &[String],
) -> String {
    let mut out = String::new();
    for v in contact {
        out.push_str(&format!("Contact: {v}\n"));
    }
    out.push_str(&format!("Expires: {expires}\n"));
    for v in encryption {
        out.push_str(&format!("Encryption: {v}\n"));
    }
    for v in policy {
        out.push_str(&format!("Policy: {v}\n"));
    }
    out
}

// ---------------------------------------------------------------- MIME

/// Check a MIME extension (`checkMimeExtension` in src/site.ts): `.` and `[A-Za-z0-9+_-]+`, lowercased.
pub fn check_mime_extension(raw: &str, span: &SourceSpan) -> Result<String> {
    let ok = raw.len() > 1
        && raw.starts_with('.')
        && raw[1..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'_' || b == b'-');
    if !ok {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("MIME extension '{raw}' must be a dot and letters, digits, '+', '-' or '_' (e.g. .usdz)"),
            span,
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

/// An RFC 6838 restricted name: an alphanumeric first character, then up to 126 of `[A-Za-z0-9!#$&^_.+-]`.
fn restricted_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 127
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(c))
}

/// Check a MIME media type (`checkMimeType` in src/site.ts): `type/subtype`, no parameters, lowercased.
pub fn check_mime_type(raw: &str, span: &SourceSpan) -> Result<String> {
    let ok = match raw.split_once('/') {
        Some((t, s)) => restricted_name(t) && restricted_name(s),
        None => false,
    };
    if !ok {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("MIME type '{raw}' must be type/subtype (e.g. model/vnd.usdz+zip), without parameters"),
            span,
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sitemap_split_matches_typescript() {
        // The same tree and expected files as SPLIT_EXPECTED in test/canonical.test.ts.
        let t = Tree::from_files(
            [
                "/index.html",
                "/a.html",
                "/b/index.html",
                "/404.html",
                "/x.css",
            ]
            .iter()
            .map(|p| {
                (
                    p.to_string(),
                    crate::tree::make_file(Vec::new(), "test", 0o644),
                )
            }),
        )
        .unwrap();
        assert_eq!(
            sitemap_files(&t, "https://e.x", "/sitemap.xml", SITEMAP_LIMITS).len(),
            1
        );
        let split = sitemap_files(&t, "https://e.x", "/maps/site.xml", (2, 1_000_000));
        let head = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
        assert_eq!(
            split,
            vec![
                ("/maps/site.xml".to_string(), format!("{head}<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n  <sitemap><loc>https://e.x/maps/site-1.xml</loc></sitemap>\n  <sitemap><loc>https://e.x/maps/site-2.xml</loc></sitemap>\n</sitemapindex>\n")),
                ("/maps/site-1.xml".to_string(), format!("{head}<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n  <url><loc>https://e.x/a.html</loc></url>\n  <url><loc>https://e.x/b/</loc></url>\n</urlset>\n")),
                ("/maps/site-2.xml".to_string(), format!("{head}<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n  <url><loc>https://e.x/</loc></url>\n</urlset>\n")),
            ]
        );
        let by_bytes: Vec<String> = sitemap_files(&t, "https://e.x", "/s.xml", (100, 150))
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(by_bytes, ["/s.xml", "/s-1.xml", "/s-2.xml", "/s-3.xml"]);
        assert_eq!(sitemap_part("/sitemap.xml", 3), "/sitemap-3.xml");
        assert_eq!(sitemap_part("/maps/sitemap", 1), "/maps/sitemap-1");
        assert_eq!(sitemap_part("/a.b/map", 1), "/a.b/map-1");
    }

    fn span() -> SourceSpan {
        SourceSpan::new("S", 1, 1)
    }

    #[test]
    fn urls_and_escaping() {
        assert_eq!(page_url("https://x", "/index.html"), "https://x/");
        assert_eq!(page_url("https://x", "/a/index.html"), "https://x/a/");
        assert_eq!(
            page_url("https://x", "/a/myindex.html"),
            "https://x/a/myindex.html"
        );
        assert_eq!(
            page_url("https://x/blog", "/a b/ü.html"),
            "https://x/blog/a%20b/%C3%BC.html"
        );
        assert_eq!(
            xml_escape("<a href=\"x\">&'\u{1}"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&apos;\u{fffd}"
        );
        assert_eq!(
            check_base_url("HTTPS://x.example//", "B", &span()).unwrap(),
            "HTTPS://x.example"
        );
        for bad in [
            "ftp://x",
            "https://",
            "https://u@x",
            "https://x/?q",
            "https://x/#f",
            "https://x y",
            "x.example",
        ] {
            let e = check_base_url(bad, "SITEMAP BASE", &span()).unwrap_err();
            assert!(
                e.detail.contains("must be an absolute http(s) URL"),
                "{bad}"
            );
        }
    }

    #[test]
    fn dates() {
        assert_eq!(
            parse_feed_date("2026-03-01").unwrap().text,
            "2026-03-01T00:00:00Z"
        );
        assert_eq!(
            parse_feed_date("2026-03-01 10:20z").unwrap().text,
            "2026-03-01T10:20:00Z"
        );
        assert_eq!(
            parse_feed_date("2026-03-01T10:20:30.5+02:00").unwrap().text,
            "2026-03-01T10:20:30.5+02:00"
        );
        assert_eq!(
            parse_feed_date("2026-03-01T10:20:30.5+02:00")
                .unwrap()
                .fraction,
            "500000000"
        );
        assert_eq!(
            parse_feed_date("2026-03-01T10:20:30+02:00")
                .unwrap()
                .seconds,
            parse_feed_date("2026-03-01T08:20:30Z").unwrap().seconds
        );
        for bad in [
            "2026-02-30",
            "2026-13-01",
            "2026-3-1",
            "March 1",
            "2026-03-01T25:00Z",
            "2026-03-01T10:00",
            "2026-03-01T10:00:5Z",
            "2026-03-01T10:00:05.Z",
            "２０２６-03-01",
        ] {
            assert_eq!(parse_feed_date(bad), None, "{bad}");
        }
        assert_eq!(parse_feed_date("2024-02-29").unwrap().seconds, 1709164800);
    }

    #[test]
    fn rule_files_and_checks() {
        assert_eq!(
            redirects_file([("/a", "/b", 301), ("/c/*", "https://x/:splat", 302)]),
            "/a /b 301\n/c/* https://x/:splat 302\n"
        );
        assert_eq!(
            headers_file([("/*", "A", "1"), ("/x", "B", "2"), ("/*", "C", "3")]),
            "/*\n  A: 1\n  C: 3\n/x\n  B: 2\n"
        );
        assert!(check_site_path("/a b", "REDIRECT source", &span())
            .unwrap_err()
            .detail
            .contains("must not contain spaces"));
        assert!(check_site_path("a", "REDIRECT source", &span())
            .unwrap_err()
            .detail
            .contains("REDIRECT source 'a' must be an absolute path"));
        assert!(check_redirect_target("https://x/a", &span()).is_ok());
        for bad in ["b", "javascript:alert(1)", "https:///x", "/a\nb"] {
            assert!(check_redirect_target(bad, &span()).is_err(), "{bad}");
        }
        assert!(check_header_name("X(A)", &span())
            .unwrap_err()
            .detail
            .contains("is not a valid header name"));
        assert_eq!(check_header_value(" a\tb ", "X", &span()).unwrap(), "a\tb");
        assert!(check_header_value("a\r\nb", "X", &span())
            .unwrap_err()
            .detail
            .contains("control characters"));
        assert!(check_header_value(" ", "X", &span())
            .unwrap_err()
            .detail
            .contains("the value is empty"));
    }
}
