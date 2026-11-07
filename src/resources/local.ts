import { promises as fs, type Stats } from 'node:fs';
import { join, sep, relative, posix } from 'node:path';
import { SitefileError, type SourceSpan } from '../errors.js';
import { sha256 } from '../digest.js';
import { Tree, makeFile, type FileNode } from '../tree.js';
import { nfc } from '../paths.js';
import { IgnoreRules } from './ignore.js';
import type { FileResource, TreeResource } from './types.js';

export interface LocalContextOptions {
  /** Context-relative POSIX paths skipped while walking directories. Default `['.git']`. */
  readonly ignore?: readonly string[];
  /** Read `.sitefileignore` from the context root (default true). Applies to directory walks only. */
  readonly sitefileignore?: boolean;
  /**
   * Permit regular files with more than one hard link (default false). realpath cannot
   * tell whether a hard link points outside the context, so by default such files are
   * refused with `E_HARDLINK`. Allowing them means you assert they are safe.
   */
  readonly allowHardlinks?: boolean;
}

export const SITEFILEIGNORE = '.sitefileignore';

const NON_ASCII = /[^\x00-\x7f]/;

function normalizationCollision(rel: string, span?: SourceSpan): SitefileError {
  return new SitefileError(
    'E_PATH_COLLISION',
    `'${display(rel)}' names more than one file in the build context: their names differ only in Unicode normalization (NFC/NFD); rename one`,
    span,
  );
}

function errCode(e: unknown): string | undefined {
  return (e as NodeJS.ErrnoException | null)?.code;
}

async function exists(p: string): Promise<boolean> {
  return fs.lstat(p).then(
    () => true,
    () => false,
  );
}

function display(rel: string): string {
  return rel === '' ? '.' : './' + rel;
}

/**
 * The local build context. Every read is confined to `root`: real paths are
 * resolved and must stay inside it.
 */
export class LocalContext {
  private readonly ignore: readonly string[];

  private constructor(
    readonly root: string,
    ignore: readonly string[],
    private readonly rules: IgnoreRules,
    private readonly allowHardlinks: boolean,
  ) {
    this.ignore = ignore.map((p) => posix.normalize(nfc(p)).replace(/^\.\/+/, '').replace(/\/+$/, '')).filter((p) => p !== '' && p !== '.');
  }

  static async open(dir: string, options?: LocalContextOptions): Promise<LocalContext> {
    let root: string;
    try {
      root = await fs.realpath(dir);
    } catch (e) {
      if (errCode(e) === 'ENOENT') throw new SitefileError('E_NOT_FOUND', `build context '${dir}' does not exist`, undefined, { cause: e });
      throw e;
    }
    const st = await fs.stat(root);
    if (!st.isDirectory()) throw new SitefileError('E_UNSUPPORTED_FILE', `build context '${dir}' is not a directory`);
    let rules = IgnoreRules.empty;
    if (options?.sitefileignore !== false && (await exists(join(root, SITEFILEIGNORE)))) {
      // Read through the sandbox: a symlinked .sitefileignore must stay inside the context.
      const probe = new LocalContext(root, [], IgnoreRules.empty, true);
      // A broken ignore file fails the build: skipping it could publish files it was meant to exclude.
      const { real, st } = await probe.resolve(SITEFILEIGNORE).catch((e: unknown) => {
        if (!(e instanceof SitefileError) || e.code !== 'E_NOT_FOUND') throw e;
        throw new SitefileError(
          'E_NOT_FOUND',
          `'${SITEFILEIGNORE}' is a symlink to a missing file; fix or remove it (its rules would otherwise be skipped)`,
          undefined,
          { cause: e },
        );
      });
      if (!st.isFile()) throw new SitefileError('E_UNSUPPORTED_FILE', `'${SITEFILEIGNORE}' is not a regular file`);
      rules = IgnoreRules.parse(nfc(await fs.readFile(real, 'utf8')));
    }
    return new LocalContext(root, options?.ignore ?? ['.git'], rules, options?.allowHardlinks ?? false);
  }

  private inside(real: string): boolean {
    return real === this.root || real.startsWith(this.root.endsWith(sep) ? this.root : this.root + sep);
  }

  private isIgnored(rel: string): boolean {
    return this.ignore.some((ig) => rel === ig || rel.startsWith(ig + '/'));
  }

  /** Hard-linked files may alias files outside the context, which realpath cannot detect. */
  private checkHardlink(st: Stats, rel: string, span?: SourceSpan): void {
    if (this.allowHardlinks || !st.isFile() || st.nlink <= 1) return;
    throw new SitefileError(
      'E_HARDLINK',
      `'${display(rel)}' has ${st.nlink} hard links and may alias a file outside the build context (--allow-hardlinks permits this)`,
      span,
    );
  }

  /** Context-relative POSIX path of a real path inside root. */
  private relOfReal(real: string): string {
    return relative(this.root, real).split(sep).join('/');
  }

  /** Resolve a context-relative path to its real path and stats, enforcing the sandbox. */
  /** A context file's real path and bytes, with the usual sandbox, symlink and hard-link rules. */
  async realFile(rel: string, span?: SourceSpan): Promise<{ real: string; bytes: Uint8Array }> {
    const { real } = await this.resolve(rel, span);
    const f = await this.readFile(rel, span);
    return { real, bytes: f.bytes };
  }

  /**
   * The host path of the context-relative (NFC) path `rel`. A non-ASCII
   * segment is matched by NFC form against its directory listing, so a file
   * is found whichever normalization its on-disk name uses (macOS stores what
   * it was given, often NFD; Linux compares bytes). Two names in one
   * directory with the same NFC form are E_PATH_COLLISION. A segment with no
   * match is joined as is, so the caller sees the usual ENOENT.
   */
  private async hostPath(rel: string, span?: SourceSpan): Promise<string> {
    if (rel === '') return this.root;
    if (!NON_ASCII.test(rel)) return join(this.root, ...rel.split('/'));
    let cur = this.root;
    let done = '';
    for (const seg of rel.split('/')) {
      done = done === '' ? seg : `${done}/${seg}`;
      if (!NON_ASCII.test(seg)) {
        cur = join(cur, seg);
        continue;
      }
      const names = await fs.readdir(cur).catch(() => [] as string[]);
      const matches = names.filter((n) => nfc(n) === seg);
      if (matches.length > 1) throw normalizationCollision(done, span);
      cur = join(cur, matches[0] ?? seg);
    }
    return cur;
  }

  private async resolve(rel: string, span?: SourceSpan, linkName?: string): Promise<{ real: string; st: Stats }> {
    let real: string;
    try {
      real = await fs.realpath(await this.hostPath(rel, span));
    } catch (e) {
      if (e instanceof SitefileError) throw e;
      const code = errCode(e);
      if (code === 'ENOENT' || code === 'ENOTDIR') {
        throw new SitefileError(
          'E_NOT_FOUND',
          linkName ? `symlink '${linkName}' is dangling` : `'${display(rel)}' does not exist in the build context`,
          span,
          { cause: e },
        );
      }
      if (code === 'ELOOP') {
        throw new SitefileError('E_UNSUPPORTED_FILE', `'${display(rel)}' is a symlink loop`, span, { cause: e });
      }
      throw new SitefileError('E_NOT_FOUND', `cannot resolve '${display(rel)}': ${(e as Error).message}`, span, { cause: e });
    }
    if (!this.inside(real)) {
      throw new SitefileError(
        'E_PATH_ESCAPE',
        linkName
          ? `symlink '${linkName}' points outside the build context`
          : `'${display(rel)}' resolves outside the build context`,
        span,
      );
    }
    const st = await fs.stat(real);
    return { real, st };
  }

  private static kindOf(st: Stats, rel: string, span?: SourceSpan): 'file' | 'directory' {
    if (st.isFile()) return 'file';
    if (st.isDirectory()) return 'directory';
    throw new SitefileError(
      'E_UNSUPPORTED_FILE',
      `'${display(rel)}' is not a regular file or directory (sockets, fifos and devices are not supported)`,
      span,
    );
  }

  async stat(rel: string, span?: SourceSpan): Promise<'file' | 'directory'> {
    const { st } = await this.resolve(rel, span);
    return LocalContext.kindOf(st, rel, span);
  }

  /** The real path of a context-relative path, with the sandbox checks of every read (an imported Sitefile's context). */
  async realPath(rel: string, span?: SourceSpan): Promise<string> {
    return (await this.resolve(rel, span)).real;
  }

  async readFile(rel: string, span?: SourceSpan): Promise<FileResource> {
    const { real, st } = await this.resolve(rel, span);
    if (LocalContext.kindOf(st, rel, span) !== 'file') {
      throw new SitefileError('E_UNSUPPORTED_FILE', `'${display(rel)}' is a directory, not a file`, span);
    }
    this.checkHardlink(st, rel, span);
    const bytes = new Uint8Array(await fs.readFile(real));
    return {
      kind: 'file',
      bytes,
      mode: st.mode & 0o111 ? 0o755 : 0o644,
      basename: posix.basename(rel),
      provenance: { uri: './' + rel, digest: sha256(bytes) },
    };
  }

  /** `exclude` (COPY --exclude) skips paths relative to `rel`, like `.sitefileignore` rules (excluded directories are not read). */
  async readTree(rel: string, origin: string, span?: SourceSpan, exclude?: IgnoreRules): Promise<TreeResource> {
    const { real, st } = await this.resolve(rel, span);
    if (LocalContext.kindOf(st, rel, span) !== 'directory') {
      throw new SitefileError('E_UNSUPPORTED_FILE', `'${display(rel)}' is a file, not a directory`, span);
    }
    const files: Array<[string, FileNode]> = [];
    await this.walk(real, rel, '', origin, new Set([real]), files, span, exclude);
    // With the instruction's span: two names that differ only in case (possible on a
    // case-sensitive file system) fail at the line that read the directory.
    const tree = Tree.empty.withFiles(files, 'error', span);
    return { kind: 'tree', tree, provenance: { uri: rel === '' ? './' : './' + rel, digest: tree.digest() } };
  }

  /**
   * @param realDir   real path of the directory being walked
   * @param ctxRel    logical context-relative path of that directory
   * @param treeRel   path of that directory inside the resulting tree ('' = root)
   * @param ancestors real paths of directories on the current walk path (cycle detection)
   */
  private async walk(
    realDir: string,
    ctxRel: string,
    treeRel: string,
    origin: string,
    ancestors: Set<string>,
    out: Array<[string, FileNode]>,
    span: SourceSpan | undefined,
    exclude?: IgnoreRules,
  ): Promise<void> {
    const entries = await fs.readdir(realDir, { withFileTypes: true });
    const names = entries.map((d) => d.name).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    const byName = new Map(entries.map((d) => [d.name, d]));
    // Tree paths are NFC, so two names that differ only in normalization (possible
    // on Linux, not on macOS) would be one path: refuse rather than pick one.
    const seen = new Set<string>();
    for (const raw of names) {
      const dirent = byName.get(raw)!;
      const name = nfc(raw);
      if (seen.has(name)) throw normalizationCollision(ctxRel === '' ? name : `${ctxRel}/${name}`, span);
      seen.add(name);
      const childCtx = ctxRel === '' ? name : `${ctxRel}/${name}`;
      const childTree = treeRel === '' ? name : `${treeRel}/${name}`;
      if (this.isIgnored(childCtx)) continue;

      let childReal = join(realDir, raw);
      let st: Stats;
      if (dirent.isSymbolicLink()) {
        const r = await this.resolve(childCtx, span, display(childCtx));
        childReal = r.real;
        st = r.st;
        // A link to an ignored location (e.g. .git) is skipped too.
        if (this.isIgnored(this.relOfReal(childReal))) continue;
      } else {
        st = await fs.lstat(childReal);
      }

      if (this.rules.ignores(childCtx, st.isDirectory())) continue;
      if (exclude?.ignores(childTree, st.isDirectory())) continue;

      if (st.isFile()) {
        this.checkHardlink(st, childCtx, span);
        const bytes = new Uint8Array(await fs.readFile(childReal));
        out.push(['/' + childTree, makeFile(bytes, origin, st.mode)]);
      } else if (st.isDirectory()) {
        if (ancestors.has(childReal)) continue; // directory symlink cycle
        ancestors.add(childReal);
        try {
          await this.walk(childReal, childCtx, childTree, origin, ancestors, out, span, exclude);
        } finally {
          ancestors.delete(childReal);
        }
      } else {
        throw new SitefileError(
          'E_UNSUPPORTED_FILE',
          `'${display(childCtx)}' is not a regular file or directory (sockets, fifos and devices are not supported)`,
          span,
        );
      }
    }
  }
}
