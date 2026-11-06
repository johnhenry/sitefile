//! Virtual (artifact) and local (context-relative) path rules, matching src/paths.ts.

use crate::casefold_data::CASEFOLD_RUNS;
use crate::error::{err, ErrorCode::*, Result, SourceSpan};
use icu_normalizer::ComposingNormalizerBorrowed;
use std::borrow::Cow;

/// Unicode NFC. Every virtual path (Sitefile operands, names read from the
/// context, archive entries, plugin outputs) is stored in NFC, so NFC and NFD
/// spellings of a name are the same path on every OS.
pub fn nfc(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        Cow::Borrowed(s)
    } else {
        ComposingNormalizerBorrowed::new_nfc().normalize(s)
    }
}

fn fold_char(c: char) -> char {
    let cp = c as u32;
    let i = CASEFOLD_RUNS.partition_point(|&(_, end, _, _)| end < cp);
    match CASEFOLD_RUNS.get(i) {
        Some(&(start, _, delta, step)) if cp >= start && (cp - start) % step == 0 => {
            char::from_u32((cp as i64 + delta as i64) as u32).unwrap_or(c)
        }
        _ => c,
    }
}

/// Unicode simple case folding (CaseFolding.txt status C and S, one code
/// point to one code point; no full or Turkic mappings), applied code point
/// by code point. Two paths with the same fold differ only in case.
/// `src/paths.ts` uses the same generated table (`tools/gen-casefold.mjs`).
pub fn fold_case(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.chars().map(fold_char).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualPath {
    /// Normalized absolute POSIX path inside the artifact, `/` for the root.
    pub path: String,
    /// The original ended with `/` (or was the root).
    pub trailing_slash: bool,
}

/// Must be absolute; `.` and repeated slashes collapse; any `..` is rejected;
/// the result is NFC.
pub fn normalize_virtual(raw: &str, span: Option<&SourceSpan>, what: &str) -> Result<VirtualPath> {
    let raw = &*nfc(raw);
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
/// The result is NFC; `LocalContext` finds the file whatever normalization its
/// on-disk name uses.
pub fn normalize_local(raw: &str, span: Option<&SourceSpan>) -> Result<LocalPath> {
    let raw = &*nfc(raw);
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
    let name = nfc(name);
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
    fn unicode() {
        assert_eq!(nfc("cafe\u{301}"), "caf\u{e9}");
        assert_eq!(normalize_virtual("/cafe\u{301}/x", None, "p").unwrap().path, "/caf\u{e9}/x");
        assert_eq!(normalize_local("./cafe\u{301}.txt", None).unwrap().path, "caf\u{e9}.txt");
        assert_eq!(join_virtual("/", "cafe\u{301}"), "/caf\u{e9}");
        assert_eq!(fold_case("/Docs/A.HTML"), "/docs/a.html");
        assert_eq!(fold_case("Σ"), fold_case("ς"));
        assert_eq!(fold_case("σ"), fold_case("ς"));
        assert_eq!(fold_case("\u{212a}"), "k");
        assert_eq!(fold_case("\u{17f}"), "s");
        assert_eq!(fold_case("ẞ"), "ß");
        assert_ne!(fold_case("ß"), fold_case("ss"));
        assert_eq!(fold_case("\u{130}"), "\u{130}");
        assert_eq!(fold_case("\u{13a0}"), "\u{13a0}");
        assert_eq!(fold_case("\u{ab70}"), "\u{13a0}");
        assert_eq!(fold_case("\u{10400}"), "\u{10428}");
        assert_eq!(fold_case("Ǆ"), "ǆ");
        assert_eq!(fold_case("ǅ"), "ǆ");
    }
    // Differential checks against the TypeScript implementation: the same
    // digests are asserted in test/path-safety.test.ts, which computes them
    // with String.prototype.normalize and src/paths.ts foldCase. A change in
    // either (a Unicode version bump in Node's ICU or in icu_normalizer_data, a
    // regenerated case folding table) shows up as a mismatch in both suites.
    pub(crate) const NFC_DIGEST: &str = "sha256:fe17995393e0d09199fd78c8aedca5d57273bdde642335534485565355b364a1";

    /// Combining marks that icu_normalizer_data 2.4 assigns a combining class
    /// and Node's ICU (78, in Node 26) still treats as unassigned: NFC differs for
    /// sequences containing them (docs/RUST.md, "Known differences").
    pub(crate) fn nfc_unsettled(c: u32) -> bool {
        (0x5c8..=0x5c9).contains(&c) || (0x1ade..=0x1af0).contains(&c)
    }
    pub(crate) const FOLD_DIGEST: &str = "sha256:e6e9bc02adedd2b908bbc65d7996e3a36e68294e4dde23c0f04f7aa240e69659";

    fn scalars() -> impl Iterator<Item = char> {
        (0..=0x10ffffu32).filter_map(char::from_u32)
    }

    #[test]
    fn nfc_matches_typescript() {
        let mut s = String::new();
        let mut add = |t: String| {
            s.push_str(&nfc(&t));
            s.push('\n');
        };
        for c in scalars().filter(|&c| !nfc_unsettled(c as u32)) {
            add(c.to_string());
        }
        for b in (0..0x2000u32).filter(|&b| !nfc_unsettled(b)) {
            for m in 0x300..=0x36fu32 {
                add([char::from_u32(b).unwrap(), char::from_u32(m).unwrap()].iter().collect());
            }
        }
        for l in 0x1100..0x1113u32 {
            for v in 0x1161..0x1176u32 {
                let (l, v) = (char::from_u32(l).unwrap(), char::from_u32(v).unwrap());
                add([l, v].iter().collect());
                add([l, v, '\u{11a8}'].iter().collect());
            }
        }
        assert_eq!(crate::digest::sha256(s.as_bytes()), NFC_DIGEST);
    }

    #[test]
    fn fold_matches_typescript() {
        let mut s = String::new();
        for c in scalars() {
            s.push_str(&fold_case(&c.to_string()));
            s.push('\n');
        }
        assert_eq!(crate::digest::sha256(s.as_bytes()), FOLD_DIGEST);
        for w in CASEFOLD_RUNS.windows(2) {
            assert!(w[0].1 < w[1].0, "runs are sorted and disjoint");
        }
    }

    #[test]
    fn ancestors() {
        assert_eq!(ancestors_virtual("/a/b/c.txt"), vec!["/a", "/a/b"]);
        assert!(ancestors_virtual("/a").is_empty());
    }
}
