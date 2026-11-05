//! Virtual (artifact) and local (context-relative) path rules, matching src/paths.ts.

use crate::error::{err, ErrorCode::*, Result, SourceSpan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualPath {
    /// Normalized absolute POSIX path inside the artifact, `/` for the root.
    pub path: String,
    /// The original ended with `/` (or was the root).
    pub trailing_slash: bool,
}

/// Must be absolute; `.` and repeated slashes collapse; any `..` is rejected.
pub fn normalize_virtual(raw: &str, span: Option<&SourceSpan>, what: &str) -> Result<VirtualPath> {
    if raw.contains('\0') {
        return err(E_PATH, format!("{what} contains a NUL byte"), span);
    }
    if !raw.starts_with('/') {
        return err(E_PATH, format!("{what} '{raw}' must be an absolute artifact path (start with '/')"), span);
    }
    let mut segs: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return err(E_PATH, format!("{what} '{raw}' must not contain '..'"), span);
        }
        segs.push(seg);
    }
    let path = format!("/{}", segs.join("/"));
    let trailing_slash = raw.ends_with('/') || path == "/";
    Ok(VirtualPath { path, trailing_slash })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPath {
    /// Relative POSIX path, `""` for the context root.
    pub path: String,
    pub trailing_slash: bool,
}

/// Relative to the build context; absolute paths and `..` escapes are refused.
pub fn normalize_local(raw: &str, span: Option<&SourceSpan>) -> Result<LocalPath> {
    if raw.contains('\0') {
        return err(E_PATH, "path contains a NUL byte", span);
    }
    let b = raw.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\');
    if raw.starts_with('/') || raw.starts_with('\\') || drive {
        return err(E_PATH_ESCAPE, format!("local path '{raw}' must be relative to the build context"), span);
    }
    // posix.normalize semantics
    let mut out: Vec<&str> = Vec::new();
    let mut ups = 0usize;
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    ups += 1;
                }
            }
            s => out.push(s),
        }
    }
    if ups > 0 {
        return err(E_PATH_ESCAPE, format!("local path '{raw}' escapes the build context"), span);
    }
    Ok(LocalPath { path: out.join("/"), trailing_slash: raw.ends_with('/') })
}

pub fn join_virtual(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
}

pub fn basename_virtual(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

/// Ancestors of a virtual path, excluding the root and the path itself.
pub fn ancestors_virtual(p: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (i, c) in p.char_indices().skip(1) {
        if c == '/' {
            out.push(&p[..i]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_paths() {
        assert_eq!(normalize_virtual("/a//./b/", None, "p").unwrap(), VirtualPath { path: "/a/b".into(), trailing_slash: true });
        assert_eq!(normalize_virtual("/", None, "p").unwrap().path, "/");
        assert!(normalize_virtual("a", None, "p").is_err());
        assert!(normalize_virtual("/a/../b", None, "p").is_err());
    }
    #[test]
    fn local_paths() {
        assert_eq!(normalize_local("./a/../b/", None).unwrap(), LocalPath { path: "b".into(), trailing_slash: true });
        assert_eq!(normalize_local(".", None).unwrap().path, "");
        assert_eq!(normalize_local("../x", None).unwrap_err().code, crate::error::ErrorCode::E_PATH_ESCAPE);
        assert_eq!(normalize_local("/etc", None).unwrap_err().code, crate::error::ErrorCode::E_PATH_ESCAPE);
    }
    #[test]
    fn ancestors() {
        assert_eq!(ancestors_virtual("/a/b/c.txt"), vec!["/a", "/a/b"]);
        assert!(ancestors_virtual("/a").is_empty());
    }
}
