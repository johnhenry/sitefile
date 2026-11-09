// PRECOMPRESS (docs/LANGUAGE.md, "Precompressed sidecars"): write `<file>.gz` and/or `<file>.br` next to
// compressible files, for hosts and servers that serve them in place of the file (nginx gzip_static,
// Caddy precompressed, sirv). Ported as rust/src/precompress.rs. gzip uses the shared deflate encoder
// (src/deflate.ts); br uses node:zlib's Brotli, and the Rust port compiles the same C encoder at the
// same version (rust/vendor/brotli, rust/src/brotli.rs) with the same settings, so the sidecars are
// byte-identical.
import { brotliCompressSync, constants } from 'node:zlib';
import type { SourceSpan } from './errors.js';
import { gzip } from './deflate.js';
import type { PrecompressFormat } from './language/ast.js';
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

/** The Brotli encoder version rust/vendor/brotli holds; br sidecars match the Rust port's only while Node bundles it. */
export const BROTLI_VERSION = '1.2.0';

/** `data` as Brotli: quality 11, window 22 (4 MiB), generic mode, one FINISH over the whole input (rust/src/brotli.rs). */
export function brotli(data: Uint8Array): Uint8Array {
  return new Uint8Array(
    brotliCompressSync(data, {
      params: {
        [constants.BROTLI_PARAM_QUALITY]: 11,
        [constants.BROTLI_PARAM_LGWIN]: 22,
        [constants.BROTLI_PARAM_MODE]: constants.BROTLI_MODE_GENERIC,
      },
    }),
  );
}

const ENCODERS: Record<PrecompressFormat, { readonly ext: string; readonly encode: (b: Uint8Array) => Uint8Array }> = {
  gzip: { ext: '.gz', encode: gzip },
  br: { ext: '.br', encode: brotli },
};

/** The file a sidecar compresses: `path` without its `.gz` or `.br`. */
export function sidecarSource(path: string): string {
  return path.endsWith('.gz') || path.endsWith('.br') ? path.slice(0, -3) : path;
}

export interface PrecompressOp {
  readonly formats: readonly PrecompressFormat[];
  readonly patterns: readonly string[];
  readonly min: number;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

/**
 * The tree with a sidecar per format (`<path>.gz`: gzip, fixed header; `<path>.br`: Brotli) for every file
 * that matches a pattern, is compressible and has at least `min` bytes, when that format compresses it to
 * fewer bytes than it has. Sidecars keep the
 * file's mode and get the origin `origin`; one that would replace a different file is a conflict unless
 * `overwrite`. Warns when no file matches the patterns.
 */
export function precompress(tree: Tree, op: PrecompressOp, origin: string, warn: (m: string) => void): Tree {
  const matched = tree.entries().filter(([p]) => op.patterns.some((pat) => globMatches(pat, p)));
  if (matched.length === 0) warn(`${origin}: no files match ${op.patterns.join(' ')}`);
  const files: Array<[string, FileNode]> = [];
  for (const [p, node] of matched) {
    if (!compressible(p) || node.bytes.byteLength < op.min) continue;
    for (const format of op.formats) {
      const { ext, encode } = ENCODERS[format];
      const out = encode(node.bytes);
      if (out.byteLength < node.bytes.byteLength) files.push([p + ext, makeFile(out, origin, node.mode)]);
    }
  }
  return files.length ? tree.withFiles(files, op.overwrite ? 'overwrite' : 'error', op.span) : tree;
}
