// FINGERPRINT (docs/LANGUAGE.md, "Fingerprinting"): rename assets to content-hashed names and rewrite
// the HTML and CSS that reference them. Ported as rust/src/fingerprint.rs: both must produce the same
// tree, manifest and messages.
import { createHash } from 'node:crypto';
import { SitefileError, type SourceSpan } from './errors.js';
import { referenceKind, resolveReference, rewriteReferences, scanFile, type Reference } from './links.js';
import { nfc } from './paths.js';
import { makeFile, Tree, type FileNode } from './tree.js';

/** Hex digits of the SHA-256 of the content in a fingerprinted name. */
export const HASH_LENGTH = 10;

/** Names FINGERPRINT never renames (besides HTML pages): files hosts look up by name. */
const FIXED_NAMES = new Set(['_redirects', '_headers']);

// ---------------------------------------------------------------- patterns

/**
 * Check a FINGERPRINT pattern: an absolute path (NFC) with no control characters. A trailing `/`
 * means everything below that directory (`/assets/` is `/assets/**`).
 */
export function checkPattern(raw: string, span: SourceSpan): string {
  if (/[\u0000-\u001f\u007f]/.test(raw)) throw new SitefileError('E_VALIDATION', 'FINGERPRINT pattern must not contain control characters', span);
  if (!raw.startsWith('/')) throw new SitefileError('E_VALIDATION', `FINGERPRINT pattern '${raw}' must be an absolute path starting with '/' (e.g. /assets/ or /**/*.css)`, span);
  const p = nfc(raw);
  return p.endsWith('/') ? p + '**' : p;
}

/** One path segment against one pattern segment: `*` is any run of characters, `?` any one. */
function segmentMatches(pat: readonly string[], s: readonly string[], pi = 0, si = 0): boolean {
  while (pi < pat.length) {
    const c = pat[pi]!;
    if (c === '*') {
      for (let k = si; k <= s.length; k++) if (segmentMatches(pat, s, pi + 1, k)) return true;
      return false;
    }
    if (si >= s.length || (c !== '?' && c !== s[si])) return false;
    pi++;
    si++;
  }
  return si === s.length;
}

/**
 * Whether a tree path matches a pattern. Segments are compared one by one; a `**` segment matches any
 * number of segments (also none). Matching is by code point and case-sensitive.
 */
export function globMatches(pattern: string, path: string): boolean {
  const pat = pattern.split('/').slice(1).map((s) => [...s]);
  const segs = path.split('/').slice(1).map((s) => [...s]);
  const go = (pi: number, si: number): boolean => {
    if (pi === pat.length) return si === segs.length;
    const p = pat[pi]!;
    if (p.length === 2 && p[0] === '*' && p[1] === '*') {
      for (let k = si; k <= segs.length; k++) if (go(pi + 1, k)) return true;
      return false;
    }
    return si < segs.length && segmentMatches(p, segs[si]!) && go(pi + 1, si + 1);
  };
  return go(0, 0);
}

// ---------------------------------------------------------------- names

/** `name.<hash>.ext`: the hash goes before the last extension (`app.min.<hash>.js`), or at the end (`LICENSE.<hash>`, `.env.<hash>`). */
export function hashedName(path: string, hash: string): string {
  const slash = path.lastIndexOf('/');
  const base = path.slice(slash + 1);
  const dot = base.lastIndexOf('.');
  const named = dot > 0 ? `${base.slice(0, dot)}.${hash}${base.slice(dot)}` : `${base}.${hash}`;
  return path.slice(0, slash + 1) + named;
}

function contentHash(bytes: Uint8Array): string {
  return createHash('sha256').update(bytes).digest('hex').slice(0, HASH_LENGTH);
}

/** Whether FINGERPRINT may rename this path: not an HTML page and not a host file such as `_redirects`. */
function renamable(path: string): boolean {
  return referenceKind(path) !== 'html' && !FIXED_NAMES.has(path.slice(path.lastIndexOf('/') + 1));
}

// ---------------------------------------------------------------- the operation

export interface FingerprintOp {
  readonly patterns: readonly string[];
  readonly manifest?: string;
  /** The directory the site is served from (the EXPORT path): absolute references resolve below it. */
  readonly root: string;
  readonly overwrite: boolean;
  readonly span: SourceSpan;
}

export interface Fingerprinted {
  readonly tree: Tree;
  /** Old path -> new path, sorted by old path. */
  readonly renames: ReadonlyArray<readonly [string, string]>;
}

function utf8Compare(a: string, b: string): number {
  return Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
}

/** The manifest: a JSON object of old path -> new path, sorted, two-space indented, with a final newline. */
export function manifestJson(renames: ReadonlyArray<readonly [string, string]>): string {
  if (renames.length === 0) return '{}\n';
  return '{\n' + renames.map(([a, b]) => `  ${JSON.stringify(a)}: ${JSON.stringify(b)}`).join(',\n') + '\n}\n';
}

/**
 * Fingerprint the files of `tree` that match the patterns. Each is renamed to `name.<hash>.ext`, where the
 * hash is of its final content: a CSS file's references to other fingerprinted files are rewritten first, so
 * its hash covers them (a cycle of such references is an error). Then every HTML and CSS file in the tree has
 * its references to renamed files rewritten. The renamed and rewritten files get the origin `origin`.
 */
export function fingerprint(tree: Tree, op: FingerprintOp, origin: string, warn: (m: string) => void): Fingerprinted {
  const policy = op.overwrite ? 'overwrite' : 'error';
  const entries = tree.entries().sort(([a], [b]) => utf8Compare(a, b));
  const selected = entries.filter(([p]) => renamable(p) && p !== op.manifest && op.patterns.some((pat) => globMatches(pat, p)));
  if (selected.length === 0) warn(`${origin}: no files match ${op.patterns.join(' ')}`);
  const isSelected = new Set(selected.map(([p]) => p));

  const scans = new Map<string, { text: string; refs: Reference[] } | undefined>();
  const scan = (p: string, node: FileNode): { text: string; refs: Reference[] } | undefined => {
    if (!scans.has(p)) scans.set(p, scanFile(p, node.bytes));
    return scans.get(p);
  };

  // Hash in dependency order: a CSS file after the fingerprinted files it references.
  const renamed = new Map<string, string>();
  const content = new Map<string, Uint8Array>();
  const state = new Map<string, 'visiting' | 'done'>();
  const byPath = new Map(entries);
  const visit = (p: string, stack: string[]): void => {
    const st = state.get(p);
    if (st === 'done') return;
    if (st === 'visiting') {
      const cycle = [...stack.slice(stack.indexOf(p)), p].join(' -> ');
      throw new SitefileError('E_VALIDATION', `FINGERPRINT: ${cycle} is a cycle of references, so no file's name can include the hash of the files it references; leave one of them out of FINGERPRINT`, op.span);
    }
    state.set(p, 'visiting');
    const node = byPath.get(p)!;
    const s = referenceKind(p) === 'css' ? scan(p, node) : undefined;
    let bytes = node.bytes;
    if (s) {
      for (const r of s.refs) {
        const t = resolveReference(r.url, p, op.root);
        if (t.kind === 'internal' && !t.dir && isSelected.has(t.path)) visit(t.path, [...stack, p]);
      }
      const text = rewriteReferences(s.text, s.refs, p, op.root, (q) => renamed.get(q));
      if (text !== undefined) bytes = new TextEncoder().encode(text);
    }
    renamed.set(p, hashedName(p, contentHash(bytes)));
    content.set(p, bytes);
    state.set(p, 'done');
  };
  for (const [p] of selected) visit(p, []);

  const files: Array<[string, FileNode]> = [];
  for (const [p, node] of entries) {
    if (isSelected.has(p)) continue;
    const s = referenceKind(p) !== undefined ? scan(p, node) : undefined;
    const text = s ? rewriteReferences(s.text, s.refs, p, op.root, (q) => renamed.get(q)) : undefined;
    files.push([p, text === undefined ? node : makeFile(new TextEncoder().encode(text), origin, node.mode)]);
  }
  const renames = selected.map(([p]): [string, string] => [p, renamed.get(p)!]);
  let out = Tree.fromFiles(files);
  out = out.withFiles(
    selected.map(([p, node]) => [renamed.get(p)!, makeFile(content.get(p)!, origin, node.mode)] as const),
    policy,
    op.span,
  );
  if (op.manifest !== undefined) out = out.withFiles([[op.manifest, makeFile(new TextEncoder().encode(manifestJson(renames)), origin)]], policy, op.span);
  return { tree: out, renames };
}

/** Compose rename maps: `earlier` then `later` (a renamed file renamed again maps to its last name). */
export function composeRenames(earlier: ReadonlyMap<string, string>, later: ReadonlyArray<readonly [string, string]>): Map<string, string> {
  const next = new Map(later);
  const intermediate = new Set(earlier.values());
  const out = new Map<string, string>();
  for (const [a, b] of earlier) out.set(a, next.get(b) ?? b);
  for (const [a, b] of later) if (!intermediate.has(a)) out.set(a, b);
  return out;
}
