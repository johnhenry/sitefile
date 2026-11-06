//! Port of `src/cache-limit.ts`: the size limit for what Sitefile keeps under
//! `$SITEFILE_HOME` (docs/CLI.md, "Cache size limit").
//!
//! EVICTABLE data is the TRANSFORM result cache (`cache/transform`), the
//! compiled wasm module cache (`cache/wasm`) and, in the image store, untagged
//! images and blobs no tagged image uses. Tagged images and every blob they
//! reference are never evicted.
//!
//! After a build, when the evictable bytes exceed the limit
//! (`$SITEFILE_CACHE_MAX`, default 1 GiB, 0 = no limit), the least recently
//! used items are deleted until it fits. Recency is the file's mtime, refreshed
//! on every cache hit (atime is often disabled): a TRANSFORM entry, a `.cwasm`
//! file, an untagged image's manifest blob. Nothing touched within the grace
//! period is deleted (10 minutes for the pure caches, 1 hour for the image
//! store and temp files), so a concurrent build never loses what it is writing
//! or has just written; writes are temp file + rename, and the store part runs
//! under the store lock. Under the limit the check is one directory listing per
//! cache. Same results and messages as the TypeScript version.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::Result;
use crate::plugins::cache::mtime_ms;
use crate::store::Store;

pub const CACHE_MAX_ENV: &str = "SITEFILE_CACHE_MAX";
pub const DEFAULT_CACHE_MAX: u64 = 1024 * 1024 * 1024;
/// Files touched more recently than this are never evicted from the pure caches.
pub const CACHE_GRACE_MS: f64 = 10.0 * 60_000.0;
/// Temp files and image-store data get a longer grace (pulls and builds can take a while).
pub const LONG_GRACE_MS: f64 = 60.0 * 60_000.0;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `1048576`, `512K`, `512M`, `1.5G`, `2GiB`, `1T` (binary units,
/// case-insensitive); `0` = no limit.
pub fn parse_size(raw: &str) -> Option<u64> {
    let s = raw.trim();
    let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, rest) = s.split_at(num_end);
    // \d+(\.\d+)?
    let (int, frac) = match num.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (num, None),
    };
    if int.is_empty() || frac.is_some_and(|f| f.is_empty() || f.contains('.')) {
        return None;
    }
    let unit = rest.trim_start().to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        u => {
            let (first, tail) = u.split_at(1);
            let m = match first {
                "k" => 1024.0,
                "m" => 1024.0 * 1024.0,
                "g" => 1024.0 * 1024.0 * 1024.0,
                "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                _ => return None,
            };
            if !matches!(tail, "" | "b" | "ib") {
                return None;
            }
            m
        }
    };
    let n = (num.parse::<f64>().ok()? * mult).floor();
    (n <= MAX_SAFE_INTEGER).then_some(n as u64)
}

/// A limit as it is shown: `unlimited`, `1GiB`, `512MiB`, `100KiB`, else bytes (`1000B`).
pub fn format_limit(bytes: u64) -> String {
    if bytes == 0 {
        return "unlimited".into();
    }
    for (u, n) in [("TiB", 1u64 << 40), ("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes % n == 0 {
            return format!("{}{u}", bytes / n);
        }
    }
    format!("{bytes}B")
}

/// The limit from `$SITEFILE_CACHE_MAX` (unset or empty: the default). A bad
/// value warns and uses the default.
pub fn cache_limit_from(raw: Option<&str>, warn: &mut dyn FnMut(&str)) -> u64 {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else { return DEFAULT_CACHE_MAX };
    if let Some(n) = parse_size(raw) {
        return n;
    }
    warn(&format!(
        "{CACHE_MAX_ENV}='{raw}' is not a size (bytes, or a number with K, M, G or T; 0 for no limit); using {}",
        format_limit(DEFAULT_CACHE_MAX)
    ));
    DEFAULT_CACHE_MAX
}

pub fn cache_limit_from_env(warn: &mut dyn FnMut(&str)) -> u64 {
    cache_limit_from(std::env::var(CACHE_MAX_ENV).ok().as_deref(), warn)
}

// ---------------------------------------------------------------- model

struct Blob {
    file: PathBuf,
    size: u64,
    mtime: f64,
    grace: f64,
    refs: usize,
    gone: bool,
}

struct Root {
    key: String,
    /// Own file (a TRANSFORM entry), or None for an untagged image (its manifest is a blob).
    file: Option<PathBuf>,
    size: u64,
    mtime: f64,
    grace: f64,
    refs: Vec<String>,
    /// Image ID to drop from the store index.
    image: Option<String>,
}

struct FileStat {
    name: String,
    file: PathBuf,
    size: u64,
    mtime: f64,
}

fn files(dir: &Path) -> Vec<FileStat> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let file = dir.join(&name);
            let st = fs::symlink_metadata(&file).ok().filter(|m| m.is_file())?;
            Some(FileStat { name, file, size: st.len(), mtime: mtime_ms(&st) })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheUsage {
    /// `transform`, `wasm`, `store` or `servers`.
    pub name: &'static str,
    pub path: PathBuf,
    /// TRANSFORM entries, .cwasm modules, store blobs, server files.
    pub items: u64,
    pub bytes: u64,
    /// Bytes the size limit may delete (everything but tagged images' blobs and server state).
    pub evictable: u64,
}

pub struct CachePaths {
    pub transform: PathBuf,
    pub wasm: PathBuf,
    pub store: PathBuf,
    pub servers: PathBuf,
}

pub fn cache_paths(home: &Path) -> CachePaths {
    CachePaths {
        transform: home.join("cache").join("transform"),
        wasm: home.join("cache").join("wasm"),
        store: home.join("blobs").join("sha256"),
        servers: home.join("servers"),
    }
}

#[derive(Default)]
struct Model {
    blobs: BTreeMap<String, Blob>,
    roots: Vec<Root>,
    total: u64,
}

impl Model {
    fn blob(&mut self, key: String, f: &FileStat, grace: f64) {
        self.total += f.size;
        self.blobs.insert(key, Blob { file: f.file.clone(), size: f.size, mtime: f.mtime, grace, refs: 0, gone: false });
    }
    fn root(&mut self, r: Root) {
        self.total += r.size;
        for k in &r.refs {
            if let Some(b) = self.blobs.get_mut(k) {
                b.refs += 1;
            }
        }
        self.roots.push(r);
    }
}

fn add_transform(m: &mut Model, dir: &Path) -> u64 {
    for f in files(&dir.join("blobs")) {
        let grace = if is_hex64(&f.name) { CACHE_GRACE_MS } else { LONG_GRACE_MS };
        m.blob(format!("t:{}", f.name), &f, grace);
    }
    let mut entries = 0;
    for f in files(&dir.join("entries")) {
        if !f.name.ends_with(".json") || f.name.starts_with('.') {
            m.blob(format!("t:entries/{}", f.name), &f, LONG_GRACE_MS);
            continue;
        }
        entries += 1;
        let mut refs: Vec<String> = Vec::new();
        if let Some(Value::Array(list)) = fs::read_to_string(&f.file).ok().and_then(|raw| serde_json::from_str::<Value>(&raw).ok()).and_then(|j| j.get("files").cloned()) {
            for x in list {
                if let Some(h) = x.as_array().and_then(|a| a.get(1)).and_then(Value::as_str).filter(|h| is_hex64(h)) {
                    let k = format!("t:{h}");
                    if !refs.contains(&k) {
                        refs.push(k);
                    }
                }
            }
        }
        m.root(Root { key: format!("t:entries/{}", f.name), file: Some(f.file.clone()), size: f.size, mtime: f.mtime, grace: CACHE_GRACE_MS, refs, image: None });
    }
    entries
}

fn add_wasm(m: &mut Model, dir: &Path) -> u64 {
    let mut modules = 0;
    for f in files(dir) {
        let ok = f.name.strip_suffix(".cwasm").is_some_and(is_hex64);
        if ok {
            modules += 1;
        }
        m.blob(format!("w:{}", f.name), &f, if ok { CACHE_GRACE_MS } else { LONG_GRACE_MS });
    }
    modules
}

/// Adds the store's evictable part; returns (blob count, total bytes).
fn add_store(m: &mut Model, store: &Store, dir: &Path) -> Result<(u64, u64)> {
    let all = files(dir);
    let view = store.gc_view()?;
    for f in &all {
        if !view.live.contains(&f.name) {
            m.blob(format!("s:{}", f.name), f, LONG_GRACE_MS);
        }
    }
    for (id, blobs) in view.untagged {
        let mtime = m.blobs.get(&format!("s:{}", &id[7..])).map(|b| b.mtime).unwrap_or(0.0);
        let mut refs: Vec<String> = Vec::new();
        for h in blobs.iter().filter(|h| !view.live.contains(*h)) {
            let k = format!("s:{h}");
            if !refs.contains(&k) {
                refs.push(k);
            }
        }
        m.root(Root { key: format!("s:image/{id}"), file: None, size: 0, mtime, grace: LONG_GRACE_MS, refs, image: Some(id) });
    }
    Ok((all.len() as u64, all.iter().map(|f| f.size).sum()))
}

fn size_of(dir: &Path) -> (u64, u64) {
    let f = files(dir);
    (f.len() as u64, f.iter().map(|x| x.size).sum())
}

/// Sizes per cache under `home` (for `cache du`).
pub fn cache_usage(home: &Path) -> Result<Vec<CacheUsage>> {
    let p = cache_paths(home);
    let mut t = Model::default();
    let entries = add_transform(&mut t, &p.transform);
    let mut w = Model::default();
    let modules = add_wasm(&mut w, &p.wasm);
    let mut s = Model::default();
    let (mut store_items, mut store_bytes) = (0, 0);
    if let Some(store) = Store::open_existing(home) {
        (store_items, store_bytes) = add_store(&mut s, &store, &p.store)?;
    }
    let (server_items, server_bytes) = size_of(&p.servers);
    Ok(vec![
        CacheUsage { name: "transform", path: p.transform, items: entries, bytes: t.total, evictable: t.total },
        CacheUsage { name: "wasm", path: p.wasm, items: modules, bytes: w.total, evictable: w.total },
        CacheUsage { name: "store", path: p.store, items: store_items, bytes: store_bytes, evictable: s.total },
        CacheUsage { name: "servers", path: p.servers, items: server_items, bytes: server_bytes, evictable: 0 },
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictResult {
    /// TRANSFORM entries, modules, images and stray blobs deleted.
    pub items: u64,
    pub bytes: u64,
    /// Evictable bytes before and after.
    pub before: u64,
    pub after: u64,
}

pub fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

/// Delete least recently used evictable data until it fits in `limit` bytes
/// (0: no limit). Cheap when under the limit: one listing per cache directory.
pub fn enforce_cache_limit(home: &Path, limit: u64) -> Result<EvictResult> {
    enforce_cache_limit_at(home, limit, now_ms())
}

/// [`enforce_cache_limit`] with an explicit "now" (ms since the epoch).
pub fn enforce_cache_limit_at(home: &Path, limit: u64, now: f64) -> Result<EvictResult> {
    if limit == 0 {
        return Ok(EvictResult::default());
    }
    let p = cache_paths(home);
    // Upper bound first (it counts tagged images too): under the limit, nothing more to do.
    let bound: u64 = [p.transform.join("blobs"), p.transform.join("entries"), p.wasm.clone(), p.store.clone()].iter().map(|d| size_of(d).1).sum();
    if bound <= limit {
        return Ok(EvictResult { items: 0, bytes: 0, before: bound, after: bound });
    }
    let store = Store::open_existing(home);
    let run = || -> Result<EvictResult> {
        let mut m = Model::default();
        add_transform(&mut m, &p.transform);
        add_wasm(&mut m, &p.wasm);
        if let Some(s) = &store {
            add_store(&mut m, s, &p.store)?;
        }
        let before = m.total;
        let mut items = 0;
        enum Cand {
            Root(usize),
            Blob(String),
        }
        let mut cands: Vec<(f64, String, Cand)> = m.roots.iter().enumerate().map(|(i, r)| (r.mtime, r.key.clone(), Cand::Root(i))).collect();
        cands.extend(m.blobs.iter().filter(|(_, b)| b.refs == 0).map(|(k, b)| (b.mtime, k.clone(), Cand::Blob(k.clone()))));
        cands.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, _, c) in cands {
            if m.total <= limit {
                break;
            }
            match c {
                Cand::Root(i) => {
                    let (mtime, grace) = (m.roots[i].mtime, m.roots[i].grace);
                    if now - mtime < grace {
                        continue;
                    }
                    if let (Some(id), Some(s)) = (&m.roots[i].image, &store) {
                        s.drop_untagged_locked(id)?;
                    }
                    if let Some(f) = &m.roots[i].file {
                        let _ = fs::remove_file(f);
                    }
                    m.total -= m.roots[i].size;
                    items += 1;
                    let refs = m.roots[i].refs.clone();
                    for k in refs {
                        let Some(b) = m.blobs.get_mut(&k) else { continue };
                        if b.gone {
                            continue;
                        }
                        b.refs -= 1;
                        if b.refs == 0 && now - b.mtime >= b.grace {
                            let _ = fs::remove_file(&b.file);
                            b.gone = true;
                            m.total -= b.size;
                        }
                    }
                }
                Cand::Blob(k) => {
                    let Some(b) = m.blobs.get_mut(&k) else { continue };
                    if !b.gone && b.refs == 0 && now - b.mtime >= b.grace {
                        let _ = fs::remove_file(&b.file);
                        b.gone = true;
                        m.total -= b.size;
                        items += 1;
                    }
                }
            }
        }
        Ok(EvictResult { items, bytes: before - m.total, before, after: m.total })
    };
    match &store {
        Some(s) => s.with_lock(run),
        None => run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::exporter::tests::TmpDir;
    use crate::export::oci::OciOptions;
    use crate::plugins::cache::TransformCache;
    use crate::plugins::host::PluginResult;
    use crate::refs::parse_local_ref;
    use crate::tree::{make_file, Tree};
    use std::time::Duration;

    const HOUR: f64 = 3_600_000.0;
    fn later() -> f64 {
        now_ms() + 2.0 * HOUR
    }
    fn age(file: &Path, ms_ago: f64) {
        let now = SystemTime::now();
        let t = if ms_ago >= 0.0 { now - Duration::from_millis(ms_ago as u64) } else { now + Duration::from_millis((-ms_ago) as u64) };
        fs::OpenOptions::new().write(true).open(file).unwrap().set_modified(t).unwrap();
    }
    fn hex(c: &str) -> String {
        c.repeat(64)
    }
    fn mk(files: &[(&str, &str)]) -> Tree {
        Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644)))).unwrap()
    }
    fn usage_of(home: &Path, name: &str) -> CacheUsage {
        cache_usage(home).unwrap().into_iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn sizes_and_env() {
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("1048576"), Some(1048576));
        assert_eq!(parse_size("512K"), Some(512 * 1024));
        assert_eq!(parse_size(" 512m "), Some(512 << 20));
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("2gb"), Some(2 << 30));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("1T"), Some(1 << 40));
        assert_eq!(parse_size("100B"), Some(100));
        for bad in ["", "x", "-1", "1X", "1 GB extra", "1e3", ".5G"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
        assert_eq!(format_limit(0), "unlimited");
        assert_eq!(format_limit(1 << 30), "1GiB");
        assert_eq!(format_limit(512 << 20), "512MiB");
        assert_eq!(format_limit(1536 * 1024), "1536KiB");
        assert_eq!(format_limit(1000), "1000B");
        let mut warnings: Vec<String> = Vec::new();
        let mut w = |m: &str| warnings.push(m.to_string());
        assert_eq!(cache_limit_from(None, &mut w), DEFAULT_CACHE_MAX);
        assert_eq!(cache_limit_from(Some(""), &mut w), DEFAULT_CACHE_MAX);
        assert_eq!(cache_limit_from(Some("0"), &mut w), 0);
        assert_eq!(cache_limit_from(Some("64M"), &mut w), 64 << 20);
        assert_eq!(cache_limit_from(Some("lots"), &mut w), DEFAULT_CACHE_MAX);
        assert_eq!(warnings, vec!["SITEFILE_CACHE_MAX='lots' is not a size (bytes, or a number with K, M, G or T; 0 for no limit); using 1GiB".to_string()]);
    }

    #[test]
    fn transform_lru_hit_refreshes_and_shared_blobs_stay() {
        let d = TmpDir::new("limit-t");
        let home = d.path();
        let mut c = TransformCache::new(home.join("cache/transform"));
        c.put("a", &PluginResult::File(vec![b'A'; 100]));
        c.put("b", &PluginResult::File(vec![b'B'; 100]));
        c.put("c", &PluginResult::Files(vec![("x".into(), vec![b'C'; 100]), ("y".into(), vec![b'A'; 100])]));
        let entry = |k: &str| home.join("cache/transform/entries").join(format!("{k}.json"));
        age(&entry("a"), 3.0 * HOUR);
        age(&entry("b"), 2.0 * HOUR);
        age(&entry("c"), HOUR);
        for f in files(&home.join("cache/transform/blobs")) {
            age(&f.file, 3.0 * HOUR);
        }
        assert!(c.get("a").is_some(), "hit: a becomes the most recently used");
        let t = usage_of(home, "transform");
        assert_eq!(t.items, 3);
        let r = enforce_cache_limit_at(home, t.evictable - 1, later()).unwrap();
        assert_eq!(r.items, 1);
        assert!(!entry("b").exists());
        assert!(c.get("a").is_some());
        assert!(c.get("c").is_some());
        assert_eq!(r.before - r.after, r.bytes);
        let all = enforce_cache_limit_at(home, 1, later()).unwrap();
        assert_eq!((all.items, all.after), (2, 0));
        assert!(files(&home.join("cache/transform/blobs")).is_empty());
    }

    #[test]
    fn grace_period_and_cheap_path() {
        let d = TmpDir::new("limit-g");
        let home = d.path();
        let mut c = TransformCache::new(home.join("cache/transform"));
        c.put("a", &PluginResult::File(b"fresh".to_vec()));
        let r = enforce_cache_limit(home, 1).unwrap();
        assert_eq!(r.items, 0);
        assert!(c.get("a").is_some());
        assert_eq!(enforce_cache_limit_at(home, 1024, later()).unwrap(), EvictResult { items: 0, bytes: 0, before: r.before, after: r.before });
        assert_eq!(enforce_cache_limit_at(home, 0, later()).unwrap().items, 0, "0 = no limit");
    }

    #[test]
    fn wasm_lru_and_temp_files() {
        let d = TmpDir::new("limit-w");
        let home = d.path();
        let w = home.join("cache/wasm");
        fs::create_dir_all(&w).unwrap();
        let (a, b) = (w.join(format!("{}.cwasm", hex("a"))), w.join(format!("{}.cwasm", hex("b"))));
        let (tc, td) = (w.join(format!(".{}.1.0.tmp", hex("c"))), w.join(format!(".{}.1.0.tmp", hex("d"))));
        for (p, ch) in [(&a, "a"), (&b, "b"), (&tc, "c"), (&td, "d")] {
            fs::write(p, ch.repeat(50)).unwrap();
        }
        age(&a, 30.0 * 60_000.0);
        age(&b, 20.0 * 60_000.0);
        age(&tc, 30.0 * 60_000.0);
        age(&td, 3.0 * HOUR);
        let u = usage_of(home, "wasm");
        assert_eq!((u.items, u.bytes, u.evictable), (2, 200, 200));
        let r = enforce_cache_limit(home, 120).unwrap();
        assert_eq!(r.items, 2);
        let mut left: Vec<String> = files(&w).into_iter().map(|f| f.name).collect();
        left.sort();
        let mut want = vec![format!(".{}.1.0.tmp", hex("c")), format!("{}.cwasm", hex("b"))];
        want.sort();
        assert_eq!(left, want);
    }

    #[test]
    fn store_tagged_images_are_never_evicted() {
        let d = TmpDir::new("limit-s");
        let home = d.path();
        let store = Store::open(home).unwrap();
        let site = mk(&[("/index.html", "shared layer")]);
        let o1 = OciOptions { labels: [("v".to_string(), "1".to_string())].into_iter().collect(), ..Default::default() };
        let o2 = OciOptions { labels: [("v".to_string(), "2".to_string())].into_iter().collect(), ..Default::default() };
        let tagged = store.add_image(&site, &o1).unwrap();
        store.tag(&tagged.id, &parse_local_ref("site:1").unwrap()).unwrap();
        let shared = store.add_image(&site, &o2).unwrap();
        let own = store.add_image(&mk(&[("/index.html", "other")]), &OciOptions::default()).unwrap();
        let blobs = home.join("blobs/sha256");
        let stray = blobs.join(hex("e"));
        fs::write(&stray, "stray").unwrap();
        let tmp = blobs.join(".tmp-abc");
        fs::write(&tmp, "pulling").unwrap();
        let before = usage_of(home, "store");
        assert_eq!(before.items, 10); // 3 + 2 (layer shared with the tagged one) + 3 + stray + tmp
        assert!(before.evictable > 0 && before.evictable < before.bytes);

        assert_eq!(enforce_cache_limit_at(home, 1, now_ms() + 59.0 * 60_000.0).unwrap().items, 0);
        age(&tmp, -3.0 * HOUR);
        let r = enforce_cache_limit_at(home, 1, later()).unwrap();
        assert_eq!(r.items, 3, "two untagged images and the stray blob");
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![tagged.id.clone()]);
        assert!(!ids.contains(&shared.id) && !ids.contains(&own.id));
        assert_eq!(store.load_tree(&tagged.id).unwrap().len(), 1, "the tagged image is intact");
        assert!(!stray.exists());
        assert!(tmp.exists());
        let after = usage_of(home, "store");
        assert_eq!((after.evictable, after.items), (7, 4));
    }

    #[test]
    fn an_unreadable_store_is_an_error_not_a_panic() {
        let d = TmpDir::new("limit-bad");
        let home = d.path();
        fs::create_dir_all(home.join("blobs/sha256")).unwrap();
        fs::write(home.join("blobs/sha256").join(hex("f")), "x".repeat(5000)).unwrap();
        fs::write(home.join("index.json"), "{not json").unwrap();
        let e = enforce_cache_limit_at(home, 1024, later()).unwrap_err();
        assert!(e.message().contains("index.json is corrupt"), "{}", e.message());
    }
}
