import { readdir, readFile, realpath } from 'node:fs/promises';
import path from 'node:path';
import { SitefileError, type SourceSpan } from './errors.js';
import { sha256 } from './digest.js';
import { FEED_ITEM, type ExportFormat, type OciCompression, type Program } from './language/ast.js';
import { parseProgram } from './language/validate.js';
import { joinVirtual, basenameVirtual, normalizeVirtual } from './paths.js';
import { plan, targetPlan, type ImportSpec, type Operation, type Plan, type PluginSpec, type SourceRef, type Stage } from './planner.js';
import { checkWasmPlugin } from './plugins/wasm.js';
import { DEFAULT_PLUGIN_POLICY, PluginHost, type PluginPolicy, type PluginRequest, type PluginResult } from './plugins/host.js';
import { cacheKey, digestFolder, TransformCache, type CacheStats, type RemoteCache } from './plugins/cache.js';
import { DEFAULT_TOOL_POLICY, resolveTool, runTool, type ResolvedTool, type ToolPolicy } from './plugins/exec.js';
import type { Operand } from './language/ast.js';
import { decodeUtf8, parseRenderTemplate, renderTemplate, resolveJsonPath, walk } from './render.js';
import { dataCollection, markdownCollection, sortDocuments, type MarkdownDocument } from './resources/documents.js';
import { bytesToResource, verifyIntegrity } from './resources/convert.js';
import { fetchRemote } from './resources/https.js';
import { LocalContext } from './resources/local.js';
import { Lockfile } from './resources/lockfile.js';
import { findVendorDir, Vendor } from './resources/vendor.js';
import { DEFAULT_ARCHIVE_LIMITS, DEFAULT_FETCH_POLICY, type ArchiveLimits, type FetchPolicy, type Resource } from './resources/types.js';
import { makeFile, Tree, type FileNode } from './tree.js';
import type { Scalar } from './values.js';
import { exportDirectory, exportFile, isExporterTempName } from './exporter.js';
import { sourceDateEpoch } from './archive-writers.js';
import { encodeTree, isArchiveFormat } from './export-formats.js';
import { buildFacts, makeAttestations, type AttestRequest, type Referrer } from './attest.js';
import { ANN, buildOciImage, imageMeta, layoutTree, ociArchive, parseConfig, type BaseImage, type OciImage } from './oci.js';
import { baseRef as parseBaseRef, readLayerBlobs, resolveBase } from './base-image.js';
import type { Credentials } from './registry-auth.js';
import type { RegistryOptions } from './registry.js';
import { Store } from './store.js';
import { begin, type Profiler } from './profile.js';
import { BuildMemo, memoKey, valueKey, type DocumentMemo, type MemoStats } from './memo.js';
import type { TemplateNode } from './render.js';
import { atomXml, headersFile, pageUrl, parseFeedDate, redirectsFile, sitemapXml, type FeedEntry } from './site.js';

export interface BuildOptions {
  /** Build context directory; local paths resolve from here. */
  readonly contextDir: string;
  /** Sitefile path. Default: `<contextDir>/Sitefile`. */
  readonly sitefile?: string;
  readonly buildArgs?: Readonly<Record<string, string>>;
  /** `--target <stage>`: build up to this named stage and export its whole tree (planner `targetPlan`). */
  readonly target?: string;
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
  /**
   * Context-relative vendor directory written by `sitefile vendor` (default `vendor/sitefile`, used when
   * its manifest.json exists; a directory given here must exist). Remote IMPORTs and BASE blobs found
   * there are read from it instead of the network, verified as usual. It is never part of a directory copy.
   */
  readonly vendorDir?: string;
  /**
   * `--strict-pins`: fail (E_UNPINNED) before fetching anything when a remote IMPORT has neither
   * INTEGRITY nor a lockfile entry, or the BASE image is neither `@sha256:`-pinned nor locked.
   */
  readonly strictPins?: boolean;
  /** Maximum imports resolved at once (default 4). */
  readonly concurrency?: number;
  /** Plugin process limits (per-call timeout, maximum result size). */
  readonly pluginPolicy?: Partial<PluginPolicy>;
  /** Permit hard-linked files in the context (you assert they are safe). Default false. */
  readonly allowHardlinks?: boolean;
  /**
   * External programs TOOL may use: `name` (looked up on PATH) or `name=/absolute/path`,
   * like the CLI's `--allow-exec`. A TOOL not listed here fails the build.
   */
  readonly allowExec?: readonly string[];
  /**
   * JavaScript PLUGINs allowed to run, by declared name, like the CLI's `--allow-plugin`.
   * A JS PLUGIN not listed here (and without `allowAllPlugins`) fails the build.
   * WebAssembly plugins need no permission.
   */
  readonly allowPlugin?: readonly string[];
  /** Allow every JavaScript PLUGIN, like the CLI's `--allow-plugins`. */
  readonly allowAllPlugins?: boolean;
  /** External tool limits (per-call timeout, maximum output size). */
  readonly toolPolicy?: Partial<ToolPolicy>;
  /** Directory of the TRANSFORM result cache. Omit to disable caching (the CLI sets `$SITEFILE_HOME/cache/transform`). */
  readonly cacheDir?: string;
  /** Consulted on a local cache miss and told about every entry used (`--cache-from` / `--cache-to`). Needs `cacheDir`. */
  readonly remoteCache?: RemoteCache;
  readonly onWarning?: (message: string) => void;
  /** oci: `os/arch` of the image (and of the BASE to pull); default linux/amd64. */
  readonly platform?: string;
  /** `--base`: build the oci image on this image, overriding the Sitefile's `AS oci BASE`. */
  readonly baseImage?: string;
  /** Do not resolve a BASE (the output will not be an oci image). */
  readonly skipBase?: boolean;
  /** Registry transport/auth options for pulling the BASE. */
  readonly registry?: RegistryOptions & { readonly explicitCredentials?: Credentials };
  /** The image store used for BASE pulls (default: $SITEFILE_HOME). */
  readonly store?: Store;
  /** Informational lines (e.g. the path each allowed tool resolved to). */
  readonly onNotice?: (message: string) => void;
  /** `--profile`: record a timing trace of the build here (src/profile.ts). */
  readonly profiler?: Profiler;
  /** Other paths this command writes (e.g. the --profile file): skipped when walking the context, like `outDir`. */
  readonly alsoWrites?: readonly string[];
  /**
   * Incremental rebuilds: reuse the results of steps whose inputs did not change since an
   * earlier build with the same memo (watch mode keeps one per session; src/memo.ts).
   */
  readonly memo?: BuildMemo;
  /** Keep every intermediate tree in {@link BuildResult.steps} (for `sitefile why`). */
  readonly recordSteps?: boolean;
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
  /** The resolved BASE image (EXPORT ... AS oci BASE <image> or --base), whose blobs are in the store. */
  readonly base?: BaseImage;
  /** Allow-listed tools this build resolved. */
  readonly tools: readonly ResolvedTool[];
  /** What the TRANSFORMs did: processes started, calls made, cache activity. */
  readonly transformStats: TransformStats;
  /** With `memo`: how many steps were reused and how many ran. */
  readonly memoStats?: MemoStats;
  /**
   * With `recordSteps`: per stage of `plan.stages`, the tree it starts from, then the tree after each of its
   * operations (`steps[s][i + 1]` follows `operations[i]`). `tree` is the last one plus the EXPORT site files.
   */
  readonly steps?: readonly (readonly Tree[])[];
  /** `--strict-pins`: how each remote input was pinned (recorded in the provenance). */
  readonly pins?: Pins;
}

export interface TransformStats extends CacheStats {
  /** Plugin processes started (0 when every result came from the cache). */
  pluginsSpawned: number;
  /** transform() calls made to plugin processes. */
  pluginCalls: number;
  /** External tool invocations. */
  toolRuns: number;
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
  return planFor(program, options);
}

/** The plan of a program for these options (`target` applied). */
function planFor(program: Program, options: Pick<BuildOptions, 'buildArgs' | 'target'>): Plan {
  const p = plan(program, options.buildArgs ?? {});
  return options.target === undefined ? p : targetPlan(p, options.target);
}

/** The lockfile a build would use: `Sitefile.lock` next to the Sitefile, unless disabled. */
function lockPathFor(options: Pick<BuildOptions, 'lockfile'>, sitefilePath: string): string | undefined {
  return options.lockfile === false ? undefined : (options.lockfile ?? path.join(path.dirname(sitefilePath), 'Sitefile.lock'));
}

/** `validate --strict-pins`: check the pins of `p` against the lockfile without building or fetching. */
export async function checkPlanPins(options: BuildOptions, p: Plan): Promise<void> {
  const sitefilePath = path.resolve(options.sitefile ?? path.join(options.contextDir, 'Sitefile'));
  const lockPath = lockPathFor(options, sitefilePath);
  const lock = lockPath ? await Lockfile.load(lockPath) : undefined;
  void checkPins(p, lock, options.skipBase ? undefined : (options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined)));
}

/** Run a build in memory. */
export async function build(options: BuildOptions): Promise<BuildResult> {
  const memo = options.memo;
  if (!memo) return buildOnce(options);
  memo.begin();
  let ok = false;
  try {
    const r = await buildOnce(options);
    ok = true;
    return { ...r, memoStats: { ...memo.stats } };
  } finally {
    memo.finish(ok);
  }
}

async function buildOnce(options: BuildOptions): Promise<BuildResult> {
  const prof = options.profiler;
  const parseSpan = begin(prof, 'parse', 'build');
  const { program, sitefilePath, label } = await loadProgram(options);
  const p = planFor(program, options);
  parseSpan.end({ imports: p.imports.length, stages: p.stages.length, ...(p.target !== undefined ? { target: p.target } : {}) });
  const warnings: string[] = [];
  const warn = (m: string): void => {
    warnings.push(m);
    options.onWarning?.(m);
  };
  p.warnings.forEach(warn);
  const tools = await resolveTools(p, options, warn);
  const pluginAllowed = checkPlugins(p, options, warn);

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
  const vendorDir = await findVendorDir(contextReal, options.vendorDir);
  if (vendorDir !== undefined) ignore.push(vendorDir);
  for (const w of options.alsoWrites ?? []) {
    const rel = path.relative(contextReal, await realpathLoose(path.resolve(w)));
    if (rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel)) ignore.push(rel.split(path.sep).join('/'));
  }
  const local = await LocalContext.open(contextReal, { ignore, allowHardlinks: options.allowHardlinks ?? false });
  const vendor = vendorDir !== undefined ? await Vendor.open(local, vendorDir) : undefined;
  const policy: FetchPolicy = { ...DEFAULT_FETCH_POLICY, ...options.fetchPolicy };
  const archiveLimits: ArchiveLimits = { ...DEFAULT_ARCHIVE_LIMITS, ...options.archiveLimits };
  const lockPath = lockPathFor(options, sitefilePath);
  const lock = lockPath ? await Lockfile.load(lockPath) : undefined;

  // BASE: the image to build on (resolved after the files, below).
  const baseRef = options.skipBase ? undefined : (options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined));
  const pins = options.strictPins ? checkPins(p, lock, baseRef) : undefined;

  const memo = options.memo;
  const env: ImportEnv = {
    local,
    policy,
    archiveLimits,
    lock,
    label,
    warn,
    frozen: !!options.frozenLockfile,
    update: !!options.updateLock,
    vendor,
    ...(memo ? { memo } : {}),
  };
  // Imports resolve concurrently: each gets its own trace thread, reserved in Sitefile order.
  const importSpans = p.imports.map((spec, i) => {
    prof?.nameThread(i + 2, `import ${spec.name}`);
    return prof ? prof.reserve(`import ${spec.name}`, 'import', { uri: spec.uri, as: spec.as ?? 'file' }, i + 2) : undefined;
  });
  const resolved = await mapLimit(p.imports, options.concurrency ?? 4, async (spec, i) => {
    importSpans[i]?.start();
    const r = await resolveImport(spec, env);
    importSpans[i]?.end({ kind: r.res.kind });
    return r;
  });
  const resources = new Map<string, Resource>(p.imports.map((s, i) => [s.name, sorted(s, resolved[i]!.res)]));
  // Incremental rebuilds: what each import is (its spec and the digest of what it read), and the build's ARG/LET values.
  const resourceKeys = new Map<string, string>(memo ? p.imports.map((s, i) => [s.name, resolved[i]!.key]) : []);
  const envKey = memo ? memoKey('env', label, valueKey([...p.vars])) : '';

  const stageTrees = new Map<string, Tree>();
  const plugins = new PluginHost({ ...DEFAULT_PLUGIN_POLICY, ...options.pluginPolicy }, warn);
  const cache = options.cacheDir !== undefined ? new TransformCache(options.cacheDir, options.remoteCache) : undefined;
  const stats: TransformStats = { hits: 0, misses: 0, writes: 0, corrupt: 0, pluginsSpawned: 0, pluginCalls: 0, toolRuns: 0 };
  const execEnv: ExecEnv = {
    local,
    resources,
    stages: stageTrees,
    vars: p.vars,
    label,
    warn,
    plugins,
    tools: new Map(tools.map((t) => [t.name, t])),
    toolPolicy: { ...DEFAULT_TOOL_POLICY, ...options.toolPolicy },
    pluginDigests: new Map(),
    stats,
    ...(cache ? { cache } : {}),
    ...(prof ? { profiler: prof } : {}),
    ...(memo ? { memo } : {}),
    resourceKeys,
    envKey,
    stageKeys: new Map(),
  };
  let tree = Tree.empty;
  const steps: Tree[][] | undefined = options.recordSteps ? [] : undefined;
  try {
    for (const spec of p.plugins) {
      const s = begin(prof, `PLUGIN ${spec.name}`, 'plugin', { path: './' + spec.path });
      await registerPlugin(spec, local, plugins, pluginAllowed);
      s.end();
    }
    for (const [i, stage] of p.stages.entries()) {
      const s = begin(prof, `stage ${stage.name ?? '#' + (i + 1)}`, 'stage', { from: stageFrom(stage) });
      const start = await startStage(stage, execEnv);
      tree = start.tree;
      let key = start.key;
      const record = steps ? [tree] : undefined;
      steps?.push(record!);
      for (const op of stage.operations) {
        const o = begin(prof, `${label}:${op.span.line} ${op.kind}`, 'operation', { line: op.span.line });
        const r = await executeOp(op, tree, key, execEnv);
        tree = r.tree;
        key = r.key;
        record?.push(tree);
        o.end({ files: tree.size, ...(r.memo ? { memo: r.memo } : {}) });
      }
      if (stage.name !== undefined) {
        stageTrees.set(stage.name, tree);
        execEnv.stageKeys.set(stage.name, key);
      }
      s.end({ files: tree.size });
    }
  } finally {
    plugins.close();
    stats.pluginsSpawned = plugins.stats.spawned;
    stats.pluginCalls = plugins.stats.calls;
    if (cache) Object.assign(stats, cache.stats);
  }

  if (tree.isFile(p.exportPath)) {
    throw new SitefileError('E_EXPORT', `EXPORT path '${p.exportPath}' is a file; export a directory`);
  }
  tree = withSiteRules(tree, p, label);
  const output = tree.subtree(p.exportPath);
  if (output.size === 0) warn(p.target === undefined ? `EXPORT ${p.exportPath} contains no files` : `--target ${p.target}: stage '${p.target}' contains no files`);

  // BASE: the image to build on. Resolved after the files so a failing build never pulls anything; its
  // lockfile entry is saved together with the imports'.
  const baseSpan = baseRef ? begin(prof, `BASE ${baseRef}`, 'import') : undefined;
  const base = baseRef
    ? await resolveBase(baseRef, {
        store: options.store ?? (await Store.open()),
        platform: options.platform,
        lock,
        frozen: !!options.frozenLockfile,
        update: !!options.updateLock,
        vendor,
        registry: {
          ...options.registry,
          allowPrivateNetwork: options.registry?.allowPrivateNetwork ?? policy.allowPrivateNetwork,
        },
        warn,
      })
    : undefined;
  baseSpan?.end();

  const lockfileWritten = lock && !options.frozenLockfile ? await lock.save() : false;
  return { plan: p, tree, output, resources, warnings, lockfileWritten, tools, transformStats: stats, ...(base ? { base } : {}), ...(steps ? { steps } : {}), ...(pins ? { pins } : {}) };
}

/** How the remote inputs of a `--strict-pins` build are pinned: remote IMPORTs by name, and the BASE. */
export interface Pins {
  readonly imports: Readonly<Record<string, 'integrity' | 'lockfile'>>;
  readonly base?: 'digest' | 'lockfile';
}

/**
 * `--strict-pins`: every remote input must be pinned, so the build is reproducible. A remote IMPORT
 * needs INTEGRITY or a `Sitefile.lock` entry; a BASE image needs `@sha256:` or a lockfile `images`
 * entry. `lock` is undefined with --no-lockfile (only the Sitefile's own pins count then). Checked in
 * source order, before anything is fetched; the first unpinned input fails with E_UNPINNED.
 */
export function checkPins(p: Plan, lock: Lockfile | undefined, baseRef: string | undefined): Pins {
  const imports: Record<string, 'integrity' | 'lockfile'> = {};
  for (const spec of p.imports) {
    if (spec.location.kind !== 'remote') continue;
    if (spec.integrity) imports[spec.name] = 'integrity';
    else if (lock?.get(spec.location.url)) imports[spec.name] = 'lockfile';
    if (imports[spec.name]) continue;
    throw new SitefileError(
      'E_UNPINNED',
      `IMPORT ${spec.name}: '${spec.location.url}' is not pinned (--strict-pins); add INTEGRITY sha256:<hex>, or record it in Sitefile.lock with a build without --strict-pins`,
      spec.span,
    );
  }
  if (baseRef === undefined) return { imports };
  const { ref, lockKey } = parseBaseRef(baseRef);
  if (ref.digest !== undefined) return { imports, base: 'digest' };
  if (lock?.getImage(lockKey)) return { imports, base: 'lockfile' };
  throw new SitefileError(
    'E_UNPINNED',
    `BASE ${baseRef}: '${lockKey}' is not pinned (--strict-pins); pin it with @sha256:..., or record it in Sitefile.lock with a build without --strict-pins`,
  );
}

export interface ExportTarget {
  /** Overrides the Sitefile's EXPORT format. */
  readonly format?: ExportFormat;
  /** Overrides the Sitefile's EXPORT ... BASE (wbn). */
  readonly baseUrl?: string;
  /** oci: root directory inside the layer (overrides EXPORT ... AT; default /srv/www). */
  readonly ociRoot?: string;
  /** oci: site layer encoding (overrides EXPORT ... COMPRESSION; default gzip). */
  readonly ociCompression?: OciCompression;
  /** Extra OCI config labels. */
  readonly imageLabels?: Readonly<Record<string, string>>;
  /** Add org.opencontainers.image.source/revision from the git checkout (opt-in). */
  readonly gitLabels?: boolean;
  /** oci: `name:tag` recorded as org.opencontainers.image.ref.name in index.json. */
  readonly ociRefName?: string;
  /** oci: attach a provenance and/or SBOM attestation (`--provenance`, `--sbom`). */
  readonly attest?: AttestRequest;
}

export interface ExportedBuild extends BuildResult {
  readonly format: ExportFormat;
  /** Real path written: a directory for `directory`, otherwise the archive file. */
  readonly outDir: string;
  /** Archive size in bytes (archive formats only). */
  readonly bytes?: number;
  /** `sha256:<hex>` of the archive file (archive formats only). */
  readonly fileDigest?: string;
  /** oci: the image's manifest digest (its image ID). */
  readonly manifestDigest?: string;
  /** Watch mode: set when the directory export applied only the changes (src/exporter.ts). */
  readonly incremental?: { readonly written: number; readonly removed: number };
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
  const span = begin(options.profiler, 'export', 'export', { format });
  try {
    return await exportBuild(result, format, options);
  } finally {
    span.end();
  }
}

async function exportBuild(
  result: BuildResult,
  format: ExportFormat,
  options: BuildOptions & ExportTarget & { readonly outDir: string; readonly force?: boolean },
): Promise<ExportedBuild> {
  const exportOptions = { contextDir: options.contextDir, ...(options.force ? { force: true } : {}) };
  // Watch mode: a directory export writes only what changed since the session's last one.
  const exportState = options.memo?.exportState;
  if (format !== 'directory') exportState?.clear();
  if (format === 'oci') {
    const img = buildOciImage(result.output, {
      root: options.ociRoot ?? result.plan.exportRoot,
      compression: options.ociCompression ?? result.plan.exportCompression,
      platform: options.platform,
      epoch: sourceDateEpoch(),
      base: result.base,
      ...(await imageMeta(options.contextDir, { git: !!options.gitLabels, ...(options.imageLabels ? { labels: options.imageLabels } : {}) })),
    });
    // An image built on a BASE carries the base's layers: copy them in so the export is self-contained.
    const baseBlobs = result.base ? await readLayerBlobs(options.store ?? (await Store.open()), img.baseLayers) : undefined;
    const referrers = options.attest ? await imageAttestations(result, img, options, options.attest) : [];
    // A .tar destination gets an OCI archive; anything else a layout directory.
    if (options.outDir.toLowerCase().endsWith('.tar')) {
      const data = ociArchive(img, options.ociRefName, sourceDateEpoch(), baseBlobs, referrers);
      const written = await exportFile(data, options.outDir, exportOptions);
      return { ...result, format, outDir: written.outFile, bytes: written.bytes, fileDigest: sha256(data), manifestDigest: img.manifestDigest };
    }
    const written = await exportDirectory(layoutTree(img, options.ociRefName, baseBlobs, referrers), options.outDir, exportOptions);
    return { ...result, format, outDir: written.outDir, manifestDigest: img.manifestDigest };
  }
  if (!isArchiveFormat(format)) {
    const written = await exportDirectory(result.output, options.outDir, { ...exportOptions, ...(exportState ? { incremental: exportState } : {}) });
    return { ...result, format, outDir: written.outDir, ...(written.incremental ? { incremental: written.incremental } : {}) };
  }
  const data = encodeTree(result.output, format, {
    epoch: sourceDateEpoch(),
    baseUrl: options.baseUrl ?? result.plan.exportBase,
  });
  const written = await exportFile(data, options.outDir, exportOptions);
  return { ...result, format, outDir: written.outFile, bytes: written.bytes, fileDigest: sha256(data) };
}

/**
 * The attestations (`--provenance`, `--sbom`) of an image built from `result`: in-toto statements
 * whose subject is the image manifest. They never change the image itself.
 */
export async function imageAttestations(
  result: BuildResult,
  img: Pick<OciImage, 'manifestDigest' | 'manifestBytes' | 'configBytes' | 'root'>,
  options: Pick<BuildOptions, 'contextDir' | 'sitefile' | 'buildArgs' | 'platform'>,
  request: AttestRequest,
): Promise<Referrer[]> {
  if (!request.provenance && !request.sbom) return [];
  const facts = await buildFacts(result, options);
  const title = parseConfig(img.configBytes).labels[ANN.title];
  return makeAttestations({ digest: img.manifestDigest, size: img.manifestBytes.byteLength, files: result.output, root: img.root, title }, facts, request, sourceDateEpoch());
}

// ---------------------------------------------------------------- imports

interface ImportEnv extends RemoteEnv {
  readonly local: LocalContext;
  readonly archiveLimits: ArchiveLimits;
  readonly memo?: BuildMemo;
}

/**
 * Read an import. `key` identifies the result for incremental rebuilds: the import's spec
 * (with its Sitefile position) and the digest of the bytes or tree it read. With a memo,
 * conversions (parsing Markdown, data and archives) whose key is unchanged are reused.
 */
async function resolveImport(spec: ImportSpec, env: ImportEnv): Promise<{ res: Resource; key: string }> {
  const origin = `${env.label}:${spec.span.line} IMPORT ${spec.name}`;
  const id = env.memo ? valueKey(spec) : '';
  const keyOf = (raw: string): string => (env.memo ? memoKey('import', env.label, id, raw) : '');
  // A collection's documents are memoized one by one, as children of the import's entry.
  const convert = <T extends Resource>(key: string, compute: (docs?: DocumentMemo) => T): T =>
    env.memo ? env.memo.memoize(key, (children) => compute(env.memo!.documents(children))) : compute();
  const convertBytes = async (key: string, compute: () => Promise<Resource>): Promise<Resource> => {
    const hit = env.memo?.get<Resource>(key);
    if (hit) return hit;
    const r = await compute();
    env.memo?.set(key, r);
    return r;
  };
  if (spec.location.kind === 'local') {
    const rel = spec.location.path;
    const kind = await env.local.stat(rel, spec.span);
    if (kind === 'directory') {
      if (spec.as === 'markdown') {
        if (spec.integrity) throw new SitefileError('E_VALIDATION', `IMPORT ${spec.name}: INTEGRITY applies to files, not directories`, spec.span);
        const tree = await env.local.readTree(rel, origin, spec.span);
        const key = keyOf(tree.provenance.digest);
        return {
          key,
          res: convert(key, (docs) => {
            const value = markdownCollection(tree.tree, spec.uri.replace(/\/+$/, ''), spec.span, docs);
            return { kind: 'value', format: 'markdown', value, provenance: tree.provenance };
          }),
        };
      }
      if (spec.as === 'json' || spec.as === 'yaml') {
        if (spec.integrity) throw new SitefileError('E_VALIDATION', `IMPORT ${spec.name}: INTEGRITY applies to files, not directories`, spec.span);
        const format = spec.as;
        const tree = await env.local.readTree(rel, origin, spec.span);
        const key = keyOf(tree.provenance.digest);
        return {
          key,
          res: convert(key, (docs) => {
            const value = dataCollection(tree.tree, format, spec.uri.replace(/\/+$/, ''), spec.span, docs);
            return { kind: 'value', format, value, provenance: tree.provenance };
          }),
        };
      }
      if (spec.as && spec.as !== 'tree') {
        throw new SitefileError('E_TYPE', `IMPORT ${spec.name}: '${spec.uri}' is a directory and cannot be imported AS ${spec.as}`, spec.span);
      }
      if (spec.integrity) throw new SitefileError('E_VALIDATION', `IMPORT ${spec.name}: INTEGRITY applies to files, not directories`, spec.span);
      const tree = await env.local.readTree(rel, origin, spec.span);
      return { key: keyOf(tree.provenance.digest), res: tree };
    }
    const file = await env.local.readFile(rel, spec.span);
    if (spec.integrity) verifyIntegrity(file.bytes, spec.integrity, spec.uri, spec.span);
    const key = keyOf(`${file.provenance.digest}\0${file.mode}\0${file.basename}`);
    if (!spec.as || spec.as === 'file') return { key, res: file };
    const as = spec.as;
    return { key, res: await convertBytes(key, () => bytesToResource(file.bytes, as, { uri: spec.uri, basename: file.basename, mode: file.mode }, spec.span, env.archiveLimits)) };
  }

  const url = spec.location.url;
  const as = spec.as ?? 'file';
  const { bytes, finalUrl } = await fetchRemoteImport(spec, url, env);
  const basename = path.posix.basename(new URL(finalUrl).pathname) || spec.name;
  const key = keyOf(`${sha256(bytes)}\0${finalUrl}`);
  return { key, res: await convertBytes(key, () => bytesToResource(bytes, as, { uri: url, resolvedUrl: finalUrl, basename }, spec.span, env.archiveLimits)) };
}

/** What fetching a remote IMPORT needs: the network policy and the lockfile rules (and, for builds, the vendor directory). */
export interface RemoteEnv {
  readonly policy: FetchPolicy;
  readonly lock: Lockfile | undefined;
  readonly label: string;
  readonly warn: (m: string) => void;
  readonly frozen: boolean;
  readonly update: boolean;
  /** Vendored copies (`sitefile vendor`), preferred over the network. */
  readonly vendor?: Vendor | undefined;
}

/**
 * The bytes of a remote IMPORT, checked against its INTEGRITY and the lockfile (and recorded there):
 * read from the vendor directory when it has the URL, otherwise fetched.
 */
export async function fetchRemoteImport(spec: ImportSpec, url: string, env: RemoteEnv): Promise<{ bytes: Uint8Array; finalUrl: string }> {
  const entry = env.lock?.get(url);
  if (env.frozen && !entry) {
    throw new SitefileError('E_LOCK_MISSING', `IMPORT ${spec.name}: '${url}' is not in the lockfile (--frozen-lockfile)`, spec.span);
  }
  const vendored = env.vendor?.resource(url);
  if (env.vendor && !vendored) {
    env.warn(`${env.label}:${spec.span.line}: IMPORT ${spec.name}: '${url}' is not vendored in ${env.vendor.dir}; fetching it (run 'sitefile vendor' to add it)`);
  }
  const { bytes, finalUrl } = vendored && env.vendor ? await env.vendor.readResource(url, vendored, spec.span) : await fetchRemote(url, env.policy, spec.span);
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
  return { bytes, finalUrl };
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
  readonly plugins: PluginHost;
  readonly tools: ReadonlyMap<string, ResolvedTool>;
  readonly toolPolicy: ToolPolicy;
  readonly cache?: TransformCache;
  /** Digest of each plugin's folder (cache key); undefined when the folder was too large to hash. */
  readonly pluginDigests: Map<string, string | undefined>;
  readonly stats: TransformStats;
  readonly profiler?: Profiler;
  /** Incremental rebuilds (with `BuildOptions.memo`): the memo and the keys of what operations read. */
  readonly memo?: BuildMemo;
  /** Import name -> its key (resolveImport). */
  readonly resourceKeys: ReadonlyMap<string, string>;
  /** The Sitefile label and its ARG/LET values. */
  readonly envKey: string;
  /** Stage name -> the key of its final tree. */
  readonly stageKeys: Map<string, string>;
  /** The current element of a RENDER … FOR loop. */
  readonly loop?: { readonly name: string; readonly value: unknown };
}

/** The tree a stage starts from, and its key. */
async function startStage(stage: Stage, env: ExecEnv): Promise<{ tree: Tree; key: string }> {
  const base = stage.base;
  switch (base.kind) {
    case 'scratch':
      return { tree: Tree.empty, key: env.memo ? memoKey('scratch', env.envKey) : '' };
    case 'stage':
      return { tree: env.stages.get(base.name) ?? fail(base.name, stage.span), key: env.stageKeys.get(base.name) ?? '' };
    case 'local': {
      const display = base.path === '' ? './' : './' + base.path;
      if ((await env.local.stat(base.path, stage.span)) !== 'directory') {
        throw new SitefileError('E_TYPE', `FROM '${display}' is a file; FROM needs a directory`, stage.span);
      }
      const t = await env.local.readTree(base.path, `${env.label}:${stage.span.line} FROM`, stage.span);
      return { tree: t.tree, key: env.memo ? memoKey('from', env.envKey, String(stage.span.line), base.path, t.provenance.digest) : '' };
    }
  }
}

/** What a stage starts from, for the trace. */
function stageFrom(stage: Stage): string {
  const b = stage.base;
  return b.kind === 'scratch' ? 'scratch' : b.kind === 'stage' ? b.name : b.path === '' ? './' : './' + b.path;
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
      /** Local files: the content digest (memo keys). */
      readonly digest?: string;
      /** Set for heredoc bodies: where the content sits in the Sitefile, for template error positions. */
      readonly inline?: { readonly file: string; readonly firstLine: number };
    }
  | { readonly kind: 'tree'; readonly tree: Tree; readonly label: string; readonly digest?: string };

type Files = Array<[string, FileNode]>;

/** An operation's files, before they are added to the tree. `key`: memoized under it (undefined: not memoizable). */
interface Produced {
  readonly files: Files;
  readonly key?: string;
  readonly hit?: boolean;
}

/** An ExecEnv whose warnings are also recorded (a memoized step replays them). */
type RecEnv = ExecEnv & { readonly warnings: string[]; readonly unrecorded: (m: string) => void };

function recording(env: ExecEnv): RecEnv {
  const warnings: string[] = [];
  return {
    ...env,
    warnings,
    unrecorded: env.warn,
    warn: (m) => {
      warnings.push(m);
      env.warn(m);
    },
  };
}

/**
 * Run `compute` unless `key` is memoized. A hit replays the warnings the step printed
 * after those this run already printed before the lookup (the same code ran up to here,
 * so they are a prefix). `store` says whether the result may be reused (a plugin can opt out).
 */
async function memoFiles(
  env: RecEnv,
  key: string | undefined,
  compute: (children: string[]) => Promise<Files>,
  store: () => boolean = () => true,
): Promise<Produced> {
  if (!env.memo || key === undefined) return { files: await compute([]) };
  const hit = env.memo.get<{ files: Files; warnings: readonly string[] }>(key);
  if (hit) {
    for (const w of hit.warnings.slice(env.warnings.length)) env.warn(w);
    return { files: hit.files, key, hit: true };
  }
  const children: string[] = [];
  const files = await compute(children);
  if (!store()) return { files };
  env.memo.set(key, { files, warnings: [...env.warnings] }, children);
  return { files, key };
}

/** What a source reference reads, for memo keys (the operation's own text covers inline bodies and paths). */
function sourceKey(ref: SourceRef, src: Source | undefined, env: ExecEnv): string {
  switch (ref.kind) {
    case 'inline':
      return 'inline';
    case 'local':
      return `local\0${src?.digest ?? ''}`;
    case 'stage':
      return `stage\0${env.stageKeys.get(ref.name) ?? ''}`;
    case 'import':
      return `import\0${env.resourceKeys.get(ref.name) ?? ''}`;
  }
}

/** Names an operand interpolates. */
function operandNames(ops: ReadonlyArray<Operand | undefined>, out = new Set<string>()): Set<string> {
  for (const op of ops) for (const part of op?.parts ?? []) if (part.kind !== 'text') out.add(part.name);
  return out;
}

/**
 * The keys of the imports an operation may read, by name (ARG/LET values are in its opId).
 * `except`: a loop variable, which hides an import of the same name.
 */
function namesKey(env: ExecEnv, names: Iterable<string>, except?: string): string {
  const list = [...new Set(names)].filter((n) => n !== except).sort();
  return memoKey('names', ...list.flatMap((n) => [n, env.resourceKeys.get(n) ?? '']));
}

/** Top-level names a template reads (variables, #each lists, #if conditions). */
function templateNames(nodes: readonly TemplateNode[], out = new Set<string>()): Set<string> {
  for (const n of nodes) {
    if (n.kind === 'var') out.add(n.name);
    else if (n.kind === 'each') {
      out.add(n.list.name);
      templateNames(n.body, out);
      templateNames(n.empty, out);
    } else if (n.kind === 'if') {
      out.add(n.cond.name);
      templateNames(n.then, out);
      templateNames(n.otherwise, out);
    }
  }
  return out;
}

/**
 * Run one operation: produce its files, then add them to the tree. With a memo, a step whose
 * key (the operation and everything it reads) is unchanged reuses its files, and one whose
 * input tree is also unchanged reuses the resulting tree. The key of the result chains the
 * input tree's key with the step's, like image layers.
 */
async function executeOp(op: Operation, tree: Tree, prevKey: string, env: ExecEnv): Promise<{ tree: Tree; key: string; memo?: 'hit' | 'files' | 'miss' }> {
  const policy = op.overwrite ? 'overwrite' : 'error';
  const memo = env.memo;
  if (!memo) return { tree: op.kind === 'SITEMAP' ? sitemap(op, tree, env) : tree.withFiles((await produce(op, tree, recording(env), '')).files, policy, op.span), key: '' };
  const opId = memoKey('op', env.envKey, valueKey(op));
  if (op.kind === 'SITEMAP') {
    // Reads the whole tree: only the chain key can say it is unchanged.
    const key = memoKey('chain', prevKey, opId);
    const hit = memo.get<Tree>(key);
    if (hit) return { tree: hit, key, memo: 'hit' };
    const out = sitemap(op, tree, env);
    memo.set(key, out);
    return { tree: out, key, memo: 'miss' };
  }
  const p = await produce(op, tree, recording(env), opId);
  // Not memoizable (e.g. a plugin that opted out of caching): chain on what it produced instead.
  const stepKey = p.key ?? memoKey('files', ...p.files.flatMap(([f, n]) => [f, String(n.mode), n.digest, n.origin]));
  const key = memoKey('chain', prevKey, stepKey);
  const hit = memo.get<Tree>(key);
  if (hit) return { tree: hit, key, memo: 'hit' };
  const out = tree.withFiles(p.files, policy, op.span);
  memo.set(key, out);
  return { tree: out, key, memo: p.hit ? 'files' : 'miss' };
}

function sitemap(op: Extract<Operation, { kind: 'SITEMAP' }>, tree: Tree, env: ExecEnv): Tree {
  const origin = `${env.label}:${op.span.line} SITEMAP`;
  return tree.withFiles([[op.dest, makeFile(new TextEncoder().encode(sitemapXml(tree, op.base)), origin)]], op.overwrite ? 'overwrite' : 'error', op.span);
}

/** Check a plugin file (sandbox, own folder, integrity) and start its process. */
async function registerPlugin(spec: PluginSpec, local: LocalContext, host: PluginHost, allowed: (spec: PluginSpec) => void): Promise<void> {
  const { real, bytes } = await local.realFile(spec.path, spec.span);
  if (spec.integrity) verifyIntegrity(bytes, spec.integrity, `./${spec.path}`, spec.span);
  if (real.endsWith('.wasm')) {
    // A wasm plugin has no imports, so it reads nothing: no own-folder rule needed.
    checkWasmPlugin(spec.name, bytes, spec.span);
    host.register(spec.name, real, spec.span, bytes);
    return;
  }
  // It runs as JavaScript (also a '.wasm' path that is a symlink to a JS file), so it needs the opt-in.
  allowed(spec);
  // A JS plugin may read its whole folder, so that folder must not be the context itself.
  if (path.dirname(real) === local.root) {
    throw new SitefileError(
      'E_VALIDATION',
      `PLUGIN ${spec.name}: put the plugin in its own folder (e.g. ./plugins/${path.basename(real)}); a plugin can read every file next to it`,
      spec.span,
    );
  }
  host.register(spec.name, real, spec.span);
}

/**
 * Check every JavaScript PLUGIN against `--allow-plugin` / `--allow-plugins` before
 * anything is read or started. A wasm plugin (a module that imports nothing) runs
 * without permission. Returns the check, which registerPlugin repeats on the real path.
 */
function checkPlugins(p: Plan, options: BuildOptions, warn: (m: string) => void): (spec: PluginSpec) => void {
  const names = new Set<string>();
  for (const name of options.allowPlugin ?? []) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) throw new SitefileError('E_USAGE', `--allow-plugin expects a PLUGIN name (an identifier), got '${name}'`);
    names.add(name);
  }
  const all = options.allowAllPlugins ?? false;
  const allowed = (spec: PluginSpec): void => {
    if (all || names.has(spec.name)) return;
    throw new SitefileError(
      'E_UNSUPPORTED',
      `PLUGIN ${spec.name} runs JavaScript, which must be allowed explicitly: add --allow-plugin ${spec.name} (or --allow-plugins to allow every PLUGIN); WebAssembly plugins need no flag`,
      spec.span,
    );
  };
  for (const spec of p.plugins) if (!spec.path.endsWith('.wasm')) allowed(spec);
  for (const name of names) if (!p.plugins.some((s) => s.name === name)) warn(`--allow-plugin ${name} does not match any PLUGIN and was ignored`);
  return allowed;
}

/** Match each declared TOOL to a `--allow-exec` entry and resolve it to a real path. */
async function resolveTools(p: Plan, options: BuildOptions, warn: (m: string) => void): Promise<ResolvedTool[]> {
  const allowed = new Map<string, string | undefined>();
  for (const spec of options.allowExec ?? []) {
    const eq = spec.indexOf('=');
    const name = eq < 0 ? spec : spec.slice(0, eq);
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || (eq >= 0 && eq === spec.length - 1)) {
      throw new SitefileError('E_USAGE', `--allow-exec expects NAME or NAME=/absolute/path (NAME is an identifier), got '${spec}'`);
    }
    allowed.set(name, eq < 0 ? undefined : spec.slice(eq + 1));
  }
  const out: ResolvedTool[] = [];
  for (const t of p.tools) {
    if (!allowed.has(t.name)) {
      throw new SitefileError(
        'E_UNSUPPORTED',
        `TOOL ${t.name} runs an external program, which must be allowed explicitly: add --allow-exec ${t.name} (or --allow-exec ${t.name}=/absolute/path)`,
        t.span,
      );
    }
    const tool = await resolveTool(t.name, allowed.get(t.name), process.env['PATH'], t.span);
    options.onNotice?.(`tool ${t.name} -> ${tool.path}`);
    out.push(tool);
  }
  for (const name of allowed.keys()) if (!p.tools.some((t) => t.name === name)) warn(`--allow-exec ${name} does not match any TOOL and was ignored`);
  return out;
}

/** Interpolate an operand with ARG/LET values and, inside FOR, the loop element. */
function interpolate(op: Operand, env: ExecEnv): string {
  let out = '';
  for (const part of op.parts) {
    if (part.kind === 'text') out += part.value;
    else out += String(lookup(part.name, part.path, part.span, env));
  }
  return out;
}

async function produceTransform(op: Extract<Operation, { kind: 'TRANSFORM' }>, env: RecEnv, opId: string): Promise<Produced> {
  const origin = `${env.label}:${op.span.line} TRANSFORM`;
  let items: unknown[] = [undefined];
  if (op.each) {
    const list = valueOf(op.each.list, [], op.span, false, env);
    if (!Array.isArray(list)) {
      throw new SitefileError('E_TYPE', `FOR ${op.each.as} IN ${op.each.list}: '${op.each.list}' is not a list`, op.span);
    }
    if (list.length === 0) env.warn(`${origin}: FOR ${op.each.as} IN ${op.each.list} produces nothing: the list is empty`);
    items = list;
  }

  // The source is the same for every element; read it once.
  let input: PluginRequest['input'];
  let data: unknown;
  let srcKey = '';
  if (op.source) {
    const ref = op.source;
    const res = ref.kind === 'import' ? env.resources.get(ref.name) : undefined;
    if (ref.kind === 'import' && res?.kind === 'value') {
      if (ref.path !== '/') throw new SitefileError('E_PATH', `import '${ref.name}' is a value; use '${ref.name}:'`, ref.span);
      data = res.value;
    } else {
      const src = await readSource(op.source, origin, env);
      if (src.kind !== 'file') {
        throw new SitefileError('E_TYPE', `TRANSFORM source '${src.label}' is a directory; plugins take one file`, op.source.span);
      }
      let text: string | undefined;
      try {
        text = new TextDecoder('utf-8', { fatal: true }).decode(src.bytes);
      } catch {
        text = undefined;
      }
      input = { path: src.label, name: src.basename || src.label, bytes: src.bytes, ...(text !== undefined ? { text } : {}) };
      if (env.memo) srcKey = sourceKey(ref, src, env);
    }
  }

  const tool = env.tools.get(op.plugin);
  if (tool && !input && op.source) {
    throw new SitefileError('E_TYPE', `TRANSFORM … WITH ${op.plugin}: a TOOL needs a file source, not a value import`, op.source.span);
  }
  const inputDigest = input ? sha256(input.bytes) : undefined;
  let pluginDigest: string | undefined;
  if ((env.cache || env.memo) && !tool) {
    if (!env.pluginDigests.has(op.plugin)) {
      // A wasm plugin is just its module; a JS plugin can read its whole folder.
      const wasm = env.plugins.wasmBytes(op.plugin);
      const dir = env.plugins.folderOf(op.plugin);
      const digest = wasm ? sha256(wasm) : dir ? await digestFolder(dir) : undefined;
      env.pluginDigests.set(op.plugin, digest);
      if (digest === undefined && env.cache) env.warn(`${origin}: PLUGIN ${op.plugin}'s folder is too large to hash; its results are not cached`);
    }
    pluginDigest = env.pluginDigests.get(op.plugin);
  }

  // Incremental rebuilds: the same code (plugin digest, or tool binary), source, data, list and interpolated names.
  const code = tool ? `tool\0${tool.path}\0${tool.size}\0${tool.mtimeMs}` : pluginDigest !== undefined ? `plugin\0${pluginDigest}` : undefined;
  const memo = env.memo && code !== undefined ? env.memo : undefined;
  const operandRefs = operandNames([op.dest, ...op.options.map((o) => o.value), ...(op.args ?? [])], new Set(data !== undefined && op.source?.kind === 'import' ? [op.source.name] : []));
  const key = memo ? memoKey('transform', opId, srcKey, code!, namesKey(env, op.each ? new Set([...operandRefs, op.each.list]) : operandRefs)) : undefined;
  // One FOR element's files: the same request gives the same result (the list itself is not read).
  const itemBase = memo && op.each ? memoKey('transform-item', opId, srcKey, code!, namesKey(env, operandRefs, op.each.as)) : undefined;
  const storable = (): boolean => tool !== undefined || env.plugins.cacheable(op.plugin);
  return memoFiles(
    env,
    key,
    async (children) => {
      const files: Files = [];
      for (const item of items) {
        const itemKey = itemBase !== undefined ? memoKey(itemBase, valueKey(item)) : undefined;
        const reused = itemKey !== undefined ? memo!.get<Files>(itemKey, children) : undefined;
        if (reused) {
          files.push(...reused);
          continue;
        }
        const itemFiles: Files = [];
        const itemEnv: ExecEnv = op.each ? { ...env, loop: { name: op.each.as, value: item } } : env;
        const rawDest = interpolate(op.dest, itemEnv);
        const dest = normalizeVirtual(rawDest, op.dest.span, 'TRANSFORM destination');
        const options: Record<string, string> = Object.create(null) as Record<string, string>;
        for (const o of op.options) options[o.key] = interpolate(o.value, itemEnv);
        const request: PluginRequest = {
          apiVersion: 1,
          dest: dest.path,
          options,
          ...(input ? { input } : {}),
          ...(data !== undefined ? { data } : {}),
          ...(op.each ? { item } : {}),
        };
        let args: string[] | undefined;
        if (tool) {
          if (dest.trailingSlash) throw new SitefileError('E_PLUGIN', `TOOL ${op.plugin} writes one file, but '${rawDest}' is a directory destination`, op.dest.span);
          args = (op.args ?? []).map((a) => interpolate(a, itemEnv));
        }
        // Everything the result can depend on: the code (plugin folder digest, or tool binary + argv) and the request.
        const cacheable = env.cache !== undefined && (tool !== undefined || pluginDigest !== undefined);
        const cached = cacheable
          ? cacheKey({
              code: tool ? { tool: { path: tool.path, size: tool.size, mtimeMs: tool.mtimeMs }, args } : { plugin: pluginDigest },
              request: {
                dest: dest.path,
                options,
                input: input && { path: input.path, name: input.name, digest: inputDigest },
                data,
                item,
              },
            })
          : undefined;
        const call = begin(env.profiler, `TRANSFORM ${op.plugin} ${dest.path}`, 'transform', { plugin: op.plugin, tool: tool !== undefined, dest: dest.path });
        let result: PluginResult | undefined = cached ? await env.cache!.get(cached) : undefined;
        const cacheState = !cached ? 'off' : result ? 'hit' : 'miss';
        if (!result) {
          if (tool) {
            env.stats.toolRuns++;
            result = await runTool(
              tool,
              { args: args!, dest: dest.path, options, ...(input ? { input: { name: input.name, bytes: input.bytes } } : {}) },
              env.toolPolicy,
              // The tool's stderr, like a TRANSFORM cache hit, is not replayed by a memo hit.
              (l) => env.unrecorded(l),
              op.span,
            );
          } else {
            result = await env.plugins.call(op.plugin, request, op.span);
          }
          if (cached && (tool || env.plugins.cacheable(op.plugin))) await env.cache!.put(cached, result);
        }
        call.end({ cache: cacheState });
        if (result.kind === 'file') {
          if (dest.trailingSlash) {
            throw new SitefileError('E_PLUGIN', `PLUGIN ${op.plugin} returned one file, but '${rawDest}' is a directory destination`, op.dest.span);
          }
          itemFiles.push([dest.path, makeFile(result.bytes, origin)]);
        } else {
          // { files }: placed under the destination directory.
          for (const [rel, bytes] of result.files) {
            const p = normalizeVirtual(`${dest.path === '/' ? '' : dest.path}/${rel}`, op.dest.span, `file '${rel}' returned by PLUGIN ${op.plugin}`).path;
            if (rel.includes('..') || rel.startsWith('/')) {
              throw new SitefileError('E_PLUGIN', `PLUGIN ${op.plugin} returned an unsafe path '${rel}'`, op.span);
            }
            itemFiles.push([p, makeFile(bytes, origin)]);
          }
        }
        if (itemKey !== undefined && storable()) memo!.set(itemKey, itemFiles);
        files.push(...itemFiles);
      }
      return files;
    },
    // A plugin that answered `cache: false` is not reused either.
    storable,
  );
}

/** REDIRECT and HEADER lines become `/_redirects` and `/_headers` at the root of the exported directory. */
function withSiteRules(tree: Tree, p: Plan, label: string): Tree {
  const files: Array<{ name: string; text: string; rules: ReadonlyArray<{ overwrite: boolean; span: SourceSpan }>; kind: string }> = [
    { name: '_redirects', text: redirectsFile(p.redirects), rules: p.redirects, kind: 'REDIRECT' },
    { name: '_headers', text: headersFile(p.headers), rules: p.headers, kind: 'HEADER' },
  ];
  for (const f of files) {
    const first = f.rules[0];
    if (!first) continue;
    const policy = f.rules.some((r) => r.overwrite) ? 'overwrite' : 'error';
    const node = makeFile(new TextEncoder().encode(f.text), `${label}:${first.span.line} ${f.kind}`);
    tree = tree.withFiles([[joinVirtual(p.exportPath, f.name), node]], policy, first.span);
  }
  return tree;
}

function feedFiles(op: Extract<Operation, { kind: 'FEED' }>, env: ExecEnv): Files {
  const origin = `${env.label}:${op.span.line} FEED`;
  const list = valueOf(op.list, [], op.span, false, env);
  if (!Array.isArray(list)) {
    throw new SitefileError('E_TYPE', `FEED ${op.list}: '${op.list}' is not a list (import a directory AS markdown)`, op.span);
  }
  if (list.length === 0) env.warn(`${origin}: '${op.list}' is empty, so the feed has no entries`);
  const entries: FeedEntry[] = [];
  list.forEach((item: unknown, i: number) => {
    if (item === null || typeof item !== 'object' || Array.isArray(item)) {
      throw new SitefileError('E_TYPE', `FEED ${op.list}: element ${i + 1} is not a document (an object with title and date)`, op.span);
    }
    const doc = item as Record<string, unknown>;
    const name = typeof doc['path'] === 'string' ? doc['path'] : `#${i + 1}`;
    const text = (key: string): string | undefined => {
      const v = doc[key];
      if (v === undefined || v === null) return undefined;
      if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return String(v);
      throw new SitefileError('E_TYPE', `FEED ${op.list}: '${key}' of document '${name}' is not text`, op.span);
    };
    const required = (key: string): string => {
      const v = text(key);
      if (v === undefined) throw new SitefileError('E_VALIDATION', `FEED ${op.list}: document '${name}' has no '${key}' (add it to its frontmatter)`, op.span);
      return v;
    };
    const title = required('title');
    const rawDate = required('date');
    const date = parseFeedDate(rawDate);
    if (!date) {
      throw new SitefileError('E_VALIDATION', `FEED ${op.list}: document '${name}' has date '${rawDate}'; expected YYYY-MM-DD or an RFC 3339 date-time`, op.span);
    }
    const raw = interpolate(op.link, { ...env, loop: { name: FEED_ITEM, value: item } });
    const link = normalizeVirtual(raw, op.link.span, 'FEED LINK');
    const author = text('author');
    const summary = text('excerpt');
    const content = text('content');
    entries.push({
      title,
      url: pageUrl(op.base, link.path === '/' || !link.trailingSlash ? link.path : link.path + '/'),
      date,
      ...(author !== undefined && author !== '' ? { author } : {}),
      ...(summary !== undefined && summary !== '' ? { summary } : {}),
      ...(content !== undefined ? { content } : {}),
    });
  });
  const xml = atomXml({ title: op.title, siteUrl: op.base + '/', selfUrl: pageUrl(op.base, op.dest), entries });
  return [[op.dest, makeFile(new TextEncoder().encode(xml), origin)]];
}

/** The files an operation (other than SITEMAP) adds; `opId` is its memo identity ('' without a memo). */
async function produce(op: Exclude<Operation, { kind: 'SITEMAP' }>, tree: Tree, env: RecEnv, opId: string): Promise<Produced> {
  if (op.kind === 'TRANSFORM') return produceTransform(op, env, opId);
  // A feed reads its list and what its LINK names.
  if (op.kind === 'FEED') {
    const key = env.memo ? memoKey('feed', opId, namesKey(env, operandNames([op.link], new Set([op.list])))) : undefined;
    return memoFiles(env, key, async () => feedFiles(op, env));
  }
  const origin = `${env.label}:${op.span.line} ${op.kind}`;

  if (op.kind === 'COPY') {
    const src = await readSource(op.source, origin, env);
    if (src.kind === 'file') {
      const intoDir = op.destIsDir || tree.isDirectory(op.dest);
      const key = env.memo ? memoKey('copy', opId, sourceKey(op.source, src, env), String(intoDir)) : undefined;
      return memoFiles(env, key, async () => {
        if (intoDir && src.inline) {
          throw new SitefileError('E_PATH', `'${op.dest}' is a directory; an inline ${src.label} body needs a file destination`, op.span);
        }
        const dest = intoDir ? joinVirtual(op.dest, src.basename) : op.dest;
        return [[dest, makeFile(src.bytes, origin, src.mode)]];
      });
    }
    const key = env.memo ? memoKey('copy', opId, sourceKey(op.source, src, env), 'tree') : undefined;
    return memoFiles(env, key, async () => {
      if (src.tree.size === 0) env.warn(`${origin}: '${src.label}' contains no files`);
      return src.tree.mountedAt(op.dest).map(([p, n]): [string, FileNode] => [p, { ...n, origin }]);
    });
  }

  const src = await readSource(op.template, origin, env);
  if (src.kind !== 'file') {
    throw new SitefileError('E_TYPE', `RENDER template '${src.label}' is a directory, not a file`, op.template.span);
  }
  const text = decodeUtf8(src.bytes, `template '${src.label}'`, op.template.span);
  const parts = src.inline
    ? parseRenderTemplate(text, src.inline.file, src.inline.firstLine)
    : parseRenderTemplate(text, src.label);
  const templateKey = env.memo ? sourceKey(op.template, src, env) : '';
  // What the template names, the FOR list and the destination's variables (ARG/LET values are in opId).
  const key = env.memo
    ? memoKey('render', opId, templateKey, namesKey(env, op.each ? operandNames([op.each.dest], templateNames(parts).add(op.each.list)) : templateNames(parts)))
    : undefined;
  return memoFiles(env, key, async (children) => {
    if (!op.each) {
      const out = renderTemplate(parts, (name, p, span) => lookup(name, p, span, env), op.escape, (name, p, span, loose) => valueOf(name, p, span, loose, env));
      return [[op.dest, makeFile(new TextEncoder().encode(out), origin)]];
    }

    // RENDER … FOR item IN list: one file per element, the destination interpolated per element.
    const { as, list, dest } = op.each;
    const items = valueOf(list, [], op.span, false, env);
    if (!Array.isArray(items)) {
      throw new SitefileError('E_TYPE', `FOR ${as} IN ${list}: '${list}' is not a list (import a directory AS markdown, or a JSON array)`, op.span);
    }
    if (items.length === 0) env.warn(`${origin}: FOR ${as} IN ${list} renders nothing: '${list}' is empty`);
    // Incremental rebuilds: an element's page depends on the template, the element and the imports the
    // template names (other than the loop variable), so editing one document re-renders one page.
    const memo = env.memo;
    const refsKey = memo ? namesKey(env, templateNames(parts), as) : '';
    const files: Files = [];
    for (const item of items) {
      const itemEnv: ExecEnv = { ...env, loop: { name: as, value: item } };
      let raw = '';
      for (const part of dest.parts) {
        if (part.kind === 'text') raw += part.value;
        else raw += String(part.name === as ? resolveJsonPath(item, part.path, as, part.span) : lookup(part.name, part.path, part.span, env));
      }
      const target = normalizeVirtual(raw, dest.span, `RENDER destination for ${as}`);
      if (target.trailingSlash) {
        throw new SitefileError('E_PATH', `RENDER destination '${raw}' must be a file path, not a directory`, dest.span);
      }
      const page = (): FileNode => {
        const out = renderTemplate(parts, (name, p, span) => lookup(name, p, span, itemEnv), op.escape, (name, p, span, loose) => valueOf(name, p, span, loose, itemEnv));
        return makeFile(new TextEncoder().encode(out), origin);
      };
      files.push([target.path, memo ? memo.memoize(memoKey('item', opId, templateKey, refsKey, valueKey(item)), page, children) : page()]);
    }
    return files;
  });
}

/** Apply an import's SORT BY to a markdown collection. */
function sorted(spec: ImportSpec, res: Resource): Resource {
  if (!spec.sort || res.kind !== 'value') return res;
  if (!Array.isArray(res.value)) {
    throw new SitefileError('E_TYPE', `IMPORT ${spec.name}: SORT BY needs a list (a directory of documents, a CSV file or a JSON/YAML array), not a single value`, spec.span);
  }
  if (!res.value.every((d) => d !== null && typeof d === 'object' && !Array.isArray(d))) {
    throw new SitefileError('E_TYPE', `IMPORT ${spec.name}: SORT BY needs a list of objects with a '${spec.sort.key}' field`, spec.span);
  }
  return { ...res, value: sortDocuments(res.value as MarkdownDocument[], spec.sort.key, spec.sort.descending) };
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
      const t = await env.local.readTree(ref.path, origin, ref.span);
      return { kind: 'tree', tree: t.tree, label, digest: t.provenance.digest };
    }
    const f = await env.local.readFile(ref.path, ref.span);
    return { kind: 'file', bytes: f.bytes, mode: f.mode, basename: f.basename, label, digest: `${f.provenance.digest}\0${f.mode}\0${f.basename}` };
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
  if (env.loop && env.loop.name === name) return resolveJsonPath(env.loop.value, p, name, span);
  const v = env.vars.get(name);
  if (v !== undefined) {
    if (p.length) throw new SitefileError('E_TYPE', `'${name}' is an ARG/LET value; dotted access only works on JSON imports`, span);
    return v;
  }
  const res = env.resources.get(name);
  if (!res) throw new SitefileError('E_UNDEFINED', `'${name}' is not an ARG, LET or IMPORT`, span);
  if (res.kind !== 'value') {
    throw new SitefileError('E_TYPE', `import '${name}' is a ${res.kind}; templates can only read value imports (text, json, yaml, csv, markdown, wbn)`, span);
  }
  if (res.format !== 'text') return resolveJsonPath(res.value, p, name, span);
  if (p.length) throw new SitefileError('E_TYPE', `import '${name}' is text; dotted access only works on structured imports (json, yaml, csv, markdown, wbn)`, span);
  return res.value as string;
}

/** Any value (lists, documents, scalars) for template blocks and FOR. `loose`: missing → undefined. */
function valueOf(name: string, p: readonly string[], span: SourceSpan, loose: boolean, env: ExecEnv): unknown {
  if (env.loop && env.loop.name === name) return walk(env.loop.value, p, name, span, loose);
  const v = env.vars.get(name);
  if (v !== undefined) return walk(v, p, name, span, loose);
  const res = env.resources.get(name);
  if (!res) {
    if (loose) return undefined;
    throw new SitefileError('E_UNDEFINED', `'${name}' is not an ARG, LET or IMPORT`, span);
  }
  if (res.kind !== 'value') {
    throw new SitefileError('E_TYPE', `import '${name}' is a ${res.kind}; templates can only read value imports (text, json, yaml, csv, markdown, wbn)`, span);
  }
  return walk(res.value, p, name, span, loose);
}

// ---------------------------------------------------------------- helpers

async function mapLimit<T, R>(items: readonly T[], limit: number, fn: (item: T, index: number) => Promise<R>): Promise<R[]> {
  const out = new Array<R>(items.length);
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i]!, i);
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
