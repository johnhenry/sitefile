//! OCI image-spec v1.1 writer and reader for Sitefile images (port of `src/oci.ts`).
//!
//! A Sitefile image has one site layer: a deterministic tar of the site's files
//! placed under a root directory (default /srv/www), gzip-compressed. With
//! `BASE`, the base image's layers come first and its config is kept.
//! Everything hashed (config, manifest, index) is canonical JSON, so the same
//! tree and options always give the same config and manifest bytes. (The
//! layer digest depends on the gzip encoder: flate2 here, zlib in Node, so
//! the two implementations agree on the config/manifest *shape* and on the
//! diff_id, but not on the layer and manifest digests.)
//!
//! Layout (the store is this layout; `save`/`EXPORT AS oci` write it standalone):
//!
//! ```text
//! oci-layout            {"imageLayoutVersion":"1.0.0"}
//! index.json            image index; entries carry org.opencontainers.image.ref.name
//! blobs/sha256/<hex>    manifest, config and layer blobs
//! ```

use super::archive_writers::{gzip_tar, iso_millis, write_tar, WriterOptions};
use crate::digest::sha256;
use crate::language::ast::OciCompression;
use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::paths::normalize_virtual;
use crate::refs::parse_ref;
use crate::resources::archive::extract_archive;
use crate::resources::types::ArchiveLimits;
use crate::tree::{make_file, Node, Tree};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

pub const DEFAULT_OCI_ROOT: &str = "/srv/www";

pub mod media {
    pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    pub const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
    pub const LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
    /// Uncompressed layer (`EXPORT ... COMPRESSION none`): the blob is the tar itself, digest = diff_id.
    pub const LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
    pub const INDEX: &str = "application/vnd.oci.image.index.v1+json";
    pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
    pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
    pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
    pub const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";
}

pub mod ann {
    pub const REF_NAME: &str = "org.opencontainers.image.ref.name";
    pub const TITLE: &str = "org.opencontainers.image.title";
    pub const SOURCE: &str = "org.opencontainers.image.source";
    pub const REVISION: &str = "org.opencontainers.image.revision";
    pub const ROOT: &str = "dev.sitefile.root";
    pub const FILES: &str = "dev.sitefile.files";
    pub const SIZE: &str = "dev.sitefile.size";
    pub const STORED: &str = "dev.sitefile.created";
    pub const BASE_NAME: &str = "org.opencontainers.image.base.name";
    pub const BASE_DIGEST: &str = "org.opencontainers.image.base.digest";
    pub const CONTAINERD_NAME: &str = "io.containerd.image.name";
}

const EPOCH_ISO: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Default)]
pub struct OciOptions {
    /// Where the files live inside the layer. Default /srv/www.
    pub root: Option<String>,
    /// `os/arch`, default linux/amd64. Metadata only.
    pub platform: Option<String>,
    /// `SOURCE_DATE_EPOCH` seconds; default the fixed epoch.
    pub epoch: Option<u64>,
    pub title: Option<String>,
    pub source: Option<String>,
    pub revision: Option<String>,
    /// Extra config labels (`--label key=value`); they override the defaults above.
    pub labels: BTreeMap<String, String>,
    /// Build on top of this image (`EXPORT ... AS oci BASE <image>`).
    pub base: Option<BaseImage>,
    /// Site layer encoding (default gzip). Base layers are kept as they are.
    pub compression: OciCompression,
}

/// A resolved base image: one platform manifest, its config and layers.
#[derive(Debug, Clone)]
pub struct BaseImage {
    /// Canonical reference as written (`docker.io/library/nginx:alpine`), without any digest.
    pub name: String,
    /// Digest of the platform manifest the layers come from.
    pub manifest_digest: String,
    pub layers: Vec<Descriptor>,
    /// The base's parsed config JSON.
    pub config: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    pub annotations: Option<BTreeMap<String, String>>,
}

impl Descriptor {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("mediaType".into(), Value::String(self.media_type.clone()));
        m.insert("digest".into(), Value::String(self.digest.clone()));
        m.insert("size".into(), Value::from(self.size));
        if let Some(a) = &self.annotations {
            m.insert("annotations".into(), str_map(a));
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone)]
pub struct OciImage {
    pub manifest_bytes: Vec<u8>,
    pub manifest_digest: String,
    pub config_bytes: Vec<u8>,
    pub config_digest: String,
    pub layer_bytes: Vec<u8>,
    pub layer_digest: String,
    /// sha256 of the uncompressed tar.
    pub diff_id: String,
    pub root: String,
    /// Layers below the site layer (from BASE), normalized to OCI media types.
    pub base_layers: Vec<Descriptor>,
}

/// The blobs a standalone layout needs (borrowed from an [`OciImage`] or the store).
#[derive(Debug, Clone, Copy)]
pub struct LayoutParts<'a> {
    pub manifest_bytes: &'a [u8],
    pub manifest_digest: &'a str,
    pub config_bytes: &'a [u8],
    pub config_digest: &'a str,
    pub layer_bytes: &'a [u8],
    pub layer_digest: &'a str,
}

impl OciImage {
    pub fn parts(&self) -> LayoutParts<'_> {
        LayoutParts {
            manifest_bytes: &self.manifest_bytes,
            manifest_digest: &self.manifest_digest,
            config_bytes: &self.config_bytes,
            config_digest: &self.config_digest,
            layer_bytes: &self.layer_bytes,
            layer_digest: &self.layer_digest,
        }
    }
}

// ------------------------------------------------------------ canonical JSON

/// JSON with sorted keys (UTF-16 order) and no whitespace, exactly as
/// TypeScript's `canonicalJson` (see [`crate::values::js_json_stringify`]).
pub fn canonical_json(v: &Value) -> String {
    crate::values::js_json_stringify(v, true)
}

/// JavaScript's `Number.prototype.toString` for a finite double.
fn js_number(f: f64) -> String {
    crate::values::js_json_stringify(&crate::values::js_number_json(f), false)
}

fn str_map(m: &BTreeMap<String, String>) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
}

// ------------------------------------------------------------ options

pub fn parse_platform(p: Option<&str>) -> Result<(String, String)> {
    let Some(p) = p else { return Ok(("linux".into(), "amd64".into())) };
    let part_ok = |s: &str| {
        let b = s.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
    };
    match p.split_once('/') {
        Some((os, arch)) if part_ok(os) && part_ok(arch) => Ok((os.into(), arch.into())),
        _ => Err(SitefileError::plain(E_USAGE, format!("platform must be os/arch (e.g. linux/amd64), got '{p}'"))),
    }
}

/// Validate an in-image root path (absolute, no `..`); returns the normalized form.
pub fn normalize_oci_root(raw: Option<&str>) -> Result<String> {
    Ok(normalize_virtual(raw.unwrap_or(DEFAULT_OCI_ROOT), None, "OCI root")?.path)
}

/// `new Date(epoch * 1000).toISOString()` without milliseconds, or the fixed epoch.
pub fn iso_of(epoch: Option<u64>) -> String {
    match epoch {
        None => EPOCH_ISO.to_string(),
        Some(e) => {
            let s = iso_millis((e as i64).saturating_mul(1000));
            match s.strip_suffix(".000Z") {
                Some(t) => format!("{t}Z"),
                None => s,
            }
        }
    }
}

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// Build the image for `tree` (the site's files, rooted at `/`).
pub fn build_oci_image(tree: &Tree, options: &OciOptions) -> Result<OciImage> {
    let root = normalize_oci_root(options.root.as_deref())?;
    let (os, arch) = parse_platform(options.platform.as_deref())?;
    let layer_tree = Tree::from_files(tree.mounted_at(&root))?;
    let segs: Vec<&str> = if root == "/" { vec![] } else { root[1..].split('/').collect() };
    let dirs: Vec<String> = (0..segs.len()).map(|i| segs[..=i].join("/")).collect();
    let tar = write_tar(&layer_tree, &WriterOptions { epoch: Some(options.epoch.unwrap_or(0)), dirs })?;
    let uncompressed = options.compression == OciCompression::None;
    let diff_id = sha256(&tar);
    let (layer_bytes, layer_digest) = if uncompressed {
        (tar, diff_id.clone())
    } else {
        let gz = gzip_tar(&tar);
        let d = sha256(&gz);
        (gz, d)
    };
    let size: u64 = tree.entries().map(|(_, f)| f.bytes.len() as u64).sum();

    let empty = Map::new();
    let base = options.base.as_ref();
    let base_cfg = base.map(|b| &b.config).unwrap_or(&empty);
    let base_inner = obj(base_cfg.get("config")).unwrap_or(&empty);
    let mut labels: Map<String, Value> = Map::new();
    if let Some(l) = obj(base_inner.get("Labels")) {
        for (k, v) in l {
            if v.is_string() {
                labels.insert(k.clone(), v.clone());
            }
        }
    }
    let set = |m: &mut Map<String, Value>, k: &str, v: &str| {
        m.insert(k.to_string(), Value::String(v.to_string()));
    };
    set(&mut labels, ann::ROOT, &root);
    for (k, v) in [(ann::TITLE, &options.title), (ann::SOURCE, &options.source), (ann::REVISION, &options.revision)] {
        if let Some(v) = v.as_deref().filter(|s| !s.is_empty()) {
            set(&mut labels, k, v);
        }
    }
    if let Some(b) = base {
        set(&mut labels, ann::BASE_NAME, &b.name);
        set(&mut labels, ann::BASE_DIGEST, &b.manifest_digest);
    }
    for (k, v) in &options.labels {
        set(&mut labels, k, v);
    }
    // created: SOURCE_DATE_EPOCH when set; else the base's own timestamp, or the fixed epoch.
    let base_created = base_cfg.get("created").and_then(Value::as_str);
    let created = match (options.epoch, base_created) {
        (None, Some(c)) => c.to_string(),
        _ => iso_of(options.epoch),
    };
    let base_rootfs = obj(base_cfg.get("rootfs")).unwrap_or(&empty);
    let base_diff_ids: Vec<Value> = base_rootfs.get("diff_ids").and_then(Value::as_array).cloned().unwrap_or_default();
    let base_history: Vec<Value> = base_cfg.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
    let base_layers: Vec<Descriptor> = base.map(|b| b.layers.iter().map(normalize_layer).collect()).unwrap_or_default();
    if let Some(b) = base {
        if base_diff_ids.len() != base_layers.len() {
            return Err(SitefileError::plain(E_STORE, format!("base image {}: config diff_ids do not match its layers", b.name)));
        }
    }

    let mut cfg = base_cfg.clone();
    let pick = |key: &str, dflt: &str| -> Value {
        match (base, base_cfg.get(key).and_then(Value::as_str)) {
            (Some(_), Some(s)) => Value::String(s.to_string()),
            _ => Value::String(dflt.to_string()),
        }
    };
    cfg.insert("architecture".into(), pick("architecture", &arch));
    cfg.insert("os".into(), pick("os", &os));
    cfg.insert("created".into(), Value::String(created));
    let mut inner = base_inner.clone();
    inner.insert("Labels".into(), Value::Object(labels));
    cfg.insert("config".into(), Value::Object(inner));
    let mut diff_ids = base_diff_ids;
    diff_ids.push(Value::String(diff_id.clone()));
    cfg.insert("rootfs".into(), serde_json::json!({ "type": "layers", "diff_ids": diff_ids }));
    let mut history = base_history;
    history.push(serde_json::json!({
        "created": iso_of(options.epoch),
        "created_by": "sitefile",
        "comment": "static site files",
        "author": "sitefile",
    }));
    cfg.insert("history".into(), Value::Array(history));
    let config_bytes = canonical_json(&Value::Object(cfg)).into_bytes();
    let config_digest = sha256(&config_bytes);

    let mut annotations = BTreeMap::new();
    annotations.insert(ann::FILES.to_string(), tree.len().to_string());
    annotations.insert(ann::SIZE.to_string(), size.to_string());
    if let Some(t) = options.title.as_deref().filter(|s| !s.is_empty()) {
        annotations.insert(ann::TITLE.to_string(), t.to_string());
    }
    if let Some(b) = base {
        annotations.insert(ann::BASE_NAME.to_string(), b.name.clone());
        annotations.insert(ann::BASE_DIGEST.to_string(), b.manifest_digest.clone());
    }
    let mut layers: Vec<Value> = base_layers.iter().map(Descriptor::to_json).collect();
    let layer_type = if uncompressed { media::LAYER_TAR } else { media::LAYER };
    layers.push(serde_json::json!({ "mediaType": layer_type, "digest": layer_digest, "size": layer_bytes.len() }));
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": media::MANIFEST,
        "config": { "mediaType": media::CONFIG, "digest": config_digest, "size": config_bytes.len() },
        "layers": layers,
        "annotations": str_map(&annotations),
    });
    let manifest_bytes = canonical_json(&manifest).into_bytes();
    Ok(OciImage {
        manifest_digest: sha256(&manifest_bytes),
        manifest_bytes,
        config_bytes,
        config_digest,
        layer_bytes,
        layer_digest,
        diff_id,
        root,
        base_layers,
    })
}

/// Docker layer media types become their OCI equivalents (same bytes, same digest).
fn normalize_layer(d: &Descriptor) -> Descriptor {
    let media_type = if d.media_type == media::DOCKER_LAYER { media::LAYER.to_string() } else { d.media_type.clone() };
    Descriptor { media_type, digest: d.digest.clone(), size: d.size, annotations: None }
}

// ------------------------------------------------------------ index

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub digest: String,
    pub size: u64,
    pub ref_name: Option<String>,
    /// When the image was first stored locally (store bookkeeping; not part of any digest).
    pub stored: Option<String>,
    /// Extra annotations written verbatim (exports only).
    pub extra: BTreeMap<String, String>,
}

/// Index annotations for an exported image named `ref_name`, in the convention
/// `docker save` uses: `io.containerd.image.name` is the full normalized
/// reference and `org.opencontainers.image.ref.name` the tag.
pub fn export_name_annotations(ref_name: &str) -> Result<BTreeMap<String, String>> {
    let r = parse_ref(ref_name)?;
    let tag = r.tag.clone().unwrap_or_else(|| "latest".into());
    let full = if r.registry.is_some() || r.name.contains('/') { r.name.clone() } else { format!("library/{}", r.name) };
    let host = r.registry.clone().unwrap_or_else(|| "docker.io".into());
    let mut m = BTreeMap::new();
    m.insert(ann::CONTAINERD_NAME.to_string(), format!("{host}/{full}:{tag}"));
    m.insert(ann::REF_NAME.to_string(), tag);
    Ok(m)
}

pub fn encode_index(entries: &[IndexEntry]) -> Vec<u8> {
    let manifests: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut a = BTreeMap::new();
            if let Some(r) = e.ref_name.as_deref().filter(|s| !s.is_empty()) {
                a.insert(ann::REF_NAME.to_string(), r.to_string());
            }
            if let Some(s) = e.stored.as_deref().filter(|s| !s.is_empty()) {
                a.insert(ann::STORED.to_string(), s.to_string());
            }
            for (k, v) in &e.extra {
                a.insert(k.clone(), v.clone());
            }
            let mut m = Map::new();
            m.insert("mediaType".into(), Value::String(media::MANIFEST.into()));
            m.insert("digest".into(), Value::String(e.digest.clone()));
            m.insert("size".into(), Value::from(e.size));
            if !a.is_empty() {
                m.insert("annotations".into(), str_map(&a));
            }
            Value::Object(m)
        })
        .collect();
    canonical_json(&serde_json::json!({ "schemaVersion": 2, "mediaType": media::INDEX, "manifests": manifests })).into_bytes()
}

pub const OCI_LAYOUT_BYTES: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

fn size_of(v: Option<&Value>) -> Option<u64> {
    let n = v?.as_number()?;
    n.as_u64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= 9_007_199_254_740_991.0).map(|f| f as u64))
}

pub fn decode_index(bytes: &[u8]) -> std::result::Result<Vec<IndexEntry>, String> {
    let j: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let manifests = match (j.get("schemaVersion").and_then(Value::as_u64), j.get("manifests").and_then(Value::as_array)) {
        (Some(2), Some(m)) => m,
        _ => return Err("not an OCI image index".into()),
    };
    manifests
        .iter()
        .map(|m| {
            let digest = m.get("digest").and_then(Value::as_str).filter(|d| crate::digest::is_integrity(d));
            let (Some(digest), Some(size)) = (digest, size_of(m.get("size"))) else { return Err("bad index entry".to_string()) };
            let a = m.get("annotations");
            let s = |k: &str| a.and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
            Ok(IndexEntry { digest: digest.into(), size, ref_name: s(ann::REF_NAME), stored: s(ann::STORED), extra: BTreeMap::new() })
        })
        .collect()
}

pub fn blob_path_of(digest: &str) -> Result<String> {
    if !crate::digest::is_integrity(digest) {
        return Err(SitefileError::plain(E_STORE, format!("invalid digest '{digest}'")));
    }
    Ok(format!("/blobs/sha256/{}", &digest[7..]))
}

/// A standalone OCI image layout holding one image, as a Tree (write it as a directory or tar it).
/// `base_blobs` are the blobs of the layers below the site layer (digest, bytes).
pub fn layout_tree(img: LayoutParts<'_>, ref_name: Option<&str>, base_blobs: &[(String, Vec<u8>)]) -> Result<Tree> {
    let extra = match ref_name.filter(|s| !s.is_empty()) {
        Some(r) => export_name_annotations(r)?,
        None => BTreeMap::new(),
    };
    let index = encode_index(&[IndexEntry { digest: img.manifest_digest.into(), size: img.manifest_bytes.len() as u64, extra, ..Default::default() }]);
    let mut files: Vec<(String, Node)> = vec![
        ("/oci-layout".into(), make_file(OCI_LAYOUT_BYTES.to_vec(), "oci", 0o644)),
        ("/index.json".into(), make_file(index, "oci", 0o644)),
        (blob_path_of(img.manifest_digest)?, make_file(img.manifest_bytes.to_vec(), "oci", 0o644)),
        (blob_path_of(img.config_digest)?, make_file(img.config_bytes.to_vec(), "oci", 0o644)),
        (blob_path_of(img.layer_digest)?, make_file(img.layer_bytes.to_vec(), "oci", 0o644)),
    ];
    for (d, b) in base_blobs {
        files.push((blob_path_of(d)?, make_file(b.clone(), "oci", 0o644)));
    }
    Tree::from_files(files)
}

/// Deterministic tar of [`layout_tree`]: an "OCI archive" (`.oci.tar`).
pub fn oci_archive(img: LayoutParts<'_>, ref_name: Option<&str>, epoch: Option<u64>, base_blobs: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    write_tar(&layout_tree(img, ref_name, base_blobs)?, &WriterOptions { epoch: Some(epoch.unwrap_or(0)), dirs: vec![] })
}

// ------------------------------------------------------------ parsing

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedManifest {
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    pub annotations: BTreeMap<String, String>,
}

fn desc(d: Option<&Value>, what: &str) -> std::result::Result<Descriptor, String> {
    let bad = || format!("bad {what} descriptor");
    let o = d.and_then(Value::as_object).ok_or_else(bad)?;
    let media_type = o.get("mediaType").and_then(Value::as_str).ok_or_else(bad)?;
    let digest = o.get("digest").and_then(Value::as_str).filter(|d| crate::digest::is_integrity(d)).ok_or_else(bad)?;
    let size = size_of(o.get("size")).ok_or_else(bad)?;
    Ok(Descriptor { media_type: media_type.into(), digest: digest.into(), size, annotations: None })
}

fn string_values(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(o) = v.and_then(Value::as_object) {
        for (k, v) in o {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

pub fn parse_manifest(bytes: &[u8]) -> std::result::Result<ParsedManifest, String> {
    let m: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mt = m.get("mediaType");
    let mt_ok = match mt {
        None => true,
        Some(Value::String(s)) => s == media::MANIFEST || s == media::DOCKER_MANIFEST,
        Some(_) => false,
    };
    let layers = m.get("layers").and_then(Value::as_array);
    let (true, Some(2), Some(layers)) = (mt_ok, m.get("schemaVersion").and_then(Value::as_u64), layers) else {
        return Err("not an image manifest (OCI or Docker v2)".into());
    };
    Ok(ParsedManifest {
        config: desc(m.get("config"), "config")?,
        layers: layers.iter().map(|l| desc(Some(l), "layer")).collect::<std::result::Result<_, _>>()?,
        annotations: string_values(m.get("annotations")),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConfig {
    pub os: String,
    pub architecture: String,
    pub created: String,
    pub labels: BTreeMap<String, String>,
    pub diff_ids: Vec<String>,
}

/// `String(x ?? '')`
fn js_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map(js_number).unwrap_or_default(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(_)) => String::new(),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

pub fn parse_config(bytes: &[u8]) -> std::result::Result<ParsedConfig, String> {
    let c: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let rootfs = c.get("rootfs");
    let ids = rootfs.and_then(|r| r.get("diff_ids")).and_then(Value::as_array);
    let typ = rootfs.and_then(|r| r.get("type")).and_then(Value::as_str);
    let ids: Vec<String> = match (typ, ids) {
        (Some("layers"), Some(ids)) if ids.iter().all(|d| d.as_str().is_some_and(crate::digest::is_integrity)) => {
            ids.iter().filter_map(|d| d.as_str().map(str::to_string)).collect()
        }
        _ => return Err("bad rootfs".into()),
    };
    Ok(ParsedConfig {
        os: js_string(c.get("os")),
        architecture: js_string(c.get("architecture")),
        created: js_string(c.get("created")),
        labels: string_values(c.get("config").and_then(|x| x.get("Labels"))),
        diff_ids: ids,
    })
}

/// Index of the layer holding the site's files: the only layer, or the last
/// one of an image that carries the `dev.sitefile.root` label (built with
/// BASE). `None` when the image is not a Sitefile image.
pub fn site_layer_index(m: &ParsedManifest, cfg: &ParsedConfig) -> Option<usize> {
    match m.layers.len() {
        1 => Some(0),
        n if n > 1 && cfg.labels.contains_key(ann::ROOT) => Some(n - 1),
        _ => None,
    }
}

const MAX_LAYER_ENTRIES: usize = 1_000_000;
const MAX_LAYER_BYTES: u64 = 8 * (1 << 30);

/// Decode a site image's files, verifying the layer digest and diff_id.
/// `layer_bytes` are the compressed blob bytes. Returns the tree under the image's root.
pub fn extract_image_files(m: &ParsedManifest, cfg: &ParsedConfig, layer_bytes: &[u8], origin: &str) -> Result<Tree> {
    let idx = site_layer_index(m, cfg);
    let layer = idx.and_then(|i| m.layers.get(i));
    let (Some(idx), Some(layer)) = (idx, layer.filter(|l| l.media_type == media::LAYER || l.media_type == media::LAYER_TAR)) else {
        return Err(SitefileError::plain(
            E_STORE,
            format!("{origin}: not a Sitefile image (only single-layer tar or tar+gzip images, or images built with BASE, hold site files)"),
        ));
    };
    if sha256(layer_bytes) != layer.digest || layer_bytes.len() as u64 != layer.size {
        return Err(SitefileError::plain(E_STORE, format!("{origin}: layer blob does not match its digest")));
    }
    let limit = MAX_LAYER_BYTES + (MAX_LAYER_ENTRIES as u64 + 2) * 4 * 512;
    let mut tar = Vec::new();
    if layer.media_type == media::LAYER_TAR {
        tar.extend_from_slice(&layer_bytes[..layer_bytes.len().min(limit as usize + 1)]);
    } else {
        let mut dec = flate2::read::GzDecoder::new(layer_bytes).take(limit + 1);
        if dec.read_to_end(&mut tar).is_err() {
            return Err(SitefileError::plain(E_STORE, format!("{origin}: layer is not valid gzip")));
        }
    }
    if tar.len() as u64 > limit {
        return Err(SitefileError::plain(E_ARCHIVE_LIMIT, format!("{origin}: archive expands beyond {MAX_LAYER_BYTES} bytes")));
    }
    if cfg.diff_ids.get(idx).map(String::as_str) != Some(sha256(&tar).as_str()) {
        return Err(SitefileError::plain(E_STORE, format!("{origin}: layer content does not match the config diff_id")));
    }
    let root = cfg.labels.get(ann::ROOT).map(String::as_str).unwrap_or("/");
    let limits = ArchiveLimits { max_entries: MAX_LAYER_ENTRIES as u64, max_total_bytes: MAX_LAYER_BYTES, max_ratio: u64::MAX };
    let all = extract_archive(&tar, origin, &limits, None)?;
    Ok(all.subtree(&normalize_virtual(root, None, "path")?.path))
}

// ------------------------------------------------------------ metadata

/// Label values for an image built from `context_dir`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageMeta {
    pub title: Option<String>,
    pub source: Option<String>,
    pub revision: Option<String>,
    pub labels: BTreeMap<String, String>,
}

/// The title (directory name), extra `--label`s and, only with `git`,
/// source/revision from a git checkout (opt-in: they change the image ID with
/// every commit, and finding the checkout reads outside the build context).
pub fn image_meta(context_dir: &Path, git: bool, labels: BTreeMap<String, String>) -> ImageMeta {
    let info = if git { git_info(context_dir) } else { GitInfo::default() };
    let resolved = super::exporter::resolve_path(context_dir);
    let title = resolved.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    ImageMeta { title: Some(title), source: info.source, revision: info.revision, labels }
}

/// `^[A-Za-z0-9][A-Za-z0-9._/-]{0,254}$`
fn is_label_key(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty() && b.len() <= 255 && b[0].is_ascii_alphanumeric() && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-'))
}

/// Parse `--label key=value` flags.
pub fn parse_labels(flags: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for f in flags {
        match f.split_once('=') {
            Some((k, v)) if is_label_key(k) => {
                out.insert(k.to_string(), v.to_string());
            }
            _ => return Err(SitefileError::plain(E_USAGE, format!("--label expects key=value with a key like org.example.team, got '{f}'"))),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub source: Option<String>,
    pub revision: Option<String>,
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Best-effort `org.opencontainers.image.{source,revision}` from a git
/// checkout (never runs git; credentials in URLs are dropped).
pub fn git_info(context_dir: &Path) -> GitInfo {
    let mut out = GitInfo::default();
    let mut dir = super::exporter::resolve_path(context_dir);
    let mut git_dir = None;
    for _ in 0..8 {
        let g = dir.join(".git");
        if std::fs::read_to_string(g.join("HEAD")).is_ok() {
            git_dir = Some(g);
            break;
        }
        match dir.parent() {
            Some(up) => dir = up.to_path_buf(),
            None => break,
        }
    }
    let Some(git_dir) = git_dir else { return out };
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        let head = head.trim();
        let mut rev = head.to_string();
        if let Some(r) = head.strip_prefix("ref: refs/") {
            let r = format!("refs/{r}");
            if r.bytes().all(|c| is_word(c) || matches!(c, b'.' | b'/' | b'-')) {
                rev = match std::fs::read_to_string(git_dir.join(&r)) {
                    Ok(t) => t.trim().to_string(),
                    Err(_) => {
                        let packed = std::fs::read_to_string(git_dir.join("packed-refs")).unwrap_or_default();
                        let suffix = format!(" {r}");
                        packed.split('\n').find(|l| l.ends_with(&suffix)).and_then(|l| l.split(' ').next()).unwrap_or("").to_string()
                    }
                };
            }
        }
        if (40..=64).contains(&rev.len()) && rev.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
            out.revision = Some(rev);
        }
    }
    if let Ok(cfg) = std::fs::read_to_string(git_dir.join("config")) {
        if let Some(url) = origin_url(&cfg) {
            out.source = clean_remote(&url);
        }
    }
    out
}

/// `\[remote "origin"\][^[]*?\burl\s*=\s*(\S+)`
fn origin_url(cfg: &str) -> Option<String> {
    let start = cfg.find("[remote \"origin\"]")? + "[remote \"origin\"]".len();
    let section = &cfg[start..];
    let section = &section[..section.find('[').unwrap_or(section.len())];
    let b = section.as_bytes();
    let mut i = 0;
    while let Some(j) = section[i..].find("url") {
        let at = i + j;
        let boundary = at == 0 || !is_word(b[at - 1]);
        let mut p = at + 3;
        while p < b.len() && b[p].is_ascii_whitespace() {
            p += 1;
        }
        if boundary && p < b.len() && b[p] == b'=' {
            p += 1;
            while p < b.len() && b[p].is_ascii_whitespace() {
                p += 1;
            }
            let end = section[p..].find(|c: char| c.is_whitespace()).map(|e| p + e).unwrap_or(b.len());
            if end > p {
                return Some(section[p..end].to_string());
            }
        }
        i = at + 1;
    }
    None
}

fn clean_remote(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if !scheme.is_empty() && scheme.bytes().all(|c| c.is_ascii_lowercase() || c == b'+') {
        let rest = &url[scheme_end + 3..];
        let rest = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
        let auth_end = rest.find('/').unwrap_or(rest.len());
        let (auth, path) = rest.split_at(auth_end);
        let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth).to_ascii_lowercase();
        if host.is_empty() && scheme != "file" {
            return None;
        }
        let special = matches!(scheme, "http" | "https" | "ws" | "wss" | "ftp" | "file");
        let path = if path.is_empty() && special { "/" } else { path };
        return Some(format!("{scheme}://{host}{path}"));
    }
    None
}

/// scp-style remotes (`user@host:path`) are kept as-is (no secret).
fn scp_style(url: &str) -> bool {
    let ok = |s: &str, extra: &[u8]| !s.is_empty() && s.bytes().all(|c| is_word(c) || extra.contains(&c));
    match url.split_once('@') {
        Some((user, rest)) => match rest.split_once(':') {
            Some((host, path)) => ok(user, b".-") && ok(host, b".-") && ok(path, b"./-"),
            None => false,
        },
        None => false,
    }
}

#[doc(hidden)]
pub fn clean_remote_url(url: &str) -> Option<String> {
    if url.contains("://") {
        clean_remote(url)
    } else if scp_style(url) {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::archive_writers::tests::gunzip;
    use crate::export::exporter::tests::TmpDir;

    fn tree() -> Tree {
        Tree::from_files([
            ("/index.html".to_string(), make_file(b"<h1>hi</h1>".to_vec(), "t", 0o644)),
            ("/css/site.css".to_string(), make_file(b"a{b:c}".to_vec(), "t", 0o644)),
            ("/bin/run.sh".to_string(), make_file(b"#!/bin/sh\n".to_vec(), "t", 0o755)),
        ])
        .unwrap()
    }

    /// Layers are read with the resource importer's tar reader (as oci.ts does).
    fn read_tar(b: &[u8], origin: &str, max_entries: u64, max_total: u64) -> Result<Tree> {
        extract_archive(b, origin, &ArchiveLimits { max_entries, max_total_bytes: max_total, max_ratio: u64::MAX }, None)
    }

    fn tar_number(b: &[u8]) -> Option<u64> {
        let s: String = b.iter().take_while(|&&c| c != 0 && c != b' ').map(|&c| c as char).collect();
        u64::from_str_radix(s.trim(), 8).ok()
    }

    fn c_string(b: &[u8]) -> String {
        String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]).into_owned()
    }

    fn tar_names(tar: &[u8]) -> Vec<(String, u8)> {
        let mut out = vec![];
        let mut o = 0;
        while o + 512 <= tar.len() && tar[o..o + 512].iter().any(|&b| b != 0) {
            let h = &tar[o..o + 512];
            let size = tar_number(&h[124..136]).unwrap() as usize;
            out.push((c_string(&h[0..100]), h[156]));
            o += 512 + size.div_ceil(512) * 512;
        }
        out
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v = serde_json::json!({ "b": [1, { "z": 1, "a": 2 }], "a": "x" });
        assert_eq!(canonical_json(&v), r#"{"a":"x","b":[1,{"a":2,"z":1}]}"#);
        assert_eq!(canonical_json(&serde_json::json!({"\u{e000}": 1, "\u{10000}": 2})), "{\"\u{10000}\":2,\"\u{e000}\":1}");
        assert_eq!(canonical_json(&serde_json::json!("a\"\n\u{1f}é")), "\"a\\\"\\n\\u001fé\"");
        for (f, s) in [(1.5, "1.5"), (1.0, "1"), (1e21, "1e+21"), (0.000001, "0.000001"), (123456.789, "123456.789")] {
            assert_eq!(js_number(f), s);
        }
    }

    #[test]
    fn image_parses_back() {
        let opts = OciOptions {
            title: Some("demo".into()),
            revision: Some("a".repeat(40)),
            source: Some("https://example.com/r.git".into()),
            ..Default::default()
        };
        let img = build_oci_image(&tree(), &opts).unwrap();
        let layout = layout_tree(img.parts(), Some("demo:latest"), &[]).unwrap();
        assert_eq!(&*layout.get("/oci-layout").unwrap().bytes, OCI_LAYOUT_BYTES);
        let idx_bytes = layout.get("/index.json").unwrap().bytes.clone();
        let entry = decode_index(&idx_bytes).unwrap().remove(0);
        assert_eq!(entry.ref_name.as_deref(), Some("latest"));
        let j: Value = serde_json::from_slice(&idx_bytes).unwrap();
        assert_eq!(j["manifests"][0]["annotations"][ann::CONTAINERD_NAME], "docker.io/library/demo:latest");
        for (given, full, tag) in [
            ("blog:1", "docker.io/library/blog:1", "1"),
            ("team/blog", "docker.io/team/blog:latest", "latest"),
            ("localhost:5000/blog:2", "localhost:5000/blog:2", "2"),
            ("docker.io/library/nginx:alpine", "docker.io/library/nginx:alpine", "alpine"),
        ] {
            let a = export_name_annotations(given).unwrap();
            assert_eq!(a[ann::CONTAINERD_NAME], full);
            assert_eq!(a[ann::REF_NAME], tag);
        }
        let blob = |d: &str| layout.get(&format!("/blobs/sha256/{}", &d[7..])).unwrap().bytes.clone();
        for (p, n) in layout.entries() {
            if p.starts_with("/blobs/") {
                assert_eq!(format!("sha256:{}", p.rsplit('/').next().unwrap()), n.digest);
            }
        }
        let mb = blob(&entry.digest);
        assert_eq!(sha256(&mb), entry.digest);
        assert_eq!(mb.len() as u64, entry.size);
        let m = parse_manifest(&mb).unwrap();
        let mj: Value = serde_json::from_slice(&mb).unwrap();
        assert_eq!(mj["mediaType"], media::MANIFEST);
        assert_eq!(mj["schemaVersion"], 2);
        assert_eq!(m.config.media_type, media::CONFIG);
        let cb = blob(&m.config.digest);
        assert_eq!(sha256(&cb), m.config.digest);
        let cfg = parse_config(&cb).unwrap();
        assert_eq!((cfg.os.as_str(), cfg.architecture.as_str(), cfg.created.as_str()), ("linux", "amd64", "1970-01-01T00:00:00Z"));
        assert_eq!(cfg.labels[ann::ROOT], "/srv/www");
        assert_eq!(cfg.labels[ann::TITLE], "demo");
        assert_eq!(cfg.labels[ann::REVISION], "a".repeat(40));
        assert_eq!(cfg.labels[ann::SOURCE], "https://example.com/r.git");
        assert_eq!(m.layers.len(), 1);
        assert_eq!(m.layers[0].media_type, media::LAYER);
        let gz = blob(&m.layers[0].digest);
        let tar = gunzip(&gz);
        assert_eq!(sha256(&tar), cfg.diff_ids[0]);
        let names = tar_names(&tar);
        assert_eq!(names[..2], [("srv/".to_string(), b'5'), ("srv/www/".to_string(), b'5')]);
        assert!(names.contains(&("srv/www/index.html".to_string(), b'0')));
        let all = read_tar(&tar, "test", 100, 1 << 20).unwrap();
        assert_eq!(all.subtree("/srv/www").digest(), tree().digest());
        assert_eq!(all.get("/srv/www/bin/run.sh").unwrap().mode, 0o755);
        assert_eq!(extract_image_files(&m, &cfg, &gz, "img").unwrap().digest(), tree().digest());
    }

    #[test]
    fn reproducible_and_options_change_it() {
        let a = build_oci_image(&tree(), &OciOptions::default()).unwrap();
        let mut rev: Vec<(String, Node)> = tree().entries().map(|(p, n)| (p.clone(), n.clone())).collect();
        rev.reverse();
        let b = build_oci_image(&Tree::from_files(rev).unwrap(), &OciOptions::default()).unwrap();
        assert_eq!(a.manifest_digest, b.manifest_digest);
        assert_ne!(build_oci_image(&tree(), &OciOptions { root: Some("/var/www".into()), ..Default::default() }).unwrap().manifest_digest, a.manifest_digest);
        let e = build_oci_image(&tree(), &OciOptions { epoch: Some(1_700_000_000), ..Default::default() }).unwrap();
        assert_eq!(parse_config(&e.config_bytes).unwrap().created, "2023-11-14T22:13:20Z");
        let arm = build_oci_image(&tree(), &OciOptions { platform: Some("linux/arm64".into()), ..Default::default() }).unwrap();
        assert_eq!(parse_config(&arm.config_bytes).unwrap().architecture, "arm64");
        assert_eq!(build_oci_image(&tree(), &OciOptions { platform: Some("nonsense".into()), ..Default::default() }).unwrap_err().code, E_USAGE);
        assert_eq!(build_oci_image(&tree(), &OciOptions { root: Some("relative".into()), ..Default::default() }).unwrap_err().code, E_PATH);
        let slash = build_oci_image(&tree(), &OciOptions { root: Some("/".into()), ..Default::default() }).unwrap();
        assert_eq!(tar_names(&gunzip(&slash.layer_bytes))[0].0, "bin/run.sh");
        assert_eq!(oci_archive(a.parts(), None, None, &[]).unwrap(), oci_archive(b.parts(), None, None, &[]).unwrap());
    }

    #[test]
    fn labels_validate_and_override() {
        let l = parse_labels(&["org.example.team=web".into(), "empty=".into()]).unwrap();
        assert_eq!(l["org.example.team"], "web");
        assert_eq!(l["empty"], "");
        for bad in ["novalue", "=x", "has space=x"] {
            assert_eq!(parse_labels(&[bad.to_string()]).unwrap_err().code, E_USAGE, "{bad}");
        }
        let t = Tree::from_files([("/index.html".to_string(), make_file(b"hi".to_vec(), "t", 0o644))]).unwrap();
        let mut labels = BTreeMap::new();
        labels.insert("org.example.team".to_string(), "web".to_string());
        labels.insert(ann::TITLE.to_string(), "override".to_string());
        let img = build_oci_image(&t, &OciOptions { title: Some("demo".into()), labels, ..Default::default() }).unwrap();
        let cfg = parse_config(&img.config_bytes).unwrap();
        assert_eq!(cfg.labels["org.example.team"], "web");
        assert_eq!(cfg.labels[ann::TITLE], "override");
    }

    #[test]
    fn git_labels_are_opt_in_and_drop_credentials() {
        let d = TmpDir::new("oci");
        let dir = d.path();
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), format!("{}\n", "b".repeat(40))).unwrap();
        std::fs::write(dir.join(".git/config"), "[remote \"origin\"]\n\turl = https://alice:ghp_SECRET@github.com/acme/site.git?x=1\n").unwrap();
        let off = image_meta(dir, false, BTreeMap::new());
        assert_eq!((off.source, off.revision), (None, None));
        let on = image_meta(dir, true, BTreeMap::new());
        assert_eq!(on.revision.as_deref(), Some("b".repeat(40).as_str()));
        assert_eq!(on.source.as_deref(), Some("https://github.com/acme/site.git"));
        let gi = format!("{:?}", git_info(dir));
        assert!(!gi.contains("SECRET") && !gi.contains("alice"));
        assert_eq!(clean_remote_url("git@github.com:acme/site.git").as_deref(), Some("git@github.com:acme/site.git"));
        assert_eq!(clean_remote_url("https://GitHub.com").as_deref(), Some("https://github.com/"));
        // packed-refs fallback
        std::fs::remove_file(dir.join(".git/refs/heads/main")).unwrap();
        std::fs::write(dir.join(".git/packed-refs"), format!("# pack-refs\n{} refs/heads/main\n", "c".repeat(40))).unwrap();
        assert_eq!(git_info(dir).revision.as_deref(), Some("c".repeat(40).as_str()));
    }

    #[test]
    fn uncompressed_layer_is_the_tar_itself() {
        let gz = build_oci_image(&tree(), &OciOptions::default()).unwrap();
        let img = build_oci_image(&tree(), &OciOptions { compression: OciCompression::None, ..Default::default() }).unwrap();
        assert_eq!(img.diff_id, gz.diff_id);
        assert_eq!(img.layer_digest, img.diff_id);
        assert_eq!(sha256(&img.layer_bytes), img.diff_id);
        assert_eq!(img.config_digest, gz.config_digest);
        assert_ne!(img.manifest_digest, gz.manifest_digest);
        let m = parse_manifest(&img.manifest_bytes).unwrap();
        assert_eq!(m.layers[0].media_type, media::LAYER_TAR);
        assert_eq!(m.layers[0].size, img.layer_bytes.len() as u64);
        let cfg = parse_config(&img.config_bytes).unwrap();
        assert_eq!(extract_image_files(&m, &cfg, &img.layer_bytes, "x").unwrap().digest(), tree().digest());
        let mut bad = img.layer_bytes.clone();
        bad[600] ^= 1;
        assert_eq!(extract_image_files(&m, &cfg, &bad, "x").unwrap_err().code, E_STORE);
        // with a BASE, base layers keep their (normalized) media type
        let base = BaseImage {
            name: "docker.io/library/nginx:alpine".into(),
            manifest_digest: format!("sha256:{}", "2".repeat(64)),
            layers: vec![Descriptor { media_type: media::DOCKER_LAYER.into(), digest: format!("sha256:{}", "3".repeat(64)), size: 10, annotations: None }],
            config: serde_json::json!({ "rootfs": { "type": "layers", "diff_ids": [format!("sha256:{}", "1".repeat(64))] } }).as_object().unwrap().clone(),
        };
        let img = build_oci_image(&tree(), &OciOptions { compression: OciCompression::None, base: Some(base), ..Default::default() }).unwrap();
        let types: Vec<String> = parse_manifest(&img.manifest_bytes).unwrap().layers.into_iter().map(|l| l.media_type).collect();
        assert_eq!(types, [media::LAYER, media::LAYER_TAR]);
    }

    #[test]
    fn base_image_layers_and_config_are_kept() {
        let base_cfg: Value = serde_json::json!({
            "architecture": "arm64", "os": "linux", "created": "2024-01-01T00:00:00Z",
            "config": { "Cmd": ["nginx"], "Labels": { "maintainer": "x", "n": 1 } },
            "rootfs": { "type": "layers", "diff_ids": [format!("sha256:{}", "1".repeat(64))] },
            "history": [{ "created_by": "base" }]
        });
        let base = BaseImage {
            name: "docker.io/library/nginx:alpine".into(),
            manifest_digest: format!("sha256:{}", "2".repeat(64)),
            layers: vec![Descriptor { media_type: media::DOCKER_LAYER.into(), digest: format!("sha256:{}", "3".repeat(64)), size: 10, annotations: None }],
            config: base_cfg.as_object().unwrap().clone(),
        };
        let img = build_oci_image(&tree(), &OciOptions { base: Some(base.clone()), ..Default::default() }).unwrap();
        let m = parse_manifest(&img.manifest_bytes).unwrap();
        assert_eq!(m.layers.len(), 2);
        assert_eq!(m.layers[0].media_type, media::LAYER);
        let cfg = parse_config(&img.config_bytes).unwrap();
        assert_eq!(cfg.architecture, "arm64");
        assert_eq!(cfg.created, "2024-01-01T00:00:00Z");
        assert_eq!(cfg.labels["maintainer"], "x");
        assert!(!cfg.labels.contains_key("n"));
        assert_eq!(cfg.labels[ann::BASE_NAME], base.name);
        assert_eq!(cfg.diff_ids.len(), 2);
        assert_eq!(site_layer_index(&m, &cfg), Some(1));
        let j: Value = serde_json::from_slice(&img.config_bytes).unwrap();
        assert_eq!(j["config"]["Cmd"][0], "nginx");
        assert_eq!(j["history"].as_array().unwrap().len(), 2);
        let mut bad = base;
        bad.layers.push(bad.layers[0].clone());
        assert_eq!(build_oci_image(&tree(), &OciOptions { base: Some(bad), ..Default::default() }).unwrap_err().code, E_STORE);
    }

    #[test]
    fn tar_reader_rejects_unsafe_entries() {
        use crate::export::archive_writers::write_tar;
        let ok = write_tar(&tree(), &WriterOptions::default()).unwrap();
        let mut sym = ok.clone();
        sym[156] = b'2';
        // fix checksum
        sym[148..156].fill(b' ');
        let sum: u64 = sym[..512].iter().map(|&b| b as u64).sum();
        sym[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        assert_eq!(read_tar(&sym, "t", 10, 1 << 20).unwrap_err().code, E_ARCHIVE_UNSAFE);
        let mut bad = ok.clone();
        bad[0] ^= 1;
        assert_eq!(read_tar(&bad, "t", 10, 1 << 20).unwrap_err().code, E_ARCHIVE);
        assert_eq!(read_tar(&ok, "t", 2, 1 << 20).unwrap_err().code, E_ARCHIVE_LIMIT);
        assert_eq!(read_tar(&ok[..515], "t", 10, 1 << 20).unwrap_err().code, E_ARCHIVE);
    }

    /// The config and manifest JSON are byte-identical to the TypeScript writer
    /// once the layer digest (gzip-dependent) is substituted; the diff_id is identical.
    #[test]
    fn json_identical_to_typescript() {
        let mut labels = BTreeMap::new();
        labels.insert("org.example.team".to_string(), "web".to_string());
        let opts = OciOptions {
            title: Some("demo".into()),
            revision: Some("a".repeat(40)),
            source: Some("https://example.com/r.git".into()),
            epoch: Some(1_700_000_000),
            labels,
            ..Default::default()
        };
        let img = build_oci_image(&tree(), &opts).unwrap();
        assert_eq!(img.diff_id, TS_DIFF_ID);
        assert_eq!(String::from_utf8(img.config_bytes.clone()).unwrap(), TS_CONFIG);
        let manifest = String::from_utf8(img.manifest_bytes.clone()).unwrap();
        let ts = TS_MANIFEST
            .replace("LAYER_DIGEST", &img.layer_digest)
            .replace("LAYER_SIZE", &img.layer_bytes.len().to_string())
            .replace("CONFIG_DIGEST", &img.config_digest);
        assert_eq!(manifest, ts);
        let idx = String::from_utf8(layout_tree(img.parts(), Some("localhost:5000/blog:2"), &[]).unwrap().get("/index.json").unwrap().bytes.to_vec()).unwrap();
        let ts_idx = TS_INDEX.replace("MANIFEST_DIGEST", &img.manifest_digest).replace("MANIFEST_SIZE", &img.manifest_bytes.len().to_string());
        assert_eq!(idx, ts_idx);
    }
    const TS_DIFF_ID: &str = "sha256:258f521e8b16cd73e7241edb0b75726cf7fb470db86d67635c1a3545cf272b8c";
    const TS_CONFIG: &str = r#"{"architecture":"amd64","config":{"Labels":{"dev.sitefile.root":"/srv/www","org.example.team":"web","org.opencontainers.image.revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","org.opencontainers.image.source":"https://example.com/r.git","org.opencontainers.image.title":"demo"}},"created":"2023-11-14T22:13:20Z","history":[{"author":"sitefile","comment":"static site files","created":"2023-11-14T22:13:20Z","created_by":"sitefile"}],"os":"linux","rootfs":{"diff_ids":["sha256:258f521e8b16cd73e7241edb0b75726cf7fb470db86d67635c1a3545cf272b8c"],"type":"layers"}}"#;
    const TS_MANIFEST: &str = r#"{"annotations":{"dev.sitefile.files":"3","dev.sitefile.size":"27","org.opencontainers.image.title":"demo"},"config":{"digest":"CONFIG_DIGEST","mediaType":"application/vnd.oci.image.config.v1+json","size":564},"layers":[{"digest":"LAYER_DIGEST","mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","size":LAYER_SIZE}],"mediaType":"application/vnd.oci.image.manifest.v1+json","schemaVersion":2}"#;
    const TS_INDEX: &str = r#"{"manifests":[{"annotations":{"io.containerd.image.name":"localhost:5000/blog:2","org.opencontainers.image.ref.name":"2"},"digest":"MANIFEST_DIGEST","mediaType":"application/vnd.oci.image.manifest.v1+json","size":MANIFEST_SIZE}],"mediaType":"application/vnd.oci.image.index.v1+json","schemaVersion":2}"#;
}
