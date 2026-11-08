//! Port of `src/plugins/cache.ts`: a content-addressed cache of TRANSFORM results
//! (see docs/PLUGINS.md, "Caching").
//!
//! ```text
//! <dir>/entries/<key>.json   manifest: which blobs make up the result, and
//!                            the PLUGIN or TOOL that produced it ("plugin")
//! <dir>/blobs/<hex>          result bytes, named by their SHA-256
//! ```
//!
//! `"plugin"` is optional (entries written before it was recorded have none and
//! are still read); it is not part of the key, so recording it changed no key.
//!
//! The key is the SHA-256 of a canonical JSON document describing everything the
//! result can depend on. It is byte-for-byte the document the TypeScript version
//! hashes (`JSON.stringify` number formatting, string escaping and JavaScript's
//! property order are reproduced), so both CLIs share one cache. Reads verify
//! every blob's digest and size; a damaged entry is removed and counts as a miss.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::digest::sha256;
use crate::plugins::host::PluginResult;

/// Bump to invalidate every existing entry when the key or entry format changes.
pub const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    /// Entries found damaged (and discarded).
    pub corrupt: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInfo {
    pub dir: PathBuf,
    pub entries: u64,
    pub blobs: u64,
    pub bytes: u64,
}

/// What one plugin's entries hold ([`TransformCache::usage_by_plugin`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginCacheUsage {
    /// The PLUGIN or TOOL name; `None` for entries that do not record one
    /// (written before names were recorded, or fetched from a registry cache
    /// without one).
    pub plugin: Option<String>,
    pub entries: u64,
    /// Distinct blobs these entries reference (a blob shared with another
    /// plugin's entry counts for both).
    pub blobs: u64,
    /// Size of those blobs.
    pub bytes: u64,
}

/// What [`TransformCache::prune`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneResult {
    pub entries: u64,
    pub blobs: u64,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// JSON exactly as JavaScript's JSON.stringify writes it
//
// Kept here rather than using `crate::values::js_json_stringify`, which (as of
// this port) differs from JSON.stringify in three ways the cache-key fixture
// catches: array-index keys ("9", "10") are not iterated first, `__proto__`
// is not dropped by canonicalization, and 1.5e-7 prints as "0.00000015"
// instead of "1.5e-7". Once values.rs matches, these can delegate to it.

/// `Number.prototype.toString()` for a finite double (ECMA-262 Number::toString).
pub fn js_number(x: f64) -> String {
    if !x.is_finite() {
        return "null".into(); // JSON.stringify writes NaN and ±Infinity as null
    }
    if x == 0.0 {
        return "0".into(); // also -0
    }
    if x < 0.0 {
        return format!("-{}", js_number(-x));
    }
    // Rust's `{:e}` gives the shortest digits that round-trip, like ECMAScript.
    let e = format!("{x:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let n = exp + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let d = n - 1;
        let sign = if d < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{sign}{}", d.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], d.abs())
        }
    }
}

fn js_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A property name JavaScript treats as an array index (iterated first, in numeric order).
fn array_index(k: &str) -> Option<u32> {
    if k.is_empty()
        || k.len() > 10
        || !k.bytes().all(|b| b.is_ascii_digit())
        || (k.len() > 1 && k.starts_with('0'))
    {
        return None;
    }
    k.parse::<u64>()
        .ok()
        .filter(|n| *n < u32::MAX as u64)
        .map(|n| n as u32)
}

fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Keys in the order a JavaScript object built from them iterates: array
/// indices ascending, then the rest (sorted when `sort`, else as given).
/// `__proto__` is dropped: assigning it to a plain object sets the prototype.
pub(crate) fn js_key_order(map: &Map<String, Value>, sort: bool) -> Vec<&String> {
    let mut idx: Vec<(u32, &String)> = Vec::new();
    let mut rest: Vec<&String> = Vec::new();
    for k in map.keys() {
        if k == "__proto__" && sort {
            continue;
        }
        match array_index(k) {
            Some(i) => idx.push((i, k)),
            None => rest.push(k),
        }
    }
    idx.sort_by_key(|(i, _)| *i);
    if sort {
        rest.sort_by(|a, b| utf16_cmp(a, b));
    }
    idx.into_iter().map(|(_, k)| k).chain(rest).collect()
}

fn write_js(v: &Value, sort: bool, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&js_number(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => js_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js(x, sort, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, k) in js_key_order(m, sort).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                js_string(k, out);
                out.push(':');
                write_js(&m[k.as_str()], sort, out);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(value)` for a value that came from JSON (keeps key order,
/// apart from JavaScript moving array-index keys first).
pub fn js_json(value: &Value) -> String {
    let mut s = String::new();
    write_js(value, false, &mut s);
    s
}

/// JSON with sorted object keys, so equal values always serialize equally
/// (identical to `canonicalJson` in cache.ts).
pub fn canonical_json(value: &Value) -> String {
    let mut s = String::new();
    write_js(value, true, &mut s);
    s
}

/// SHA-256 hex of `canonical_json({ cacheVersion, ...parts })`.
/// `parts` should be an object; anything else is hashed as `{ cacheVersion }`
/// plus nothing, like spreading a non-object in JavaScript.
pub fn cache_key(parts: &Value) -> String {
    let mut m = Map::new();
    m.insert("cacheVersion".into(), json!(CACHE_VERSION));
    if let Value::Object(p) = parts {
        for (k, v) in p {
            m.insert(k.clone(), v.clone());
        }
    }
    let doc = canonical_json(&Value::Object(m));
    let d = Sha256::digest(doc.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Node's `stats.mtimeMs` for a file's metadata (`sec * 1000 + nsec / 1e6`).
#[cfg(unix)]
pub fn mtime_ms(meta: &fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as f64 * 1000.0 + meta.mtime_nsec() as f64 / 1_000_000.0
}

#[cfg(not(unix))]
pub fn mtime_ms(meta: &fs::Metadata) -> f64 {
    match meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    {
        Some(d) => d.as_secs() as f64 * 1000.0 + d.subsec_nanos() as f64 / 1_000_000.0,
        None => 0.0,
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ---------------------------------------------------------------------------

/// What an entry records: the result kind (`file` or `files`) and
/// `(path, blob hex, size)` per file (a `file` result has one, with path '').
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub kind: String,
    pub files: Vec<(String, String, u64)>,
}

impl CacheEntry {
    /// `{"version":1,"kind":…,"files":[…]}`, then `"plugin":…` when named (as cache.ts writes it).
    fn manifest_json(&self, plugin: Option<&str>) -> Value {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|(p, h, n)| json!([p, h, n]))
            .collect();
        let mut m = json!({ "version": CACHE_VERSION, "kind": self.kind, "files": files });
        if let (Some(name), Value::Object(o)) = (plugin, &mut m) {
            o.insert("plugin".into(), json!(name));
        }
        m
    }
}

/// The `"plugin"` an entry manifest records, if any.
fn manifest_plugin(m: &Value) -> Option<String> {
    m.get("plugin").and_then(Value::as_str).map(str::to_string)
}

/// The blob hex digests a manifest references, or `None` when it is not one.
fn manifest_blobs(m: &Value) -> Option<Vec<String>> {
    m.get("files")?
        .as_array()?
        .iter()
        .map(|f| f.get(1).and_then(Value::as_str).map(String::from))
        .collect()
}

/// A second-level cache consulted on a local miss (`--cache-from`, see
/// `crate::registry_cache`), and told about every entry this build used or
/// wrote (`--cache-to`).
pub trait RemoteCache: Send + Sync {
    /// Fetch the entry for `key` and its blobs (written verified into `blob_dir/<hex>`). Never fails.
    fn fetch(&self, key: &str, blob_dir: &Path) -> Option<CacheEntry>;
    fn used(&self, key: &str, entry: &CacheEntry);
}

pub struct TransformCache {
    pub dir: PathBuf,
    pub stats: CacheStats,
    remote: Option<std::sync::Arc<dyn RemoteCache>>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TransformCache {
    /// `dir` holds `entries/` and `blobs/` (the CLI uses `$SITEFILE_HOME/cache/transform`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        TransformCache {
            dir: dir.into(),
            stats: CacheStats::default(),
            remote: None,
        }
    }

    /// Consult `remote` on a local miss and report every entry used or written to it.
    pub fn with_remote(mut self, remote: Option<std::sync::Arc<dyn RemoteCache>>) -> Self {
        self.remote = remote;
        self
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join("entries").join(format!("{key}.json"))
    }
    fn blob_path(&self, hex: &str) -> PathBuf {
        self.dir.join("blobs").join(hex)
    }

    pub fn get(&mut self, key: &str) -> Option<PluginResult> {
        self.get_for(key, None)
    }

    /// [`get`](Self::get) for a TRANSFORM by `plugin` (the PLUGIN or TOOL name):
    /// an entry fetched from the remote cache is recorded locally under that name.
    pub fn get_for(&mut self, key: &str, plugin: Option<&str>) -> Option<PluginResult> {
        let file = self.entry_path(key);
        let raw = match fs::read_to_string(&file) {
            Ok(r) => r,
            Err(_) => {
                let fetched = self
                    .remote
                    .as_ref()
                    .and_then(|r| r.fetch(key, &self.dir.join("blobs")));
                let Some(entry) = fetched else {
                    self.stats.misses += 1;
                    return None;
                };
                let raw = js_json(&entry.manifest_json(plugin));
                let _ = write_atomic(&file, raw.as_bytes(), false);
                raw
            }
        };
        match self.load(&raw) {
            Some((result, entry)) => {
                self.stats.hits += 1;
                // Mark the entry as used (prune --older-than goes by mtime).
                if let Ok(f) = fs::OpenOptions::new().write(true).open(&file) {
                    let _ = f.set_modified(SystemTime::now());
                }
                if let Some(r) = &self.remote {
                    r.used(key, &entry);
                }
                Some(result)
            }
            None => {
                self.stats.corrupt += 1;
                self.stats.misses += 1;
                let _ = fs::remove_file(&file);
                None
            }
        }
    }

    fn load(&self, raw: &str) -> Option<(PluginResult, CacheEntry)> {
        let m: Value = serde_json::from_str(raw).ok()?;
        let m = m.as_object()?;
        if m.get("version").and_then(Value::as_f64) != Some(CACHE_VERSION as f64) {
            return None;
        }
        let kind = m.get("kind").and_then(Value::as_str)?;
        if kind != "file" && kind != "files" {
            return None;
        }
        let files = m.get("files").and_then(Value::as_array)?;
        if kind == "file" && files.len() != 1 {
            return None;
        }
        let mut loaded: Vec<(String, Vec<u8>)> = Vec::new();
        let mut entry = CacheEntry {
            kind: kind.to_string(),
            files: Vec::new(),
        };
        for f in files {
            let f = f.as_array()?;
            let p = f.first().and_then(Value::as_str)?;
            let hex = f.get(1).and_then(Value::as_str).filter(|h| is_hex64(h))?;
            let size = f.get(2).and_then(Value::as_f64)?;
            let bytes = fs::read(self.blob_path(hex)).ok()?;
            if bytes.len() as f64 != size || sha256(&bytes) != format!("sha256:{hex}") {
                let _ = fs::remove_file(self.blob_path(hex));
                return None;
            }
            entry
                .files
                .push((p.to_string(), hex.to_string(), bytes.len() as u64));
            loaded.push((p.to_string(), bytes));
        }
        let result = if kind == "file" {
            loaded.pop().map(|(_, b)| PluginResult::File(b))?
        } else {
            PluginResult::Files(loaded)
        };
        Some((result, entry))
    }

    /// Store a result. Never fails: a cache that cannot be written (read-only
    /// home, full disk) must not fail the build.
    pub fn put(&mut self, key: &str, result: &PluginResult) {
        self.put_for(key, None, result)
    }

    /// [`put`](Self::put), recording the PLUGIN or TOOL that produced the result
    /// (so [`usage_by_plugin`](Self::usage_by_plugin) and
    /// [`prune_plugin`](Self::prune_plugin) can find it).
    pub fn put_for(&mut self, key: &str, plugin: Option<&str>, result: &PluginResult) {
        if let Ok(entry) = self.try_put(key, plugin, result) {
            self.stats.writes += 1;
            if let Some(r) = &self.remote {
                r.used(key, &entry);
            }
        }
    }

    fn try_put(
        &self,
        key: &str,
        plugin: Option<&str>,
        result: &PluginResult,
    ) -> io::Result<CacheEntry> {
        let (kind, pairs): (&str, Vec<(&str, &[u8])>) = match result {
            PluginResult::File(b) => ("file", vec![("", b.as_slice())]),
            PluginResult::Files(fs) => (
                "files",
                fs.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect(),
            ),
        };
        let mut entry = CacheEntry {
            kind: kind.to_string(),
            files: Vec::new(),
        };
        for (p, bytes) in pairs {
            let digest = sha256(bytes);
            let hex = &digest["sha256:".len()..];
            write_atomic(&self.blob_path(hex), bytes, true)?;
            entry
                .files
                .push((p.to_string(), hex.to_string(), bytes.len() as u64));
        }
        write_atomic(
            &self.entry_path(key),
            js_json(&entry.manifest_json(plugin)).as_bytes(),
            false,
        )?;
        Ok(entry)
    }

    pub fn info(&self) -> CacheInfo {
        let entries = read_names(&self.dir.join("entries"))
            .iter()
            .filter(|n| n.ends_with(".json"))
            .count() as u64;
        let mut blobs = 0;
        let mut bytes = 0;
        for n in read_names(&self.dir.join("blobs")) {
            if let Ok(st) = fs::metadata(self.dir.join("blobs").join(&n)) {
                if st.is_file() {
                    blobs += 1;
                    bytes += st.len();
                }
            }
        }
        CacheInfo {
            dir: self.dir.clone(),
            entries,
            blobs,
            bytes,
        }
    }

    /// Remove entries not used for `older_than` (zero removes all), then every
    /// blob no remaining entry references.
    pub fn prune(&self, older_than: Duration) -> PruneResult {
        self.prune_at(older_than, SystemTime::now())
    }

    /// [`prune`](Self::prune) with an explicit "now".
    pub fn prune_at(&self, older_than: Duration, now: SystemTime) -> PruneResult {
        self.prune_where(older_than, now, None)
    }

    /// Remove the entries `plugin` (a PLUGIN or TOOL name) wrote that were not
    /// used for `older_than` (zero: all of them), then every blob no remaining
    /// entry references. Entries of other plugins, and entries that record no
    /// plugin, are kept.
    pub fn prune_plugin(&self, plugin: &str, older_than: Duration) -> PruneResult {
        self.prune_where(older_than, SystemTime::now(), Some(plugin))
    }

    /// Entries, blobs and bytes per plugin, sorted by name (unnamed entries
    /// first). Reads every entry manifest (no blob is read).
    pub fn usage_by_plugin(&self) -> Vec<PluginCacheUsage> {
        let mut by: std::collections::BTreeMap<
            Option<String>,
            (u64, std::collections::BTreeSet<String>),
        > = std::collections::BTreeMap::new();
        let entries = self.dir.join("entries");
        for n in read_names(&entries) {
            if !n.ends_with(".json") {
                continue;
            }
            let Some(m) = fs::read_to_string(entries.join(&n))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            else {
                continue;
            };
            let Some(blobs) = manifest_blobs(&m) else {
                continue;
            };
            let slot = by.entry(manifest_plugin(&m)).or_default();
            slot.0 += 1;
            slot.1.extend(blobs);
        }
        by.into_iter()
            .map(|(plugin, (entries, blobs))| {
                let mut u = PluginCacheUsage {
                    plugin,
                    entries,
                    ..Default::default()
                };
                for hex in blobs {
                    if let Ok(st) = fs::metadata(self.blob_path(&hex)) {
                        if st.is_file() {
                            u.blobs += 1;
                            u.bytes += st.len();
                        }
                    }
                }
                u
            })
            .collect()
    }

    fn prune_where(
        &self,
        older_than: Duration,
        now: SystemTime,
        plugin: Option<&str>,
    ) -> PruneResult {
        let older_ms = older_than.as_secs_f64() * 1000.0;
        let now_ms = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let mut out = PruneResult::default();
        let mut live = std::collections::HashSet::new();
        let mut removed = std::collections::HashSet::new();
        let entries = self.dir.join("entries");
        for n in read_names(&entries) {
            let file = entries.join(&n);
            let Ok(st) = fs::metadata(&file) else {
                continue;
            };
            let old = older_ms == 0.0 || now_ms - mtime_ms(&st) >= older_ms;
            if plugin.is_none() && (old || !n.ends_with(".json")) {
                let _ = remove_any(&file);
                out.entries += 1;
                continue;
            }
            let manifest: Option<Value> = fs::read_to_string(&file)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            if let Some(name) = plugin {
                // Only this plugin's entries go; anything else (even unreadable) is left alone.
                let Some(m) = manifest.as_ref() else {
                    continue;
                };
                let refs = manifest_blobs(m).unwrap_or_default();
                if old && manifest_plugin(m).is_some_and(|p| p == name) {
                    let _ = remove_any(&file);
                    out.entries += 1;
                    removed.extend(refs);
                } else {
                    live.extend(refs);
                }
                continue;
            }
            let refs = manifest.as_ref().and_then(manifest_blobs);
            match refs {
                Some(r) => live.extend(r),
                None => {
                    let _ = remove_any(&file);
                    out.entries += 1;
                }
            }
        }
        let blobs = self.dir.join("blobs");
        for n in read_names(&blobs) {
            // A plugin's prune deletes only blobs its removed entries used (not one
            // another build is writing, whose entry has not landed yet).
            if live.contains(&n) || (plugin.is_some() && !removed.contains(&n)) {
                continue;
            }
            let file = blobs.join(&n);
            let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let _ = remove_any(&file);
            out.blobs += 1;
            out.bytes += size;
        }
        out
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

fn read_names(dir: &Path) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn write_atomic(file: &Path, data: &[u8], keep_if_same_size: bool) -> io::Result<()> {
    if keep_if_same_size {
        if let Ok(st) = fs::metadata(file) {
            if st.is_file() && st.len() == data.len() as u64 {
                // Reused: refresh its mtime so the size limit's grace period covers it until the entry lands.
                if let Ok(f) = fs::OpenOptions::new().write(true).open(file) {
                    let _ = f.set_modified(SystemTime::now());
                }
                return Ok(());
            }
        }
    }
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.{n}{nanos:x}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    if let Err(e) = fs::write(&tmp, data).and_then(|_| fs::rename(&tmp, file)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Default limits for [`digest_folder`]: 2000 files, 128 MiB.
pub const DIGEST_FOLDER_MAX_FILES: u64 = 2000;
pub const DIGEST_FOLDER_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Digest of every regular file under `dir` (a plugin can read all of them, so
/// any may change its output). `Ok(None)` when the folder is over the limits.
/// Same value as `digestFolder` in cache.ts.
/// What INTEGRITY on a JavaScript plugin pins: `sha256:` + `digest_folder` of the plugin's folder.
/// `Err(problem)` when the folder holds anything but regular files and directories (a symlink
/// could change what an import resolves to without changing any file) or is over
/// `digest_folder`'s limits. Port of `pluginFolderIntegrity` (src/plugins/cache.ts).
pub fn plugin_folder_integrity(dir: &Path) -> io::Result<std::result::Result<String, String>> {
    fn other(dir: &Path, rel: &str) -> io::Result<Option<String>> {
        let here = if rel.is_empty() {
            dir.to_path_buf()
        } else {
            dir.join(rel)
        };
        let mut entries: Vec<(String, fs::FileType)> = Vec::new();
        for e in fs::read_dir(&here)? {
            let e = e?;
            entries.push((e.file_name().to_string_lossy().into_owned(), e.file_type()?));
        }
        entries.sort_by(|a, b| utf16_cmp(&a.0, &b.0));
        for (name, ft) in entries {
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                if let Some(found) = other(dir, &r)? {
                    return Ok(Some(found));
                }
            } else if !ft.is_file() {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
    if let Some(odd) = other(dir, "")? {
        return Ok(Err(format!(
            "contains {odd}, which is not a regular file or directory (a pinned folder can hold only those)"
        )));
    }
    Ok(match digest_folder(dir)? {
        Some(hex) => Ok(format!("sha256:{hex}")),
        None => Err("is too large to pin (more than 2000 files or 128 MiB)".into()),
    })
}

pub fn digest_folder(dir: &Path) -> io::Result<Option<String>> {
    digest_folder_with_limits(dir, DIGEST_FOLDER_MAX_FILES, DIGEST_FOLDER_MAX_BYTES)
}

pub fn digest_folder_with_limits(
    dir: &Path,
    max_files: u64,
    max_bytes: u64,
) -> io::Result<Option<String>> {
    struct Walk<'a> {
        dir: &'a Path,
        h: Sha256,
        files: u64,
        bytes: u64,
        max_files: u64,
        max_bytes: u64,
    }
    fn walk(w: &mut Walk<'_>, rel: &str) -> io::Result<bool> {
        let here = if rel.is_empty() {
            w.dir.to_path_buf()
        } else {
            w.dir.join(rel)
        };
        let mut entries: Vec<(String, fs::FileType)> = Vec::new();
        for e in fs::read_dir(&here)? {
            let e = e?;
            entries.push((e.file_name().to_string_lossy().into_owned(), e.file_type()?));
        }
        entries.sort_by(|a, b| utf16_cmp(&a.0, &b.0));
        for (name, ft) in entries {
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                if !walk(w, &r)? {
                    return Ok(false);
                }
            } else if ft.is_file() {
                let data = fs::read(w.dir.join(&r))?;
                w.files += 1;
                w.bytes += data.len() as u64;
                if w.files > w.max_files || w.bytes > w.max_bytes {
                    return Ok(false);
                }
                w.h.update(format!("{r}\0{}\n", sha256(&data)).as_bytes());
            }
        }
        Ok(true)
    }
    let mut w = Walk {
        dir,
        h: Sha256::new(),
        files: 0,
        bytes: 0,
        max_files,
        max_bytes,
    };
    if walk(&mut w, "")? {
        Ok(Some(
            w.h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
        ))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::host::PluginResult;

    pub(crate) fn tmp(prefix: &str) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("{prefix}{}-{n}-{nanos}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn js_number_formatting() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (1730000000123.4567, "1730000000123.4568"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (f64::NAN, "null"),
        ];
        for (x, want) in cases {
            assert_eq!(js_number(*x), *want, "{x}");
        }
    }

    #[test]
    fn canonical_json_ignores_key_order_and_the_key_covers_every_part() {
        let a = json!({ "b": 1, "a": { "d": [1, { "z": 1 }], "c": null } });
        let b = json!({ "a": { "c": null, "d": [1, { "z": 1 }] }, "b": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            canonical_json(&a),
            r#"{"a":{"c":null,"d":[1,{"z":1}]},"b":1}"#
        );
        let base = json!({ "code": { "plugin": "x" }, "request": { "dest": "/a", "options": { "n": "1" } } });
        let k = cache_key(&base);
        assert!(is_hex64(&k));
        assert_eq!(
            cache_key(&json!({ "request": base["request"], "code": base["code"] })),
            k
        );
        assert_ne!(
            cache_key(&json!({ "code": { "plugin": "y" }, "request": base["request"] })),
            k
        );
        assert_ne!(
            cache_key(
                &json!({ "code": base["code"], "request": { "dest": "/a", "options": { "n": "2" } } })
            ),
            k
        );
        // JavaScript iterates array-index keys first; non-BMP keys sort by UTF-16.
        assert_eq!(
            canonical_json(&json!({ "b": 1, "10": 2, "9": 3, "a": 4 })),
            r#"{"9":3,"10":2,"a":4,"b":1}"#
        );
        assert_eq!(
            canonical_json(&json!({ "\u{10000}": 1, "\u{ffff}": 2 })),
            "{\"\u{10000}\":1,\"\u{ffff}\":2}"
        );
        assert_eq!(
            canonical_json(&json!("a\"\\\n\u{1}\u{7f}é")),
            "\"a\\\"\\\\\\n\\u0001\u{7f}é\""
        );
    }

    /// Cases committed with the keys the TypeScript implementation computes
    /// (`testdata/cache-keys.json`, generated by `node` on lib/src/plugins/cache.js).
    #[test]
    fn cache_keys_match_the_typescript_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("testdata/cache-keys.json")).unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert!(cases.len() >= 5);
        for c in cases {
            assert_eq!(
                canonical_json(&c["parts"]),
                c["canonical"].as_str().unwrap(),
                "canonical json of {}",
                c["parts"]
            );
            assert_eq!(
                cache_key(&c["parts"]),
                c["key"].as_str().unwrap(),
                "key of {}",
                c["parts"]
            );
        }
    }

    /// The same check against a live `node` (skipped unless node and the built lib/ exist).
    #[test]
    fn cache_keys_match_live_typescript() {
        let lib = repo_root().join("lib/src/plugins/cache.js");
        if !lib.exists()
            || std::process::Command::new("node")
                .arg("--version")
                .output()
                .is_err()
        {
            eprintln!("skipped: needs node and `npm run build` (lib/src/plugins/cache.js)");
            return;
        }
        let fixture: Value =
            serde_json::from_str(include_str!("testdata/cache-keys.json")).unwrap();
        let src = format!(
            "import {{ cacheKey, canonicalJson }} from {url};\nimport {{ readFileSync }} from 'node:fs';\nconst cases = JSON.parse(readFileSync(0, 'utf8'));\nprocess.stdout.write(JSON.stringify(cases.map((p) => [canonicalJson(p), cacheKey(p)])));",
            url = js_json(&Value::String(format!("file://{}", lib.display())))
        );
        let parts: Vec<Value> = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["parts"].clone())
            .collect();
        let mut child = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &src])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(serde_json::to_string(&parts).unwrap().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let got: Vec<(String, String)> = serde_json::from_slice(&out.stdout).unwrap();
        for (p, (canon, key)) in parts.iter().zip(got) {
            assert_eq!(canonical_json(p), canon);
            assert_eq!(cache_key(p), key);
        }
    }

    #[test]
    fn put_get_round_trip_and_damage_handling() {
        let d = tmp("sitefile-cache-rs-");
        let mut c = TransformCache::new(d.join("cache/transform"));
        assert!(c.get("k1").is_none());
        assert_eq!(c.stats.misses, 1);
        c.put("k1", &PluginResult::File(b"alpha".to_vec()));
        let files = PluginResult::Files(vec![
            ("a.txt".into(), b"A".to_vec()),
            ("sub/b.bin".into(), vec![1, 2, 3]),
        ]);
        c.put("k2", &files);
        assert_eq!(c.stats.writes, 2);
        assert_eq!(c.get("k1"), Some(PluginResult::File(b"alpha".to_vec())));
        assert_eq!(c.get("k2"), Some(files.clone()));
        assert_eq!(c.stats.hits, 2);
        // the manifest is what cache.ts writes
        let m = fs::read_to_string(c.entry_path("k1")).unwrap();
        assert_eq!(
            m,
            format!(
                r#"{{"version":1,"kind":"file","files":[["","{}",5]]}}"#,
                crate::digest::hex_of(&sha256(b"alpha"))
            )
        );

        let blob = c.blob_path(crate::digest::hex_of(&sha256(b"alpha")));
        // 1. tampered blob
        fs::write(&blob, "tampered").unwrap();
        assert!(c.get("k1").is_none());
        assert_eq!(c.stats.corrupt, 1);
        assert!(!c.entry_path("k1").exists());
        c.put("k1", &PluginResult::File(b"alpha".to_vec()));
        assert!(c.get("k1").is_some());
        // 2. manifest not JSON
        fs::write(c.entry_path("k1"), "{not json").unwrap();
        assert!(c.get("k1").is_none());
        assert_eq!(c.stats.corrupt, 2);
        // 3. missing blob
        c.put("k1", &PluginResult::File(b"alpha".to_vec()));
        fs::remove_file(&blob).unwrap();
        assert!(c.get("k1").is_none());
        assert_eq!(c.stats.corrupt, 3);
        // 4. truncated blob (right name, wrong size)
        c.put("k1", &PluginResult::File(b"alpha".to_vec()));
        fs::write(&blob, "").unwrap();
        assert!(c.get("k1").is_none());
        assert_eq!(c.stats.corrupt, 4);
        c.put("k1", &PluginResult::File(b"alpha".to_vec()));
        assert!(c.get("k1").is_some());
        fs::remove_dir_all(&d).unwrap();
    }

    struct OneEntry(CacheEntry, Vec<u8>);
    impl RemoteCache for OneEntry {
        fn fetch(&self, _key: &str, blob_dir: &Path) -> Option<CacheEntry> {
            fs::create_dir_all(blob_dir).ok()?;
            fs::write(blob_dir.join(&self.0.files[0].1), &self.1).ok()?;
            Some(self.0.clone())
        }
        fn used(&self, _key: &str, _entry: &CacheEntry) {}
    }

    #[test]
    fn entries_record_their_plugin_and_older_entries_still_read() {
        let d = tmp("sitefile-cache-plugin-rs-");
        let mut c = TransformCache::new(d.join("cache/transform"));
        let hex = |b: &[u8]| crate::digest::hex_of(&sha256(b)).to_string();
        c.put_for("k1", Some("shout"), &PluginResult::File(b"alpha".to_vec()));
        // The manifest cache.ts writes for `put(key, result, 'shout')`, byte for byte.
        assert_eq!(
            fs::read_to_string(c.entry_path("k1")).unwrap(),
            format!(
                r#"{{"version":1,"kind":"file","files":[["","{}",5]],"plugin":"shout"}}"#,
                hex(b"alpha")
            )
        );
        assert_eq!(c.get("k1"), Some(PluginResult::File(b"alpha".to_vec())));
        // An entry written before plugins were recorded (no "plugin") is read as before.
        c.put("k2", &PluginResult::File(b"beta".to_vec()));
        assert!(!fs::read_to_string(c.entry_path("k2"))
            .unwrap()
            .contains("plugin"));
        assert_eq!(
            c.get_for("k2", Some("shout")),
            Some(PluginResult::File(b"beta".to_vec()))
        );
        // Shared blob: "alpha" again under another plugin.
        c.put_for("k3", Some("upper"), &PluginResult::File(b"alpha".to_vec()));
        c.put_for(
            "k4",
            Some("upper"),
            &PluginResult::Files(vec![("x".into(), b"xyz".to_vec())]),
        );
        let u = c.usage_by_plugin();
        assert_eq!(
            u,
            [
                PluginCacheUsage {
                    plugin: None,
                    entries: 1,
                    blobs: 1,
                    bytes: 4
                },
                PluginCacheUsage {
                    plugin: Some("shout".into()),
                    entries: 1,
                    blobs: 1,
                    bytes: 5
                },
                PluginCacheUsage {
                    plugin: Some("upper".into()),
                    entries: 2,
                    blobs: 2,
                    bytes: 8
                },
            ]
        );
        // Pruning one plugin keeps a blob another entry still uses, and unnamed entries.
        let r = c.prune_plugin("upper", Duration::ZERO);
        assert_eq!((r.entries, r.blobs, r.bytes), (2, 1, 3));
        assert!(c.get("k1").is_some() && c.get("k2").is_some());
        assert!(c.get("k3").is_none());
        // Only entries unused for the given time go.
        let r = c.prune_plugin("shout", Duration::from_secs(3600));
        assert_eq!(r.entries, 0);
        assert_eq!(
            c.prune_plugin("nobody", Duration::ZERO),
            PruneResult::default()
        );
        // A blob another build is writing (no entry yet) is not touched by a plugin's prune.
        fs::write(c.blob_path(&hex(b"inflight")), "inflight").unwrap();
        let r = c.prune_plugin("shout", Duration::ZERO);
        assert_eq!((r.entries, r.blobs), (1, 1));
        assert!(c.blob_path(&hex(b"inflight")).exists());

        // An entry fetched from the remote cache is stored under the plugin that asked for it.
        let mut remote =
            TransformCache::new(d.join("other")).with_remote(Some(std::sync::Arc::new(OneEntry(
                CacheEntry {
                    kind: "file".into(),
                    files: vec![("".into(), hex(b"remote"), 6)],
                },
                b"remote".to_vec(),
            ))));
        assert!(remote.get_for("rk", Some("shout")).is_some());
        assert!(fs::read_to_string(remote.entry_path("rk"))
            .unwrap()
            .ends_with(r#","plugin":"shout"}"#));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_unwritable_cache_does_not_fail() {
        let d = tmp("sitefile-cache-block-");
        fs::write(d.join("file"), "not a dir").unwrap();
        let mut c = TransformCache::new(d.join("file/cache"));
        c.put("k", &PluginResult::File(b"x".to_vec()));
        assert_eq!(c.stats.writes, 0);
        assert!(c.get("k").is_none());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prune_and_info() {
        let d = tmp("sitefile-cache-prune-");
        let mut c = TransformCache::new(&d);
        c.put("a", &PluginResult::File(b"alpha".to_vec()));
        c.put("b", &PluginResult::File(b"beta".to_vec()));
        let i = c.info();
        assert_eq!((i.entries, i.blobs, i.bytes), (2, 2, 9));
        let past = SystemTime::now() - Duration::from_secs(10 * 86_400);
        fs::OpenOptions::new()
            .write(true)
            .open(c.entry_path("a"))
            .unwrap()
            .set_modified(past)
            .unwrap();
        let r = c.prune(Duration::from_secs(7 * 86_400));
        assert_eq!((r.entries, r.blobs, r.bytes), (1, 1, 5));
        assert_eq!(c.info().entries, 1);
        assert!(c.get("b").is_some());
        let all = c.prune(Duration::ZERO);
        assert_eq!((all.entries, all.blobs), (1, 1));
        assert_eq!(
            c.info(),
            CacheInfo {
                dir: d.clone(),
                entries: 0,
                blobs: 0,
                bytes: 0
            }
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn plugin_folder_integrity_pins_the_folder_and_refuses_symlinks() {
        let d = tmp("sitefile-folder-integrity-");
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::write(d.join("p.mjs"), "import './lib/h.mjs';").unwrap();
        fs::write(d.join("lib/h.mjs"), "export const h = 1;").unwrap();
        let pin = plugin_folder_integrity(&d).unwrap().unwrap();
        assert_eq!(
            pin,
            format!("sha256:{}", digest_folder(&d).unwrap().unwrap())
        );
        fs::write(d.join("lib/h.mjs"), "export const h = 2;").unwrap();
        assert_ne!(plugin_folder_integrity(&d).unwrap().unwrap(), pin);
        std::os::unix::fs::symlink("h.mjs", d.join("lib/alias.mjs")).unwrap();
        assert_eq!(
            plugin_folder_integrity(&d).unwrap().unwrap_err(),
            "contains lib/alias.mjs, which is not a regular file or directory (a pinned folder can hold only those)"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn digest_folder_matches_typescript_and_respects_limits() {
        let d = tmp("sitefile-digest-folder-");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("p.mjs"), "export function transform() {}").unwrap();
        fs::write(d.join("helper.txt"), "h1").unwrap();
        fs::write(d.join("sub/B.txt"), "b").unwrap();
        let ours = digest_folder(&d).unwrap().unwrap();
        assert!(is_hex64(&ours));
        assert_eq!(digest_folder_with_limits(&d, 2, u64::MAX).unwrap(), None);
        assert_eq!(digest_folder_with_limits(&d, 100, 10).unwrap(), None);
        fs::write(d.join("helper.txt"), "h2").unwrap();
        assert_ne!(digest_folder(&d).unwrap().unwrap(), ours);

        let lib = repo_root().join("lib/src/plugins/cache.js");
        if lib.exists() {
            let src = format!(
                "import {{ digestFolder }} from {};\nprocess.stdout.write(String(await digestFolder(process.argv[1])));",
                js_json(&Value::String(format!("file://{}", lib.display())))
            );
            if let Ok(out) = std::process::Command::new("node")
                .args(["--input-type=module", "-e", &src, d.to_str().unwrap()])
                .output()
            {
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    digest_folder(&d).unwrap().unwrap()
                );
            }
        }
        fs::remove_dir_all(&d).unwrap();
    }
}
