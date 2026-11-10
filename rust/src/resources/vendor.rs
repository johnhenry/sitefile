//! The vendor directory written by `sitefile vendor` (port of `src/resources/vendor.ts`, issue #96):
//! every remote input's bytes under content-addressed names, plus a manifest that maps URLs and image
//! tags to them.
//!
//! ```text
//! <dir>/manifest.json          {"version":1,"resources":{url:{integrity,size,resolved}},"images":{ref:{digest,platform,manifest}}}
//! <dir>/blobs/sha256/<hex>     IMPORT bodies and BASE image blobs (index, manifest, config, layers)
//! ```
//!
//! Builds prefer these copies over the network and verify each one against its digest (and, as for any
//! fetch, against INTEGRITY and Sitefile.lock). Nothing secret is written: URLs with credentials are
//! rejected before any fetch, and registry tokens or passwords never leave memory.

use crate::digest::{is_integrity, sha256};
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::executor::{normalize_posix, realpath_loose};
use crate::export::exporter::random_hex;
use crate::export::oci::{media, parse_manifest};
use crate::registry::select_platform;
use crate::resources::local::{EntryKind, LocalContext};
use crate::resources::types::Resource;
use crate::store::Store;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_VENDOR_DIR: &str = "vendor/sitefile";
pub const VENDOR_MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredResource {
    /// `sha256:<hex>` of the body: also its blob name.
    pub integrity: String,
    pub size: u64,
    /// Final URL after redirects, as the lockfile records it.
    pub resolved: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredImage {
    /// What the reference resolved to (an index for multi-platform images, else the manifest).
    pub digest: String,
    /// The platform that was vendored (`os/arch`).
    pub platform: String,
    /// That platform's manifest digest (the image ID).
    pub manifest: String,
}

/// A Git IMPORT: the commit it was read from and its content (a file's bytes, or a tree as a tar).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredGit {
    pub commit: String,
    /// `tree` or `file`.
    pub kind: String,
    /// `sha256:<hex>` of the stored bytes: also their blob name.
    pub integrity: String,
    pub size: u64,
    /// Files only: `644` or `755`.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorManifest {
    pub resources: BTreeMap<String, VendoredResource>,
    pub images: BTreeMap<String, VendoredImage>,
    /// Written only when non-empty.
    pub git: BTreeMap<String, VendoredGit>,
}

/// Validate a vendor directory: a relative path inside the build context. Returns it normalized (POSIX, no trailing slash).
pub fn vendor_dir_rel(input: &str) -> Result<String> {
    let n = normalize_posix(input).trim_end_matches('/').to_string();
    if input.is_empty()
        || input.starts_with('/')
        || input.contains('\\')
        || n == "."
        || n == ".."
        || n.starts_with("../")
        || n.is_empty()
    {
        return Err(SitefileError::plain(
            E_USAGE,
            format!("--vendor-dir expects a relative path inside the build context, got '{input}'"),
        ));
    }
    Ok(n)
}

fn blob_rel(dir: &str, digest: &str) -> String {
    format!(
        "{dir}/blobs/sha256/{}",
        digest.strip_prefix("sha256:").unwrap_or(digest)
    )
}

fn host_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for seg in rel.split('/') {
        p.push(seg);
    }
    p
}

/// The vendor directory a build uses: `explicit` (which must have a manifest), else the default one
/// if its manifest exists, else none.
pub fn find_vendor_dir(context_real: &Path, explicit: Option<&str>) -> Result<Option<String>> {
    let dir = vendor_dir_rel(explicit.unwrap_or(DEFAULT_VENDOR_DIR))?;
    if fs::metadata(host_path(context_real, &dir).join(VENDOR_MANIFEST)).is_ok_and(|m| m.is_file())
    {
        return Ok(Some(dir));
    }
    if explicit.is_some() {
        return Err(SitefileError::plain(
            E_NOT_FOUND,
            format!("vendor directory '{dir}' has no {VENDOR_MANIFEST}; run 'sitefile vendor --vendor-dir {dir}' first"),
        ));
    }
    Ok(None)
}

/// `Number.isSafeInteger(v) && v >= 0`
fn safe_size(v: &Value) -> Option<u64> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    if let Some(n) = v.as_u64() {
        return (n <= MAX_SAFE).then_some(n);
    }
    let f = v.as_f64()?;
    (f.fract() == 0.0 && (0.0..=MAX_SAFE as f64).contains(&f)).then_some(f as u64)
}

/// Parse and check a vendor manifest. Errors are E_VALIDATION naming the file.
pub fn parse_vendor_manifest(bytes: &[u8], shown: &str) -> Result<VendorManifest> {
    let bad = |why: String| {
        SitefileError::plain(
            E_VALIDATION,
            format!("malformed vendor manifest {shown}: {why}"),
        )
    };
    let data: Value = serde_json::from_str(&String::from_utf8_lossy(bytes))
        .map_err(|_| bad("invalid JSON".into()))?;
    let obj = data
        .as_object()
        .ok_or_else(|| bad("expected a JSON object".into()))?;
    let version = obj.get("version");
    if version.and_then(Value::as_f64) != Some(1.0) {
        let shown_v = version
            .map(|v| v.to_string())
            .unwrap_or_else(|| "undefined".into());
        return Err(bad(format!("unsupported version {shown_v} (expected 1)")));
    }
    let empty = Map::new();
    let section = |name: &str| -> Result<&Map<String, Value>> {
        match obj.get(name) {
            None | Some(Value::Null) => Ok(&empty),
            Some(Value::Object(m)) => Ok(m),
            Some(_) => Err(bad(format!("\"{name}\" must be an object"))),
        }
    };
    let mut m = VendorManifest::default();
    for (url, e) in section("resources")? {
        let integrity = e
            .get("integrity")
            .and_then(Value::as_str)
            .filter(|s| is_integrity(s));
        let size = e.get("size").and_then(safe_size);
        let resolved = e.get("resolved").and_then(Value::as_str);
        match (e.is_object(), integrity, size, resolved) {
            (true, Some(i), Some(s), Some(r)) => {
                m.resources.insert(
                    url.clone(),
                    VendoredResource {
                        integrity: i.into(),
                        size: s,
                        resolved: r.into(),
                    },
                );
            }
            _ => {
                return Err(bad(format!(
                "entry for {url} needs \"integrity\" (sha256:<64 hex>), \"size\" and \"resolved\""
            )))
            }
        }
    }
    for (r, e) in section("images")? {
        let digest = e
            .get("digest")
            .and_then(Value::as_str)
            .filter(|s| is_integrity(s));
        let platform = e.get("platform").and_then(Value::as_str);
        let manifest = e
            .get("manifest")
            .and_then(Value::as_str)
            .filter(|s| is_integrity(s));
        match (e.is_object(), digest, platform, manifest) {
            (true, Some(d), Some(p), Some(mf)) => {
                m.images.insert(
                    r.clone(),
                    VendoredImage {
                        digest: d.into(),
                        platform: p.into(),
                        manifest: mf.into(),
                    },
                );
            }
            _ => {
                return Err(bad(format!(
                    "image entry for {r} needs \"digest\", \"platform\" and \"manifest\""
                )))
            }
        }
    }
    for (url, e) in section("git")? {
        let commit = e
            .get("commit")
            .and_then(Value::as_str)
            .filter(|c| crate::resources::git::is_commit(c));
        let kind = e
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| *k == "tree" || *k == "file");
        let integrity = e
            .get("integrity")
            .and_then(Value::as_str)
            .filter(|s| is_integrity(s));
        let size = e.get("size").and_then(safe_size);
        let mode = e.get("mode");
        let mode_ok = match kind {
            Some("file") => matches!(mode.and_then(Value::as_str), Some("644" | "755")),
            _ => mode.is_none(),
        };
        match (e.is_object(), commit, kind, integrity, size, mode_ok) {
            (true, Some(c), Some(k), Some(i), Some(s), true) => {
                let mode = if k == "file" { mode.and_then(Value::as_str).map(str::to_string) } else { None };
                m.git.insert(url.clone(), VendoredGit { commit: c.into(), kind: k.into(), integrity: i.into(), size: s, mode });
            }
            _ => return Err(bad(format!("git entry for {url} needs \"commit\", \"kind\" (tree or file), \"integrity\", \"size\" (and \"mode\" for a file)"))),
        }
    }
    Ok(m)
}

/// The manifest as `sitefile vendor` writes it: sorted keys, two-space indent, trailing newline.
pub fn serialize_vendor_manifest(m: &VendorManifest) -> String {
    let mut resources = Map::new();
    for (url, e) in &m.resources {
        let mut o = Map::new();
        o.insert("integrity".into(), Value::String(e.integrity.clone()));
        o.insert("size".into(), Value::from(e.size));
        o.insert("resolved".into(), Value::String(e.resolved.clone()));
        resources.insert(url.clone(), Value::Object(o));
    }
    let mut images = Map::new();
    for (r, e) in &m.images {
        let mut o = Map::new();
        o.insert("digest".into(), Value::String(e.digest.clone()));
        o.insert("platform".into(), Value::String(e.platform.clone()));
        o.insert("manifest".into(), Value::String(e.manifest.clone()));
        images.insert(r.clone(), Value::Object(o));
    }
    let mut data = Map::new();
    data.insert("version".into(), Value::from(1));
    data.insert("resources".into(), Value::Object(resources));
    data.insert("images".into(), Value::Object(images));
    if !m.git.is_empty() {
        let mut git = Map::new();
        for (url, e) in &m.git {
            let mut o = Map::new();
            o.insert("commit".into(), Value::String(e.commit.clone()));
            o.insert("kind".into(), Value::String(e.kind.clone()));
            o.insert("integrity".into(), Value::String(e.integrity.clone()));
            o.insert("size".into(), Value::from(e.size));
            if let Some(md) = &e.mode {
                o.insert("mode".into(), Value::String(md.clone()));
            }
            git.insert(url.clone(), Value::Object(o));
        }
        data.insert("git".into(), Value::Object(git));
    }
    crate::values::js_json_pretty(&Value::Object(data)) + "\n"
}

fn is_index(bytes: &[u8]) -> bool {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(j) => match j.get("mediaType").and_then(Value::as_str) {
            Some(t) => t == media::INDEX || t == media::DOCKER_LIST,
            None => j.get("manifests").is_some_and(Value::is_array),
        },
        Err(_) => false,
    }
}

/// A vendor directory; its blobs are read through the build's sandboxed context reader.
#[derive(Debug, Clone)]
pub struct Vendor {
    /// Context-relative directory.
    pub dir: String,
    pub manifest: VendorManifest,
}

impl Vendor {
    pub fn open(local: &LocalContext, dir: &str) -> Result<Vendor> {
        let rel = format!("{dir}/{VENDOR_MANIFEST}");
        let Resource::File { bytes, .. } = local.read_file(&rel, None)? else {
            unreachable!()
        };
        Ok(Vendor {
            dir: dir.to_string(),
            manifest: parse_vendor_manifest(&bytes, &rel)?,
        })
    }

    pub fn resource(&self, url: &str) -> Option<&VendoredResource> {
        self.manifest.resources.get(url)
    }

    pub fn image(&self, r: &str) -> Option<&VendoredImage> {
        self.manifest.images.get(r)
    }

    pub fn git(&self, url: &str) -> Option<&VendoredGit> {
        self.manifest.git.get(url)
    }

    /// The vendored content of a Git IMPORT, verified against its digest.
    pub fn read_git(
        &self,
        local: &LocalContext,
        url: &str,
        e: &VendoredGit,
        basename: &str,
        limits: &crate::resources::types::ArchiveLimits,
        span: Option<&SourceSpan>,
    ) -> Result<crate::resources::git::GitSelection> {
        let what = format!("vendored copy of '{url}'");
        let bytes = self.read_blob(local, &e.integrity, &what, span)?;
        if e.kind == "file" {
            let mode = if e.mode.as_deref() == Some("755") {
                0o755
            } else {
                0o644
            };
            return Ok(crate::resources::git::GitSelection::File {
                bytes: bytes.into(),
                mode,
                basename: basename.to_string(),
            });
        }
        let tree = crate::resources::archive::extract_archive(&bytes, &what, limits, span)?;
        Ok(crate::resources::git::GitSelection::Tree(
            tree.entries()
                .map(|(p, n)| (p.clone(), n.bytes.clone(), n.mode))
                .collect(),
        ))
    }

    /// The vendored body of `url`, verified against its digest: (bytes, final URL).
    pub fn read_resource(
        &self,
        local: &LocalContext,
        url: &str,
        e: &VendoredResource,
        span: Option<&SourceSpan>,
    ) -> Result<(Vec<u8>, String)> {
        Ok((
            self.read_blob(
                local,
                &e.integrity,
                &format!("vendored copy of '{url}'"),
                span,
            )?,
            e.resolved.clone(),
        ))
    }

    fn read_blob(
        &self,
        local: &LocalContext,
        digest: &str,
        what: &str,
        span: Option<&SourceSpan>,
    ) -> Result<Vec<u8>> {
        let rel = blob_rel(&self.dir, digest);
        let Resource::File { bytes, .. } = local.read_file(&rel, span)? else {
            unreachable!()
        };
        let got = sha256(&bytes);
        if got != digest {
            return Err(SitefileError::new(
                E_INTEGRITY,
                format!("{what} ({rel}) is corrupt: expected {digest}, got {got}; run 'sitefile vendor' again"),
                span.cloned(),
            ));
        }
        Ok(bytes)
    }

    fn has_blob(&self, local: &LocalContext, digest: &str) -> Result<bool> {
        match local.stat(&blob_rel(&self.dir, digest), None) {
            Ok(k) => Ok(k == EntryKind::File),
            Err(e) if e.code == E_NOT_FOUND => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Copy the vendored blobs of the image `root_digest` (for `platform`) into the store, each
    /// verified, so pulling it finds everything locally. Stops quietly at the first blob that is not
    /// vendored: the pull then fetches the rest from the registry.
    pub fn seed_store(
        &self,
        local: &LocalContext,
        store: &Store,
        root_digest: &str,
        platform: &str,
    ) -> Result<()> {
        if !self.has_blob(local, root_digest)? {
            return Ok(());
        }
        let what = "vendored image blob";
        let root = self.read_blob(local, root_digest, what, None)?;
        store.put_small_blob(&root)?;
        let mut manifest = root;
        if is_index(&manifest) {
            let Ok(d) = select_platform(&manifest, platform) else {
                return Ok(());
            };
            if !self.has_blob(local, &d.digest)? {
                return Ok(());
            }
            manifest = self.read_blob(local, &d.digest, what, None)?;
            store.put_small_blob(&manifest)?;
        }
        let Ok(parsed) = parse_manifest(&manifest) else {
            return Ok(());
        };
        for d in std::iter::once(&parsed.config).chain(parsed.layers.iter()) {
            if store.has_blob(&d.digest, Some(d.size)) {
                continue;
            }
            if !self.has_blob(local, &d.digest)? {
                return Ok(());
            }
            let bytes = self.read_blob(local, &d.digest, what, None)?;
            store.import_blob(&d.digest, d.size, &mut bytes.as_slice(), None)?;
        }
        Ok(())
    }
}

fn inside(root: &Path, real: &Path) -> bool {
    real != root && real.starts_with(root)
}

/// Replace `<context_real>/<dir>` with a vendor directory holding `manifest_text` and `blobs`: written
/// to a temporary sibling, then swapped in. Refuses a directory that resolves outside the context, a
/// symlink, and an existing directory that is not a vendor directory (no manifest.json).
pub fn write_vendor_dir(
    context_real: &Path,
    dir: &str,
    manifest_text: &str,
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf> {
    let escape = || {
        SitefileError::plain(
            E_PATH_ESCAPE,
            format!("vendor directory '{dir}' resolves outside the build context"),
        )
    };
    let io = |e: std::io::Error| {
        SitefileError::plain(
            E_EXPORT,
            format!("cannot write vendor directory '{dir}': {e}"),
        )
    };
    let target = host_path(context_real, dir);
    let parent = target.parent().unwrap_or(context_real).to_path_buf();
    if !inside(context_real, &realpath_loose(&target)) {
        return Err(escape());
    }
    let st = fs::symlink_metadata(&target).ok();
    if let Some(st) = &st {
        if st.file_type().is_symlink() {
            return Err(SitefileError::plain(
                E_EXPORT_UNSAFE,
                format!("vendor directory '{dir}' is a symlink; refusing to replace it"),
            ));
        }
        if !st.is_dir() {
            return Err(SitefileError::plain(
                E_EXPORT_UNSAFE,
                format!("vendor directory '{dir}' exists and is not a directory"),
            ));
        }
        let names: Vec<String> = fs::read_dir(&target)
            .map_err(io)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        if !names.is_empty() && !names.iter().any(|n| n == VENDOR_MANIFEST) {
            return Err(SitefileError::plain(
                E_EXPORT_UNSAFE,
                format!("'{dir}' exists and is not a sitefile vendor directory (no {VENDOR_MANIFEST}); remove it or choose another --vendor-dir"),
            ));
        }
    }
    fs::create_dir_all(&parent).map_err(io)?;
    let parent_real = crate::platform::canonicalize(&parent).map_err(io)?;
    if parent_real != context_real && !inside(context_real, &parent_real) {
        return Err(escape());
    }
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tag = format!("{}-{}", std::process::id(), random_hex(4));
    let tmp = parent.join(format!(".{name}.sitefile-vendor-tmp-{tag}"));
    let old = parent.join(format!(".{name}.sitefile-vendor-old-{tag}"));
    let r = (|| -> std::io::Result<()> {
        let blob_dir = tmp.join("blobs").join("sha256");
        fs::create_dir_all(&blob_dir)?;
        for (digest, bytes) in blobs {
            fs::write(
                blob_dir.join(digest.strip_prefix("sha256:").unwrap_or(digest)),
                bytes,
            )?;
        }
        fs::write(tmp.join(VENDOR_MANIFEST), manifest_text)?;
        if st.is_some() {
            fs::rename(&target, &old)?;
        }
        fs::rename(&tmp, &target)
    })();
    if let Err(e) = r {
        let _ = fs::remove_dir_all(&tmp);
        if st.is_some() && fs::symlink_metadata(&target).is_err() {
            let _ = fs::rename(&old, &target);
        }
        return Err(io(e));
    }
    let _ = fs::remove_dir_all(&old);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_dirs_must_stay_inside_the_context() {
        assert_eq!(vendor_dir_rel("a//b/").unwrap(), "a/b");
        assert_eq!(
            vendor_dir_rel("vendor/sitefile").unwrap(),
            "vendor/sitefile"
        );
        for bad in ["", ".", "..", "../x", "/abs", "a/../..", "a\\b"] {
            let e = vendor_dir_rel(bad).unwrap_err();
            assert_eq!(e.code, E_USAGE, "{bad}");
            assert_eq!(
                e.detail,
                format!(
                    "--vendor-dir expects a relative path inside the build context, got '{bad}'"
                )
            );
        }
    }

    #[test]
    fn manifest_round_trips_and_rejects_bad_input() {
        let mut m = VendorManifest::default();
        let d = format!("sha256:{}", "a".repeat(64));
        m.resources.insert(
            "https://b.example/x".into(),
            VendoredResource {
                integrity: d.clone(),
                size: 3,
                resolved: "https://b.example/y".into(),
            },
        );
        m.resources.insert(
            "https://a.example/x".into(),
            VendoredResource {
                integrity: d.clone(),
                size: 3,
                resolved: "https://a.example/x".into(),
            },
        );
        m.images.insert(
            "r.example/lib/base:1".into(),
            VendoredImage {
                digest: d.clone(),
                platform: "linux/amd64".into(),
                manifest: d.clone(),
            },
        );
        let text = serialize_vendor_manifest(&m);
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"resources\": {\n    \"https://a.example/x\": {\n      \"integrity\""), "{text}");
        assert!(text.ends_with("}\n"));
        assert_eq!(parse_vendor_manifest(text.as_bytes(), "m").unwrap(), m);
        assert_eq!(
            serialize_vendor_manifest(&VendorManifest::default()),
            "{\n  \"version\": 1,\n  \"resources\": {},\n  \"images\": {}\n}\n"
        );
        let err = |s: &str| {
            parse_vendor_manifest(s.as_bytes(), "v/manifest.json")
                .unwrap_err()
                .detail
        };
        assert_eq!(
            err("nope"),
            "malformed vendor manifest v/manifest.json: invalid JSON"
        );
        assert_eq!(
            err("{\"version\":2}"),
            "malformed vendor manifest v/manifest.json: unsupported version 2 (expected 1)"
        );
        assert_eq!(
            err("{}"),
            "malformed vendor manifest v/manifest.json: unsupported version undefined (expected 1)"
        );
        assert_eq!(
            err("{\"version\":1,\"resources\":[]}"),
            "malformed vendor manifest v/manifest.json: \"resources\" must be an object"
        );
        assert!(
            err("{\"version\":1,\"resources\":{\"u\":{\"integrity\":\"x\"}}}")
                .contains("entry for u needs")
        );
        assert!(err("{\"version\":1,\"images\":{\"r\":{}}}").contains("image entry for r needs"));
    }
}
