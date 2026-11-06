//! The local image store (port of `src/store.ts`). It IS an OCI image layout
//! (image-spec v1.1) at `$SITEFILE_HOME` (default `~/.sitefile`), in the same
//! on-disk format as the TypeScript store, so both CLIs share one store:
//!
//! ```text
//! oci-layout           {"imageLayoutVersion":"1.0.0"}
//! index.json           image index; tags are entries annotated
//!                      org.opencontainers.image.ref.name = "name:tag"; an untagged
//!                      image is a "dangling" entry without it; dev.sitefile.created
//!                      records when the image was first stored
//! blobs/sha256/<hex>   manifest, config and layer blobs (identical layers share a blob)
//! referrers/sha256/<hex>  attestations of image <hex> (`build -t --provenance/--sbom`):
//!                      an OCI image index of artifact manifests whose subject is that
//!                      image (their blobs are in blobs/)
//! servers/             site server state and logs (see servers.rs)
//! lock                 advisory lock for mutations
//! ```
//!
//! IMAGE ID = the manifest digest. All writes are temp file + rename,
//! mutations hold the lock, and every blob is digest-verified when read.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::export::archive_writers::iso_millis;
use crate::export::attest::{decode_referrers_index, encode_referrers_index, Referrer, ReferrerDescriptor, PREDICATE_ANNOTATION};
use crate::export::exporter::random_hex;
use crate::export::oci::{
    ann, blob_path_of, build_oci_image, decode_index, encode_index, extract_image_files_at, layout_tree, parse_config, parse_manifest,
    site_layer_index, IndexEntry, LayoutParts, OciOptions, ParsedConfig, ParsedManifest, OCI_LAYOUT_BYTES,
};
use crate::refs::{looks_like_image_id, parse_local_ref, ref_key, ImageRef};
use crate::tree::Tree;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    /// Full `sha256:<hex>` image ID (the manifest digest).
    pub id: String,
    pub tags: Vec<String>,
    /// When the image was first stored.
    pub created: String,
    pub files: u64,
    /// Total bytes of the site's files (uncompressed).
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDetails {
    pub id: String,
    pub tags: Vec<String>,
    pub created: String,
    pub files: u64,
    pub size: u64,
    pub manifest: ParsedManifest,
    pub config: ParsedConfig,
    pub root: String,
    /// Compressed layer size in bytes.
    pub layer_size: u64,
}

impl ImageDetails {
    pub fn summary(&self) -> ImageSummary {
        ImageSummary { id: self.id.clone(), tags: self.tags.clone(), created: self.created.clone(), files: self.files, size: self.size }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResult {
    pub id: String,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveResult {
    pub untagged: Vec<String>,
    pub deleted: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneResult {
    pub manifests: usize,
    pub blobs: usize,
    pub bytes: u64,
}

/// See [`Store::gc_view`].
#[derive(Debug, Clone, Default)]
pub struct GcView {
    pub live: BTreeSet<String>,
    /// `(image ID, blob hexes)` of each untagged image.
    pub untagged: Vec<(String, Vec<String>)>,
}

/// `$SITEFILE_HOME`, else `~/.sitefile`.
pub fn resolve_store_home() -> PathBuf {
    resolve_store_home_from(std::env::var("SITEFILE_HOME").ok().as_deref())
}

pub fn resolve_store_home_from(h: Option<&str>) -> PathBuf {
    let p = match h {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into())).join(".sitefile"),
    };
    crate::export::exporter::resolve_path(&p)
}

fn store_error(msg: impl Into<String>) -> SitefileError {
    SitefileError::plain(E_STORE, msg)
}

pub fn short_id(id: &str) -> String {
    id.strip_prefix("sha256:").unwrap_or(id).chars().take(12).collect()
}

/// The current time as `Date.prototype.toISOString()` would print it.
pub fn now_iso() -> String {
    iso_of_time(SystemTime::now())
}

pub fn iso_of_time(t: SystemTime) -> String {
    let ms = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    };
    iso_millis(ms)
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Paths added (`A`), removed (`D`) or changed (`C`) going from `a` to `b`
/// (the `image diff` output lines, sorted by path).
pub fn diff_trees(a: &Tree, b: &Tree) -> Vec<String> {
    let paths: BTreeSet<&String> = a.entries().map(|(p, _)| p).chain(b.entries().map(|(p, _)| p)).collect();
    let mut out = Vec::new();
    for p in paths {
        match (a.get(p), b.get(p)) {
            (None, Some(_)) => out.push(format!("A {p}")),
            (Some(_), None) => out.push(format!("D {p}")),
            (Some(x), Some(y)) if x.digest != y.digest || x.mode != y.mode => out.push(format!("C {p}")),
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Store {
    pub home: PathBuf,
}

struct LockGuard(PathBuf);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Store {
    /// Open (creating if needed) the store at `home`.
    pub fn open(home: impl AsRef<Path>) -> Result<Store> {
        let s = Store { home: crate::export::exporter::resolve_path(home.as_ref()) };
        let blobs = s.home.join("blobs").join("sha256");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&blobs)
            .map_err(|e| store_error(format!("cannot open the store at '{}': {e}", s.home.display())))?;
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(s.home.join("oci-layout")) {
            Ok(mut f) => f.write_all(OCI_LAYOUT_BYTES).map_err(|e| store_error(format!("cannot write oci-layout: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(store_error(format!("cannot open the store at '{}': {e}", s.home.display()))),
        }
        Ok(s)
    }

    /// The store at `home` if one exists there (nothing is created).
    pub fn open_existing(home: impl AsRef<Path>) -> Option<Store> {
        let home = crate::export::exporter::resolve_path(home.as_ref());
        fs::metadata(home.join("blobs").join("sha256")).ok().filter(|m| m.is_dir()).map(|_| Store { home })
    }

    /// Open the store at `$SITEFILE_HOME` or `~/.sitefile`.
    pub fn open_default() -> Result<Store> {
        Store::open(resolve_store_home())
    }

    /// The file holding a blob.
    pub fn blob_file(&self, digest: &str) -> Result<PathBuf> {
        Ok(self.home.join(&blob_path_of(digest)?[1..]))
    }

    // ------------------------------------------------------------ locking

    /// Run `f` holding the store lock (an exclusive lock file; stale locks are broken).
    pub fn with_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let lock = self.home.join("lock");
        let deadline = SystemTime::now() + Duration::from_secs(20);
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&lock) {
                Ok(mut fh) => {
                    let _ = fh.write_all(std::process::id().to_string().as_bytes());
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&lock) {
                        let _ = fs::remove_file(&lock);
                        continue;
                    }
                    if SystemTime::now() > deadline {
                        return Err(store_error(format!("timed out waiting for the store lock '{}'", lock.display())));
                    }
                    let jitter = u64::from_str_radix(&random_hex(1), 16).unwrap_or(0) * 30 / 255;
                    std::thread::sleep(Duration::from_millis(20 + jitter));
                }
                Err(e) => return Err(store_error(format!("cannot lock the store at '{}': {e}", self.home.display()))),
            }
        }
        let _guard = LockGuard(lock);
        f()
    }

    fn write_atomic(&self, dest: &Path, data: &[u8]) -> Result<()> {
        let dir = dest.parent().unwrap_or(&self.home);
        let tmp = dir.join(format!(".tmp-{}", random_hex(6)));
        let r = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(data)?;
            drop(f);
            fs::rename(&tmp, dest)
        })();
        r.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            store_error(format!("cannot write '{}': {e}", dest.display()))
        })
    }

    // ------------------------------------------------------------ index

    fn read_index(&self) -> Result<Vec<IndexEntry>> {
        let bytes = match fs::read(self.home.join("index.json")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(store_error(format!("cannot read index.json: {e}"))),
        };
        decode_index(&bytes).map_err(|e| store_error(format!("index.json is corrupt: {e}")))
    }

    /// Normalize (one dangling entry per untagged image, none for tagged ones), sort, write.
    fn write_index(&self, entries: Vec<IndexEntry>) -> Result<()> {
        let tagged: BTreeSet<String> = entries.iter().filter(|e| e.ref_name.is_some()).map(|e| e.digest.clone()).collect();
        let mut seen = BTreeSet::new();
        let mut out: Vec<IndexEntry> = entries
            .into_iter()
            .filter(|e| {
                if e.ref_name.is_some() {
                    return true;
                }
                if tagged.contains(&e.digest) || seen.contains(&e.digest) {
                    return false;
                }
                seen.insert(e.digest.clone());
                true
            })
            .collect();
        let key = |e: &IndexEntry| format!("{}{}", e.digest, e.ref_name.as_deref().unwrap_or(""));
        out.sort_by(|a, b| key(a).encode_utf16().cmp(key(b).encode_utf16()));
        self.write_atomic(&self.home.join("index.json"), &encode_index(&out))
    }

    /// `name:tag` -> image ID.
    pub fn refs(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.read_index()?.into_iter().filter_map(|e| e.ref_name.map(|r| (r, e.digest))).collect())
    }

    // ------------------------------------------------------------ blobs

    /// Read a blob and verify its digest (and size when given).
    pub fn read_blob(&self, digest: &str, size: Option<u64>) -> Result<Vec<u8>> {
        let bytes = match fs::read(self.blob_file(digest)?) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(store_error(format!("blob {digest} is missing"))),
            Err(e) => return Err(store_error(format!("cannot read blob {digest}: {e}"))),
        };
        if size.is_some_and(|s| s != bytes.len() as u64) || sha256(&bytes) != digest {
            return Err(store_error(format!("blob {digest} is corrupt: its content does not match its digest")));
        }
        Ok(bytes)
    }

    fn put_blob(&self, digest: &str, bytes: &[u8]) -> Result<()> {
        let bp = self.blob_file(digest)?;
        if let Ok(st) = fs::metadata(&bp) {
            if st.is_file() && st.len() == bytes.len() as u64 {
                self.touch_blob(digest);
                return Ok(());
            }
        }
        self.write_atomic(&bp, bytes)
    }

    /// Mark a blob as just used (its mtime is the recency the size limit goes
    /// by, see cache_limit.rs; atime is often disabled). Never fails.
    pub fn touch_blob(&self, digest: &str) {
        if let Ok(p) = self.blob_file(digest) {
            if let Ok(f) = fs::OpenOptions::new().write(true).open(p) {
                let _ = f.set_modified(SystemTime::now());
            }
        }
    }

    /// True when the blob exists with the given size (content is verified when it was written).
    pub fn has_blob(&self, digest: &str, size: Option<u64>) -> bool {
        match self.blob_file(digest).ok().and_then(|p| fs::metadata(p).ok()) {
            Some(st) => st.is_file() && size.is_none_or(|s| s == st.len()),
            None => false,
        }
    }

    /// Open a stored blob for streaming (for upload); write-time verification means no re-hash here.
    pub fn blob_reader(&self, digest: &str) -> Result<fs::File> {
        fs::File::open(self.blob_file(digest)?).map_err(|e| store_error(format!("cannot read blob {digest}: {e}")))
    }

    /// Store a blob from a stream, verifying its size and sha256 while it is
    /// written to a temp file that is renamed into place only when both match.
    /// `max_bytes` (default `size`) bounds what is accepted.
    pub fn import_blob(&self, digest: &str, size: u64, source: &mut dyn Read, max_bytes: Option<u64>) -> Result<()> {
        let max_bytes = max_bytes.unwrap_or(size);
        let dest = self.blob_file(digest)?;
        let tmp = dest.parent().unwrap_or(&self.home).join(format!(".tmp-{}", random_hex(6)));
        let mut fh = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| store_error(format!("cannot write '{}': {e}", tmp.display())))?;
        let r = (|| -> Result<()> {
            let mut hash = Sha256::new();
            let mut total: u64 = 0;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match source.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(SitefileError::plain(E_FETCH, format!("blob {digest}: {e}"))),
                };
                total += n as u64;
                if total > size || total > max_bytes {
                    return Err(SitefileError::plain(E_TOO_LARGE, format!("blob {digest} is larger than its declared size ({size} bytes)")));
                }
                hash.update(&buf[..n]);
                fh.write_all(&buf[..n]).map_err(|e| store_error(format!("cannot write '{}': {e}", tmp.display())))?;
            }
            if total != size {
                return Err(SitefileError::plain(E_INTEGRITY, format!("blob {digest} is truncated: got {total} of {size} bytes")));
            }
            let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if format!("sha256:{got}") != digest {
                return Err(SitefileError::plain(E_INTEGRITY, format!("blob {digest}: content does not match its digest")));
            }
            fs::rename(&tmp, &dest).map_err(|e| store_error(format!("cannot write '{}': {e}", dest.display())))
        })();
        if r.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        r
    }

    /// Store a small blob (manifest, config, index) given its bytes; returns its digest.
    pub fn put_small_blob(&self, bytes: &[u8]) -> Result<String> {
        let digest = sha256(bytes);
        self.put_blob(&digest, bytes)?;
        Ok(digest)
    }

    /// Register a pulled image manifest (all its blobs must already be stored),
    /// optionally tagged (replacing that tag's previous target).
    pub fn add_manifest(&self, manifest_bytes: &[u8], tag: Option<&ImageRef>) -> Result<String> {
        self.add_manifest_at(manifest_bytes, tag, SystemTime::now())
    }

    pub fn add_manifest_at(&self, manifest_bytes: &[u8], tag: Option<&ImageRef>, now: SystemTime) -> Result<String> {
        let id = self.put_small_blob(manifest_bytes)?;
        let parsed = parse_manifest(manifest_bytes).map_err(|e| store_error(format!("manifest {id} is not a usable image manifest: {e}")))?;
        for d in std::iter::once(&parsed.config).chain(parsed.layers.iter()) {
            if !self.has_blob(&d.digest, Some(d.size)) {
                return Err(store_error(format!("image {} is incomplete: blob {} is missing", short_id(&id), d.digest)));
            }
        }
        self.with_lock(|| {
            let mut entries = self.read_index()?;
            if !entries.iter().any(|e| e.digest == id) {
                entries.push(IndexEntry { digest: id.clone(), size: manifest_bytes.len() as u64, stored: Some(iso_of_time(now)), ..Default::default() });
                self.write_index(entries)?;
            }
            Ok(())
        })?;
        if let Some(t) = tag {
            self.tag(&id, t)?;
        }
        Ok(id)
    }

    /// Whether an image with this manifest digest is in the index.
    pub fn has_image(&self, id: &str) -> Result<bool> {
        Ok(self.read_index()?.iter().any(|e| e.digest == id))
    }

    // ------------------------------------------------------------ images

    /// Store a tree as an OCI image (idempotent: identical content and options give the same ID). Untagged.
    pub fn add_image(&self, tree: &Tree, options: &OciOptions) -> Result<AddResult> {
        self.add_image_at(tree, options, SystemTime::now())
    }

    pub fn add_image_at(&self, tree: &Tree, options: &OciOptions, now: SystemTime) -> Result<AddResult> {
        let img = build_oci_image(tree, options)?;
        self.with_lock(|| {
            self.put_blob(&img.layer_digest, &img.layer_bytes)?;
            self.put_blob(&img.config_digest, &img.config_bytes)?;
            self.put_blob(&img.manifest_digest, &img.manifest_bytes)?;
            let mut entries = self.read_index()?;
            if entries.iter().any(|e| e.digest == img.manifest_digest) {
                return Ok(AddResult { id: img.manifest_digest.clone(), created: false });
            }
            let _ = fs::remove_file(self.referrers_file(&img.manifest_digest)?); // left over from a removed image
            entries.push(IndexEntry {
                digest: img.manifest_digest.clone(),
                size: img.manifest_bytes.len() as u64,
                stored: Some(iso_of_time(now)),
                ..Default::default()
            });
            self.write_index(entries)?;
            Ok(AddResult { id: img.manifest_digest.clone(), created: true })
        })
    }

    /// Manifest and config of an image, digest-verified (not its layer).
    pub fn details(&self, id: &str) -> Result<ImageDetails> {
        let entries: Vec<IndexEntry> = self.read_index()?.into_iter().filter(|e| e.digest == id).collect();
        let Some(first) = entries.first() else {
            return Err(SitefileError::plain(E_NOT_FOUND, format!("no such image: {}", short_id(id))));
        };
        let manifest_bytes = self.read_blob(id, Some(first.size))?;
        let corrupt = |e: String| store_error(format!("image {} is corrupt: {e}", short_id(id)));
        let manifest = parse_manifest(&manifest_bytes).map_err(corrupt)?;
        let config = parse_config(&self.read_blob(&manifest.config.digest, Some(manifest.config.size))?).map_err(corrupt)?;
        let mut stored: Vec<&String> = entries.iter().filter_map(|e| e.stored.as_ref().filter(|s| !s.is_empty())).collect();
        stored.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        let created = stored.first().map(|s| s.to_string()).unwrap_or_else(|| config.created.clone());
        let num = |k: &str| manifest.annotations.get(k).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        let layer_size = site_layer_index(&manifest, &config)
            .and_then(|i| manifest.layers.get(i))
            .or(manifest.layers.first())
            .map(|l| l.size)
            .unwrap_or(0);
        Ok(ImageDetails {
            id: id.to_string(),
            tags: entries.iter().filter_map(|e| e.ref_name.clone()).collect(),
            created,
            files: num(ann::FILES),
            size: num(ann::SIZE),
            root: config.labels.get(ann::ROOT).cloned().unwrap_or_else(|| "/".into()),
            manifest,
            config,
            layer_size,
        })
    }

    /// Load an image's files (under its root), verifying manifest, config, layer digest and diff_id.
    pub fn load_tree(&self, id: &str) -> Result<Tree> {
        self.load_tree_with(id, None, None)
    }

    /// [`Store::load_tree`] with the origin the files' origins and error messages name (default
    /// `image:<short id>`) and a directory inside the image to root the tree at (default: its root label).
    pub fn load_tree_with(&self, id: &str, origin: Option<&str>, root: Option<&str>) -> Result<Tree> {
        let d = self.details(id)?;
        let origin = origin.map(str::to_string).unwrap_or_else(|| format!("image:{}", short_id(id)));
        let Some(layer) = site_layer_index(&d.manifest, &d.config).and_then(|i| d.manifest.layers.get(i)) else {
            return Err(store_error(format!("{origin}: not a Sitefile image (it has {} layers and no site layer)", d.manifest.layers.len())));
        };
        let bytes = self.read_blob(&layer.digest, Some(layer.size))?;
        extract_image_files_at(&d.manifest, &d.config, &bytes, &origin, root)
    }

    /// The image as a standalone OCI layout Tree (for `save`), optionally
    /// annotated with a tag. Includes every layer, so an image built on a BASE
    /// is self-contained.
    pub fn layout_of(&self, id: &str, ref_name: Option<&str>) -> Result<Tree> {
        let d = self.details(id)?;
        let Some(site) = site_layer_index(&d.manifest, &d.config).and_then(|i| d.manifest.layers.get(i)) else {
            return Err(store_error(format!("image {} is not a Sitefile image and cannot be saved", short_id(id))));
        };
        let mut base: Vec<(String, Vec<u8>)> = Vec::new();
        for l in &d.manifest.layers {
            if l.digest != site.digest && !base.iter().any(|(dg, _)| *dg == l.digest) {
                base.push((l.digest.clone(), self.read_blob(&l.digest, Some(l.size))?));
            }
        }
        let manifest_bytes = self.read_blob(id, None)?;
        let config_bytes = self.read_blob(&d.manifest.config.digest, None)?;
        let layer_bytes = self.read_blob(&site.digest, Some(site.size))?;
        layout_tree(
            LayoutParts {
                manifest_bytes: &manifest_bytes,
                manifest_digest: id,
                config_bytes: &config_bytes,
                config_digest: &d.manifest.config.digest,
                layer_bytes: &layer_bytes,
                layer_digest: &site.digest,
            },
            ref_name,
            &base,
            &self.referrers(id)?,
        )
    }

    // ------------------------------------------------------------ referrers (attestations)

    fn referrers_file(&self, id: &str) -> Result<PathBuf> {
        blob_path_of(id)?; // validates the digest
        Ok(self.home.join("referrers").join("sha256").join(&id[7..]))
    }

    /// Descriptors of the attestations stored for image `id` (none: empty).
    pub fn referrer_descriptors(&self, id: &str) -> Result<Vec<ReferrerDescriptor>> {
        let bytes = match fs::read(self.referrers_file(id)?) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(store_error(format!("cannot read the attestations of {}: {e}", short_id(id)))),
        };
        decode_referrers_index(&bytes).map_err(|e| store_error(format!("the attestations of {} are corrupt: {e}", short_id(id))))
    }

    /// The attestations stored for image `id`, with their blobs (digest-verified).
    pub fn referrers(&self, id: &str) -> Result<Vec<Referrer>> {
        let mut out = Vec::new();
        for d in self.referrer_descriptors(id)? {
            let manifest_bytes = self.read_blob(&d.digest, Some(d.size))?;
            let m = parse_manifest(&manifest_bytes)
                .map_err(|e| store_error(format!("attestation {} of {} is corrupt: {e}", short_id(&d.digest), short_id(id))))?;
            let mut blobs = vec![(d.digest.clone(), manifest_bytes)];
            for b in std::iter::once(&m.config).chain(m.layers.iter()) {
                blobs.push((b.digest.clone(), self.read_blob(&b.digest, Some(b.size))?));
            }
            out.push(Referrer { descriptor: d, blobs });
        }
        Ok(out)
    }

    /// Attach attestations to stored image `id`: their blobs are written, and
    /// each replaces a stored attestation with the same predicate type (others are kept).
    pub fn put_referrers(&self, id: &str, referrers: &[Referrer]) -> Result<()> {
        if referrers.is_empty() {
            return Ok(());
        }
        self.with_lock(|| {
            if !self.read_index()?.iter().any(|e| e.digest == id) {
                return Err(SitefileError::plain(E_NOT_FOUND, format!("no such image: {}", short_id(id))));
            }
            for r in referrers {
                for (d, b) in &r.blobs {
                    self.put_blob(d, b)?;
                }
            }
            let kind = |d: &ReferrerDescriptor| {
                (d.artifact_type.clone().unwrap_or_default(), d.annotations.as_ref().and_then(|a| a.get(PREDICATE_ANNOTATION)).cloned().unwrap_or_default())
            };
            let fresh: Vec<_> = referrers.iter().map(|r| kind(&r.descriptor)).collect();
            let mut list: Vec<ReferrerDescriptor> = self
                .referrer_descriptors(id)?
                .into_iter()
                .filter(|d| !fresh.contains(&kind(d)) && !referrers.iter().any(|r| r.descriptor.digest == d.digest))
                .collect();
            list.extend(referrers.iter().map(|r| r.descriptor.clone()));
            let file = self.referrers_file(id)?;
            if let Some(dir) = file.parent() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|e| store_error(format!("cannot create '{}': {e}", dir.display())))?;
            }
            self.write_atomic(&file, &encode_referrers_index(&list))
        })
    }

    /// Blob hexes of the attestations of `id`; unreadable ones count as none.
    fn referrer_blob_hexes(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        for d in self.referrer_descriptors(id).unwrap_or_default() {
            out.push(d.digest[7..].to_string());
            if let Ok(m) = self.read_blob(&d.digest, Some(d.size)).and_then(|b| parse_manifest(&b).map_err(store_error)) {
                for b in std::iter::once(&m.config).chain(m.layers.iter()) {
                    out.push(b.digest[7..].to_string());
                }
            }
        }
        out
    }

    /// Every stored image, newest first.
    pub fn list(&self) -> Result<Vec<ImageSummary>> {
        let mut ids: Vec<String> = Vec::new();
        for e in self.read_index()? {
            if !ids.contains(&e.digest) {
                ids.push(e.digest);
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.details(&id)?.summary());
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    fn ids_with_prefix(&self, input: &str) -> Result<Vec<String>> {
        let want = format!("sha256:{}", input.strip_prefix("sha256:").unwrap_or(input));
        let ids: BTreeSet<String> = self.read_index()?.into_iter().map(|e| e.digest).filter(|d| d.starts_with(&want)).collect();
        Ok(ids.into_iter().collect())
    }

    /// Resolve a reference or (short) image ID to a full image ID.
    pub fn resolve(&self, input: &str) -> Result<String> {
        if looks_like_image_id(input) {
            let matches = self.ids_with_prefix(input)?;
            if matches.len() == 1 {
                return Ok(matches[0].clone());
            }
            if matches.len() > 1 {
                return Err(store_error(format!("ambiguous image ID '{input}' matches {} images", matches.len())));
            }
        }
        let r = parse_local_ref(input)?;
        if let Some(d) = &r.digest {
            if !self.read_index()?.iter().any(|e| &e.digest == d) {
                return Err(SitefileError::plain(E_NOT_FOUND, format!("no such image: {input}")));
            }
            return Ok(d.clone());
        }
        self.refs()?.remove(&ref_key(&r)).ok_or_else(|| SitefileError::plain(E_NOT_FOUND, format!("no such image: {input}")))
    }

    /// Point `name:tag` at an image (replacing any previous target). Returns the `name:tag` key.
    pub fn tag(&self, id: &str, r: &ImageRef) -> Result<String> {
        if r.digest.is_some() {
            return Err(SitefileError::plain(E_USAGE, "a tag must be name[:tag] or registry/name[:tag] (no digest)"));
        }
        let key = ref_key(r);
        self.with_lock(|| {
            let entries = self.read_index()?;
            let Some(mine) = entries.iter().find(|e| e.digest == id).cloned() else {
                return Err(SitefileError::plain(E_NOT_FOUND, format!("no such image: {}", short_id(id))));
            };
            let previous = entries.iter().find(|e| e.ref_name.as_deref() == Some(key.as_str())).cloned();
            let mut next: Vec<IndexEntry> = entries.iter().filter(|e| e.ref_name.as_deref() != Some(key.as_str())).cloned().collect();
            // The old target stays as an untagged image if this was its last tag.
            if let Some(p) = previous {
                if !next.iter().any(|e| e.digest == p.digest) {
                    next.push(IndexEntry { ref_name: None, ..p });
                }
            }
            let stored = entries.iter().find(|e| e.digest == id && e.stored.as_deref().is_some_and(|s| !s.is_empty())).and_then(|e| e.stored.clone());
            next.push(IndexEntry { digest: id.to_string(), size: mine.size, ref_name: Some(key.clone()), stored, ..Default::default() });
            self.write_index(next)
        })?;
        Ok(key)
    }

    /// `rmi`: remove a tag, or an image by ID. The image leaves the index when
    /// no tag references it (blobs are reclaimed by `prune`). Removing by ID
    /// while several tags point at it needs `force`.
    pub fn remove_image(&self, input: &str, force: bool) -> Result<RemoveResult> {
        self.with_lock(|| {
            let mut entries = self.read_index()?;
            let mut untagged = Vec::new();
            let id_matches = if looks_like_image_id(input) { self.ids_with_prefix(input)? } else { vec![] };
            let id;
            if !id_matches.is_empty() || crate::digest::is_integrity(input) {
                id = self.resolve(input)?;
                let tags: Vec<String> = entries.iter().filter(|e| e.digest == id).filter_map(|e| e.ref_name.clone()).collect();
                if tags.len() > 1 && !force {
                    return Err(store_error(format!(
                        "image {} is referenced by {} tags ({}); remove a tag, or use -f",
                        short_id(&id),
                        tags.len(),
                        tags.join(", ")
                    )));
                }
                untagged.extend(tags);
                entries.retain(|e| e.digest != id);
            } else {
                let r = parse_local_ref(input)?;
                if r.digest.is_some() {
                    return Err(SitefileError::plain(E_USAGE, "rmi takes name[:tag] or an image ID, not name@digest"));
                }
                let key = ref_key(&r);
                let Some(target) = entries.iter().find(|e| e.ref_name.as_deref() == Some(key.as_str())) else {
                    return Err(SitefileError::plain(E_NOT_FOUND, format!("no such image: {input}")));
                };
                id = target.digest.clone();
                untagged.push(key.clone());
                entries.retain(|e| e.ref_name.as_deref() != Some(key.as_str()));
                if entries.iter().any(|e| e.digest == id) {
                    self.write_index(entries)?;
                    return Ok(RemoveResult { untagged, deleted: None });
                }
            }
            self.write_index(entries)?;
            let _ = fs::remove_file(self.referrers_file(&id)?);
            Ok(RemoveResult { untagged, deleted: Some(id) })
        })
    }

    /// Drop untagged images from the index, then delete every blob no remaining image uses.
    /// What the size limit may evict: the blob hexes of tagged images (never
    /// evicted), and each untagged image with the blobs it uses. An unreadable
    /// tagged manifest is an error (its blobs are unknown, so nothing may go).
    pub fn gc_view(&self) -> Result<GcView> {
        let entries = self.read_index()?;
        let blobs_of = |id: &str| -> Result<Vec<String>> {
            let m = parse_manifest(&self.read_blob(id, None)?).map_err(|e| store_error(format!("image {} is corrupt: {e}", short_id(id))))?;
            let mut v: Vec<String> =
                std::iter::once(id).chain(std::iter::once(m.config.digest.as_str())).chain(m.layers.iter().map(|l| l.digest.as_str())).map(|d| d[7..].to_string()).collect();
            v.extend(self.referrer_blob_hexes(id));
            Ok(v)
        };
        let mut live = BTreeSet::new();
        let tagged: BTreeSet<&str> = entries.iter().filter(|e| e.ref_name.is_some()).map(|e| e.digest.as_str()).collect();
        for id in &tagged {
            live.extend(blobs_of(id)?);
        }
        let mut untagged = Vec::new();
        let mut seen = BTreeSet::new();
        for e in entries.iter().filter(|e| e.ref_name.is_none()) {
            if !seen.insert(e.digest.clone()) || live.contains(&e.digest[7..]) {
                continue;
            }
            let blobs = blobs_of(&e.digest).unwrap_or_else(|_| vec![e.digest[7..].to_string()]);
            untagged.push((e.digest.clone(), blobs));
        }
        Ok(GcView { live, untagged })
    }

    /// Remove an untagged image from the index (its blobs stay). The caller holds the lock.
    pub fn drop_untagged_locked(&self, id: &str) -> Result<()> {
        let entries = self.read_index()?;
        if entries.iter().any(|e| e.digest == id && e.ref_name.is_some()) {
            return Ok(());
        }
        self.write_index(entries.into_iter().filter(|e| e.digest != id).collect())?;
        let _ = fs::remove_file(self.referrers_file(id)?);
        Ok(())
    }

    pub fn prune(&self) -> Result<PruneResult> {
        self.with_lock(|| {
            let entries = self.read_index()?;
            let keep: Vec<IndexEntry> = entries.iter().filter(|e| e.ref_name.is_some()).cloned().collect();
            let mut dropped: BTreeSet<String> = entries.iter().filter(|e| e.ref_name.is_none()).map(|e| e.digest.clone()).collect();
            for e in &keep {
                dropped.remove(&e.digest);
            }
            self.write_index(keep.clone())?;
            let mut live: BTreeSet<String> = BTreeSet::new();
            let ids: BTreeSet<String> = keep.iter().map(|e| e.digest.clone()).collect();
            for id in ids {
                let d = self.details(&id)?;
                live.insert(id[7..].to_string());
                live.insert(d.manifest.config.digest[7..].to_string());
                for l in &d.manifest.layers {
                    live.insert(l.digest[7..].to_string());
                }
                live.extend(self.referrer_blob_hexes(&id));
            }
            // Attestation lists of images that are gone.
            if let Ok(rd) = fs::read_dir(self.home.join("referrers").join("sha256")) {
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    if is_hex64(&name) && !keep.iter().any(|e| e.digest[7..] == name) {
                        let _ = fs::remove_file(ent.path());
                    }
                }
            }
            let (mut blobs, mut bytes) = (0usize, 0u64);
            let dir = self.home.join("blobs").join("sha256");
            let rd = fs::read_dir(&dir).map_err(|e| store_error(format!("cannot read '{}': {e}", dir.display())))?;
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                let full = dir.join(&name);
                let Ok(st) = fs::metadata(&full) else { continue };
                if !st.is_file() {
                    continue;
                }
                let fresh = st.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_none_or(|age| age < Duration::from_secs(3600));
                // keep live blobs and fresh temp files
                if if is_hex64(&name) { live.contains(&name) } else { fresh } {
                    continue;
                }
                if fs::remove_file(&full).is_ok() {
                    blobs += 1;
                    bytes += st.len();
                }
            }
            Ok(PruneResult { manifests: dropped.len(), blobs, bytes })
        })
    }
}

fn lock_is_stale(lock: &Path) -> bool {
    let (Ok(text), Ok(st)) = (fs::read_to_string(lock), fs::metadata(lock)) else { return false };
    let age = st.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).unwrap_or_default();
    if age > Duration::from_secs(120) {
        return true;
    }
    match text.trim().parse::<i64>() {
        Ok(pid) if pid > 0 => !crate::servers::pid_alive(pid),
        _ => age > Duration::from_secs(2), // still being written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::exporter::tests::TmpDir;
    use crate::tree::make_file;

    fn mk(files: &[(&str, &str)]) -> Tree {
        Tree::from_files(files.iter().map(|(p, c)| (p.to_string(), make_file(c.as_bytes().to_vec(), "t", 0o644)))).unwrap()
    }
    fn blob_count(home: &Path) -> usize {
        fs::read_dir(home.join("blobs/sha256")).unwrap().count()
    }
    fn title(t: &str) -> OciOptions {
        OciOptions { title: Some(t.into()), ..Default::default() }
    }
    fn lr(s: &str) -> ImageRef {
        parse_local_ref(s).unwrap()
    }

    #[test]
    fn layout_add_tag_list() {
        let d = TmpDir::new("store");
        let home = d.path();
        let store = Store::open(home).unwrap();
        assert_eq!(fs::read(home.join("oci-layout")).unwrap(), OCI_LAYOUT_BYTES);
        let t = mk(&[("/index.html", "hi"), ("/a/b.txt", "x")]);
        let r = store.add_image(&t, &title("demo")).unwrap();
        assert!(r.created);
        assert!(crate::digest::is_integrity(&r.id));
        store.tag(&r.id, &lr("demo")).unwrap();
        store.tag(&r.id, &lr("demo:v1")).unwrap();
        let idx: serde_json::Value = serde_json::from_slice(&fs::read(home.join("index.json")).unwrap()).unwrap();
        assert_eq!(idx["mediaType"], "application/vnd.oci.image.index.v1+json");
        let mut names: Vec<String> =
            idx["manifests"].as_array().unwrap().iter().map(|m| m["annotations"][ann::REF_NAME].as_str().unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["demo:latest", "demo:v1"]);
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, r.id);
        let mut tags = list[0].tags.clone();
        tags.sort();
        assert_eq!(tags, ["demo:latest", "demo:v1"]);
        assert_eq!((list[0].files, list[0].size), (2, 3));
        assert_eq!(store.load_tree(&r.id).unwrap().digest(), t.digest());
    }

    #[test]
    fn stable_id_and_second_add_is_noop() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let t2020 = UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        let a = store.add_image_at(&mk(&[("/index.html", "same")]), &title("x"), t2020).unwrap();
        let b = store.add_image_at(&mk(&[("/index.html", "same")]), &title("x"), UNIX_EPOCH + Duration::from_secs(1_893_456_000)).unwrap();
        assert_eq!(a.id, b.id);
        assert!(!b.created);
        let l = store.list().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].created, "2020-01-01T00:00:00.000Z");
        let c = store.add_image(&mk(&[("/index.html", "different")]), &title("x")).unwrap();
        assert_ne!(c.id, a.id);
    }

    #[test]
    fn blobs_are_shared() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let t = mk(&[("/index.html", "shared layer")]);
        store.add_image(&t, &title("a")).unwrap();
        assert_eq!(blob_count(d.path()), 3);
        store.add_image(&t, &title("b")).unwrap();
        assert_eq!(blob_count(d.path()), 5);
    }

    #[test]
    fn tag_moves_rmi_and_prune() {
        let d = TmpDir::new("store");
        let home = d.path();
        let store = Store::open(home).unwrap();
        let a = store.add_image(&mk(&[("/i.html", "one")]), &OciOptions::default()).unwrap().id;
        let b = store.add_image(&mk(&[("/i.html", "two")]), &OciOptions::default()).unwrap().id;
        store.tag(&a, &lr("site:1")).unwrap();
        store.tag(&b, &lr("site:1")).unwrap();
        let list = store.list().unwrap();
        assert!(list.iter().find(|i| i.id == a).unwrap().tags.is_empty());
        assert_eq!(list.iter().find(|i| i.id == b).unwrap().tags, ["site:1"]);
        let before = blob_count(home);
        let p = store.prune().unwrap();
        assert_eq!((p.manifests, p.blobs), (1, 3));
        assert!(p.bytes > 0);
        assert_eq!(blob_count(home), before - 3);
        assert_eq!(store.list().unwrap().len(), 1);
        store.tag(&b, &lr("other")).unwrap();
        assert_eq!(store.remove_image("other", false).unwrap(), RemoveResult { untagged: vec!["other:latest".into()], deleted: None });
        store.tag(&b, &lr("other")).unwrap();
        assert_eq!(store.remove_image(&short_id(&b), false).unwrap_err().code, E_STORE);
        let mut r = store.remove_image(&short_id(&b), true).unwrap();
        r.untagged.sort();
        assert_eq!(r.untagged, ["other:latest", "site:1"]);
        assert_eq!(r.deleted.as_deref(), Some(b.as_str()));
        assert!(store.list().unwrap().is_empty());
        assert_eq!(store.prune().unwrap().blobs, 3);
        assert_eq!(blob_count(home), 0);
        assert_eq!(store.remove_image("site:1", false).unwrap_err().code, E_NOT_FOUND);
    }

    #[test]
    fn short_ids_refs_and_digests_resolve() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let id = store.add_image(&mk(&[("/x", "1")]), &OciOptions::default()).unwrap().id;
        store.tag(&id, &lr("x")).unwrap();
        let hex = &id[7..];
        assert_eq!(store.resolve(&hex[..12]).unwrap(), id);
        assert_eq!(store.resolve(&id).unwrap(), id);
        assert_eq!(store.resolve("x").unwrap(), id);
        assert_eq!(store.resolve("x:latest").unwrap(), id);
        assert_eq!(store.resolve(&format!("x@{id}")).unwrap(), id);
        assert_eq!(store.resolve(&hex[..11]).unwrap_err().code, E_NOT_FOUND);
        assert_eq!(store.resolve("nope").unwrap_err().code, E_NOT_FOUND);
        assert_eq!(store.resolve("registry.io/x").unwrap_err().code, E_NOT_FOUND);
    }

    #[test]
    fn corrupt_and_missing_blobs_are_detected() {
        for which in ["layer", "config", "manifest"] {
            let d = TmpDir::new("store");
            let store = Store::open(d.path()).unwrap();
            let id = store.add_image(&mk(&[("/index.html", &format!("content {which}"))]), &OciOptions::default()).unwrap().id;
            let det = store.details(&id).unwrap();
            let digest = match which {
                "layer" => det.manifest.layers[0].digest.clone(),
                "config" => det.manifest.config.digest.clone(),
                _ => id.clone(),
            };
            let file = store.blob_file(&digest).unwrap();
            let mut bytes = fs::read(&file).unwrap();
            let n = bytes.len();
            bytes[n - 3] ^= 0xff;
            fs::write(&file, bytes).unwrap();
            let e = if which == "layer" { store.load_tree(&id).unwrap_err() } else { store.details(&id).unwrap_err() };
            assert_eq!(e.code, E_STORE, "{which}");
            assert!(e.detail.contains("corrupt") || e.detail.contains("does not match"), "{which}: {}", e.detail);
        }
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let id = store.add_image(&mk(&[("/a", "b")]), &OciOptions::default()).unwrap().id;
        fs::remove_file(store.blob_file(&store.details(&id).unwrap().manifest.layers[0].digest).unwrap()).unwrap();
        let e = store.load_tree(&id).unwrap_err();
        assert_eq!(e.code, E_STORE);
        assert!(e.detail.contains("missing"));
    }

    #[test]
    fn layout_of_is_the_stored_image_and_diff() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let a = mk(&[("/index.html", "hi"), ("/gone.txt", "x"), ("/same.txt", "s")]);
        let b = mk(&[("/index.html", "changed"), ("/new.txt", "n"), ("/same.txt", "s")]);
        let id = store.add_image(&a, &OciOptions::default()).unwrap().id;
        let layout = store.layout_of(&id, Some("blog:1")).unwrap();
        let idx = decode_index(&layout.get("/index.json").unwrap().bytes).unwrap();
        assert_eq!(idx[0].digest, id);
        assert_eq!(idx[0].ref_name.as_deref(), Some("1"));
        assert!(layout.is_file(&format!("/blobs/sha256/{}", &id[7..])));
        assert_eq!(diff_trees(&a, &b), ["D /gone.txt", "C /index.html", "A /new.txt"]);
    }

    #[test]
    fn attestations_are_kept_with_their_image_saved_and_pruned_with_it() {
        use crate::export::attest::{attestation_referrer, CYCLONEDX, SLSA};
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let id = store.add_image(&mk(&[("/index.html", "hi")]), &OciOptions::default()).unwrap().id;
        let size = store.read_blob(&id, None).unwrap().len() as u64;
        let prov = attestation_referrer(&id, size, SLSA, serde_json::json!({ "v": 1 }));
        let sbom = attestation_referrer(&id, size, CYCLONEDX, serde_json::json!({}));
        store.put_referrers(&id, &[prov.clone(), sbom.clone()]).unwrap();
        // a newer provenance replaces the old one; the SBOM stays
        let prov2 = attestation_referrer(&id, size, SLSA, serde_json::json!({ "v": 2 }));
        store.put_referrers(&id, &[prov2.clone()]).unwrap();
        let list: Vec<String> = store.referrer_descriptors(&id).unwrap().into_iter().map(|r| r.digest).collect();
        assert_eq!(list, [sbom.descriptor.digest.clone(), prov2.descriptor.digest.clone()]);
        // save: listed after the image, with their blobs
        let layout = store.layout_of(&id, None).unwrap();
        let idx = decode_index(&layout.get("/index.json").unwrap().bytes).unwrap();
        assert_eq!(idx.iter().map(|e| e.digest.clone()).collect::<Vec<_>>(), [id.clone(), list[0].clone(), list[1].clone()]);
        for (dg, _) in &prov2.blobs {
            assert!(layout.is_file(&format!("/blobs/sha256/{}", &dg[7..])));
        }
        // tagged: prune keeps them; untagged: prune drops the list and the blobs
        store.tag(&id, &parse_local_ref("x:1").unwrap()).unwrap();
        store.prune().unwrap();
        assert_eq!(store.referrers(&id).unwrap().len(), 2);
        assert!(!store.has_blob(&prov.descriptor.digest, None), "the replaced provenance is garbage");
        store.remove_image("x:1", false).unwrap();
        store.prune().unwrap();
        assert!(store.referrer_descriptors(&id).unwrap().is_empty());
        assert!(!store.has_blob(&sbom.descriptor.digest, None));
    }

    #[test]
    fn import_blob_verifies_size_and_digest() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        let data = b"hello blob".to_vec();
        let digest = sha256(&data);
        assert_eq!(store.import_blob(&digest, 5, &mut &data[..], None).unwrap_err().code, E_TOO_LARGE);
        assert_eq!(store.import_blob(&digest, 20, &mut &data[..], None).unwrap_err().code, E_INTEGRITY);
        let other = sha256(b"x");
        assert_eq!(store.import_blob(&other, data.len() as u64, &mut &data[..], None).unwrap_err().code, E_INTEGRITY);
        store.import_blob(&digest, data.len() as u64, &mut &data[..], None).unwrap();
        assert!(store.has_blob(&digest, Some(data.len() as u64)));
        assert_eq!(store.read_blob(&digest, None).unwrap(), data);
        assert_eq!(blob_count(d.path()), 1, "no temp files left");
        // a manifest whose blobs are missing is refused
        let m = br#"{"schemaVersion":2,"config":{"mediaType":"x","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":1},"layers":[]}"#;
        assert_eq!(store.add_manifest(m, None).unwrap_err().code, E_STORE);
    }

    #[test]
    fn lock_is_exclusive_and_stale_locks_break() {
        let d = TmpDir::new("store");
        let store = Store::open(d.path()).unwrap();
        // A lock left by a dead process is broken.
        fs::write(d.path().join("lock"), "999999999").unwrap();
        store.with_lock(|| Ok(())).unwrap();
        assert!(!d.path().join("lock").exists());
        // Concurrent writers serialize.
        let home = d.path().to_path_buf();
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let home = home.clone();
                std::thread::spawn(move || {
                    let s = Store::open(&home).unwrap();
                    let id = s.add_image(&mk(&[("/i", &format!("{i}"))]), &OciOptions::default()).unwrap().id;
                    s.tag(&id, &lr(&format!("t:{i}"))).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.refs().unwrap().len(), 6);
    }

    #[test]
    fn store_home_resolution() {
        assert_eq!(resolve_store_home_from(Some("/x/y/../z")), PathBuf::from("/x/z"));
        assert!(resolve_store_home_from(None).ends_with(".sitefile"));
        assert_eq!(short_id("sha256:0123456789abcdef"), "0123456789ab");
    }
}
