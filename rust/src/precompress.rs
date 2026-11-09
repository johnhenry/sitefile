//! PRECOMPRESS (port of `src/precompress.ts`; docs/LANGUAGE.md, "Precompressed sidecars"): write
//! `<file>.gz` next to compressible files. Both implementations use the shared deflate encoder
//! (`crate::deflate`), so the sidecars are byte-identical.

use crate::deflate::gzip;
use crate::error::{Result, SourceSpan};
use crate::fingerprint::glob_matches;
use crate::tree::{make_file, ConflictPolicy, Node, Tree};

/// Extensions (lowercase, after the last dot) of the file types PRECOMPRESS compresses.
pub const COMPRESSIBLE: [&str; 27] = [
    "html",
    "htm",
    "css",
    "js",
    "mjs",
    "cjs",
    "json",
    "map",
    "xml",
    "svg",
    "txt",
    "md",
    "csv",
    "tsv",
    "webmanifest",
    "rss",
    "atom",
    "ics",
    "vtt",
    "yaml",
    "yml",
    "toml",
    "wasm",
    "ttf",
    "otf",
    "eot",
    "ico",
];

/// Whether a path names a compressible file type (by its extension, ASCII case-insensitive).
pub fn compressible(path: &str) -> bool {
    let base = &path[path.rfind('/').map_or(0, |i| i + 1)..];
    match base.rfind('.') {
        Some(dot) if dot > 0 => {
            COMPRESSIBLE.contains(&base[dot + 1..].to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

pub struct PrecompressOp<'a> {
    pub patterns: &'a [String],
    pub min: u64,
    pub overwrite: bool,
    pub span: &'a SourceSpan,
}

/// The tree with a `<path>.gz` sidecar for every matching, compressible file of at least `min` bytes
/// whose gzip is smaller (see `precompress` in the TypeScript module). Warns when nothing matches.
pub fn precompress(
    tree: Tree,
    op: &PrecompressOp,
    origin: &str,
    warn: &dyn Fn(&str),
) -> Result<Tree> {
    let matched: Vec<(&String, &Node)> = tree
        .entries()
        .filter(|(p, _)| op.patterns.iter().any(|pat| glob_matches(pat, p)))
        .collect();
    if matched.is_empty() {
        warn(&format!(
            "{origin}: no files match {}",
            op.patterns.join(" ")
        ));
    }
    let mut files: Vec<(String, Node)> = Vec::new();
    for (p, node) in matched {
        if !compressible(p) || (node.bytes.len() as u64) < op.min {
            continue;
        }
        let gz = gzip(&node.bytes);
        if gz.len() < node.bytes.len() {
            files.push((format!("{p}.gz"), make_file(gz, origin, node.mode)));
        }
    }
    if files.is_empty() {
        return Ok(tree);
    }
    let policy = if op.overwrite {
        ConflictPolicy::Overwrite
    } else {
        ConflictPolicy::Error
    };
    tree.with_files(files, policy, Some(op.span))
}
