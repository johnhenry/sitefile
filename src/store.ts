import { randomBytes } from 'node:crypto';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import {
  ANN,
  blobPathOf,
  buildOciImage,
  decodeIndex,
  encodeIndex,
  extractImageFiles,
  layoutTree,
  OCI_LAYOUT_BYTES,
  parseConfig,
  parseManifest,
  type IndexEntry,
  type OciOptions,
  type ParsedConfig,
  type ParsedManifest,
} from './oci.js';
import { looksLikeImageId, parseLocalRef, refKey, type ImageRef } from './ref.js';
import type { Tree } from './tree.js';

/**
 * The local image store IS an OCI image layout (image-spec v1.1) at $SITEFILE_HOME (default ~/.sitefile):
 *
 *   oci-layout           {"imageLayoutVersion":"1.0.0"}
 *   index.json           image index. Each entry is a manifest descriptor; tags are entries annotated
 *                        org.opencontainers.image.ref.name = "name:tag". An image with no tag is a
 *                        "dangling" entry without that annotation. dev.sitefile.created records when
 *                        the image was first stored (bookkeeping only; no digest depends on it).
 *   blobs/sha256/<hex>   manifest, config and layer blobs (identical layers share a blob)
 *   servers/             site server state and logs (see servers.ts)
 *   lock                 advisory lock for mutations
 *
 * IMAGE ID = the manifest digest. All writes are temp file + rename, mutations hold the lock, and every
 * blob is digest-verified when read (corruption is E_STORE).
 */

export interface ImageSummary {
  /** Full `sha256:<hex>` image ID (the manifest digest). */
  readonly id: string;
  readonly tags: readonly string[];
  /** When the image was first stored. */
  readonly created: string;
  readonly files: number;
  /** Total bytes of the site's files (uncompressed). */
  readonly size: number;
}

export interface ImageDetails extends ImageSummary {
  readonly manifest: ParsedManifest;
  readonly config: ParsedConfig;
  readonly root: string;
  /** Compressed layer size in bytes. */
  readonly layerSize: number;
}

const HEX64 = /^[0-9a-f]{64}$/;

export function resolveStoreHome(env: NodeJS.ProcessEnv = process.env): string {
  const h = env.SITEFILE_HOME;
  return path.resolve(h !== undefined && h !== '' ? h : path.join(os.homedir(), '.sitefile'));
}

function storeError(msg: string, cause?: unknown): SitefileError {
  return new SitefileError('E_STORE', msg, undefined, cause === undefined ? undefined : { cause });
}

function errCode(e: unknown): string | undefined {
  return (e as NodeJS.ErrnoException | null)?.code;
}

export function shortId(id: string): string {
  return id.replace(/^sha256:/, '').slice(0, 12);
}

export class Store {
  private constructor(readonly home: string) {}

  static async open(home: string = resolveStoreHome()): Promise<Store> {
    const s = new Store(path.resolve(home));
    await fs.mkdir(path.join(s.home, 'blobs', 'sha256'), { recursive: true, mode: 0o700 });
    await fs.writeFile(path.join(s.home, 'oci-layout'), OCI_LAYOUT_BYTES, { flag: 'wx', mode: 0o600 }).catch((e: unknown) => {
      if (errCode(e) !== 'EEXIST') throw e;
    });
    return s;
  }

  private blobFile(digest: string): string {
    return path.join(this.home, blobPathOf(digest));
  }

  // ------------------------------------------------------------ locking

  /** Run `fn` holding the store lock (an exclusive lock file; stale locks are broken). */
  async withLock<T>(fn: () => Promise<T>): Promise<T> {
    const lock = path.join(this.home, 'lock');
    const deadline = Date.now() + 20_000;
    for (;;) {
      try {
        const fh = await fs.open(lock, 'wx', 0o600);
        await fh.writeFile(String(process.pid));
        await fh.close();
        break;
      } catch (e) {
        if (errCode(e) !== 'EEXIST') throw storeError(`cannot lock the store at '${this.home}': ${(e as Error).message}`, e);
        if (await this.lockIsStale(lock)) {
          await fs.rm(lock, { force: true });
          continue;
        }
        if (Date.now() > deadline) throw storeError(`timed out waiting for the store lock '${lock}'`);
        await new Promise((r) => setTimeout(r, 20 + Math.random() * 30));
      }
    }
    try {
      return await fn();
    } finally {
      await fs.rm(lock, { force: true });
    }
  }

  private async lockIsStale(lock: string): Promise<boolean> {
    try {
      const [text, st] = await Promise.all([fs.readFile(lock, 'utf8'), fs.stat(lock)]);
      const pid = Number(text);
      if (Date.now() - st.mtimeMs > 120_000) return true;
      if (!Number.isInteger(pid) || pid <= 0) return Date.now() - st.mtimeMs > 2_000; // still being written
      try {
        process.kill(pid, 0);
        return false;
      } catch (e) {
        return errCode(e) === 'ESRCH';
      }
    } catch {
      return false; // vanished or unreadable: retry the open
    }
  }

  private async writeAtomic(dest: string, data: Uint8Array | string): Promise<void> {
    const tmp = path.join(path.dirname(dest), `.tmp-${randomBytes(6).toString('hex')}`);
    try {
      await fs.writeFile(tmp, data, { mode: 0o600 });
      await fs.rename(tmp, dest);
    } catch (e) {
      await fs.rm(tmp, { force: true });
      throw storeError(`cannot write '${dest}': ${(e as Error).message}`, e);
    }
  }

  // ------------------------------------------------------------ index

  private async readIndex(): Promise<IndexEntry[]> {
    let bytes: Buffer;
    try {
      bytes = await fs.readFile(path.join(this.home, 'index.json'));
    } catch (e) {
      if (errCode(e) === 'ENOENT') return [];
      throw storeError(`cannot read index.json: ${(e as Error).message}`, e);
    }
    try {
      return decodeIndex(bytes);
    } catch (e) {
      throw storeError(`index.json is corrupt: ${(e as Error).message}`, e);
    }
  }

  /** Normalize (one dangling entry per untagged image, none for tagged ones), sort, write. */
  private async writeIndex(entries: readonly IndexEntry[]): Promise<void> {
    const tagged = new Set(entries.filter((e) => e.refName).map((e) => e.digest));
    const seenDangling = new Set<string>();
    const out = entries.filter((e) => {
      if (e.refName) return true;
      if (tagged.has(e.digest) || seenDangling.has(e.digest)) return false;
      seenDangling.add(e.digest);
      return true;
    });
    out.sort((a, b) => (a.digest + (a.refName ?? '') < b.digest + (b.refName ?? '') ? -1 : 1));
    await this.writeAtomic(path.join(this.home, 'index.json'), encodeIndex(out));
  }

  /** `name:tag` → image ID. */
  async refs(): Promise<Record<string, string>> {
    const out: Record<string, string> = {};
    for (const e of await this.readIndex()) if (e.refName) out[e.refName] = e.digest;
    return out;
  }

  // ------------------------------------------------------------ blobs

  /** Read a blob and verify its digest (and size when given). */
  async readBlob(digest: string, size?: number): Promise<Uint8Array> {
    let bytes: Uint8Array;
    try {
      bytes = new Uint8Array(await fs.readFile(this.blobFile(digest)));
    } catch (e) {
      if (errCode(e) === 'ENOENT') throw storeError(`blob ${digest} is missing`, e);
      throw storeError(`cannot read blob ${digest}: ${(e as Error).message}`, e);
    }
    if ((size !== undefined && bytes.byteLength !== size) || sha256(bytes) !== digest) {
      throw storeError(`blob ${digest} is corrupt: its content does not match its digest`);
    }
    return bytes;
  }

  private async putBlob(digest: string, bytes: Uint8Array): Promise<void> {
    const bp = this.blobFile(digest);
    const st = await fs.stat(bp).catch(() => undefined);
    if (st?.isFile() && st.size === bytes.byteLength) return;
    await this.writeAtomic(bp, bytes);
  }

  // ------------------------------------------------------------ images

  /** Store a tree as an OCI image (idempotent: identical content and options give the same ID). Untagged. */
  async addImage(tree: Tree, options: OciOptions = {}, now: Date = new Date()): Promise<{ id: string; created: boolean }> {
    const img = buildOciImage(tree, options);
    return this.withLock(async () => {
      await this.putBlob(img.layerDigest, img.layerBytes);
      await this.putBlob(img.configDigest, img.configBytes);
      await this.putBlob(img.manifestDigest, img.manifestBytes);
      const entries = await this.readIndex();
      if (entries.some((e) => e.digest === img.manifestDigest)) return { id: img.manifestDigest, created: false };
      await this.writeIndex([...entries, { digest: img.manifestDigest, size: img.manifestBytes.byteLength, stored: now.toISOString() }]);
      return { id: img.manifestDigest, created: true };
    });
  }

  /** Manifest and config of an image, digest-verified (not its layer). */
  async details(id: string): Promise<ImageDetails> {
    const entries = (await this.readIndex()).filter((e) => e.digest === id);
    const first = entries[0];
    if (!first) throw new SitefileError('E_NOT_FOUND', `no such image: ${shortId(id)}`);
    const manifestBytes = await this.readBlob(id, first.size);
    let manifest: ParsedManifest;
    let config: ParsedConfig;
    try {
      manifest = parseManifest(manifestBytes);
      config = parseConfig(await this.readBlob(manifest.config.digest, manifest.config.size));
    } catch (e) {
      if (e instanceof SitefileError) throw e;
      throw storeError(`image ${shortId(id)} is corrupt: ${(e as Error).message}`, e);
    }
    const stored = entries.map((e) => e.stored).filter((s): s is string => !!s).sort()[0] ?? config.created;
    return {
      id,
      tags: entries.flatMap((e) => (e.refName ? [e.refName] : [])),
      created: stored,
      files: Number(manifest.annotations[ANN.files] ?? 0),
      size: Number(manifest.annotations[ANN.size] ?? 0),
      manifest,
      config,
      root: config.labels[ANN.root] ?? '/',
      layerSize: manifest.layers[0]?.size ?? 0,
    };
  }

  /** Load an image's files (under its root), verifying manifest, config, layer digest and diff_id. */
  async loadTree(id: string): Promise<Tree> {
    const d = await this.details(id);
    const layer = d.manifest.layers[0];
    if (!layer) throw storeError(`image ${shortId(id)} has no layer`);
    const bytes = await this.readBlob(layer.digest, layer.size);
    return extractImageFiles(d.manifest, d.config, bytes, `image:${shortId(id)}`);
  }

  /** The image as a standalone OCI layout Tree (for `save`), optionally annotated with a tag. */
  async layoutOf(id: string, refName?: string): Promise<Tree> {
    const d = await this.details(id);
    const layer = d.manifest.layers[0];
    if (!layer) throw storeError(`image ${shortId(id)} has no layer`);
    return layoutTree(
      {
        manifestBytes: await this.readBlob(id),
        manifestDigest: id,
        configBytes: await this.readBlob(d.manifest.config.digest),
        configDigest: d.manifest.config.digest,
        layerBytes: await this.readBlob(layer.digest, layer.size),
        layerDigest: layer.digest,
      },
      refName,
    );
  }

  /** Every stored image, newest first. */
  async list(): Promise<ImageSummary[]> {
    const out: ImageSummary[] = [];
    for (const id of [...new Set((await this.readIndex()).map((e) => e.digest))]) {
      const { id: _i, tags, created, files, size } = await this.details(id);
      out.push({ id, tags, created, files, size });
    }
    return out.sort((a, b) => (a.created < b.created ? 1 : a.created > b.created ? -1 : a.id < b.id ? -1 : 1));
  }

  private async idsWithPrefix(input: string): Promise<string[]> {
    const hex = input.replace(/^sha256:/, '');
    return [...new Set((await this.readIndex()).map((e) => e.digest))].filter((id) => id.startsWith(`sha256:${hex}`));
  }

  /** Resolve a reference or (short) image ID to a full image ID. */
  async resolve(input: string): Promise<string> {
    if (looksLikeImageId(input)) {
      const matches = await this.idsWithPrefix(input);
      if (matches.length === 1) return matches[0] as string;
      if (matches.length > 1) throw storeError(`ambiguous image ID '${input}' matches ${matches.length} images`);
    }
    const ref = parseLocalRef(input);
    if (ref.digest !== undefined) {
      if (!(await this.readIndex()).some((e) => e.digest === ref.digest)) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
      return ref.digest;
    }
    const id = (await this.refs())[refKey(ref)];
    if (id === undefined) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
    return id;
  }

  /** Point `name:tag` at an image (replacing any previous target). Returns the `name:tag` key. */
  async tag(id: string, ref: ImageRef): Promise<string> {
    if (ref.registry !== undefined || ref.digest !== undefined) {
      throw new SitefileError('E_USAGE', 'a tag must be a local name[:tag] (no registry host, no digest)');
    }
    const key = refKey(ref);
    await this.withLock(async () => {
      const entries = await this.readIndex();
      const mine = entries.find((e) => e.digest === id);
      if (!mine) throw new SitefileError('E_NOT_FOUND', `no such image: ${shortId(id)}`);
      const previous = entries.find((e) => e.refName === key);
      const next = entries.filter((e) => e.refName !== key);
      // The old target stays as an untagged image if this was its last tag (writeIndex adds the dangling entry).
      if (previous && !next.some((e) => e.digest === previous.digest)) next.push({ ...previous, refName: undefined });
      const stored = entries.find((e) => e.digest === id && e.stored)?.stored;
      next.push({ digest: id, size: mine.size, refName: key, stored });
      await this.writeIndex(next);
    });
    return key;
  }

  /**
   * `rmi`: remove a tag, or an image by ID. The image leaves the index when no tag references it
   * (blobs are reclaimed by `prune`). Removing by ID while several tags point at it needs `force`.
   */
  async removeImage(input: string, force = false): Promise<{ untagged: string[]; deleted: string | undefined }> {
    return this.withLock(async () => {
      let entries = await this.readIndex();
      const untagged: string[] = [];
      let id: string;
      const idMatches = looksLikeImageId(input) ? await this.idsWithPrefix(input) : [];
      if (idMatches.length > 0 || /^sha256:[0-9a-f]{64}$/.test(input)) {
        id = await this.resolve(input);
        const tags = entries.filter((e) => e.digest === id && e.refName).map((e) => e.refName as string);
        if (tags.length > 1 && !force) {
          throw storeError(`image ${shortId(id)} is referenced by ${tags.length} tags (${tags.join(', ')}); remove a tag, or use -f`);
        }
        untagged.push(...tags);
        entries = entries.filter((e) => e.digest !== id);
      } else {
        const ref = parseLocalRef(input);
        if (ref.digest !== undefined) throw new SitefileError('E_USAGE', 'rmi takes name[:tag] or an image ID, not name@digest');
        const key = refKey(ref);
        const target = entries.find((e) => e.refName === key);
        if (!target) throw new SitefileError('E_NOT_FOUND', `no such image: ${input}`);
        id = target.digest;
        untagged.push(key);
        entries = entries.filter((e) => e.refName !== key);
        if (entries.some((e) => e.digest === id)) {
          await this.writeIndex(entries);
          return { untagged, deleted: undefined };
        }
      }
      await this.writeIndex(entries);
      return { untagged, deleted: id };
    });
  }

  /** Drop untagged images from the index, then delete every blob no remaining image uses. */
  async prune(): Promise<{ manifests: number; blobs: number; bytes: number }> {
    return this.withLock(async () => {
      const entries = await this.readIndex();
      const keep = entries.filter((e) => e.refName);
      const dropped = new Set(entries.filter((e) => !e.refName).map((e) => e.digest));
      for (const e of keep) dropped.delete(e.digest);
      await this.writeIndex(keep);
      const live = new Set<string>();
      for (const id of new Set(keep.map((e) => e.digest))) {
        const d = await this.details(id);
        live.add(id.slice(7));
        live.add(d.manifest.config.digest.slice(7));
        for (const l of d.manifest.layers) live.add(l.digest.slice(7));
      }
      let blobs = 0;
      let bytes = 0;
      const dir = path.join(this.home, 'blobs', 'sha256');
      for (const name of await fs.readdir(dir)) {
        const full = path.join(dir, name);
        const st = await fs.stat(full).catch(() => undefined);
        if (!st?.isFile()) continue;
        if (HEX64.test(name) ? live.has(name) : Date.now() - st.mtimeMs < 3_600_000) continue; // keep live blobs and fresh temp files
        await fs.rm(full, { force: true });
        blobs++;
        bytes += st.size;
      }
      return { manifests: dropped.size, blobs, bytes };
    });
  }
}
