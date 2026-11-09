// PRECACHE (docs/LANGUAGE.md, "Precache manifest"): a Workbox-style list of the files a service worker
// precaches, `[{url, revision}]`. Ported as rust/src/precache.rs: both must write the same bytes.
import { createHash } from 'node:crypto';
import { SitefileError, type SourceSpan } from './errors.js';
import { globMatches, HASH_LENGTH } from './fingerprint.js';
import { encodePath } from './links.js';
import { makeFile, type FileNode, type Tree } from './tree.js';

/** Where `injectManifest` puts the manifest in a service worker (Workbox's default `injectionPoint`). */
export const INJECTION_POINT = 'self.__WB_MANIFEST';

export interface PrecacheOp {
  readonly patterns: readonly string[];
  readonly dest?: string;
  readonly into?: string;
  readonly root: string;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

function utf8Compare(a: string, b: string): number {
  return Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
}

/**
 * The manifest entries: every file matching a pattern below `root` (other than the manifest and the
 * service worker), as `{url, revision}` sorted by URL. The URL is the path below `root`, percent-encoded;
 * the revision is the first 10 hex digits of the file's SHA-256, or null for a file a FINGERPRINT renamed
 * (`fingerprinted`: its name already changes with its content).
 */
export function precacheEntries(tree: Tree, op: PrecacheOp, fingerprinted: ReadonlySet<string>): Array<{ url: string; revision: string | null }> {
  const out: Array<{ url: string; revision: string | null }> = [];
  for (const [p, node] of tree.entries()) {
    if (p === op.dest || p === op.into || !op.patterns.some((pat) => globMatches(pat, p))) continue;
    const below = op.root === '/' ? p : p.startsWith(op.root + '/') ? p.slice(op.root.length) : undefined;
    if (below === undefined) continue;
    const revision = fingerprinted.has(p) ? null : createHash('sha256').update(node.bytes).digest('hex').slice(0, HASH_LENGTH);
    out.push({ url: encodePath(below), revision });
  }
  return out.sort((a, b) => utf8Compare(a.url, b.url));
}

/** The manifest as JSON: one entry per line, two-space indented, with a final newline (`[]` when empty). */
export function precacheJson(entries: ReadonlyArray<{ url: string; revision: string | null }>): string {
  if (entries.length === 0) return '[]\n';
  return '[\n' + entries.map((e) => `  {"url": ${JSON.stringify(e.url)}, "revision": ${JSON.stringify(e.revision)}}`).join(',\n') + '\n]\n';
}

const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/**
 * Write the manifest to `dest` and/or replace the first `self.__WB_MANIFEST` in the service worker `into`
 * (a file of the tree, valid UTF-8, holding the placeholder: E_VALIDATION otherwise) with the JSON array.
 * Warns when no file matches.
 */
export function precache(tree: Tree, op: PrecacheOp, fingerprinted: ReadonlySet<string>, origin: string, warn: (m: string) => void): Tree {
  const entries = precacheEntries(tree, op, fingerprinted);
  if (entries.length === 0) warn(`${origin}: no files match ${op.patterns.join(' ')}`);
  const json = precacheJson(entries);
  const files: Array<[string, FileNode]> = [];
  if (op.dest !== undefined) files.push([op.dest, makeFile(new TextEncoder().encode(json), origin)]);
  if (op.into !== undefined) {
    const sw = tree.get(op.into);
    if (!sw) throw new SitefileError('E_VALIDATION', `PRECACHE INTO ${op.into}: there is no such file in the stage (COPY the service worker in first)`, op.span);
    let text: string;
    try {
      text = UTF8.decode(sw.bytes);
    } catch {
      throw new SitefileError('E_VALIDATION', `PRECACHE INTO ${op.into}: the service worker is not UTF-8 text`, op.span);
    }
    const at = text.indexOf(INJECTION_POINT);
    if (at === -1) throw new SitefileError('E_VALIDATION', `PRECACHE INTO ${op.into}: the service worker has no ${INJECTION_POINT} to replace with the manifest`, op.span);
    const injected = text.slice(0, at) + json.trimEnd() + text.slice(at + INJECTION_POINT.length);
    files.push([op.into, makeFile(new TextEncoder().encode(injected), origin, sw.mode)]);
  }
  // The service worker is replaced by its injected version; the manifest follows the conflict rules.
  let out = tree;
  for (const [p, node] of files) out = out.withFiles([[p, node]], p === op.into || op.overwrite ? 'overwrite' : 'error', op.span);
  return out;
}
