//! Immutable filesystem trees (src/tree.ts). Files are shared via `Arc`, so
//! copying a tree copies pointers, not bytes.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::{fold_case, nfc};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    pub bytes: Arc<[u8]>,
    /// 0o644 or 0o755.
    pub mode: u32,
    /// `sha256:<hex>` of bytes.
    pub digest: String,
    /// Who wrote this path, e.g. `Sitefile:4 COPY`.
    pub origin: String,
}

pub type Node = Arc<FileNode>;

/// Reduce a mode to 0o644 or 0o755.
pub fn normalize_mode(mode: u32) -> u32 {
    if mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

pub fn make_file(bytes: impl Into<Arc<[u8]>>, origin: impl Into<String>, mode: u32) -> Node {
    let bytes: Arc<[u8]> = bytes.into();
    let digest = sha256(&bytes);
    Arc::new(FileNode { bytes, mode: normalize_mode(mode), digest, origin: origin.into() })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Error,
    Overwrite,
}

/// The spelling of a path (file or implicit directory) that owns its case-folded key.
#[derive(Debug)]
struct Owner {
    path: String,
    origin: String,
    dir: bool,
}

/// A path and its ancestors, as (path, fold_case(path)) pairs from the
/// shortest ancestor to the path itself. Folding is per segment, so prefix
/// lengths never depend on how folding changes a string's length.
fn prefixes(path: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut raw = String::with_capacity(path.len());
    let mut folded = String::with_capacity(path.len());
    for seg in path[1..].split('/') {
        raw.push('/');
        raw.push_str(seg);
        folded.push('/');
        folded.push_str(&fold_case(seg));
        out.push((raw.clone(), folded.clone()));
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Tree {
    files: BTreeMap<String, Node>,
    dirs: BTreeSet<String>,
    /// fold_case(path) -> the one spelling of it in this tree (files and directories).
    folded: HashMap<String, Arc<Owner>>,
}

impl Tree {
    pub fn empty() -> Tree {
        Tree::default()
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    pub fn get(&self, path: &str) -> Option<&Node> {
        self.files.get(path)
    }
    pub fn is_file(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
    /// True for `/` and any implicit directory.
    pub fn is_directory(&self, path: &str) -> bool {
        path == "/" || self.dirs.contains(path)
    }
    /// Sorted (path, node) pairs. Paths compare as UTF-16 code units in the
    /// TypeScript version; for the ASCII paths sites use this is byte order.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.files.iter()
    }

    /// Add files. Paths are normalized to NFC. Identical content (digest and
    /// mode) keeps the first owner; different content fails with E_CONFLICT
    /// unless `Overwrite`; a file can never replace a directory or the
    /// reverse. A path (or one of its directories) equal to an existing one
    /// under simple case folding but spelled differently fails with
    /// E_PATH_COLLISION whatever the policy.
    pub fn with_files<I>(&self, additions: I, policy: ConflictPolicy, span: Option<&SourceSpan>) -> Result<Tree>
    where
        I: IntoIterator<Item = (String, Node)>,
    {
        let mut files = self.files.clone();
        let mut dirs = self.dirs.clone();
        let mut folded = self.folded.clone();
        for (raw_path, node) in additions {
            let path = match nfc(&raw_path) {
                std::borrow::Cow::Borrowed(_) => raw_path,
                std::borrow::Cow::Owned(p) => p,
            };
            if path == "/" || dirs.contains(&path) {
                return Err(SitefileError::new(E_CONFLICT, format!("cannot write file '{path}': it is a directory"), span.cloned()));
            }
            let chain = prefixes(&path);
            let ancestors = &chain[..chain.len() - 1];
            for (a, _) in ancestors {
                if let Some(f) = files.get(a) {
                    return Err(SitefileError::new(
                        E_CONFLICT,
                        format!("cannot write '{path}': '{a}' is a file (written by {})", f.origin),
                        span.cloned(),
                    ));
                }
            }
            for (p, key) in &chain {
                if let Some(prior) = folded.get(key) {
                    if prior.path != *p {
                        let by = format!("{} by {}", if prior.dir { "created" } else { "written" }, prior.origin);
                        let what = format!("'{p}' differs only in case from '{}' ({by}); case-insensitive file systems cannot hold both", prior.path);
                        return Err(SitefileError::new(
                            E_PATH_COLLISION,
                            if *p == path { what } else { format!("cannot write '{path}': {what}") },
                            span.cloned(),
                        ));
                    }
                }
            }
            if let Some(existing) = files.get(&path) {
                if existing.digest == node.digest && existing.mode == node.mode {
                    continue;
                }
                if policy == ConflictPolicy::Error {
                    return Err(SitefileError::new(
                        E_CONFLICT,
                        format!("'{path}' was already written with different content by {}; use --overwrite to replace it", existing.origin),
                        span.cloned(),
                    ));
                }
            }
            for (a, key) in ancestors {
                if dirs.insert(a.clone()) {
                    folded.insert(key.clone(), Arc::new(Owner { path: a.clone(), origin: node.origin.clone(), dir: true }));
                }
            }
            let key = chain.last().expect("a path has at least one segment").1.clone();
            folded.insert(key, Arc::new(Owner { path: path.clone(), origin: node.origin.clone(), dir: false }));
            files.insert(path, node);
        }
        Ok(Tree { files, dirs, folded })
    }

    /// The files under a directory, re-rooted at `/`.
    pub fn subtree(&self, dir: &str) -> Tree {
        if dir == "/" {
            return self.clone();
        }
        let prefix = format!("{dir}/");
        let pairs = self
            .files
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .map(|(p, n)| (p[dir.len()..].to_string(), n.clone()));
        Tree::from_files(pairs).expect("a subtree of a valid tree is valid")
    }

    /// The files of this tree placed under `dir`.
    pub fn mounted_at(&self, dir: &str) -> Vec<(String, Node)> {
        self.files
            .iter()
            .map(|(p, n)| (if dir == "/" { p.clone() } else { format!("{dir}{p}") }, n.clone()))
            .collect()
    }

    /// Content digest of the whole tree (paths, modes and file digests),
    /// identical to `Tree.digest()` in TypeScript.
    pub fn digest(&self) -> String {
        let mut s = String::with_capacity(self.files.len() * 96);
        for (p, n) in &self.files {
            s.push_str(p);
            s.push('\0');
            s.push_str(&format!("{:o}", n.mode));
            s.push('\0');
            s.push_str(&n.digest);
            s.push('\n');
        }
        sha256(s.as_bytes())
    }

    pub fn from_files<I: IntoIterator<Item = (String, Node)>>(files: I) -> Result<Tree> {
        Tree::empty().with_files(files, ConflictPolicy::Error, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn f(s: &str) -> Node {
        make_file(s.as_bytes().to_vec(), "t", 0o644)
    }
    #[test]
    fn conflicts_and_identical_writes() {
        let t = Tree::from_files([("/a/b.txt".to_string(), f("x"))]).unwrap();
        assert!(t.is_directory("/a"));
        assert!(t.with_files([("/a/b.txt".into(), f("x"))], ConflictPolicy::Error, None).is_ok());
        assert_eq!(t.with_files([("/a/b.txt".into(), f("y"))], ConflictPolicy::Error, None).unwrap_err().code, E_CONFLICT);
        assert!(t.with_files([("/a/b.txt".into(), f("y"))], ConflictPolicy::Overwrite, None).is_ok());
        assert_eq!(t.with_files([("/a".into(), f("y"))], ConflictPolicy::Overwrite, None).unwrap_err().code, E_CONFLICT);
        assert_eq!(t.with_files([("/a/b.txt/c".into(), f("y"))], ConflictPolicy::Error, None).unwrap_err().code, E_CONFLICT);
    }
    fn g(s: &str) -> Node {
        make_file(s.as_bytes().to_vec(), format!("Sitefile:1 COPY {s}"), 0o644)
    }
    fn collision(r: Result<Tree>) -> String {
        let e = r.unwrap_err();
        assert_eq!(e.code, E_PATH_COLLISION, "{}", e.detail);
        e.detail
    }
    #[test]
    fn case_collisions() {
        let t = Tree::from_files([("/A.html".to_string(), g("A"))]).unwrap();
        assert_eq!(
            collision(t.with_files([("/a.html".into(), g("b"))], ConflictPolicy::Error, None)),
            "'/a.html' differs only in case from '/A.html' (written by Sitefile:1 COPY A); case-insensitive file systems cannot hold both"
        );
        collision(t.with_files([("/a.html".into(), g("b"))], ConflictPolicy::Overwrite, None));
        collision(t.with_files([("/a.html".into(), g("A"))], ConflictPolicy::Error, None));
        assert_eq!(t.with_files([("/A.html".into(), g("A"))], ConflictPolicy::Error, None).unwrap().len(), 1);
        assert_eq!(t.with_files([("/A.html".into(), g("B"))], ConflictPolicy::Error, None).unwrap_err().code, E_CONFLICT);

        let d = Tree::from_files([("/Docs/x".to_string(), g("x"))]).unwrap();
        assert_eq!(
            collision(d.with_files([("/docs/y".into(), g("y"))], ConflictPolicy::Error, None)),
            "cannot write '/docs/y': '/docs' differs only in case from '/Docs' (created by Sitefile:1 COPY x); case-insensitive file systems cannot hold both"
        );
        assert!(collision(d.with_files([("/docs".into(), g("y"))], ConflictPolicy::Overwrite, None)).starts_with("'/docs' differs only in case from '/Docs'"));
        collision(d.with_files([("/DOCS/x/z".into(), g("y"))], ConflictPolicy::Error, None));
        let u = Tree::from_files([("/readme".to_string(), g("r"))]).unwrap();
        assert!(collision(u.with_files([("/README/a".into(), g("a"))], ConflictPolicy::Error, None)).contains("'/README' differs only in case from '/readme'"));
        assert_eq!(d.with_files([("/Docs/y".into(), g("y"))], ConflictPolicy::Error, None).unwrap().len(), 2);
        let s = Tree::from_files([("/Σ.html".to_string(), g("s"))]).unwrap();
        collision(s.with_files([("/ς.html".into(), g("s"))], ConflictPolicy::Error, None));
        collision(Tree::from_files([("/a".to_string(), g("1")), ("/A".to_string(), g("2"))]));
    }
    #[test]
    fn paths_are_nfc() {
        let nfc = "/caf\u{e9}/index.html";
        let nfd = "/cafe\u{301}/index.html";
        let a = Tree::from_files([(nfc.to_string(), g("x"))]).unwrap();
        let b = Tree::from_files([(nfd.to_string(), g("x"))]).unwrap();
        assert_eq!(b.entries().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec![nfc]);
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.with_files([(nfd.into(), g("x"))], ConflictPolicy::Error, None).unwrap().len(), 1);
        assert_eq!(a.with_files([(nfd.into(), g("y"))], ConflictPolicy::Error, None).unwrap_err().code, E_CONFLICT);
        collision(a.with_files([("/CAFE\u{301}/index.html".into(), g("x"))], ConflictPolicy::Error, None));
    }
    #[test]
    fn subtree_and_digest_format() {
        let t = Tree::from_files([("/x/a".to_string(), f("1")), ("/x/b".to_string(), f("2")), ("/y".to_string(), f("3"))]).unwrap();
        let s = t.subtree("/x");
        assert_eq!(s.entries().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["/a", "/b"]);
        assert!(s.digest().starts_with("sha256:"));
    }
}
