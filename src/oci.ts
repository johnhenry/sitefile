import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { gunzipSync } from 'node:zlib';
import { gzipTar, writeTar } from './archive-writers.js';
import type { Referrer } from './attest.js';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import type { OciCompression } from './language/ast.js';
import { normalizeVirtual } from './paths.js';
import { parseRef } from './ref.js';
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
  /** Uncompressed layer (`EXPORT ... COMPRESSION none`): the blob is the tar itself, digest = diff_id. */
  layerTar: 'application/vnd.oci.image.layer.v1.tar',
  index: 'application/vnd.oci.image.index.v1+json',
  dockerManifest: 'application/vnd.docker.distribution.manifest.v2+json',
  dockerList: 'application/vnd.docker.distribution.manifest.list.v2+json',
  dockerConfig: 'application/vnd.docker.container.image.v1+json',
  dockerLayer: 'application/vnd.docker.image.rootfs.diff.tar.gzip',
} as const;
export const ANN_BASE_NAME = 'org.opencontainers.image.base.name';
export const ANN_BASE_DIGEST = 'org.opencontainers.image.base.digest';
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
  /** Extra config labels (`--label key=value`); they override the defaults above. */
  readonly labels?: Readonly<Record<string, string>> | undefined;
  /**
   * Build on top of this image (`EXPORT ... AS oci BASE <image>`): its layers come first, its config
   * (Entrypoint, Cmd, Env, ExposedPorts, WorkingDir, ...) is kept, and the site layer is appended.
   * The base's blobs must already be in the store (see base-image.ts).
   */
  readonly base?: BaseImage | undefined;
  /** Site layer encoding: gzip (default, `tar+gzip`) or none (plain `tar`). Base layers are kept as they are. */
  readonly compression?: OciCompression | undefined;
}

/** A resolved base image: one platform manifest, its config and layers. */
export interface BaseImage {
  /** Canonical reference as written (`docker.io/library/nginx:alpine`), without any digest. */
  readonly name: string;
  /** Digest of the platform manifest the layers come from (recorded in the image). */
  readonly manifestDigest: string;
  readonly layers: readonly Descriptor[];
  /** The base's parsed config JSON. */
  readonly config: Readonly<Record<string, unknown>>;
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
  /** Layers below the site layer (from BASE), already normalized to OCI media types. Empty otherwise. */
  readonly baseLayers: readonly Descriptor[];
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
  const uncompressed = options.compression === 'none';
  const layerBytes = uncompressed ? tar : gzipTar(tar);
  const diffId = sha256(tar);
  const layerDigest = uncompressed ? diffId : sha256(layerBytes);
  const size = tree.entries().reduce((n, [, f]) => n + f.bytes.byteLength, 0);

  const base = options.base;
  const baseCfg = base?.config ?? {};
  const baseInner = (isRecord(baseCfg.config) ? baseCfg.config : {}) as Record<string, unknown>;
  const labels: Record<string, string> = {};
  if (isRecord(baseInner.Labels)) for (const [k, v] of Object.entries(baseInner.Labels)) if (typeof v === 'string') labels[k] = v;
  labels[ANN.root] = root;
  if (options.title) labels[ANN.title] = options.title;
  if (options.source) labels[ANN.source] = options.source;
  if (options.revision) labels[ANN.revision] = options.revision;
  if (base) {
    labels[ANN_BASE_NAME] = base.name;
    labels[ANN_BASE_DIGEST] = base.manifestDigest;
  }
  for (const [k, v] of Object.entries(options.labels ?? {})) labels[k] = v;
  // created: SOURCE_DATE_EPOCH when set; else the base's own timestamp (deterministic given the base
  // digest) or the fixed epoch when there is no base.
  const baseCreated = typeof baseCfg.created === 'string' ? baseCfg.created : undefined;
  const created = options.epoch === undefined && baseCreated !== undefined ? baseCreated : isoOf(options.epoch);
  const baseRootfs = isRecord(baseCfg.rootfs) ? baseCfg.rootfs : {};
  const baseDiffIds = Array.isArray(baseRootfs.diff_ids) ? (baseRootfs.diff_ids as unknown[]) : [];
  const baseHistory = Array.isArray(baseCfg.history) ? (baseCfg.history as unknown[]) : [];
  const baseLayers = (base?.layers ?? []).map(normalizeLayer);
  if (base && baseDiffIds.length !== baseLayers.length) throw new SitefileError('E_STORE', `base image ${base.name}: config diff_ids do not match its layers`);
  const configBytes = enc.encode(
    canonicalJson({
      ...baseCfg,
      architecture: base && typeof baseCfg.architecture === 'string' ? baseCfg.architecture : arch,
      os: base && typeof baseCfg.os === 'string' ? baseCfg.os : os,
      created,
      config: { ...baseInner, Labels: labels },
      rootfs: { type: 'layers', diff_ids: [...baseDiffIds, diffId] },
      history: [...baseHistory, { created: isoOf(options.epoch), created_by: 'sitefile', comment: 'static site files', author: 'sitefile' }],
    }),
  );
  const configDigest = sha256(configBytes);
  const annotations: Record<string, string> = { [ANN.files]: String(tree.size), [ANN.size]: String(size) };
  if (options.title) annotations[ANN.title] = options.title;
  if (base) {
    annotations[ANN_BASE_NAME] = base.name;
    annotations[ANN_BASE_DIGEST] = base.manifestDigest;
  }
  const manifestBytes = enc.encode(
    canonicalJson({
      schemaVersion: 2,
      mediaType: MEDIA.manifest,
      config: { mediaType: MEDIA.config, digest: configDigest, size: configBytes.byteLength },
      layers: [...baseLayers, { mediaType: uncompressed ? MEDIA.layerTar : MEDIA.layer, digest: layerDigest, size: layerBytes.byteLength }],
      annotations,
    }),
  );
  return { manifestBytes, manifestDigest: sha256(manifestBytes), configBytes, configDigest, layerBytes, layerDigest, diffId, root, baseLayers };
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** Docker layer media types become their OCI equivalents (same bytes, same digest). */
function normalizeLayer(d: Descriptor): Descriptor {
  const mediaType = d.mediaType === MEDIA.dockerLayer ? MEDIA.layer : d.mediaType;
  return { mediaType, digest: d.digest, size: d.size };
}

export interface IndexEntry {
  readonly digest: string;
  readonly size: number;
  readonly refName?: string | undefined;
  /** When the image was first stored locally (store bookkeeping; not part of any digest). */
  readonly stored?: string | undefined;
  /** Extra annotations written verbatim (exports only). */
  readonly extra?: Readonly<Record<string, string>> | undefined;
  /** OCI 1.1 artifactType (attestation entries of exports only). */
  readonly artifactType?: string | undefined;
}

export const ANN_CONTAINERD_NAME = 'io.containerd.image.name';

/**
 * Index annotations for an exported image named `refName` (`blog:1`, `localhost:5000/blog:1`), in the
 * convention `docker save` uses so Docker's containerd store finds the image by name after `docker load`:
 * `io.containerd.image.name` is the full normalized reference and `org.opencontainers.image.ref.name` the tag.
 * Single-component names live in docker.io/library/, other names without a registry host in docker.io/.
 */
export function exportNameAnnotations(refName: string): Record<string, string> {
  const r = parseRef(refName);
  const tag = r.tag ?? 'latest';
  const full = r.registry !== undefined ? r.name : r.name.includes('/') ? r.name : `library/${r.name}`;
  const host = r.registry ?? 'docker.io';
  return { [ANN_CONTAINERD_NAME]: `${host}/${full}:${tag}`, [ANN.refName]: tag };
}

export function encodeIndex(entries: readonly IndexEntry[]): Uint8Array {
  const manifests = entries.map((e) => {
    const annotations: Record<string, string> = {};
    if (e.refName) annotations[ANN.refName] = e.refName;
    if (e.stored) annotations[ANN.stored] = e.stored;
    Object.assign(annotations, e.extra);
    return { mediaType: MEDIA.manifest, digest: e.digest, size: e.size, artifactType: e.artifactType, ...(Object.keys(annotations).length ? { annotations } : {}) };
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

/**
 * A standalone OCI image layout holding one image, as a Tree (write it as a directory or tar it).
 * `referrers` (attestations, `build --provenance/--sbom`) are listed in index.json after the image,
 * without a name, and their blobs are included.
 */
export function layoutTree(
  img: Pick<OciImage, 'manifestBytes' | 'manifestDigest' | 'configBytes' | 'configDigest' | 'layerBytes' | 'layerDigest'>,
  refName?: string,
  /** Blobs of the layers below the site layer (digest -> bytes), for images built on a BASE. */
  baseBlobs: ReadonlyMap<string, Uint8Array> = new Map(),
  referrers: readonly Referrer[] = [],
): Tree {
  const entries: IndexEntry[] = [
    { digest: img.manifestDigest, size: img.manifestBytes.byteLength, ...(refName ? { extra: exportNameAnnotations(refName) } : {}) },
    ...referrers.map((r) => ({ digest: r.descriptor.digest, size: r.descriptor.size, artifactType: r.descriptor.artifactType, extra: r.descriptor.annotations })),
  ];
  const blobs = new Map<string, Uint8Array>([[img.manifestDigest, img.manifestBytes], [img.configDigest, img.configBytes], [img.layerDigest, img.layerBytes], ...baseBlobs]);
  for (const r of referrers) for (const [d, b] of r.blobs) blobs.set(d, b);
  const files: Array<[string, FileNode]> = [
    ['/oci-layout', makeFile(OCI_LAYOUT_BYTES, 'oci')],
    ['/index.json', makeFile(encodeIndex(entries), 'oci')],
    ...[...blobs].map(([d, b]) => [blobPathOf(d), makeFile(b, 'oci')] as [string, FileNode]),
  ];
  return Tree.fromFiles(files);
}

/** Deterministic tar of {@link layoutTree}: an "OCI archive" (`.oci.tar`). */
export function ociArchive(
  img: Parameters<typeof layoutTree>[0],
  refName: string | undefined,
  epoch: number | undefined,
  baseBlobs?: ReadonlyMap<string, Uint8Array>,
  referrers: readonly Referrer[] = [],
): Uint8Array {
  return writeTar(layoutTree(img, refName, baseBlobs, referrers), { epoch: epoch ?? 0 });
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
  // OCI manifests may omit mediaType; Docker v2 manifests (pulled images) are read as-is.
  if (m.schemaVersion !== 2 || (m.mediaType !== undefined && m.mediaType !== MEDIA.manifest && m.mediaType !== MEDIA.dockerManifest) || !Array.isArray(m.layers)) {
    throw new Error('not an image manifest (OCI or Docker v2)');
  }
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
 * Index of the layer holding the site's files: the only layer, or the last one of an image that
 * carries the `dev.sitefile.root` label (built with BASE). -1 when the image is not a Sitefile image.
 */
export function siteLayerIndex(m: ParsedManifest, cfg: ParsedConfig): number {
  if (m.layers.length === 1) return 0;
  return m.layers.length > 1 && cfg.labels[ANN.root] !== undefined ? m.layers.length - 1 : -1;
}

/**
 * Decode a site image's files, verifying the layer digest and diff_id.
 * `layerBytes` are the compressed blob bytes. Returns the tree under the image's root.
 */
export async function extractImageFiles(m: ParsedManifest, cfg: ParsedConfig, layerBytes: Uint8Array, origin: string): Promise<Tree> {
  const idx = siteLayerIndex(m, cfg);
  const layer = m.layers[idx];
  if (idx < 0 || !layer || (layer.mediaType !== MEDIA.layer && layer.mediaType !== MEDIA.layerTar)) {
    throw new SitefileError('E_STORE', `${origin}: not a Sitefile image (only single-layer tar or tar+gzip images, or images built with BASE, hold site files)`);
  }
  if (sha256(layerBytes) !== layer.digest || layerBytes.byteLength !== layer.size) {
    throw new SitefileError('E_STORE', `${origin}: layer blob does not match its digest`);
  }
  let tar: Uint8Array = layerBytes;
  if (layer.mediaType === MEDIA.layer) {
    try {
      tar = new Uint8Array(gunzipSync(layerBytes));
    } catch (e) {
      throw new SitefileError('E_STORE', `${origin}: layer is not valid gzip`, undefined, { cause: e });
    }
  }
  if (sha256(tar) !== cfg.diffIds[idx]) throw new SitefileError('E_STORE', `${origin}: layer content does not match the config diff_id`);
  const root = cfg.labels[ANN.root] ?? '/';
  const all = await extractArchive(tar, origin, { maxEntries: 1_000_000, maxTotalBytes: 8 * 2 ** 30, maxRatio: Number.MAX_SAFE_INTEGER });
  return all.subtree(normalizeVirtual(root).path);
}

/**
 * Label values for an image built from `contextDir`: the title (directory name),
 * extra `--label`s, and, only with `--git-labels`, source/revision from a git
 * checkout. Git labels are opt-in because they make the image ID change with
 * every commit even when the files don't, and because finding the checkout
 * reads outside the build context.
 */
export async function imageMeta(
  contextDir: string,
  opts: { readonly git?: boolean; readonly labels?: Readonly<Record<string, string>> } = {},
): Promise<Pick<OciOptions, 'title' | 'source' | 'revision' | 'labels'>> {
  const info = opts.git ? await gitInfo(contextDir) : {};
  return { title: path.basename(path.resolve(contextDir)), ...info, ...(opts.labels ? { labels: opts.labels } : {}) };
}

const LABEL_KEY = /^[A-Za-z0-9][A-Za-z0-9._\/-]{0,254}$/;

/** Parse `--label key=value` flags. */
export function parseLabels(flags: readonly string[]): Record<string, string> {
  const out: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const f of flags) {
    const eq = f.indexOf('=');
    const key = eq === -1 ? f : f.slice(0, eq);
    if (eq === -1 || !LABEL_KEY.test(key)) {
      throw new SitefileError('E_USAGE', `--label expects key=value with a key like org.example.team, got '${f}'`);
    }
    out[key] = f.slice(eq + 1);
  }
  return out;
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
