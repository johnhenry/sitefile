//! Port of `src/plugins/exec.ts`: the exec adapter behind `TOOL`.
//!
//! Runs an external program that the user allow-listed with `--allow-exec`.
//! There is no shell: the program is started with an argv array, in a fresh
//! temporary directory, with an almost empty environment (`PATH=/usr/bin:/bin`)
//! and a closed stdin. Nothing can restrict a native binary, so the tool runs
//! with the user's privileges: the allow-list is the control. See docs/PLUGINS.md.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::error::{ErrorCode, Result, SitefileError, SourceSpan};
use crate::plugins::cache::mtime_ms;
use crate::plugins::host::{exit_reason, PluginResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Per-call time limit (default 60 s).
    pub timeout_ms: u64,
    /// Largest output file accepted (default 64 MiB).
    pub max_output_bytes: u64,
}

pub const DEFAULT_TOOL_POLICY: ToolPolicy = ToolPolicy {
    timeout_ms: 60_000,
    max_output_bytes: 64 * 1024 * 1024,
};

impl Default for ToolPolicy {
    fn default() -> Self {
        DEFAULT_TOOL_POLICY
    }
}

/// The environment given to every tool: nothing but a fixed PATH (tools are started by absolute path).
pub const TOOL_ENV: &[(&str, &str)] = &[("PATH", "/usr/bin:/bin")];

/// An allow-listed program, resolved to an absolute real path.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTool {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Node's `stats.mtimeMs` (it is part of the cache key, so it must match the TypeScript value).
    pub mtime_ms: f64,
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    true
}

/// Resolve `--allow-exec name[=path]`. A bare name is looked up on `search_path`
/// (the build's PATH); an explicit path must be absolute. The result is the real
/// path (symlinks followed), so what is printed, keyed and run is one file.
pub fn resolve_tool(
    name: &str,
    explicit: Option<&str>,
    search_path: Option<&str>,
    span: Option<&SourceSpan>,
) -> Result<ResolvedTool> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    match explicit {
        Some(p) => {
            if !Path::new(p).is_absolute() {
                return Err(SitefileError::new(
                    ErrorCode::E_USAGE,
                    format!("--allow-exec {name}={p}: the path must be absolute"),
                    span.cloned(),
                ));
            }
            candidates.push(PathBuf::from(p));
        }
        None => {
            for dir in std::env::split_paths(search_path.unwrap_or("")) {
                if !dir.as_os_str().is_empty() && dir.is_absolute() {
                    // Windows: `name.exe` first, then the name as given (#318).
                    #[cfg(windows)]
                    candidates.push(dir.join(format!("{name}.exe")));
                    candidates.push(dir.join(name));
                }
            }
        }
    }
    for c in candidates {
        let Ok(real) = crate::platform::canonicalize(&c) else {
            continue;
        };
        let Ok(st) = fs::metadata(&real) else {
            continue;
        };
        if !st.is_file() || !is_executable(&st) {
            continue;
        }
        return Ok(ResolvedTool {
            name: name.into(),
            path: real,
            size: st.len(),
            mtime_ms: mtime_ms(&st),
        });
    }
    let detail = match explicit {
        Some(p) => format!("TOOL {name}: {p} is not an executable file"),
        None => format!("TOOL {name}: no executable '{name}' found on PATH (or pass --allow-exec {name}=/absolute/path)"),
    };
    Err(SitefileError::new(
        ErrorCode::E_NOT_FOUND,
        detail,
        span.cloned(),
    ))
}

/// The input file given to a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInput {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// What the runner needs from one TRANSFORM call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    /// argv template with `${…}` already interpolated; `{input}`, `{output}` and `{option.key}` are replaced here.
    pub args: Vec<String>,
    pub dest: String,
    pub options: Vec<(String, String)>,
    pub input: Option<ToolInput>,
}

const MAX_LOG_LINES: usize = 20;
const MAX_LINE: usize = 300;
const KEEP: usize = 16 * 1024;

/// A file extension safe to put in a temp file name (tools such as ffmpeg pick formats from it).
/// Same rules as `path.extname` plus `/^\.[A-Za-z0-9]{1,12}$/`.
fn safe_ext(name: &str) -> &str {
    let base = name.trim_end_matches('/');
    let base = base.rsplit('/').next().unwrap_or(base);
    match base.rfind('.') {
        Some(i) if i > 0 => {
            let ext = &base[i..];
            if (2..=13).contains(&ext.len()) && ext[1..].bytes().all(|b| b.is_ascii_alphanumeric())
            {
                ext
            } else {
                ""
            }
        }
        _ => "",
    }
}

/// The placeholders of `TOOL_PLACEHOLDER_RE` (`/\{(input|output|option\.[A-Za-z_][A-Za-z0-9_-]*)\}/g`)
/// in `s`, as (start, end, name) byte ranges of the whole `{…}`.
pub fn tool_placeholders(s: &str) -> Vec<(usize, usize, &str)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            if let Some(close) = s[i + 1..].find('}') {
                let inner = &s[i + 1..i + 1 + close];
                let ok = inner == "input"
                    || inner == "output"
                    || inner.strip_prefix("option.").is_some_and(|k| {
                        let mut cs = k.bytes();
                        matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
                            && cs.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                    });
                if ok {
                    out.push((i, i + close + 2, inner));
                    i += close + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// The argv after placeholder substitution. `{input}` and `{output}` become
/// *relative* names inside the call's temp dir (the tool's cwd). Values are
/// inserted verbatim into one argv entry; they are never re-scanned, and never split.
pub fn substitute_args(
    args: &[String],
    input_file: Option<&str>,
    output_file: &str,
    options: &[(String, String)],
) -> Vec<String> {
    args.iter()
        .map(|a| {
            let mut out = String::with_capacity(a.len());
            let mut last = 0;
            for (start, end, name) in tool_placeholders(a) {
                out.push_str(&a[last..start]);
                match name {
                    "input" => out.push_str(input_file.unwrap_or("")),
                    "output" => out.push_str(output_file),
                    _ => {
                        let key = &name["option.".len()..];
                        if let Some((_, v)) = options.iter().find(|(k, _)| k == key) {
                            out.push_str(v);
                        }
                    }
                }
                last = end;
            }
            out.push_str(&a[last..]);
            out
        })
        .collect()
}

/// A fresh private directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> std::io::Result<TempDir> {
        static N: AtomicU64 = AtomicU64::new(0);
        const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let base = std::env::temp_dir();
        let mut last_err = None;
        for _ in 0..100 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0) as u64;
            let mut x = nanos
                ^ (std::process::id() as u64).rotate_left(32)
                ^ N.fetch_add(1, Ordering::Relaxed)
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let mut suffix = String::new();
            for _ in 0..6 {
                suffix.push(ALPHA[(x % ALPHA.len() as u64) as usize] as char);
                x /= ALPHA.len() as u64;
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
            }
            let dir = base.join(format!("{prefix}{suffix}"));
            let mut b = fs::DirBuilder::new();
            {
                use crate::platform::DirBuilderExt;
                b.mode(0o700);
            }
            match b.create(&dir) {
                Ok(()) => return Ok(TempDir(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| std::io::Error::other("could not create a temporary directory")))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Last `KEEP` bytes of a stream, so a chatty tool can't exhaust memory.
fn read_tail(mut r: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut keep: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    keep.extend_from_slice(&chunk[..n]);
                    if keep.len() > KEEP {
                        keep.drain(..keep.len() - KEEP);
                    }
                }
            }
        }
        keep
    })
}

fn tail(s: &str) -> Vec<String> {
    let lines: Vec<String> = s
        .split('\n')
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l.chars().count() > MAX_LINE {
                format!("{}…", l.chars().take(MAX_LINE).collect::<String>())
            } else {
                l
            }
        })
        .collect();
    lines[lines.len().saturating_sub(MAX_LOG_LINES)..].to_vec()
}

/// Run one TOOL call. Log lines (`[tool name] …`) are the tool's stdout, then its stderr.
pub fn run_tool(
    tool: &ResolvedTool,
    call: &ToolCall,
    policy: &ToolPolicy,
    log: &dyn Fn(&str),
    span: Option<&SourceSpan>,
) -> Result<PluginResult> {
    let label = format!("TOOL {}", tool.name);
    let io_err = |e: std::io::Error| {
        SitefileError::new(ErrorCode::E_PLUGIN, format!("{label}: {e}"), span.cloned())
    };
    let dir = TempDir::new("sitefile-tool-").map_err(io_err)?;

    let mut input_file: Option<String> = None;
    if let Some(input) = &call.input {
        let name = format!("input{}", safe_ext(&input.name));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        o.open(dir.0.join(&name))
            .and_then(|mut f| f.write_all(&input.bytes))
            .map_err(io_err)?;
        input_file = Some(name);
    }
    let output_file = format!("output{}", safe_ext(&call.dest));
    let argv = substitute_args(
        &call.args,
        input_file.as_deref(),
        &output_file,
        &call.options,
    );
    let out_path = dir.0.join(&output_file);

    let mut cmd = Command::new(&tool.path);
    cmd.args(&argv)
        .current_dir(&dir.0)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in TOOL_ENV {
        cmd.env(k, v);
    }

    let mut spawn_error: Option<String> = None;
    let mut timed_out = false;
    let mut oversized = false;
    let mut status = None;
    let mut stdout = String::new();
    let mut stderr = String::new();
    match cmd.spawn() {
        Err(e) => spawn_error = Some(e.to_string()),
        Ok(mut child) => {
            let out_h = read_tail(child.stdout.take().expect("piped"));
            let err_h = read_tail(child.stderr.take().expect("piped"));
            let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
            let mut next_size_check = Instant::now() + Duration::from_millis(100);
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        status = Some(st);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        spawn_error = Some(e.to_string());
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
                let now = Instant::now();
                if now >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    status = child.wait().ok();
                    break;
                }
                // Stop a tool that is filling the disk with an oversized output.
                if now >= next_size_check {
                    next_size_check = now + Duration::from_millis(100);
                    if fs::metadata(&out_path).is_ok_and(|m| m.len() > policy.max_output_bytes) {
                        oversized = true;
                        let _ = child.kill();
                        status = child.wait().ok();
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
            stdout = String::from_utf8_lossy(&out_h.join().unwrap_or_default()).into_owned();
            stderr = String::from_utf8_lossy(&err_h.join().unwrap_or_default()).into_owned();
        }
    }

    let fail = |why: String| -> SitefileError {
        let lines = tail(if stderr.is_empty() { &stdout } else { &stderr });
        let lines = &lines[lines.len().saturating_sub(8)..];
        let extra = if lines.is_empty() {
            String::new()
        } else {
            format!("\n{}", lines.join("\n"))
        };
        SitefileError::new(
            ErrorCode::E_PLUGIN,
            format!("{label}: {why}{extra}"),
            span.cloned(),
        )
    };
    if let Some(e) = spawn_error {
        return Err(fail(format!(
            "could not start {}: {e}",
            tool.path.display()
        )));
    }
    if timed_out {
        return Err(fail(format!("timed out after {} ms", policy.timeout_ms)));
    }
    if oversized {
        return Err(fail(format!(
            "output is larger than {} bytes",
            policy.max_output_bytes
        )));
    }
    match status {
        Some(st) if st.success() => {}
        Some(st) => return Err(fail(exit_reason(st))),
        None => return Err(fail("exited".into())),
    }

    for line in tail(&stdout) {
        log(&format!("[tool {}] {line}", tool.name));
    }
    for line in tail(&stderr) {
        log(&format!("[tool {}] {line}", tool.name));
    }

    let Ok(st) = fs::symlink_metadata(&out_path) else {
        return Err(fail(format!("did not write {{output}} ({output_file})")));
    };
    if !st.is_file() {
        return Err(fail("{output} is not a regular file".into()));
    }
    if st.len() > policy.max_output_bytes {
        return Err(fail(format!(
            "output is larger than {} bytes",
            policy.max_output_bytes
        )));
    }
    let bytes = fs::read(&out_path).map_err(|e| fail(e.to_string()))?;
    Ok(PluginResult::File(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn tmp(prefix: &str) -> PathBuf {
        let d = TempDir::new(prefix).unwrap();
        let p = d.0.clone();
        std::mem::forget(d);
        crate::platform::canonicalize(p).unwrap()
    }

    fn node() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join("node"))
            .find(|p| p.is_file())
    }

    /// The fake tool from test/exec-tool.test.ts (a node script).
    fn fake_tool() -> Option<(PathBuf, ResolvedTool)> {
        let node = node()?;
        let d = tmp("sitefile-faketool-");
        let file = d.join("fake");
        let src = format!(
            r#"#!{}
const fs = require('node:fs');
const [mode, ...rest] = process.argv.slice(2);
const out = rest[rest.length - 1];
const listing = fs.readdirSync('.').sort();
switch (mode) {{
  case 'dump':
    fs.writeFileSync(out, JSON.stringify({{ argv: rest.slice(0, -1), env: process.env, cwd: process.cwd(), files: listing, stdinIsNull: fs.fstatSync(0).isCharacterDevice() }}));
    break;
  case 'sleep':
    setTimeout(() => fs.writeFileSync(out, 'late'), 5000);
    break;
  case 'fail':
    console.error('first line');
    console.error('boom: bad input');
    process.exit(3);
  case 'nowrite':
    break;
  case 'big':
    fs.writeFileSync(out, Buffer.alloc(100000, 1));
    break;
  case 'noisy':
    console.log('to stdout');
    console.error('to stderr');
    fs.writeFileSync(out, 'ok');
    break;
  case 'symlink':
    fs.symlinkSync('/etc/hosts', out);
    break;
  case 'copy':
    fs.copyFileSync(rest[0], out);
    break;
}}
"#,
            node.display()
        );
        fs::write(&file, src).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let t = resolve_tool("fake", Some(file.to_str().unwrap()), None, None).unwrap();
        Some((d, t))
    }

    fn call(args: &[&str]) -> ToolCall {
        ToolCall {
            args: args.iter().map(|s| s.to_string()).collect(),
            dest: "/o.json".into(),
            options: vec![],
            input: Some(ToolInput {
                name: "in.txt".into(),
                bytes: b"hello".to_vec(),
            }),
        }
    }

    fn run(t: &ResolvedTool, c: &ToolCall, p: ToolPolicy) -> (Result<PluginResult>, Vec<String>) {
        let logs = RefCell::new(Vec::new());
        let r = run_tool(t, c, &p, &|l| logs.borrow_mut().push(l.to_string()), None);
        (r, logs.into_inner())
    }

    fn rejects(t: &ResolvedTool, c: &ToolCall, p: ToolPolicy, needle: &str) {
        let e = run(t, c, p).0.unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN, "{}", e.message());
        assert!(
            e.message().contains(needle),
            "{:?} does not contain {needle:?}",
            e.message()
        );
    }

    #[test]
    fn substitute_args_replaces_only_known_placeholders_once() {
        let args: Vec<String> = [
            "{input}",
            "x={output}",
            "{option.a}",
            "{option.missing}|",
            "{other}",
            "%{pts}{x}",
            "{{input}}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            substitute_args(
                &args,
                Some("input.txt"),
                "output.mp4",
                &[("a".into(), "{output}".into())]
            ),
            vec![
                "input.txt",
                "x=output.mp4",
                "{output}",
                "|",
                "{other}",
                "%{pts}{x}",
                "{input.txt}"
            ]
        );
        assert_eq!(
            substitute_args(&["{input}".to_string()], None, "o", &[]),
            vec![""]
        );
    }

    #[test]
    fn safe_ext_rules() {
        assert_eq!(safe_ext("/clip.mp4"), ".mp4");
        assert_eq!(safe_ext("a.tar.gz"), ".gz");
        assert_eq!(safe_ext(".bashrc"), "");
        assert_eq!(safe_ext("x.a b"), "");
        assert_eq!(safe_ext("x."), "");
        assert_eq!(safe_ext("x.abcdefghijklm"), "");
        assert_eq!(safe_ext("/d.x/"), ".x");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_by_path_name_explicit_and_errors() {
        let cat = resolve_tool(
            "cat",
            None,
            Some("relative/dir:/nonexistent:/bin:/usr/bin"),
            None,
        )
        .unwrap();
        assert_eq!(cat.path, crate::platform::canonicalize("/bin/cat").unwrap());
        assert!(cat.size > 0 && cat.mtime_ms > 0.0);
        let e = resolve_tool("fake", None, Some("/nonexistent"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_NOT_FOUND);
        assert!(e.message().contains(
            "no executable 'fake' found on PATH (or pass --allow-exec fake=/absolute/path)"
        ));
        let e = resolve_tool("fake", Some("bin/fake"), None, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_USAGE);
        assert!(e.message().contains("must be absolute"));
        let e = resolve_tool("fake", Some("/nonexistent/fake"), None, None).unwrap_err();
        assert_eq!(
            (e.code, e.detail.as_str()),
            (
                ErrorCode::E_NOT_FOUND,
                "TOOL fake: /nonexistent/fake is not an executable file"
            )
        );
        // not executable
        let d = tmp("sitefile-noexec-");
        fs::write(d.join("x"), "x").unwrap();
        assert!(resolve_tool("x", Some(d.join("x").to_str().unwrap()), None, None).is_err());
        // a symlink resolves to the real binary
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/bin/cat", d.join("kat")).unwrap();
            assert_eq!(
                resolve_tool("kat", Some(d.join("kat").to_str().unwrap()), None, None)
                    .unwrap()
                    .path,
                crate::platform::canonicalize("/bin/cat").unwrap()
            );
        }
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mtime_ms_matches_node() {
        let Some(node) = node() else { return };
        let t = resolve_tool("cat", Some("/bin/cat"), None, None).unwrap();
        let out = Command::new(node).args(["-e", "process.stdout.write(JSON.stringify(require('fs').statSync(process.argv[1]).mtimeMs))", t.path.to_str().unwrap()]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            crate::plugins::cache::js_number(t.mtime_ms)
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_tools_copy_and_translate() {
        let cp = resolve_tool("cp", None, Some("/bin"), None).unwrap();
        let (r, _) = run(
            &cp,
            &ToolCall {
                args: vec!["{input}".into(), "{output}".into()],
                dest: "/b.txt".into(),
                ..call(&[])
            },
            ToolPolicy::default(),
        );
        assert_eq!(r.unwrap(), PluginResult::File(b"hello".to_vec()));
        // cat writes to stdout only: its output is logged, and the missing {output} is an error
        let cat = resolve_tool("cat", None, Some("/bin"), None).unwrap();
        let (r, logs) = run(&cat, &call(&["{input}"]), ToolPolicy::default());
        assert!(r
            .unwrap_err()
            .message()
            .contains("TOOL cat: did not write {output} (output.json)"));
        assert_eq!(logs, vec!["[tool cat] hello"]);
        // a usage error: the exit code and the tool's stderr are in the message
        let tr = resolve_tool("tr", None, Some("/usr/bin"), None).unwrap();
        rejects(
            &tr,
            &call(&["a-z", "A-Z", "{output}"]),
            ToolPolicy::default(),
            "TOOL tr: exited with code 1\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn argv_placeholders_no_shell_scrubbed_env_temp_cwd_closed_stdin() {
        let Some((d, tool)) = fake_tool() else { return };
        std::env::set_var("SITEFILE_SECRET_PROBE", "hunter2");
        let hostile = "; echo pwned > /tmp/pwned-by-sitefile $(id) `id` | cat && *";
        let c = ToolCall {
            args: [
                "dump",
                "-i",
                "{input}",
                "--size={option.size}",
                "--note={option.note}",
                hostile,
                "$(whoami)",
                ";",
                "%{pts}",
                "{unknown}",
                "{output}",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            dest: "/out.json".into(),
            options: vec![
                ("size".into(), "640x480".into()),
                ("note".into(), "a b".into()),
            ],
            input: Some(ToolInput {
                name: "in.txt".into(),
                bytes: b"hello".to_vec(),
            }),
        };
        let PluginResult::File(bytes) = run(&tool, &c, ToolPolicy::default()).0.unwrap() else {
            panic!()
        };
        let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            got["argv"],
            serde_json::json!([
                "-i",
                "input.txt",
                "--size=640x480",
                "--note=a b",
                hostile,
                "$(whoami)",
                ";",
                "%{pts}",
                "{unknown}"
            ])
        );
        let env: Vec<&String> = got["env"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| *k != "__CF_USER_TEXT_ENCODING")
            .collect();
        assert_eq!(env, vec!["PATH"]);
        assert_eq!(got["env"]["PATH"], "/usr/bin:/bin");
        let cwd = got["cwd"].as_str().unwrap();
        assert!(Path::new(cwd)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("sitefile-tool-"));
        assert_eq!(got["files"], serde_json::json!(["input.txt"]));
        assert_eq!(got["stdinIsNull"], true);
        assert!(
            !Path::new(cwd).exists(),
            "the temp dir is removed afterwards"
        );
        assert!(!Path::new("/tmp/pwned-by-sitefile").exists());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn failures_timeouts_bad_outputs_and_logs() {
        let Some((d, tool)) = fake_tool() else { return };
        let p = ToolPolicy::default();
        rejects(
            &tool,
            &call(&["fail", "{output}"]),
            p,
            "TOOL fake: exited with code 3\nfirst line\nboom: bad input",
        );
        let t0 = Instant::now();
        rejects(
            &tool,
            &call(&["sleep", "{output}"]),
            ToolPolicy {
                timeout_ms: 300,
                ..p
            },
            "timed out after 300 ms",
        );
        assert!(t0.elapsed() < Duration::from_secs(4));
        rejects(
            &tool,
            &ToolCall {
                dest: "/o.bin".into(),
                ..call(&["nowrite", "{output}"])
            },
            p,
            "did not write {output} (output.bin)",
        );
        rejects(
            &tool,
            &call(&["big", "{output}"]),
            ToolPolicy {
                max_output_bytes: 1000,
                ..p
            },
            "output is larger than 1000 bytes",
        );
        rejects(
            &tool,
            &call(&["symlink", "{output}"]),
            p,
            "not a regular file",
        );
        let (r, logs) = run(&tool, &call(&["noisy", "{output}"]), p);
        assert_eq!(r.unwrap(), PluginResult::File(b"ok".to_vec()));
        assert_eq!(logs, vec!["[tool fake] to stdout", "[tool fake] to stderr"]);
        let (r, _) = run(&tool, &call(&["copy", "{input}", "{output}"]), p);
        assert_eq!(r.unwrap(), PluginResult::File(b"hello".to_vec()));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_tool_that_vanished_is_an_error_not_a_panic() {
        let t = ResolvedTool {
            name: "gone".into(),
            path: "/nonexistent/gone".into(),
            size: 0,
            mtime_ms: 0.0,
        };
        rejects(
            &t,
            &call(&["{output}"]),
            ToolPolicy::default(),
            "TOOL gone: could not start /nonexistent/gone:",
        );
    }
}
