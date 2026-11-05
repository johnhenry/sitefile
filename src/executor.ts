import { readdir, readFile, realpath } from 'node:fs/promises';
import path from 'node:path';
import { SitefileError, type SourceSpan } from './errors.js';
import { sha256 } from './digest.js';
import type { ExportFormat, Program } from './language/ast.js';
import { parseProgram } from './language/validate.js';
import { joinVirtual, basenameVirtual } from './paths.js';
import { plan, type ImportSpec, type Operation, type Plan, type SourceRef, type Stage } from './planner.js';
import { decodeUtf8, parseRenderTemplate, renderTemplate, resolveJsonPath } from './render.js';
import { bytesToResource, verifyIntegrity } from './resources/convert.js';
import { fetchRemote } from './resources/https.js';
import { LocalContext } from './resources/local.js';
import { Lockfile } from './resources/lockfile.js';
import { DEFAULT_ARCHIVE_LIMITS, DEFAULT_FETCH_POLICY, type ArchiveLimits, type FetchPolicy, type Resource } from './resources/types.js';
import { makeFile, Tree, type FileNode } from './tree.js';
import type { Scalar } from './values.js';
import { exportDirectory, exportFile, isExporterTempName } from './exporter.js';
import { sourceDateEpoch } from './archive-writers.js';
import { encodeTree, isArchiveFormat } from './export-formats.js';

export interface BuildOptions {
  /** Build context directory; local paths resolve from here. */
  readonly contextDir: string;
  /** Sitefile path. Default: `<contextDir>/Sitefile`. */
  readonly sitefile?: string;
  readonly buildArgs?: Readonly<Record<string, string>>;
  /** Export destination; skipped when walking the context if it lies inside it. */
  readonly outDir?: string;
  readonly fetchPolicy?: Partial<FetchPolicy>;
  /** Limits for extracting archives imported AS tree. */
  readonly archiveLimits?: Partial<ArchiveLimits>;
  /** Lockfile path, or false to disable. Default: `Sitefile.lock` next to the Sitefile. */
  readonly lockfile?: string | false;
  /** Fail instead of adding or changing lockfile entries. */
  readonly frozenLockfile?: boolean;
  /** Re-record unpinned URLs whose content changed. */
  readonly updateLock?: boolean;
  /** Maximum imports resolved at once (default 4). */
  readonly concurrency?: number;
  /** Permit hard-linked files in the context (you assert they are safe). Default false. */
  readonly allowHardlinks?: boolean;
  readonly onWarning?: (message: string) => void;
}

export interface BuildResult {
  readonly plan: Plan;
  /** The final stage's whole tree. */
  readonly tree: Tree;
  /** The exported subtree. */
  readonly output: Tree;
  readonly resources: ReadonlyMap<string, Resource>;
  readonly warnings: readonly string[];
  readonly lockfileWritten: boolean;
}

interface Loaded {
  readonly program: Program;
  readonly sitefilePath: string;
  readonly label: string;
}

/** Read, parse and validate a Sitefile. */
export async function loadProgram(options: Pick<BuildOptions, 'contextDir' | 'sitefile'>): Promise<Loaded> {
  const sitefilePath = path.resolve(options.sitefile ?? path.join(options.contextDir, 'Sitefile'));
  let source: string;
  try {
    source = await readFile(sitefilePath, 'utf8');
  } catch (e) {
    throw new SitefileError('E_NOT_FOUND', `cannot read Sitefile '${sitefilePath}'`, undefined, { cause: e });
  }
  const label = path.relative(process.cwd(), sitefilePath) || path.basename(sitefilePath);
  return { program: parseProgram(source, label), sitefilePath, label };
}

/** Parse, validate and evaluate variables, without reading any sources. */
export async function planBuild(options: BuildOptions): Promise<Plan> {
  const { program } = await loadProgram(options);
  return plan(program, options.buildArgs ?? {});
}

/** Run a build in memory. */
export async function build(options: BuildOptions): Promise<BuildResult> {
  const { program, sitefilePath, label } = await loadProgram(options);
  const p = plan(program, options.buildArgs ?? {});
  const warnings: string[] = [];
  const warn = (m: string): void => {
    warnings.push(m);
    options.onWarning?.(m);
  };
  p.warnings.forEach(warn);

  const contextReal = await realpath(options.contextDir).catch((e: unknown) => {
    throw new SitefileError('E_NOT_FOUND', `build context '${options.contextDir}' does not exist`, undefined, { cause: e });
  });
  const ignore = ['.git'];
  if (options.outDir !== undefined) {
    const outReal = await realpathLoose(path.resolve(options.outDir));
    const rel = path.relative(contextReal, outReal);
    if (rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel)) {
      ignore.push(rel.split(path.sep).join('/'));
      // Also skip temp/old siblings an interrupted export may have left behind.
      const parent = path.dirname(outReal);
      const siblings = await readdir(parent).catch(() => [] as string[]);
      for (const name of siblings) {
        if (isExporterTempName(name, path.basename(outReal))) {
          ignore.push(path.relative(contextReal, path.join(parent, name)).split(path.sep).join('/'));
        }
      }
    }
  }
  const local = await LocalContext.open(contextReal, { ignore, allowHardlinks: options.allowHardlinks ?? false });
  const policy: FetchPolicy = { ...DEFAULT_FETCH_POLICY, ...options.fetchPolicy };
  const archiveLimits: ArchiveLimits = { ...DEFAULT_ARCHIVE_LIMITS, ...options.archiveLimits };
  const lockPath =
    options.lockfile === false ? undefined : (options.lockfile ?? path.join(path.dirname(sitefilePath), 'Sitefile.lock'));
  const lock = lockPath ? await Lockfile.load(lockPath) : undefined;

  const env: ImportEnv = { local, policy, archiveLimits, lock, label, warn, frozen: !!options.frozenLockfile, update: !!options.updateLock };
  const resolved = await mapLimit(p.imports, options.concurrency ?? 4, (spec) => resolveImport(spec, env));
  const resources = new Map<string, Resource>(p.imports.map((s, i) => [s.name, resolved[i]!]));

  const stageTrees = new Map<string, Tree>();
  const execEnv: ExecEnv = { local, resources, stages: stageTrees, vars: p.vars, label, warn };
  let tree = Tree.empty;
  for (const stage of p.stages) {
    tree = await startStage(stage, execEnv);
    for (const op of stage.operations) tree = await execute(op, tree, execEnv);
    if (stage.name !== undefined) stageTrees.set(stage.name, tree);
  }

  if (tree.isFile(p.exportPath)) {
    throw new SitefileError('E_EXPORT', `EXPORT path '${p.exportPath}' is a file; export a directory`);
  }
  const output = tree.subtree(p.exportPath);
  if (output.size === 0) warn(`EXPORT ${p.exportPath} contains no files`);

  const lockfileWritten = lock && !options.frozenLockfile ? await lock.save() : false;
  return { plan: p, tree, output, resources, warnings, lockfileWritten };
}

export interface ExportTarget {
  /** Overrides the Sitefile's EXPORT format. */
  readonly format?: ExportFormat;
  /** Overrides the Sitefile's EXPORT ... BASE (wbn). */
  readonly baseUrl?: string;
}

export interface ExportedBuild extends BuildResult {
  readonly format: ExportFormat;
  /** Real path written: a directory for `directory`, otherwise the archive file. */
  readonly outDir: string;
  /** Archive size in bytes (archive formats only). */
  readonly bytes?: number;
  /** `sha256:<hex>` of the archive file (archive formats only). */
  readonly fileDigest?: string;
}

/**
 * Build, then write the exported tree to `outDir`: a directory for the
 * `directory` format, a single file for zip, tar, tgz and wbn.
 */
export async function buildToDirectory(
  options: BuildOptions & ExportTarget & { readonly outDir: string; readonly force?: boolean },
): Promise<ExportedBuild> {
  const result = await build(options);
  const format = options.format ?? result.plan.exportFormat;
  const exportOptions = { contextDir: options.contextDir, ...(options.force ? { force: true } : {}) };
  if (!isArchiveFormat(format)) {
    const written = await exportDirectory(result.output, options.outDir, exportOptions);
    return { ...result, format, outDir: written.outDir };
  }
  const data = encodeTree(result.output, format, {
    epoch: sourceDateEpoch(),
    baseUrl: options.baseUrl ?? result.plan.exportBase,
  });
  const written = await exportFile(data, options.outDir, exportOptions);
  return { ...result, format, outDir: written.outFile, bytes: written.bytes, fileDigest: sha256(data) };
}

// ---------------------------------------------------------------- imports

interface ImportEnv {
  readonly local: LocalContext;
  readonly policy: FetchPolicy;
  readonly archiveLimits: ArchiveLimits;
  readonly lock: Lockfile | undefined;
  readonly label: string;
  readonly warn: (m: string) => void;
  readonly frozen: boolean;
  readonly update: boolean;
}

async function resolveImport(spec: ImportSpec, env: ImportEnv): Promise<Resource> {
  const origin = `${env.label}:${spec.span.line} IMPORT ${spec.name}`;
  if (spec.location.kind === 'local') {
    const rel = spec.location.path;
    const kind = await env.local.stat(rel, spec.span);
    if (kind === 'directory') {
      if (spec.as && spec.as !== 'tree') {
        throw new SitefileError('E_TYPE', `IMPORT ${spec.name}: '${spec.uri}' is a directory and cannot be imported AS ${spec.as}`, spec.span);
      }
      if (spec.integrity) throw new SitefileError('E_VALIDATION', `IMPORT ${spec.name}: INTEGRITY applies to files, not directories`, spec.span);
      return env.local.readTree(rel, origin, spec.span);
    }
    const file = await env.local.readFile(rel, spec.span);
    if (spec.integrity) verifyIntegrity(file.bytes, spec.integrity, spec.uri, spec.span);
    if (!spec.as || spec.as === 'file') return file;
    return bytesToResource(file.bytes, spec.as, { uri: spec.uri, basename: file.basename, mode: file.mode }, spec.span, env.archiveLimits);
  }

  const url = spec.location.url;
  const as = spec.as ?? 'file';
  const entry = env.lock?.get(url);
  if (env.frozen && !entry) {
    throw new SitefileError('E_LOCK_MISSING', `IMPORT ${spec.name}: '${url}' is not in the lockfile (--frozen-lockfile)`, spec.span);
  }
  const { bytes, finalUrl } = await fetchRemote(url, env.policy, spec.span);
  const digest = sha256(bytes);
  if (spec.integrity) {
    verifyIntegrity(bytes, spec.integrity, url, spec.span);
  } else if (entry && !env.update && entry.integrity !== digest) {
    throw new SitefileError(
      'E_LOCK_MISMATCH',
      `IMPORT ${spec.name}: '${url}' changed since it was locked (locked ${entry.integrity}, got ${digest}); rerun with --update-lock to accept it`,
      spec.span,
    );
  } else if (!entry) {
    env.warn(`${env.label}:${spec.span.line}: IMPORT ${spec.name} is not pinned; add INTEGRITY ${digest} for a reproducible build`);
  }
  if (env.frozen && entry && entry.integrity !== digest) {
    throw new SitefileError('E_LOCK_MISMATCH', `IMPORT ${spec.name}: '${url}' does not match the lockfile (--frozen-lockfile)`, spec.span);
  }
  env.lock?.record(url, { integrity: digest, size: bytes.byteLength, resolved: finalUrl });
  const basename = path.posix.basename(new URL(finalUrl).pathname) || spec.name;
  return bytesToResource(bytes, as, { uri: url, resolvedUrl: finalUrl, basename }, spec.span, env.archiveLimits);
}

// ---------------------------------------------------------------- operations

interface ExecEnv {
  readonly local: LocalContext;
  readonly resources: ReadonlyMap<string, Resource>;
  /** Final trees of the stages finished so far, by name. */
  readonly stages: ReadonlyMap<string, Tree>;
  readonly vars: ReadonlyMap<string, Scalar>;
  readonly label: string;
  readonly warn: (m: string) => void;
}

/** The tree a stage starts from. */
async function startStage(stage: Stage, env: ExecEnv): Promise<Tree> {
  const base = stage.base;
  switch (base.kind) {
    case 'scratch':
      return Tree.empty;
    case 'stage':
      return env.stages.get(base.name) ?? fail(base.name, stage.span);
    case 'local': {
      const display = base.path === '' ? './' : './' + base.path;
      if ((await env.local.stat(base.path, stage.span)) !== 'directory') {
        throw new SitefileError('E_TYPE', `FROM '${display}' is a file; FROM needs a directory`, stage.span);
      }
      return (await env.local.readTree(base.path, `${env.label}:${stage.span.line} FROM`, stage.span)).tree;
    }
  }
}

function fail(name: string, span: SourceSpan): never {
  throw new SitefileError('E_UNDEFINED', `unknown stage '${name}'`, span);
}

type Source =
  | {
      readonly kind: 'file';
      readonly bytes: Uint8Array;
      readonly mode: number;
      readonly basename: string;
      readonly label: string;
      /** Set for heredoc bodies: where the content sits in the Sitefile, for template error positions. */
      readonly inline?: { readonly file: string; readonly firstLine: number };
    }
  | { readonly kind: 'tree'; readonly tree: Tree; readonly label: string };

async function execute(op: Operation, tree: Tree, env: ExecEnv): Promise<Tree> {
  const origin = `${env.label}:${op.span.line} ${op.kind}`;
  const policy = op.overwrite ? 'overwrite' : 'error';

  if (op.kind === 'COPY') {
    const src = await readSource(op.source, origin, env);
    if (src.kind === 'file') {
      const intoDir = op.destIsDir || tree.isDirectory(op.dest);
      if (intoDir && src.inline) {
        throw new SitefileError('E_PATH', `'${op.dest}' is a directory; an inline ${src.label} body needs a file destination`, op.span);
      }
      const dest = intoDir ? joinVirtual(op.dest, src.basename) : op.dest;
      return tree.withFiles([[dest, makeFile(src.bytes, origin, src.mode)]], policy, op.span);
    }
    if (src.tree.size === 0) env.warn(`${origin}: '${src.label}' contains no files`);
    const files = src.tree.mountedAt(op.dest).map(([p, n]): [string, FileNode] => [p, { ...n, origin }]);
    return tree.withFiles(files, policy, op.span);
  }

  const src = await readSource(op.template, origin, env);
  if (src.kind !== 'file') {
    throw new SitefileError('E_TYPE', `RENDER template '${src.label}' is a directory, not a file`, op.template.span);
  }
  const text = decodeUtf8(src.bytes, `template '${src.label}'`, op.template.span);
  const parts = src.inline
    ? parseRenderTemplate(text, src.inline.file, src.inline.firstLine)
    : parseRenderTemplate(text, src.label);
  const out = renderTemplate(parts, (name, p, span) => lookup(name, p, span, env), op.escape);
  return tree.withFiles([[op.dest, makeFile(new TextEncoder().encode(out), origin)]], policy, op.span);
}

async function readSource(ref: SourceRef, origin: string, env: ExecEnv): Promise<Source> {
  if (ref.kind === 'inline') {
    const bytes = new TextEncoder().encode(ref.text);
    const inline = { file: env.label, firstLine: ref.bodyLine };
    return { kind: 'file', bytes, mode: 0o644, basename: '', label: `<<${ref.delimiter}`, inline };
  }
  if (ref.kind === 'local') {
    const label = './' + ref.path;
    if ((await env.local.stat(ref.path, ref.span)) === 'directory') {
      return { kind: 'tree', tree: (await env.local.readTree(ref.path, origin, ref.span)).tree, label };
    }
    const f = await env.local.readFile(ref.path, ref.span);
    return { kind: 'file', bytes: f.bytes, mode: f.mode, basename: f.basename, label };
  }

  if (ref.kind === 'stage') {
    const tree = env.stages.get(ref.name) ?? fail(ref.name, ref.span);
    const label = `--from=${ref.name} ${ref.path}`;
    const node = tree.get(ref.path);
    if (node) return { kind: 'file', bytes: node.bytes, mode: node.mode, basename: basenameVirtual(ref.path), label };
    if (tree.isDirectory(ref.path)) return { kind: 'tree', tree: tree.subtree(ref.path), label };
    throw new SitefileError('E_NOT_FOUND', `'${ref.path}' does not exist in stage '${ref.name}'`, ref.span);
  }

  const label = ref.path === '/' ? `${ref.name}:` : `${ref.name}:${ref.path}`;
  const res = env.resources.get(ref.name);
  if (!res) throw new SitefileError('E_UNDEFINED', `unknown import '${ref.name}'`, ref.span);
  switch (res.kind) {
    case 'value':
      throw new SitefileError(
        'E_TYPE',
        `import '${ref.name}' is a ${res.format} value: RENDER templates can read it, but it cannot be copied (import it AS file to copy it)`,
        ref.span,
      );
    case 'file':
      if (ref.path !== '/') throw new SitefileError('E_PATH', `import '${ref.name}' is a single file; use '${ref.name}:'`, ref.span);
      return { kind: 'file', bytes: res.bytes, mode: res.mode, basename: res.basename, label };
    case 'tree': {
      const node = res.tree.get(ref.path);
      if (node) return { kind: 'file', bytes: node.bytes, mode: node.mode, basename: basenameVirtual(ref.path), label };
      if (res.tree.isDirectory(ref.path)) return { kind: 'tree', tree: res.tree.subtree(ref.path), label };
      throw new SitefileError('E_NOT_FOUND', `'${ref.path}' does not exist in import '${ref.name}'`, ref.span);
    }
  }
}

function lookup(name: string, p: readonly string[], span: SourceSpan, env: ExecEnv): Scalar {
  const v = env.vars.get(name);
  if (v !== undefined) {
    if (p.length) throw new SitefileError('E_TYPE', `'${name}' is an ARG/LET value; dotted access only works on JSON imports`, span);
    return v;
  }
  const res = env.resources.get(name);
  if (!res) throw new SitefileError('E_UNDEFINED', `'${name}' is not an ARG, LET or IMPORT`, span);
  if (res.kind !== 'value') {
    throw new SitefileError('E_TYPE', `import '${name}' is a ${res.kind}; templates can only read text and json imports`, span);
  }
  if (res.format === 'json') return resolveJsonPath(res.value, p, name, span);
  if (p.length) throw new SitefileError('E_TYPE', `import '${name}' is text; dotted access only works on JSON imports`, span);
  return res.value as string;
}

// ---------------------------------------------------------------- helpers

async function mapLimit<T, R>(items: readonly T[], limit: number, fn: (item: T) => Promise<R>): Promise<R[]> {
  const out = new Array<R>(items.length);
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i]!);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, worker));
  return out;
}

/** realpath of the nearest existing ancestor, with the missing tail re-joined. */
async function realpathLoose(p: string): Promise<string> {
  const tail: string[] = [];
  let cur = p;
  for (;;) {
    try {
      return path.join(await realpath(cur), ...tail.reverse());
    } catch {
      const parent = path.dirname(cur);
      if (parent === cur) return p;
      tail.push(path.basename(cur));
      cur = parent;
    }
  }
}
