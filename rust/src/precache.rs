//! PRECACHE (port of `src/precache.ts`; docs/LANGUAGE.md, "Precache manifest"): a Workbox-style list of
//! the files a service worker precaches. The manifest bytes and messages match the TypeScript.

use crate::digest::hex_of;
use crate::error::{ErrorCode::*, Result, SitefileError, SourceSpan};
use crate::fingerprint::{glob_matches, HASH_LENGTH};
use crate::links::encode_path;
use crate::tree::{make_file, ConflictPolicy, Tree};
use std::collections::HashSet;

/// Where `injectManifest` puts the manifest in a service worker (Workbox's default `injectionPoint`).
pub const INJECTION_POINT: &str = "self.__WB_MANIFEST";

pub struct PrecacheOp<'a> {
    pub patterns: &'a [String],
    pub dest: Option<&'a str>,
    pub into: Option<&'a str>,
    pub root: &'a str,
    pub overwrite: bool,
    pub span: &'a SourceSpan,
}

/// The manifest entries (url, revision; None for a fingerprinted file), sorted by URL.
pub fn precache_entries(
    tree: &Tree,
    op: &PrecacheOp,
    fingerprinted: &HashSet<String>,
) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for (p, node) in tree.entries() {
        if Some(p.as_str()) == op.dest
            || Some(p.as_str()) == op.into
            || !op.patterns.iter().any(|pat| glob_matches(pat, p))
        {
            continue;
        }
        let below = if op.root == "/" {
            Some(p.as_str())
        } else {
            p.strip_prefix(op.root).filter(|r| r.starts_with('/'))
        };
        let Some(below) = below else {
            continue;
        };
        let revision =
            (!fingerprinted.contains(p)).then(|| hex_of(&node.digest)[..HASH_LENGTH].to_string());
        out.push((encode_path(below), revision));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The manifest as JSON: one entry per line, two-space indented, with a final newline (`[]` when empty).
pub fn precache_json(entries: &[(String, Option<String>)]) -> String {
    if entries.is_empty() {
        return "[]\n".into();
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|(url, rev)| {
            format!(
                "  {{\"url\": {}, \"revision\": {}}}",
                serde_json::to_string(url).unwrap(),
                serde_json::to_string(rev).unwrap()
            )
        })
        .collect();
    format!("[\n{}\n]\n", lines.join(",\n"))
}

/// See `precache` in the TypeScript module.
pub fn precache(
    tree: Tree,
    op: &PrecacheOp,
    fingerprinted: &HashSet<String>,
    origin: &str,
    warn: &dyn Fn(&str),
) -> Result<Tree> {
    let entries = precache_entries(&tree, op, fingerprinted);
    if entries.is_empty() {
        warn(&format!("{origin}: no files match {}", op.patterns.join(" ")));
    }
    let json = precache_json(&entries);
    let mut files = Vec::new();
    if let Some(d) = op.dest {
        files.push((d.to_string(), make_file(json.clone().into_bytes(), origin, 0o644)));
    }
    if let Some(sw_path) = op.into {
        let Some(sw) = tree.get(sw_path) else {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("PRECACHE INTO {sw_path}: there is no such file in the stage (COPY the service worker in first)"),
                op.span,
            ));
        };
        let Ok(text) = std::str::from_utf8(&sw.bytes) else {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("PRECACHE INTO {sw_path}: the service worker is not UTF-8 text"),
                op.span,
            ));
        };
        let Some(at) = text.find(INJECTION_POINT) else {
            return Err(SitefileError::at(
                E_VALIDATION,
                format!("PRECACHE INTO {sw_path}: the service worker has no {INJECTION_POINT} to replace with the manifest"),
                op.span,
            ));
        };
        let injected = format!(
            "{}{}{}",
            &text[..at],
            json.trim_end(),
            &text[at + INJECTION_POINT.len()..]
        );
        files.push((sw_path.to_string(), make_file(injected.into_bytes(), origin, sw.mode)));
    }
    let mut out = tree;
    for (p, node) in files {
        let policy = if Some(p.as_str()) == op.into || op.overwrite {
            ConflictPolicy::Overwrite
        } else {
            ConflictPolicy::Error
        };
        out = out.with_files([(p, node)], policy, Some(op.span))?;
    }
    Ok(out)
}
