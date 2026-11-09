//! PRECOMPRESS (port of `src/precompress.ts`; docs/LANGUAGE.md, "Precompressed sidecars"): write
//! `<file>.gz` and/or `<file>.br` next to compressible files. gzip uses the shared deflate encoder
//! (`crate::deflate`); br uses the vendored C Brotli encoder at node:zlib's version and settings
//! (`crate::brotli`), so the sidecars are byte-identical.

use crate::brotli::brotli_compress;
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

/// The file a sidecar compresses: `path` without its `.gz` or `.br`.
pub fn sidecar_source(path: &str) -> &str {
    path.strip_suffix(".gz")
        .or_else(|| path.strip_suffix(".br"))
        .unwrap_or(path)
}

pub struct PrecompressOp<'a> {
    /// `gzip` and/or `br`, in the order given.
    pub formats: &'a [String],
    pub patterns: &'a [String],
    pub min: u64,
    pub overwrite: bool,
    pub span: &'a SourceSpan,
}

/// The tree with a `<path>.gz` / `<path>.br` sidecar per format for every matching, compressible file
/// of at least `min` bytes whose compressed form is smaller (see `precompress` in the TypeScript module). Warns when nothing matches.
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
        for format in op.formats {
            let (ext, out) = match format.as_str() {
                "br" => ("br", brotli_compress(&node.bytes)),
                _ => ("gz", gzip(&node.bytes)),
            };
            if out.len() < node.bytes.len() {
                files.push((format!("{p}.{ext}"), make_file(out, origin, node.mode)));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn br_and_gzip_sidecars_in_the_order_given_each_only_when_smaller() {
        let big = "body { color: red; }\n".repeat(100);
        let tree = Tree::from_files([
            (
                "/a.css".to_string(),
                make_file(big.as_bytes().to_vec(), "t", 0o644),
            ),
            (
                "/r.txt".to_string(),
                make_file(b"x7Kq".to_vec(), "t", 0o644),
            ),
        ])
        .unwrap();
        let span = SourceSpan {
            file: "Sitefile".into(),
            line: 2,
            column: 1,
        };
        let formats = ["br".to_string(), "gzip".to_string()];
        let patterns = ["/**".to_string()];
        let op = PrecompressOp {
            formats: &formats,
            patterns: &patterns,
            min: 4,
            overwrite: false,
            span: &span,
        };
        let out = precompress(tree, &op, "Sitefile:2 PRECOMPRESS", &|_| {}).unwrap();
        let paths: Vec<&String> = out.entries().map(|(p, _)| p).collect();
        assert_eq!(paths, ["/a.css", "/a.css.br", "/a.css.gz", "/r.txt"]);
        assert_eq!(
            &out.get("/a.css.br").unwrap().bytes[..],
            &brotli_compress(big.as_bytes())[..]
        );
        assert_eq!(sidecar_source("/a.css.br"), "/a.css");
        assert_eq!(sidecar_source("/a.css.gz"), "/a.css");
    }
}
