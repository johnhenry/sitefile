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

interface LockfileJson {
  version: 1;
  resources: Record<string, LockEntry>;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** `Sitefile.lock`: pinned digests of remote resources. */
export class Lockfile {
  private readonly entries: Map<string, LockEntry>;
  private readonly used = new Set<string>();

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
    return new Lockfile(path, entries, true, text);
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
    return JSON.stringify(data, null, 2) + '\n';
  }

  /**
   * Write the entries used in this build. Returns true if the file was
   * written. Does nothing when the content is unchanged, and never creates
   * a lockfile that would have no entries.
   */
  async save(): Promise<boolean> {
    const text = this.serialize();
    if (!this.existed && this.used.size === 0) return false;
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
