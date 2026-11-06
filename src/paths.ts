import { posix } from 'node:path';
import { SitefileError, type SourceSpan } from './errors.js';
import { CASEFOLD_RUNS } from './casefold-data.js';

const ASCII = /^[\x00-\x7f]*$/;

/**
 * Unicode NFC. Every virtual path (Sitefile operands, names read from the
 * context, archive entries, plugin outputs) is stored in NFC, so NFC and NFD
 * spellings of a name are the same path on every OS.
 */
export function nfc(s: string): string {
  return ASCII.test(s) ? s : s.normalize('NFC');
}

function foldCodePoint(c: number): number {
  let lo = 0;
  let hi = CASEFOLD_RUNS.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [start, end, delta, step] = CASEFOLD_RUNS[mid]!;
    if (c < start) hi = mid - 1;
    else if (c > end) lo = mid + 1;
    else return (c - start) % step === 0 ? c + delta : c;
  }
  return c;
}

/**
 * Unicode simple case folding (CaseFolding.txt status C and S, one code
 * point to one code point; no full or Turkic mappings), applied code point
 * by code point. Two paths with the same fold differ only in case.
 * `rust/src/paths.rs` uses the same generated table.
 */
export function foldCase(s: string): string {
  if (ASCII.test(s)) return s.toLowerCase();
  let out = '';
  for (const ch of s) out += String.fromCodePoint(foldCodePoint(ch.codePointAt(0)!));
  return out;
}

export interface VirtualPath {
  /** Normalized absolute POSIX path inside the artifact, `/` for the root. */
  readonly path: string;
  /** The original ended with `/` (or was the root). */
  readonly trailingSlash: boolean;
}

/**
 * Normalize an artifact (virtual) path. Must be absolute; `.` and repeated
 * slashes collapse; any `..` segment is rejected outright; the result is NFC.
 */
export function normalizeVirtual(raw: string, span?: SourceSpan, what = 'path'): VirtualPath {
  raw = nfc(raw);
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
 * that escapes the context with `..`. The result is NFC; LocalContext finds
 * the file whatever normalization its on-disk name uses.
 */
export function normalizeLocal(raw: string, span?: SourceSpan): { path: string; trailingSlash: boolean } {
  raw = nfc(raw);
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
  name = nfc(name);
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
