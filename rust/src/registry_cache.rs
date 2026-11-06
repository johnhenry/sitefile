//! TRANSFORM cache shared through an OCI registry (`build --cache-from <ref>`
//! / `--cache-to <ref>`, docs/CLI.md "Registry cache"). Port of
//! `src/registry-cache.ts`, kept byte-for-byte in step with it. The cache is
//! one OCI artifact:
//!
//! - manifest: `application/vnd.oci.image.manifest.v1+json`, artifactType
//!   [`CACHE_ARTIFACT_TYPE`], config = the empty descriptor (`{}`), layers =
//!   `[index, ...result blobs by digest]`;
//! - index ([`CACHE_INDEX_MEDIA`]): canonical JSON
//!   `{"entries":{"<key>":{"files":[[path,hex,size]],"kind":k}},"version":1}`;
//! - blobs ([`CACHE_BLOB_MEDIA`]): result bytes, exactly as in the local
//!   cache's `blobs/` directory.
//!
//! `--cache-from` reads the manifest and the index before the build; a
//! result's blobs are fetched only on a hit (a local miss whose key is in the
//! index), verified, and written to the local cache. `--cache-to` pushes, after
//! a successful build, the entries this build used or wrote; blobs the registry
//! already has are not uploaded again. A registry problem is a warning, never a
//! build failure. Pushed bytes are deterministic and hold no local paths or
//! secrets: keys are SHA-256 hashes, entries name output-relative paths,
//! digests and sizes only.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cache_limit::{format_limit, CACHE_MAX_ENV};
use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::executor::Callback;
use crate::export::oci::{canonical_json, media, parse_manifest, Descriptor};
use crate::plugins::cache::{CacheEntry, RemoteCache};
use crate::refs::{canonical_ref, parse_ref, ImageRef};
use crate::registry::{BlobPush, Body, RegistryClient};

pub const CACHE_ARTIFACT_TYPE: &str = "application/vnd.sitefile.cache.v1+json";
pub const CACHE_INDEX_MEDIA: &str = "application/vnd.sitefile.cache.index.v1+json";
pub const CACHE_BLOB_MEDIA: &str = "application/vnd.sitefile.cache.blob.v1";
pub const EMPTY_MEDIA: &str = "application/vnd.oci.empty.v1+json";
/// Largest cache index accepted.
pub const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const INDEX_VERSION: f64 = 1.0;
const EMPTY: &[u8] = b"{}";
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone)]
pub struct CacheRef {
    /// As written on the command line (used in messages).
    pub input: String,
    pub r: ImageRef,
}

/// Validate a `--cache-from` / `--cache-to` reference (E_USAGE when it is not usable).
pub fn parse_cache_ref(flag: &str, input: &str) -> Result<CacheRef> {
    let r = parse_ref(input)?;
    if r.registry.is_none() {
        return Err(SitefileError::plain(E_USAGE, format!("{flag} '{input}' has no registry host (e.g. localhost:5000/team/site-cache:main)")));
    }
    if flag == "--cache-to" && r.digest.is_some() {
        return Err(SitefileError::plain(E_USAGE, format!("{flag} '{input}' must name a tag, not a digest")));
    }
    Ok(CacheRef { input: input.to_string(), r: canonical_ref(&r) })
}

fn invalid() -> SitefileError {
    SitefileError::plain(E_REGISTRY, "the cache index is invalid")
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `Number.isSafeInteger(x) && x >= 0` for a JSON number.
fn safe_size(v: &Value) -> Option<u64> {
    let f = v.as_f64()?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&f) && f.fract() == 0.0).then_some(f as u64)
}

/// Parse and validate an index; every blob it names must be a layer of the manifest with that size.
pub fn parse_cache_index(bytes: &[u8], blobs: &HashMap<String, u64>) -> Result<BTreeMap<String, CacheEntry>> {
    let j: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let j = j.as_object().ok_or_else(invalid)?;
    if j.get("version").and_then(Value::as_f64) != Some(INDEX_VERSION) || !j.get("version").is_some_and(Value::is_number) {
        return Err(invalid());
    }
    let entries = j.get("entries").and_then(Value::as_object).ok_or_else(invalid)?;
    let mut out = BTreeMap::new();
    for (key, e) in entries {
        let e = e.as_object().filter(|_| is_hex64(key)).ok_or_else(invalid)?;
        let kind = e.get("kind").and_then(Value::as_str).filter(|k| *k == "file" || *k == "files").ok_or_else(invalid)?;
        let list = e.get("files").and_then(Value::as_array).ok_or_else(invalid)?;
        if kind == "file" && list.len() != 1 {
            return Err(invalid());
        }
        let mut files = Vec::new();
        let mut paths = std::collections::HashSet::new();
        for f in list {
            let f = f.as_array().filter(|f| f.len() == 3).ok_or_else(invalid)?;
            let p = f[0].as_str().ok_or_else(invalid)?;
            let hex = f[1].as_str().filter(|h| is_hex64(h)).ok_or_else(invalid)?;
            let size = safe_size(&f[2]).ok_or_else(invalid)?;
            let unsafe_path = if kind == "file" {
                !p.is_empty()
            } else {
                p.is_empty() || p.starts_with('/') || p.contains("..") || p.contains('\0') || p.contains('\\')
            };
            if unsafe_path || blobs.get(hex) != Some(&size) || !paths.insert(p.to_string()) {
                return Err(invalid());
            }
            files.push((p.to_string(), hex.to_string(), size));
        }
        out.insert(key.clone(), CacheEntry { kind: kind.to_string(), files });
    }
    Ok(out)
}

/// The index bytes for a set of entries (canonical JSON, keys sorted).
pub fn encode_cache_index(entries: &BTreeMap<String, CacheEntry>) -> Vec<u8> {
    let mut o = serde_json::Map::new();
    for (k, e) in entries {
        let files: Vec<Value> = e.files.iter().map(|(p, h, n)| json!([p, h, n])).collect();
        o.insert(k.clone(), json!({ "kind": e.kind, "files": files }));
    }
    canonical_json(&json!({ "version": 1, "entries": Value::Object(o) })).into_bytes()
}

/// The artifact manifest for an index and the result blobs (`(hex, size)`, any order).
pub fn encode_cache_manifest(index: &[u8], blobs: &[(String, u64)]) -> Vec<u8> {
    let mut sorted = blobs.to_vec();
    sorted.sort();
    let mut layers = vec![json!({ "mediaType": CACHE_INDEX_MEDIA, "digest": sha256(index), "size": index.len() })];
    for (hex, size) in &sorted {
        layers.push(json!({ "mediaType": CACHE_BLOB_MEDIA, "digest": format!("sha256:{hex}"), "size": size }));
    }
    canonical_json(&json!({
        "schemaVersion": 2,
        "mediaType": media::MANIFEST,
        "artifactType": CACHE_ARTIFACT_TYPE,
        "config": { "mediaType": EMPTY_MEDIA, "digest": sha256(EMPTY), "size": EMPTY.len() },
        "layers": layers,
    }))
    .into_bytes()
}

/// JavaScript's `s.slice(0, n)` (UTF-16 code units, never splitting a character here).
fn js_slice(s: &str, n: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &s[..i];
        }
    }
    s
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn touch(p: &Path) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(p) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// sha256 hex and size of a file, or None when it cannot be read.
fn hash_file(file: &Path) -> Option<(String, u64)> {
    let mut f = fs::File::open(file).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Some((to_hex(&h.finalize()), size))
}

pub type ClientFactory = Arc<dyn Fn(&str) -> RegistryClient + Send + Sync>;

pub struct RegistryCacheOptions {
    pub from: Vec<CacheRef>,
    pub to: Option<CacheRef>,
    /// A client for a canonical registry name (auth, TLS and network policy are the client's).
    pub client: ClientFactory,
    /// `$SITEFILE_CACHE_MAX` in bytes (0: no limit): a build never fetches more than this.
    pub limit: u64,
    pub warn: Callback,
    pub notice: Callback,
}

struct Source {
    name: String,
    client: RegistryClient,
    repo: String,
    entries: BTreeMap<String, CacheEntry>,
    /// hex -> size of every result blob layer.
    blobs: HashMap<String, u64>,
    disabled: bool,
}

#[derive(Default)]
struct State {
    used: BTreeMap<String, CacheEntry>,
    sources: Vec<Source>,
    stopped: bool,
    fetched: usize,
    fetched_bytes: u64,
}

pub struct RegistryCache {
    o: RegistryCacheOptions,
    state: Mutex<State>,
}

/// One blob of a `--cache-to` push: the empty config, the index, or a result blob (`file_hex`).
struct Upload {
    digest: String,
    size: u64,
    media_type: &'static str,
    file_hex: Option<String>,
    /// A `--cache-from` repository on the same registry that holds it.
    mount: Option<String>,
}

/// Counts of a `--cache-to` push.
struct Pushed {
    entries: usize,
    uploaded: usize,
    mounted: usize,
    existing: usize,
    digest: String,
}

impl RegistryCache {
    pub fn new(o: RegistryCacheOptions) -> RegistryCache {
        RegistryCache { o, state: Mutex::new(State::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Entries this build used or wrote (what `--cache-to` pushes).
    pub fn used_entries(&self) -> BTreeMap<String, CacheEntry> {
        self.lock().used.clone()
    }

    /// Results fetched from `--cache-from` registries, and their blob bytes downloaded.
    pub fn fetched(&self) -> (usize, u64) {
        let s = self.lock();
        (s.fetched, s.fetched_bytes)
    }

    /// Read every `--cache-from` manifest and index (in order). Problems are warnings.
    pub fn load(&self) {
        for c in &self.o.from {
            match self.load_one(c) {
                Ok(s) => {
                    (self.o.notice)(&format!("Cache from {}: {}", c.input, plural(s.entries.len(), "entry", "entries")));
                    self.lock().sources.push(s);
                }
                Err(e) => (self.o.warn)(&format!("--cache-from {} not used: {}", c.input, e.message())),
            }
        }
    }

    fn load_one(&self, c: &CacheRef) -> Result<Source> {
        let client = (self.o.client)(c.r.registry.as_deref().unwrap_or_default());
        let repo = c.r.name.clone();
        let reference = c.r.digest.clone().or_else(|| c.r.tag.clone()).unwrap_or_else(|| "latest".into());
        let m = client.get_manifest(&repo, &reference)?;
        let not_oci = || SitefileError::plain(E_UNSUPPORTED, format!("{} is not a sitefile cache (not an OCI manifest)", c.input));
        let j: Value = serde_json::from_slice(&m.bytes).map_err(|_| not_oci())?;
        let layers = parse_manifest(&m.bytes).map_err(|_| not_oci())?.layers;
        match j.get("artifactType") {
            Some(Value::String(t)) if t == CACHE_ARTIFACT_TYPE => {}
            other => {
                let shown = match other {
                    Some(Value::String(t)) => format!("'{}'", js_slice(t, 100)),
                    _ => "missing".to_string(),
                };
                return Err(SitefileError::plain(E_UNSUPPORTED, format!("{} is not a sitefile cache (artifactType {shown})", c.input)));
            }
        }
        let indexes: Vec<&Descriptor> = layers.iter().filter(|l| l.media_type == CACHE_INDEX_MEDIA).collect();
        let index = match indexes.as_slice() {
            [one] => (*one).clone(),
            _ => return Err(SitefileError::plain(E_REGISTRY, format!("{} has no cache index layer", c.input))),
        };
        if index.size > MAX_INDEX_BYTES {
            return Err(SitefileError::plain(
                E_TOO_LARGE,
                format!("the cache index is {} bytes, exceeding the limit of {MAX_INDEX_BYTES} bytes", index.size),
            ));
        }
        let mut blobs = HashMap::new();
        for l in layers.iter().filter(|l| l.media_type == CACHE_BLOB_MEDIA) {
            blobs.insert(l.digest["sha256:".len()..].to_string(), l.size);
        }
        let mut reader = client.get_blob(&repo, &index)?;
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if (bytes.len() + n) as u64 > index.size {
                        return Err(SitefileError::plain(E_TOO_LARGE, format!("cache index {} exceeded {} bytes", index.digest, index.size)));
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                Err(e) => {
                    return Err(reader
                        .take_error()
                        .unwrap_or_else(|| SitefileError::plain(E_REGISTRY, format!("error downloading blob {}: {e}", index.digest))))
                }
            }
        }
        if bytes.len() as u64 != index.size || sha256(&bytes) != index.digest {
            return Err(SitefileError::plain(E_INTEGRITY, format!("the cache index does not match its digest {}", index.digest)));
        }
        let entries = parse_cache_index(&bytes, &blobs)?;
        Ok(Source { name: c.input.clone(), client, repo, entries, blobs, disabled: false })
    }

    /// Stream a result blob into the local cache: verified size and digest, temp file + rename.
    fn download(s: &Source, hex: &str, size: u64, file: &Path) -> Result<()> {
        let digest = format!("sha256:{hex}");
        if let Some(parent) = file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let mut tmp = file.as_os_str().to_owned();
        tmp.push(format!(".{}.{nanos:x}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let io_err = |e: std::io::Error| SitefileError::plain(E_REGISTRY, format!("cannot write {}: {e}", tmp.display()));
        let result = (|| -> Result<()> {
            let desc = Descriptor { media_type: CACHE_BLOB_MEDIA.into(), digest: digest.clone(), size, annotations: None };
            let mut reader = s.client.get_blob(&s.repo, &desc)?;
            let mut out = fs::File::create(&tmp).map_err(io_err)?;
            let mut h = Sha256::new();
            let mut n = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => {
                        n += k as u64;
                        if n > size {
                            break;
                        }
                        h.update(&buf[..k]);
                        std::io::Write::write_all(&mut out, &buf[..k]).map_err(io_err)?;
                    }
                    Err(e) => {
                        return Err(reader.take_error().unwrap_or_else(|| SitefileError::plain(E_REGISTRY, format!("error downloading blob {digest}: {e}"))))
                    }
                }
            }
            if n != size || to_hex(&h.finalize()) != hex {
                return Err(SitefileError::plain(E_INTEGRITY, format!("cached blob {digest} does not match its digest or size")));
            }
            drop(out);
            fs::rename(&tmp, file).map_err(io_err)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Push the used entries to `--cache-to` (after a successful build). Problems are warnings.
    pub fn push(&self, cache_dir: &Path) {
        let Some(to) = &self.o.to else { return };
        match self.push_to(to, cache_dir) {
            Ok(r) => (self.o.notice)(&format!(
                "Cache to {}: {}, {} uploaded, {} mounted, {} already present ({})",
                to.input,
                plural(r.entries, "entry", "entries"),
                plural(r.uploaded, "blob", "blobs"),
                r.mounted,
                r.existing,
                r.digest
            )),
            Err(e) => (self.o.warn)(&format!("--cache-to {} not pushed: {}", to.input, e.message())),
        }
    }

    fn push_to(&self, to: &CacheRef, cache_dir: &Path) -> Result<Pushed> {
        let blob_file = |hex: &str| cache_dir.join("blobs").join(hex);
        let used = self.used_entries();
        // Only entries whose blobs are all still intact locally.
        let mut good: HashMap<String, u64> = HashMap::new();
        let mut bad: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut entries = BTreeMap::new();
        for (k, e) in &used {
            let mut keep = true;
            for (_, hex, size) in &e.files {
                if !good.contains_key(hex) && !bad.contains(hex) {
                    match hash_file(&blob_file(hex)) {
                        Some((h, n)) if h == *hex && n == *size => {
                            good.insert(hex.clone(), *size);
                        }
                        _ => {
                            bad.insert(hex.clone());
                        }
                    }
                }
                if bad.contains(hex) {
                    keep = false;
                }
            }
            if keep {
                entries.insert(k.clone(), e.clone());
            }
        }
        let mut blobs: BTreeMap<String, u64> = BTreeMap::new();
        for e in entries.values() {
            for (_, hex, size) in &e.files {
                blobs.insert(hex.clone(), *size);
            }
        }
        let blob_list: Vec<(String, u64)> = blobs.into_iter().collect();
        let index = encode_cache_index(&entries);
        let manifest = encode_cache_manifest(&index, &blob_list);
        let client = (self.o.client)(to.r.registry.as_deref().unwrap_or_default());
        if manifest.len() as u64 > client.limits.max_manifest_bytes {
            return Err(SitefileError::plain(
                E_TOO_LARGE,
                format!("the cache manifest would be {} bytes, exceeding the limit of {} bytes", manifest.len(), client.limits.max_manifest_bytes),
            ));
        }
        let repo = to.r.name.as_str();
        client.ping()?;
        let (mut uploaded, mut mounted, mut existing) = (0, 0, 0);
        // config, index, then the result blobs by digest
        let mut uploads = vec![
            Upload { digest: sha256(EMPTY), size: EMPTY.len() as u64, media_type: EMPTY_MEDIA, file_hex: None, mount: None },
            Upload { digest: sha256(&index), size: index.len() as u64, media_type: CACHE_INDEX_MEDIA, file_hex: None, mount: None },
        ];
        {
            let st = self.lock();
            for (hex, size) in &blob_list {
                // A blob a --cache-from repository on this registry holds is mounted rather than uploaded.
                let mount = st
                    .sources
                    .iter()
                    .find(|s| s.client.registry == client.registry && s.repo != repo && s.blobs.get(hex) == Some(size))
                    .map(|s| s.repo.clone());
                uploads.push(Upload { digest: format!("sha256:{hex}"), size: *size, media_type: CACHE_BLOB_MEDIA, file_hex: Some(hex.clone()), mount });
            }
        }
        for Upload { digest, size, media_type, file_hex, mount } in &uploads {
            if client.has_blob(repo, digest)? {
                existing += 1;
                continue;
            }
            let desc = Descriptor { media_type: media_type.to_string(), digest: digest.clone(), size: *size, annotations: None };
            let pushed = match file_hex {
                Some(hex) => {
                    let path = blob_file(hex);
                    let open = move || -> Result<Box<dyn Read + Send>> {
                        let f = fs::File::open(&path).map_err(|e| SitefileError::plain(E_REGISTRY, format!("cannot read cached blob sha256:{hex}: {e}")))?;
                        Ok(Box::new(f))
                    };
                    client.push_blob(repo, &desc, &Body::Stream { length: *size, open: &open }, mount.as_deref())?
                }
                None if *media_type == EMPTY_MEDIA => client.push_blob(repo, &desc, &Body::Bytes(EMPTY), None)?,
                None => client.push_blob(repo, &desc, &Body::Bytes(&index), None)?,
            };
            match pushed {
                BlobPush::Uploaded => uploaded += 1,
                BlobPush::Mounted => mounted += 1,
            }
        }
        let tag = to.r.tag.clone().unwrap_or_else(|| "latest".into());
        let digest = client.put_manifest(repo, &tag, &manifest, media::MANIFEST)?;
        Ok(Pushed { entries: entries.len(), uploaded, mounted, existing, digest })
    }
}

impl RemoteCache for RegistryCache {
    fn used(&self, key: &str, entry: &CacheEntry) {
        self.lock().used.insert(key.to_string(), entry.clone());
    }

    fn fetch(&self, key: &str, blob_dir: &Path) -> Option<CacheEntry> {
        let mut st = self.lock();
        let st = &mut *st;
        for i in 0..st.sources.len() {
            if st.stopped {
                return None;
            }
            let s = &st.sources[i];
            let Some(entry) = (if s.disabled { None } else { s.entries.get(key).cloned() }) else { continue };
            let mut missing: Vec<(String, u64)> = Vec::new();
            for (_, hex, size) in &entry.files {
                let p = blob_dir.join(hex);
                match fs::metadata(&p) {
                    Ok(m) if m.is_file() && m.len() == *size => touch(&p),
                    _ => {
                        if !missing.iter().any(|(h, _)| h == hex) {
                            missing.push((hex.clone(), *size));
                        }
                    }
                }
            }
            let need: u64 = missing.iter().map(|(_, n)| n).sum();
            if self.o.limit != 0 && st.fetched_bytes + need > self.o.limit {
                st.stopped = true;
                (self.o.warn)(&format!("--cache-from: not fetching more cached results: they would exceed {CACHE_MAX_ENV}={}", format_limit(self.o.limit)));
                return None;
            }
            let res = missing.iter().try_for_each(|(hex, size)| Self::download(s, hex, *size, &blob_dir.join(hex)));
            match res {
                Ok(()) => {
                    st.fetched_bytes += need;
                    st.fetched += 1;
                    return Some(entry);
                }
                Err(e) => {
                    (self.o.warn)(&format!("--cache-from {}: {}; not using it for the rest of this build", s.name, e.message()));
                    st.sources[i].disabled = true;
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::cache::TransformCache;
    use crate::plugins::host::PluginResult;
    use crate::registry::mock::{clean_env, tmp, Auth, MockOptions, MockRegistry, SeedImage};
    use crate::registry::{make_client, Env, RegistryOptions};

    type Log = Arc<Mutex<Vec<String>>>;

    fn cache_for(from: Vec<String>, to: Option<String>, env: Env, limit: u64) -> (Arc<RegistryCache>, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let (lw, ln) = (log.clone(), log.clone());
        let warn: Callback = Arc::new(move |m: &str| lw.lock().unwrap().push(format!("warning: {m}")));
        let notice: Callback = Arc::new(move |m: &str| ln.lock().unwrap().push(m.to_string()));
        let client: ClientFactory = Arc::new(move |r: &str| make_client(r, &RegistryOptions::default(), None, &env));
        let o = RegistryCacheOptions {
            from: from.iter().map(|r| parse_cache_ref("--cache-from", r).unwrap()).collect(),
            to: to.map(|r| parse_cache_ref("--cache-to", &r).unwrap()),
            client,
            limit,
            warn,
            notice,
        };
        (Arc::new(RegistryCache::new(o)), log)
    }

    fn local(rc: &Arc<RegistryCache>) -> TransformCache {
        let remote: Arc<dyn RemoteCache> = rc.clone();
        TransformCache::new(tmp("rcache-").join("transform")).with_remote(Some(remote))
    }

    const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// Push two results to `reg/<repo>`; returns the local cache that wrote them.
    fn seed(reg: &MockRegistry, repo: &str) -> TransformCache {
        let (rc, log) = cache_for(vec![], Some(format!("{}/{repo}", reg.host)), clean_env(), 0);
        let mut c = local(&rc);
        c.put(K1, &PluginResult::File(b"one".to_vec()));
        c.put(K2, &PluginResult::Files(vec![("x/a.txt".into(), b"two-a".to_vec()), ("b.txt".into(), b"two-b".to_vec())]));
        rc.push(&c.dir);
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with(&format!("Cache to {}/{repo}: 2 entries, 5 blobs uploaded, 0 mounted, 0 already present (sha256:", reg.host)), "{lines:?}");
        c
    }

    fn blob_gets(reg: &MockRegistry, since: usize) -> usize {
        reg.requests()[since..].iter().filter(|q| q.method == "GET" && q.url.contains("/blobs/")).count()
    }

    #[test]
    fn push_then_fetch_lazily_on_a_hit() {
        let reg = MockRegistry::start(MockOptions::default());
        seed(&reg, "team/c:1");
        let m: Value = serde_json::from_slice(&reg.state.lock().unwrap().manifests["team/c:1"]).unwrap();
        assert_eq!(m["artifactType"], CACHE_ARTIFACT_TYPE);
        assert_eq!(m["config"]["mediaType"], EMPTY_MEDIA);
        assert_eq!(m["layers"][0]["mediaType"], CACHE_INDEX_MEDIA);
        assert_eq!(m["layers"].as_array().unwrap().len(), 4);

        let (rc, log) = cache_for(vec![format!("{}/team/c:1", reg.host)], None, clean_env(), 0);
        let start = reg.requests().len();
        rc.load();
        assert_eq!(log.lock().unwrap().clone(), vec![format!("Cache from {}/team/c:1: 2 entries", reg.host)]);
        assert_eq!(blob_gets(&reg, start), 1, "only the index");
        let mut c = local(&rc);
        let after_load = reg.requests().len();
        assert!(matches!(c.get(K1), Some(PluginResult::File(b)) if b == b"one"));
        assert_eq!(blob_gets(&reg, after_load), 1, "one blob for one hit");
        assert!(c.get(&"3".repeat(64)).is_none());
        assert_eq!(c.stats.hits, 1);
        assert_eq!(rc.fetched(), (1, 3));
        // fetched entries are local now: no more registry traffic
        let n = reg.requests().len();
        let mut again = TransformCache::new(c.dir.clone());
        assert!(again.get(K1).is_some());
        assert_eq!(reg.requests().len(), n);
        assert!(matches!(c.get(K2), Some(PluginResult::Files(f)) if f.len() == 2 && f[0].0 == "x/a.txt"));
        assert_eq!(rc.used_entries().len(), 2);
    }

    #[test]
    fn pushing_again_uploads_nothing_and_other_repos_mount() {
        let reg = MockRegistry::start(MockOptions::default());
        let c = seed(&reg, "team/c:1");
        let (rc, log) = cache_for(vec![format!("{}/team/c:1", reg.host)], Some(format!("{}/team/c:1", reg.host)), clean_env(), 0);
        rc.load();
        let mut l = TransformCache::new(c.dir.clone()).with_remote(Some(rc.clone() as Arc<dyn RemoteCache>));
        assert!(l.get(K1).is_some() && l.get(K2).is_some());
        rc.push(&c.dir);
        assert!(log.lock().unwrap()[1].contains("2 entries, 0 blobs uploaded, 0 mounted, 5 already present"), "{:?}", log.lock().unwrap());
        let (rc2, log2) = cache_for(vec![format!("{}/team/c:1", reg.host)], Some(format!("{}/team/d:1", reg.host)), clean_env(), 0);
        rc2.load();
        let mut l2 = TransformCache::new(c.dir.clone()).with_remote(Some(rc2.clone() as Arc<dyn RemoteCache>));
        assert!(l2.get(K1).is_some() && l2.get(K2).is_some());
        rc2.push(&c.dir);
        assert!(log2.lock().unwrap()[1].contains("2 entries, 2 blobs uploaded, 3 mounted, 0 already present"), "{:?}", log2.lock().unwrap());
        let st = reg.state.lock().unwrap();
        assert_eq!(sha256(&st.manifests["team/c:1"]), sha256(&st.manifests["team/d:1"]), "same entries, same bytes");
    }

    #[test]
    fn failures_only_warn() {
        let reg = MockRegistry::start(MockOptions::default());
        reg.seed_image("img/base", "1", SeedImage { layers: vec![vec![("/a", "a")]], ..Default::default() });
        let (rc, log) = cache_for(
            vec![format!("{}/none/here:1", reg.host), format!("{}/img/base:1", reg.host), "127.0.0.1:1/x/y:1".into()],
            Some("127.0.0.1:1/x/y:1".into()),
            clean_env(),
            0,
        );
        rc.load();
        rc.push(&tmp("empty-"));
        let lines = log.lock().unwrap().clone();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].starts_with(&format!("warning: --cache-from {}/none/here:1 not used: ", reg.host)) && lines[0].contains("HTTP 404"), "{lines:?}");
        assert_eq!(lines[1], format!("warning: --cache-from {0}/img/base:1 not used: {0}/img/base:1 is not a sitefile cache (artifactType missing)", reg.host));
        assert!(lines[2].starts_with("warning: --cache-from 127.0.0.1:1/x/y:1 not used: "), "{lines:?}");
        assert!(lines[3].starts_with("warning: --cache-to 127.0.0.1:1/x/y:1 not pushed: "), "{lines:?}");
    }

    #[test]
    fn a_corrupt_blob_drops_the_source_and_leaves_no_temp_file() {
        let reg = MockRegistry::start(MockOptions::default());
        seed(&reg, "team/c:1");
        let one = format!("sha256:{}", &sha256(b"one")[7..]);
        reg.set_options(|o| o.corrupt = Some(one.clone()));
        let (rc, log) = cache_for(vec![format!("{}/team/c:1", reg.host)], None, clean_env(), 0);
        rc.load();
        let mut c = local(&rc);
        assert!(c.get(K1).is_none());
        assert!(c.get(K2).is_none(), "the source is not used again");
        let lines = log.lock().unwrap().clone();
        assert_eq!(
            lines[1],
            format!("warning: --cache-from {}/team/c:1: cached blob {one} does not match its digest or size; not using it for the rest of this build", reg.host)
        );
        assert_eq!(lines.len(), 2);
        let left: Vec<_> = fs::read_dir(c.dir.join("blobs")).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn the_size_limit_bounds_what_is_fetched() {
        let reg = MockRegistry::start(MockOptions::default());
        seed(&reg, "team/c:1");
        let (rc, log) = cache_for(vec![format!("{}/team/c:1", reg.host)], None, clean_env(), 4);
        rc.load();
        let mut c = local(&rc);
        assert!(c.get(K1).is_some(), "3 bytes fit");
        assert!(c.get(K2).is_none(), "10 more do not");
        assert_eq!(log.lock().unwrap()[1], "warning: --cache-from: not fetching more cached results: they would exceed SITEFILE_CACHE_MAX=4B");
    }

    #[test]
    fn credentials_come_from_the_environment_and_are_never_pushed() {
        let secret = "cache-s3cr3t-do-not-push";
        let reg = MockRegistry::start(MockOptions { auth: Auth::Bearer, user: "ci".into(), pass: secret.into(), anonymous_pull: false, ..Default::default() });
        let target = format!("{}/private/c:1", reg.host);
        let (rc, log) = cache_for(vec![target.clone()], Some(target.clone()), clean_env(), 0);
        rc.load();
        let mut c = local(&rc);
        c.put(K1, &PluginResult::File(b"one".to_vec()));
        rc.push(&c.dir);
        let lines = log.lock().unwrap().clone();
        assert!(lines[0].starts_with(&format!("warning: --cache-from {target} not used: ")), "{lines:?}");
        assert!(lines[1].starts_with(&format!("warning: --cache-to {target} not pushed: ")) && lines[1].contains("no credentials were found"), "{lines:?}");
        let mut env = clean_env();
        env.insert("SITEFILE_REGISTRY_AUTH".into(), json!({ reg.host.clone(): format!("ci:{secret}") }).to_string());
        let (rc2, log2) = cache_for(vec![], Some(target.clone()), env.clone(), 0);
        let mut c2 = local(&rc2);
        c2.put(K1, &PluginResult::File(b"one".to_vec()));
        rc2.push(&c2.dir);
        assert!(log2.lock().unwrap()[0].starts_with(&format!("Cache to {target}: 1 entry, 3 blobs uploaded")), "{:?}", log2.lock().unwrap());
        let (rc3, log3) = cache_for(vec![target.clone()], None, env, 0);
        rc3.load();
        assert!(local(&rc3).get(K1).is_some());
        for l in log.lock().unwrap().iter().chain(log2.lock().unwrap().iter()).chain(log3.lock().unwrap().iter()) {
            assert!(!l.contains(secret));
        }
        for b in reg.state.lock().unwrap().blobs.values() {
            assert!(!String::from_utf8_lossy(b).contains(secret));
        }
    }

    #[test]
    fn cache_refs_need_a_registry_host_and_cache_to_a_tag() {
        assert_eq!(
            parse_cache_ref("--cache-from", "team/cache:1").unwrap_err().detail,
            "--cache-from 'team/cache:1' has no registry host (e.g. localhost:5000/team/site-cache:main)"
        );
        let d = format!("localhost:5000/c@sha256:{}", "a".repeat(64));
        assert_eq!(parse_cache_ref("--cache-to", &d).unwrap_err().detail, format!("--cache-to '{d}' must name a tag, not a digest"));
        assert!(parse_cache_ref("--cache-from", &d).is_ok());
        assert_eq!(parse_cache_ref("--cache-from", "docker.io/cache").unwrap().r.name, "library/cache");
    }

    #[test]
    fn index_validation_rejects_unsafe_or_inconsistent_entries() {
        let hex = "a".repeat(64);
        let key = "b".repeat(64);
        let blobs: HashMap<String, u64> = [(hex.clone(), 3)].into_iter().collect();
        let ok = json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["x/y.txt", hex, 3]] } } });
        assert_eq!(parse_cache_index(ok.to_string().as_bytes(), &blobs).unwrap().len(), 1);
        let bad = [
            json!({ "version": 2, "entries": {} }),
            json!({ "version": 1, "entries": [] }),
            json!({ "version": 1, "entries": { "nothex": { "kind": "file", "files": [["", hex, 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "file", "files": [["a", hex, 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["../x", hex, 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["/abs", hex, 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["a", hex, 3], ["a", hex, 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["a", hex, 4]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "files", "files": [["a", "c".repeat(64), 3]] } } }),
            json!({ "version": 1, "entries": { key.clone(): { "kind": "dir", "files": [] } } }),
        ];
        for b in bad {
            assert_eq!(parse_cache_index(b.to_string().as_bytes(), &blobs).unwrap_err().detail, "the cache index is invalid", "{b}");
        }
        assert!(parse_cache_index(b"{", &blobs).is_err());
    }

    #[test]
    fn golden_bytes_match_the_typescript_encoder() {
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|x| x.repeat(64));
        let mut entries = BTreeMap::new();
        entries.insert(c, CacheEntry { kind: "files".into(), files: vec![("x/y.txt".into(), d.clone(), 5), ("z.txt".into(), a.clone(), 3)] });
        entries.insert(b, CacheEntry { kind: "file".into(), files: vec![(String::new(), a.clone(), 3)] });
        let index = encode_cache_index(&entries);
        assert_eq!(sha256(&index), "sha256:a240770e79b2f3734cbef07e5478cdbaee4db476baf9e09d563a338275760f67");
        assert_eq!(sha256(&encode_cache_manifest(&index, &[(d, 5), (a, 3)])), "sha256:b36173b65e53cb51601880e5f54e3ca044ff17783b7cfb510551b9efc44f12fa");
    }
}

