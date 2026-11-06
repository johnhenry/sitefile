//! FINGERPRINT (port of `src/fingerprint.ts`; docs/LANGUAGE.md, "Fingerprinting"): rename assets to
//! content-hashed names and rewrite the HTML and CSS that reference them. The tree, manifest and
//! messages match the TypeScript implementation.

use crate::digest::{hex_of, sha256};
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::links::{reference_kind, resolve_reference, rewrite_references, scan_file, Reference, Target};
use crate::paths::nfc;
use crate::tree::{make_file, ConflictPolicy, Node, Tree};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Hex digits of the SHA-256 of the content in a fingerprinted name.
pub const HASH_LENGTH: usize = 10;

/// Names FINGERPRINT never renames (besides HTML pages): files hosts look up by name.
const FIXED_NAMES: [&str; 2] = ["_redirects", "_headers"];

/// Check a FINGERPRINT pattern: an absolute path (NFC) with no control characters; a trailing `/` means `/**`.
pub fn check_pattern(raw: &str, span: &SourceSpan) -> Result<String> {
    if raw.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err(SitefileError::at(E_VALIDATION, "FINGERPRINT pattern must not contain control characters", span));
    }
    if !raw.starts_with('/') {
        return Err(SitefileError::at(
            E_VALIDATION,
            format!("FINGERPRINT pattern '{raw}' must be an absolute path starting with '/' (e.g. /assets/ or /**/*.css)"),
            span,
        ));
    }
    let p = nfc(raw).into_owned();
    Ok(if p.ends_with('/') { p + "**" } else { p })
}

fn segment_matches(pat: &[char], s: &[char]) -> bool {
    match pat.split_first() {
        None => s.is_empty(),
        Some(('*', rest)) => (0..=s.len()).any(|k| segment_matches(rest, &s[k..])),
        Some((c, rest)) => !s.is_empty() && (*c == '?' || *c == s[0]) && segment_matches(rest, &s[1..]),
    }
}

/// Whether a tree path matches a pattern: segment by segment, `**` matching any number of segments.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<Vec<char>> = pattern.split('/').skip(1).map(|s| s.chars().collect()).collect();
    let segs: Vec<Vec<char>> = path.split('/').skip(1).map(|s| s.chars().collect()).collect();
    fn go(pat: &[Vec<char>], segs: &[Vec<char>]) -> bool {
        match pat.split_first() {
            None => segs.is_empty(),
            Some((p, rest)) if p.as_slice() == ['*', '*'] => (0..=segs.len()).any(|k| go(rest, &segs[k..])),
            Some((p, rest)) => !segs.is_empty() && segment_matches(p, &segs[0]) && go(rest, &segs[1..]),
        }
    }
    go(&pat, &segs)
}

/// `name.<hash>.ext` (the hash before the last extension), or `name.<hash>` without one.
pub fn hashed_name(path: &str, hash: &str) -> String {
    let slash = path.rfind('/').map_or(0, |k| k + 1);
    let base = &path[slash..];
    let named = match base.rfind('.') {
        Some(dot) if dot > 0 => format!("{}.{hash}{}", &base[..dot], &base[dot..]),
        _ => format!("{base}.{hash}"),
    };
    format!("{}{named}", &path[..slash])
}

fn content_hash(bytes: &[u8]) -> String {
    hex_of(&sha256(bytes))[..HASH_LENGTH].to_string()
}

fn renamable(path: &str) -> bool {
    reference_kind(path) != Some("html") && !FIXED_NAMES.contains(&&path[path.rfind('/').map_or(0, |k| k + 1)..])
}

/// The manifest: a JSON object of old path -> new path, sorted, two-space indented, with a final newline.
pub fn manifest_json(renames: &[(String, String)]) -> String {
    if renames.is_empty() {
        return "{}\n".into();
    }
    let lines: Vec<String> = renames
        .iter()
        .map(|(a, b)| format!("  {}: {}", serde_json::to_string(a).unwrap(), serde_json::to_string(b).unwrap()))
        .collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

pub struct FingerprintOp<'a> {
    pub patterns: &'a [String],
    pub manifest: Option<&'a str>,
    /// The directory the site is served from (the EXPORT path): absolute references resolve below it.
    pub root: &'a str,
    pub overwrite: bool,
    pub span: &'a SourceSpan,
}

/// Fingerprint the files of `tree` that match the patterns (see `fingerprint` in the TypeScript module).
/// Returns the tree and the renames (old path, new path), sorted by old path.
pub fn fingerprint(tree: &Tree, op: &FingerprintOp, origin: &str, warn: &dyn Fn(&str)) -> Result<(Tree, Vec<(String, String)>)> {
    let policy = if op.overwrite { ConflictPolicy::Overwrite } else { ConflictPolicy::Error };
    let entries: Vec<(&String, &Node)> = tree.entries().collect();
    let selected: Vec<&String> = entries
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| renamable(p) && Some(p.as_str()) != op.manifest && op.patterns.iter().any(|pat| glob_matches(pat, p)))
        .collect();
    if selected.is_empty() {
        warn(&format!("{origin}: no files match {}", op.patterns.join(" ")));
    }
    let is_selected: HashSet<&str> = selected.iter().map(|p| p.as_str()).collect();
    let by_path: HashMap<&str, &Node> = entries.iter().map(|(p, n)| (p.as_str(), *n)).collect();

    struct State<'t> {
        by_path: HashMap<&'t str, &'t Node>,
        is_selected: HashSet<&'t str>,
        renamed: HashMap<String, String>,
        content: HashMap<String, Vec<u8>>,
        visiting: HashSet<String>,
        scans: HashMap<String, Option<(String, Vec<Reference>)>>,
    }
    fn scan<'s>(st: &'s mut State, p: &str) -> &'s Option<(String, Vec<Reference>)> {
        if !st.scans.contains_key(p) {
            let node = st.by_path[p];
            let s = scan_file(p, &node.bytes);
            st.scans.insert(p.to_string(), s);
        }
        &st.scans[p]
    }
    fn visit(st: &mut State, p: &str, stack: &mut Vec<String>, root: &str, span: &SourceSpan) -> Result<()> {
        if st.renamed.contains_key(p) {
            return Ok(());
        }
        if st.visiting.contains(p) {
            let at = stack.iter().position(|s| s == p).unwrap_or(0);
            let mut cycle: Vec<String> = stack[at..].to_vec();
            cycle.push(p.to_string());
            return Err(SitefileError::at(
                E_VALIDATION,
                format!(
                    "FINGERPRINT: {} is a cycle of references, so no file's name can include the hash of the files it references; leave one of them out of FINGERPRINT",
                    cycle.join(" -> ")
                ),
                span,
            ));
        }
        st.visiting.insert(p.to_string());
        let mut bytes: Vec<u8> = st.by_path[p].bytes.to_vec();
        if reference_kind(p) == Some("css") {
            if let Some((text, refs)) = scan(st, p).clone() {
                for r in &refs {
                    if let Target::Internal { path, dir: false } = resolve_reference(&r.url, p, root) {
                        if st.is_selected.contains(path.as_str()) {
                            stack.push(p.to_string());
                            let res = visit(st, &path, stack, root, span);
                            stack.pop();
                            res?;
                        }
                    }
                }
                if let Some(t) = rewrite_references(&text, &refs, p, root, |q| st.renamed.get(q).cloned()) {
                    bytes = t.into_bytes();
                }
            }
        }
        st.renamed.insert(p.to_string(), hashed_name(p, &content_hash(&bytes)));
        st.content.insert(p.to_string(), bytes);
        st.visiting.remove(p);
        Ok(())
    }

    let mut st = State { by_path, is_selected, renamed: HashMap::new(), content: HashMap::new(), visiting: HashSet::new(), scans: HashMap::new() };
    for p in &selected {
        visit(&mut st, p, &mut Vec::new(), op.root, op.span)?;
    }

    let mut files: Vec<(String, Node)> = Vec::new();
    for (p, node) in &entries {
        if st.is_selected.contains(p.as_str()) {
            continue;
        }
        let rewritten = if reference_kind(p).is_some() {
            match scan(&mut st, p).clone() {
                Some((text, refs)) => rewrite_references(&text, &refs, p, op.root, |q| st.renamed.get(q).cloned()),
                None => None,
            }
        } else {
            None
        };
        files.push(((*p).clone(), match rewritten {
            Some(t) => make_file(t.into_bytes(), origin, node.mode),
            None => (*node).clone(),
        }));
    }
    let renames: Vec<(String, String)> = selected.iter().map(|p| ((*p).clone(), st.renamed[p.as_str()].clone())).collect();
    let mut out = Tree::from_files(files)?;
    let added: Vec<(String, Node)> = selected
        .iter()
        .map(|p| (st.renamed[p.as_str()].clone(), make_file(st.content.remove(p.as_str()).unwrap(), origin, by_path_mode(&entries, p))))
        .collect();
    out = out.with_files(added, policy, Some(op.span))?;
    if let Some(m) = op.manifest {
        out = out.with_files([(m.to_string(), make_file(manifest_json(&renames).into_bytes(), origin, 0o644))], policy, Some(op.span))?;
    }
    Ok((out, renames))
}

fn by_path_mode(entries: &[(&String, &Node)], p: &str) -> u32 {
    entries.iter().find(|(q, _)| q.as_str() == p).map_or(0o644, |(_, n)| n.mode)
}

/// Compose rename maps: `earlier` then `later` (a renamed file renamed again maps to its last name).
pub fn compose_renames(earlier: &BTreeMap<String, String>, later: &[(String, String)]) -> BTreeMap<String, String> {
    let next: HashMap<&str, &str> = later.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let intermediate: HashSet<&str> = earlier.values().map(String::as_str).collect();
    let mut out = BTreeMap::new();
    for (a, b) in earlier {
        out.insert(a.clone(), next.get(b.as_str()).map_or_else(|| b.clone(), |s| s.to_string()));
    }
    for (a, b) in later {
        if !intermediate.contains(a.as_str()) {
            out.insert(a.clone(), b.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_matches("/assets/**", "/assets/a/b.css"));
        assert!(glob_matches("/**/*.css", "/a.css"));
        assert!(glob_matches("/**/*.css", "/x/y/a.css"));
        assert!(!glob_matches("/*.css", "/x/a.css"));
        assert!(glob_matches("/a?.js", "/ab.js"));
        assert!(!glob_matches("/a?.js", "/a.js"));
        assert!(glob_matches("/é*", "/éa"));
    }

    #[test]
    fn names() {
        assert_eq!(hashed_name("/a/app.min.js", "0123456789"), "/a/app.min.0123456789.js");
        assert_eq!(hashed_name("/LICENSE", "0123456789"), "/LICENSE.0123456789");
        assert_eq!(hashed_name("/.env", "0123456789"), "/.env.0123456789");
    }
}
