//! Immutable filesystem trees (src/tree.ts). Files are shared via `Arc`, so
//! copying a tree copies pointers, not bytes.

use crate::digest::sha256;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::paths::ancestors_virtual;
use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, Clone, Default)]
pub struct Tree {
    files: BTreeMap<String, Node>,
    dirs: BTreeSet<String>,
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

    /// Add files. Identical content (digest and mode) keeps the first owner;
    /// different content fails with E_CONFLICT unless `Overwrite`; a file can
    /// never replace a directory or the reverse.
    pub fn with_files<I>(&self, additions: I, policy: ConflictPolicy, span: Option<&SourceSpan>) -> Result<Tree>
    where
        I: IntoIterator<Item = (String, Node)>,
    {
        let mut files = self.files.clone();
        let mut dirs = self.dirs.clone();
        for (path, node) in additions {
            if path == "/" || dirs.contains(&path) {
                return Err(SitefileError::new(E_CONFLICT, format!("cannot write file '{path}': it is a directory"), span.cloned()));
            }
            for a in ancestors_virtual(&path) {
                if let Some(f) = files.get(a) {
                    return Err(SitefileError::new(
                        E_CONFLICT,
                        format!("cannot write '{path}': '{a}' is a file (written by {})", f.origin),
                        span.cloned(),
                    ));
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
            for a in ancestors_virtual(&path) {
                dirs.insert(a.to_string());
            }
            files.insert(path, node);
        }
        Ok(Tree { files, dirs })
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
    #[test]
    fn subtree_and_digest_format() {
        let t = Tree::from_files([("/x/a".to_string(), f("1")), ("/x/b".to_string(), f("2")), ("/y".to_string(), f("3"))]).unwrap();
        let s = t.subtree("/x");
        assert_eq!(s.entries().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["/a", "/b"]);
        assert!(s.digest().starts_with("sha256:"));
    }
}
