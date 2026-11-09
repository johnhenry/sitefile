import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { SitefileError } from './errors.js';
import { INTEGRITY_RE } from './digest.js';
import { LocalContext } from './resources/local.js';
import { loadArchiveTree } from './servers.js';
import type { Store } from './store.js';
import type { Tree } from './tree.js';

/**
 * Per-file digest manifests (issue #211): every exported file with its size and SHA-256,
 * plus a Merkle root over the sorted (path, digest) pairs, so a deployed or extracted
 * copy can be checked file by file (`sitefile verify`) and a single file can be proven
 * against the root with a path of hashes.
 *
 * The root is the RFC 6962 Merkle Tree Hash of the leaves `path 0x00 digest` (UTF-8,
 * digest written `sha256:<hex>`) in path order: a leaf hashes as SHA-256(0x00 ‖ leaf),
 * an inner node as SHA-256(0x01 ‖ left ‖ right), the split at the largest power of two
 * below the count, and no files as SHA-256 of nothing.
 */
export const MANIFEST_MEDIA_TYPE = 'application/vnd.sitefile.manifest.v1+json';

export interface ManifestFile {
  readonly path: string;
  readonly size: number;
  /** `sha256:<hex>` of the file's bytes. */
  readonly digest: string;
}

export interface Manifest {
  readonly mediaType: typeof MANIFEST_MEDIA_TYPE;
  /** `sha256:<hex>`: the Merkle root of {@link files}. */
  readonly root: string;
  readonly files: readonly ManifestFile[];
}

const sha = (...parts: Uint8Array[]): Buffer => {
  const h = createHash('sha256');
  for (const p of parts) h.update(p);
  return h.digest();
};

const LEAF = new Uint8Array([0]);
const NODE = new Uint8Array([1]);
const enc = new TextEncoder();

function mth(leaves: readonly Uint8Array[], lo: number, hi: number): Buffer {
  const n = hi - lo;
  if (n === 0) return sha();
  if (n === 1) return sha(LEAF, leaves[lo] as Uint8Array);
  let k = 1;
  while (k * 2 < n) k *= 2;
  return sha(NODE, mth(leaves, lo, lo + k), mth(leaves, lo + k, hi));
}

/** The Merkle root of `files` (in the order given; manifests list them by path). */
export function merkleRoot(files: readonly ManifestFile[]): string {
  const leaves = files.map((f) => enc.encode(`${f.path}\0${f.digest}`));
  return 'sha256:' + mth(leaves, 0, leaves.length).toString('hex');
}

/** The manifest of a tree: every file in path order. */
export function manifestOf(tree: Tree): Manifest {
  const files = tree.entries().map(([p, n]) => ({ path: p, size: n.bytes.byteLength, digest: n.digest }));
  return { mediaType: MANIFEST_MEDIA_TYPE, root: merkleRoot(files), files };
}

/** The manifest as written to a file: `JSON.stringify(manifest, null, 2)` and a newline. */
export function encodeManifest(m: Manifest): Uint8Array {
  return enc.encode(JSON.stringify(m, null, 2) + '\n');
}

/** Parse a manifest file. Its root must match its files. */
export function parseManifest(bytes: Uint8Array, origin: string): Manifest {
  const bad = (why: string): SitefileError => new SitefileError('E_INTEGRITY', `${origin}: ${why}`);
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  } catch {
    throw bad('not a Sitefile manifest (invalid JSON)');
  }
  const o = v as Record<string, unknown> | null;
  if (typeof o !== 'object' || o === null || Array.isArray(o) || o.mediaType !== MANIFEST_MEDIA_TYPE) {
    throw bad(`not a Sitefile manifest (mediaType is not ${MANIFEST_MEDIA_TYPE})`);
  }
  if (typeof o.root !== 'string' || !INTEGRITY_RE.test(o.root) || !Array.isArray(o.files)) throw bad('not a Sitefile manifest (root or files missing)');
  const files: ManifestFile[] = [];
  const seen = new Set<string>();
  for (const f of o.files as unknown[]) {
    const e = f as Record<string, unknown> | null;
    if (
      typeof e !== 'object' ||
      e === null ||
      typeof e.path !== 'string' ||
      !e.path.startsWith('/') ||
      typeof e.size !== 'number' ||
      !Number.isSafeInteger(e.size) ||
      e.size < 0 ||
      typeof e.digest !== 'string' ||
      !INTEGRITY_RE.test(e.digest)
    ) {
      throw bad('not a Sitefile manifest (a file entry needs path, size and digest)');
    }
    if (seen.has(e.path)) throw bad(`lists '${e.path}' twice`);
    seen.add(e.path);
    files.push({ path: e.path, size: e.size, digest: e.digest });
  }
  const root = merkleRoot(files);
  if (root !== o.root) throw bad(`its root ${o.root} does not match its files (${root})`);
  return { mediaType: MANIFEST_MEDIA_TYPE, root, files };
}

/** What `verify` and `diff` compare: a directory, an archive file, or an image in the store. */
export type Target = { readonly kind: 'dir' | 'archive'; readonly path: string } | { readonly kind: 'image'; readonly ref: string };

/** True for an argument written as a path: `.`, `..`, `./x`, `../x`, `/x`, `~x`, `x/`. */
const looksLikePath = (arg: string): boolean => /^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$/.test(arg);

async function kindOnDisk(p: string): Promise<'dir' | 'archive' | undefined> {
  const st = await fs.stat(p).catch(() => undefined);
  return st?.isDirectory() ? 'dir' : st?.isFile() ? 'archive' : undefined;
}

/**
 * Resolve a `verify`/`diff` argument: a path (written `./x`, `/x`, `x/`, …) is a directory or
 * an archive file; anything else is an image reference or ID when the store has it, else a
 * directory or file by that name.
 */
export async function resolveTarget(arg: string, store: () => Promise<Store>): Promise<Target> {
  if (looksLikePath(arg)) {
    const kind = await kindOnDisk(arg);
    if (!kind) throw new SitefileError('E_NOT_FOUND', `'${arg}' is not a directory or a file`);
    return { kind, path: arg };
  }
  try {
    const s = await store();
    await s.resolve(arg);
    return { kind: 'image', ref: arg };
  } catch (e) {
    if (!(e instanceof SitefileError) || e.code !== 'E_NOT_FOUND') throw e;
    const kind = await kindOnDisk(arg);
    if (kind) return { kind, path: arg };
    throw e;
  }
}

/**
 * The files of a target, hidden ones included (a manifest lists every exported file): a
 * directory is read like a build context (symlinks must stay inside it; `.git` is skipped),
 * an archive is extracted with the default limits, an image is loaded from the store with
 * every blob's digest checked.
 */
export async function loadTarget(t: Target, store: () => Promise<Store>): Promise<Tree> {
  switch (t.kind) {
    case 'image': {
      const s = await store();
      return s.loadTree(await s.resolve(t.ref));
    }
    case 'dir': {
      const ctx = await LocalContext.open(t.path, { sitefileignore: false, allowHardlinks: true });
      return (await ctx.readTree('', 'verify')).tree;
    }
    case 'archive':
      return loadArchiveTree(t.path, undefined, { hidden: true });
  }
}

export interface VerifyChange {
  readonly path: string;
  readonly expected: { readonly size: number; readonly digest: string };
  readonly actual: { readonly size: number; readonly digest: string };
}

export interface VerifyReport {
  readonly ok: boolean;
  readonly root: string;
  /** Files the manifest lists. */
  readonly files: number;
  readonly missing: readonly string[];
  readonly changed: readonly VerifyChange[];
  readonly extra: readonly string[];
}

/** Compare `tree` with `manifest`. `ignore` is a path not counted as extra (the manifest itself, inside a verified directory). */
export function verifyTree(tree: Tree, manifest: Manifest, ignore?: string): VerifyReport {
  const listed = new Set(manifest.files.map((f) => f.path));
  const missing: string[] = [];
  const changed: VerifyChange[] = [];
  for (const f of manifest.files) {
    const n = tree.get(f.path);
    if (!n) missing.push(f.path);
    else if (n.digest !== f.digest || n.bytes.byteLength !== f.size) {
      changed.push({ path: f.path, expected: { size: f.size, digest: f.digest }, actual: { size: n.bytes.byteLength, digest: n.digest } });
    }
  }
  const extra = tree
    .entries()
    .map(([p]) => p)
    .filter((p) => !listed.has(p) && p !== ignore);
  return { ok: !missing.length && !changed.length && !extra.length, root: manifest.root, files: manifest.files.length, missing, changed, extra };
}

/** The text report: one `missing|changed|extra  <path>` line per difference, in path order. */
export function verifyText(r: VerifyReport): string {
  const lines = [
    ...r.missing.map((p) => [p, 'missing'] as const),
    ...r.changed.map((c) => [c.path, 'changed'] as const),
    ...r.extra.map((p) => [p, 'extra'] as const),
  ].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return lines.map(([p, k]) => `${k.padEnd(8)} ${p}\n`).join('');
}

/** The tree path of `file` when it lies inside directory `dir` (both real paths), else undefined. */
export async function pathInside(dir: string, file: string): Promise<string | undefined> {
  const [d, f] = await Promise.all([fs.realpath(dir).catch(() => undefined), fs.realpath(file).catch(() => undefined)]);
  if (d === undefined || f === undefined) return undefined;
  const rel = path.relative(d, f);
  if (rel === '' || rel.startsWith('..') || path.isAbsolute(rel)) return undefined;
  return '/' + rel.split(path.sep).join('/');
}

export interface DiffFile {
  readonly path: string;
  readonly size: number;
  readonly digest: string;
  /** `644` or `755`. */
  readonly mode: string;
}

export interface RuleDiff {
  readonly added: readonly string[];
  readonly removed: readonly string[];
}

export interface DiffReport {
  readonly added: readonly DiffFile[];
  readonly removed: readonly DiffFile[];
  readonly changed: ReadonlyArray<{ readonly path: string; readonly before: DiffFile; readonly after: DiffFile }>;
  /** Rule changes in `/_redirects` and `/_headers`, when either side has the file and they differ. */
  readonly rules: { readonly [file: string]: RuleDiff };
}

const RULE_FILES = ['/_redirects', '/_headers'] as const;

/**
 * The rules of a `_redirects` or `_headers` file, one string each: `_redirects` lines with their
 * fields joined by one space; for `_headers`, `<path> <Name>: <value>` per header line. Blank lines
 * and `#` comments are not rules.
 */
export function ruleLines(file: string, bytes: Uint8Array | undefined): string[] {
  if (!bytes) return [];
  const text = new TextDecoder().decode(bytes);
  const out: string[] = [];
  let current: string | undefined;
  for (const raw of text.split(/\r?\n/)) {
    const t = raw.trim();
    if (t === '' || t.startsWith('#')) continue;
    if (file === '/_redirects') out.push(t.split(/[ \t]+/).join(' '));
    else if (!/^[ \t]/.test(raw)) current = t;
    else out.push(`${current ?? '(no path)'} ${t}`);
  }
  return out;
}

/** Lines of `a` not matched one for one in `b`, in `a`'s order. */
function minus(a: readonly string[], b: readonly string[]): string[] {
  const left = new Map<string, number>();
  for (const l of b) left.set(l, (left.get(l) ?? 0) + 1);
  return a.filter((l) => {
    const n = left.get(l) ?? 0;
    if (n > 0) left.set(l, n - 1);
    return n === 0;
  });
}

const fileOf = (p: string, n: { bytes: Uint8Array; digest: string; mode: number }): DiffFile => ({ path: p, size: n.bytes.byteLength, digest: n.digest, mode: n.mode.toString(8) });

/** Files added, removed and changed (digest or mode) going from `a` to `b`, in path order. */
export function diffTrees(a: Tree, b: Tree): DiffReport {
  const am = new Map(a.entries());
  const bm = new Map(b.entries());
  const added: DiffFile[] = [];
  const removed: DiffFile[] = [];
  const changed: Array<{ path: string; before: DiffFile; after: DiffFile }> = [];
  for (const p of [...new Set([...am.keys(), ...bm.keys()])].sort((x, y) => (x < y ? -1 : x > y ? 1 : 0))) {
    const x = am.get(p);
    const y = bm.get(p);
    if (!x && y) added.push(fileOf(p, y));
    else if (x && !y) removed.push(fileOf(p, x));
    else if (x && y && (x.digest !== y.digest || x.mode !== y.mode)) changed.push({ path: p, before: fileOf(p, x), after: fileOf(p, y) });
  }
  const rules: Record<string, RuleDiff> = {};
  for (const f of RULE_FILES) {
    const x = am.get(f);
    const y = bm.get(f);
    if ((!x && !y) || x?.digest === y?.digest) continue;
    const before = ruleLines(f, x?.bytes);
    const after = ruleLines(f, y?.bytes);
    const r = { added: minus(after, before), removed: minus(before, after) };
    if (r.added.length || r.removed.length) rules[f] = r;
  }
  return { added, removed, changed, rules };
}

/** The text report: `A|D|C <path>` lines (with sizes), then rule changes, then a summary line. */
export function diffText(r: DiffReport): string {
  const size = (f: DiffFile): string => `${f.size} B`;
  const lines: string[] = [
    ...r.added.map((f) => [f.path, `A  ${f.path}  (${size(f)})`] as const),
    ...r.removed.map((f) => [f.path, `D  ${f.path}  (${size(f)})`] as const),
    ...r.changed.map(
      (c) => [c.path, `C  ${c.path}  (${size(c.before)} -> ${size(c.after)}${c.before.mode !== c.after.mode ? `, mode ${c.before.mode} -> ${c.after.mode}` : ''})`] as const,
    ),
  ]
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([, l]) => l);
  for (const [file, d] of Object.entries(r.rules)) {
    for (const l of d.removed) lines.push(`-  ${file}: ${l}`);
    for (const l of d.added) lines.push(`+  ${file}: ${l}`);
  }
  lines.push(`${r.added.length} added, ${r.removed.length} removed, ${r.changed.length} changed`);
  return lines.join('\n') + '\n';
}
