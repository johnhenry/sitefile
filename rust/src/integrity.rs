//! Per-file digest manifests and `sitefile verify` (port of `src/integrity.ts`, issue #211).
//!
//! A manifest lists every exported file with its size and SHA-256, plus a Merkle
//! root over the sorted (path, digest) pairs: the RFC 6962 Merkle Tree Hash of the
//! leaves `path 0x00 digest` (UTF-8, digest written `sha256:<hex>`). A leaf hashes
//! as SHA-256(0x00 ‖ leaf), an inner node as SHA-256(0x01 ‖ left ‖ right), split
//! at the largest power of two below the count; no files hash as SHA-256 of nothing.

use crate::error::{ErrorCode::*, Result, SitefileError};
use crate::resources::local::{LocalContext, LocalContextOptions};
use crate::resources::types::Resource;
use crate::servers::{load_archive_tree_all, looks_like_dir};
use crate::store::Store;
use crate::tree::Tree;
use crate::values::js_json_pretty;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.sitefile.manifest.v1+json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    /// `sha256:<hex>` of the file's bytes.
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `sha256:<hex>`: the Merkle root of `files`.
    pub root: String,
    pub files: Vec<ManifestFile>,
}

fn sha(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn mth(leaves: &[Vec<u8>]) -> [u8; 32] {
    match leaves.len() {
        0 => sha(&[]),
        1 => sha(&[&[0], &leaves[0]]),
        n => {
            let mut k = 1;
            while k * 2 < n {
                k *= 2;
            }
            let (l, r) = (mth(&leaves[..k]), mth(&leaves[k..]));
            sha(&[&[1], &l, &r])
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The Merkle root of `files` (in the order given; manifests list them by path).
pub fn merkle_root(files: &[ManifestFile]) -> String {
    let leaves: Vec<Vec<u8>> = files
        .iter()
        .map(|f| format!("{}\0{}", f.path, f.digest).into_bytes())
        .collect();
    format!("sha256:{}", hex(&mth(&leaves)))
}

/// Tree paths in the TypeScript order (UTF-16 code units).
pub fn sorted_entries(tree: &Tree) -> Vec<(&String, &crate::tree::Node)> {
    let mut v: Vec<_> = tree.entries().collect();
    v.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    v
}

/// The manifest of a tree: every file in path order.
pub fn manifest_of(tree: &Tree) -> Manifest {
    let files: Vec<ManifestFile> = sorted_entries(tree)
        .into_iter()
        .map(|(p, n)| ManifestFile {
            path: p.clone(),
            size: n.bytes.len() as u64,
            digest: n.digest.clone(),
        })
        .collect();
    Manifest {
        root: merkle_root(&files),
        files,
    }
}

/// The manifest as written to a file: `JSON.stringify(manifest, null, 2)` and a newline.
pub fn encode_manifest(m: &Manifest) -> Vec<u8> {
    let files: Vec<Value> = m
        .files
        .iter()
        .map(|f| json!({ "path": f.path, "size": f.size, "digest": f.digest }))
        .collect();
    let v = json!({ "mediaType": MANIFEST_MEDIA_TYPE, "root": m.root, "files": files });
    (js_json_pretty(&v) + "\n").into_bytes()
}

fn is_digest(s: &str) -> bool {
    crate::digest::is_integrity(s)
}

/// Parse a manifest file. Its root must match its files.
pub fn parse_manifest(bytes: &[u8], origin: &str) -> Result<Manifest> {
    let bad = |why: String| SitefileError::plain(E_INTEGRITY, format!("{origin}: {why}"));
    let v: Value = std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| serde_json::from_str(s).ok())
        .ok_or_else(|| bad("not a Sitefile manifest (invalid JSON)".into()))?;
    let o: &Map<String, Value> = match v.as_object() {
        Some(o) if o.get("mediaType").and_then(Value::as_str) == Some(MANIFEST_MEDIA_TYPE) => o,
        _ => {
            return Err(bad(format!(
                "not a Sitefile manifest (mediaType is not {MANIFEST_MEDIA_TYPE})"
            )))
        }
    };
    let root = o
        .get("root")
        .and_then(Value::as_str)
        .filter(|r| is_digest(r));
    let (Some(root), Some(list)) = (root, o.get("files").and_then(Value::as_array)) else {
        return Err(bad("not a Sitefile manifest (root or files missing)".into()));
    };
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for f in list {
        let e = f.as_object();
        let path = e
            .and_then(|e| e.get("path"))
            .and_then(Value::as_str)
            .filter(|p| p.starts_with('/'));
        // Number.isSafeInteger(size) && size >= 0
        let size = e.and_then(|e| e.get("size")).and_then(|s| {
            s.as_u64().or_else(|| {
                s.as_f64()
                    .filter(|x| x.fract() == 0.0 && *x >= 0.0 && *x <= 9_007_199_254_740_991.0)
                    .map(|x| x as u64)
            })
        });
        let size = size.filter(|s| *s <= 9_007_199_254_740_991);
        let digest = e
            .and_then(|e| e.get("digest"))
            .and_then(Value::as_str)
            .filter(|d| is_digest(d));
        let (Some(path), Some(size), Some(digest)) = (path, size, digest) else {
            return Err(bad(
                "not a Sitefile manifest (a file entry needs path, size and digest)".into(),
            ));
        };
        if !seen.insert(path.to_string()) {
            return Err(bad(format!("lists '{path}' twice")));
        }
        files.push(ManifestFile {
            path: path.to_string(),
            size,
            digest: digest.to_string(),
        });
    }
    let computed = merkle_root(&files);
    if computed != root {
        return Err(bad(format!(
            "its root {root} does not match its files ({computed})"
        )));
    }
    Ok(Manifest {
        root: computed,
        files,
    })
}

/// What `verify` and `diff` compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Dir(String),
    Archive(String),
    Image(String),
}

fn kind_on_disk(p: &str) -> Option<Target> {
    let m = fs::metadata(p).ok()?;
    if m.is_dir() {
        Some(Target::Dir(p.to_string()))
    } else if m.is_file() {
        Some(Target::Archive(p.to_string()))
    } else {
        None
    }
}

/// Lazily opened store (verify of a directory never needs one).
#[derive(Default)]
pub struct LazyStore(Option<Store>);

impl LazyStore {
    pub fn get(&mut self) -> Result<&Store> {
        if self.0.is_none() {
            self.0 = Some(Store::open_default()?);
        }
        Ok(self.0.as_ref().expect("opened"))
    }
}

/// Resolve a `verify`/`diff` argument: a path (written `./x`, `/x`, `x/`, …) is a
/// directory or an archive file; anything else is an image reference or ID when the
/// store has it, else a directory or file by that name.
pub fn resolve_target(arg: &str, store: &mut LazyStore) -> Result<Target> {
    if looks_like_dir(arg) {
        return kind_on_disk(arg).ok_or_else(|| {
            SitefileError::plain(E_NOT_FOUND, format!("'{arg}' is not a directory or a file"))
        });
    }
    match store.get().and_then(|s| s.resolve(arg)) {
        Ok(_) => Ok(Target::Image(arg.to_string())),
        Err(e) if e.code == E_NOT_FOUND => kind_on_disk(arg).ok_or(e),
        Err(e) => Err(e),
    }
}

/// The files of a target, hidden ones included (see `loadTarget` in integrity.ts).
pub fn load_target(t: &Target, store: &mut LazyStore) -> Result<Tree> {
    match t {
        Target::Image(r) => {
            let s = store.get()?;
            s.load_tree(&s.resolve(r)?)
        }
        Target::Dir(d) => {
            let ctx = LocalContext::open(
                d,
                &LocalContextOptions {
                    sitefileignore: false,
                    allow_hardlinks: true,
                    ..Default::default()
                },
            )?;
            let Resource::Tree { tree, .. } = ctx.read_tree("", "verify", None)? else {
                unreachable!("read_tree returns a tree")
            };
            Ok(tree)
        }
        Target::Archive(f) => load_archive_tree_all(Path::new(f)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyChange {
    pub path: String,
    pub expected: (u64, String),
    pub actual: (u64, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub root: String,
    pub files: usize,
    pub missing: Vec<String>,
    pub changed: Vec<VerifyChange>,
    pub extra: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.extra.is_empty()
    }

    pub fn to_json(&self) -> Value {
        let side = |(size, digest): &(u64, String)| json!({ "size": size, "digest": digest });
        json!({
            "ok": self.ok(),
            "root": self.root,
            "files": self.files,
            "missing": self.missing,
            "changed": self.changed.iter().map(|c| json!({
                "path": c.path, "expected": side(&c.expected), "actual": side(&c.actual)
            })).collect::<Vec<_>>(),
            "extra": self.extra,
        })
    }

    /// One `missing|changed|extra  <path>` line per difference, in path order.
    pub fn text(&self) -> String {
        let mut lines: Vec<(&str, &str)> = self
            .missing
            .iter()
            .map(|p| (p.as_str(), "missing"))
            .chain(self.changed.iter().map(|c| (c.path.as_str(), "changed")))
            .chain(self.extra.iter().map(|p| (p.as_str(), "extra")))
            .collect();
        lines.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        lines.iter().map(|(p, k)| format!("{k:<8} {p}\n")).collect()
    }
}

/// Compare `tree` with `manifest`; `ignore` is not counted as extra.
pub fn verify_tree(tree: &Tree, manifest: &Manifest, ignore: Option<&str>) -> VerifyReport {
    let listed: std::collections::HashSet<&str> =
        manifest.files.iter().map(|f| f.path.as_str()).collect();
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    for f in &manifest.files {
        match tree.get(&f.path) {
            None => missing.push(f.path.clone()),
            Some(n) if n.digest != f.digest || n.bytes.len() as u64 != f.size => {
                changed.push(VerifyChange {
                    path: f.path.clone(),
                    expected: (f.size, f.digest.clone()),
                    actual: (n.bytes.len() as u64, n.digest.clone()),
                })
            }
            Some(_) => {}
        }
    }
    let extra = sorted_entries(tree)
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| !listed.contains(p.as_str()) && Some(p.as_str()) != ignore)
        .cloned()
        .collect();
    VerifyReport {
        root: manifest.root.clone(),
        files: manifest.files.len(),
        missing,
        changed,
        extra,
    }
}

/// The tree path of `file` when it lies inside directory `dir` (by real paths).
pub fn path_inside(dir: &str, file: &str) -> Option<String> {
    let d = fs::canonicalize(dir).ok()?;
    let f = fs::canonicalize(file).ok()?;
    let rel = f.strip_prefix(&d).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(format!(
        "/{}",
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str, d: &str) -> ManifestFile {
        ManifestFile {
            path: p.into(),
            size: 1,
            digest: d.into(),
        }
    }

    #[test]
    fn rfc6962_shape() {
        let leaf = |s: &str| sha(&[&[0], s.as_bytes()]);
        let a = f("/a", "sha256:00");
        let b = f("/b", "sha256:11");
        let c = f("/c", "sha256:22");
        assert_eq!(merkle_root(&[]), format!("sha256:{}", hex(&sha(&[]))));
        let (la, lb, lc) = (
            leaf("/a\0sha256:00"),
            leaf("/b\0sha256:11"),
            leaf("/c\0sha256:22"),
        );
        let ab = sha(&[&[1], &la, &lb]);
        assert_eq!(
            merkle_root(&[a.clone(), b.clone()]),
            format!("sha256:{}", hex(&ab))
        );
        let abc = sha(&[&[1], &ab, &lc]);
        assert_eq!(merkle_root(&[a, b, c]), format!("sha256:{}", hex(&abc)));
    }

    #[test]
    fn manifest_round_trips_and_rejects_tampering() {
        let files = vec![f("/a", &crate::digest::sha256(b"x"))];
        let m = Manifest {
            root: merkle_root(&files),
            files,
        };
        let bytes = encode_manifest(&m);
        assert_eq!(parse_manifest(&bytes, "m").unwrap(), m);
        let text = String::from_utf8(bytes)
            .unwrap()
            .replace("\"/a\"", "\"/b\"");
        let e = parse_manifest(text.as_bytes(), "m").unwrap_err();
        assert_eq!(e.code, E_INTEGRITY);
    }
}
