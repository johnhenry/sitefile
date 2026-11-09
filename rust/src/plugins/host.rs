//! Port of `src/plugins/host.ts`.
//!
//! Plugins run one Node process each, under Node's permission model:
//!
//! ```text
//! node --permission --allow-fs-read=<plugin folder> --max-old-space-size=<MiB> --input-type=module -e <runner>
//! ```
//!
//! so a plugin can read files in its own folder and nothing else: no writes,
//! child processes, workers, native addons, WASI or network, and an empty
//! environment (no secrets). Requests and results travel over stdin/stdout as
//! JSON lines; the runner below is the same string as in host.ts, so both
//! implementations speak one protocol.
//!
//! Node documents the permission model as a "seat belt" for trusted code, not a
//! defence against deliberately malicious code: plugins are code you choose to
//! run. Pin them with INTEGRITY.
//!
//! The sandbox is only as strong as the Node that runs it: `node` on PATH by
//! default, or the one named by `--node` / `SITEFILE_NODE` / `BuildOptions::node`.
//! Its version is checked (at least [`MIN_NODE`]) before the first plugin starts,
//! and reported through the notice callback.
//!
//! Each process is driven with plain threads and channels: a writer thread feeds
//! stdin (so a plugin that stops reading cannot block the build), a reader thread
//! splits stdout into protocol messages and log lines, and another collects
//! stderr. Calls wait on the channel with the per-call deadline.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::error::{ErrorCode, Result, SitefileError, SourceSpan};
use crate::plugins::cache::js_json;
use crate::plugins::wasm::WasmPlugin;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginPolicy {
    /// Per-call time limit (default 60 s); the process is killed when it passes.
    pub timeout_ms: u64,
    /// Largest result accepted from one call (default 64 MiB).
    pub max_output_bytes: usize,
    /// Memory limit in MiB (default 1024, `--plugin-memory`): a JavaScript plugin's heap
    /// (`node --max-old-space-size`) and a wasm plugin's linear memory.
    pub max_memory_mib: usize,
}

pub const DEFAULT_PLUGIN_POLICY: PluginPolicy = PluginPolicy {
    timeout_ms: 60_000,
    max_output_bytes: 64 * 1024 * 1024,
    max_memory_mib: 1024,
};

/// What V8 prints when a heap passes --max-old-space-size (followed by a native stack trace).
const OUT_OF_MEMORY: &str = "JavaScript heap out of memory";

/// V8's out-of-memory report starts with one of these; it and everything after it is not logged.
fn is_fatal_report(line: &str) -> bool {
    line.starts_with("<--- Last few GCs --->") || line.starts_with("FATAL ERROR:")
}

impl Default for PluginPolicy {
    fn default() -> Self {
        DEFAULT_PLUGIN_POLICY
    }
}

/// The oldest Node that JavaScript plugins run on. Node 25 added network permissions
/// (before it, `--permission` did not block sockets); 26 is the release sitefile
/// requires and tests with (package.json `engines`).
pub const MIN_NODE: [u64; 3] = [26, 0, 0];

/// The Node binary JavaScript plugins run on, as checked when the first one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRuntime {
    /// Absolute real path.
    pub path: PathBuf,
    /// `process.version` of that binary, e.g. `v26.11.1`.
    pub version: String,
}

/// `vX.Y.Z` -> [X, Y, Z].
pub fn parse_node_version(v: &str) -> Option<[u64; 3]> {
    let rest = v.trim().strip_prefix('v')?;
    let mut it = rest.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let part = it.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if it.next().is_some() {
        return None;
    }
    Some(out)
}

/// Resolve and check the Node binary for JavaScript plugins (`node`: an absolute path; default
/// `node` on PATH). The error text is the part after `PLUGIN name: `.
pub fn check_node(
    node: Option<&Path>,
    timeout_ms: u64,
) -> std::result::Result<NodeRuntime, String> {
    let choose = "choose another with --node or SITEFILE_NODE";
    let given = match node {
        Some(n) => n.to_path_buf(),
        None => find_on_path("node")
            .ok_or_else(|| "could not start: node was not found on PATH".to_string())?,
    };
    if !given.is_absolute() {
        return Err(format!(
            "the Node binary must be an absolute path, got '{}'",
            given.display()
        ));
    }
    let real = crate::platform::canonicalize(&given)
        .ok()
        .filter(|r| r.is_file())
        .ok_or_else(|| format!("could not start: no Node binary at {}", given.display()))?;
    let no_version = || {
        format!(
            "could not start: {} --version did not print a Node version; {choose}",
            real.display()
        )
    };
    let mut child = Command::new(&real)
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| no_version())?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(no_version());
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    let parsed = parse_node_version(&out).ok_or_else(no_version)?;
    let version = out.trim().to_string();
    if parsed < MIN_NODE {
        return Err(format!(
            "node {version} ({}) is too old for the plugin sandbox: JavaScript plugins need Node {}.{}.{} or later; {choose}",
            real.display(),
            MIN_NODE[0],
            MIN_NODE[1],
            MIN_NODE[2]
        ));
    }
    Ok(NodeRuntime {
        path: real,
        version,
    })
}

/// The file a plugin receives (`request.input`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginInput {
    pub path: String,
    pub name: String,
    pub bytes: Vec<u8>,
    /// The bytes decoded as UTF-8, when they are valid UTF-8.
    pub text: Option<String>,
}

impl PluginInput {
    /// `text` is filled like the TypeScript executor does: a strict UTF-8 decode
    /// (TextDecoder, which drops a leading BOM), or nothing.
    pub fn new(path: impl Into<String>, name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let text = std::str::from_utf8(&bytes)
            .ok()
            .map(|t| t.strip_prefix('\u{feff}').unwrap_or(t).to_string());
        PluginInput {
            path: path.into(),
            name: name.into(),
            bytes,
            text,
        }
    }
}

/// What a plugin receives (see docs/PLUGINS.md). `apiVersion` is always 1.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginRequest {
    pub dest: String,
    /// In OPTIONS order.
    pub options: Vec<(String, String)>,
    pub input: Option<PluginInput>,
    /// Set for a value-import source (`TRANSFORM name: …`).
    pub data: Option<Value>,
    /// Set for `FOR x IN list`.
    pub item: Option<Value>,
}

impl PluginRequest {
    /// The wire form: `{apiVersion, dest, options, input?: {path, name, bytes: <base64>, text?}, data?, item?}`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("apiVersion".into(), json!(1));
        m.insert("dest".into(), Value::String(self.dest.clone()));
        let mut opts = Map::new();
        for (k, v) in &self.options {
            opts.insert(k.clone(), Value::String(v.clone()));
        }
        m.insert("options".into(), Value::Object(opts));
        if let Some(i) = &self.input {
            let mut im = Map::new();
            im.insert("path".into(), Value::String(i.path.clone()));
            im.insert("name".into(), Value::String(i.name.clone()));
            im.insert(
                "bytes".into(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(&i.bytes)),
            );
            if let Some(t) = &i.text {
                im.insert("text".into(), Value::String(t.clone()));
            }
            m.insert("input".into(), Value::Object(im));
        }
        if let Some(d) = &self.data {
            m.insert("data".into(), d.clone());
        }
        if let Some(it) = &self.item {
            m.insert("item".into(), it.clone());
        }
        Value::Object(m)
    }

    /// `JSON.stringify` of [`to_json`](Self::to_json), byte-identical to the TypeScript host.
    pub fn to_wire_string(&self) -> String {
        js_json(&self.to_json())
    }
}

/// A plugin's result, normalized: one file, or several under the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginResult {
    File(Vec<u8>),
    Files(Vec<(String, Vec<u8>)>),
}

/// A directory TRANSFORM runs at most this many wasm or tool calls at a time (a JS plugin, one).
pub fn transform_jobs() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Run `f(0..n)` at most `limit` at a time, starting them in index order. After a failure no
/// later index starts, so the lowest failing index is always among the results (and every
/// index below it has run): callers report that one, whatever order the calls finished in.
pub fn run_bounded<R: Send>(
    n: usize,
    limit: usize,
    f: impl Fn(usize) -> Result<R> + Sync,
) -> Vec<Option<Result<R>>> {
    let out: Vec<Mutex<Option<Result<R>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed_at = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let lane = || loop {
        let i = next.fetch_add(1, Ordering::SeqCst);
        if i >= n || i > failed_at.load(Ordering::SeqCst) {
            return;
        }
        let r = f(i);
        if r.is_err() {
            failed_at.fetch_min(i, Ordering::SeqCst);
        }
        *out[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    };
    let lanes = limit.max(1).min(n);
    if lanes <= 1 {
        lane();
    } else {
        thread::scope(|s| {
            for _ in 0..lanes {
                s.spawn(&lane);
            }
        });
    }
    out.into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

/// Counters a build reports (BuildResult.transformStats).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostStats {
    /// Plugin processes (or wasm runners) started.
    pub spawned: u64,
    /// transform() calls sent to a plugin.
    pub calls: u64,
}

/// `Buffer.from(s, 'base64')`: skips characters outside the alphabet (both the
/// standard and URL-safe ones), stops at `=`, tolerates missing padding.
pub fn b64_decode_lenient(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Object keys in the order JavaScript's `Object.entries` yields them.
pub(crate) fn js_keys(m: &Map<String, Value>) -> Vec<&String> {
    crate::plugins::cache::js_key_order(m, false)
}

/// Prefix that marks protocol lines on the child's stdout; anything else is plugin output.
const MARK: &str = "\u{0}SITEFILE:";

/// Runs inside the sandboxed process (passed with -e, so it needs no file access).
/// Verbatim copy of `RUNNER` in src/plugins/host.ts (a test checks they match).
pub const RUNNER: &str = r#"
const MARK = '\u0000SITEFILE:';
const send = (msg) => process.stdout.write(MARK + JSON.stringify(msg) + '\n');
// Plugin console output goes to stdout as unmarked lines, so it reaches the host before
// the result that follows it (stderr is another pipe, with no order against stdout).
for (const k of ['log', 'info', 'debug']) console[k] = (...a) => process.stdout.write(a.map(String).join(' ').split('\n').filter((l) => !l.startsWith(MARK)).join('\n') + '\n');
const toB64 = (v) => Buffer.from(typeof v === 'string' ? Buffer.from(v, 'utf8') : v).toString('base64');
const isBytes = (v) => typeof v === 'string' || v instanceof Uint8Array;
let transform;
let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf('\n')) !== -1) {
    const line = buf.slice(0, nl);
    buf = buf.slice(nl + 1);
    handle(JSON.parse(line)).catch((e) => send({ type: 'fatal', message: String(e && e.stack || e) }));
  }
});
async function handle(msg) {
  if (msg.type === 'load') {
    try {
      const mod = await import(msg.url);
      const api = mod.default && typeof mod.default.transform === 'function' ? mod.default : mod;
      const version = api.apiVersion ?? mod.apiVersion ?? 1;
      if (typeof api.transform !== 'function') throw new Error('the module must export transform(request), or a default object with transform');
      if (version !== 1) throw new Error('unsupported plugin apiVersion ' + JSON.stringify(version) + ' (this Sitefile supports 1)');
      transform = api.transform;
      // 'export const cache = false' opts a (non-deterministic) plugin out of the result cache;
      // 'cache = true' says it is deterministic. Without either it is cached, with a warning.
      const cache = api.cache ?? mod.cache;
      if (cache !== undefined && typeof cache !== 'boolean') throw new Error('cache must be true or false, got ' + JSON.stringify(cache));
      send({ type: 'ready', cache: cache !== false, declared: cache !== undefined });
    } catch (e) {
      send({ type: 'load-error', message: e && e.message ? e.message : String(e) });
    }
    return;
  }
  if (msg.type === 'transform') {
    const req = msg.request;
    if (req.input) {
      req.input.bytes = new Uint8Array(Buffer.from(req.input.bytes, 'base64'));
    }
    try {
      let out = await transform(req);
      if (isBytes(out)) out = { bytes: out };
      if (out && isBytes(out.bytes)) return send({ type: 'result', id: msg.id, bytes: toB64(out.bytes) });
      if (out && out.files && typeof out.files === 'object') {
        const files = {};
        for (const [k, v] of Object.entries(out.files)) {
          if (!isBytes(v)) throw new Error('files[' + JSON.stringify(k) + '] must be a string or Uint8Array');
          files[k] = toB64(v);
        }
        return send({ type: 'result', id: msg.id, files });
      }
      throw new Error('transform() must return a string, a Uint8Array, { bytes } or { files }');
    } catch (e) {
      send({ type: 'error', id: msg.id, message: e && e.message ? e.message : String(e) });
    }
  }
}
"#;

type Log = Arc<dyn Fn(&str) + Send + Sync>;

enum Msg {
    /// A protocol message (MARK line).
    Proto(Value),
    /// The reader stopped for a reason of its own (oversized or malformed output).
    Fail(String),
    /// stdout closed.
    Eof,
}

enum LoadState {
    Pending,
    Ready,
    Failed(SitefileError),
}

struct PluginProcess {
    name: String,
    child: Option<Child>,
    stdin: Option<Sender<Vec<u8>>>,
    rx: Receiver<Msg>,
    stderr: Arc<Mutex<String>>,
    stderr_done: Arc<AtomicBool>,
    /// V8 reported that the heap passed the memory limit.
    out_of_memory: Arc<AtomicBool>,
    next_id: u64,
    /// False when the module exports `cache = false`; known once it has loaded.
    cacheable: bool,
    /// Whether the module exports `cache` at all; known once it has loaded.
    declared_cache: bool,
    load: LoadState,
    exited: Option<String>,
    policy: PluginPolicy,
    span: Option<SourceSpan>,
}

/// Percent-encode a path for a `file://` URL.
fn file_url(file: &Path) -> String {
    let mut s = String::from("file://");
    for b in file.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    let n = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 if cfg!(target_os = "linux") => "SIGUSR1",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("signal {sig}"),
    };
    n.into()
}

/// "stopped by SIGKILL" / "exited with code 3", like the Node 'exit' event.
pub(crate) fn exit_reason(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("stopped by {}", signal_name(sig));
        }
    }
    match status.code() {
        Some(c) => format!("exited with code {c}"),
        None => "exited".into(),
    }
}

impl PluginProcess {
    fn start(
        name: &str,
        file: &Path,
        node: &Path,
        policy: PluginPolicy,
        log: Log,
        span: Option<SourceSpan>,
    ) -> PluginProcess {
        let (tx, rx) = mpsc::channel::<Msg>();
        let mut p = PluginProcess {
            name: name.into(),
            child: None,
            stdin: None,
            rx,
            stderr: Arc::new(Mutex::new(String::new())),
            stderr_done: Arc::new(AtomicBool::new(true)),
            out_of_memory: Arc::new(AtomicBool::new(false)),
            next_id: 1,
            cacheable: true,
            declared_cache: true,
            load: LoadState::Pending,
            exited: None,
            policy,
            span,
        };
        let dir = file.parent().unwrap_or(Path::new("/")).to_path_buf();
        let spawned = Command::new(node)
            .arg("--permission")
            .arg(format!("--allow-fs-read={}", dir.display()))
            .arg(format!("--max-old-space-size={}", policy.max_memory_mib))
            .args([
                "--disable-warning=ExperimentalWarning",
                "--input-type=module",
                "-e",
                RUNNER,
            ])
            .current_dir(&dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                p.fail(&format!("could not start: {e}"));
                return p;
            }
        };

        // stdin writer
        let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();
        let mut stdin = child.stdin.take().expect("piped");
        thread::spawn(move || {
            for chunk in in_rx {
                if stdin.write_all(&chunk).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
            // dropping stdin closes the pipe
        });

        // stdout reader: protocol messages and log lines
        let mut stdout = child.stdout.take().expect("piped");
        let limit = (policy.max_output_bytes as f64 * 1.4) as usize + 1024;
        let max = policy.max_output_bytes;
        let out_log = log.clone();
        thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let n = match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let scan_from = buf.len();
                buf.extend_from_slice(&chunk[..n]);
                let mut start = 0;
                let mut i = scan_from;
                while let Some(off) = buf[i..].iter().position(|b| *b == b'\n') {
                    let end = i + off;
                    let line = &buf[start..end];
                    if line.starts_with(MARK.as_bytes()) {
                        if line.len() > limit {
                            let _ =
                                tx.send(Msg::Fail(format!("a result is larger than {max} bytes")));
                            return;
                        }
                        match serde_json::from_slice::<Value>(&line[MARK.len()..]) {
                            Ok(v) => {
                                if tx.send(Msg::Proto(v)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Fail(format!(
                                    "sent a malformed protocol message ({e})"
                                )));
                                return;
                            }
                        }
                    } else {
                        let l = String::from_utf8_lossy(line);
                        if !l.trim().is_empty() {
                            out_log(&l);
                        }
                    }
                    start = end + 1;
                    i = start;
                }
                buf.drain(..start);
                if buf.len() > limit {
                    let _ = tx.send(Msg::Fail(format!("a result is larger than {max} bytes")));
                    return;
                }
            }
            let _ = tx.send(Msg::Eof);
        });

        // stderr: log lines, keep a tail for error messages
        let mut stderr = child.stderr.take().expect("piped");
        let tail = p.stderr.clone();
        let done = Arc::new(AtomicBool::new(false));
        p.stderr_done = done.clone();
        let oom = p.out_of_memory.clone();
        thread::spawn(move || {
            let mut pending: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 16 * 1024];
            // Set once V8's fatal error report starts: the rest is a native stack trace, not logged.
            let mut quiet = false;
            let mut flush = |line: &[u8]| {
                let l = String::from_utf8_lossy(line);
                if is_fatal_report(&l) {
                    quiet = true;
                }
                if !l.trim().is_empty() && !quiet {
                    log(&l);
                }
            };
            loop {
                let n = match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                {
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if t.len() > 8192 {
                        let mut cut = t.len() - 8192;
                        while !t.is_char_boundary(cut) {
                            cut += 1;
                        }
                        t.drain(..cut);
                    }
                    if t.contains(OUT_OF_MEMORY) {
                        oom.store(true, Ordering::SeqCst);
                    }
                }
                pending.extend_from_slice(&chunk[..n]);
                while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                    flush(&pending[..nl]);
                    pending.drain(..=nl);
                }
                if pending.len() > 64 * 1024 {
                    flush(&pending);
                    pending.clear();
                }
            }
            if !pending.is_empty() {
                flush(&pending);
            }
            done.store(true, Ordering::SeqCst);
        });

        p.child = Some(child);
        p.stdin = Some(in_tx);
        p.write(&json!({ "type": "load", "url": file_url(file) }));
        p
    }

    fn write(&self, msg: &Value) {
        if let Some(tx) = &self.stdin {
            let mut line = js_json(msg).into_bytes();
            line.push(b'\n');
            let _ = tx.send(line);
        }
    }

    fn err(&self, detail: String, span: Option<&SourceSpan>) -> SitefileError {
        SitefileError::new(ErrorCode::E_PLUGIN, detail, span.cloned())
    }

    /// Stop the process for `reason`; returns the message for pending callers (reason + stderr tail).
    fn fail(&mut self, reason: &str) -> String {
        let first = self.exited.is_none();
        let out_of_memory = first && self.out_of_memory.load(Ordering::SeqCst);
        let reason = match &self.exited {
            Some(r) => r.clone(),
            // The stderr tail would be V8's native stack trace: machine-specific, and no help.
            None if out_of_memory => {
                let r = format!(
                    "ran out of memory (more than the {} MiB limit)",
                    self.policy.max_memory_mib
                );
                self.stderr
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                self.exited = Some(r.clone());
                r
            }
            None => {
                self.exited = Some(reason.to_string());
                reason.to_string()
            }
        };
        self.kill();
        let tail = {
            let t = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
            let t = t.trim();
            if t.is_empty() {
                String::new()
            } else {
                let lines: Vec<&str> = t.split('\n').collect();
                format!("\n{}", lines[lines.len().saturating_sub(5)..].join("\n"))
            }
        };
        let message = format!("{reason}{tail}");
        if matches!(self.load, LoadState::Pending) {
            self.load = LoadState::Failed(self.err(
                format!("PLUGIN {}: {message}", self.name),
                self.span.as_ref(),
            ));
        }
        message
    }

    fn kill(&mut self) {
        self.stdin = None;
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// stdout closed: wait for the exit status and the rest of stderr, then fail with the reason.
    fn on_eof(&mut self) -> String {
        let mut reason = "exited".to_string();
        if let Some(c) = &mut self.child {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                match c.try_wait() {
                    Ok(Some(st)) => {
                        reason = exit_reason(st);
                        break;
                    }
                    Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(5)),
                    _ => {
                        reason = "closed its output".into();
                        break;
                    }
                }
            }
        }
        let until = Instant::now() + Duration::from_millis(500);
        while !self.stderr_done.load(Ordering::SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
        }
        self.fail(&reason)
    }

    /// Next message, or the reason the process can't deliver one by `deadline`.
    fn next(&mut self, deadline: Instant) -> std::result::Result<Value, String> {
        let wait = deadline.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(wait) {
            Ok(Msg::Proto(v)) => Ok(v),
            Ok(Msg::Fail(r)) => Err(self.fail(&r)),
            Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => Err(self.on_eof()),
            Err(RecvTimeoutError::Timeout) => {
                Err(self.fail(&format!("timed out after {} ms", self.policy.timeout_ms)))
            }
        }
    }

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.policy.timeout_ms);
        loop {
            match &self.load {
                LoadState::Ready => return Ok(()),
                LoadState::Failed(e) => return Err(e.clone()),
                LoadState::Pending => {}
            }
            // On Err, fail() has already recorded the load error.
            if let Ok(msg) = self.next(deadline) {
                match msg.get("type").and_then(Value::as_str) {
                    Some("ready") => {
                        self.cacheable = msg.get("cache") != Some(&Value::Bool(false));
                        self.declared_cache = msg.get("declared") == Some(&Value::Bool(true));
                        self.load = LoadState::Ready;
                    }
                    Some("load-error") => {
                        let m = message_of(&msg);
                        self.load = LoadState::Failed(
                            self.err(format!("PLUGIN {}: {m}", self.name), self.span.as_ref()),
                        );
                    }
                    Some("fatal") => {
                        let m = message_of(&msg);
                        self.fail(&m);
                    }
                    _ => {}
                }
            }
        }
    }

    fn call(&mut self, request: &PluginRequest, span: Option<&SourceSpan>) -> Result<PluginResult> {
        self.wait_ready()?;
        if let Some(x) = &self.exited {
            return Err(self.err(format!("PLUGIN {} {x}", self.name), span));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({ "type": "transform", "id": id, "request": request.to_json() }));
        let deadline = Instant::now() + Duration::from_millis(self.policy.timeout_ms);
        let name = self.name.clone();
        let reject = |m: String| {
            SitefileError::new(
                ErrorCode::E_PLUGIN,
                format!("PLUGIN {name}: {m}"),
                span.cloned(),
            )
        };
        loop {
            let msg = self.next(deadline).map_err(reject)?;
            match msg.get("type").and_then(Value::as_str) {
                Some("fatal") => {
                    let m = message_of(&msg);
                    return Err(reject(self.fail(&m)));
                }
                Some("error") | Some("result")
                    if msg.get("id").and_then(Value::as_u64) == Some(id) =>
                {
                    if msg["type"] == "error" {
                        return Err(reject(message_of(&msg)));
                    }
                    return self.decode_result(&msg).map_err(reject);
                }
                _ => {} // ready / load-error after load, or a stale id
            }
        }
    }

    fn decode_result(&self, msg: &Value) -> std::result::Result<PluginResult, String> {
        let max = self.policy.max_output_bytes;
        let decode = |v: &Value| -> std::result::Result<Vec<u8>, String> {
            let bytes = b64_decode_lenient(v.as_str().unwrap_or(""));
            if bytes.len() > max {
                return Err(format!("result is larger than {max} bytes"));
            }
            Ok(bytes)
        };
        if let Some(b @ Value::String(_)) = msg.get("bytes") {
            return Ok(PluginResult::File(decode(b)?));
        }
        match msg.get("files") {
            Some(Value::Object(files)) => {
                let mut out = Vec::new();
                for k in js_keys(files) {
                    out.push((k.clone(), decode(&files[k.as_str()])?));
                }
                Ok(PluginResult::Files(out))
            }
            _ => Err("sent a result without bytes or files".into()),
        }
    }

    fn close(&mut self) {
        if self.exited.is_none() {
            self.exited = Some("closed".into());
        }
        self.kill();
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        self.close();
    }
}

fn message_of(msg: &Value) -> String {
    match msg.get("message") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    }
}

/// What PluginHost warns when a JavaScript plugin whose results may be reused doesn't export `cache`.
pub fn undeclared_cache_warning(name: &str) -> String {
    format!("PLUGIN {name} does not export cache, so its results are reused as if it were deterministic: export const cache = true to confirm, or cache = false if a result can depend on the clock, randomness or an earlier call")
}

/// What a plugin runs as: a sandboxed Node process (.mjs) or a WebAssembly runner (.wasm).
enum Runner {
    Js(PluginProcess),
    Wasm(WasmPlugin),
}

impl Runner {
    fn cacheable(&self) -> bool {
        match self {
            Runner::Js(p) => p.cacheable,
            Runner::Wasm(w) => w.cacheable(),
        }
    }
    fn call(&mut self, request: &PluginRequest, span: Option<&SourceSpan>) -> Result<PluginResult> {
        match self {
            Runner::Js(p) => p.call(request, span),
            Runner::Wasm(w) => w.call(request, span),
        }
    }
    fn close(&mut self) {
        match self {
            Runner::Js(p) => p.close(),
            Runner::Wasm(w) => w.close(),
        }
    }
}

struct Registered {
    file: PathBuf,
    span: Option<SourceSpan>,
    wasm: Option<Vec<u8>>,
}

/// Starts a plugin's runner on its first call (not at registration) and stops them all when the build ends.
pub struct PluginHost {
    policy: PluginPolicy,
    log: Log,
    node: Option<PathBuf>,
    reuses_results: bool,
    warned_cache: Vec<String>,
    /// The checked Node binary, once the first JavaScript plugin needed it.
    runtime: Option<std::result::Result<NodeRuntime, String>>,
    notice: Option<Log>,
    registered: HashMap<String, Registered>,
    procs: HashMap<String, Runner>,
    pub stats: HostStats,
    store_home: Option<PathBuf>,
}

impl PluginHost {
    /// `log` receives plugin console output as `[plugin name] line`.
    pub fn new(policy: PluginPolicy, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        PluginHost {
            policy,
            log: Arc::new(log),
            node: None,
            reuses_results: false,
            warned_cache: Vec::new(),
            runtime: None,
            notice: None,
            registered: HashMap::new(),
            procs: HashMap::new(),
            stats: HostStats::default(),
            store_home: None,
        }
    }

    /// Where wasm plugins' compiled modules are cached: `<home>/cache/wasm`
    /// (`None`, the default: `$SITEFILE_HOME`). See `BuildOptions::store_home`.
    pub fn set_store_home(&mut self, home: Option<PathBuf>) {
        self.store_home = home;
    }

    pub fn store_home(&self) -> Option<&Path> {
        self.store_home.as_deref()
    }

    /// The Node binary for JS plugins, an absolute path (default: `node` on PATH, looked up and
    /// version-checked at first use).
    pub fn set_node(&mut self, node: impl Into<PathBuf>) {
        self.node = Some(node.into());
        self.runtime = None;
    }

    /// Receives `plugins run on node <version> (<path>)` when the first JavaScript plugin starts.
    pub fn set_notice(&mut self, notice: impl Fn(&str) + Send + Sync + 'static) {
        self.notice = Some(Arc::new(notice));
    }

    /// The checked Node binary (resolved once, on the first JavaScript plugin).
    pub fn node(&mut self) -> std::result::Result<NodeRuntime, String> {
        if self.runtime.is_none() {
            let r = check_node(self.node.as_deref(), self.policy.timeout_ms);
            if let (Ok(rt), Some(n)) = (&r, &self.notice) {
                n(&format!(
                    "plugins run on node {} ({})",
                    rt.version,
                    rt.path.display()
                ));
            }
            self.runtime = Some(r);
        }
        self.runtime.clone().expect("set above")
    }

    /// `file` is the plugin's absolute real path (already sandbox- and integrity-checked).
    /// For a `.wasm` plugin, pass the checked module bytes. Runners start lazily.
    /// A second registration of the same name is ignored.
    pub fn register(
        &mut self,
        name: &str,
        file: impl Into<PathBuf>,
        span: Option<SourceSpan>,
        wasm: Option<Vec<u8>>,
    ) {
        self.registered
            .entry(name.to_string())
            .or_insert_with(|| Registered {
                file: file.into(),
                span,
                wasm,
            });
    }

    /// Whether a registered plugin is a WebAssembly module.
    pub fn is_wasm(&self, name: &str) -> bool {
        self.registered.get(name).is_some_and(|r| r.wasm.is_some())
    }

    /// A registered wasm plugin's module bytes.
    pub fn wasm_bytes(&self, name: &str) -> Option<&[u8]> {
        self.registered.get(name)?.wasm.as_deref()
    }

    /// The folder a registered plugin lives in.
    pub fn folder_of(&self, name: &str) -> Option<PathBuf> {
        self.registered
            .get(name)
            .map(|r| r.file.parent().map(Path::to_path_buf).unwrap_or_default())
    }

    /// Whether results of this plugin may be cached (false after it exported `cache = false`).
    pub fn cacheable(&self, name: &str) -> bool {
        self.procs.get(name).map(Runner::cacheable).unwrap_or(true)
    }

    /// The build caches or memoizes results (a cache directory or an incremental rebuild), so a
    /// JavaScript plugin that doesn't export `cache` gets one warning.
    pub fn set_reuses_results(&mut self, reuses: bool) {
        self.reuses_results = reuses;
    }

    pub fn call(
        &mut self,
        name: &str,
        request: &PluginRequest,
        span: Option<&SourceSpan>,
    ) -> Result<PluginResult> {
        self.start(name, span)?;
        self.stats.calls += 1;
        let runner = self.procs.get_mut(name).expect("started above");
        let result = runner.call(request, span)?;
        let undeclared = matches!(runner, Runner::Js(p) if !p.declared_cache);
        if self.reuses_results && undeclared && !self.warned_cache.iter().any(|w| w == name) {
            self.warned_cache.push(name.to_string());
            (self.log)(&undeclared_cache_warning(name));
        }
        Ok(result)
    }

    /// A wasm plugin, compiled, whose calls may run on several threads at once (`WasmPlugin::call_prepared`;
    /// add them to `stats.calls`). None for a JavaScript plugin: its one process takes one call at a time.
    pub fn shared_wasm(
        &mut self,
        name: &str,
        span: Option<&SourceSpan>,
    ) -> Result<Option<&WasmPlugin>> {
        if !self.is_wasm(name) {
            return Ok(None);
        }
        self.start(name, span)?;
        match self.procs.get_mut(name) {
            Some(Runner::Wasm(w)) => {
                w.prepare(span)?;
                Ok(Some(&*w))
            }
            _ => Ok(None),
        }
    }

    /// Start a plugin's runner on its first use.
    fn start(&mut self, name: &str, span: Option<&SourceSpan>) -> Result<()> {
        if !self.procs.contains_key(name) {
            let Some(reg) = self.registered.get(name) else {
                return Err(SitefileError::new(
                    ErrorCode::E_UNDEFINED,
                    format!("unknown plugin '{name}'"),
                    span.cloned(),
                ));
            };
            let node = if reg.wasm.is_none() {
                let span = reg.span.clone();
                Some(self.node().map_err(|m| {
                    SitefileError::new(ErrorCode::E_PLUGIN, format!("PLUGIN {name}: {m}"), span)
                })?)
            } else {
                None
            };
            let reg = self.registered.get(name).expect("checked above");
            let runner = match &reg.wasm {
                Some(bytes) => Runner::Wasm(
                    WasmPlugin::new(name, bytes.clone(), self.policy)
                        .with_store_home(self.store_home.clone()),
                ),
                None => {
                    let log = self.log.clone();
                    let prefix = format!("[plugin {name}] ");
                    let log: Log = Arc::new(move |l: &str| log(&format!("{prefix}{l}")));
                    Runner::Js(PluginProcess::start(
                        name,
                        &reg.file,
                        &node.expect("a JavaScript plugin").path,
                        self.policy,
                        log,
                        reg.span.clone(),
                    ))
                }
            };
            self.procs.insert(name.to_string(), runner);
            self.stats.spawned += 1;
        }
        Ok(())
    }

    /// Stop every runner and forget the registrations.
    pub fn close(&mut self) {
        for p in self.procs.values_mut() {
            p.close();
        }
        self.procs.clear();
        self.registered.clear();
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn node_available() -> bool {
        find_on_path("node").is_some()
    }

    fn tmp(prefix: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "{prefix}{}-{n}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        crate::platform::canonicalize(d).unwrap()
    }

    /// A project folder with `plugins/<name>.mjs` (+ extra files); returns (root, plugin path).
    fn plugin(src: &str, extra: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let root = tmp("sitefile-plugins-rs-");
        fs::create_dir_all(root.join("plugins")).unwrap();
        let file = root.join("plugins/p.mjs");
        fs::write(&file, src).unwrap();
        for (rel, content) in extra {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        (root, file)
    }

    fn host_with(file: &Path, policy: PluginPolicy) -> (PluginHost, Arc<Mutex<Vec<String>>>) {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let l = logs.clone();
        let mut h = PluginHost::new(policy, move |line| l.lock().unwrap().push(line.to_string()));
        h.register("p", file, Some(SourceSpan::new("Sitefile", 2, 1)), None);
        (h, logs)
    }

    fn req(dest: &str) -> PluginRequest {
        PluginRequest {
            dest: dest.into(),
            ..Default::default()
        }
    }

    fn text(r: &PluginResult) -> String {
        match r {
            PluginResult::File(b) => String::from_utf8(b.clone()).unwrap(),
            other => panic!("expected one file, got {other:?}"),
        }
    }

    const ECHO: &str = "export async function transform(req) {
  return JSON.stringify({ dest: req.dest, options: req.options, input: req.input && { path: req.input.path, name: req.input.name, text: req.input.text, size: req.input.bytes.length }, data: req.data, item: req.item });
}";

    #[test]
    fn runner_is_the_typescript_runner_verbatim() {
        let ts = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/plugins/host.ts"),
        )
        .unwrap();
        let start =
            ts.find("const RUNNER = String.raw`").unwrap() + "const RUNNER = String.raw`".len();
        let end = start + ts[start..].find("\n`;").unwrap() + 1;
        assert_eq!(RUNNER, &ts[start..end]);
        assert!(ts.contains("const MARK = '\\u0000SITEFILE:';"));
    }

    #[test]
    fn wire_format_and_helpers() {
        let mut r = PluginRequest {
            dest: "/out.json".into(),
            options: vec![("n".into(), "3".into()), ("label".into(), "a b".into())],
            ..Default::default()
        };
        r.input = Some(PluginInput::new(
            "./notes.txt",
            "notes.txt",
            b"hello".to_vec(),
        ));
        r.item = Some(json!({ "slug": "ada" }));
        assert_eq!(
            r.to_wire_string(),
            r#"{"apiVersion":1,"dest":"/out.json","options":{"n":"3","label":"a b"},"input":{"path":"./notes.txt","name":"notes.txt","bytes":"aGVsbG8=","text":"hello"},"item":{"slug":"ada"}}"#
        );
        assert_eq!(PluginInput::new("a", "a", vec![0xff]).text, None);
        assert_eq!(
            PluginInput::new("a", "a", b"\xef\xbb\xbfhi".to_vec())
                .text
                .as_deref(),
            Some("hi")
        );
        assert_eq!(b64_decode_lenient("aGVsbG8="), b"hello");
        assert_eq!(b64_decode_lenient("aGVsbG8"), b"hello");
        assert_eq!(b64_decode_lenient("aG Vs\nbG8=junk"), b"hello");
        assert_eq!(
            file_url(Path::new("/a b/c#d.mjs")),
            "file:///a%20b/c%23d.mjs"
        );
    }

    #[test]
    fn transform_receives_file_options_dest_data_and_item() {
        if !node_available() {
            eprintln!("skipped: node is not on PATH");
            return;
        }
        let (root, file) = plugin(ECHO, &[]);
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        let mut r = PluginRequest {
            dest: "/out.json".into(),
            options: vec![("n".into(), "3".into()), ("label".into(), "a b".into())],
            ..Default::default()
        };
        r.input = Some(PluginInput::new(
            "./notes.txt",
            "notes.txt",
            b"hello".to_vec(),
        ));
        let got: Value = serde_json::from_str(&text(&h.call("p", &r, None).unwrap())).unwrap();
        assert_eq!(
            got,
            json!({ "dest": "/out.json", "options": { "n": "3", "label": "a b" }, "input": { "path": "./notes.txt", "name": "notes.txt", "text": "hello", "size": 5 } })
        );
        let mut r2 = req("/p/ada.json");
        r2.data = Some(json!({ "k": 1 }));
        r2.item = Some(json!({ "slug": "ada", "name": "Ada" }));
        let got: Value = serde_json::from_str(&text(&h.call("p", &r2, None).unwrap())).unwrap();
        assert_eq!(got["data"], json!({ "k": 1 }));
        assert_eq!(got["item"], json!({ "slug": "ada", "name": "Ada" }));
        assert_eq!(
            (h.stats.spawned, h.stats.calls),
            (1, 2),
            "one process serves every call"
        );
        assert!(h.cacheable("p"));
        h.close();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn several_files_as_bytes_or_strings() {
        if !node_available() {
            return;
        }
        let (root, file) = plugin(
            "export default { apiVersion: 1, transform: () => ({ files: { 'a.txt': 'A', 'sub/b.bin': new Uint8Array([1, 2, 3]), '7': '' } }) };",
            &[],
        );
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        let r = h.call("p", &req("/out/"), None).unwrap();
        assert_eq!(
            r,
            PluginResult::Files(vec![
                ("7".into(), vec![]),
                ("a.txt".into(), b"A".to_vec()),
                ("sub/b.bin".into(), vec![1, 2, 3])
            ])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sandbox_denies_reads_outside_the_folder_writes_processes_network_and_environment() {
        if !node_available() {
            return;
        }
        let probe = r#"export async function transform() {
    const out = {};
    const t = async (k, f) => { try { await f(); out[k] = 'allowed'; } catch { out[k] = 'denied'; } };
    const fs = await import('node:fs/promises');
    await t('readOwn', () => fs.readFile(new URL('./helper.txt', import.meta.url)));
    await t('readContext', () => fs.readFile(new URL('../secret.txt', import.meta.url)));
    await t('readEtc', () => fs.readFile('/etc/hosts'));
    await t('write', () => fs.writeFile(new URL('./written.txt', import.meta.url), 'x'));
    await t('spawn', async () => (await import('node:child_process')).execFileSync('/bin/echo', ['x']));
    await t('net', () => new Promise(async (res, rej) => { const s = (await import('node:net')).connect(80, '1.1.1.1'); s.on('connect', () => { s.destroy(); res(); }); s.on('error', rej); }));
    out.env = Object.keys(process.env).filter((k) => k !== '__CF_USER_TEXT_ENCODING');
    return JSON.stringify(out);
  }"#;
        std::env::set_var("SITEFILE_SECRET_PROBE", "hunter2");
        let (root, file) = plugin(
            probe,
            &[("plugins/helper.txt", "ok"), ("secret.txt", "TOP SECRET")],
        );
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        let got: Value =
            serde_json::from_str(&text(&h.call("p", &req("/r.json"), None).unwrap())).unwrap();
        assert_eq!(
            got,
            json!({ "readOwn": "allowed", "readContext": "denied", "readEtc": "denied", "write": "denied", "spawn": "denied", "net": "denied", "env": [] })
        );
        assert!(!root.join("plugins/written.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn rejects(src: &str, policy: PluginPolicy, needle: &str) -> SitefileError {
        let (root, file) = plugin(src, &[]);
        let (mut h, _) = host_with(&file, policy);
        let e = h.call("p", &req("/a.out"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN, "{}", e.message());
        assert!(
            e.message().contains(needle),
            "{:?} does not contain {needle:?}",
            e.message()
        );
        h.close();
        fs::remove_dir_all(root).unwrap();
        e
    }

    #[test]
    fn plugin_errors_bad_results_and_timeouts() {
        if !node_available() {
            return;
        }
        let p = PluginPolicy::default();
        let e = rejects(
            "export function transform() { throw new Error('boom: bad input'); }",
            p,
            "boom: bad input",
        );
        assert_eq!(e.message(), "PLUGIN p: boom: bad input");
        rejects(
            "export function transform() { return 42; }",
            p,
            "must return",
        );
        let e = rejects("export const x = 1;", p, "must export transform");
        assert_eq!(
            e.span,
            Some(SourceSpan::new("Sitefile", 2, 1)),
            "load errors point at the PLUGIN line"
        );
        rejects(
            "export const apiVersion = 2; export function transform() { return ''; }",
            p,
            "apiVersion",
        );
        rejects("export const = ;", p, "PLUGIN p: ");
        let t0 = Instant::now();
        rejects(
            "export function transform() { return new Promise(() => {}); }",
            PluginPolicy {
                timeout_ms: 300,
                ..p
            },
            "timed out after 300 ms",
        );
        assert!(t0.elapsed() < Duration::from_secs(5));
        rejects(
            "export function transform() { return 'x'.repeat(1000); }",
            PluginPolicy {
                max_output_bytes: 10,
                ..p
            },
            "larger than 10 bytes",
        );
        rejects(
            "export function transform() { console.error('dying'); process.exit(3); }",
            p,
            "exited with code 3\ndying",
        );
    }

    #[test]
    fn a_plugin_that_passes_the_memory_limit_fails_without_the_v8_report() {
        if !node_available() {
            return;
        }
        // Retained objects grow the heap until V8 gives up at --max-old-space-size (#157).
        let (root, file) = plugin(
            "export function transform() { const keep = []; for (;;) keep.push({ n: keep.length, s: String(keep.length) }); }",
            &[],
        );
        let policy = PluginPolicy {
            max_memory_mib: 32,
            ..PluginPolicy::default()
        };
        let (mut h, logs) = host_with(&file, policy);
        let t0 = Instant::now();
        let e = h.call("p", &req("/a"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN);
        assert_eq!(
            e.message(),
            "PLUGIN p: ran out of memory (more than the 32 MiB limit)"
        );
        assert!(t0.elapsed() < Duration::from_secs(30));
        let logs = logs.lock().unwrap();
        assert!(
            !logs.iter().any(|l| l.contains("FATAL ERROR")
                || l.contains("Last few GCs")
                || l.contains("node::")),
            "{logs:?}"
        );
        drop(logs);
        h.close();
        fs::remove_dir_all(root).unwrap();

        // Within the limit: unaffected.
        let (root, file) = plugin(
            "export function transform() { return 'x'.repeat(4 << 20).length + ''; }",
            &[],
        );
        let policy = PluginPolicy {
            max_memory_mib: 64,
            ..PluginPolicy::default()
        };
        let (mut h, _) = host_with(&file, policy);
        assert_eq!(text(&h.call("p", &req("/a"), None).unwrap()), "4194304");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_dead_process_fails_later_calls() {
        if !node_available() {
            return;
        }
        let (root, file) = plugin(
            "let n = 0; export function transform() { if (n++) process.exit(4); return 'first'; }",
            &[],
        );
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        assert_eq!(text(&h.call("p", &req("/a"), None).unwrap()), "first");
        let e = h.call("p", &req("/a"), None).unwrap_err();
        assert!(
            e.message().contains("PLUGIN p: exited with code 4"),
            "{}",
            e.message()
        );
        let e = h.call("p", &req("/a"), None).unwrap_err();
        assert_eq!(e.message(), "PLUGIN p exited with code 4");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn console_output_becomes_log_lines_and_cache_false_is_reported() {
        if !node_available() {
            return;
        }
        let (root, file) = plugin("export function transform() { console.log('working…'); return 'done'; }\nexport const cache = false;", &[]);
        let (mut h, logs) = host_with(&file, PluginPolicy::default());
        assert!(h.cacheable("p"), "unknown until it loads");
        assert_eq!(text(&h.call("p", &req("/a.out"), None).unwrap()), "done");
        assert!(!h.cacheable("p"));
        // Console lines travel ahead of the result on stdout: they are logged
        // by the time the call returns, with no wait (#161).
        assert!(
            logs.lock()
                .unwrap()
                .iter()
                .any(|l| l == "[plugin p] working…"),
            "{:?}",
            logs.lock().unwrap()
        );
        fs::remove_dir_all(root).unwrap();

        let (root, file) = plugin(
            "export default { apiVersion: 1, cache: false, transform: () => 'x' };",
            &[],
        );
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        h.call("p", &req("/a"), None).unwrap();
        assert!(!h.cacheable("p"));
        fs::remove_dir_all(root).unwrap();
    }

    /// An executable that prints `out` for --version, standing in for another Node.
    #[cfg(unix)]
    fn fake_node(out: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("sitefile-node-rs-");
        let file = dir.join("node");
        fs::write(&file, format!("#!/bin/sh\necho '{out}'\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        file
    }

    #[test]
    fn node_versions_parse_and_compare() {
        assert_eq!(parse_node_version("v26.11.1\n"), Some([26, 11, 1]));
        assert_eq!(parse_node_version("26.1.0"), None);
        assert_eq!(parse_node_version("v26.1"), None);
        assert_eq!(parse_node_version("v26.1.0.1"), None);
        assert_eq!(parse_node_version("v26.x.0"), None);
        assert_eq!(MIN_NODE, [26, 0, 0]);
        assert!([25, 9, 9] < MIN_NODE && [26, 0, 0] >= MIN_NODE && [27, 0, 0] > MIN_NODE);
    }

    #[cfg(unix)]
    #[test]
    fn the_node_binary_is_checked_and_reported_once() {
        assert_eq!(
            check_node(Some(Path::new("node")), 10_000).unwrap_err(),
            "the Node binary must be an absolute path, got 'node'"
        );
        assert_eq!(
            check_node(Some(Path::new("/nonexistent/node")), 10_000).unwrap_err(),
            "could not start: no Node binary at /nonexistent/node"
        );
        let old = fake_node("v22.1.0");
        assert_eq!(
            check_node(Some(&old), 10_000).unwrap_err(),
            format!(
                "node v22.1.0 ({}) is too old for the plugin sandbox: JavaScript plugins need Node 26.0.0 or later; choose another with --node or SITEFILE_NODE",
                old.display()
            )
        );
        let garbage = fake_node("hello");
        assert!(check_node(Some(&garbage), 10_000)
            .unwrap_err()
            .contains("--version did not print a Node version"));

        // A too-old Node fails at the PLUGIN line, before any plugin process starts.
        let (root, file) = plugin("export function transform() { return 'x'; }", &[]);
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        h.set_node(&old);
        let e = h.call("p", &req("/a"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN);
        assert!(
            e.message()
                .starts_with("Sitefile:2:1: PLUGIN p: node v22.1.0 ("),
            "{}",
            e.message()
        );
        assert_eq!(e.span, Some(SourceSpan::new("Sitefile", 2, 1)));
        assert_eq!(h.stats.spawned, 0);
        fs::remove_dir_all(old.parent().unwrap()).unwrap();
        fs::remove_dir_all(garbage.parent().unwrap()).unwrap();

        if !node_available() {
            fs::remove_dir_all(root).unwrap();
            return;
        }
        // The chosen Node runs the plugin and is named in one notice.
        let node = crate::platform::canonicalize(find_on_path("node").unwrap()).unwrap();
        let version = check_node(Some(&node), 10_000).unwrap().version;
        let notices = Arc::new(Mutex::new(Vec::<String>::new()));
        let n = notices.clone();
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        h.set_node(&node);
        h.set_notice(move |m| n.lock().unwrap().push(m.to_string()));
        assert_eq!(text(&h.call("p", &req("/a"), None).unwrap()), "x");
        assert_eq!(text(&h.call("p", &req("/b"), None).unwrap()), "x");
        assert_eq!(
            *notices.lock().unwrap(),
            vec![format!(
                "plugins run on node {version} ({})",
                node.display()
            )]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_undeclared_cache_warns_once_when_results_are_reused() {
        if !node_available() {
            return;
        }
        let warned = |logs: &Arc<Mutex<Vec<String>>>| -> Vec<String> {
            logs.lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains("does not export cache"))
                .cloned()
                .collect()
        };
        // Undeclared: cacheable, one warning for two calls (#160).
        let (root, file) = plugin("export function transform(r) { return r.dest; }", &[]);
        let (mut h, logs) = host_with(&file, PluginPolicy::default());
        h.set_reuses_results(true);
        h.call("p", &req("/a"), None).unwrap();
        h.call("p", &req("/b"), None).unwrap();
        assert!(h.cacheable("p"));
        assert_eq!(warned(&logs), vec![undeclared_cache_warning("p")]);
        assert_eq!(
            undeclared_cache_warning("p"),
            "PLUGIN p does not export cache, so its results are reused as if it were deterministic: export const cache = true to confirm, or cache = false if a result can depend on the clock, randomness or an earlier call"
        );
        // Nothing reused: no warning.
        let (mut h, logs) = host_with(&file, PluginPolicy::default());
        h.call("p", &req("/a"), None).unwrap();
        assert!(warned(&logs).is_empty());
        fs::remove_dir_all(root).unwrap();
        // Declared either way: no warning.
        for (src, cacheable) in [
            (
                "export const cache = true; export function transform() { return 'x'; }",
                true,
            ),
            (
                "export default { apiVersion: 1, cache: true, transform: () => 'x' };",
                true,
            ),
            (
                "export const cache = false; export function transform() { return 'x'; }",
                false,
            ),
        ] {
            let (root, file) = plugin(src, &[]);
            let (mut h, logs) = host_with(&file, PluginPolicy::default());
            h.set_reuses_results(true);
            h.call("p", &req("/a"), None).unwrap();
            assert_eq!(h.cacheable("p"), cacheable, "{src}");
            assert!(warned(&logs).is_empty(), "{src}");
            fs::remove_dir_all(root).unwrap();
        }
        rejects(
            "export const cache = 'yes'; export function transform() { return 'x'; }",
            PluginPolicy::default(),
            "PLUGIN p: cache must be true or false, got \"yes\"",
        );
    }

    #[test]
    fn a_missing_node_binary_is_an_error_not_a_panic() {
        let (root, file) = plugin("export function transform() { return 'x'; }", &[]);
        let (mut h, _) = host_with(&file, PluginPolicy::default());
        h.set_node(if cfg!(windows) {
            r"C:\nonexistent\node.exe"
        } else {
            "/nonexistent/node"
        });
        let e = h.call("p", &req("/a"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN);
        assert!(
            e.message().contains("PLUGIN p: could not start:"),
            "{}",
            e.message()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
