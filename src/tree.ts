import { SitefileError, type SourceSpan } from './errors.js';
import { sha256 } from './digest.js';
import { ancestorsVirtual } from './paths.js';

/** An immutable file inside a tree. */
export interface FileNode {
  readonly bytes: Uint8Array;
  /** 0o644 or 0o755. */
  readonly mode: number;
  /** `sha256:<hex>` of bytes. */
  readonly digest: string;
  /** Who wrote this path, e.g. `Sitefile:4 COPY`, for conflict messages and inspect. */
  readonly origin: string;
}

export type ConflictPolicy = 'error' | 'overwrite';

export function makeFile(bytes: Uint8Array, origin: string, mode = 0o644): FileNode {
  return { bytes, mode: mode & 0o111 ? 0o755 : 0o644, digest: sha256(bytes), origin };
}

/**
 * Immutable snapshot of a filesystem tree. Keys are normalized absolute
 * virtual paths of files; directories are implicit. Every operation returns
 * a new Tree.
 */
export class Tree {
  static readonly empty = new Tree(new Map(), new Set());

  private constructor(
    private readonly files: ReadonlyMap<string, FileNode>,
    private readonly dirs: ReadonlySet<string>,
  ) {}

  get size(): number {
    return this.files.size;
  }

  get(path: string): FileNode | undefined {
    return this.files.get(path);
  }

  isFile(path: string): boolean {
    return this.files.has(path);
  }

  /** True for `/` and any implicit directory. */
  isDirectory(path: string): boolean {
    return path === '/' || this.dirs.has(path);
  }

  /** Sorted [path, node] pairs. */
  entries(): Array<[string, FileNode]> {
    return [...this.files.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  }

  /**
   * Add files. Writing identical content (digest and mode) is a no-op that
   * keeps the first owner. Different content fails with E_CONFLICT unless the
   * policy is 'overwrite'. A file can never replace a directory or vice versa.
   */
  withFiles(additions: Iterable<readonly [string, FileNode]>, policy: ConflictPolicy, span?: SourceSpan): Tree {
    const files = new Map(this.files);
    const dirs = new Set(this.dirs);
    for (const [path, node] of additions) {
      if (path === '/' || dirs.has(path)) {
        throw new SitefileError('E_CONFLICT', `cannot write file '${path}': it is a directory`, span);
      }
      for (const a of ancestorsVirtual(path)) {
        const f = files.get(a);
        if (f) {
          throw new SitefileError(
            'E_CONFLICT',
            `cannot write '${path}': '${a}' is a file (written by ${f.origin})`,
            span,
          );
        }
      }
      const existing = files.get(path);
      if (existing) {
        if (existing.digest === node.digest && existing.mode === node.mode) continue;
        if (policy === 'error') {
          throw new SitefileError(
            'E_CONFLICT',
            `'${path}' was already written with different content by ${existing.origin}; use --overwrite to replace it`,
            span,
          );
        }
      }
      files.set(path, node);
      for (const a of ancestorsVirtual(path)) dirs.add(a);
    }
    return new Tree(files, dirs);
  }

  /** The files under a directory, re-rooted at `/`. */
  subtree(dir: string): Tree {
    if (dir === '/') return this;
    const prefix = dir + '/';
    const out: Array<[string, FileNode]> = [];
    for (const [p, n] of this.files) if (p.startsWith(prefix)) out.push([p.slice(dir.length), n]);
    return Tree.empty.withFiles(out, 'error');
  }

  /** The files of this tree placed under `dir`. */
  mountedAt(dir: string): Array<[string, FileNode]> {
    return this.entries().map(([p, n]) => [dir === '/' ? p : dir + p, n]);
  }

  /** Content digest of the whole tree (paths, modes and file digests). */
  digest(): string {
    return sha256(this.entries().map(([p, n]) => `${p}\0${n.mode.toString(8)}\0${n.digest}\n`).join(''));
  }

  static fromFiles(files: Iterable<readonly [string, FileNode]>): Tree {
    return Tree.empty.withFiles(files, 'error');
  }
}
