// PRECOMPRESS (docs/LANGUAGE.md, "Precompressed sidecars"): write `<file>.gz` next to compressible files,
// for hosts and servers that serve them in place of the file (nginx gzip_static, Caddy precompressed, sirv).
// Ported as rust/src/precompress.rs: both use the shared deflate encoder (src/deflate.ts), so the sidecars
// are byte-identical.
import type { SourceSpan } from './errors.js';
import { gzip } from './deflate.js';
import { globMatches } from './fingerprint.js';
import { makeFile, type FileNode, type Tree } from './tree.js';

/** Extensions (lowercase, after the last dot) of the file types PRECOMPRESS compresses: text, scripts, data, fonts without their own compression. */
export const COMPRESSIBLE = new Set([
  'html', 'htm', 'css', 'js', 'mjs', 'cjs', 'json', 'map', 'xml', 'svg', 'txt', 'md', 'csv', 'tsv',
  'webmanifest', 'rss', 'atom', 'ics', 'vtt', 'yaml', 'yml', 'toml', 'wasm', 'ttf', 'otf', 'eot', 'ico',
]);

/** Whether a path names a compressible file type (by its extension, ASCII case-insensitive). */
export function compressible(path: string): boolean {
  const base = path.slice(path.lastIndexOf('/') + 1);
  const dot = base.lastIndexOf('.');
  return dot > 0 && COMPRESSIBLE.has(base.slice(dot + 1).toLowerCase());
}

export interface PrecompressOp {
  readonly patterns: readonly string[];
  readonly min: number;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

/**
 * The tree with a `<path>.gz` sidecar (gzip, fixed header) for every file that matches a pattern, is
 * compressible, has at least `min` bytes, and compresses to fewer bytes than it has. Sidecars keep the
 * file's mode and get the origin `origin`; one that would replace a different file is a conflict unless
 * `overwrite`. Warns when no file matches the patterns.
 */
export function precompress(tree: Tree, op: PrecompressOp, origin: string, warn: (m: string) => void): Tree {
  const matched = tree.entries().filter(([p]) => op.patterns.some((pat) => globMatches(pat, p)));
  if (matched.length === 0) warn(`${origin}: no files match ${op.patterns.join(' ')}`);
  const files: Array<[string, FileNode]> = [];
  for (const [p, node] of matched) {
    if (!compressible(p) || node.bytes.byteLength < op.min) continue;
    const gz = gzip(node.bytes);
    if (gz.byteLength < node.bytes.byteLength) files.push([p + '.gz', makeFile(gz, origin, node.mode)]);
  }
  return files.length ? tree.withFiles(files, op.overwrite ? 'overwrite' : 'error', op.span) : tree;
}
