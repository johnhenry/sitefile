//! Detached site servers ("containers"), port of `src/servers.ts`. One state
//! file per server, in the same JSON format as TypeScript so `servers` lists
//! servers started by either CLI:
//!
//! ```text
//! $SITEFILE_HOME/servers/<id>.json   state (ServerState)
//! $SITEFILE_HOME/servers/<id>.log    stdout/stderr of the server process
//! ```
//!
//! A server is `running` only when its pid is alive AND its port answers with
//! this server's `x-sitefile-server` header.
//!
//! Embedders (a long-running host such as Sitefile Desktop) start, restart and
//! stop detached servers with [`serve_detached`], [`restart_detached`] and
//! [`stop_server`], the functions `serve -d`, `server restart` and
//! `server stop` use. The worker is the executable given as `worker_exe`
//! (default: the current one) run as `<exe> __serve-worker <id>`, so an
//! embedding binary must hand that argv to `sitefile::cli::main` first thing.
//!
//! **Reaping.** The worker is a child of the process that started it, which
//! waits for it on a background thread (`sitefile-reaper`). So a long-lived
//! host never keeps a stopped worker as a zombie, which `kill -0` (and so
//! [`pid_alive`]) would report as alive, making [`stop_server`] wait out its
//! grace period and fail. When the starting process exits first (the CLI),
//! the worker is re-parented and keeps running.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::exporter::random_hex;
use crate::export::oci::{
    blob_path_of, decode_index, media, parse_config, parse_manifest, site_layer_indices,
};
use crate::resources::archive::{
    extract_archive, open_zip_file, os_metadata_warning, without_os_metadata,
};
use crate::resources::local::{LocalContext, LocalContextOptions};
use crate::resources::types::{ArchiveLimits, Resource, DEFAULT_ARCHIVE_LIMITS};
use crate::server::SiteContent;
use crate::store::Store;
use crate::tree::Tree;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ServerSource {
    Image {
        #[serde(rename = "imageId")]
        image_id: String,
    },
    Dir {
        path: String,
    },
    /// An archive file: zip, tar, tar.gz/tgz, a Web Bundle (`.wbn`), or an OCI
    /// image archive (`.oci.tar`). Read again on every start, as a directory is.
    Archive {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    pub host: String,
    pub port: u16,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerState {
    pub id: String,
    pub name: String,
    /// What the user asked to serve: the image reference as typed, or the directory.
    pub image: String,
    pub source: ServerSource,
    pub spa: bool,
    /// `--spa-exclude`: request paths that never fall back (absent: none).
    #[serde(default, rename = "spaExclude", skip_serializing_if = "Vec::is_empty")]
    pub spa_exclude: Vec<String>,
    /// `--spa-fallback`: the file every fallback serves (absent: the nearest 200.html or index.html).
    #[serde(
        default,
        rename = "spaFallback",
        skip_serializing_if = "Option::is_none"
    )]
    pub spa_fallback: Option<String>,
    /// `--base`: the path prefix the site is served under, `/repo/` (absent: `/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// `--rewrite-base`: the production URL replaced by the local one in HTML, CSS and XML.
    #[serde(
        default,
        rename = "rewriteBase",
        skip_serializing_if = "Option::is_none"
    )]
    pub rewrite_base: Option<String>,
    /// `--host-profile`: netlify, cloudflare, vercel, github-pages or firebase.
    #[serde(
        default,
        rename = "hostProfile",
        skip_serializing_if = "Option::is_none"
    )]
    pub host_profile: Option<String>,
    /// `--html-handling`: auto, force-slash, drop-slash or none.
    #[serde(
        default,
        rename = "htmlHandling",
        skip_serializing_if = "Option::is_none"
    )]
    pub html_handling: Option<String>,
    /// `--keep-os-metadata`: serve a directory's or archive's OS metadata files too.
    #[serde(
        default,
        rename = "keepOsMetadata",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub keep_os_metadata: bool,
    /// `--allowed-host` names (localhost names and IP addresses are always allowed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// `--cors`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cors: bool,
    /// `--isolate`: the Cross-Origin-Embedder-Policy sent with COOP and CORP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate: Option<String>,
    /// `--max-age`, `--immutable`, `--max-age-html`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<crate::server::CachePreview>,
    /// `--log-format json` (`None`: text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_format: Option<String>,
    /// `--log-ip`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub log_ip: bool,
    /// `--quiet`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// `--auth`, `SITEFILE_SERVE_AUTH`: users and password verifiers (never passwords).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<crate::serve_auth::AuthEntry>,
    /// `--auth-file`: an htpasswd file, read at every start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_file: Option<String>,
    /// `--max-connections-per-ip` (0: no limit).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_connections_per_ip: usize,
    /// `--random-route`: `base` is a secret, so `/` is a plain 404.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_base: bool,
    /// `--allow-ip` blocks as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_ip: Option<Vec<String>>,
    /// `--deny-ip` blocks as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_ip: Option<Vec<String>>,
    /// `--stop-after`: when the server stops itself (ISO time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// `--country`: what `_redirects` `Country=` conditions match (absent: none matches).
    pub country: Option<String>,
    /// `--strict-site-files`: an ignored `_redirects`/`_headers` line stops the server from starting.
    #[serde(
        default,
        rename = "strictSiteFiles",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub strict_site_files: bool,
    /// The requested bind address (port 0 = pick a free port).
    pub bind: Bind,
    pub host: String,
    pub port: u16,
    pub pid: i64,
    pub started: String,
    pub log_file: String,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The servers file (`sitefile up`) that started it, by real path; None for `serve -d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerView {
    pub state: ServerState,
    /// `running` or `exited`.
    pub status: &'static str,
    pub url: String,
}

fn is_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$`
fn is_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

pub struct ServerRegistry {
    pub dir: PathBuf,
    pub store: Store,
}

impl ServerRegistry {
    pub fn open(store: Option<Store>) -> Result<ServerRegistry> {
        let store = match store {
            Some(s) => s,
            None => Store::open_default()?,
        };
        let dir = store.home.join("servers");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| {
                SitefileError::plain(E_SERVER, format!("cannot create '{}': {e}", dir.display()))
            })?;
        Ok(ServerRegistry { dir, store })
    }

    pub fn state_path(&self, id: &str) -> Result<PathBuf> {
        if !is_id(id) {
            return Err(SitefileError::plain(
                E_SERVER,
                format!("invalid server id '{id}'"),
            ));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    pub fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.log"))
    }

    /// The state, or `None` when missing or unreadable as JSON.
    pub fn read(&self, id: &str) -> Result<Option<ServerState>> {
        match fs::read(self.state_path(id)?) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SitefileError::plain(
                E_SERVER,
                format!("cannot read server '{id}': {e}"),
            )),
        }
    }

    /// `JSON.stringify(state, null, 2) + "\n"`, written atomically.
    pub fn write(&self, state: &ServerState) -> Result<()> {
        let dest = self.state_path(&state.id)?;
        let tmp = PathBuf::from(format!("{}.{}.tmp", dest.display(), random_hex(4)));
        let mut text = serde_json::to_string_pretty(state)
            .map_err(|e| SitefileError::plain(E_SERVER, e.to_string()))?;
        text.push('\n');
        let r = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(text.as_bytes())?;
            drop(f);
            fs::rename(&tmp, &dest)
        })();
        r.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            SitefileError::plain(E_SERVER, format!("cannot write '{}': {e}", dest.display()))
        })
    }

    pub fn update(
        &self,
        id: &str,
        patch: impl FnOnce(&mut ServerState),
    ) -> Result<Option<ServerState>> {
        let Some(mut cur) = self.read(id)? else {
            return Ok(None);
        };
        patch(&mut cur);
        self.write(&cur)?;
        Ok(Some(cur))
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let _ = fs::remove_file(self.state_path(id)?);
        let _ = fs::remove_file(self.log_path(id));
        Ok(())
    }

    /// Every server, oldest first.
    pub fn all(&self) -> Result<Vec<ServerState>> {
        let rd = fs::read_dir(&self.dir).map_err(|e| {
            SitefileError::plain(
                E_SERVER,
                format!("cannot read '{}': {e}", self.dir.display()),
            )
        })?;
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json").filter(|i| is_id(i)) {
                if let Some(s) = self.read(id)? {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| a.started.cmp(&b.started));
        Ok(out)
    }

    /// Find by exact ID, exact name, or unique ID prefix (3+ characters).
    pub fn find(&self, key: &str) -> Result<ServerState> {
        let all = self.all()?;
        if let Some(s) = all
            .iter()
            .find(|s| s.id == key)
            .or_else(|| all.iter().find(|s| s.name == key))
        {
            return Ok(s.clone());
        }
        let pre: Vec<&ServerState> = if key.len() >= 3 {
            all.iter().filter(|s| s.id.starts_with(key)).collect()
        } else {
            vec![]
        };
        match pre.len() {
            1 => Ok(pre[0].clone()),
            0 => Err(SitefileError::plain(
                E_NOT_FOUND,
                format!("no such server: {key}"),
            )),
            n => Err(SitefileError::plain(
                E_SERVER,
                format!("ambiguous server '{key}' matches {n} servers"),
            )),
        }
    }

    /// Allocate an ID and validate/choose the name (unique among all servers, running or not).
    pub fn allocate(&self, name: Option<&str>) -> Result<(String, String)> {
        let all = self.all()?;
        if let Some(n) = name {
            if !is_name(n) {
                return Err(SitefileError::plain(
                    E_USAGE,
                    format!("invalid server name '{n}' (letters, digits, '_', '.', '-')"),
                ));
            }
            if all.iter().any(|s| s.name == n) {
                return Err(SitefileError::plain(
                    E_SERVER,
                    format!(
                        "a server named '{n}' already exists; remove it with \"sitefile rm {n}\""
                    ),
                ));
            }
        }
        let id = random_hex(6);
        let name = name
            .map(str::to_string)
            .unwrap_or_else(|| format!("site-{}", &id[..6]));
        Ok((id, name))
    }

    pub fn view(&self, s: &ServerState) -> ServerView {
        let running = is_running(s);
        ServerView {
            state: s.clone(),
            status: if running { "running" } else { "exited" },
            url: if s.ready {
                format!(
                    "{}{}",
                    url_of(&s.host, s.port),
                    &s.base.as_deref().unwrap_or("/")[1..]
                )
            } else {
                String::new()
            },
        }
    }
}

fn probe_host(host: &str) -> &str {
    match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        h => h,
    }
}

fn url_of(host: &str, port: u16) -> String {
    let shown = probe_host(host);
    if shown.contains(':') {
        format!("http://[{shown}]:{port}/")
    } else {
        format!("http://{shown}:{port}/")
    }
}

/// Whether a process exists (`kill -0`; EPERM counts as alive). A zombie (a
/// child that exited and was not waited for) still counts; detached workers
/// are reaped by [`start_detached`], so this sees them exit.
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    match std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => true,
        Ok(o) => String::from_utf8_lossy(&o.stderr)
            .to_ascii_lowercase()
            .contains("not permitted"),
        Err(_) => false,
    }
}

/// Alive AND answering on its port with its own id header (guards against pid and port reuse).
pub fn is_running(s: &ServerState) -> bool {
    if !s.ready || !pid_alive(s.pid) {
        return false;
    }
    let Some(addr) = (probe_host(&s.host), s.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    else {
        return false;
    };
    let t = Duration::from_millis(1500);
    let Ok(mut c) = TcpStream::connect_timeout(&addr, t) else {
        return false;
    };
    let _ = c.set_read_timeout(Some(t));
    let _ = c.set_write_timeout(Some(t));
    let req = format!(
        "HEAD / HTTP/1.1\r\nhost: {}\r\nx-sitefile-probe: {}\r\nconnection: close\r\n\r\n",
        probe_host(&s.host),
        s.id
    );
    if c.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = c.take(64 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    head.split("\r\n").skip(1).any(|l| match l.split_once(':') {
        Some((k, v)) => k.trim().eq_ignore_ascii_case("x-sitefile-server") && v.trim() == s.id,
        None => false,
    })
}

/// Load a directory into a Tree through the sandboxed local reader (symlinks
/// must stay inside, hard-linked and special files are refused, `.git` is
/// skipped, `.sitefileignore` is not read); hidden files (except `.well-known`) are not served.
pub fn load_directory_tree(dir: &Path) -> Result<Tree> {
    load_directory_tree_with(dir, &ServedTreeOptions::default())
}

/// How a directory or archive is read for serving.
#[derive(Default, Clone, Copy)]
pub struct ServedTreeOptions<'a> {
    /// Serve OS metadata files (`__MACOSX/`, `._*`, `Thumbs.db`, `desktop.ini`, ...) too.
    pub keep_os_metadata: bool,
    /// Told how many OS metadata files were skipped.
    pub warn: Option<&'a dyn Fn(&str)>,
}

/// `tree` as served: without OS metadata (unless kept; with a warning naming
/// `origin`) and hidden files except `.well-known`.
fn served(tree: Tree, origin: &str, options: &ServedTreeOptions) -> Result<Tree> {
    let tree = if options.keep_os_metadata {
        tree
    } else {
        let (tree, skipped) = without_os_metadata(tree)?;
        if skipped > 0 {
            if let Some(warn) = options.warn {
                warn(&os_metadata_warning(origin, skipped));
            }
        }
        tree
    };
    visible(&tree)
}

/// [`load_directory_tree`] with `options`.
pub fn load_directory_tree_with(dir: &Path, options: &ServedTreeOptions) -> Result<Tree> {
    let ctx = LocalContext::open(
        dir,
        &LocalContextOptions {
            sitefileignore: false,
            ..Default::default()
        },
    )?;
    let Resource::Tree { tree, .. } = ctx.read_tree("", "serve", None)? else {
        unreachable!("read_tree returns a tree")
    };
    served(tree, &dir.display().to_string(), options)
}

/// Paths of `tree` a directory or archive serves: no hidden files except `.well-known`.
fn visible(tree: &Tree) -> Result<Tree> {
    let visible = tree
        .entries()
        .filter(|(p, _)| {
            p.split('/')
                .all(|seg| !seg.starts_with('.') || seg.is_empty() || seg == ".well-known")
        })
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect::<Vec<_>>();
    Tree::from_files(visible)
}

/// [`load_archive_tree_with`] with [`DEFAULT_ARCHIVE_LIMITS`].
pub fn load_archive_tree(file: &Path) -> Result<Tree> {
    load_archive_tree_with(file, &DEFAULT_ARCHIVE_LIMITS)
}

/// Load an archive file into a Tree. The format comes from the bytes, never
/// the name: zip, tar, tar.gz, a Web Bundle, or an OCI image archive (a tar
/// holding an OCI layout, as `build -o x.oci.tar` and `save` write it). A zip,
/// tar or Web Bundle is extracted with `limits` (unsafe entries are refused)
/// and served like a directory, without hidden files except `.well-known`; an
/// OCI archive serves its first image's files, like that image from the store,
/// after checking every blob's digest and its site layer against `limits`.
pub fn load_archive_tree_with(file: &Path, limits: &ArchiveLimits) -> Result<Tree> {
    load_archive_tree_opts(file, limits, &ServedTreeOptions::default())
}

/// [`load_archive_tree`] with hidden files and OS metadata kept (`verify` and `diff` compare every file).
pub fn load_archive_tree_all(file: &Path) -> Result<Tree> {
    load_archive(
        file,
        &DEFAULT_ARCHIVE_LIMITS,
        true,
        &ServedTreeOptions::default(),
    )
}

/// [`load_archive_tree_with`] with `options` (OS metadata files are dropped unless kept).
pub fn load_archive_tree_opts(
    file: &Path,
    limits: &ArchiveLimits,
    options: &ServedTreeOptions,
) -> Result<Tree> {
    load_archive(file, limits, false, options)
}

fn load_archive(
    file: &Path,
    limits: &ArchiveLimits,
    hidden: bool,
    options: &ServedTreeOptions,
) -> Result<Tree> {
    let origin = file.display().to_string();
    let unreadable = || SitefileError::plain(E_ARCHIVE, format!("cannot read '{origin}'"));
    check_archive_file(file, limits)?;
    let bytes = fs::read(file).map_err(|_| unreadable())?;
    let warc = crate::warc::looks_like_warc(&bytes);
    let tree = if warc {
        crate::warc::read_warc(&bytes, &origin, limits, None)?
    } else {
        extract_archive(&bytes, &origin, limits, None)?
    };
    if !warc && tree.is_file("/oci-layout") {
        return oci_archive_tree(&tree, &origin, limits);
    }
    if hidden {
        return Ok(tree);
    }
    served(tree, &origin, options)
}

/// The checks every archive file gets before it is read.
fn check_archive_file(file: &Path, limits: &ArchiveLimits) -> Result<()> {
    let origin = file.display().to_string();
    let unreadable = || SitefileError::plain(E_ARCHIVE, format!("cannot read '{origin}'"));
    let meta = fs::metadata(file).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SitefileError::plain(E_NOT_FOUND, format!("'{origin}' does not exist"))
        } else {
            unreadable()
        }
    })?;
    if !meta.is_file() {
        return Err(SitefileError::plain(
            E_TYPE,
            format!("'{origin}' is not a file"),
        ));
    }
    // The archive is at most its expanded size plus tar framing for every entry.
    let cap = limits
        .max_total_bytes
        .saturating_add(limits.max_entries.saturating_add(2).saturating_mul(4 * 512));
    if meta.len() > cap {
        return Err(SitefileError::plain(
            E_ARCHIVE_LIMIT,
            format!(
                "{origin}: archive expands beyond {} bytes",
                limits.max_total_bytes
            ),
        ));
    }
    Ok(())
}

/// What a server serves: the tree and, for a zip served in place, where its contents come from.
pub struct ServedSite {
    pub tree: Tree,
    pub content: Option<Arc<dyn SiteContent>>,
}

/// A zip archive file opened for serving in place (#213): its entries are
/// checked and indexed at start (each inflated once, one at a time, for its
/// CRC-32 and digest) and read from the file on each request, so memory does
/// not grow with the archive. The tree is the one [`load_archive_tree_opts`]
/// serves, with the same errors. `None` for anything that is not a zip (or a
/// zip holding an OCI layout).
pub fn open_zip_archive(
    file: &Path,
    limits: &ArchiveLimits,
    options: &ServedTreeOptions,
) -> Result<Option<ServedSite>> {
    let origin = file.display().to_string();
    check_archive_file(file, limits)?;
    let mut head = [0u8; 4];
    {
        use std::io::Read;
        let mut f = fs::File::open(file)
            .map_err(|_| SitefileError::plain(E_ARCHIVE, format!("cannot read '{origin}'")))?;
        let mut got = 0;
        while got < 4 {
            match f.read(&mut head[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => {
                    return Err(SitefileError::plain(
                        E_ARCHIVE,
                        format!("cannot read '{origin}'"),
                    ))
                }
            }
        }
    }
    let zip = head[0] == 0x50
        && head[1] == 0x4b
        && ((head[2] == 3 && head[3] == 4) || (head[2] == 5 && head[3] == 6));
    if !zip {
        return Ok(None);
    }
    let z = open_zip_file(file, &origin, limits)?;
    if z.tree.is_file("/oci-layout") {
        return Ok(None);
    }
    let tree = served(z.tree.clone(), &origin, options)?;
    Ok(Some(ServedSite {
        tree,
        content: Some(Arc::new(z)),
    }))
}

/// [`load_served_tree_with`], serving a zip archive in place ([`open_zip_archive`]).
pub fn load_served_site(
    store: &Store,
    source: &ServerSource,
    options: &ServedTreeOptions,
) -> Result<ServedSite> {
    if let ServerSource::Archive { path } = source {
        if let Some(site) = open_zip_archive(Path::new(path), &DEFAULT_ARCHIVE_LIMITS, options)? {
            return Ok(site);
        }
    }
    Ok(ServedSite {
        tree: load_served_tree_with(store, source, options)?,
        content: None,
    })
}

/// The files of the first image in an extracted OCI layout.
fn oci_archive_tree(layout: &Tree, origin: &str, limits: &ArchiveLimits) -> Result<Tree> {
    let bad = |why: String| SitefileError::plain(E_ARCHIVE, format!("{origin}: {why}"));
    let blob = |digest: &str, size: u64| -> Result<Vec<u8>> {
        let path = blob_path_of(digest)?;
        let node = layout
            .get(&path)
            .ok_or_else(|| bad(format!("OCI archive has no blob {digest}")))?;
        if sha256(&node.bytes) != digest || node.bytes.len() as u64 != size {
            return Err(bad(format!("blob {digest} does not match its digest")));
        }
        Ok(node.bytes.to_vec())
    };
    let index = layout
        .get("/index.json")
        .ok_or_else(|| bad("OCI archive has no index.json".into()))?;
    let entries =
        decode_index(&index.bytes).map_err(|_| bad("OCI archive index.json is corrupt".into()))?;
    let Some(first) = entries.first() else {
        return Err(bad("OCI archive holds no image".into()));
    };
    let m = parse_manifest(&blob(&first.digest, first.size)?)
        .map_err(|_| bad("OCI archive's first manifest is not an image manifest".into()))?;
    let cfg = parse_config(&blob(&m.config.digest, m.config.size)?)
        .map_err(|_| bad("OCI archive's image config is corrupt".into()))?;
    let layers: Vec<&crate::export::oci::Descriptor> = site_layer_indices(&m, &cfg)
        .into_iter()
        .map(|i| &m.layers[i])
        .collect();
    // An empty list makes extract_site_layers report "not a Sitefile image".
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    if layers
        .iter()
        .all(|l| l.media_type == media::LAYER || l.media_type == media::LAYER_TAR)
    {
        for l in &layers {
            let b = blob(&l.digest, l.size)?;
            // Hold each site layer to the archive limits before it is fully decoded.
            extract_archive(&b, origin, limits, None)?;
            blobs.push(b);
        }
    }
    if layers.is_empty() {
        blobs.push(Vec::new());
    }
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    crate::export::oci::extract_site_layers(&m, &cfg, &refs, origin, None)
}

/// The tree a server state serves.
pub fn load_served_tree(store: &Store, source: &ServerSource) -> Result<Tree> {
    load_served_tree_with(store, source, &ServedTreeOptions::default())
}

/// [`load_served_tree`] with `options` (for a directory or archive).
pub fn load_served_tree_with(
    store: &Store,
    source: &ServerSource,
    options: &ServedTreeOptions,
) -> Result<Tree> {
    match source {
        ServerSource::Image { image_id } => store.load_tree(image_id),
        ServerSource::Dir { path } => load_directory_tree_with(Path::new(path), options),
        ServerSource::Archive { path } => {
            load_archive_tree_opts(Path::new(path), &DEFAULT_ARCHIVE_LIMITS, options)
        }
    }
}

/// When a server's files last changed, in Unix seconds (`Last-Modified`): for an image, when it
/// was first stored (never older than its files, unlike the config's `created`); for a directory,
/// the newest modification time of the files it serves (hidden entries but `.well-known` skipped,
/// symbolic links to files followed); for an archive, the file's modification time. `None` when
/// there is none. Same rules as `servedLastModified` in src/servers.ts.
pub fn served_last_modified(store: &Store, source: &ServerSource) -> Option<i64> {
    fn secs(t: std::time::SystemTime) -> Option<i64> {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
    }
    fn walk(dir: &Path, newest: &mut Option<i64>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name != ".well-known" {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                walk(&p, newest);
            } else if ft.is_file() || ft.is_symlink() {
                if let Ok(m) = fs::metadata(&p) {
                    if m.is_file() {
                        if let Some(t) = m.modified().ok().and_then(secs) {
                            *newest = Some(newest.map_or(t, |n| n.max(t)));
                        }
                    }
                }
            }
        }
    }
    match source {
        ServerSource::Image { image_id } => {
            let created = store.details(image_id).ok()?.created;
            crate::cli::common::parse_iso(&created).map(|ms| ms.div_euclid(1000))
        }
        ServerSource::Archive { path } => fs::metadata(path).ok()?.modified().ok().and_then(secs),
        ServerSource::Dir { path } => {
            let mut newest = None;
            walk(Path::new(path), &mut newest);
            newest
        }
    }
}

/// `-p` value: `port`, `host:port` or `[v6]:port`. Default (`None`) bind is
/// 127.0.0.1 with a free port; `localhost` means 127.0.0.1. Errors are `E_USAGE`.
pub fn parse_publish(spec: Option<&str>) -> Result<Bind> {
    let usage = |m: String| Err(SitefileError::plain(E_USAGE, m));
    let Some(spec) = spec else {
        return Ok(Bind {
            host: "127.0.0.1".into(),
            port: 0,
        });
    };
    let bad = || usage(format!("-p expects [host:]port, got '{spec}'"));
    let is_port = |s: &str| (1..=5).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    let (host, port) = if is_port(spec) {
        (None, spec)
    } else if let Some(rest) = spec.strip_prefix('[') {
        let Some((h, p)) = rest.split_once("]:") else {
            return bad();
        };
        if h.is_empty() || !h.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') || !is_port(p) {
            return bad();
        }
        (Some(h), p)
    } else {
        let Some((h, p)) = spec.split_once(':') else {
            return bad();
        };
        if h.is_empty() || h.contains(['[', ']']) || !is_port(p) {
            return bad();
        }
        (Some(h), p)
    };
    let port: u32 = port.parse().unwrap_or(0);
    if port > 65535 {
        return usage(format!("-p: port {port} is out of range"));
    }
    let mut host = host.unwrap_or("127.0.0.1").to_string();
    if host == "localhost" {
        host = "127.0.0.1".into();
    }
    if host.parse::<IpAddr>().is_err() {
        return usage(format!(
            "-p: host must be an IP address or 'localhost', got '{host}'"
        ));
    }
    Ok(Bind {
        host,
        port: port as u16,
    })
}

/// `^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$`: an argument that names a directory.
pub fn looks_like_dir(arg: &str) -> bool {
    arg == "."
        || arg == ".."
        || arg.starts_with('~')
        || arg.starts_with("./")
        || arg.starts_with("../")
        || arg.starts_with('/')
        || arg.ends_with('/')
}

/// An archive file source: labelled by its absolute path, stored by its real path.
pub fn archive_source(arg: &str) -> (String, ServerSource) {
    let file = crate::export::exporter::resolve_path(Path::new(arg));
    let real = fs::canonicalize(&file).unwrap_or(file.clone());
    (
        file.display().to_string(),
        ServerSource::Archive {
            path: real.display().to_string(),
        },
    )
}

pub fn is_regular_file(p: &str) -> bool {
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// What `sitefile serve <arg>` serves: a directory or archive file when `arg`
/// looks like a path (`./site`, `../x.zip`, `/abs`, `~/x`, `dir/`), else an
/// image reference or ID in `store`, else an archive file by that name
/// (`site.zip`). Returns the label (what the user asked for, shown and stored
/// as `ServerState::image`; the absolute path for a directory or file) and the
/// source. A bare name that is not an image but is a directory gets a hint to
/// write `./name`. Whether a file is an archive is checked when it is loaded
/// ([`load_served_tree`]).
pub fn resolve_source(store: &Store, arg: &str) -> Result<(String, ServerSource)> {
    if looks_like_dir(arg) {
        let dir = crate::export::exporter::resolve_path(Path::new(arg));
        if is_regular_file(&dir.display().to_string()) {
            return Ok(archive_source(arg));
        }
        if !fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(SitefileError::plain(
                E_NOT_FOUND,
                format!("'{arg}' is not a directory or a file"),
            ));
        }
        let real = fs::canonicalize(&dir).unwrap_or(dir.clone());
        return Ok((
            dir.display().to_string(),
            ServerSource::Dir {
                path: real.display().to_string(),
            },
        ));
    }
    match store.resolve(arg) {
        Ok(id) => Ok((arg.to_string(), ServerSource::Image { image_id: id })),
        Err(e) => {
            if e.code == E_NOT_FOUND && fs::metadata(arg).map(|m| m.is_dir()).unwrap_or(false) {
                return Err(SitefileError::plain(
                    E_NOT_FOUND,
                    format!("{} (to serve the directory, write ./{arg})", e.message()),
                ));
            }
            if is_regular_file(arg) {
                return Ok(archive_source(arg));
            }
            Err(e)
        }
    }
}

/// Warn about every `_redirects`/`_headers` line `serve` ignores; with `strict`
/// (`--strict-site-files`) any such line is then an error (port of `checkSiteFiles`).
pub fn check_site_files(
    tree: &Tree,
    strict: bool,
    warn: &dyn Fn(&str),
    content: Option<&dyn crate::server::SiteContent>,
) -> Result<()> {
    let ignored = crate::server::site_file_warnings(tree, content);
    for m in &ignored {
        warn(m);
    }
    let n = ignored.len();
    if strict && n > 0 {
        return Err(SitefileError::plain(
            E_VALIDATION,
            format!(
                "{n} line{} of _redirects/_headers {} ignored (--strict-site-files)",
                if n == 1 { "" } else { "s" },
                if n == 1 { "is" } else { "are" }
            ),
        ));
    }
    Ok(())
}

/// A detached server to start ([`serve_detached`]): `serve -d`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedServe {
    /// What is served, as shown (`ServerState::image`): see [`resolve_source`].
    pub label: String,
    pub source: ServerSource,
    /// `--name` (default `site-<6 hex>`).
    pub name: Option<String>,
    /// `-p` ([`parse_publish`]; default 127.0.0.1, a free port).
    pub bind: Bind,
    /// `--spa`.
    pub spa: bool,
    /// `--spa-exclude`.
    pub spa_exclude: Vec<String>,
    /// `--spa-fallback`.
    pub spa_fallback: Option<String>,
    /// `--base` (normalized; `None` for `/`).
    pub base: Option<String>,
    /// `--rewrite-base` (normalized).
    pub rewrite_base: Option<String>,
    /// `--host-profile`.
    pub host_profile: Option<String>,
    /// `--html-handling`.
    pub html_handling: Option<String>,
    /// `--keep-os-metadata`.
    pub keep_os_metadata: bool,
    /// `--allowed-host` names, lowercased (see [`crate::server::parse_allowed_host`]).
    pub allowed_hosts: Vec<String>,
    /// `--cors`.
    pub cors: bool,
    /// `--isolate`: the Cross-Origin-Embedder-Policy (`require-corp` or `credentialless`).
    pub isolate: Option<String>,
    /// `--max-age`, `--immutable`, `--max-age-html`.
    pub cache: Option<crate::server::CachePreview>,
    /// `--log-format json` (`Some("json")`; `None`: text).
    pub log_format: Option<String>,
    /// `--log-ip`.
    pub log_ip: bool,
    /// `--quiet`.
    pub quiet: bool,
    /// `--auth` verifiers.
    pub auth: Vec<crate::serve_auth::AuthEntry>,
    /// `--auth-file`, an absolute path.
    pub auth_file: Option<String>,
    /// `--max-connections-per-ip` (0: no limit).
    pub max_connections_per_ip: usize,
    /// `--random-route`: `base` is a secret.
    pub hide_base: bool,
    /// `--allow-ip`, `--deny-ip` blocks as given.
    pub allow_ip: Option<Vec<String>>,
    pub deny_ip: Option<Vec<String>>,
    /// `--stop-after`: when the server stops (ISO time).
    pub expires: Option<String>,
    /// `--country`.
    pub country: Option<String>,
    /// `--strict-site-files`.
    pub strict_site_files: bool,
    /// The servers file it belongs to (`sitefile up`), by real path.
    pub project: Option<String>,
}

impl DetachedServe {
    /// Serve `arg` (an image reference or ID, a directory or an archive file, as `serve` takes it)
    /// on 127.0.0.1 with a free port, unnamed, not as an SPA.
    pub fn resolve(store: &Store, arg: &str) -> Result<DetachedServe> {
        let (label, source) = resolve_source(store, arg)?;
        Ok(DetachedServe {
            label,
            source,
            name: None,
            bind: parse_publish(None)?,
            spa: false,
            spa_exclude: Vec::new(),
            spa_fallback: None,
            base: None,
            rewrite_base: None,
            host_profile: None,
            html_handling: None,
            keep_os_metadata: false,
            allowed_hosts: Vec::new(),
            cors: false,
            isolate: None,
            cache: None,
            log_format: None,
            log_ip: false,
            quiet: false,
            auth: Vec::new(),
            auth_file: None,
            max_connections_per_ip: 0,
            hide_base: false,
            allow_ip: None,
            deny_ip: None,
            expires: None,
            country: None,
            strict_site_files: false,
            project: None,
        })
    }
}

/// How long [`start_detached`] waits for the worker to listen.
pub const START_TIMEOUT: Duration = Duration::from_secs(15);

/// `serve -d`: check that the image, directory or archive loads, allocate an ID and
/// name, record the server, start its worker and wait until it listens.
/// Returns the running server's state (its real host and port). On failure
/// the record is removed. `worker_exe` runs as `<exe> __serve-worker <id>`
/// (`None`: the current executable); the worker is reaped (see the module docs).
pub fn serve_detached(
    reg: &ServerRegistry,
    serve: &DetachedServe,
    worker_exe: Option<&Path>,
) -> Result<ServerState> {
    serve_detached_with(reg, serve, worker_exe, &|_| {})
}

/// [`serve_detached`], telling `warn` how many OS metadata files the directory or archive skips.
pub fn serve_detached_with(
    reg: &ServerRegistry,
    serve: &DetachedServe,
    worker_exe: Option<&Path>,
    warn: &dyn Fn(&str),
) -> Result<ServerState> {
    let options = ServedTreeOptions {
        keep_os_metadata: serve.keep_os_metadata,
        warn: Some(warn),
    };
    // fail fast on a bad image, directory or archive (or an ignored site-file line with --strict-site-files)
    let site = load_served_site(&reg.store, &serve.source, &options)?;
    let routing = crate::server::routing_for(
        serve.host_profile.as_deref(),
        serve.html_handling.as_deref(),
    );
    if routing.is_none_or(|p| p.site_files) {
        check_site_files(
            &site.tree,
            serve.strict_site_files,
            warn,
            site.content.as_deref(),
        )?;
    }
    drop(site);
    let (id, final_name) = reg.allocate(serve.name.as_deref())?;
    let state = ServerState {
        id: id.clone(),
        name: final_name,
        image: serve.label.clone(),
        source: serve.source.clone(),
        spa: serve.spa,
        spa_exclude: serve.spa_exclude.clone(),
        spa_fallback: serve.spa_fallback.clone(),
        base: serve.base.clone(),
        rewrite_base: serve.rewrite_base.clone(),
        host_profile: serve.host_profile.clone(),
        html_handling: serve.html_handling.clone(),
        keep_os_metadata: serve.keep_os_metadata,
        allowed_hosts: serve.allowed_hosts.clone(),
        cors: serve.cors,
        isolate: serve.isolate.clone(),
        cache: serve.cache.clone(),
        log_format: serve.log_format.clone(),
        log_ip: serve.log_ip,
        quiet: serve.quiet,
        auth: serve.auth.clone(),
        auth_file: serve.auth_file.clone(),
        max_connections_per_ip: serve.max_connections_per_ip,
        hide_base: serve.hide_base,
        allow_ip: serve.allow_ip.clone(),
        deny_ip: serve.deny_ip.clone(),
        expires: serve.expires.clone(),
        country: serve.country.clone(),
        strict_site_files: serve.strict_site_files,
        host: serve.bind.host.clone(),
        port: serve.bind.port,
        bind: serve.bind.clone(),
        pid: 0,
        started: crate::store::now_iso(),
        log_file: reg.log_path(&id).display().to_string(),
        ready: false,
        error: None,
        project: serve.project.clone(),
    };
    reg.write(&state)?;
    match start_detached(reg, &state, worker_exe) {
        Ok(up) => Ok(up),
        Err(e) => {
            reg.remove(&id)?;
            Err(e)
        }
    }
}

/// `server restart`: stop `s` if it is running (SIGTERM, `grace`, SIGKILL),
/// then start it again with the same ID, name, source and requested bind
/// address (an image by the same ID; a directory or archive file is re-read). On failure the
/// record stays, marked not ready.
pub fn restart_detached(
    reg: &ServerRegistry,
    s: &ServerState,
    grace: Duration,
    worker_exe: Option<&Path>,
) -> Result<ServerState> {
    stop_server(reg, s, grace)?;
    let options = ServedTreeOptions {
        keep_os_metadata: s.keep_os_metadata,
        warn: None,
    };
    load_served_site(&reg.store, &s.source, &options)?;
    let mut fresh = s.clone();
    fresh.host = s.bind.host.clone();
    fresh.port = s.bind.port;
    fresh.pid = 0;
    fresh.ready = false;
    fresh.started = crate::store::now_iso();
    fresh.error = None;
    reg.write(&fresh)?;
    match start_detached(reg, &fresh, worker_exe) {
        Ok(u) => Ok(u),
        Err(e) => {
            reg.update(&s.id, |x| x.ready = false)?;
            Err(e)
        }
    }
}

/// Spawn the detached worker for an already recorded `state` (its own process
/// group, stdin closed, output appended to its log, `SITEFILE_HOME` set to the
/// registry's store) and wait until the record says ready, the worker fails, or
/// [`START_TIMEOUT`]. The worker is reaped on a background thread.
pub fn start_detached(
    reg: &ServerRegistry,
    state: &ServerState,
    worker_exe: Option<&Path>,
) -> Result<ServerState> {
    use std::os::unix::process::CommandExt;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&state.log_file)
        .map_err(|e| {
            SitefileError::plain(E_SERVER, format!("cannot open '{}': {e}", state.log_file))
        })?;
    let exe = match worker_exe {
        Some(p) => p.to_path_buf(),
        None => std::env::current_exe().map_err(|e| {
            SitefileError::plain(
                E_SERVER,
                format!("cannot find the sitefile executable: {e}"),
            )
        })?,
    };
    let log2 = log
        .try_clone()
        .map_err(|e| SitefileError::plain(E_SERVER, e.to_string()))?;
    let mut child = std::process::Command::new(exe)
        .arg("__serve-worker")
        .arg(&state.id)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log2)
        .env("SITEFILE_HOME", &reg.store.home)
        .process_group(0)
        .spawn()
        .map_err(|e| {
            SitefileError::plain(E_SERVER, format!("cannot start the server process: {e}"))
        })?;
    let deadline = Instant::now() + START_TIMEOUT;
    let result = loop {
        let cur = match reg.read(&state.id) {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                break Err(e);
            }
        };
        if let Some(c) = cur.as_ref().filter(|c| c.ready) {
            break Ok(c.clone());
        }
        let exited = matches!(child.try_wait(), Ok(Some(_)) | Err(_));
        let error = cur.as_ref().and_then(|c| c.error.clone());
        if exited || error.is_some() || Instant::now() > deadline {
            if !exited {
                let _ = child.kill();
            }
            let why = error.unwrap_or_else(|| {
                if exited {
                    "the server process exited before it was ready".into()
                } else {
                    "timed out waiting for the server to start".into()
                }
            });
            break Err(SitefileError::plain(
                E_SERVER,
                why.strip_prefix("error[E_SERVER]: ")
                    .unwrap_or(&why)
                    .to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(40));
    };
    reap(child);
    result
}

/// Wait for `child` on a background thread, so it never lingers as a zombie.
/// (If no thread can be started, it stays unreaped until this process exits.)
fn reap(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("sitefile-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn send(pid: i64, sig: i32) -> bool {
    if pid <= 0 || pid > i32::MAX as i64 {
        return false;
    }
    // SAFETY: plain syscall wrapper.
    unsafe { kill(pid as i32, sig) == 0 }
}

/// `server stop`: SIGTERM, wait up to `grace`, then SIGKILL (and 2 s more).
/// Returns false if the server was not running. Marks the record not ready.
pub fn stop_server(reg: &ServerRegistry, s: &ServerState, grace: Duration) -> Result<bool> {
    if !is_running(s) {
        return Ok(false);
    }
    let gone = |d: Duration| {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if !pid_alive(s.pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !pid_alive(s.pid)
    };
    if !send(s.pid, SIGTERM) {
        return Ok(false);
    }
    if !gone(grace) {
        send(s.pid, SIGKILL);
        if !gone(Duration::from_secs(2)) {
            return Err(SitefileError::plain(
                E_SERVER,
                format!("could not stop {} (pid {})", s.name, s.pid),
            ));
        }
    }
    let _ = reg.update(&s.id, |x| x.ready = false);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::exporter::tests::TmpDir;
    use crate::server::{listen, SiteServerOptions};
    use crate::tree::make_file;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn state(reg: &ServerRegistry, id: &str, name: &str, started: &str) -> ServerState {
        ServerState {
            id: id.into(),
            name: name.into(),
            image: "./site".into(),
            source: ServerSource::Dir {
                path: "/tmp/site".into(),
            },
            spa: false,
            spa_exclude: Vec::new(),
            spa_fallback: None,
            base: None,
            rewrite_base: None,
            host_profile: None,
            html_handling: None,
            keep_os_metadata: false,
            allowed_hosts: Vec::new(),
            cors: false,
            isolate: None,
            cache: None,
            log_format: None,
            log_ip: false,
            quiet: false,
            auth: Vec::new(),
            auth_file: None,
            max_connections_per_ip: 0,
            hide_base: false,
            allow_ip: None,
            deny_ip: None,
            expires: None,
            country: None,
            strict_site_files: false,
            bind: Bind {
                host: "127.0.0.1".into(),
                port: 0,
            },
            host: "127.0.0.1".into(),
            port: 0,
            pid: 0,
            started: started.into(),
            log_file: reg.log_path(id).to_string_lossy().into_owned(),
            ready: false,
            error: None,
            project: None,
        }
    }

    #[test]
    fn state_json_matches_typescript_format() {
        let d = TmpDir::new("servers");
        let reg = ServerRegistry::open(Some(Store::open(d.path()).unwrap())).unwrap();
        let mut s = state(&reg, "0123456789ab", "web", "2024-01-01T00:00:00.000Z");
        s.source = ServerSource::Image {
            image_id: format!("sha256:{}", "a".repeat(64)),
        };
        reg.write(&s).unwrap();
        let text = fs::read_to_string(reg.state_path(&s.id).unwrap()).unwrap();
        let expected = format!(
            "{{\n  \"id\": \"0123456789ab\",\n  \"name\": \"web\",\n  \"image\": \"./site\",\n  \"source\": {{\n    \"kind\": \"image\",\n    \"imageId\": \"sha256:{}\"\n  }},\n  \"spa\": false,\n  \"bind\": {{\n    \"host\": \"127.0.0.1\",\n    \"port\": 0\n  }},\n  \"host\": \"127.0.0.1\",\n  \"port\": 0,\n  \"pid\": 0,\n  \"started\": \"2024-01-01T00:00:00.000Z\",\n  \"logFile\": {},\n  \"ready\": false\n}}\n",
            "a".repeat(64),
            serde_json::to_string(&s.log_file).unwrap()
        );
        assert_eq!(text, expected);
        // a TypeScript-written state (with error) reads back
        let ts = r#"{"id":"0123456789ab","name":"web","image":"./d","source":{"kind":"dir","path":"/x"},"spa":true,"bind":{"host":"0.0.0.0","port":8080},"host":"0.0.0.0","port":8080,"pid":0,"started":"s","logFile":"/l","ready":false,"error":"error[E_SERVER]: boom"}"#;
        fs::write(reg.state_path("0123456789ab").unwrap(), ts).unwrap();
        let back = reg.read("0123456789ab").unwrap().unwrap();
        assert_eq!(back.error.as_deref(), Some("error[E_SERVER]: boom"));
        assert_eq!(back.source, ServerSource::Dir { path: "/x".into() });
        let v = reg.view(&back);
        assert_eq!((v.status, v.url.as_str()), ("exited", ""));
        fs::write(reg.state_path("0123456789ab").unwrap(), "{not json").unwrap();
        assert_eq!(reg.read("0123456789ab").unwrap(), None);
        assert_eq!(reg.state_path("../x").unwrap_err().code, E_SERVER);
    }

    #[test]
    fn find_allocate_update_remove() {
        let d = TmpDir::new("servers");
        let reg = ServerRegistry::open(Some(Store::open(d.path()).unwrap())).unwrap();
        reg.write(&state(&reg, "abc111111111", "one", "2")).unwrap();
        reg.write(&state(&reg, "abc222222222", "two", "1")).unwrap();
        assert_eq!(
            reg.all()
                .unwrap()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["two", "one"]
        );
        assert_eq!(reg.find("one").unwrap().id, "abc111111111");
        assert_eq!(reg.find("abc2").unwrap().name, "two");
        assert_eq!(reg.find("abc").unwrap_err().code, E_SERVER);
        assert_eq!(reg.find("ab").unwrap_err().code, E_NOT_FOUND);
        assert_eq!(reg.allocate(Some("one")).unwrap_err().code, E_SERVER);
        assert_eq!(reg.allocate(Some("-bad")).unwrap_err().code, E_USAGE);
        let (id, name) = reg.allocate(None).unwrap();
        assert!(is_id(&id));
        assert_eq!(name, format!("site-{}", &id[..6]));
        let up = reg
            .update("abc111111111", |s| s.error = Some("x".into()))
            .unwrap()
            .unwrap();
        assert_eq!(up.error.as_deref(), Some("x"));
        assert_eq!(reg.update("fff111111111", |_| {}).unwrap(), None);
        reg.remove("abc111111111").unwrap();
        assert_eq!(reg.all().unwrap().len(), 1);
    }

    #[test]
    fn running_needs_live_pid_and_matching_header() {
        let d = TmpDir::new("servers");
        let reg = ServerRegistry::open(Some(Store::open(d.path()).unwrap())).unwrap();
        let id = "00000000aaaa";
        let t = Arc::new(
            Tree::from_files([(
                "/index.html".to_string(),
                make_file(b"hi".to_vec(), "t", 0o644),
            )])
            .unwrap(),
        );
        let srv = listen(
            t.clone(),
            SiteServerOptions {
                server_id: Some(id.into()),
                ..Default::default()
            },
            "127.0.0.1",
            0,
        )
        .unwrap();
        let mut s = state(&reg, id, "live", "1");
        s.port = srv.port;
        s.ready = true;
        s.pid = std::process::id() as i64;
        assert!(is_running(&s));
        assert_eq!(reg.view(&s).status, "running");
        assert_eq!(reg.view(&s).url, format!("http://127.0.0.1:{}/", srv.port));
        let mut other = s.clone();
        other.id = "00000000bbbb".into();
        assert!(
            !is_running(&other),
            "a reused port answering with another id is not ours"
        );
        let mut dead = s.clone();
        dead.pid = 999_999_999;
        assert!(!is_running(&dead));
        srv.close();
        assert!(!is_running(&s));
        assert!(pid_alive(std::process::id() as i64));
        assert!(!pid_alive(0));
    }

    #[test]
    fn directory_trees_hide_dotfiles_and_refuse_escapes() {
        let d = TmpDir::new("servers");
        let site = d.path().join("site");
        fs::create_dir_all(site.join(".well-known")).unwrap();
        fs::create_dir_all(site.join(".git")).unwrap();
        fs::create_dir_all(site.join("sub")).unwrap();
        fs::write(site.join("index.html"), "hi").unwrap();
        fs::write(site.join(".env"), "secret").unwrap();
        fs::write(site.join(".well-known/security.txt"), "x").unwrap();
        fs::write(site.join(".git/HEAD"), "ref").unwrap();
        fs::write(site.join("sub/a.txt"), "a").unwrap();
        std::os::unix::fs::symlink(site.join("sub/a.txt"), site.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&site, site.join("sub/loop")).unwrap();
        let t = load_directory_tree(&site).unwrap();
        let paths: Vec<&str> = t.entries().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/.well-known/security.txt",
                "/index.html",
                "/link.txt",
                "/sub/a.txt"
            ]
        );
        fs::write(d.path().join("outside.txt"), "no").unwrap();
        std::os::unix::fs::symlink(d.path().join("outside.txt"), site.join("escape.txt")).unwrap();
        assert_eq!(load_directory_tree(&site).unwrap_err().code, E_PATH_ESCAPE);
        fs::remove_file(site.join("escape.txt")).unwrap();
        fs::hard_link(site.join("index.html"), site.join("hard.html")).unwrap();
        assert_eq!(load_directory_tree(&site).unwrap_err().code, E_HARDLINK);
        assert_eq!(
            load_directory_tree(&d.path().join("nope"))
                .unwrap_err()
                .code,
            E_NOT_FOUND
        );
        let store = Store::open(d.path().join("home")).unwrap();
        assert_eq!(
            load_served_tree(
                &store,
                &ServerSource::Image {
                    image_id: format!("sha256:{}", "0".repeat(64))
                }
            )
            .unwrap_err()
            .code,
            E_NOT_FOUND
        );
    }

    fn site_tree() -> Tree {
        Tree::from_files(
            [
                ("/index.html", "hi"),
                ("/.env", "secret"),
                ("/.well-known/security.txt", "x"),
                ("/sub/a.txt", "a"),
            ]
            .map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644))),
        )
        .unwrap()
    }

    fn paths(t: &Tree) -> Vec<&str> {
        t.entries().map(|(p, _)| p.as_str()).collect()
    }

    /// Paths and contents.
    fn files(t: &Tree) -> Vec<(String, Vec<u8>)> {
        t.entries()
            .map(|(p, n)| (p.clone(), n.bytes.to_vec()))
            .collect()
    }

    /// `site_tree()` as zip, tar, tgz, wbn and an OCI archive, written into `dir`.
    fn write_archives(dir: &Path) -> Vec<(&'static str, PathBuf)> {
        use crate::export::archive_writers::{write_tar, write_tgz, write_zip, WriterOptions};
        use crate::export::oci::{build_oci_image, oci_archive, OciOptions};
        let site = site_tree();
        let w = WriterOptions::default();
        let img = build_oci_image(&site, &OciOptions::default()).unwrap();
        let all: [(&str, Vec<u8>); 5] = [
            ("site.zip", write_zip(&site, &w).unwrap()),
            ("site.tar", write_tar(&site, &w).unwrap()),
            ("site.tgz", write_tgz(&site, &w).unwrap()),
            (
                "site.wbn",
                crate::export::wbn::write_web_bundle(&site, Some("https://example.com/")).unwrap(),
            ),
            (
                "site.oci.tar",
                oci_archive(img.parts(), Some("site:1"), None, &[], &[]).unwrap(),
            ),
        ];
        all.into_iter()
            .map(|(n, b)| {
                fs::write(dir.join(n), b).unwrap();
                (n, dir.join(n))
            })
            .collect()
    }

    #[test]
    fn archive_files_are_served_in_every_format() {
        let d = TmpDir::new("servers");
        let store = Store::open(d.path().join("home")).unwrap();
        for (name, file) in write_archives(d.path()) {
            let arg = file.display().to_string();
            let (label, source) = resolve_source(&store, &arg).unwrap();
            let real = fs::canonicalize(&file).unwrap().display().to_string();
            assert_eq!(label, arg, "{name}");
            assert_eq!(source, ServerSource::Archive { path: real }, "{name}");
            let t = load_served_tree(&store, &source).unwrap();
            if name == "site.oci.tar" {
                // An OCI archive is an image: served exactly as the store would serve it.
                assert_eq!(files(&t), files(&site_tree()), "{name}");
            } else {
                assert_eq!(
                    paths(&t),
                    ["/.well-known/security.txt", "/index.html", "/sub/a.txt"],
                    "{name}: hidden files are not served, as for a directory"
                );
                assert_eq!(&*t.get("/index.html").unwrap().bytes, b"hi");
            }
        }
    }

    #[test]
    fn os_metadata_is_not_served_unless_kept() {
        let d = TmpDir::new("servers");
        let zip = d.path().join("site.zip");
        fs::write(&zip, crate::resources::archive::tests::finder_zip()).unwrap();
        let dir = d.path().join("d");
        fs::create_dir_all(dir.join("__MACOSX")).unwrap();
        for (p, c) in [
            ("index.html", "hi"),
            ("Thumbs.db", "x"),
            ("__MACOSX/._index.html", "x"),
            ("desktop.ini", "x"),
        ] {
            fs::write(dir.join(p), c).unwrap();
        }
        let warnings = std::cell::RefCell::new(Vec::<String>::new());
        let warn = |m: &str| warnings.borrow_mut().push(m.to_string());
        let opts = ServedTreeOptions {
            keep_os_metadata: false,
            warn: Some(&warn),
        };
        let keep = ServedTreeOptions {
            keep_os_metadata: true,
            warn: None,
        };
        let t = load_archive_tree_opts(&zip, &DEFAULT_ARCHIVE_LIMITS, &opts).unwrap();
        assert_eq!(paths(&t), ["/site/img/a.png", "/site/index.html"]);
        // Hidden files (.DS_Store, ._*) are still not served.
        assert_eq!(
            load_archive_tree_opts(&zip, &DEFAULT_ARCHIVE_LIMITS, &keep)
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            paths(&load_directory_tree_with(&dir, &opts).unwrap()),
            ["/index.html"]
        );
        assert_eq!(load_directory_tree_with(&dir, &keep).unwrap().len(), 3);
        let w = warnings.borrow();
        assert_eq!(w.len(), 2);
        assert!(
            w[0].contains("site.zip: skipped 6 OS metadata files"),
            "{}",
            w[0]
        );
        assert!(w[1].contains(": skipped 3 OS metadata files"), "{}", w[1]);
        // The default loaders filter too.
        assert_eq!(load_archive_tree(&zip).unwrap().len(), 2);
    }

    fn zip_site() -> Tree {
        Tree::from_files(
            [
                ("/index.html", "<h1>home</h1>".repeat(40)),
                ("/about/index.html", "<h1>about</h1>".into()),
                ("/404.html", "missing".into()),
                (
                    "/_redirects",
                    "/old /about/ 301\n/rewrite /index.html 200\n".into(),
                ),
                ("/_headers", "/*\n  X-Test: yes\n".into()),
                ("/app.js", "console.log(1);".repeat(30)),
                ("/tiny.txt", "x".into()),
                ("/.env", "hidden".into()),
            ]
            .map(|(p, c)| (p.to_string(), make_file(c.into_bytes(), "t", 0o644))),
        )
        .unwrap()
    }

    /// Where stored entry `name`'s data starts in zip `z` (after its local header's name and extra field).
    fn data_of(z: &[u8], name: &str) -> usize {
        let at = z
            .windows(name.len())
            .position(|w| w == name.as_bytes())
            .unwrap();
        at + name.len() + u16::from_le_bytes([z[at - 2], z[at - 1]]) as usize
    }

    #[test]
    fn a_zip_served_in_place_answers_as_the_extracted_zip_does() {
        use crate::export::archive_writers::{write_zip, WriterOptions};
        use crate::server::handle_request;
        let d = TmpDir::new("servers");
        let file = d.path().join("site.zip");
        fs::write(
            &file,
            write_zip(&zip_site(), &WriterOptions::default()).unwrap(),
        )
        .unwrap();
        let eager = load_archive_tree(&file).unwrap();
        let lazy = open_zip_archive(
            &file,
            &DEFAULT_ARCHIVE_LIMITS,
            &ServedTreeOptions::default(),
        )
        .unwrap()
        .unwrap();
        let shape = |t: &Tree| {
            t.entries()
                .map(|(p, n)| (p.clone(), n.digest.clone(), n.mode))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&lazy.tree), shape(&eager));
        assert!(lazy.tree.entries().all(|(_, n)| n.bytes.is_empty()));
        let lm: crate::server::LastModifiedFn = Arc::new(|| Some(1_700_000_000));
        let plain = SiteServerOptions {
            last_modified: Some(lm.clone()),
            ..Default::default()
        };
        let opts = SiteServerOptions {
            content: lazy.content.clone(),
            last_modified: Some(lm),
            ..Default::default()
        };
        let none = HashMap::new();
        // Byte ranges and Last-Modified / If-Modified-Since work on entries read from the zip, as on extracted ones.
        let variants: Vec<HashMap<String, String>> = [
            vec![],
            vec![("range", "bytes=0-9")],
            vec![("range", "bytes=5-")],
            vec![("range", "bytes=9999-")],
            vec![("range", "bytes=0-9"), ("accept-encoding", "gzip")],
            vec![("if-modified-since", "Tue, 14 Nov 2023 22:13:20 GMT")],
            vec![("if-modified-since", "Mon, 01 Jan 2001 00:00:00 GMT")],
        ]
        .into_iter()
        .map(|v| {
            v.into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .collect();
        for method in ["GET", "HEAD"] {
            for h in &variants {
                for p in ["/", "/index.html", "/app.js", "/tiny.txt", "/nope"] {
                    let want = handle_request(&eager, &plain, method, p, h);
                    let got = handle_request(&lazy.tree, &opts, method, p, h);
                    assert_eq!(
                        (got.status, &got.headers, &got.body),
                        (want.status, &want.headers, &want.body),
                        "{method} {p} {h:?}"
                    );
                }
            }
        }
        for method in ["GET", "HEAD"] {
            for p in [
                "/",
                "/index.html",
                "/about",
                "/about/",
                "/old",
                "/rewrite",
                "/app.js",
                "/tiny.txt",
                "/nope",
                "/.env",
                "/_headers",
            ] {
                let want = handle_request(&eager, &plain, method, p, &none);
                let got = handle_request(&lazy.tree, &opts, method, p, &none);
                assert_eq!(
                    (got.status, &got.headers, &got.body),
                    (want.status, &want.headers, &want.body),
                    "{method} {p}"
                );
            }
        }
        // Gzip clients get deflated entries as gzip, with their own ETag.
        let gz: HashMap<String, String> =
            [("accept-encoding".to_string(), "br, gzip;q=0.8".to_string())].into();
        let p = handle_request(&lazy.tree, &opts, "GET", "/app.js", &none);
        let g = handle_request(&lazy.tree, &opts, "GET", "/app.js", &gz);
        assert_eq!(g.header("content-encoding"), Some("gzip"));
        assert_eq!(g.header("vary"), Some("accept-encoding"));
        assert_eq!(
            g.header("etag").unwrap(),
            format!("{}-gzip\"", p.header("etag").unwrap().trim_end_matches('"'))
        );
        let mut un = Vec::new();
        use std::io::Read;
        flate2::read::GzDecoder::new(&g.body[..])
            .read_to_end(&mut un)
            .unwrap();
        assert_eq!(un, p.body);
        let stored = handle_request(&lazy.tree, &opts, "GET", "/tiny.txt", &gz);
        assert_eq!(stored.header("content-encoding"), None);
        let q0: HashMap<String, String> =
            [("accept-encoding".to_string(), "gzip;q=0".to_string())].into();
        assert_eq!(
            handle_request(&lazy.tree, &opts, "GET", "/app.js", &q0).header("content-encoding"),
            None
        );
    }

    #[test]
    fn a_zip_changed_after_start_is_a_500() {
        use crate::export::archive_writers::{write_zip, WriterOptions};
        use crate::server::handle_request;
        let d = TmpDir::new("servers");
        let file = d.path().join("site.zip");
        let mut bytes = write_zip(&zip_site(), &WriterOptions::default()).unwrap();
        fs::write(&file, &bytes).unwrap();
        let lazy = open_zip_archive(
            &file,
            &DEFAULT_ARCHIVE_LIMITS,
            &ServedTreeOptions::default(),
        )
        .unwrap()
        .unwrap();
        let at = data_of(&bytes, "tiny.txt");
        assert_eq!(bytes[at], b'x');
        bytes[at] = b'y';
        fs::write(&file, &bytes).unwrap();
        let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        let opts = SiteServerOptions {
            content: lazy.content.clone(),
            log: Some(Arc::new(move |m: &str| {
                l2.lock().unwrap().push(m.to_string())
            })),
            ..Default::default()
        };
        let r = handle_request(&lazy.tree, &opts, "GET", "/tiny.txt", &HashMap::new());
        assert_eq!(r.status, 500);
        assert!(lines.lock().unwrap()[0]
            .contains("zip entry 'tiny.txt' changed since the server started"));
        // A corrupt zip fails to open with the extraction error; other archives are not opened in place.
        let e = open_zip_archive(
            &file,
            &DEFAULT_ARCHIVE_LIMITS,
            &ServedTreeOptions::default(),
        )
        .err()
        .unwrap();
        assert!(
            e.message()
                .contains("zip entry 'tiny.txt' fails its CRC-32 check"),
            "{}",
            e.message()
        );
        let tar = d.path().join("x.tar");
        fs::write(&tar, vec![0u8; 1024]).unwrap();
        assert!(
            open_zip_archive(&tar, &DEFAULT_ARCHIVE_LIMITS, &ServedTreeOptions::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn archive_files_reread_on_every_load() {
        let d = TmpDir::new("servers");
        let store = Store::open(d.path().join("home")).unwrap();
        let file = d.path().join("site.tar");
        let v = |c: &str| {
            Tree::from_files([(
                "/index.html".to_string(),
                make_file(c.as_bytes().to_vec(), "t", 0o644),
            )])
            .unwrap()
        };
        let tar = |t: &Tree| crate::export::archive_writers::write_tar(t, &Default::default());
        fs::write(&file, tar(&v("one")).unwrap()).unwrap();
        let (_, source) = resolve_source(&store, &file.display().to_string()).unwrap();
        let get = || {
            load_served_tree(&store, &source)
                .unwrap()
                .get("/index.html")
                .unwrap()
                .bytes
                .to_vec()
        };
        assert_eq!(get(), b"one");
        fs::write(&file, tar(&v("two")).unwrap()).unwrap();
        assert_eq!(get(), b"two");
    }

    #[test]
    fn archive_errors_name_the_file() {
        let d = TmpDir::new("servers");
        let store = Store::open(d.path().join("home")).unwrap();
        let notes = d.path().join("notes.txt");
        fs::write(&notes, "just text").unwrap();
        // A regular file resolves as an archive; loading it says what it is not.
        let (_, source) = resolve_source(&store, &notes.display().to_string()).unwrap();
        let e = load_served_tree(&store, &source).unwrap_err();
        assert_eq!(e.code, E_TYPE);
        assert_eq!(
            e.message(),
            format!(
                "{}: not a recognized archive (expected tar, tar.gz, zip or web bundle)",
                fs::canonicalize(&notes).unwrap().display()
            )
        );
        let gone = d.path().join("gone.zip");
        let e = load_archive_tree(&gone).unwrap_err();
        assert_eq!(
            (e.code, e.message()),
            (E_NOT_FOUND, format!("'{}' does not exist", gone.display()))
        );
        let e = load_archive_tree(d.path()).unwrap_err();
        assert_eq!(
            (e.code, e.message()),
            (E_TYPE, format!("'{}' is not a file", d.path().display()))
        );
        let e = resolve_source(&store, &gone.display().to_string()).unwrap_err();
        assert_eq!(
            (e.code, e.message()),
            (
                E_NOT_FOUND,
                format!("'{}' is not a directory or a file", gone.display())
            )
        );
    }

    #[test]
    fn archive_limits_are_enforced() {
        let d = TmpDir::new("servers");
        let files = write_archives(d.path());
        let few = ArchiveLimits {
            max_entries: 2,
            ..DEFAULT_ARCHIVE_LIMITS
        };
        let small = ArchiveLimits {
            max_total_bytes: 3,
            ..DEFAULT_ARCHIVE_LIMITS
        };
        for (name, file) in &files {
            for limits in [&few, &small] {
                let e = load_archive_tree_with(file, limits).unwrap_err();
                assert_eq!(e.code, E_ARCHIVE_LIMIT, "{name}: {}", e.message());
            }
            assert!(load_archive_tree_with(file, &DEFAULT_ARCHIVE_LIMITS).is_ok());
        }
        // A file larger than any archive within the limits is refused before it is read.
        let tiny = ArchiveLimits {
            max_total_bytes: 0,
            max_entries: 0,
            max_ratio: 100,
        };
        let big = d.path().join("big.bin");
        fs::write(&big, vec![b'x'; 5000]).unwrap();
        let e = load_archive_tree_with(&big, &tiny).unwrap_err();
        assert_eq!(
            (e.code, e.message()),
            (
                E_ARCHIVE_LIMIT,
                format!("{}: archive expands beyond 0 bytes", big.display())
            )
        );
        // The compression ratio holds for a gzip bomb.
        let bomb = Tree::from_files([(
            "/zeros".to_string(),
            make_file(vec![0u8; 8 << 20], "t", 0o644),
        )])
        .unwrap();
        let tgz = d.path().join("bomb.tgz");
        fs::write(
            &tgz,
            crate::export::archive_writers::write_tgz(&bomb, &Default::default()).unwrap(),
        )
        .unwrap();
        let e = load_archive_tree(&tgz).unwrap_err();
        assert_eq!(e.code, E_ARCHIVE_LIMIT);
        assert!(
            e.message().contains("compression ratio exceeds 100:1"),
            "{}",
            e.message()
        );
    }

    #[test]
    fn oci_archives_check_every_blob() {
        use crate::export::archive_writers::write_tar;
        use crate::export::oci::{build_oci_image, layout_tree, OciOptions};
        let d = TmpDir::new("servers");
        let img = build_oci_image(&site_tree(), &OciOptions::default()).unwrap();
        let layout = layout_tree(img.parts(), None, &[], &[]).unwrap();
        let write = |name: &str, t: &Tree| {
            let p = d.path().join(name);
            fs::write(&p, write_tar(t, &Default::default()).unwrap()).unwrap();
            p
        };
        let with = |path: &str, bytes: &[u8]| {
            let mut files: Vec<(String, crate::tree::Node)> = layout
                .entries()
                .filter(|(p, _)| p.as_str() != path)
                .map(|(p, n)| (p.clone(), n.clone()))
                .collect();
            if !bytes.is_empty() {
                files.push((path.to_string(), make_file(bytes.to_vec(), "t", 0o644)));
            }
            Tree::from_files(files).unwrap()
        };
        let config = blob_path_of(&img.config_digest).unwrap();
        let layer = blob_path_of(&img.layer_digest).unwrap();
        let cases = [
            (
                "tampered",
                with(&config, b"{}"),
                "does not match its digest",
            ),
            ("no-layer", with(&layer, b""), "OCI archive has no blob"),
            (
                "no-index",
                with("/index.json", b""),
                "OCI archive has no index.json",
            ),
            (
                "bad-index",
                with("/index.json", b"[]"),
                "OCI archive index.json is corrupt",
            ),
        ];
        for (name, t, why) in cases {
            let p = write(&format!("{name}.oci.tar"), &t);
            let e = load_archive_tree(&p).unwrap_err();
            assert_eq!(e.code, E_ARCHIVE, "{name}");
            assert!(
                e.message().starts_with(&format!("{}: ", p.display())) && e.message().contains(why),
                "{name}: {}",
                e.message()
            );
        }
        let ok = write("ok.oci.tar", &layout);
        assert_eq!(files(&load_archive_tree(&ok).unwrap()), files(&site_tree()));
    }

    #[test]
    fn archive_records_round_trip_and_old_records_still_load() {
        let d = TmpDir::new("servers");
        let reg = ServerRegistry::open(Some(Store::open(d.path()).unwrap())).unwrap();
        let mut s = state(&reg, "0123456789ab", "zip", "1");
        s.source = ServerSource::Archive {
            path: "/x/site.zip".into(),
        };
        reg.write(&s).unwrap();
        let text = fs::read_to_string(reg.state_path(&s.id).unwrap()).unwrap();
        assert!(text.contains(
            "\"source\": {\n    \"kind\": \"archive\",\n    \"path\": \"/x/site.zip\"\n  }"
        ));
        assert_eq!(reg.read(&s.id).unwrap().unwrap(), s);
        for (json, want) in [
            (
                r#"{"kind":"dir","path":"/d"}"#,
                ServerSource::Dir { path: "/d".into() },
            ),
            (
                r#"{"kind":"image","imageId":"sha256:0"}"#,
                ServerSource::Image {
                    image_id: "sha256:0".into(),
                },
            ),
        ] {
            assert_eq!(serde_json::from_str::<ServerSource>(json).unwrap(), want);
        }
    }
}
