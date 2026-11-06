//! Port of `src/plugins/wasm.ts`: WebAssembly renderer plugins (ABI v1), run
//! with wasmtime.
//!
//! A plugin is a `wasm32-unknown-unknown` module with **no imports**, so it has
//! no way to reach files, the network, the clock or randomness: it can only
//! compute. It exports:
//!
//! ```text
//! memory                                  its linear memory
//! sitefile_alloc(len: i32) -> i32         reserve `len` bytes, return a pointer
//! sitefile_transform(ptr: i32, len: i32) -> i64
//!                                         read the request at ptr..ptr+len and
//!                                         return (resultPtr << 32) | resultLen
//! sitefile_api_version() -> i32           optional; must return 1
//! ```
//!
//! Request and result are UTF-8 JSON (see docs/PLUGINS.md). Each call gets a
//! fresh Store and Instance (no state carries over), linear memory is capped at
//! 1 GiB, and the call is interrupted when it runs past `policy.timeout_ms` of
//! wall-clock time (epoch interruption driven by a ticker thread).
//!
//! Compiling with Cranelift costs ~55 ms, so compiled modules are cached in
//! memory (one compile per plugin per process) and on disk at
//! `$SITEFILE_HOME/cache/wasm/<key>.cwasm` (`SITEFILE_NO_WASM_CACHE=1` turns the
//! disk cache off). The key covers the wasm bytes, wasmtime version, target and
//! codegen settings. Cache files are trusted by `Module::deserialize_file`, so
//! they live in a 0700 directory, are written 0600 via temp file + rename, and a
//! file that fails to load is recompiled and replaced. Import/export checks run
//! on every load, hits included. No cache failure ever fails a build. A hit
//! refreshes the file's mtime: the size limit (`$SITEFILE_CACHE_MAX`, see
//! `cache_limit.rs`) evicts least recently used modules first.

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine, ExternType, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, Trap, UpdateDeadline, Val};

use crate::error::{ErrorCode, Result, SitefileError, SourceSpan};
use crate::store::resolve_store_home;
use crate::plugins::host::{b64_decode_lenient, PluginPolicy, PluginRequest, PluginResult};

/// Largest linear memory a wasm plugin may use (1 GiB).
pub const DEFAULT_MAX_MEMORY: usize = 1024 * 1024 * 1024;

/// How often the ticker advances the engine's epoch while a call runs.
const TICK: Duration = Duration::from_millis(5);

/// One engine for every plugin. Epoch ticks are shared, so each call checks its
/// own wall-clock deadline in the epoch callback.
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut c = Config::new();
        c.epoch_interruption(true);
        Engine::new(&c).expect("wasmtime engine configuration is valid")
    })
}

fn plugin_err(name: &str, why: impl std::fmt::Display, span: Option<&SourceSpan>) -> SitefileError {
    SitefileError::new(ErrorCode::E_PLUGIN, format!("PLUGIN {name}: {why}"), span.cloned())
}

fn kind_name(t: &ExternType) -> &'static str {
    match t {
        ExternType::Func(_) => "function",
        ExternType::Global(_) => "global",
        ExternType::Table(_) => "table",
        ExternType::Memory(_) => "memory",
    }
}

fn compile(name: &str, bytes: &[u8], span: Option<&SourceSpan>) -> Result<Module> {
    COMPILES.with(|c| c.set(c.get() + 1));
    Module::new(engine(), bytes).map_err(|e| plugin_err(name, format!("not a valid WebAssembly module ({})", first_line(&e)), span))
}

thread_local! {
    /// How many times this thread ran Cranelift (tests use it to see cache hits).
    static COMPILES: Cell<usize> = const { Cell::new(0) };
}

/// Set to a non-empty value other than `0` to skip the on-disk compiled-module cache.
pub const NO_CACHE_ENV: &str = "SITEFILE_NO_WASM_CACHE";

/// `$SITEFILE_HOME/cache/wasm`, or `None` when the disk cache is switched off.
/// (Unit tests never touch the real home: they pass their own directory.)
fn disk_cache_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    match std::env::var(NO_CACHE_ENV) {
        Ok(v) if !v.is_empty() && v != "0" => None,
        _ => Some(resolve_store_home().join("cache").join("wasm")),
    }
}

/// Cache key: wasm bytes + everything that changes generated code (wasmtime
/// version, target, codegen settings via the engine's compatibility hash, and
/// the epoch-interruption setting this crate turns on).
fn cache_key(bytes: &[u8]) -> String {
    struct H(Sha256);
    impl Hasher for H {
        fn write(&mut self, bytes: &[u8]) {
            self.0.update(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    let mut h = H(Sha256::new());
    h.0.update(b"sitefile-wasm-cache-v1\0");
    h.0.update(format!("wasmtime {WASMTIME_VERSION}\0{}-{}\0epoch_interruption=true\0", std::env::consts::ARCH, std::env::consts::OS).as_bytes());
    engine().precompile_compatibility_hash().hash(&mut h);
    h.0.update((bytes.len() as u64).to_le_bytes());
    h.0.update(bytes);
    h.0.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Keep in step with the `wasmtime` requirement in Cargo.toml (belt and braces:
/// the engine compatibility hash already includes the real version).
const WASMTIME_VERSION: &str = "29";

/// Make sure `dir` is a private directory we may trust `deserialize_file` on.
#[cfg(unix)]
fn private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !dir.exists() && std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_err() {
        return false;
    }
    match std::fs::symlink_metadata(dir) {
        // A real directory (not a symlink) that nobody else can write into.
        Ok(m) => m.is_dir() && m.mode() & 0o022 == 0,
        Err(_) => false,
    }
}
#[cfg(not(unix))]
fn private_dir(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok()
}

/// Atomic write (temp file + rename), mode 0600.
fn write_entry(dir: &Path, key: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static N: AtomicUsize = AtomicUsize::new(0);
    let tmp = dir.join(format!(".{key}.{}.{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let res = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join(format!("{key}.cwasm")))
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Disk,
    Compiled,
}

/// Load from `dir` when possible, else compile (and try to store). Imports and
/// exports are checked every time, and before anything is written to the cache.
fn load_with_dir(dir: Option<&Path>, name: &str, bytes: &[u8], span: Option<&SourceSpan>) -> Result<(Module, Origin)> {
    let entry = dir.filter(|d| private_dir(d)).map(|d| (d, cache_key(bytes)));
    if let Some((d, key)) = &entry {
        let path = d.join(format!("{key}.cwasm"));
        if std::fs::symlink_metadata(&path).map(|m| m.is_file()).unwrap_or(false) {
            // SAFETY: the file is a regular file in a private (not group/world-writable)
            // directory, under a key covering the engine settings; wasmtime also rejects
            // artifacts from another version/config. Any failure falls back to compiling.
            if let Ok(module) = unsafe { Module::deserialize_file(engine(), &path) } {
                check_module(name, &module, span)?;
                // A hit: the mtime is the recency the cache size limit goes by (cache_limit.rs).
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                    let _ = f.set_modified(std::time::SystemTime::now());
                }
                return Ok((module, Origin::Disk));
            }
        }
    }
    let module = compile(name, bytes, span)?;
    check_module(name, &module, span)?;
    if let Some((d, key)) = &entry {
        if let Ok(data) = module.serialize() {
            let _ = write_entry(d, key, &data);
        }
    }
    Ok((module, Origin::Compiled))
}

/// One compile per distinct plugin per process; later loads clone the handle.
fn load_module(name: &str, bytes: &[u8], span: Option<&SourceSpan>) -> Result<Module> {
    static MEMO: Mutex<Option<HashMap<String, Module>>> = Mutex::new(None);
    let key = crate::digest::sha256(bytes);
    if let Some(m) = MEMO.lock().ok().and_then(|g| g.as_ref().and_then(|m| m.get(&key).cloned())) {
        return Ok(m);
    }
    let (module, _) = load_with_dir(disk_cache_dir().as_deref(), name, bytes, span)?;
    if let Ok(mut g) = MEMO.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, module.clone());
    }
    Ok(module)
}

fn first_line(e: &wasmtime::Error) -> String {
    // wasmtime's messages carry context chains; keep the root reason short.
    let s = format!("{:#}", e);
    s.lines().next().unwrap_or("").to_string()
}

fn check_module(name: &str, module: &Module, span: Option<&SourceSpan>) -> Result<()> {
    let imports: Vec<String> = module.imports().map(|i| format!("{}.{} ({})", i.module(), i.name(), kind_name(&i.ty()))).collect();
    if !imports.is_empty() {
        return Err(plugin_err(
            name,
            format!(
                "WebAssembly plugins can't import anything, so they can't reach files, the network or the clock; this module imports {}",
                imports.join(", ")
            ),
            span,
        ));
    }
    for (ename, kind) in [("memory", "memory"), ("sitefile_alloc", "function"), ("sitefile_transform", "function")] {
        let got = module.exports().find(|e| e.name() == ename).map(|e| kind_name(&e.ty()));
        if got != Some(kind) {
            return Err(plugin_err(name, format!("a WebAssembly plugin must export {kind} '{ename}' (see docs/PLUGINS.md)"), span));
        }
    }
    Ok(())
}

/// Compile and check a module against ABI v1; E_PLUGIN with a precise reason.
pub fn check_wasm_plugin(name: &str, bytes: &[u8], span: Option<&SourceSpan>) -> Result<()> {
    load_module(name, bytes, span).map(|_| ())
}

struct Ctx {
    limits: StoreLimits,
}

pub struct WasmPlugin {
    pub name: String,
    bytes: Vec<u8>,
    module: Option<Module>,
    policy: PluginPolicy,
}

impl WasmPlugin {
    /// No imports means no clock or randomness: results depend only on the request.
    pub const CACHEABLE: bool = true;

    pub fn new(name: impl Into<String>, bytes: Vec<u8>, policy: PluginPolicy) -> Self {
        WasmPlugin { name: name.into(), bytes, module: None, policy }
    }

    pub fn cacheable(&self) -> bool {
        Self::CACHEABLE
    }

    pub fn call(&mut self, request: &PluginRequest, span: Option<&SourceSpan>) -> Result<PluginResult> {
        self.prepare(span)?;
        self.call_prepared(request, span)
    }

    /// Compile the module (once), so `call_prepared` can run calls from several threads.
    pub fn prepare(&mut self, span: Option<&SourceSpan>) -> Result<()> {
        if self.module.is_none() {
            self.module = Some(load_module(&self.name, &self.bytes, span)?);
        }
        Ok(())
    }

    /// One call on the compiled module (`prepare` first): a fresh instance, so calls may run in parallel.
    pub fn call_prepared(&self, request: &PluginRequest, span: Option<&SourceSpan>) -> Result<PluginResult> {
        let json = request.to_wire_string().into_bytes();
        let Some(module) = self.module.as_ref() else {
            return Err(plugin_err(&self.name, "the module is not compiled", span));
        };
        let out = run(module, &json, &self.policy).map_err(|why| plugin_err(&self.name, why, span))?;
        self.parse(&out, span)
    }

    fn parse(&self, out: &[u8], span: Option<&SourceSpan>) -> Result<PluginResult> {
        let bad = |why: String| plugin_err(&self.name, why, span);
        if out.len() > self.policy.max_output_bytes {
            return Err(bad(format!("result is larger than {} bytes", self.policy.max_output_bytes)));
        }
        let text = std::str::from_utf8(out).map_err(|e| bad(format!("the result is not UTF-8 JSON ({e})")))?;
        // TextDecoder drops a leading BOM before JSON.parse sees the text.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let msg: Value = serde_json::from_str(text).map_err(|e| bad(format!("the result is not UTF-8 JSON ({e})")))?;
        let Value::Object(r) = msg else {
            return Err(bad("the result must be a JSON object".into()));
        };
        if let Some(Value::String(e)) = r.get("error") {
            return Err(bad(e.clone()));
        }
        let b64 = |v: &Value, what: &str| -> Result<Vec<u8>> {
            match v {
                Value::String(s) if is_b64_shape(s) => Ok(b64_decode_lenient(s)),
                _ => Err(bad(format!("{what} must be a base64 string"))),
            }
        };
        if let Some(v) = r.get("bytes") {
            return Ok(PluginResult::File(b64(v, "bytes")?));
        }
        if let Some(Value::String(t)) = r.get("text") {
            return Ok(PluginResult::File(t.as_bytes().to_vec()));
        }
        if let Some(Value::Object(files)) = r.get("files") {
            let mut out = Vec::new();
            for k in crate::plugins::host::js_keys(files) {
                out.push((k.clone(), b64(&files[k.as_str()], &format!("files[\"{k}\"]"))?));
            }
            return Ok(PluginResult::Files(out));
        }
        Err(bad("the result must have \"bytes\", \"text\", \"files\" or \"error\"".into()))
    }

    pub fn close(&mut self) {
        self.module = None;
    }
}

/// `/^[A-Za-z0-9+/]*={0,2}$/`
fn is_b64_shape(s: &str) -> bool {
    let body = s.trim_end_matches('=');
    s.len() - body.len() <= 2 && body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Runs one call in a fresh store; the error is the reason text (without the `PLUGIN name:` prefix).
fn run(module: &Module, request: &[u8], policy: &PluginPolicy) -> std::result::Result<Vec<u8>, String> {
    let engine = engine();
    let limits = StoreLimitsBuilder::new().memory_size(DEFAULT_MAX_MEMORY).instances(1).build();
    let mut store = Store::new(engine, Ctx { limits });
    store.limiter(|c| &mut c.limits);
    let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |_| {
        if Instant::now() >= deadline {
            Err(Trap::Interrupt.into())
        } else {
            Ok(UpdateDeadline::Continue(1))
        }
    });

    // The ticker advances the shared epoch until the call returns.
    let (stop, stopped) = mpsc::channel::<()>();
    let ticker = {
        let engine = engine.clone();
        thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(TICK) {
                engine.increment_epoch();
            }
        })
    };
    let result = call_abi(&mut store, module, request);
    drop(stop);
    let _ = ticker.join();
    result.map_err(|e| describe(&e, policy))
}

fn describe(e: &wasmtime::Error, policy: &PluginPolicy) -> String {
    if let Some(t) = e.downcast_ref::<Trap>() {
        if *t == Trap::Interrupt {
            return format!("timed out after {} ms", policy.timeout_ms);
        }
        return t.to_string();
    }
    if let Some(m) = e.downcast_ref::<AbiError>() {
        return m.0.clone();
    }
    first_line(e)
}

#[derive(Debug)]
struct AbiError(String);
impl std::fmt::Display for AbiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AbiError {}

fn abi(msg: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::new(AbiError(msg.into()))
}

fn call_abi(store: &mut Store<Ctx>, module: &Module, request: &[u8]) -> wasmtime::Result<Vec<u8>> {
    let instance = Instance::new(&mut *store, module, &[])?;
    if let Some(f) = instance.get_func(&mut *store, "sitefile_api_version") {
        let mut out = vec![Val::I32(0); f.ty(&*store).results().len()];
        f.call(&mut *store, &[], &mut out)?;
        if !matches!(out.first(), Some(Val::I32(1))) {
            return Err(abi("sitefile_api_version() must return 1"));
        }
    }
    let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| abi("the module must export memory 'memory'"))?;
    let alloc = instance.get_typed_func::<i32, i32>(&mut *store, "sitefile_alloc")?;
    let transform = instance.get_typed_func::<(i32, i32), i64>(&mut *store, "sitefile_transform")?;
    let len = i32::try_from(request.len()).map_err(|_| abi("the request is larger than 2 GiB"))?;
    let ptr = alloc.call(&mut *store, len)? as u32 as usize;
    if ptr + request.len() > memory.data_size(&*store) {
        return Err(abi("sitefile_alloc returned a pointer outside memory"));
    }
    memory.data_mut(&mut *store)[ptr..ptr + request.len()].copy_from_slice(request);
    let packed = transform.call(&mut *store, (ptr as i32, len))? as u64;
    let out_ptr = (packed >> 32) as usize;
    let out_len = (packed & 0xffff_ffff) as usize;
    let size = memory.data_size(&*store);
    if size > DEFAULT_MAX_MEMORY {
        return Err(abi(format!("used {size} bytes of memory, more than the {DEFAULT_MAX_MEMORY} allowed")));
    }
    if out_ptr + out_len > size {
        return Err(abi("sitefile_transform returned a result outside memory"));
    }
    Ok(memory.data(&*store)[out_ptr..out_ptr + out_len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::host::{PluginHost, PluginInput};

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }
    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(root().join("test/fixtures/wasm").join(format!("{name}.wasm"))).unwrap()
    }
    fn badge() -> Vec<u8> {
        std::fs::read(root().join("examples/wasm/plugins/badge.wasm")).unwrap()
    }
    fn req(dest: &str, options: &[(&str, &str)]) -> PluginRequest {
        PluginRequest {
            dest: dest.into(),
            options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }
    fn call(bytes: Vec<u8>, r: &PluginRequest, policy: PluginPolicy) -> Result<PluginResult> {
        check_wasm_plugin("p", &bytes, None)?;
        WasmPlugin::new("p", bytes, policy).call(r, None)
    }
    fn text(r: PluginResult) -> String {
        match r {
            PluginResult::File(b) => String::from_utf8(b).unwrap(),
            other => panic!("expected one file, got {other:?}"),
        }
    }
    fn fails(bytes: Vec<u8>, needle: &str, policy: PluginPolicy) {
        let e = call(bytes, &req("/out.txt", &[]), policy).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN, "{}", e.message());
        assert!(e.message().contains(needle), "{:?} does not contain {needle:?}", e.message());
        assert!(e.message().starts_with("PLUGIN p: "), "{}", e.message());
    }

    #[test]
    fn returns_text() {
        assert_eq!(text(call(fixture("const"), &req("/out.txt", &[]), PluginPolicy::default()).unwrap()), "hello from wasm");
    }

    #[test]
    fn the_request_reaches_the_module() {
        let mut r = req("/out.txt", &[("k", "v")]);
        r.input = Some(PluginInput::new("./a.txt", "a.txt", b"abc".to_vec()));
        let n = r.to_wire_string().len() % 10;
        assert_eq!(text(call(fixture("echo-len"), &r, PluginPolicy::default()).unwrap()), n.to_string());
    }

    #[test]
    fn the_rust_badge_plugin() {
        let svg = text(call(badge(), &req("/static.svg", &[("label", "build"), ("value", "passing <ok>"), ("color", "#4c1")]), PluginPolicy::default()).unwrap());
        assert!(svg.starts_with("<svg "), "{svg}");
        assert!(svg.contains("aria-label=\"build: passing &lt;ok&gt;\""), "{svg}");
        assert!(!svg.contains("<ok>"));
        let mut r = req("/read/a.svg", &[]);
        r.item = Some(serde_json::json!({ "slug": "a", "title": "A", "content": format!("<p>{}</p>", "word ".repeat(450)) }));
        let svg = text(call(badge(), &r, PluginPolicy::default()).unwrap());
        assert!(svg.contains("3 min · 450 words"), "{svg}");
        let e = call(badge(), &req("/x.svg", &[("value", "x"), ("color", "red;x")]), PluginPolicy::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN);
        assert!(e.message().contains("option \"color\""), "{}", e.message());
    }

    #[test]
    fn modules_that_break_the_abi_are_rejected() {
        let e = check_wasm_plugin("p", &fixture("imports"), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_PLUGIN);
        assert!(e.message().contains("can't import anything"), "{}", e.message());
        assert!(e.message().contains("env.now"), "{}", e.message());
        let e = check_wasm_plugin("p", &fixture("noexport"), None).unwrap_err();
        assert!(e.message().contains("must export function 'sitefile_transform'"), "{}", e.message());
        let e = check_wasm_plugin("p", b"not wasm", Some(&SourceSpan::new("Sitefile", 2, 1))).unwrap_err();
        assert!(e.message().starts_with("Sitefile:2:1: PLUGIN p: not a valid WebAssembly module ("), "{}", e.message());
    }

    #[test]
    fn traps_bad_results_errors_and_timeouts() {
        let p = PluginPolicy::default();
        fails(fixture("trap"), "unreachable", p);
        fails(fixture("oob"), "outside memory", p);
        fails(fixture("badjson"), "not UTF-8 JSON", p);
        fails(fixture("error"), "boom: bad input", p);
        let t0 = Instant::now();
        fails(fixture("loop"), "timed out after 300 ms", PluginPolicy { timeout_ms: 300, ..p });
        assert!(t0.elapsed() < Duration::from_secs(3));
        fails(fixture("const"), "result is larger than 3 bytes", PluginPolicy { max_output_bytes: 3, ..p });
    }

    #[test]
    fn result_shapes() {
        let w = WasmPlugin::new("p", Vec::new(), PluginPolicy::default());
        assert_eq!(w.parse(br#"{"bytes":"aGk="}"#, None).unwrap(), PluginResult::File(b"hi".to_vec()));
        assert_eq!(w.parse(br#"{"bytes":"aGk"}"#, None).unwrap(), PluginResult::File(b"hi".to_vec()));
        assert_eq!(
            w.parse(br#"{"files":{"b.txt":"Qg==","a.txt":"QQ=="}}"#, None).unwrap(),
            PluginResult::Files(vec![("b.txt".into(), b"B".to_vec()), ("a.txt".into(), b"A".to_vec())])
        );
        let msg = |b: &[u8]| w.parse(b, None).unwrap_err().message();
        assert!(msg(br#"{"bytes":"%%"}"#).contains("bytes must be a base64 string"));
        assert!(msg(br#"{"files":{"x":1}}"#).contains("files[\"x\"] must be a base64 string"));
        assert!(msg(b"[1]").contains("must be a JSON object"));
        assert!(msg(b"{}").contains("must have \"bytes\", \"text\", \"files\" or \"error\""));
        assert!(msg(&[0xff, 0xfe]).contains("not UTF-8 JSON"));
    }

    #[test]
    fn the_host_runs_wasm_plugins_and_counts_them() {
        let mut host = PluginHost::new(PluginPolicy::default(), |_| {});
        let bytes = fixture("const");
        host.register("p", "/nowhere/p.wasm", None, Some(bytes.clone()));
        assert!(host.is_wasm("p"));
        assert_eq!(host.wasm_bytes("p"), Some(bytes.as_slice()));
        assert_eq!(host.folder_of("p"), Some(PathBuf::from("/nowhere")));
        assert_eq!(text(host.call("p", &req("/o", &[]), None).unwrap()), "hello from wasm");
        assert_eq!(text(host.call("p", &req("/o", &[]), None).unwrap()), "hello from wasm");
        assert!(host.cacheable("p"));
        assert_eq!((host.stats.spawned, host.stats.calls), (1, 2));
        let e = host.call("nope", &req("/o", &[]), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::E_UNDEFINED);
        host.close();
    }

    fn tmp(prefix: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("sitefile-wasm-{prefix}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
    fn compiles() -> usize {
        COMPILES.with(|c| c.get())
    }

    #[test]
    fn disk_cache_miss_then_hit_then_corrupt_fallback() {
        let dir = tmp("hit").join("cache");
        let bytes = fixture("const");
        let c0 = compiles();
        let (_, o) = load_with_dir(Some(&dir), "p", &bytes, None).unwrap();
        assert_eq!((o, compiles() - c0), (Origin::Compiled, 1));
        let file = dir.join(format!("{}.cwasm", cache_key(&bytes)));
        assert!(file.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::OpenOptions::new().write(true).open(&file).unwrap().set_modified(old).unwrap();
        let (m, o) = load_with_dir(Some(&dir), "p", &bytes, None).unwrap();
        assert_eq!((o, compiles() - c0), (Origin::Disk, 1), "a hit must not compile");
        let touched = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(touched > old + std::time::Duration::from_secs(60), "a hit refreshes the mtime (LRU recency for the size limit)");
        let out = run(&m, b"{}", &PluginPolicy::default()).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("hello from wasm"));
        // Corrupt entry: falls back to compiling and rewrites a good file.
        std::fs::write(&file, b"garbage").unwrap();
        let (_, o) = load_with_dir(Some(&dir), "p", &bytes, None).unwrap();
        assert_eq!((o, compiles() - c0), (Origin::Compiled, 2));
        assert_eq!(load_with_dir(Some(&dir), "p", &bytes, None).unwrap().1, Origin::Disk);
        assert_ne!(cache_key(&bytes), cache_key(&fixture("trap")));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn disk_hits_still_check_the_module_and_bad_modules_are_never_cached() {
        let dir = tmp("check").join("cache");
        let e = load_with_dir(Some(&dir), "p", &fixture("imports"), None).unwrap_err();
        assert!(e.message().contains("can't import anything"), "{}", e.message());
        assert!(load_with_dir(Some(&dir), "p", b"not wasm", None).is_err());
        assert_eq!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0, "invalid modules must not be cached");
        // Even a planted entry for a bad module is checked on the hit.
        let bytes = fixture("imports");
        let m = Module::new(engine(), &bytes).unwrap();
        write_entry(&dir, &cache_key(&bytes), &m.serialize().unwrap()).unwrap();
        let e = load_with_dir(Some(&dir), "p", &bytes, None).unwrap_err();
        assert!(e.message().contains("can't import anything"), "{}", e.message());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn an_unusable_cache_dir_never_fails_the_build() {
        let base = tmp("ro");
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let (_, o) = load_with_dir(Some(&blocker.join("cache")), "p", &fixture("const"), None).unwrap();
        assert_eq!(o, Origin::Compiled);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = base.join("ro");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
            let (_, o) = load_with_dir(Some(&ro.join("cache")), "p", &fixture("const"), None).unwrap();
            assert_eq!(o, Origin::Compiled);
            // A group/world-writable cache dir is not trusted, so nothing is read or written.
            let open = base.join("open");
            std::fs::create_dir(&open).unwrap();
            std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
            load_with_dir(Some(&open), "p", &fixture("const"), None).unwrap();
            assert_eq!(std::fs::read_dir(&open).unwrap().count(), 0);
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(load_with_dir(None, "p", &fixture("const"), None).unwrap().1, Origin::Compiled);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn one_compile_per_plugin_per_process() {
        let bytes = fixture("echo-len");
        let c0 = compiles();
        check_wasm_plugin("p", &bytes, None).unwrap();
        let mut a = WasmPlugin::new("p", bytes.clone(), PluginPolicy::default());
        let mut b = WasmPlugin::new("p", bytes, PluginPolicy::default());
        a.call(&req("/o", &[]), None).unwrap();
        b.call(&req("/o", &[]), None).unwrap();
        // Other tests on other threads may have loaded the same fixture first.
        assert!(compiles() - c0 <= 1, "compiled {} times", compiles() - c0);
    }
}
