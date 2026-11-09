import { randomBytes } from 'node:crypto';
import { promises as fs } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import { siteFileWarnings } from './serve-rules.js';
import type { SiteContent } from './server.js';
import { blobPathOf, decodeIndex, extractImageFiles, MEDIA, parseConfig, parseManifest, siteLayerIndices } from './oci.js';
import { extractArchive, openZipFile, osMetadataWarning, withoutOsMetadata } from './resources/archive.js';
import { LocalContext } from './resources/local.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits } from './resources/types.js';
import { Store } from './store.js';
import { Tree } from './tree.js';
import type { CachePreview } from './server.js';
import type { AuthEntry } from './serve-auth.js';

/**
 * Detached site servers ("containers"). One state file per server:
 *
 *   $SITEFILE_HOME/servers/<id>.json   state (this interface)
 *   $SITEFILE_HOME/servers/<id>.log    stdout/stderr of the server process
 *
 * `serve -d` writes the state, spawns `node serve-worker.js <id>` detached, and the worker
 * fills in `pid`, `host`, `port` and `ready` once it is listening. `servers` never trusts the
 * file alone: a server is `running` only when its pid is alive AND the port answers with
 * this server's `x-sitefile-server` header.
 */
export interface ServerState {
  readonly id: string;
  readonly name: string;
  /** What the user asked to serve: the image reference as typed, or the directory. */
  readonly image: string;
  readonly source:
    | { readonly kind: 'image'; readonly imageId: string }
    | { readonly kind: 'dir'; readonly path: string }
    /** zip, tar, tar.gz/tgz, a Web Bundle or an OCI image archive; read again on every start, as a directory is. */
    | { readonly kind: 'archive'; readonly path: string };
  readonly spa: boolean;
  /** `--spa-exclude`: request paths that never fall back (absent: none). */
  readonly spaExclude?: readonly string[];
  /** `--spa-fallback`: the file every fallback serves (absent: the nearest 200.html or index.html). */
  readonly spaFallback?: string;
  /** `--base`: the path prefix the site is served under, `/repo/` (absent: `/`). */
  readonly base?: string;
  /** `--keep-os-metadata`: serve a directory's or archive's OS metadata files too (absent: false). */
  readonly keepOsMetadata?: boolean;
  /** `--allowed-host` names (absent: none; localhost names and IP addresses are always allowed). */
  readonly allowedHosts?: readonly string[];
  /** `--cors` (absent: false). */
  readonly cors?: boolean;
  /** `--isolate`: the Cross-Origin-Embedder-Policy sent with COOP and CORP (absent: not isolated). */
  readonly isolate?: 'require-corp' | 'credentialless';
  /** `--max-age`, `--immutable`, `--max-age-html` (absent: every response keeps `no-cache`). */
  readonly cache?: CachePreview;
  /** `--log-format json` (absent: text). */
  readonly logFormat?: 'json';
  /** `--log-ip` (absent: false). */
  readonly logIp?: boolean;
  /** `--quiet` (absent: false). */
  readonly quiet?: boolean;
  /** `--auth`, `SITEFILE_SERVE_AUTH`: users and password verifiers (never passwords; absent: none). */
  readonly auth?: readonly AuthEntry[];
  /** `--auth-file`: an htpasswd file, read at every start (absent: none). */
  readonly authFile?: string;
  /** `--country`: what `_redirects` `Country=` conditions match (absent: none matches). */
  readonly country?: string;
  /** `--strict-site-files`: an ignored `_redirects`/`_headers` line stops the server from starting (absent: false). */
  readonly strictSiteFiles?: boolean;
  /** The requested bind address (port 0 = pick a free port). */
  readonly bind: { readonly host: string; readonly port: number };
  readonly host: string;
  readonly port: number;
  readonly pid: number;
  readonly started: string;
  readonly logFile: string;
  readonly ready: boolean;
  readonly error?: string;
  /** The servers file (`sitefile up`) that started it, by real path; absent for `serve -d`. */
  readonly project?: string;
}

export interface ServerView extends ServerState {
  readonly status: 'running' | 'exited';
  readonly url: string;
}

const NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,62}$/;

export class ServerRegistry {
  readonly dir: string;
  private constructor(readonly store: Store) {
    this.dir = path.join(store.home, 'servers');
  }

  static async open(store?: Store): Promise<ServerRegistry> {
    const reg = new ServerRegistry(store ?? (await Store.open()));
    await fs.mkdir(reg.dir, { recursive: true, mode: 0o700 });
    return reg;
  }

  statePath(id: string): string {
    if (!/^[0-9a-f]{12}$/.test(id)) throw new SitefileError('E_SERVER', `invalid server id '${id}'`);
    return path.join(this.dir, `${id}.json`);
  }

  logPath(id: string): string {
    return path.join(this.dir, `${id}.log`);
  }

  async read(id: string): Promise<ServerState | undefined> {
    try {
      return JSON.parse(await fs.readFile(this.statePath(id), 'utf8')) as ServerState;
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === 'ENOENT' || e instanceof SyntaxError) return undefined;
      throw e;
    }
  }

  async write(state: ServerState): Promise<void> {
    const dest = this.statePath(state.id);
    const tmp = `${dest}.${randomBytes(4).toString('hex')}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(state, null, 2) + '\n', { mode: 0o600 });
    await fs.rename(tmp, dest);
  }

  async update(id: string, patch: Partial<ServerState>): Promise<ServerState | undefined> {
    const cur = await this.read(id);
    if (!cur) return undefined;
    const next = { ...cur, ...patch } as ServerState;
    await this.write(next);
    return next;
  }

  async remove(id: string): Promise<void> {
    await fs.rm(this.statePath(id), { force: true });
    await fs.rm(this.logPath(id), { force: true });
  }

  async all(): Promise<ServerState[]> {
    const out: ServerState[] = [];
    for (const f of await fs.readdir(this.dir)) {
      const m = /^([0-9a-f]{12})\.json$/.exec(f);
      const s = m ? await this.read(m[1] as string) : undefined;
      if (s) out.push(s);
    }
    return out.sort((a, b) => (a.started < b.started ? -1 : a.started > b.started ? 1 : 0));
  }

  /** Find by exact ID, exact name, or unique ID prefix. */
  async find(key: string): Promise<ServerState> {
    const all = await this.all();
    const hit = all.find((s) => s.id === key) ?? all.find((s) => s.name === key);
    if (hit) return hit;
    const pre = key.length >= 3 ? all.filter((s) => s.id.startsWith(key)) : [];
    if (pre.length === 1) return pre[0] as ServerState;
    if (pre.length > 1) throw new SitefileError('E_SERVER', `ambiguous server '${key}' matches ${pre.length} servers`);
    throw new SitefileError('E_NOT_FOUND', `no such server: ${key}`);
  }

  /** Allocate an ID and validate/choose the name (names are unique among all servers, running or not). */
  async allocate(name: string | undefined): Promise<{ id: string; name: string }> {
    const all = await this.all();
    if (name !== undefined) {
      if (!NAME_RE.test(name)) throw new SitefileError('E_USAGE', `invalid server name '${name}' (letters, digits, '_', '.', '-')`);
      if (all.some((s) => s.name === name)) throw new SitefileError('E_SERVER', `a server named '${name}' already exists; remove it with "sitefile rm ${name}"`);
    }
    const id = randomBytes(6).toString('hex');
    return { id, name: name ?? `site-${id.slice(0, 6)}` };
  }

  async view(s: ServerState): Promise<ServerView> {
    const running = await isRunning(s);
    return { ...s, status: running ? 'running' : 'exited', url: s.ready ? urlOf(s) + (s.base ?? '/').slice(1) : '' };
  }
}
function urlOf(s: Pick<ServerState, 'host' | 'port'>): string {
  const shown = s.host === '0.0.0.0' ? '127.0.0.1' : s.host === '::' ? '::1' : s.host;
  return `http://${shown.includes(':') ? `[${shown}]` : shown}:${s.port}/`;
}

export function pidAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === 'EPERM';
  }
}

/** Alive AND answering on its port with its own id header (guards against pid and port reuse). */
export async function isRunning(s: ServerState): Promise<boolean> {
  if (!s.ready || !pidAlive(s.pid)) return false;
  const host = s.host === '0.0.0.0' ? '127.0.0.1' : s.host === '::' ? '::1' : s.host;
  return new Promise((resolve) => {
    const req = http.request({ host, port: s.port, method: 'HEAD', path: '/', headers: { 'x-sitefile-probe': s.id }, timeout: 1500, agent: false }, (res) => {
      res.resume();
      resolve(res.headers['x-sitefile-server'] === s.id);
    });
    req.on('timeout', () => req.destroy());
    req.on('error', () => resolve(false));
    req.end();
  });
}

/** Paths of `tree` a directory or archive serves: no hidden files except `.well-known`. */
function visible(tree: Tree): Tree {
  return Tree.fromFiles(tree.entries().filter(([p]) => p.split('/').every((seg) => !seg.startsWith('.') || seg === '' || seg === '.well-known')));
}

/** How a directory or archive is read for serving. */
export interface ServedTreeOptions {
  /** Serve OS metadata files (`__MACOSX/`, `._*`, `Thumbs.db`, `desktop.ini`, ...) too; default false. */
  readonly keepOsMetadata?: boolean;
  /** Told how many OS metadata files were skipped. */
  readonly warn?: (message: string) => void;
}

/** `tree` as served: without OS metadata (unless kept; with a warning naming `origin`) and hidden files except `.well-known`. */
function served(tree: Tree, origin: string, options: ServedTreeOptions): Tree {
  if (!options.keepOsMetadata) {
    const r = withoutOsMetadata(tree);
    if (r.skipped > 0) options.warn?.(osMetadataWarning(origin, r.skipped));
    tree = r.tree;
  }
  return visible(tree);
}

/**
 * Load a directory into a Tree through the sandboxed local reader; hidden files (except .well-known)
 * and OS metadata files are not served.
 */
export async function loadDirectoryTree(dir: string, options: ServedTreeOptions = {}): Promise<Tree> {
  const ctx = await LocalContext.open(dir, { sitefileignore: false });
  const { tree } = await ctx.readTree('', 'serve');
  return served(tree, dir, options);
}

/**
 * Load an archive file into a Tree. The format comes from the bytes, never the name: zip, tar, tar.gz,
 * a Web Bundle, or an OCI image archive (a tar holding an OCI layout, as `build -o x.oci.tar` and `save`
 * write it). A zip, tar or Web Bundle is extracted with `limits` (unsafe entries are refused) and served
 * like a directory, without hidden files except `.well-known` and without OS metadata files (unless
 * `options.keepOsMetadata`); an OCI archive serves its first image's files, like that image from the
 * store, after checking every blob's digest and its site layer against `limits`.
 * `hidden` keeps hidden files and OS metadata (verify and diff compare every file).
 */
export async function loadArchiveTree(
  file: string,
  limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
  options: ServedTreeOptions & { readonly hidden?: boolean } = {},
): Promise<Tree> {
  const origin = file;
  const unreadable = await checkArchiveFile(file, limits);
  const bytes = new Uint8Array(await fs.readFile(file).catch(() => { throw unreadable(); }));
  const tree = await extractArchive(bytes, origin, limits);
  if (tree.get('/oci-layout')) return ociArchiveTree(tree, origin, limits);
  return options.hidden ? tree : served(tree, origin, options);
}

/** The checks every archive file gets before it is read; returns the error for an unreadable file. */
async function checkArchiveFile(file: string, limits: ArchiveLimits): Promise<() => SitefileError> {
  const unreadable = (): SitefileError => new SitefileError('E_ARCHIVE', `cannot read '${file}'`);
  const st = await fs.stat(file).catch((e: NodeJS.ErrnoException) => {
    throw e.code === 'ENOENT' ? new SitefileError('E_NOT_FOUND', `'${file}' does not exist`) : unreadable();
  });
  if (!st.isFile()) throw new SitefileError('E_TYPE', `'${file}' is not a file`);
  // The archive is at most its expanded size plus tar framing for every entry.
  if (st.size > limits.maxTotalBytes + (limits.maxEntries + 2) * 4 * 512) {
    throw new SitefileError('E_ARCHIVE_LIMIT', `${file}: archive expands beyond ${limits.maxTotalBytes} bytes`);
  }
  return unreadable;
}

/** What a server serves: the tree and, for a zip served in place, where its contents come from. */
export interface ServedSite {
  readonly tree: Tree;
  readonly content?: SiteContent;
  /** Release the open zip file, if any. */
  close(): void;
}

/**
 * A zip archive file opened for serving in place (#213): its entries are checked and indexed at
 * start (each inflated once, one at a time, for its CRC-32 and digest) and read from the file on
 * each request, so memory does not grow with the archive. The tree is the one {@link loadArchiveTree}
 * serves, with the same errors. Undefined for anything that is not a zip (or a zip holding an OCI layout).
 */
export async function openZipArchive(file: string, limits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS, options: ServedTreeOptions = {}): Promise<ServedSite | undefined> {
  const unreadable = await checkArchiveFile(file, limits);
  const head = Buffer.alloc(4);
  try {
    const fh = await fs.open(file, 'r');
    try {
      await fh.read(head, 0, 4, 0);
    } finally {
      await fh.close();
    }
  } catch {
    throw unreadable();
  }
  const zip = head[0] === 0x50 && head[1] === 0x4b && ((head[2] === 3 && head[3] === 4) || (head[2] === 5 && head[3] === 6));
  if (!zip) return undefined;
  const z = await openZipFile(file, file, limits);
  if (z.tree.get('/oci-layout')) {
    z.close();
    return undefined;
  }
  return { tree: served(z.tree, file, options), content: z, close: () => z.close() };
}

/** {@link loadServedTree}, serving a zip archive in place ({@link openZipArchive}). */
export async function loadServedSite(store: Store, source: ServerState['source'], options: ServedTreeOptions = {}): Promise<ServedSite> {
  if (source.kind === 'archive') {
    const site = await openZipArchive(source.path, DEFAULT_ARCHIVE_LIMITS, options);
    if (site) return site;
  }
  return { tree: await loadServedTree(store, source, options), close: () => undefined };
}

/** The files of the first image in an extracted OCI layout. */
async function ociArchiveTree(layout: Tree, origin: string, limits: ArchiveLimits): Promise<Tree> {
  const bad = (why: string): SitefileError => new SitefileError('E_ARCHIVE', `${origin}: ${why}`);
  const blob = (digest: string, size: number): Uint8Array => {
    const node = layout.get(blobPathOf(digest));
    if (!node) throw bad(`OCI archive has no blob ${digest}`);
    if (sha256(node.bytes) !== digest || node.bytes.byteLength !== size) throw bad(`blob ${digest} does not match its digest`);
    return node.bytes;
  };
  const index = layout.get('/index.json');
  if (!index) throw bad('OCI archive has no index.json');
  let entries;
  try {
    entries = decodeIndex(index.bytes);
  } catch {
    throw bad('OCI archive index.json is corrupt');
  }
  const first = entries[0];
  if (!first) throw bad('OCI archive holds no image');
  const manifestBytes = blob(first.digest, first.size);
  let m;
  try {
    m = parseManifest(manifestBytes);
  } catch {
    throw bad("OCI archive's first manifest is not an image manifest");
  }
  const configBytes = blob(m.config.digest, m.config.size);
  let cfg;
  try {
    cfg = parseConfig(configBytes);
  } catch {
    throw bad("OCI archive's image config is corrupt");
  }
  const layers = siteLayerIndices(m, cfg).map((i) => m.layers[i]!);
  // An empty list makes extractImageFiles report "not a Sitefile image".
  const blobs: Uint8Array[] = [];
  if (layers.every((l) => l.mediaType === MEDIA.layer || l.mediaType === MEDIA.layerTar)) {
    for (const l of layers) {
      const b = blob(l.digest, l.size);
      // Hold each site layer to the archive limits before it is fully decoded.
      await extractArchive(b, origin, limits);
      blobs.push(b);
    }
  }
  return extractImageFiles(m, cfg, layers.length ? blobs : [new Uint8Array(0)], origin);
}

/** The tree a server state serves (`options` apply to a directory or archive). */
export async function loadServedTree(store: Store, source: ServerState['source'], options: ServedTreeOptions = {}): Promise<Tree> {
  switch (source.kind) {
    case 'image':
      return store.loadTree(source.imageId);
    case 'dir':
      return loadDirectoryTree(source.path, options);
    case 'archive':
      return loadArchiveTree(source.path, DEFAULT_ARCHIVE_LIMITS, options);
  }
}

/**
 * Warn about every `_redirects`/`_headers` line `serve` ignores; with `strict` (`--strict-site-files`)
 * any such line is then an error.
 */
export function checkSiteFiles(tree: Tree, strict: boolean, warn: (m: string) => void, content?: SiteContent): void {
  const ignored = siteFileWarnings(tree, content);
  for (const m of ignored) warn(m);
  if (strict && ignored.length > 0) {
    const n = ignored.length;
    throw new SitefileError('E_VALIDATION', `${n} line${n === 1 ? '' : 's'} of _redirects/_headers ${n === 1 ? 'is' : 'are'} ignored (--strict-site-files)`);
  }
}

/**
 * When a server's files last changed, in Unix seconds (`Last-Modified`): for an image, when it was
 * first stored (never older than its files, unlike the config's `created`, which is the Unix epoch
 * unless SOURCE_DATE_EPOCH is set); for a directory, the newest modification time of the files it
 * serves (hidden entries but `.well-known` are skipped, symbolic links to files followed); for an
 * archive, the file's modification time. Undefined when there is none.
 */
export async function servedLastModified(store: Store, source: ServerState['source']): Promise<number | undefined> {
  const secs = (ms: number): number => Math.floor(ms / 1000);
  switch (source.kind) {
    case 'image': {
      const ms = Date.parse((await store.details(source.imageId)).created);
      return Number.isNaN(ms) ? undefined : secs(ms);
    }
    case 'archive':
      return secs((await fs.stat(source.path)).mtimeMs);
    case 'dir': {
      let newest: number | undefined;
      const walk = async (dir: string): Promise<void> => {
        for (const e of await fs.readdir(dir, { withFileTypes: true }).catch(() => [])) {
          if (e.name.startsWith('.') && e.name !== '.well-known') continue;
          const p = path.join(dir, e.name);
          if (e.isDirectory()) await walk(p);
          else if (e.isFile() || e.isSymbolicLink()) {
            const st = await fs.stat(p).catch(() => undefined);
            if (st?.isFile()) newest = Math.max(newest ?? 0, secs(st.mtimeMs));
          }
        }
      };
      await walk(source.path);
      return newest;
    }
  }
}
