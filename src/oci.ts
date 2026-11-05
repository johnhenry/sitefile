import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';
import { gzipTar, writeTar } from './archive-writers.js';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import { normalizeVirtual } from './paths.js';
import { extractArchive } from './resources/archive.js';
import { makeFile, Tree, type FileNode } from './tree.js';

/**
 * OCI image-spec v1.1 writer and reader for Sitefile images.
 *
 * A Sitefile image has exactly one layer: a deterministic tar of the site's files placed under a
 * root directory (default /srv/www), gzip-compressed. It carries files only: it has no entrypoint
 * and is not runnable by itself (use it with `COPY --from=` in a Dockerfile). Everything that is
 * hashed (config, manifest, index) is canonical JSON, so the same tree and options always give the
 * same manifest digest.
 *
 * Layout (the on-disk image store is this layout; `save`/`EXPORT AS oci` write it standalone):
 *   oci-layout            {"imageLayoutVersion":"1.0.0"}
 *   index.json            image index; entries carry org.opencontainers.image.ref.name
 *   blobs/sha256/<hex>    manifest, config and layer blobs
 */
export const DEFAULT_OCI_ROOT = '/srv/www';
export const MEDIA = {
  manifest: 'application/vnd.oci.image.manifest.v1+json',
  config: 'application/vnd.oci.image.config.v1+json',
  layer: 'application/vnd.oci.image.layer.v1.tar+gzip',
  index: 'application/vnd.oci.image.index.v1+json',
} as const;
export const ANN = {
  refName: 'org.opencontainers.image.ref.name',
  title: 'org.opencontainers.image.title',
  source: 'org.opencontainers.image.source',
  revision: 'org.opencontainers.image.revision',
  root: 'dev.sitefile.root',
  files: 'dev.sitefile.files',
  size: 'dev.sitefile.size',
  stored: 'dev.sitefile.created',
} as const;
const EPOCH_ISO = '1970-01-01T00:00:00Z';

export interface OciOptions {
  /** Where the files live inside the layer. Default /srv/www. */
  readonly root?: string | undefined;
  /** `os/arch`, default linux/amd64. Metadata only: the image holds no executables. */
  readonly platform?: string | undefined;
  /** `SOURCE_DATE_EPOCH` seconds; default the fixed epoch, so digests are reproducible. */
  readonly epoch?: number | undefined;
  readonly title?: string | undefined;
  readonly source?: string | undefined;
  readonly revision?: string | undefined;
}

export interface Descriptor {
  readonly mediaType: string;
  readonly digest: string;
  readonly size: number;
  readonly annotations?: Readonly<Record<string, string>>;
}

export interface OciImage {
  readonly manifestBytes: Uint8Array;
  readonly manifestDigest: string;
  readonly configBytes: Uint8Array;
  readonly configDigest: string;
  readonly layerBytes: Uint8Array;
  readonly layerDigest: string;
  /** sha256 of the uncompressed tar. */
  readonly diffId: string;
  readonly root: string;
}

/** JSON with sorted keys and no whitespace. */
export function canonicalJson(v: unknown): string {
  if (v === null || typeof v !== 'object') return JSON.stringify(v);
  if (Array.isArray(v)) return `[${v.map(canonicalJson).join(',')}]`;
  const o = v as Record<string, unknown>;
  return `{${Object.keys(o)
    .filter((k) => o[k] !== undefined)
    .sort()
    .map((k) => `${JSON.stringify(k)}:${canonicalJson(o[k])}`)
    .join(',')}}`;
}

const enc = new TextEncoder();

export function parsePlatform(p: string | undefined): { os: string; arch: string } {
  if (p === undefined) return { os: 'linux', arch: 'amd64' };
  const m = /^([a-z0-9][a-z0-9._-]*)\/([a-z0-9][a-z0-9._-]*)$/.exec(p);
  if (!m) throw new SitefileError('E_USAGE', `platform must be os/arch (e.g. linux/amd64), got '${p}'`);
  return { os: m[1] as string, arch: m[2] as string };
}

/** Validate an in-image root path (absolute, no `..`); returns the normalized form. */
export function normalizeOciRoot(raw: string | undefined): string {
  return normalizeVirtual(raw ?? DEFAULT_OCI_ROOT, undefined, 'OCI root').path;
}

function isoOf(epoch: number | undefined): string {
  return epoch === undefined ? EPOCH_ISO : new Date(epoch * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

/** Build the single-layer image for `tree` (the site's files, rooted at `/`). */
export function buildOciImage(tree: Tree, options: OciOptions = {}): OciImage {
  const root = normalizeOciRoot(options.root);
  const { os, arch } = parsePlatform(options.platform);
  const layerTree = Tree.fromFiles(tree.mountedAt(root));
  const segs = root === '/' ? [] : root.slice(1).split('/');
  const dirs = segs.map((_, i) => segs.slice(0, i + 1).join('/'));
  const tar = writeTar(layerTree, { epoch: options.epoch ?? 0, dirs });
  const layerBytes = gzipTar(tar);
  const diffId = sha256(tar);
  const layerDigest = sha256(layerBytes);
  const size = tree.entries().reduce((n, [, f]) => n + f.bytes.byteLength, 0);

  const labels: Record<string, string> = { [ANN.root]: root };
  if (options.title) labels[ANN.title] = options.title;
  if (options.source) labels[ANN.source] = options.source;
  if (options.revision) labels[ANN.revision] = options.revision;
  const created = isoOf(options.epoch);
  const configBytes = enc.encode(
    canonicalJson({
      architecture: arch,
      os,
      created,
      config: { Labels: labels },
      rootfs: { type: 'layers', diff_ids: [diffId] },
      history: [{ created, created_by: 'sitefile', comment: 'static site files', author: 'sitefile' }],
    }),
  );
  const configDigest = sha256(configBytes);
  const annotations: Record<string, string> = { [ANN.files]: String(tree.size), [ANN.size]: String(size) };
  if (options.title) annotations[ANN.title] = options.title;
  const manifestBytes = enc.encode(
    canonicalJson({
      schemaVersion: 2,
      mediaType: MEDIA.manifest,
      config: { mediaType: MEDIA.config, digest: configDigest, size: configBytes.byteLength },
      layers: [{ mediaType: MEDIA.layer, digest: layerDigest, size: layerBytes.byteLength }],
      annotations,
    }),
  );
  return { manifestBytes, manifestDigest: sha256(manifestBytes), configBytes, configDigest, layerBytes, layerDigest, diffId, root };
}

export interface IndexEntry {
  readonly digest: string;
  readonly size: number;
  readonly refName?: string | undefined;
  /** When the image was first stored locally (store bookkeeping; not part of any digest). */
  readonly stored?: string | undefined;
}

export function encodeIndex(entries: readonly IndexEntry[]): Uint8Array {
  const manifests = entries.map((e) => {
    const annotations: Record<string, string> = {};
    if (e.refName) annotations[ANN.refName] = e.refName;
    if (e.stored) annotations[ANN.stored] = e.stored;
    return { mediaType: MEDIA.manifest, digest: e.digest, size: e.size, ...(Object.keys(annotations).length ? { annotations } : {}) };
  });
  return enc.encode(canonicalJson({ schemaVersion: 2, mediaType: MEDIA.index, manifests }));
}

export const OCI_LAYOUT_BYTES = enc.encode('{"imageLayoutVersion":"1.0.0"}');

const SHA = /^sha256:[0-9a-f]{64}$/;

export function decodeIndex(bytes: Uint8Array): IndexEntry[] {
  const j = JSON.parse(Buffer.from(bytes).toString('utf8')) as { schemaVersion?: unknown; manifests?: unknown };
  if (j.schemaVersion !== 2 || !Array.isArray(j.manifests)) throw new Error('not an OCI image index');
  return (j.manifests as Array<Record<string, unknown>>).map((m) => {
    if (typeof m.digest !== 'string' || !SHA.test(m.digest) || typeof m.size !== 'number') throw new Error('bad index entry');
    const a = (m.annotations ?? {}) as Record<string, unknown>;
    const ref = a[ANN.refName];
    const stored = a[ANN.stored];
    return { digest: m.digest, size: m.size, refName: typeof ref === 'string' ? ref : undefined, stored: typeof stored === 'string' ? stored : undefined };
  });
}

export function blobPathOf(digest: string): string {
  if (!SHA.test(digest)) throw new SitefileError('E_STORE', `invalid digest '${digest}'`);
  return `/blobs/sha256/${digest.slice(7)}`;
}

/** A standalone OCI image layout holding one image, as a Tree (write it as a directory or tar it). */
export function layoutTree(img: Pick<OciImage, 'manifestBytes' | 'manifestDigest' | 'configBytes' | 'configDigest' | 'layerBytes' | 'layerDigest'>, refName?: string): Tree {
  const files: Array<[string, FileNode]> = [
    ['/oci-layout', makeFile(OCI_LAYOUT_BYTES, 'oci')],
    ['/index.json', makeFile(encodeIndex([{ digest: img.manifestDigest, size: img.manifestBytes.byteLength, refName }]), 'oci')],
    [blobPathOf(img.manifestDigest), makeFile(img.manifestBytes, 'oci')],
    [blobPathOf(img.configDigest), makeFile(img.configBytes, 'oci')],
    [blobPathOf(img.layerDigest), makeFile(img.layerBytes, 'oci')],
  ];
  return Tree.fromFiles(files);
}

/** Deterministic tar of {@link layoutTree}: an "OCI archive" (`.oci.tar`). */
export function ociArchive(img: Parameters<typeof layoutTree>[0], refName: string | undefined, epoch: number | undefined): Uint8Array {
  return writeTar(layoutTree(img, refName), { epoch: epoch ?? 0 });
}

export interface ParsedManifest {
  readonly config: Descriptor;
  readonly layers: readonly Descriptor[];
  readonly annotations: Readonly<Record<string, string>>;
}

function desc(d: unknown, what: string): Descriptor {
  const o = d as Record<string, unknown>;
  if (typeof o?.mediaType !== 'string' || typeof o.digest !== 'string' || !SHA.test(o.digest) || typeof o.size !== 'number' || !Number.isSafeInteger(o.size) || o.size < 0) {
    throw new Error(`bad ${what} descriptor`);
  }
  return { mediaType: o.mediaType, digest: o.digest, size: o.size };
}

export function parseManifest(bytes: Uint8Array): ParsedManifest {
  const m = JSON.parse(Buffer.from(bytes).toString('utf8')) as Record<string, unknown>;
  if (m.schemaVersion !== 2 || m.mediaType !== MEDIA.manifest || !Array.isArray(m.layers)) throw new Error('not an OCI image manifest');
  const annotations: Record<string, string> = {};
  for (const [k, v] of Object.entries((m.annotations ?? {}) as Record<string, unknown>)) if (typeof v === 'string') annotations[k] = v;
  return { config: desc(m.config, 'config'), layers: m.layers.map((l) => desc(l, 'layer')), annotations };
}

export interface ParsedConfig {
  readonly os: string;
  readonly architecture: string;
  readonly created: string;
  readonly labels: Readonly<Record<string, string>>;
  readonly diffIds: readonly string[];
}

export function parseConfig(bytes: Uint8Array): ParsedConfig {
  const c = JSON.parse(Buffer.from(bytes).toString('utf8')) as {
    os?: unknown;
    architecture?: unknown;
    created?: unknown;
    config?: { Labels?: Record<string, unknown> };
    rootfs?: { type?: unknown; diff_ids?: unknown };
  };
  const ids = c.rootfs?.diff_ids;
  if (c.rootfs?.type !== 'layers' || !Array.isArray(ids) || !ids.every((d) => typeof d === 'string' && SHA.test(d))) throw new Error('bad rootfs');
  const labels: Record<string, string> = {};
  for (const [k, v] of Object.entries(c.config?.Labels ?? {})) if (typeof v === 'string') labels[k] = v;
  return { os: String(c.os ?? ''), architecture: String(c.architecture ?? ''), created: String(c.created ?? ''), labels, diffIds: ids as string[] };
}

/**
 * Decode a single-layer image's files, verifying the layer digest and diff_id.
 * `layerBytes` are the compressed blob bytes. Returns the tree under the image's root.
 */
export async function extractImageFiles(m: ParsedManifest, cfg: ParsedConfig, layerBytes: Uint8Array, origin: string): Promise<Tree> {
  const layer = m.layers[0];
  if (m.layers.length !== 1 || !layer || layer.mediaType !== MEDIA.layer) {
    throw new SitefileError('E_STORE', `${origin}: only single-layer tar+gzip images are supported`);
  }
  if (sha256(layerBytes) !== layer.digest || layerBytes.byteLength !== layer.size) {
    throw new SitefileError('E_STORE', `${origin}: layer blob does not match its digest`);
  }
  let tar: Uint8Array;
  try {
    tar = new Uint8Array(gunzipSync(layerBytes));
  } catch (e) {
    throw new SitefileError('E_STORE', `${origin}: layer is not valid gzip`, undefined, { cause: e });
  }
  if (sha256(tar) !== cfg.diffIds[0]) throw new SitefileError('E_STORE', `${origin}: layer content does not match the config diff_id`);
  const root = cfg.labels[ANN.root] ?? '/';
  const all = await extractArchive(tar, origin, { maxEntries: 1_000_000, maxTotalBytes: 8 * 2 ** 30, maxRatio: Number.MAX_SAFE_INTEGER });
  return all.subtree(normalizeVirtual(root).path);
}

/** Label values for an image built from `contextDir`: title (directory name) plus source/revision when a checkout is found. */
export async function imageMeta(contextDir: string): Promise<Pick<OciOptions, 'title' | 'source' | 'revision'>> {
  const info = await gitInfo(contextDir);
  return { title: path.basename(path.resolve(contextDir)), ...info };
}

/** Best-effort `org.opencontainers.image.{source,revision}` from a git checkout (never runs git; credentials in URLs are dropped). */
export async function gitInfo(contextDir: string): Promise<{ source?: string; revision?: string }> {
  const out: { source?: string; revision?: string } = {};
  let dir = path.resolve(contextDir);
  let gitDir: string | undefined;
  for (let i = 0; i < 8; i++) {
    const g = path.join(dir, '.git');
    if (await readFile(path.join(g, 'HEAD'), 'utf8').then(() => true, () => false)) {
      gitDir = g;
      break;
    }
    const up = path.dirname(dir);
    if (up === dir) break;
    dir = up;
  }
  if (!gitDir) return out;
  try {
    const head = (await readFile(path.join(gitDir, 'HEAD'), 'utf8')).trim();
    let rev = head;
    const m = /^ref: (refs\/[\w./-]+)$/.exec(head);
    if (m) {
      rev = await readFile(path.join(gitDir, m[1] as string), 'utf8').then(
        (t) => t.trim(),
        async () => {
          const packed = await readFile(path.join(gitDir as string, 'packed-refs'), 'utf8').catch(() => '');
          return packed.split('\n').find((l) => l.endsWith(` ${m[1]}`))?.split(' ')[0] ?? '';
        },
      );
    }
    if (/^[0-9a-f]{40,64}$/.test(rev)) out.revision = rev;
  } catch {
    /* unknown */
  }
  try {
    const cfg = await readFile(path.join(gitDir, 'config'), 'utf8');
    const url = /\[remote "origin"\][^[]*?\burl\s*=\s*(\S+)/.exec(cfg)?.[1];
    if (url) {
      const u = /^[a-z+]+:\/\//.test(url) ? new URL(url) : undefined;
      if (u) {
        u.username = '';
        u.password = '';
        u.search = '';
        u.hash = '';
        out.source = u.toString();
      } else if (/^[\w.-]+@[\w.-]+:[\w./-]+$/.test(url)) {
        out.source = url; // scp-style: user@host:path (no secret)
      }
    }
  } catch {
    /* unknown */
  }
  return out;
}
