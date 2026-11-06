import { lstat, readdir, readFile, rm } from 'node:fs/promises';
import path from 'node:path';
import { Store } from './store.js';

/**
 * Size limit for what Sitefile keeps under $SITEFILE_HOME (docs/CLI.md, "Cache size limit").
 *
 * EVICTABLE data is the TRANSFORM result cache (cache/transform), the compiled wasm module cache
 * (cache/wasm, written by the Rust build only) and, in the image store, untagged images and blobs
 * no tagged image uses. Tagged images and every blob they reference are never evicted.
 *
 * After a build, when the evictable bytes exceed the limit ($SITEFILE_CACHE_MAX, default 1 GiB,
 * 0 = no limit), the least recently used items are deleted until it fits. Recency is the file's
 * mtime, refreshed on every cache hit (atime is often disabled): a TRANSFORM entry, a .cwasm file,
 * an untagged image's manifest blob. Nothing touched within the grace period is deleted (10 minutes
 * for the pure caches, 1 hour for the image store and for temp files), so a concurrent build never
 * loses what it is writing or has just written; writes are temp file + rename, and the store part
 * runs under the store lock. The check is one directory listing per cache when under the limit.
 *
 * Kept byte-for-byte in step with rust/src/cache_limit.rs.
 */

export const CACHE_MAX_ENV = 'SITEFILE_CACHE_MAX';
export const DEFAULT_CACHE_MAX = 1024 ** 3;
/** Files touched more recently than this are never evicted from the pure caches. */
export const CACHE_GRACE_MS = 10 * 60_000;
/** Temp files and image-store data get a longer grace (pulls and builds can take a while). */
export const LONG_GRACE_MS = 60 * 60_000;

const HEX64 = /^[0-9a-f]{64}$/;
const UNITS: Record<string, number> = { '': 1, k: 1024, m: 1024 ** 2, g: 1024 ** 3, t: 1024 ** 4 };

/** `1048576`, `512K`, `512M`, `1.5G`, `2GiB`, `1T` (binary units, case-insensitive); `0` = no limit. */
export function parseSize(raw: string): number | undefined {
  const m = /^(\d+(?:\.\d+)?)\s*(?:([kmgt])(?:i?b)?|b)?$/i.exec(raw.trim());
  if (!m) return undefined;
  const n = Math.floor(Number(m[1]) * (UNITS[(m[2] ?? '').toLowerCase()] ?? 1));
  return Number.isSafeInteger(n) ? n : undefined;
}

/** A limit as it is shown: `unlimited`, `1GiB`, `512MiB`, `100KiB`, else bytes (`1000B`). */
export function formatLimit(bytes: number): string {
  if (bytes === 0) return 'unlimited';
  for (const [u, n] of [['TiB', 1024 ** 4], ['GiB', 1024 ** 3], ['MiB', 1024 ** 2], ['KiB', 1024]] as const) {
    if (bytes % n === 0) return `${bytes / n}${u}`;
  }
  return `${bytes}B`;
}

/** The limit from $SITEFILE_CACHE_MAX (unset or empty: the default). A bad value warns and uses the default. */
export function cacheLimitFromEnv(env: NodeJS.ProcessEnv = process.env, warn: (m: string) => void = () => {}): number {
  const raw = env[CACHE_MAX_ENV];
  if (raw === undefined || raw.trim() === '') return DEFAULT_CACHE_MAX;
  const n = parseSize(raw);
  if (n !== undefined) return n;
  warn(`${CACHE_MAX_ENV}='${raw}' is not a size (bytes, or a number with K, M, G or T; 0 for no limit); using ${formatLimit(DEFAULT_CACHE_MAX)}`);
  return DEFAULT_CACHE_MAX;
}

// ---------------------------------------------------------------- model

interface Blob {
  readonly file: string;
  readonly size: number;
  readonly mtime: number;
  readonly grace: number;
  refs: number;
  gone: boolean;
}

interface Root {
  readonly key: string;
  /** Own file (a TRANSFORM entry), or undefined for an untagged image (its manifest is a blob). */
  readonly file: string | undefined;
  readonly size: number;
  readonly mtime: number;
  readonly grace: number;
  readonly refs: readonly string[];
  /** Image ID to drop from the store index. */
  readonly image?: string;
}

interface FileStat {
  readonly name: string;
  readonly file: string;
  readonly size: number;
  readonly mtime: number;
}

async function files(dir: string): Promise<FileStat[]> {
  const names = await readdir(dir).catch(() => [] as string[]);
  const out: FileStat[] = [];
  for (const name of names.sort()) {
    const file = path.join(dir, name);
    const st = await lstat(file).catch(() => undefined);
    if (st?.isFile()) out.push({ name, file, size: st.size, mtime: st.mtimeMs });
  }
  return out;
}

export interface CacheUsage {
  readonly name: 'transform' | 'wasm' | 'store' | 'servers';
  readonly path: string;
  /** TRANSFORM entries, .cwasm modules, store blobs, server files. */
  readonly items: number;
  readonly bytes: number;
  /** Bytes the size limit may delete (everything but tagged images' blobs and server state). */
  readonly evictable: number;
}

export function cachePaths(home: string): Record<CacheUsage['name'], string> {
  return {
    transform: path.join(home, 'cache', 'transform'),
    wasm: path.join(home, 'cache', 'wasm'),
    store: path.join(home, 'blobs', 'sha256'),
    servers: path.join(home, 'servers'),
  };
}

class Model {
  readonly blobs = new Map<string, Blob>();
  readonly roots: Root[] = [];
  /** Bytes in the model (all evictable). */
  total = 0;

  blob(key: string, f: FileStat, grace: number): void {
    this.blobs.set(key, { file: f.file, size: f.size, mtime: f.mtime, grace, refs: 0, gone: false });
    this.total += f.size;
  }

  root(r: Root): void {
    this.roots.push(r);
    this.total += r.size;
    for (const k of r.refs) {
      const b = this.blobs.get(k);
      if (b) b.refs++;
    }
  }
}

async function addTransform(m: Model, dir: string): Promise<number> {
  for (const f of await files(path.join(dir, 'blobs'))) m.blob(`t:${f.name}`, f, HEX64.test(f.name) ? CACHE_GRACE_MS : LONG_GRACE_MS);
  let entries = 0;
  for (const f of await files(path.join(dir, 'entries'))) {
    if (!f.name.endsWith('.json') || f.name.startsWith('.')) {
      m.blob(`t:entries/${f.name}`, f, LONG_GRACE_MS);
      continue;
    }
    entries++;
    let refs: string[] = [];
    try {
      const j = JSON.parse(await readFile(f.file, 'utf8')) as { files?: unknown };
      if (Array.isArray(j.files)) refs = j.files.flatMap((x) => (Array.isArray(x) && typeof x[1] === 'string' && HEX64.test(x[1]) ? [`t:${x[1]}`] : []));
    } catch {
      /* damaged: an entry with no blobs */
    }
    m.root({ key: `t:entries/${f.name}`, file: f.file, size: f.size, mtime: f.mtime, grace: CACHE_GRACE_MS, refs: [...new Set(refs)] });
  }
  return entries;
}

async function addWasm(m: Model, dir: string): Promise<number> {
  let modules = 0;
  for (const f of await files(dir)) {
    const ok = f.name.endsWith('.cwasm') && HEX64.test(f.name.slice(0, -'.cwasm'.length));
    if (ok) modules++;
    m.blob(`w:${f.name}`, f, ok ? CACHE_GRACE_MS : LONG_GRACE_MS);
  }
  return modules;
}

/** Adds the store's evictable part; returns [blob count, total bytes]. */
async function addStore(m: Model, store: Store, dir: string): Promise<[number, number]> {
  const all = await files(dir);
  const view = await store.gcView();
  for (const f of all) if (!view.live.has(f.name)) m.blob(`s:${f.name}`, f, LONG_GRACE_MS);
  for (const im of view.untagged) {
    const manifest = m.blobs.get(`s:${im.id.slice(7)}`);
    m.root({
      key: `s:image/${im.id}`,
      file: undefined,
      size: 0,
      mtime: manifest?.mtime ?? 0,
      grace: LONG_GRACE_MS,
      refs: [...new Set(im.blobs.filter((h) => !view.live.has(h)).map((h) => `s:${h}`))],
      image: im.id,
    });
  }
  return [all.length, all.reduce((a, f) => a + f.size, 0)];
}

async function sizeOf(dir: string): Promise<[number, number]> {
  const f = await files(dir);
  return [f.length, f.reduce((a, x) => a + x.size, 0)];
}

/** Sizes per cache under `home` (for `cache du`). */
export async function cacheUsage(home: string): Promise<CacheUsage[]> {
  const p = cachePaths(home);
  const t = new Model();
  const entries = await addTransform(t, p.transform);
  const w = new Model();
  const modules = await addWasm(w, p.wasm);
  const s = new Model();
  const store = await Store.openExisting(home);
  let storeItems = 0;
  let storeBytes = 0;
  if (store) [storeItems, storeBytes] = await addStore(s, store, p.store);
  const [serverItems, serverBytes] = await sizeOf(p.servers);
  return [
    { name: 'transform', path: p.transform, items: entries, bytes: t.total, evictable: t.total },
    { name: 'wasm', path: p.wasm, items: modules, bytes: w.total, evictable: w.total },
    { name: 'store', path: p.store, items: storeItems, bytes: storeBytes, evictable: s.total },
    { name: 'servers', path: p.servers, items: serverItems, bytes: serverBytes, evictable: 0 },
  ];
}

export interface EvictResult {
  /** TRANSFORM entries, modules, images and stray blobs deleted. */
  readonly items: number;
  readonly bytes: number;
  /** Evictable bytes before and after. */
  readonly before: number;
  readonly after: number;
}

/**
 * Delete least recently used evictable data until it fits in `limit` bytes (0: no limit).
 * Cheap when under the limit: one listing per cache directory, no file is read.
 */
export async function enforceCacheLimit(home: string, limit: number, now: number = Date.now()): Promise<EvictResult> {
  if (limit === 0) return { items: 0, bytes: 0, before: 0, after: 0 };
  const p = cachePaths(home);
  // Upper bound first (it counts tagged images too): under the limit, nothing more to do.
  let bound = 0;
  for (const d of [path.join(p.transform, 'blobs'), path.join(p.transform, 'entries'), p.wasm, p.store]) bound += (await sizeOf(d))[1];
  if (bound <= limit) return { items: 0, bytes: 0, before: bound, after: bound };
  const store = await Store.openExisting(home);
  const run = async (): Promise<EvictResult> => {
    const m = new Model();
    await addTransform(m, p.transform);
    await addWasm(m, p.wasm);
    if (store) await addStore(m, store, p.store);
    const before = m.total;
    let items = 0;
    type Cand = { mtime: number; key: string; root?: Root; blob?: Blob };
    const cands: Cand[] = [
      ...m.roots.map((r) => ({ mtime: r.mtime, key: r.key, root: r })),
      ...[...m.blobs].filter(([, b]) => b.refs === 0).map(([key, b]) => ({ mtime: b.mtime, key, blob: b })),
    ];
    cands.sort((a, b) => a.mtime - b.mtime || (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
    const drop = async (b: Blob): Promise<void> => {
      await rm(b.file, { force: true });
      b.gone = true;
      m.total -= b.size;
    };
    for (const c of cands) {
      if (m.total <= limit) break;
      if (c.root) {
        const r = c.root;
        if (now - r.mtime < r.grace) continue;
        if (r.image && store) await store.dropUntaggedLocked(r.image);
        if (r.file) await rm(r.file, { force: true });
        m.total -= r.size;
        items++;
        for (const k of r.refs) {
          const b = m.blobs.get(k);
          if (!b || b.gone) continue;
          b.refs--;
          if (b.refs === 0 && now - b.mtime >= b.grace) await drop(b);
        }
      } else if (c.blob && !c.blob.gone && c.blob.refs === 0 && now - c.blob.mtime >= c.blob.grace) {
        await drop(c.blob);
        items++;
      }
    }
    return { items, bytes: before - m.total, before, after: m.total };
  };
  return store ? store.withLock(run) : run();
}
