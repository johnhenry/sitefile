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
  const parent = path.dirname(outReal);
  const base = path.basename(outReal);
  const tmp = path.join(parent, `.${base}.sitefile-tmp-${randomHex()}`);
  try {
    await fs.mkdir(parent, { recursive: true });
    await fs.mkdir(tmp);
  } catch (e) {
    throw new SitefileError('E_EXPORT', `cannot create '${tmp}': ${(e as Error).message}`, undefined, { cause: e });
  }

  let oldMoved: string | undefined;
  let swapped = false;
  try {
    for (const { segments, bytes, mode } of planned) {
      const target = path.join(tmp, ...segments);
      await fs.mkdir(path.dirname(target), { recursive: true });
      await fs.writeFile(target, bytes, { flag: 'wx' });
      await fs.chmod(target, mode);
    }
    if (st) {
      const old = path.join(parent, `.${base}.sitefile-old-${randomHex()}`);
      await fs.rename(outReal, old);
      oldMoved = old;
    }
    await fs.rename(tmp, outReal);
    swapped = true;
  } catch (e) {
    await fs.rm(tmp, { recursive: true, force: true }).catch(() => {});
    if (oldMoved && !swapped) {
      await fs.rename(oldMoved, outReal).catch(() => {});
    }
    if (e instanceof SitefileError) throw e;
    throw new SitefileError('E_EXPORT', `failed to export to '${outReal}': ${(e as Error).message}`, undefined, {
      cause: e,
    });
  }

  if (oldMoved) {
    // Best effort: the new output is already in place.
    await fs.rm(oldMoved, { recursive: true, force: true }).catch(() => {});
  }
  return { outDir: outReal, files: planned.length };
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
