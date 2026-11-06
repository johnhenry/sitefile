//! Git sources over smart HTTP (port of `src/resources/git.ts`, issue #91):
//! `IMPORT name FROM git+https://host/repo.git#ref:path` and `sitefile build https://host/repo.git#ref:dir`.
//!
//! A small client for Git's protocol v2 written against the build's HTTPS client, so it inherits
//! its network policy unchanged. No `git` binary runs (no hooks, config, credential helpers or
//! filters):
//!
//!   1. GET  <repo>/info/refs?service=git-upload-pack  (Git-Protocol: version=2): capabilities
//!   2. POST <repo>/git-upload-pack  command=ls-refs    (only when the ref is not a commit)
//!   3. POST <repo>/git-upload-pack  command=fetch      want <commit>, deepen 1, done
//!
//! The packfile is parsed here (zlib objects, OFS_DELTA and REF_DELTA), every object's SHA-1 is
//! recomputed and the pack checksum verified. Only SHA-1 repositories are supported. Submodules are
//! refused; symbolic links are resolved inside the imported tree like local symlinks.

use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::nfc;
use crate::resources::https::{fetch_remote_with, redact_url, FetchRequest};
use crate::resources::types::{ArchiveLimits, FetchPolicy};
use crate::sha1::{hex, sha1_hex, Sha1};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

const USER_AGENT: &str = "sitefile/0.1";

/// 40 lowercase hex digits.
pub fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A parsed `git+https://…#ref:path` location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLocation {
    /// The URI as written (after interpolation): the lockfile and vendor key.
    pub url: String,
    /// The repository URL (`https://host/repo.git`), without `git+` and the fragment.
    pub repo: String,
    /// The ref as written: a branch, a tag, `refs/…`, a 40-hex commit, or '' for the remote HEAD.
    pub git_ref: String,
    /// The path inside the repository ('' for its root), `/`-separated, no `.` or `..`.
    pub path: String,
    /// The commit, when the ref is a full 40-hex commit id (a pin).
    pub commit: Option<String>,
}

/// `^git\+[A-Za-z][A-Za-z0-9+.-]*:`
fn git_scheme(uri: &str) -> Option<&str> {
    // Schemes are case-insensitive: GIT+HTTPS:// is git+https://.
    let rest = uri.get(..4).filter(|p| p.eq_ignore_ascii_case("git+")).map(|_| &uri[4..])?;
    let (scheme, _) = rest.split_once(':')?;
    let mut chars = scheme.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    ok.then_some(scheme)
}

/// True for URIs with a `git+` scheme (`git+https:`, `git+http:`, `git+ssh:`, …).
pub fn is_git_uri(uri: &str) -> bool {
    git_scheme(uri).is_some()
}

fn bad_ref(r: &str, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(E_VALIDATION, format!("invalid Git ref '{r}' (see git check-ref-format)"), span.cloned())
}

/// Git's ref name rules (git check-ref-format), applied to the ref in a Git URL.
pub fn check_ref_name(r: &str, span: Option<&SourceSpan>) -> Result<()> {
    if r.is_empty() {
        return Ok(());
    }
    let bad_char = r.chars().any(|c| (c as u32) <= 0x20 || c == '\x7f' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'));
    if bad_char || r.contains("..") || r.contains("@{") || r == "@" {
        return Err(bad_ref(r, span));
    }
    if r.starts_with('/') || r.ends_with('/') || r.ends_with('.') || r.starts_with('-') || r.contains("//") {
        return Err(bad_ref(r, span));
    }
    for part in r.split('/') {
        if part.starts_with('.') || part.ends_with(".lock") {
            return Err(bad_ref(r, span));
        }
    }
    Ok(())
}

/// Parse `git+https://host/repo[.git][#ref[:path]]`.
pub fn parse_git_uri(uri: &str, span: Option<&SourceSpan>) -> Result<GitLocation> {
    let scheme = git_scheme(uri).map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if scheme != "https" && scheme != "http" {
        return Err(SitefileError::new(E_PROTOCOL, format!("unsupported Git transport 'git+{scheme}:' (supported: git+https)"), span.cloned()));
    }
    let (repo_part, fragment) = match uri.find('#') {
        Some(i) => (&uri[..i], &uri[i + 1..]),
        None => (uri, ""),
    };
    let repo = repo_part["git+".len()..].to_string();
    let Some(parts) = crate::resources::https::url_parts(&repo) else {
        return Err(SitefileError::new(E_PROTOCOL, format!("invalid Git URL '{}'", redact_url(&repo)), span.cloned()));
    };
    if parts.has_credentials {
        return Err(SitefileError::new(
            E_PROTOCOL,
            format!("URL {} contains credentials; credentials in URLs are not allowed", redact_url(&repo)),
            span.cloned(),
        ));
    }
    if parts.has_query {
        return Err(SitefileError::new(E_PROTOCOL, format!("Git URL {} must not have a query string", redact_url(&repo)), span.cloned()));
    }
    let (r, raw_path) = match fragment.find(':') {
        Some(i) => (&fragment[..i], &fragment[i + 1..]),
        None => (fragment, ""),
    };
    check_ref_name(r, span)?;
    let path = git_path(raw_path, span)?;
    Ok(GitLocation { url: uri.to_string(), repo, git_ref: r.to_string(), path, commit: is_commit(r).then(|| r.to_string()) })
}

fn git_path(raw: &str, span: Option<&SourceSpan>) -> Result<String> {
    let mut segs: Vec<String> = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(SitefileError::new(E_PATH_ESCAPE, format!("path '{raw}' in a Git URL must not contain '..'"), span.cloned()));
        }
        if seg.contains('\\') || seg.contains('\0') {
            return Err(SitefileError::new(E_PATH, format!("invalid path '{raw}' in a Git URL"), span.cloned()));
        }
        segs.push(nfc(seg).into_owned());
    }
    Ok(segs.join("/"))
}

/// The URL of a location with the ref replaced by the commit it resolved to (provenance).
pub fn pinned_git_url(loc: &GitLocation, commit: &str) -> String {
    format!("git+{}#{commit}{}", loc.repo, if loc.path.is_empty() { String::new() } else { format!(":{}", loc.path) })
}

// ---------------------------------------------------------------- pkt-line

fn pkt(line: &str) -> Vec<u8> {
    let mut out = format!("{:04x}", line.len() + 4).into_bytes();
    out.extend_from_slice(line.as_bytes());
    out
}

const FLUSH: &[u8] = b"0000";
const DELIM: &[u8] = b"0001";

enum Packet<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

fn git_error(repo: &str, why: &str, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(E_GIT, format!("{}: {why}", redact_url(repo)), span.cloned())
}

struct Packets<'a> {
    bytes: &'a [u8],
    i: usize,
    repo: &'a str,
    span: Option<&'a SourceSpan>,
}

fn packets<'a>(bytes: &'a [u8], repo: &'a str, span: Option<&'a SourceSpan>) -> Packets<'a> {
    Packets { bytes, i: 0, repo, span }
}

impl<'a> Iterator for Packets<'a> {
    type Item = Result<Packet<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.bytes.len() {
            return None;
        }
        let i = self.i;
        let fail = |why: &str| {
            Some(Err(git_error(self.repo, why, self.span)))
        };
        if i + 4 > self.bytes.len() {
            self.i = self.bytes.len();
            return fail("truncated pkt-line");
        }
        let head = &self.bytes[i..i + 4];
        if !head.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            self.i = self.bytes.len();
            return fail("malformed pkt-line (not a Git smart HTTP response?)");
        }
        let n = usize::from_str_radix(std::str::from_utf8(head).unwrap_or("0"), 16).unwrap_or(0);
        let item = match n {
            0 => Packet::Flush,
            1 => Packet::Delim,
            2 => Packet::End,
            3 => {
                self.i = self.bytes.len();
                return fail("malformed pkt-line");
            }
            _ => {
                if i + n > self.bytes.len() {
                    self.i = self.bytes.len();
                    return fail("truncated pkt-line");
                }
                Packet::Data(&self.bytes[i + 4..i + n])
            }
        };
        self.i += if n < 4 { 4 } else { n };
        Some(Ok(item))
    }
}

/// UTF-16 code unit count prefix of `s` (JS `slice(0, n)` on a string without split surrogates).
fn js_slice(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// Printable text of a server message (control characters dropped, at most 200 characters).
fn server_text(data: &[u8]) -> String {
    let s = String::from_utf8_lossy(data);
    let mut out = String::new();
    let mut in_run = false;
    for c in s.chars() {
        if (c as u32) < 0x20 || c == '\x7f' {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    js_slice(out.trim(), 200)
}

fn line_of(data: &[u8]) -> String {
    let s = String::from_utf8_lossy(data).into_owned();
    match s.strip_suffix('\n') {
        Some(t) => t.to_string(),
        None => s,
    }
}

// ---------------------------------------------------------------- protocol

/// What a fetch needs: the build's network policy and extra headers for the repository's origin.
#[derive(Clone, Copy)]
pub struct GitTransport<'a> {
    pub policy: &'a FetchPolicy,
    /// Sent only to the repository's origin (IMPORT … SECRET).
    pub origin_headers: &'a [(String, String)],
    pub span: Option<&'a SourceSpan>,
}

impl<'a> GitTransport<'a> {
    pub fn new(policy: &'a FetchPolicy, span: Option<&'a SourceSpan>) -> GitTransport<'a> {
        GitTransport { policy, origin_headers: &[], span }
    }
}

struct Remote {
    base: String,
    shallow: bool,
}

const ADVERTISEMENT: &str = "application/x-git-upload-pack-advertisement";
const RESULT: &str = "application/x-git-upload-pack-result";

fn headers(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn connect(repo: &str, t: GitTransport<'_>) -> Result<Remote> {
    let trimmed = repo.trim_end_matches('/');
    let url = format!("{trimmed}/info/refs?service=git-upload-pack");
    let r = fetch_remote_with(
        &url,
        t.policy,
        t.span,
        &FetchRequest {
            headers: headers(&[("git-protocol", "version=2"), ("accept", ADVERTISEMENT), ("user-agent", USER_AGENT)]),
            origin_headers: t.origin_headers.to_vec(),
            ..FetchRequest::default()
        },
    )?;
    if r.media_type.as_deref() != Some(ADVERTISEMENT) {
        return Err(git_error(
            repo,
            &format!("not a Git smart HTTP server (content-type {}; Git protocol v2 is required)", r.media_type.as_deref().unwrap_or("missing")),
            t.span,
        ));
    }
    let suffix = "/info/refs?service=git-upload-pack";
    let Some(base) = r.final_url.strip_suffix(suffix) else {
        return Err(git_error(repo, &format!("redirected to {}, which is not a Git repository URL", redact_url(&r.final_url)), t.span));
    };
    let mut v2 = false;
    let mut fetch: Option<String> = None;
    let mut ls_refs = false;
    let mut format = "sha1".to_string();
    for p in packets(&r.bytes, repo, t.span) {
        let Packet::Data(d) = p? else { continue };
        let line = line_of(d);
        if line == "version 2" {
            v2 = true;
        } else if line == "ls-refs" || line.starts_with("ls-refs=") {
            ls_refs = true;
        } else if line == "fetch" || line.starts_with("fetch=") {
            fetch = Some(line.get("fetch=".len()..).unwrap_or("").to_string());
        } else if let Some(f) = line.strip_prefix("object-format=") {
            format = f.to_string();
        }
    }
    let Some(fetch) = fetch.filter(|_| v2 && ls_refs) else {
        return Err(git_error(repo, "the server does not speak Git protocol v2 (ls-refs and fetch are required)", t.span));
    };
    if format != "sha1" {
        return Err(git_error(repo, &format!("object format '{format}' is not supported (only sha1 repositories)"), t.span));
    }
    Ok(Remote { base: base.to_string(), shallow: fetch.split(' ').any(|f| f == "shallow") })
}

fn command(remote: &Remote, repo: &str, body: Vec<u8>, t: GitTransport<'_>) -> Result<Vec<u8>> {
    let r = fetch_remote_with(
        &format!("{}/git-upload-pack", remote.base),
        t.policy,
        t.span,
        &FetchRequest {
            method: Some("POST"),
            body: Some(body),
            headers: headers(&[
                ("git-protocol", "version=2"),
                ("content-type", "application/x-git-upload-pack-request"),
                ("accept", RESULT),
                ("user-agent", USER_AGENT),
            ]),
            origin_headers: t.origin_headers.to_vec(),
        },
    )?;
    if r.media_type.as_deref() != Some(RESULT) {
        return Err(git_error(repo, &format!("unexpected content-type {} from git-upload-pack", r.media_type.as_deref().unwrap_or("missing")), t.span));
    }
    Ok(r.bytes)
}

/// The commit `ref` names on the server ('' = remote HEAD; `refs/…` exact; else tags then heads).
pub fn resolve_ref(repo: &str, r: &str, t: GitTransport<'_>) -> Result<String> {
    let remote = connect(repo, t)?;
    ls_ref(&remote, repo, r, t)
}

fn ls_ref(remote: &Remote, repo: &str, r: &str, t: GitTransport<'_>) -> Result<String> {
    let candidates: Vec<String> = if r.is_empty() {
        vec!["HEAD".into()]
    } else if r.starts_with("refs/") {
        vec![r.to_string()]
    } else {
        vec![format!("refs/tags/{r}"), format!("refs/heads/{r}")]
    };
    let mut body = Vec::new();
    body.extend(pkt("command=ls-refs\n"));
    body.extend(pkt(&format!("agent={USER_AGENT}\n")));
    body.extend_from_slice(DELIM);
    body.extend(pkt("peel\n"));
    body.extend(pkt("symrefs\n"));
    for c in &candidates {
        body.extend(pkt(&format!("ref-prefix {c}\n")));
    }
    body.extend_from_slice(FLUSH);
    let bytes = command(remote, repo, body, t)?;
    let mut found: HashMap<String, String> = HashMap::new();
    for p in packets(&bytes, repo, t.span) {
        let d = match p? {
            Packet::Flush => break,
            Packet::Data(d) => d,
            _ => continue,
        };
        let line = line_of(d);
        if line.starts_with("ERR ") {
            return Err(git_error(repo, &format!("remote error: {}", server_text(&d[4.min(d.len())..])), t.span));
        }
        let mut parts = line.split(' ');
        let oid = parts.next();
        let name = parts.next();
        let (Some(oid), Some(name)) = (oid, name) else {
            return Err(git_error(repo, "malformed ls-refs response", t.span));
        };
        if !is_commit(oid) {
            return Err(git_error(repo, "malformed ls-refs response", t.span));
        }
        let peeled = parts.find_map(|a| a.strip_prefix("peeled:"));
        let v = match peeled {
            Some(p) if is_commit(p) => p,
            _ => oid,
        };
        found.insert(name.to_string(), v.to_string());
    }
    for c in &candidates {
        if let Some(oid) = found.get(c) {
            return Ok(oid.clone());
        }
    }
    let msg = if r.is_empty() { format!("{} has no HEAD", redact_url(repo)) } else { format!("Git ref '{r}' not found in {}", redact_url(repo)) };
    Err(SitefileError::new(E_NOT_FOUND, msg, t.span.cloned()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjType {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjType::Commit => "commit",
            ObjType::Tree => "tree",
            ObjType::Blob => "blob",
            ObjType::Tag => "tag",
        }
    }
    fn from_code(c: u8) -> Option<ObjType> {
        match c {
            1 => Some(ObjType::Commit),
            2 => Some(ObjType::Tree),
            3 => Some(ObjType::Blob),
            4 => Some(ObjType::Tag),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitObject {
    pub kind: ObjType,
    pub data: Arc<[u8]>,
}

/// A fetched commit: its objects, by hex id.
#[derive(Debug, Clone)]
pub struct GitObjects {
    pub commit: String,
    pub objects: HashMap<String, GitObject>,
    pub pack_bytes: usize,
}

/// Fetch `commit` (when given) or the commit `ref` names now: one shallow fetch of that commit.
pub fn fetch_git_commit(repo: &str, r: &str, commit: Option<&str>, t: GitTransport<'_>, limits: &ArchiveLimits) -> Result<GitObjects> {
    let remote = connect(repo, t)?;
    let want = match commit {
        Some(c) => c.to_string(),
        None => ls_ref(&remote, repo, r, t)?,
    };
    let mut body = Vec::new();
    body.extend(pkt("command=fetch\n"));
    body.extend(pkt(&format!("agent={USER_AGENT}\n")));
    body.extend_from_slice(DELIM);
    body.extend(pkt(&format!("want {want}\n")));
    if remote.shallow {
        body.extend(pkt("deepen 1\n"));
    }
    body.extend(pkt("no-progress\n"));
    body.extend(pkt("ofs-delta\n"));
    body.extend(pkt("done\n"));
    body.extend_from_slice(FLUSH);
    let bytes = command(&remote, repo, body, t)?;
    let pack = packfile_of(&bytes, repo, t.span)?;
    let objects = parse_pack(&pack, repo, limits, t.span)?;
    let Some(c) = objects.get(&want) else {
        return Err(git_error(repo, &format!("the server did not send commit {want}"), t.span));
    };
    if c.kind != ObjType::Commit {
        return Err(git_error(repo, &format!("{want} is a {}, not a commit", c.kind.as_str()), t.span));
    }
    Ok(GitObjects { commit: want, objects, pack_bytes: pack.len() })
}

fn packfile_of(bytes: &[u8], repo: &str, span: Option<&SourceSpan>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut header = true;
    for p in packets(bytes, repo, span) {
        let d = match p? {
            Packet::Flush | Packet::End => {
                if section == "packfile" {
                    break;
                }
                header = true;
                continue;
            }
            Packet::Delim => {
                header = true;
                continue;
            }
            Packet::Data(d) => d,
        };
        if header {
            let line = line_of(d);
            if line.starts_with("ERR ") {
                return Err(git_error(repo, &format!("remote error: {}", server_text(&d[4.min(d.len())..])), span));
            }
            section = line;
            header = false;
            if !matches!(section.as_str(), "packfile" | "shallow-info" | "acknowledgments" | "wanted-refs") {
                return Err(git_error(repo, &format!("unexpected fetch response section '{}'", server_text(d)), span));
            }
            continue;
        }
        if section != "packfile" {
            continue;
        }
        match d.first() {
            Some(1) => out.extend_from_slice(&d[1..]),
            Some(3) => return Err(git_error(repo, &format!("remote error: {}", server_text(&d[1..])), span)),
            Some(2) => {}
            _ => return Err(git_error(repo, "malformed side-band packet", span)),
        }
    }
    if section != "packfile" {
        return Err(git_error(repo, "the fetch response has no packfile", span));
    }
    Ok(out)
}

// ---------------------------------------------------------------- packfile

/// The SHA-1 object id of `data` as a loose object of `kind`.
pub fn git_object_id(kind: ObjType, data: &[u8]) -> String {
    sha1_hex(&[format!("{} {}\0", kind.as_str(), data.len()).as_bytes(), data])
}

struct RawEntry {
    offset: usize,
    code: u8,
    data: Arc<[u8]>,
    base_offset: Option<usize>,
    base_id: Option<String>,
}

/// Inflate one zlib stream at the start of `input` to at most max(1, size) bytes.
/// Returns (output, bytes of input consumed) or the zlib error text.
fn inflate_object(input: &[u8], size: usize) -> std::result::Result<(Vec<u8>, usize), String> {
    use flate2::{Decompress, FlushDecompress, Status};
    let cap = size.max(1);
    let mut d = Decompress::new(true);
    let mut out: Vec<u8> = Vec::with_capacity(cap);
    loop {
        let before_in = d.total_in();
        let before_out = d.total_out();
        let status = d
            .decompress_vec(&input[d.total_in() as usize..], &mut out, FlushDecompress::Finish)
            .map_err(|e| e.message().unwrap_or("invalid compressed data").to_string())?;
        if status == Status::StreamEnd {
            return Ok((out, d.total_in() as usize));
        }
        if out.len() >= cap {
            // More output than allowed: is there any? Probe one byte.
            let mut probe = Vec::with_capacity(1);
            let s2 = d.decompress_vec(&input[d.total_in() as usize..], &mut probe, FlushDecompress::Finish).map_err(|e| e.message().unwrap_or("invalid compressed data").to_string())?;
            if s2 == Status::StreamEnd && probe.is_empty() {
                return Ok((out, d.total_in() as usize));
            }
            return Err("output exceeds the declared size".into());
        }
        if d.total_in() == before_in && d.total_out() == before_out {
            return Err("unexpected end of file".into());
        }
    }
}

/// Parse a version 2 or 3 packfile into objects by id (see the TypeScript `parsePack`).
pub fn parse_pack(pack: &[u8], repo: &str, limits: &ArchiveLimits, span: Option<&SourceSpan>) -> Result<HashMap<String, GitObject>> {
    let bad = |why: &str| git_error(repo, &format!("malformed packfile: {why}"), span);
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err(bad("missing PACK header"));
    }
    let be32 = |i: usize| u32::from_be_bytes([pack[i], pack[i + 1], pack[i + 2], pack[i + 3]]);
    let version = be32(4);
    if version != 2 && version != 3 {
        return Err(bad(&format!("unsupported version {version}")));
    }
    let count = be32(8) as usize;
    let end = pack.len() - 20;
    let mut h = Sha1::new();
    h.update(&pack[..end]);
    if hex(&h.finish()) != hex(&pack[end..]) {
        return Err(bad("checksum mismatch"));
    }
    if count > end {
        return Err(bad("object count exceeds the pack size"));
    }
    let mut total: u64 = 0;
    let limit_error = || SitefileError::new(E_ARCHIVE_LIMIT, format!("{}: Git objects expand beyond {} bytes", redact_url(repo), limits.max_total_bytes), span.cloned());
    let mut charge = |n: u64| -> Result<()> {
        total += n;
        if total > limits.max_total_bytes {
            return Err(limit_error());
        }
        Ok(())
    };
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut pos = 12usize;
    for _ in 0..count {
        let offset = pos;
        if pos >= end {
            return Err(bad("truncated"));
        }
        let mut c = pack[pos];
        pos += 1;
        let code = (c >> 4) & 7;
        let mut size: u64 = (c & 15) as u64;
        let mut shift = 4u32;
        while c & 0x80 != 0 {
            if pos >= end || shift > 49 {
                return Err(bad("bad object header"));
            }
            c = pack[pos];
            pos += 1;
            size += ((c & 0x7f) as u64) << shift;
            shift += 7;
        }
        let mut base_offset = None;
        let mut base_id = None;
        if code == 6 {
            if pos >= end {
                return Err(bad("truncated"));
            }
            c = pack[pos];
            pos += 1;
            let mut n: u64 = (c & 0x7f) as u64;
            while c & 0x80 != 0 {
                if pos >= end || n > (1u64 << 45) {
                    return Err(bad("bad delta offset"));
                }
                c = pack[pos];
                pos += 1;
                n = (n + 1) * 128 + (c & 0x7f) as u64;
            }
            let b = offset as i64 - n as i64;
            if b < 12 || n == 0 {
                return Err(bad("delta base offset out of range"));
            }
            base_offset = Some(b as usize);
        } else if code == 7 {
            if pos + 20 > end {
                return Err(bad("truncated"));
            }
            base_id = Some(hex(&pack[pos..pos + 20]));
            pos += 20;
        } else if !(1..=4).contains(&code) {
            return Err(bad(&format!("unknown object type {code}")));
        }
        charge(size)?;
        let (data, used) = inflate_object(&pack[pos..end], size as usize)
            .map_err(|e| bad(&format!("object at offset {offset} does not inflate ({e})")))?;
        if data.len() as u64 != size {
            return Err(bad(&format!("object at offset {offset} has the wrong size")));
        }
        pos += used;
        entries.push(RawEntry { offset, code, data: data.into(), base_offset, base_id });
    }
    if pos != end {
        return Err(bad("trailing data after the last object"));
    }

    let by_offset: HashMap<usize, usize> = entries.iter().enumerate().map(|(i, e)| (e.offset, i)).collect();
    let mut resolved: HashMap<usize, GitObject> = HashMap::new();
    let mut objects: HashMap<String, GitObject> = HashMap::new();
    let mut offset_of: HashMap<String, usize> = HashMap::new();
    for e in &entries {
        if let Some(kind) = ObjType::from_code(e.code) {
            let o = GitObject { kind, data: e.data.clone() };
            resolved.insert(e.offset, o.clone());
            let id = git_object_id(kind, &o.data);
            objects.insert(id.clone(), o);
            offset_of.insert(id, e.offset);
        }
    }
    let mut progress = true;
    while progress {
        progress = false;
        for ei in 0..entries.len() {
            if resolved.contains_key(&entries[ei].offset) {
                continue;
            }
            let mut chain: Vec<usize> = vec![ei];
            let mut base: Option<GitObject> = None;
            loop {
                let top = &entries[*chain.last().expect("non-empty")];
                let b_off = top.base_offset.or_else(|| top.base_id.as_ref().and_then(|id| offset_of.get(id).copied()));
                let Some(b_off) = b_off else { break };
                if let Some(b) = resolved.get(&b_off) {
                    base = Some(b.clone());
                    break;
                }
                let Some(&next) = by_offset.get(&b_off) else {
                    return Err(bad("delta base is not an object in the pack"));
                };
                if chain.len() > count {
                    return Err(bad("delta cycle"));
                }
                chain.push(next);
            }
            let Some(mut base) = base else { continue };
            for &di in chain.iter().rev() {
                let d = &entries[di];
                let data = apply_delta(&base.data, &d.data, limits, &bad, &limit_error)?;
                charge(data.len() as u64)?;
                base = GitObject { kind: base.kind, data: data.into() };
                resolved.insert(d.offset, base.clone());
                let id = git_object_id(base.kind, &base.data);
                objects.insert(id.clone(), base.clone());
                offset_of.insert(id, d.offset);
            }
            progress = true;
        }
    }
    if resolved.len() != entries.len() {
        return Err(bad("delta base missing from the pack"));
    }
    Ok(objects)
}

fn apply_delta(
    base: &[u8],
    delta: &[u8],
    limits: &ArchiveLimits,
    bad: &dyn Fn(&str) -> SitefileError,
    limit_error: &dyn Fn() -> SitefileError,
) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let varint = |pos: &mut usize| -> Result<u64> {
        let mut n: u64 = 0;
        let mut shift = 0u32;
        loop {
            if *pos >= delta.len() || shift > 49 {
                return Err(bad("truncated delta"));
            }
            let c = delta[*pos];
            *pos += 1;
            n += ((c & 0x7f) as u64) << shift;
            shift += 7;
            if c & 0x80 == 0 {
                return Ok(n);
            }
        }
    };
    if varint(&mut pos)? != base.len() as u64 {
        return Err(bad("delta base size mismatch"));
    }
    let size = varint(&mut pos)?;
    // Allocating more than the limit would fail the charge that follows anyway.
    if size > limits.max_total_bytes {
        return Err(limit_error());
    }
    let size = size as usize;
    let mut out = vec![0u8; size];
    let mut o = 0usize;
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut off: usize = 0;
            let mut len: usize = 0;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off += (*delta.get(pos).unwrap_or(&0) as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len += (*delta.get(pos).unwrap_or(&0) as usize) << (8 * i);
                    pos += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            if pos > delta.len() || off + len > base.len() || o + len > size {
                return Err(bad("delta copy out of range"));
            }
            out[o..o + len].copy_from_slice(&base[off..off + len]);
            o += len;
        } else if op != 0 {
            let op = op as usize;
            if pos + op > delta.len() || o + op > size {
                return Err(bad("delta insert out of range"));
            }
            out[o..o + op].copy_from_slice(&delta[pos..pos + op]);
            pos += op;
            o += op;
        } else {
            return Err(bad("reserved delta opcode 0"));
        }
    }
    if o != size {
        return Err(bad("delta result size mismatch"));
    }
    Ok(out)
}

// ---------------------------------------------------------------- trees

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Tree,
    Blob,
    Link,
    Gitlink,
}

#[derive(Debug, Clone)]
struct TreeEntry {
    name: String,
    kind: EntryKind,
    exec: bool,
    id: String,
}

/// Selected content of a commit: a tree's files, or one file.
#[derive(Debug, Clone)]
pub enum GitSelection {
    /// (path `/a/b`, bytes, mode), in walk order.
    Tree(Vec<(String, Arc<[u8]>, u32)>),
    File { bytes: Arc<[u8]>, mode: u32, basename: String },
}

impl GitSelection {
    pub fn kind(&self) -> &'static str {
        match self {
            GitSelection::Tree(_) => "tree",
            GitSelection::File { .. } => "file",
        }
    }
}

struct Repo<'a> {
    objs: &'a GitObjects,
    repo: &'a str,
    span: Option<&'a SourceSpan>,
    cache: std::cell::RefCell<HashMap<String, Arc<Vec<TreeEntry>>>>,
}

impl Repo<'_> {
    fn object(&self, id: &str, kind: ObjType) -> Result<Arc<[u8]>> {
        let Some(o) = self.objs.objects.get(id) else {
            return Err(git_error(self.repo, &format!("object {id} is missing from the pack"), self.span));
        };
        if o.kind != kind {
            return Err(git_error(self.repo, &format!("object {id} is a {}, expected a {}", o.kind.as_str(), kind.as_str()), self.span));
        }
        Ok(o.data.clone())
    }

    fn root_tree(&self) -> Result<String> {
        let data = self.object(&self.objs.commit, ObjType::Commit)?;
        let text = String::from_utf8_lossy(&data);
        let id = text.strip_prefix("tree ").and_then(|r| r.get(..41)).filter(|s| s.ends_with('\n') && is_commit(&s[..40]));
        match id {
            Some(s) => Ok(s[..40].to_string()),
            None => Err(git_error(self.repo, &format!("commit {} has no tree", self.objs.commit), self.span)),
        }
    }

    fn entries(&self, id: &str, wher: &str) -> Result<Arc<Vec<TreeEntry>>> {
        if let Some(hit) = self.cache.borrow().get(id) {
            return Ok(hit.clone());
        }
        let data = self.object(id, ObjType::Tree)?;
        let mut out = Vec::new();
        let mut i = 0usize;
        let malformed = || git_error(self.repo, &format!("malformed tree {id}"), self.span);
        while i < data.len() {
            let sp = data[i..].iter().position(|&b| b == 0x20).map(|p| p + i);
            let nul = sp.and_then(|sp| data[sp..].iter().position(|&b| b == 0).map(|p| p + sp));
            let (Some(sp), Some(nul)) = (sp, nul) else { return Err(malformed()) };
            if nul + 21 > data.len() {
                return Err(malformed());
            }
            let mode = String::from_utf8_lossy(&data[i..sp]).into_owned();
            let Ok(name) = std::str::from_utf8(&data[sp + 1..nul]) else {
                return Err(SitefileError::new(
                    E_ENCODING,
                    format!("{}: a file name in '{}' is not valid UTF-8", redact_url(self.repo), if wher.is_empty() { "/" } else { wher }),
                    self.span.cloned(),
                ));
            };
            let shown = if wher.is_empty() { name.to_string() } else { format!("{wher}/{name}") };
            if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') || name.to_lowercase() == ".git" {
                return Err(SitefileError::new(E_ARCHIVE_UNSAFE, format!("{}: unsafe tree entry '{shown}'", redact_url(self.repo)), self.span.cloned()));
            }
            let kind = match mode.as_str() {
                "40000" => EntryKind::Tree,
                "100644" | "100755" | "100664" => EntryKind::Blob,
                "120000" => EntryKind::Link,
                "160000" => EntryKind::Gitlink,
                _ => return Err(git_error(self.repo, &format!("tree entry '{shown}' has unknown mode {mode}"), self.span)),
            };
            out.push(TreeEntry { name: name.to_string(), kind, exec: mode == "100755", id: hex(&data[nul + 1..nul + 21]) });
            i = nul + 21;
        }
        let out = Arc::new(out);
        self.cache.borrow_mut().insert(id.to_string(), out.clone());
        Ok(out)
    }

    fn child(&self, id: &str, wher: &str, seg: &str) -> Result<Option<TreeEntry>> {
        let want = nfc(seg).into_owned();
        let entries = self.entries(id, wher)?;
        let matches: Vec<&TreeEntry> = entries.iter().filter(|e| nfc(&e.name) == want.as_str()).collect();
        if matches.len() > 1 {
            return Err(collision(&if wher.is_empty() { want.clone() } else { format!("{wher}/{want}") }, self.repo, self.span));
        }
        Ok(matches.first().map(|e| (*e).clone()))
    }

    fn link_target(&self, id: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.object(id, ObjType::Blob)?).into_owned())
    }
}

fn collision(path: &str, repo: &str, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(
        E_PATH_COLLISION,
        format!("{}: two names in the repository have the same Unicode NFC form '{path}'; rename one", redact_url(repo)),
        span.cloned(),
    )
}

fn submodule(path: &str, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(
        E_UNSUPPORTED_FILE,
        format!("'{path}' is a Git submodule; submodules are not supported (import that repository separately)"),
        span.cloned(),
    )
}

#[derive(Clone)]
struct Cur {
    kind: EntryKind, // Tree or Blob
    id: String,
    exec: bool,
}

struct Resolved {
    cur: Cur,
    real: Vec<String>,
}

const MAX_LINKS: u32 = 40;

#[allow(clippy::too_many_arguments)]
fn resolve_path(r: &Repo<'_>, root: &str, segs: &[String], boundary: &[String], shown: &str, link: bool, repo: &str, span: Option<&SourceSpan>) -> Result<Resolved> {
    let escape = || {
        SitefileError::new(
            E_PATH_ESCAPE,
            if link { format!("symlink '{shown}' points outside the imported tree") } else { format!("'{shown}' resolves outside the repository") },
            span.cloned(),
        )
    };
    let missing = || {
        SitefileError::new(
            E_NOT_FOUND,
            if link { format!("symlink '{shown}' is dangling") } else { format!("'{shown}' does not exist in {}", redact_url(repo)) },
            span.cloned(),
        )
    };
    let mut pending: VecDeque<String> = segs.iter().cloned().collect();
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut cur = Cur { kind: EntryKind::Tree, id: root.to_string(), exec: false };
    let mut hops = 0;
    while let Some(seg) = pending.pop_front() {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if cur.kind != EntryKind::Tree {
                return Err(missing());
            }
            if stack.is_empty() {
                return Err(escape());
            }
            stack.pop();
            cur = Cur { kind: EntryKind::Tree, id: stack.last().map(|s| s.1.clone()).unwrap_or_else(|| root.to_string()), exec: false };
            continue;
        }
        if cur.kind != EntryKind::Tree {
            return Err(missing());
        }
        let wher = stack.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join("/");
        let Some(e) = r.child(&cur.id, &wher, &seg)? else { return Err(missing()) };
        if e.kind == EntryKind::Gitlink {
            return Err(submodule(&if wher.is_empty() { e.name.clone() } else { format!("{wher}/{}", e.name) }, span));
        }
        if e.kind == EntryKind::Link {
            hops += 1;
            if hops > MAX_LINKS {
                return Err(SitefileError::new(E_UNSUPPORTED_FILE, format!("'{shown}' is a symlink loop"), span.cloned()));
            }
            let target = r.link_target(&e.id)?;
            if target.starts_with('/') || target.is_empty() {
                return Err(escape());
            }
            for s in target.split('/').rev() {
                pending.push_front(s.to_string());
            }
            continue;
        }
        stack.push((nfc(&e.name).into_owned(), e.id.clone()));
        cur = Cur { kind: e.kind, id: e.id, exec: e.exec };
    }
    let real: Vec<String> = stack.into_iter().map(|s| s.0).collect();
    if boundary.iter().enumerate().any(|(i, b)| real.get(i) != Some(b)) || real.len() < boundary.len() {
        return Err(escape());
    }
    Ok(Resolved { cur, real })
}

fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// The content of `path` in a fetched commit (see the TypeScript `selectGitPath`).
pub fn select_git_path(objs: &GitObjects, repo: &str, path: &str, limits: &ArchiveLimits, span: Option<&SourceSpan>) -> Result<GitSelection> {
    let r = Repo { objs, repo, span, cache: Default::default() };
    let root = r.root_tree()?;
    let segs: Vec<String> = if path.is_empty() { Vec::new() } else { path.split('/').map(str::to_string).collect() };
    let top = resolve_path(&r, &root, &segs, &[], path, false, repo, span)?;
    if top.cur.kind == EntryKind::Blob {
        let bytes = r.object(&top.cur.id, ObjType::Blob)?;
        return Ok(GitSelection::File { bytes, mode: if top.cur.exec { 0o755 } else { 0o644 }, basename: segs.last().cloned().unwrap_or_default() });
    }
    let mut files: Vec<(String, Arc<[u8]>, u32)> = Vec::new();
    let mut total: u64 = 0;
    let mut ancestors: BTreeSet<String> = BTreeSet::new();
    ancestors.insert(top.real.join("/"));
    struct Walk<'w, 'a> {
        r: &'w Repo<'a>,
        root: &'w str,
        top_real: &'w [String],
        repo: &'w str,
        span: Option<&'w SourceSpan>,
        limits: &'w ArchiveLimits,
    }
    fn walk(w: &Walk<'_, '_>, id: &str, real: &[String], logical: &str, ancestors: &mut BTreeSet<String>, files: &mut Vec<(String, Arc<[u8]>, u32)>, total: &mut u64) -> Result<()> {
        let wher = real.join("/");
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut entries: Vec<TreeEntry> = w.r.entries(id, &wher)?.as_ref().clone();
        entries.sort_by(|a, b| cmp_utf16(&a.name, &b.name));
        for e in entries {
            let name = nfc(&e.name).into_owned();
            let shown_logical = if logical.is_empty() { name.clone() } else { format!("{logical}/{name}") };
            if !seen.insert(name.clone()) {
                return Err(collision(&shown_logical, w.repo, w.span));
            }
            if e.kind == EntryKind::Gitlink {
                return Err(submodule(&if wher.is_empty() { name } else { format!("{wher}/{name}") }, w.span));
            }
            let (kind, entry_id, exec, child_real) = if e.kind == EntryKind::Link {
                let shown = if wher.is_empty() { name.clone() } else { format!("{wher}/{name}") };
                let mut segs: Vec<String> = real.to_vec();
                segs.push(name.clone());
                let res = resolve_path(w.r, w.root, &segs, w.top_real, &shown, true, w.repo, w.span)?;
                (res.cur.kind, res.cur.id, res.cur.exec, res.real)
            } else {
                let mut cr = real.to_vec();
                cr.push(name.clone());
                (e.kind, e.id.clone(), e.exec, cr)
            };
            if kind == EntryKind::Blob {
                let bytes = w.r.object(&entry_id, ObjType::Blob)?;
                if files.len() as u64 + 1 > w.limits.max_entries {
                    return Err(SitefileError::new(E_ARCHIVE_LIMIT, format!("{}: more than {} files", redact_url(w.repo), w.limits.max_entries), w.span.cloned()));
                }
                *total += bytes.len() as u64;
                if *total > w.limits.max_total_bytes {
                    return Err(SitefileError::new(E_ARCHIVE_LIMIT, format!("{}: files exceed {} bytes", redact_url(w.repo), w.limits.max_total_bytes), w.span.cloned()));
                }
                files.push((format!("/{shown_logical}"), bytes, if exec { 0o755 } else { 0o644 }));
            } else {
                let key = child_real.join("/");
                if ancestors.contains(&key) {
                    continue; // directory symlink cycle
                }
                ancestors.insert(key.clone());
                let res = walk(w, &entry_id, &child_real, &shown_logical, ancestors, files, total);
                ancestors.remove(&key);
                res?;
            }
        }
        Ok(())
    }
    let w = Walk { r: &r, root: &root, top_real: &top.real, repo, span, limits };
    walk(&w, &top.cur.id, &top.real, "", &mut ancestors, &mut files, &mut total)?;
    Ok(GitSelection::Tree(files))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::resources::types::DEFAULT_ARCHIVE_LIMITS;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    fn zlib(d: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(d).unwrap();
        e.finish().unwrap()
    }

    fn header(code: u8, size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = (code << 4) | (size & 15) as u8;
        let mut s = size >> 4;
        while s > 0 {
            out.push(c | 0x80);
            c = (s & 0x7f) as u8;
            s >>= 7;
        }
        out.push(c);
        out
    }

    fn varint(mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut c = (n & 0x7f) as u8;
            n >>= 7;
            if n > 0 {
                c |= 0x80;
            }
            out.push(c);
            if n == 0 {
                return out;
            }
        }
    }

    /// copy base[0..base.len()] then insert `tail`.
    fn delta(base: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut d = varint(base.len());
        d.extend(varint(base.len() + tail.len()));
        // copy offset 0, size base.len() (< 65536)
        let len = base.len();
        d.push(0x80 | 0x10 | 0x20);
        d.push((len & 0xff) as u8);
        d.push(((len >> 8) & 0xff) as u8);
        d.push(tail.len() as u8);
        d.extend_from_slice(tail);
        d
    }

    /// Objects in order; each is (kind code, data) or a delta: (6, base index, tail) / (7, base index, tail).
    pub(crate) enum PackObj {
        Whole(ObjType, Vec<u8>),
        Ofs(usize, Vec<u8>),
        Ref(usize, Vec<u8>),
    }

    pub(crate) fn write_pack(objs: &[PackObj]) -> Vec<u8> {
        let mut out = b"PACK".to_vec();
        out.extend(2u32.to_be_bytes());
        out.extend((objs.len() as u32).to_be_bytes());
        let mut offsets = Vec::new();
        let mut datas: Vec<(ObjType, Vec<u8>)> = Vec::new();
        for o in objs {
            let off = out.len();
            offsets.push(off);
            match o {
                PackObj::Whole(k, d) => {
                    let code = match k {
                        ObjType::Commit => 1,
                        ObjType::Tree => 2,
                        ObjType::Blob => 3,
                        ObjType::Tag => 4,
                    };
                    out.extend(header(code, d.len()));
                    out.extend(zlib(d));
                    datas.push((*k, d.clone()));
                }
                PackObj::Ofs(b, tail) | PackObj::Ref(b, tail) => {
                    let (bk, bd) = datas[*b].clone();
                    let dl = delta(&bd, tail);
                    let is_ofs = matches!(o, PackObj::Ofs(..));
                    out.extend(header(if is_ofs { 6 } else { 7 }, dl.len()));
                    if is_ofs {
                        let rel = off - offsets[*b];
                        let mut enc = vec![(rel & 0x7f) as u8];
                        let mut r = rel >> 7;
                        while r > 0 {
                            r -= 1;
                            enc.insert(0, 0x80 | (r & 0x7f) as u8);
                            r >>= 7;
                        }
                        out.extend(enc);
                    } else {
                        out.extend(hex_to_bytes(&git_object_id(bk, &bd)));
                    }
                    out.extend(zlib(&dl));
                    let mut full = bd.clone();
                    full.extend_from_slice(tail);
                    datas.push((bk, full));
                }
            }
        }
        let mut h = Sha1::new();
        h.update(&out);
        let sum = h.finish();
        out.extend(sum);
        out
    }

    pub(crate) fn hex_to_bytes(h: &str) -> Vec<u8> {
        (0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    /// A tree object from (mode, name, id) entries (already in Git order).
    pub(crate) fn tree_obj(entries: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (mode, name, id) in entries {
            out.extend(format!("{mode} {name}\0").as_bytes());
            out.extend(hex_to_bytes(id));
        }
        out
    }

    fn code(r: Result<impl std::fmt::Debug>) -> String {
        match r {
            Ok(_) => "ok".into(),
            Err(e) => e.code.as_str().into(),
        }
    }

    #[test]
    fn parses_git_urls() {
        let l = parse_git_uri("git+https://example.com/x/y.git#v1.2:docs/api/", None).unwrap();
        assert_eq!(l.repo, "https://example.com/x/y.git");
        assert_eq!(l.git_ref, "v1.2");
        assert_eq!(l.path, "docs/api");
        assert_eq!(l.commit, None);
        assert_eq!(parse_git_uri("git+https://e.com/r", None).unwrap().git_ref, "");
        assert_eq!(parse_git_uri("git+https://e.com/r#:sub", None).unwrap().path, "sub");
        let c = "ab".repeat(20);
        assert_eq!(parse_git_uri(&format!("git+https://e.com/r#{c}"), None).unwrap().commit, Some(c));
        for (u, want) in [
            ("git+ssh://e.com/r", "E_PROTOCOL"),
            ("git+file:///tmp/r", "E_PROTOCOL"),
            ("git+https://u:p@e.com/r", "E_PROTOCOL"),
            ("git+https://e.com/r?x=1", "E_PROTOCOL"),
            ("git+https://e.com/r#a..b", "E_VALIDATION"),
            ("git+https://e.com/r#main:../x", "E_PATH_ESCAPE"),
        ] {
            assert_eq!(code(parse_git_uri(u, None)), want, "{u}");
        }
        assert!(is_git_uri("git+ssh:x") && !is_git_uri("https://x") && !is_git_uri("git:x"));
        for ok in ["main", "refs/heads/main", "feature/x-1", "v1.0"] {
            check_ref_name(ok, None).unwrap();
        }
        for no in ["-x", "a/", "a.lock", "a b", "a~1", ".hidden", "a//b", "@", "x@{1}"] {
            assert!(check_ref_name(no, None).is_err(), "{no}");
        }
    }

    #[test]
    fn parses_packs_with_ofs_and_ref_deltas() {
        let base = b"shared prefix line\n".repeat(100);
        let objs = vec![
            PackObj::Whole(ObjType::Blob, base.clone()),
            PackObj::Ofs(0, b"one\n".to_vec()),
            PackObj::Ref(1, b"two\n".to_vec()), // chained: a delta of a delta
            PackObj::Whole(ObjType::Blob, Vec::new()),
        ];
        let pack = write_pack(&objs);
        let got = parse_pack(&pack, "r", &DEFAULT_ARCHIVE_LIMITS, None).unwrap();
        assert_eq!(got.len(), 4);
        let mut want = base.clone();
        want.extend(b"one\ntwo\n");
        assert!(got.contains_key(&git_object_id(ObjType::Blob, &want)));
        assert!(got.contains_key("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"));
        let mut corrupt = pack.clone();
        corrupt[20] ^= 1;
        assert_eq!(code(parse_pack(&corrupt, "r", &DEFAULT_ARCHIVE_LIMITS, None)), "E_GIT");
        let small = ArchiveLimits { max_total_bytes: 1000, ..DEFAULT_ARCHIVE_LIMITS };
        assert_eq!(code(parse_pack(&pack, "r", &small, None)), "E_ARCHIVE_LIMIT");
    }

    fn objects_for(entries: Vec<(String, ObjType, Vec<u8>)>, commit: &str) -> GitObjects {
        GitObjects { commit: commit.into(), objects: entries.into_iter().map(|(id, k, d)| (id, GitObject { kind: k, data: d.into() })).collect(), pack_bytes: 0 }
    }

    #[test]
    fn trees_resolve_symlinks_and_refuse_submodules_escapes_and_collisions() {
        let mut objs: Vec<(String, ObjType, Vec<u8>)> = Vec::new();
        let mut blob = |d: &[u8]| {
            let id = git_object_id(ObjType::Blob, d);
            objs.push((id.clone(), ObjType::Blob, d.to_vec()));
            id
        };
        let a = blob(b"a");
        let link = blob(b"a.txt");
        let up = blob(b"..");
        let esc = blob(b"../../x");
        let sub_t = tree_obj(&[("120000", "up", &up)]);
        let sub_id = git_object_id(ObjType::Tree, &sub_t);
        let docs_t = tree_obj(&[("100644", "a.txt", &a), ("120000", "l.txt", &link), ("100755", "run", &a), ("40000", "sub", &sub_id)]);
        let docs_id = git_object_id(ObjType::Tree, &docs_t);
        let bad_t = tree_obj(&[("120000", "esc", &esc), ("160000", "mod", &"a".repeat(40)), ("100644", "\u{e9}", &a), ("100644", "e\u{301}", &a)]);
        let bad_id = git_object_id(ObjType::Tree, &bad_t);
        let root_t = tree_obj(&[("40000", "bad", &bad_id), ("40000", "docs", &docs_id)]);
        let root_id = git_object_id(ObjType::Tree, &root_t);
        let commit = format!("tree {root_id}\n\nm\n").into_bytes();
        let cid = git_object_id(ObjType::Commit, &commit);
        objs.push((sub_id, ObjType::Tree, sub_t));
        objs.push((docs_id, ObjType::Tree, docs_t));
        objs.push((bad_id, ObjType::Tree, bad_t));
        objs.push((root_id, ObjType::Tree, root_t));
        objs.push((cid.clone(), ObjType::Commit, commit));
        let o = objects_for(objs, &cid);
        let GitSelection::Tree(files) = select_git_path(&o, "r", "docs", &DEFAULT_ARCHIVE_LIMITS, None).unwrap() else { panic!() };
        let names: Vec<String> = files.iter().map(|f| format!("{} {:o}", f.0, f.2)).collect();
        assert_eq!(names, ["/a.txt 644", "/l.txt 644", "/run 755"]);
        let GitSelection::File { basename, .. } = select_git_path(&o, "r", "docs/l.txt", &DEFAULT_ARCHIVE_LIMITS, None).unwrap() else { panic!() };
        assert_eq!(basename, "l.txt");
        assert_eq!(code(select_git_path(&o, "r", "bad/esc", &DEFAULT_ARCHIVE_LIMITS, None)), "E_PATH_ESCAPE");
        assert_eq!(code(select_git_path(&o, "r", "bad/mod", &DEFAULT_ARCHIVE_LIMITS, None)), "E_UNSUPPORTED_FILE");
        assert_eq!(code(select_git_path(&o, "r", "bad/\u{e9}", &DEFAULT_ARCHIVE_LIMITS, None)), "E_PATH_COLLISION");
        assert_eq!(code(select_git_path(&o, "r", "nope", &DEFAULT_ARCHIVE_LIMITS, None)), "E_NOT_FOUND");
    }
}
