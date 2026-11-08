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
//! Embedders (a long-running host such as sitefile Desktop) start, restart and
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

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::exporter::random_hex;
use crate::resources::local::{LocalContext, LocalContextOptions};
use crate::resources::types::Resource;
use crate::store::Store;
use crate::tree::Tree;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bind {
    pub host: String,
    pub port: u16,
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
                url_of(&s.host, s.port)
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

/// The tree a server state serves.
pub fn load_served_tree(store: &Store, source: &ServerSource) -> Result<Tree> {
    match source {
        ServerSource::Image { image_id } => store.load_tree(image_id),
        ServerSource::Dir { path } => load_directory_tree(Path::new(path)),
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

/// What `sitefile serve <arg>` serves: a directory when `arg` looks like one
/// (`./site`, `../x`, `/abs`, `~/x`, `dir/`), else an image reference or ID in
/// `store`. Returns the label (what the user asked for, shown and stored as
/// `ServerState::image`) and the source. A bare name that is not an image but
/// is a directory gets a hint to write `./name`.
pub fn resolve_source(store: &Store, arg: &str) -> Result<(String, ServerSource)> {
    if looks_like_dir(arg) {
        let dir = crate::export::exporter::resolve_path(Path::new(arg));
        if !fs::metadata(&dir).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(SitefileError::plain(
                E_NOT_FOUND,
                format!("'{arg}' is not a directory"),
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
            Err(e)
        }
    }
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
}

impl DetachedServe {
    /// Serve `arg` (an image reference or ID, or a directory, as `serve` takes it)
    /// on 127.0.0.1 with a free port, unnamed, not as an SPA.
    pub fn resolve(store: &Store, arg: &str) -> Result<DetachedServe> {
        let (label, source) = resolve_source(store, arg)?;
        Ok(DetachedServe {
            label,
            source,
            name: None,
            bind: parse_publish(None)?,
            spa: false,
        })
    }
}

/// How long [`start_detached`] waits for the worker to listen.
pub const START_TIMEOUT: Duration = Duration::from_secs(15);

/// `serve -d`: check that the image or directory loads, allocate an ID and
/// name, record the server, start its worker and wait until it listens.
/// Returns the running server's state (its real host and port). On failure
/// the record is removed. `worker_exe` runs as `<exe> __serve-worker <id>`
/// (`None`: the current executable); the worker is reaped (see the module docs).
pub fn serve_detached(
    reg: &ServerRegistry,
    serve: &DetachedServe,
    worker_exe: Option<&Path>,
) -> Result<ServerState> {
    load_served_tree(&reg.store, &serve.source)?; // fail fast on a bad image or directory
    let (id, final_name) = reg.allocate(serve.name.as_deref())?;
    let state = ServerState {
        id: id.clone(),
        name: final_name,
        image: serve.label.clone(),
        source: serve.source.clone(),
        spa: serve.spa,
        host: serve.bind.host.clone(),
        port: serve.bind.port,
        bind: serve.bind.clone(),
        pid: 0,
        started: crate::store::now_iso(),
        log_file: reg.log_path(&id).display().to_string(),
        ready: false,
        error: None,
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
/// address (an image by the same ID; a directory is re-read). On failure the
/// record stays, marked not ready.
pub fn restart_detached(
    reg: &ServerRegistry,
    s: &ServerState,
    grace: Duration,
    worker_exe: Option<&Path>,
) -> Result<ServerState> {
    stop_server(reg, s, grace)?;
    load_served_tree(&reg.store, &s.source)?;
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
}
