import { promises as fs } from 'node:fs';
import { SitefileError } from '../errors.js';
import { INTEGRITY_RE } from '../digest.js';

export interface LockEntry {
  /** `sha256:<hex>` of the response body. */
  readonly integrity: string;
  /** Body size in bytes. */
  readonly size: number;
  /** Final URL after redirects. */
  readonly resolved: string;
}

/** A container image reference (`BASE`) pinned to the digest its tag resolved to. */
export interface ImageLockEntry {
  /** `sha256:<hex>` of what the tag pointed at: an image index or a manifest. */
  readonly digest: string;
}

/** A Git IMPORT (`git+https://…#ref:path`, keyed by the URI as written) pinned to the commit its ref resolved to. */
export interface GitLockEntry {
  /** 40 lowercase hex digits. */
  readonly commit: string;
}

interface LockfileJson {
  version: 1;
  resources: Record<string, LockEntry>;
  images?: Record<string, ImageLockEntry>;
  git?: Record<string, GitLockEntry>;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** `Sitefile.lock`: pinned digests of remote resources. */
export class Lockfile {
  private readonly entries: Map<string, LockEntry>;
  private readonly used = new Set<string>();
  private readonly images = new Map<string, ImageLockEntry>();
  private readonly usedImages = new Set<string>();
  private readonly gits = new Map<string, GitLockEntry>();
  private readonly usedGits = new Set<string>();

  private constructor(
    readonly path: string,
    entries: Map<string, LockEntry>,
    private existed: boolean,
    private originalText: string | undefined,
  ) {
    this.entries = entries;
  }

  static async load(path: string): Promise<Lockfile> {
    let text: string;
    try {
      text = await fs.readFile(path, 'utf8');
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === 'ENOENT') return new Lockfile(path, new Map(), false, undefined);
      throw e;
    }
    const bad = (why: string, cause?: unknown): SitefileError =>
      new SitefileError('E_VALIDATION', `malformed lockfile ${path}: ${why}`, undefined, cause ? { cause } : undefined);
    let data: unknown;
    try {
      data = JSON.parse(text);
    } catch (e) {
      throw bad((e as Error).message, e);
    }
    if (!isObject(data)) throw bad('expected a JSON object');
    if (data.version !== 1) throw bad(`unsupported version ${JSON.stringify(data.version)} (expected 1)`);
    const resources = data.resources ?? {};
    if (!isObject(resources)) throw bad('"resources" must be an object');
    const entries = new Map<string, LockEntry>();
    for (const [url, raw] of Object.entries(resources)) {
      if (!isObject(raw)) throw bad(`entry for ${url} must be an object`);
      const { integrity, size, resolved } = raw;
      if (typeof integrity !== 'string' || !INTEGRITY_RE.test(integrity)) {
        throw bad(`entry for ${url} has an invalid "integrity" (expected sha256:<64 hex>)`);
      }
      if (typeof size !== 'number' || !Number.isSafeInteger(size) || size < 0) {
        throw bad(`entry for ${url} has an invalid "size"`);
      }
      if (typeof resolved !== 'string') throw bad(`entry for ${url} has an invalid "resolved"`);
      entries.set(url, { integrity, size, resolved });
    }
    const lock = new Lockfile(path, entries, true, text);
    const images = data.images ?? {};
    if (!isObject(images)) throw bad('"images" must be an object');
    for (const [ref, raw] of Object.entries(images)) {
      if (!isObject(raw) || typeof raw.digest !== 'string' || !INTEGRITY_RE.test(raw.digest)) {
        throw bad(`image entry for ${ref} needs a "digest" (sha256:<64 hex>)`);
      }
      lock.images.set(ref, { digest: raw.digest });
    }
    const gits = data.git ?? {};
    if (!isObject(gits)) throw bad('"git" must be an object');
    for (const [url, raw] of Object.entries(gits)) {
      if (!isObject(raw) || typeof raw.commit !== 'string' || !/^[0-9a-f]{40}$/.test(raw.commit)) {
        throw bad(`git entry for ${url} needs a "commit" (40 lowercase hex digits)`);
      }
      lock.gits.set(url, { commit: raw.commit });
    }
    return lock;
  }

  /** The commit a Git IMPORT URI was locked to. */
  getGit(url: string): GitLockEntry | undefined {
    return this.gits.get(url);
  }

  recordGit(url: string, entry: GitLockEntry): void {
    this.gits.set(url, { commit: entry.commit });
    this.usedGits.add(url);
  }

  /** The pinned digest for a canonical image reference (`docker.io/library/nginx:alpine`). */
  getImage(ref: string): ImageLockEntry | undefined {
    return this.images.get(ref);
  }

  recordImage(ref: string, entry: ImageLockEntry): void {
    this.images.set(ref, { digest: entry.digest });
    this.usedImages.add(ref);
  }

  markImageUsed(ref: string): void {
    if (this.images.has(ref)) this.usedImages.add(ref);
  }

  get(url: string): LockEntry | undefined {
    return this.entries.get(url);
  }

  /** Add or replace an entry and mark it used in this build. */
  record(url: string, entry: LockEntry): void {
    this.entries.set(url, { integrity: entry.integrity, size: entry.size, resolved: entry.resolved });
    this.used.add(url);
  }

  /** Keep an existing entry in the saved lockfile. */
  markUsed(url: string): void {
    if (this.entries.has(url)) this.used.add(url);
  }

  /** The JSON text `save()` would write. */
  serialize(): string {
    const resources: Record<string, LockEntry> = {};
    for (const url of [...this.used].sort()) {
      const e = this.entries.get(url);
      if (e) resources[url] = { integrity: e.integrity, size: e.size, resolved: e.resolved };
    }
    const data: LockfileJson = { version: 1, resources };
    if (this.usedImages.size > 0) {
      const images: Record<string, ImageLockEntry> = {};
      for (const ref of [...this.usedImages].sort()) {
        const e = this.images.get(ref);
        if (e) images[ref] = { digest: e.digest };
      }
      data.images = images;
    }
    if (this.usedGits.size > 0) {
      const gits: Record<string, GitLockEntry> = {};
      for (const url of [...this.usedGits].sort()) {
        const e = this.gits.get(url);
        if (e) gits[url] = { commit: e.commit };
      }
      data.git = gits;
    }
    return JSON.stringify(data, null, 2) + '\n';
  }

  /**
   * Write the entries used in this build. Returns true if the file was
   * written. Does nothing when the content is unchanged, and never creates
   * a lockfile that would have no entries.
   */
  async save(): Promise<boolean> {
    const text = this.serialize();
    if (!this.existed && this.used.size === 0 && this.usedImages.size === 0 && this.usedGits.size === 0) return false;
    if (this.existed && text === this.originalText) return false;
    const tmp = `${this.path}.${process.pid}.${Date.now()}.tmp`;
    try {
      await fs.writeFile(tmp, text);
      await fs.rename(tmp, this.path);
      this.existed = true;
      this.originalText = text;
    } catch (e) {
      await fs.rm(tmp, { force: true });
      throw e;
    }
    return true;
  }
}
