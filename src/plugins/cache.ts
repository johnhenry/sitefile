import { createHash } from 'node:crypto';
import { mkdir, readFile, readdir, rename, rm, stat, utimes, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { sha256 } from '../digest.js';
import type { PluginResult } from './host.js';

/**
 * Content-addressed cache of TRANSFORM results (see docs/PLUGINS.md, "Caching").
 *
 *   <dir>/entries/<key>.json   manifest: which blobs make up the result
 *   <dir>/blobs/<hex>          result bytes, named by their SHA-256
 *
 * The key is the SHA-256 of a canonical JSON document describing everything the
 * result can depend on. Reads verify every blob's digest and size; a damaged
 * entry is removed and counts as a miss, so the next put rewrites it.
 */

/** Bump to invalidate every existing entry when the key or entry format changes. */
export const CACHE_VERSION = 1;

interface Manifest {
  readonly version: number;
  readonly kind: 'file' | 'files';
  /** `[path, blob digest, size]`; a `file` result has exactly one entry with path ''. */
  readonly files: ReadonlyArray<readonly [string, string, number]>;
}

export interface CacheStats {
  hits: number;
  misses: number;
  writes: number;
  /** Entries found damaged (and discarded). */
  corrupt: number;
}

export interface CacheInfo {
  readonly dir: string;
  readonly entries: number;
  readonly blobs: number;
  readonly bytes: number;
}

/** JSON with sorted object keys, so equal values always serialize equally. */
export function canonicalJson(value: unknown): string {
  const norm = (v: unknown): unknown => {
    if (v === null || typeof v !== 'object') return typeof v === 'bigint' ? v.toString() : v;
    if (v instanceof Date) return v.toISOString();
    if (Array.isArray(v)) return v.map(norm);
    const out: Record<string, unknown> = {};
    for (const k of Object.keys(v).sort()) {
      const x = (v as Record<string, unknown>)[k];
      if (x !== undefined) out[k] = norm(x);
    }
    return out;
  };
  return JSON.stringify(norm(value)) ?? 'null';
}

export function cacheKey(parts: object): string {
  return createHash('sha256').update(canonicalJson({ cacheVersion: CACHE_VERSION, ...parts })).digest('hex');
}

const HEX64 = /^[0-9a-f]{64}$/;

export class TransformCache {
  readonly stats: CacheStats = { hits: 0, misses: 0, writes: 0, corrupt: 0 };

  constructor(readonly dir: string) {}

  private entryPath(key: string): string {
    return path.join(this.dir, 'entries', `${key}.json`);
  }
  private blobPath(hex: string): string {
    return path.join(this.dir, 'blobs', hex);
  }

  async get(key: string): Promise<PluginResult | undefined> {
    const file = this.entryPath(key);
    let raw: string;
    try {
      raw = await readFile(file, 'utf8');
    } catch {
      this.stats.misses++;
      return undefined;
    }
    const result = await this.load(raw);
    if (!result) {
      this.stats.corrupt++;
      this.stats.misses++;
      await rm(file, { force: true });
      return undefined;
    }
    this.stats.hits++;
    const now = new Date();
    await utimes(file, now, now).catch(() => {});
    return result;
  }

  private async load(raw: string): Promise<PluginResult | undefined> {
    let m: Manifest;
    try {
      m = JSON.parse(raw) as Manifest;
    } catch {
      return undefined;
    }
    if (m?.version !== CACHE_VERSION || (m.kind !== 'file' && m.kind !== 'files') || !Array.isArray(m.files)) return undefined;
    if (m.kind === 'file' && m.files.length !== 1) return undefined;
    const loaded: Array<readonly [string, Uint8Array]> = [];
    for (const f of m.files) {
      if (!Array.isArray(f) || typeof f[0] !== 'string' || typeof f[1] !== 'string' || !HEX64.test(f[1]) || typeof f[2] !== 'number') return undefined;
      let bytes: Buffer;
      try {
        bytes = await readFile(this.blobPath(f[1]));
      } catch {
        return undefined;
      }
      if (bytes.byteLength !== f[2] || sha256(bytes) !== `sha256:${f[1]}`) {
        await rm(this.blobPath(f[1]), { force: true });
        return undefined;
      }
      loaded.push([f[0], new Uint8Array(bytes)]);
    }
    return m.kind === 'file' ? { kind: 'file', bytes: loaded[0]![1] } : { kind: 'files', files: loaded };
  }

  async put(key: string, result: PluginResult): Promise<void> {
    try {
      const pairs: ReadonlyArray<readonly [string, Uint8Array]> = result.kind === 'file' ? [['', result.bytes]] : result.files;
      const files: Array<readonly [string, string, number]> = [];
      for (const [p, bytes] of pairs) {
        const hex = sha256(bytes).slice('sha256:'.length);
        await this.writeAtomic(this.blobPath(hex), bytes, true);
        files.push([p, hex, bytes.byteLength]);
      }
      const manifest: Manifest = { version: CACHE_VERSION, kind: result.kind, files };
      await this.writeAtomic(this.entryPath(key), JSON.stringify(manifest), false);
      this.stats.writes++;
    } catch {
      // A cache that cannot be written (read-only home, full disk) must not fail the build.
    }
  }

  private async writeAtomic(file: string, data: string | Uint8Array, keepIfSameSize: boolean): Promise<void> {
    if (keepIfSameSize) {
      const size = typeof data === 'string' ? Buffer.byteLength(data) : data.byteLength;
      const st = await stat(file).catch(() => undefined);
      if (st?.isFile() && st.size === size) {
        // Reused: refresh its mtime so the size limit's grace period covers it until the entry lands.
        const now = new Date();
        await utimes(file, now, now).catch(() => {});
        return;
      }
    }
    await mkdir(path.dirname(file), { recursive: true });
    const tmp = `${file}.${process.pid}.${Math.random().toString(36).slice(2)}.tmp`;
    await writeFile(tmp, data);
    await rename(tmp, file);
  }

  async info(): Promise<CacheInfo> {
    const entries = (await readdir(path.join(this.dir, 'entries')).catch(() => [] as string[])).filter((n) => n.endsWith('.json')).length;
    let blobs = 0;
    let bytes = 0;
    for (const n of await readdir(path.join(this.dir, 'blobs')).catch(() => [] as string[])) {
      const st = await stat(path.join(this.dir, 'blobs', n)).catch(() => undefined);
      if (st?.isFile()) {
        blobs++;
        bytes += st.size;
      }
    }
    return { dir: this.dir, entries, blobs, bytes };
  }

  /**
   * Remove entries not used for `olderThanMs` (0 removes all), then every blob
   * no remaining entry references. Returns what was removed.
   */
  async prune(olderThanMs = 0, now = Date.now()): Promise<{ entries: number; blobs: number; bytes: number }> {
    let removedEntries = 0;
    const live = new Set<string>();
    for (const n of await readdir(path.join(this.dir, 'entries')).catch(() => [] as string[])) {
      const file = path.join(this.dir, 'entries', n);
      const st = await stat(file).catch(() => undefined);
      if (!st) continue;
      if (olderThanMs === 0 || now - st.mtimeMs >= olderThanMs || !n.endsWith('.json')) {
        await rm(file, { force: true });
        removedEntries++;
        continue;
      }
      try {
        for (const f of (JSON.parse(await readFile(file, 'utf8')) as Manifest).files) live.add(f[1]);
      } catch {
        await rm(file, { force: true });
        removedEntries++;
      }
    }
    let removedBlobs = 0;
    let bytes = 0;
    for (const n of await readdir(path.join(this.dir, 'blobs')).catch(() => [] as string[])) {
      if (live.has(n)) continue;
      const file = path.join(this.dir, 'blobs', n);
      const st = await stat(file).catch(() => undefined);
      await rm(file, { force: true });
      removedBlobs++;
      bytes += st?.size ?? 0;
    }
    return { entries: removedEntries, blobs: removedBlobs, bytes };
  }
}

/** Digest of every regular file under `dir` (a plugin can read all of them, so any may change its output). */
export async function digestFolder(dir: string, limits = { files: 2000, bytes: 128 * 1024 * 1024 }): Promise<string | undefined> {
  const h = createHash('sha256');
  let files = 0;
  let bytes = 0;
  const walk = async (rel: string): Promise<boolean> => {
    const entries = (await readdir(path.join(dir, rel), { withFileTypes: true })).sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    for (const e of entries) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) {
        if (!(await walk(r))) return false;
      } else if (e.isFile()) {
        const data = await readFile(path.join(dir, r));
        if (++files > limits.files || (bytes += data.byteLength) > limits.bytes) return false;
        h.update(`${r}\0${sha256(data)}\n`);
      }
    }
    return true;
  };
  return (await walk('')) ? h.digest('hex') : undefined;
}
