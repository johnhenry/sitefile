import { posix } from 'node:path';
import { SitefileError, type SourceSpan } from './errors.js';

export interface VirtualPath {
  /** Normalized absolute POSIX path inside the artifact, `/` for the root. */
  readonly path: string;
  /** The original ended with `/` (or was the root). */
  readonly trailingSlash: boolean;
}

/**
 * Normalize an artifact (virtual) path. Must be absolute; `.` and repeated
 * slashes collapse; any `..` segment is rejected outright.
 */
export function normalizeVirtual(raw: string, span?: SourceSpan, what = 'path'): VirtualPath {
  if (raw.includes('\0')) throw new SitefileError('E_PATH', `${what} contains a NUL byte`, span);
  if (!raw.startsWith('/')) {
    throw new SitefileError('E_PATH', `${what} '${raw}' must be an absolute artifact path (start with '/')`, span);
  }
  const segments: string[] = [];
  for (const seg of raw.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') throw new SitefileError('E_PATH', `${what} '${raw}' must not contain '..'`, span);
    segments.push(seg);
  }
  const path = '/' + segments.join('/');
  return { path, trailingSlash: raw.endsWith('/') || path === '/' };
}

/**
 * Normalize a host path relative to the build context. Returns a relative
 * POSIX path (`''` for the context root). Rejects absolute paths and anything
 * that escapes the context with `..`.
 */
export function normalizeLocal(raw: string, span?: SourceSpan): { path: string; trailingSlash: boolean } {
  if (raw.includes('\0')) throw new SitefileError('E_PATH', 'path contains a NUL byte', span);
  if (raw.startsWith('/') || /^[A-Za-z]:[\\/]/.test(raw) || raw.startsWith('\\')) {
    throw new SitefileError('E_PATH_ESCAPE', `local path '${raw}' must be relative to the build context`, span);
  }
  const norm = posix.normalize(raw === '' ? '.' : raw);
  if (norm === '..' || norm.startsWith('../')) {
    throw new SitefileError('E_PATH_ESCAPE', `local path '${raw}' escapes the build context`, span);
  }
  const path = norm === '.' || norm === './' ? '' : norm.replace(/\/+$/, '');
  return { path, trailingSlash: raw.endsWith('/') };
}

export function joinVirtual(dir: string, name: string): string {
  return dir === '/' ? `/${name}` : `${dir}/${name}`;
}

export function basenameVirtual(p: string): string {
  return posix.basename(p);
}

/** Ancestors of a virtual path, excluding the root and the path itself. */
export function ancestorsVirtual(p: string): string[] {
  const out: string[] = [];
  let i = p.indexOf('/', 1);
  while (i !== -1) {
    out.push(p.slice(0, i));
    i = p.indexOf('/', i + 1);
  }
  return out;
}
