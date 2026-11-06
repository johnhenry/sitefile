import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { SitefileError } from '../errors.js';
import { fetchGitCommit, parseGitUri, selectGitPath, type GitLocation } from '../resources/git.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits, type FetchPolicy } from '../resources/types.js';
import { Tree, makeFile } from '../tree.js';

/**
 * `sitefile build https://host/repo.git#ref:dir` (issue #91): a build context fetched from Git.
 * A context argument with a URL scheme (`https://`, `http://`, `git+https://`, `git+http://`) is a
 * Git URL. Its tree is fetched like a Git IMPORT (same network policy; symlinks resolved inside the
 * tree, submodules refused), written to a temporary directory, built from there, and removed.
 */
export function isGitContext(context: string): boolean {
  return /^[A-Za-z][A-Za-z0-9+.-]*:\/\//.test(context);
}

/** The location of a Git context argument (`https://…` is read as `git+https://…`). */
export function gitContextLocation(context: string): GitLocation {
  const uri = /^git\+/i.test(context) ? context : `git+${context}`;
  const loc = parseGitUri(uri);
  return { ...loc, url: context };
}

export interface GitContext {
  readonly dir: string;
  readonly commit: string;
  dispose(): Promise<void>;
}

/** Fetch the context's tree and write it to a new temporary directory. */
export async function materializeGitContext(context: string, policy: FetchPolicy, limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS): Promise<GitContext> {
  const loc = gitContextLocation(context);
  const objs = await fetchGitCommit(loc.repo, loc.ref, loc.commit, { policy }, limits);
  const sel = selectGitPath(objs, loc, limits);
  if (sel.kind !== 'tree') throw new SitefileError('E_TYPE', `build context '${context}' is a file; a Git context must name a directory`);
  // Same checks as an IMPORT of the tree (NFC, case collisions), before anything is written.
  const tree = Tree.fromFiles(sel.files.map(([p, f]) => [p, makeFile(f.bytes, context, f.mode)] as const));
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-context-'));
  const dispose = (): Promise<void> => fs.rm(dir, { recursive: true, force: true });
  try {
    for (const [p, n] of tree.entries()) {
      const dest = path.join(dir, ...p.slice(1).split('/'));
      await fs.mkdir(path.dirname(dest), { recursive: true });
      await fs.writeFile(dest, n.bytes, { mode: n.mode });
      await fs.chmod(dest, n.mode);
    }
  } catch (e) {
    await dispose();
    throw new SitefileError('E_EXPORT', `cannot write the Git context to a temporary directory: ${(e as Error).message}`, undefined, { cause: e });
  }
  return { dir, commit: objs.commit, dispose };
}
