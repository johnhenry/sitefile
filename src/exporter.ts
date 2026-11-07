import { randomBytes } from 'node:crypto';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';

export interface ExportOptions {
  /** The build context. The destination may never be, or contain, it. */
  readonly contextDir: string;
  /** Replace a non-empty destination outside the context. */
  readonly force?: boolean;
  /**
   * TEST-ONLY. Awaited after the temp tree is fully written and immediately
   * before the final re-validation and swap, so tests can deterministically
   * simulate a concurrent change to the destination. Not exposed by the CLI.
   * @internal
   */
  readonly beforeSwap?: () => Promise<void>;
  /**
   * Watch mode (directory exports only): what the previous export of this session left on
   * disk. When the destination still holds exactly that, only the files that changed are
   * written ({@link ExportState}); either way the export is recorded for the next one.
   */
  readonly incremental?: ExportState;
  /**
   * TEST-ONLY. Incremental export: awaited after the changed files are staged, before any
   * of them is moved into the destination. @internal
   */
  readonly beforeApply?: () => Promise<void>;
}

export interface ExportResult {
  /** Real absolute path the tree was written to. */
  readonly outDir: string;
  readonly files: number;
  /** Set when only the changes were applied in place (incremental export). */
  readonly incremental?: { readonly written: number; readonly removed: number };
}

/**
 * The temp directory starts empty and the tree has no two paths equal under
 * case folding, so EEXIST/ENOTDIR/EISDIR here means the destination file
 * system treats two different tree paths as one name (a folding or
 * normalization rule wider than Sitefile's). Report the artifact path, not the
 * temp path.
 */
export function occupied(e: unknown, p: string): unknown {
  const code = (e as NodeJS.ErrnoException | null)?.code;
  if (code !== 'EEXIST' && code !== 'ENOTDIR' && code !== 'EISDIR') return e;
  return new SitefileError(
    'E_EXPORT',
    `cannot write '${p}': the destination file system already has an entry by that name (it folds case or normalizes Unicode more widely than Sitefile's checks)`,
    undefined,
    { cause: e },
  );
}

/** Matches the exporter's temporary (`tmp`) and backup (`old`) sibling names. */
export const TEMP_PREFIX_PATTERN = /^\..+\.sitefile-(?:tmp|old)-[0-9a-f]+$/;

/** True if `name` is a temp/backup sibling the exporter creates for `outBasename`. */
export function isExporterTempName(name: string, outBasename: string): boolean {
  const tmp = `.${outBasename}.sitefile-tmp-`;
  const old = `.${outBasename}.sitefile-old-`;
  return (name.startsWith(tmp) || name.startsWith(old)) && TEMP_PREFIX_PATTERN.test(name);
}

/**
 * Materialize `tree` as a directory at `outDir`. Files are written into a
 * fresh temporary sibling, which is then swapped in; on failure the previous
 * destination (if any) is left in place.
 */
export async function exportDirectory(tree: Tree, outDir: string, options: ExportOptions): Promise<ExportResult> {
  const outAbs = path.resolve(outDir);
  const outReal = await realpathMissing(outAbs);
  const contextReal = await realpathOr(path.resolve(options.contextDir), 'build context');

  // --- destination safety --------------------------------------------------
  const home = await realpathOrRaw(os.homedir());
  assertSafeDestination(outReal, contextReal, home);

  const st = await lstatOrUndefined(outReal);
  if (st?.isSymbolicLink()) {
    // Never write through a final symlink; but if it points at something
    // dangerous, say so.
    const target = await fs.realpath(outReal).catch(() => undefined);
    if (target !== undefined) assertSafeDestination(target, contextReal, home);
  }
  if (st) {
    if (st.isSymbolicLink()) {
      throw new SitefileError('E_EXPORT', `export destination '${outReal}' is a symbolic link`);
    }
    if (!st.isDirectory()) {
      throw new SitefileError('E_EXPORT', `export destination '${outReal}' exists and is not a directory`);
    }
    if (!isInside(outReal, contextReal) && !options.force) {
      const entries = await fs.readdir(outReal);
      if (entries.length > 0) {
        throw new SitefileError(
          'E_EXPORT_UNSAFE',
          `export destination '${outReal}' is outside the build context and not empty; pass --force to replace it`,
        );
      }
    }
  }

  // --- validate paths before touching the disk ------------------------------
  const entries = tree.entries();
  const planned: Planned[] = [];
  for (const [p, node] of entries) {
    if (!p.startsWith('/')) throw new SitefileError('E_EXPORT', `invalid artifact path '${p}': not absolute`);
    const segments = p.slice(1).split('/');
    for (const seg of segments) {
      if (seg === '' || seg === '.' || seg === '..' || seg.includes('\0') || seg.includes('\\')) {
        throw new SitefileError('E_EXPORT', `invalid artifact path '${p}'`);
      }
    }
    planned.push({ path: p, segments, bytes: node.bytes, mode: node.mode, digest: node.digest });
  }

  // --- watch mode: apply only the changes when the destination is as we left it
  const state = options.incremental;
  if (state) {
    const applied = await exportIncremental(planned, outReal, st, state, options);
    if (applied) return applied;
  }
  const stamps = state ? new Map<string, Stamp>() : undefined;

  // --- write into a temp sibling, then swap ---------------------------------
  //
  // Race hardening (issue #5). Node has no openat/renameat2, so every check
  // below is path-based and a hostile process with write access to the
  // destination's parent can still win a window of a few syscalls. What we do
  // is shrink that window and make it detectable:
  //   1. capture dev/ino of the parent directory and of the existing `out`;
  //   2. create tmp/old siblings only in that verified parent;
  //   3. right before the swap, re-check the parent's real path + identity,
  //      and that `out` is still the same real directory (or still absent);
  //   4. after moving `out` aside, confirm what we moved is what we validated;
  //   5. after moving tmp into place, confirm `out` is the tree we wrote;
  //   6. only ever recursively delete a path whose dev/ino we verified.
  // Residual risk: (a) the two renames are not atomic together, so there is a
  // moment where `out` does not exist; (b) between a check and the following
  // rename an attacker can still swap a path component, which we only detect
  // after the fact (steps 4-5) and never silently "fix"; (c) renaming onto an
  // originally-absent `out` that an attacker just created as an empty
  // directory succeeds on POSIX; (d) if the parent itself is moved away, the
  // temp dir moves with it and cannot be located for cleanup.
  const parent = path.dirname(outReal);
  const base = path.basename(outReal);
  const tmp = path.join(parent, `.${base}.sitefile-tmp-${randomHex()}`);
  const old = path.join(parent, `.${base}.sitefile-old-${randomHex()}`);
  try {
    await fs.mkdir(parent, { recursive: true });
  } catch (e) {
    throw new SitefileError('E_EXPORT', `cannot create '${parent}': ${(e as Error).message}`, undefined, { cause: e });
  }
  const parentId = await verifiedParentId(parent);
  const outId = st ? idOf(st) : undefined;

  try {
    await fs.mkdir(tmp);
  } catch (e) {
    throw new SitefileError('E_EXPORT', `cannot create '${tmp}': ${(e as Error).message}`, undefined, { cause: e });
  }
  let tmpId: FileId;
  try {
    const tst = await fs.lstat(tmp);
    if (!tst.isDirectory() || tst.isSymbolicLink()) throw unsafe(`temporary directory '${tmp}' is not a real directory`);
    tmpId = idOf(tst);
    await assertParentUnchanged(parent, parentId);
  } catch (e) {
    await removeIfSame(tmp, undefined);
    throw e;
  }

  let oldMoved = false;
  let swapped = false;
  try {
    for (const { path: p, segments, bytes, mode } of planned) {
      const target = path.join(tmp, ...segments);
      try {
        await fs.mkdir(path.dirname(target), { recursive: true });
        await fs.writeFile(target, bytes, { flag: 'wx' });
      } catch (e) {
        throw occupied(e, p);
      }
      await fs.chmod(target, mode);
      // Renaming the temp directory leaves its files' inodes (and ctimes) as they are now.
      stamps?.set(p, stampOf(await fs.lstat(target)));
    }
    if (options.beforeSwap) await options.beforeSwap();

    // Re-validate immediately before the swap.
    await assertParentUnchanged(parent, parentId);
    await assertSameId(tmp, tmpId, 'temporary directory');
    const cur = await lstatOrUndefined(outReal);
    if (outId) {
      if (!cur || cur.isSymbolicLink() || !cur.isDirectory() || !sameId(idOf(cur), outId)) {
        throw unsafe(`export destination '${outReal}' changed during export; aborting`);
      }
    } else if (cur) {
      throw unsafe(`export destination '${outReal}' appeared during export; aborting`);
    }
    if ((await lstatOrUndefined(old)) !== undefined) throw unsafe(`backup path '${old}' already exists; aborting`);

    if (outId) {
      await fs.rename(outReal, old);
      oldMoved = true;
      // What we moved aside must be the directory we validated.
      const moved = await lstatOrUndefined(old);
      if (!moved || moved.isSymbolicLink() || !moved.isDirectory() || !sameId(idOf(moved), outId)) {
        throw unsafe(`export destination '${outReal}' was replaced during the swap; aborting`);
      }
    }
    await fs.rename(tmp, outReal);
    swapped = true;
    const now = await lstatOrUndefined(outReal);
    if (!now || now.isSymbolicLink() || !now.isDirectory() || !sameId(idOf(now), tmpId)) {
      throw unsafe(
        `export destination '${outReal}' is not the directory that was written; ` +
          (oldMoved ? `previous output preserved at '${old}'` : 'inspect it manually'),
      );
    }
  } catch (e) {
    if (!swapped) {
      await removeIfSame(tmp, tmpId);
      if (oldMoved) {
        // Roll back only if the slot is free and the backup is what we validated.
        const slot = await lstatOrUndefined(outReal);
        const bak = await lstatOrUndefined(old);
        if (!slot && bak && outId && sameId(idOf(bak), outId)) await fs.rename(old, outReal).catch(() => {});
      }
    }
    if (e instanceof SitefileError) throw e;
    throw new SitefileError('E_EXPORT', `failed to export to '${outReal}': ${(e as Error).message}`, undefined, {
      cause: e,
    });
  }

  if (oldMoved) {
    // Best effort: the new output is already in place. Only delete the exact
    // directory we validated and moved.
    await removeIfSame(old, outId);
  }
  if (state && stamps) {
    const files = new Map<string, Recorded>();
    for (const e of planned) files.set(e.path, { digest: e.digest, mode: e.mode, stamp: stamps.get(e.path)! });
    state.snapshot = { outReal, outId: tmpId, parentId, files, dirs: dirsOf(planned) };
  }
  return { outDir: outReal, files: planned.length };
}

// --- incremental export (watch mode) ---------------------------------------
//
// A watch session keeps an ExportState: the destination's identity and, for every file it
// exported, the tree digest and mode plus the file's lstat stamp (dev, ino, size, mtime,
// ctime, permission bits) as it was right after Sitefile wrote it. The next export of the
// session first checks that the destination still holds exactly that: the same real
// directory and parent (dev/ino), every recorded directory and file and nothing else (one
// readdir per directory), each file with its recorded stamp (one lstat per file; ctime
// changes on any write, chmod or rename, even when mtime is restored). If so, it writes only
// the files whose digest or mode changed and removes the files and directories that are
// gone; otherwise (nothing recorded, the destination moved or was modified, an entry that
// would collide on a case-folding or normalizing file system, or more than half of the files
// change) it falls back to the full write-and-swap above.
//
// Applying: the changed files are written to a temporary sibling first (the slow part, the
// destination untouched); then deleted files are unlinked, emptied directories removed
// (deepest first), new directories created (shallowest first) and each staged file renamed
// over its path, every step re-checking that what it replaces is the file it recorded. Each
// file is therefore always either its old or its new version (rename is atomic), and the
// destination holds a mix of two builds only while those renames run. If a step fails, the
// full swap runs, which replaces the whole destination atomically.

/** What a watch session's last directory export left on disk; see {@link ExportOptions.incremental}. */
export class ExportState {
  /** @internal */
  snapshot: Snapshot | undefined;

  /** Forget the last export: the next one rewrites everything. */
  clear(): void {
    this.snapshot = undefined;
  }
}

interface Planned {
  readonly path: string;
  readonly segments: string[];
  readonly bytes: Uint8Array;
  readonly mode: number;
  readonly digest: string;
}

interface Stamp {
  readonly dev: number;
  readonly ino: number;
  readonly size: number;
  readonly mtimeMs: number;
  readonly ctimeMs: number;
  readonly mode: number;
}

interface Recorded {
  readonly digest: string;
  readonly mode: number;
  readonly stamp: Stamp;
}

interface Snapshot {
  readonly outReal: string;
  readonly outId: FileId;
  readonly parentId: FileId;
  /** Artifact path (`/a/b.html`) → what was written there. */
  readonly files: ReadonlyMap<string, Recorded>;
  /** Every directory below the destination (`/a`, `/a/b`). */
  readonly dirs: ReadonlySet<string>;
}

function stampOf(st: import('node:fs').Stats): Stamp {
  return { dev: st.dev, ino: st.ino, size: st.size, mtimeMs: st.mtimeMs, ctimeMs: st.ctimeMs, mode: st.mode };
}

function sameStamp(st: import('node:fs').Stats | undefined, r: Stamp): boolean {
  return (
    st !== undefined &&
    st.isFile() &&
    st.dev === r.dev &&
    st.ino === r.ino &&
    st.size === r.size &&
    st.mtimeMs === r.mtimeMs &&
    st.ctimeMs === r.ctimeMs &&
    st.mode === r.mode
  );
}

/** Every directory that holds a file of `files`, below the root. */
function dirsOf(files: readonly { readonly path: string }[]): Set<string> {
  const dirs = new Set<string>();
  for (const { path: p } of files) {
    for (let i = p.indexOf('/', 1); i !== -1; i = p.indexOf('/', i + 1)) dirs.add(p.slice(0, i));
  }
  return dirs;
}

const depth = (p: string): number => p.split('/').length;

/** `fn` over `items`, at most `limit` at a time. */
async function eachLimit<T>(items: readonly T[], limit: number, fn: (item: T) => Promise<void>): Promise<void> {
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < items.length) await fn(items[next++]!);
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
}

/** True if `outReal` holds exactly the recorded directories and files, each file as recorded. */
async function matchesSnapshot(outReal: string, snap: Snapshot): Promise<boolean> {
  const files: Array<[string, Stamp]> = [];
  let dirs = 0;
  const pending = [''];
  while (pending.length) {
    const rel = pending.pop()!;
    for (const e of await fs.readdir(path.join(outReal, rel), { withFileTypes: true })) {
      const p = `${rel}/${e.name}`;
      if (e.isDirectory()) {
        if (!snap.dirs.has(p)) return false;
        dirs++;
        pending.push(p);
      } else if (e.isFile()) {
        const r = snap.files.get(p);
        if (!r) return false;
        files.push([p, r.stamp]);
      } else {
        return false; // a symlink, socket, ...: never ours
      }
    }
  }
  if (dirs !== snap.dirs.size || files.length !== snap.files.size) return false;
  let ok = true;
  await eachLimit(files, 64, async ([p, stamp]) => {
    if (ok && !sameStamp(await lstatOrUndefined(path.join(outReal, p)), stamp)) ok = false;
  });
  return ok;
}

/**
 * Apply only the changes since the recorded export (see the comment above). Returns
 * undefined to ask for the full swap; the destination is then either untouched or, if
 * applying failed part way, about to be replaced whole.
 */
async function exportIncremental(
  planned: readonly Planned[],
  outReal: string,
  st: import('node:fs').Stats | undefined,
  state: ExportState,
  options: ExportOptions,
): Promise<ExportResult | undefined> {
  const snap = state.snapshot;
  state.snapshot = undefined; // recorded again only after a successful export
  if (!snap || !st || snap.outReal !== outReal || st.isSymbolicLink() || !st.isDirectory() || !sameId(idOf(st), snap.outId)) return undefined;

  const next = new Set(planned.map((e) => e.path));
  const writes = planned.filter((e) => {
    const r = snap.files.get(e.path);
    return !r || r.digest !== e.digest || r.mode !== e.mode;
  });
  const deletes = [...snap.files.keys()].filter((p) => !next.has(p));
  if ((writes.length + deletes.length) * 2 > Math.max(planned.length, snap.files.size)) return undefined;
  const nextDirs = dirsOf(planned);
  const removeDirs = [...snap.dirs].filter((d) => !nextDirs.has(d)).sort((a, b) => depth(b) - depth(a));
  const makeDirs = [...nextDirs].filter((d) => !snap.dirs.has(d)).sort((a, b) => depth(a) - depth(b));
  const parent = path.dirname(outReal);
  const at = (p: string): string => path.join(outReal, ...p.slice(1).split('/'));

  try {
    await assertParentUnchanged(parent, snap.parentId);
    if (!(await matchesSnapshot(outReal, snap))) return undefined;
    // A new path must not exist yet, except as the very file or directory it replaces;
    // anything else is another spelling of an existing entry (case folding, Unicode
    // normalization): the full export reports that as a clean build does.
    const going = new Set([...deletes, ...removeDirs]);
    for (const p of [...writes.filter((e) => !snap.files.has(e.path)).map((e) => e.path), ...makeDirs]) {
      if ((await lstatOrUndefined(at(p))) !== undefined && !going.has(p)) return undefined;
    }
  } catch {
    return undefined;
  }

  // Stage the new contents next to the destination.
  const tmp = path.join(parent, `.${path.basename(outReal)}.sitefile-tmp-${randomHex()}`);
  let tmpId: FileId;
  try {
    await fs.mkdir(tmp);
    const tst = await fs.lstat(tmp);
    if (!tst.isDirectory() || tst.isSymbolicLink()) return undefined;
    tmpId = idOf(tst);
  } catch {
    await removeIfSame(tmp, undefined);
    return undefined;
  }
  const staged: Array<{ readonly e: Planned; readonly file: string; readonly id: FileId }> = [];
  try {
    for (const e of writes) {
      const file = path.join(tmp, String(staged.length));
      await fs.writeFile(file, e.bytes, { flag: 'wx' });
      await fs.chmod(file, e.mode);
      staged.push({ e, file, id: idOf(await fs.lstat(file)) });
    }
    if (options.beforeApply) await options.beforeApply();
  } catch {
    await removeIfSame(tmp, tmpId);
    return undefined;
  }

  // Apply.
  const files = new Map(snap.files);
  try {
    await assertParentUnchanged(parent, snap.parentId);
    await assertSameId(outReal, snap.outId, 'export destination');
    for (const p of deletes) {
      const t = at(p);
      if (!sameStamp(await lstatOrUndefined(t), snap.files.get(p)!.stamp)) throw unsafe(`'${t}' changed during export`);
      await fs.unlink(t);
      files.delete(p);
    }
    for (const d of removeDirs) await fs.rmdir(at(d)); // fails unless emptied
    for (const d of makeDirs) {
      await fs.mkdir(at(d));
      const ds = await fs.lstat(at(d));
      if (ds.isSymbolicLink() || !ds.isDirectory()) throw unsafe(`'${at(d)}' is not a directory`);
    }
    for (const { e, file, id } of staged) {
      const t = at(e.path);
      const cur = await lstatOrUndefined(t);
      const r = snap.files.get(e.path);
      if (r ? !sameStamp(cur, r.stamp) : cur !== undefined) throw unsafe(`'${t}' changed during export`);
      await fs.rename(file, t);
      const now = await fs.lstat(t);
      if (now.isSymbolicLink() || !now.isFile() || !sameId(idOf(now), id)) throw unsafe(`'${t}' is not the file that was written`);
      files.set(e.path, { digest: e.digest, mode: e.mode, stamp: stampOf(now) });
    }
  } catch {
    await removeIfSame(tmp, tmpId);
    return undefined; // the full swap replaces whatever was applied
  }
  await removeIfSame(tmp, tmpId);
  state.snapshot = { outReal, outId: snap.outId, parentId: snap.parentId, files, dirs: nextDirs };
  return { outDir: outReal, files: planned.length, incremental: { written: writes.length, removed: deletes.length } };
}

export interface ExportFileResult {
  /** Real absolute path of the file that was written. */
  readonly outFile: string;
  readonly bytes: number;
}

/**
 * Write `data` as a single file at `outFile` (zip, tar, tgz, wbn). Same
 * destination rules as {@link exportDirectory}: never the filesystem root, the
 * home directory, the build context or a parent of it; never through a symlink;
 * a directory is refused; an existing file outside the context needs
 * `force`. The bytes go to a temporary sibling that is renamed into place after
 * the parent and destination are re-validated; only that exact temp file (by
 * dev/ino) is ever deleted, and nothing already at `outFile` is removed except
 * by the replacing rename.
 */
export async function exportFile(data: Uint8Array, outFile: string, options: ExportOptions): Promise<ExportFileResult> {
  const outAbs = path.resolve(outFile);
  const outReal = await realpathMissing(outAbs);
  const contextReal = await realpathOr(path.resolve(options.contextDir), 'build context');
  const home = await realpathOrRaw(os.homedir());
  assertSafeDestination(outReal, contextReal, home);

  const st = await lstatOrUndefined(outReal);
  if (st?.isSymbolicLink()) {
    const target = await fs.realpath(outReal).catch(() => undefined);
    if (target !== undefined) assertSafeDestination(target, contextReal, home);
    throw new SitefileError('E_EXPORT', `export destination '${outReal}' is a symbolic link`);
  }
  if (st) {
    if (st.isDirectory()) {
      throw new SitefileError('E_EXPORT', `export destination '${outReal}' is a directory; an archive export needs a file path`);
    }
    if (!st.isFile()) {
      throw new SitefileError('E_EXPORT', `export destination '${outReal}' exists and is not a regular file`);
    }
    if (!isInside(outReal, contextReal) && !options.force) {
      throw new SitefileError(
        'E_EXPORT_UNSAFE',
        `export destination '${outReal}' already exists outside the build context; pass --force to replace it`,
      );
    }
  }

  const parent = path.dirname(outReal);
  const base = path.basename(outReal);
  const tmp = path.join(parent, `.${base}.sitefile-tmp-${randomHex()}`);
  try {
    await fs.mkdir(parent, { recursive: true });
  } catch (e) {
    throw new SitefileError('E_EXPORT', `cannot create '${parent}': ${(e as Error).message}`, undefined, { cause: e });
  }
  const parentId = await verifiedParentId(parent);
  const outId = st ? idOf(st) : undefined;
  // dev/ino alone can't tell a replaced file from the original: Linux often gives a file
  // created right after an unlink the same inode number. Size and the nanosecond ctime
  // can't survive a replacement (or any write) unchanged.
  const outStamp = st ? await replacementStampOf(outReal) : undefined;

  let tmpId: FileId | undefined;
  let renamed = false;
  try {
    const fh = await fs.open(tmp, 'wx', 0o644);
    try {
      tmpId = idOf(await fh.stat());
      await fh.writeFile(data);
      await fh.chmod(0o644);
    } finally {
      await fh.close();
    }
    if (options.beforeSwap) await options.beforeSwap();

    await assertParentUnchanged(parent, parentId);
    const t = await lstatOrUndefined(tmp);
    if (!t || t.isSymbolicLink() || !t.isFile() || !sameId(idOf(t), tmpId)) {
      throw unsafe(`temporary file '${tmp}' changed during export; aborting`);
    }
    const cur = await lstatOrUndefined(outReal);
    if (outId) {
      if (
        !cur ||
        cur.isSymbolicLink() ||
        !cur.isFile() ||
        !sameId(idOf(cur), outId) ||
        !sameReplacementStamp(await replacementStampOf(outReal), outStamp)
      ) {
        throw unsafe(`export destination '${outReal}' changed during export; aborting`);
      }
    } else if (cur) {
      throw unsafe(`export destination '${outReal}' appeared during export; aborting`);
    }
    await fs.rename(tmp, outReal);
    renamed = true;
    const now = await lstatOrUndefined(outReal);
    if (!now || now.isSymbolicLink() || !now.isFile() || !sameId(idOf(now), tmpId)) {
      throw unsafe(`export destination '${outReal}' is not the file that was written; inspect it manually`);
    }
  } catch (e) {
    if (!renamed && tmpId) await removeFileIfSame(tmp, tmpId);
    if (e instanceof SitefileError) throw e;
    throw new SitefileError('E_EXPORT', `failed to export to '${outReal}': ${(e as Error).message}`, undefined, { cause: e });
  }
  return { outFile: outReal, bytes: data.byteLength };
}

interface ReplacementStamp {
  readonly size: bigint;
  readonly ctimeNs: bigint;
}

async function replacementStampOf(p: string): Promise<ReplacementStamp | undefined> {
  try {
    const st = await fs.lstat(p, { bigint: true });
    return { size: st.size, ctimeNs: st.ctimeNs };
  } catch {
    return undefined;
  }
}

function sameReplacementStamp(a: ReplacementStamp | undefined, b: ReplacementStamp | undefined): boolean {
  return a !== undefined && b !== undefined && a.size === b.size && a.ctimeNs === b.ctimeNs;
}

/** Remove `p` only if it is a regular file with the identity we created. */
async function removeFileIfSame(p: string, expected: FileId): Promise<void> {
  try {
    const st = await lstatOrUndefined(p);
    if (!st || st.isSymbolicLink() || !st.isFile() || !sameId(idOf(st), expected)) return;
    await fs.unlink(p);
  } catch {
    // best effort
  }
}

interface FileId {
  readonly dev: number | bigint;
  readonly ino: number | bigint;
}

function idOf(st: { dev: number | bigint; ino: number | bigint }): FileId {
  return { dev: st.dev, ino: st.ino };
}

function sameId(a: FileId, b: FileId): boolean {
  return a.dev === b.dev && a.ino === b.ino;
}

function unsafe(message: string): SitefileError {
  return new SitefileError('E_EXPORT_UNSAFE', message);
}

/** The parent must be a real directory (no symlink in its path); return its identity. */
async function verifiedParentId(parent: string): Promise<FileId> {
  const real = await fs.realpath(parent);
  if (real !== parent) throw unsafe(`export parent '${parent}' resolves to '${real}'; aborting`);
  const st = await fs.stat(parent);
  if (!st.isDirectory()) throw unsafe(`export parent '${parent}' is not a directory`);
  return idOf(st);
}

async function assertParentUnchanged(parent: string, expected: FileId): Promise<void> {
  let now: FileId;
  try {
    now = await verifiedParentId(parent);
  } catch (e) {
    if (e instanceof SitefileError) throw e;
    throw unsafe(`export parent '${parent}' changed during export: ${(e as Error).message}`);
  }
  if (!sameId(now, expected)) throw unsafe(`export parent '${parent}' was replaced during export; aborting`);
}

async function assertSameId(p: string, expected: FileId, what: string): Promise<void> {
  const st = await lstatOrUndefined(p);
  if (!st || st.isSymbolicLink() || !st.isDirectory() || !sameId(idOf(st), expected)) {
    throw unsafe(`${what} '${p}' changed during export; aborting`);
  }
}

/**
 * Recursively remove `p` only if it is a real directory whose dev/ino equals
 * `expected`. With no `expected` (freshly created, still empty), only an empty
 * real directory is removed. Never follows or deletes through a symlink.
 */
async function removeIfSame(p: string, expected: FileId | undefined): Promise<void> {
  try {
    const st = await lstatOrUndefined(p);
    if (!st || st.isSymbolicLink() || !st.isDirectory()) return;
    if (expected === undefined) {
      await fs.rmdir(p);
      return;
    }
    if (!sameId(idOf(st), expected)) return;
    await fs.rm(p, { recursive: true, force: true });
  } catch {
    // best effort
  }
}

function assertSafeDestination(out: string, contextReal: string, home: string): void {
  if (out === path.parse(out).root) {
    throw new SitefileError('E_EXPORT_UNSAFE', `refusing to export to the filesystem root '${out}'`);
  }
  if (out === contextReal) {
    throw new SitefileError('E_EXPORT_UNSAFE', `refusing to export to the build context itself ('${out}')`);
  }
  if (isInside(contextReal, out)) {
    throw new SitefileError('E_EXPORT_UNSAFE', `refusing to export to '${out}': it contains the build context '${contextReal}'`);
  }
  if (out === home || isInside(home, out)) {
    throw new SitefileError('E_EXPORT_UNSAFE', `refusing to export to '${out}': it is or contains the home directory`);
  }
}

function randomHex(): string {
  return randomBytes(6).toString('hex');
}

/** True if `child` is strictly inside `dir`. */
function isInside(child: string, dir: string): boolean {
  const rel = path.relative(dir, child);
  return rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel);
}

async function lstatOrUndefined(p: string): Promise<import('node:fs').Stats | undefined> {
  try {
    return await fs.lstat(p);
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    if (code === 'ENOENT' || code === 'ENOTDIR') return undefined;
    throw e;
  }
}

/**
 * Resolve the real location of `abs` without following a final symlink:
 * realpath the nearest existing ancestor of its parent and rejoin the rest.
 */
async function realpathMissing(abs: string): Promise<string> {
  const root = path.parse(abs).root;
  if (abs === root) return abs;
  const rest: string[] = [path.basename(abs)];
  let dir = path.dirname(abs);
  for (;;) {
    try {
      const real = await fs.realpath(dir);
      return path.join(real, ...rest);
    } catch (e) {
      const code = (e as NodeJS.ErrnoException).code;
      if (code !== 'ENOENT' && code !== 'ENOTDIR') throw e;
      if (dir === root) return path.join(root, ...rest);
      rest.unshift(path.basename(dir));
      dir = path.dirname(dir);
    }
  }
}

async function realpathOr(p: string, what: string): Promise<string> {
  try {
    return await fs.realpath(p);
  } catch (e) {
    throw new SitefileError('E_EXPORT', `cannot resolve ${what} '${p}': ${(e as Error).message}`, undefined, {
      cause: e,
    });
  }
}

async function realpathOrRaw(p: string): Promise<string> {
  try {
    return await fs.realpath(p);
  } catch {
    return path.resolve(p);
  }
}
