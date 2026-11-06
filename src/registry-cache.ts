import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { mkdir, open, rename, rm, stat, utimes } from 'node:fs/promises';
import path from 'node:path';
import { CACHE_MAX_ENV, formatLimit } from './cache-limit.js';
import { sha256 } from './digest.js';
import { SitefileError, isSitefileError } from './errors.js';
import { canonicalJson, MEDIA, parseManifest } from './oci.js';
import { canonicalRef, parseRef, type ImageRef } from './ref.js';
import type { RegistryClient } from './registry.js';
import type { CacheEntry, RemoteCache } from './plugins/cache.js';

/**
 * TRANSFORM cache shared through an OCI registry (`build --cache-from <ref>` / `--cache-to <ref>`,
 * docs/CLI.md "Registry cache"). The cache is one OCI artifact:
 *
 *   manifest  application/vnd.oci.image.manifest.v1+json, artifactType CACHE_ARTIFACT_TYPE,
 *             config = the empty descriptor ({}), layers = [index, ...result blobs by digest]
 *   index     CACHE_INDEX_MEDIA: canonical JSON {"entries":{"<key>":{"files":[[path,hex,size]],"kind":k}},"version":1}
 *   blobs     CACHE_BLOB_MEDIA: result bytes, exactly as in the local cache's blobs/ directory
 *
 * `--cache-from` reads the manifest and the index before the build; a result's blobs are fetched
 * only on a hit (a local miss whose key is in the index), verified, and written to the local cache.
 * `--cache-to` pushes, after a successful build, the entries this build used or wrote; blobs the
 * registry already has are not uploaded again. A registry problem is a warning, never a build failure.
 * Pushed bytes are deterministic and hold no local paths or secrets: keys are SHA-256 hashes, entries
 * name output-relative paths, digests and sizes only.
 *
 * Kept byte-for-byte in step with rust/src/registry_cache.rs.
 */

export const CACHE_ARTIFACT_TYPE = 'application/vnd.sitefile.cache.v1+json';
export const CACHE_INDEX_MEDIA = 'application/vnd.sitefile.cache.index.v1+json';
export const CACHE_BLOB_MEDIA = 'application/vnd.sitefile.cache.blob.v1';
export const EMPTY_MEDIA = 'application/vnd.oci.empty.v1+json';
/** Largest cache index accepted. */
export const MAX_INDEX_BYTES = 64 * 1024 ** 2;
const INDEX_VERSION = 1;
const EMPTY = Buffer.from('{}');
const HEX64 = /^[0-9a-f]{64}$/;

export interface CacheRef {
  /** As written on the command line (used in messages). */
  readonly input: string;
  readonly ref: ImageRef;
}

/** Validate a `--cache-from` / `--cache-to` reference (E_USAGE when it is not usable). */
export function parseCacheRef(flag: '--cache-from' | '--cache-to', input: string): CacheRef {
  const r = parseRef(input);
  if (r.registry === undefined) throw new SitefileError('E_USAGE', `${flag} '${input}' has no registry host (e.g. localhost:5000/team/site-cache:main)`);
  if (flag === '--cache-to' && r.digest !== undefined) throw new SitefileError('E_USAGE', `${flag} '${input}' must name a tag, not a digest`);
  return { input, ref: canonicalRef(r) };
}

export interface RegistryCacheOptions {
  readonly from: readonly CacheRef[];
  readonly to?: CacheRef | undefined;
  /** A client for a canonical registry name (auth, TLS and network policy are the client's). */
  readonly client: (registry: string) => RegistryClient;
  /** $SITEFILE_CACHE_MAX in bytes (0: no limit): a build never fetches more than this. */
  readonly limit: number;
  readonly warn: (message: string) => void;
  readonly notice: (message: string) => void;
}

interface Source {
  readonly name: string;
  readonly client: RegistryClient;
  readonly repo: string;
  readonly entries: Map<string, CacheEntry>;
  /** hex -> size of every result blob layer. */
  readonly blobs: Map<string, number>;
  disabled: boolean;
}

const plural = (n: number, one: string, many: string): string => `${n} ${n === 1 ? one : many}`;
const messageOf = (e: unknown): string => (isSitefileError(e) ? e.message : String((e as Error)?.message ?? e));

function invalid(): SitefileError {
  return new SitefileError('E_REGISTRY', 'the cache index is invalid');
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Parse and validate an index; every blob it names must be a layer of the manifest with that size. */
export function parseCacheIndex(bytes: Uint8Array, blobs: ReadonlyMap<string, number>): Map<string, CacheEntry> {
  let j: unknown;
  try {
    j = JSON.parse(Buffer.from(bytes).toString('utf8'));
  } catch {
    throw invalid();
  }
  if (!isRecord(j) || j.version !== INDEX_VERSION || !isRecord(j.entries)) throw invalid();
  const out = new Map<string, CacheEntry>();
  for (const [key, e] of Object.entries(j.entries)) {
    if (!HEX64.test(key) || !isRecord(e) || (e.kind !== 'file' && e.kind !== 'files') || !Array.isArray(e.files)) throw invalid();
    if (e.kind === 'file' && e.files.length !== 1) throw invalid();
    const files: Array<readonly [string, string, number]> = [];
    const paths = new Set<string>();
    for (const f of e.files as unknown[]) {
      if (!Array.isArray(f) || f.length !== 3) throw invalid();
      const [p, hex, size] = f as [unknown, unknown, unknown];
      if (typeof p !== 'string' || typeof hex !== 'string' || !HEX64.test(hex) || typeof size !== 'number' || !Number.isSafeInteger(size) || size < 0) throw invalid();
      if (e.kind === 'file' ? p !== '' : p === '' || p.startsWith('/') || p.includes('..') || p.includes('\0') || p.includes('\\')) throw invalid();
      if (blobs.get(hex) !== size || paths.has(p)) throw invalid();
      paths.add(p);
      files.push([p, hex, size]);
    }
    out.set(key, { kind: e.kind, files });
  }
  return out;
}

/** The index bytes for a set of entries (canonical JSON, keys sorted). */
export function encodeCacheIndex(entries: ReadonlyMap<string, CacheEntry>): Buffer {
  const o: Record<string, unknown> = {};
  for (const [k, e] of entries) o[k] = { kind: e.kind, files: e.files.map((f) => [f[0], f[1], f[2]]) };
  return Buffer.from(canonicalJson({ version: INDEX_VERSION, entries: o }));
}

/** The artifact manifest for an index and the result blobs (`[hex, size]`, any order). */
export function encodeCacheManifest(index: Uint8Array, blobs: ReadonlyArray<readonly [string, number]>): Buffer {
  const sorted = [...blobs].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return Buffer.from(
    canonicalJson({
      schemaVersion: 2,
      mediaType: MEDIA.manifest,
      artifactType: CACHE_ARTIFACT_TYPE,
      config: { mediaType: EMPTY_MEDIA, digest: sha256(EMPTY), size: EMPTY.length },
      layers: [{ mediaType: CACHE_INDEX_MEDIA, digest: sha256(index), size: index.byteLength }, ...sorted.map(([hex, size]) => ({ mediaType: CACHE_BLOB_MEDIA, digest: `sha256:${hex}`, size }))],
    }),
  );
}

async function collect(stream: AsyncIterable<Buffer>, max: number, what: string): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let n = 0;
  for await (const c of stream) {
    n += c.length;
    if (n > max) throw new SitefileError('E_TOO_LARGE', `${what} exceeded ${max} bytes`);
    chunks.push(c);
  }
  return Buffer.concat(chunks, n);
}

/** sha256 hex and size of a file, or undefined when it cannot be read. */
async function hashFile(file: string): Promise<{ hex: string; size: number } | undefined> {
  try {
    const h = createHash('sha256');
    let size = 0;
    for await (const c of createReadStream(file) as AsyncIterable<Buffer>) {
      h.update(c);
      size += c.length;
    }
    return { hex: h.digest('hex'), size };
  } catch {
    return undefined;
  }
}

export class RegistryCache implements RemoteCache {
  /** Entries this build used or wrote (what `--cache-to` pushes). */
  readonly usedEntries = new Map<string, CacheEntry>();
  /** Results fetched from `--cache-from` registries, and their blob bytes downloaded. */
  fetched = 0;
  fetchedBytes = 0;
  private readonly sources: Source[] = [];
  private stopped = false;

  constructor(private readonly o: RegistryCacheOptions) {}

  /** Read every `--cache-from` manifest and index (in order). Problems are warnings. */
  async load(): Promise<void> {
    for (const c of this.o.from) {
      try {
        const s = await this.loadOne(c);
        this.sources.push(s);
        this.o.notice(`Cache from ${c.input}: ${plural(s.entries.size, 'entry', 'entries')}`);
      } catch (e) {
        this.o.warn(`--cache-from ${c.input} not used: ${messageOf(e)}`);
      }
    }
  }

  private async loadOne(c: CacheRef): Promise<Source> {
    const client = this.o.client(c.ref.registry as string);
    const repo = c.ref.name;
    const m = await client.getManifest(repo, c.ref.digest ?? c.ref.tag ?? 'latest');
    let artifactType: unknown;
    let layers;
    try {
      artifactType = (JSON.parse(Buffer.from(m.bytes).toString('utf8')) as { artifactType?: unknown }).artifactType;
      layers = parseManifest(m.bytes).layers;
    } catch {
      throw new SitefileError('E_UNSUPPORTED', `${c.input} is not a sitefile cache (not an OCI manifest)`);
    }
    if (artifactType !== CACHE_ARTIFACT_TYPE) {
      throw new SitefileError('E_UNSUPPORTED', `${c.input} is not a sitefile cache (artifactType ${typeof artifactType === 'string' ? `'${artifactType.slice(0, 100)}'` : 'missing'})`);
    }
    const indexes = layers.filter((l) => l.mediaType === CACHE_INDEX_MEDIA);
    const index = indexes[0];
    if (!index || indexes.length !== 1) throw new SitefileError('E_REGISTRY', `${c.input} has no cache index layer`);
    if (index.size > MAX_INDEX_BYTES) throw new SitefileError('E_TOO_LARGE', `the cache index is ${index.size} bytes, exceeding the limit of ${MAX_INDEX_BYTES} bytes`);
    const blobs = new Map<string, number>();
    for (const l of layers) if (l.mediaType === CACHE_BLOB_MEDIA) blobs.set(l.digest.slice('sha256:'.length), l.size);
    const bytes = await collect(client.getBlob(repo, index), index.size, `cache index ${index.digest}`);
    if (bytes.length !== index.size || sha256(bytes) !== index.digest) throw new SitefileError('E_INTEGRITY', `the cache index does not match its digest ${index.digest}`);
    return { name: c.input, client, repo, entries: parseCacheIndex(bytes, blobs), blobs, disabled: false };
  }

  used(key: string, entry: CacheEntry): void {
    this.usedEntries.set(key, entry);
  }

  async fetch(key: string, blobPath: (hex: string) => string): Promise<CacheEntry | undefined> {
    for (const s of this.sources) {
      if (this.stopped) return undefined;
      const entry = s.disabled ? undefined : s.entries.get(key);
      if (!entry) continue;
      try {
        const missing: Array<readonly [string, number]> = [];
        for (const [, hex, size] of entry.files) {
          const st = await stat(blobPath(hex)).catch(() => undefined);
          if (st?.isFile() && st.size === size) {
            const now = new Date();
            await utimes(blobPath(hex), now, now).catch(() => {});
          } else if (!missing.some((x) => x[0] === hex)) missing.push([hex, size]);
        }
        const need = missing.reduce((a, x) => a + x[1], 0);
        if (this.o.limit !== 0 && this.fetchedBytes + need > this.o.limit) {
          this.stopped = true;
          this.o.warn(`--cache-from: not fetching more cached results: they would exceed ${CACHE_MAX_ENV}=${formatLimit(this.o.limit)}`);
          return undefined;
        }
        for (const [hex, size] of missing) await this.download(s, hex, size, blobPath(hex));
        this.fetchedBytes += need;
        this.fetched++;
        return entry;
      } catch (e) {
        s.disabled = true;
        this.o.warn(`--cache-from ${s.name}: ${messageOf(e)}; not using it for the rest of this build`);
      }
    }
    return undefined;
  }

  /** Stream a result blob into the local cache: verified size and digest, temp file + rename. */
  private async download(s: Source, hex: string, size: number, file: string): Promise<void> {
    const digest = `sha256:${hex}`;
    await mkdir(path.dirname(file), { recursive: true });
    const tmp = `${file}.${process.pid}.${Math.random().toString(36).slice(2)}.tmp`;
    const fh = await open(tmp, 'w');
    let ok = false;
    try {
      const h = createHash('sha256');
      let n = 0;
      for await (const c of s.client.getBlob(s.repo, { mediaType: CACHE_BLOB_MEDIA, digest, size })) {
        n += c.length;
        if (n > size) break;
        h.update(c);
        await fh.write(c);
      }
      if (n !== size || h.digest('hex') !== hex) throw new SitefileError('E_INTEGRITY', `cached blob ${digest} does not match its digest or size`);
      ok = true;
    } finally {
      await fh.close();
      if (!ok) await rm(tmp, { force: true });
    }
    await rename(tmp, file);
  }

  /** Push the used entries to `--cache-to` (after a successful build). Problems are warnings. */
  async push(cacheDir: string): Promise<void> {
    const to = this.o.to;
    if (!to) return;
    try {
      const r = await this.pushTo(to, cacheDir);
      this.o.notice(
        `Cache to ${to.input}: ${plural(r.entries, 'entry', 'entries')}, ${plural(r.uploaded, 'blob', 'blobs')} uploaded, ${r.mounted} mounted, ${r.existing} already present (${r.digest})`,
      );
    } catch (e) {
      this.o.warn(`--cache-to ${to.input} not pushed: ${messageOf(e)}`);
    }
  }

  private async pushTo(to: CacheRef, cacheDir: string): Promise<{ entries: number; uploaded: number; mounted: number; existing: number; digest: string }> {
    const blobFile = (hex: string): string => path.join(cacheDir, 'blobs', hex);
    // Only entries whose blobs are all still intact locally.
    const good = new Map<string, number>();
    const bad = new Set<string>();
    const keys = [...this.usedEntries.keys()].sort();
    const entries = new Map<string, CacheEntry>();
    for (const k of keys) {
      const e = this.usedEntries.get(k) as CacheEntry;
      let keep = true;
      for (const [, hex, size] of e.files) {
        if (!good.has(hex) && !bad.has(hex)) {
          const h = await hashFile(blobFile(hex));
          if (h && h.hex === hex && h.size === size) good.set(hex, size);
          else bad.add(hex);
        }
        if (bad.has(hex)) keep = false;
      }
      if (keep) entries.set(k, e);
    }
    const blobs = new Map<string, number>();
    for (const e of entries.values()) for (const [, hex, size] of e.files) blobs.set(hex, size);
    const index = encodeCacheIndex(entries);
    const manifest = encodeCacheManifest(index, [...blobs]);
    const client = this.o.client(to.ref.registry as string);
    if (manifest.byteLength > client.limits.maxManifestBytes) {
      throw new SitefileError('E_TOO_LARGE', `the cache manifest would be ${manifest.byteLength} bytes, exceeding the limit of ${client.limits.maxManifestBytes} bytes`);
    }
    const repo = to.ref.name;
    await client.ping();
    const counts = { uploaded: 0, mounted: 0, existing: 0 };
    const sorted = [...blobs].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    const uploads: Array<{ digest: string; size: number; mediaType: string; open: () => Uint8Array | ReturnType<typeof createReadStream>; mount?: string | undefined }> = [
      { digest: sha256(EMPTY), size: EMPTY.length, mediaType: EMPTY_MEDIA, open: () => EMPTY },
      { digest: sha256(index), size: index.byteLength, mediaType: CACHE_INDEX_MEDIA, open: () => index },
      ...sorted.map(([hex, size]) => ({
        digest: `sha256:${hex}`,
        size,
        mediaType: CACHE_BLOB_MEDIA,
        open: () => createReadStream(blobFile(hex)),
        // A blob a --cache-from repository on this registry holds is mounted rather than uploaded.
        mount: this.sources.find((s) => s.client.registry === client.registry && s.repo !== repo && s.blobs.get(hex) === size)?.repo,
      })),
    ];
    for (const u of uploads) {
      if (await client.hasBlob(repo, u.digest)) {
        counts.existing++;
        continue;
      }
      const r = await client.pushBlob(repo, { mediaType: u.mediaType, digest: u.digest, size: u.size }, { length: u.size, open: u.open }, u.mount);
      counts[r]++;
    }
    const digest = await client.putManifest(repo, to.ref.tag ?? 'latest', manifest, MEDIA.manifest);
    return { entries: entries.size, ...counts, digest };
  }
}
