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
}

export interface ExportResult {
  /** Real absolute path the tree was written to. */
  readonly outDir: string;
  readonly files: number;
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
  const planned: Array<{ segments: string[]; bytes: Uint8Array; mode: number }> = [];
  for (const [p, node] of entries) {
    if (!p.startsWith('/')) throw new SitefileError('E_EXPORT', `invalid artifact path '${p}': not absolute`);
    const segments = p.slice(1).split('/');
    for (const seg of segments) {
      if (seg === '' || seg === '.' || seg === '..' || seg.includes('\0') || seg.includes('\\')) {
        throw new SitefileError('E_EXPORT', `invalid artifact path '${p}'`);
      }
    }
    planned.push({ segments, bytes: node.bytes, mode: node.mode });
  }

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
    for (const { segments, bytes, mode } of planned) {
      const target = path.join(tmp, ...segments);
      await fs.mkdir(path.dirname(target), { recursive: true });
      await fs.writeFile(target, bytes, { flag: 'wx' });
      await fs.chmod(target, mode);
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
  return { outDir: outReal, files: planned.length };
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
