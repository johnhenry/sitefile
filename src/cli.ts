#!/usr/bin/env node
import { parseArgs } from 'node:util';
import path from 'node:path';
import { errorJson, SitefileError, isSitefileError } from './errors.js';
import { build, buildToDirectory, checkPlanPins, lintBuild, planBuild, sitefileCompression, type BuildOptions } from './executor.js';
import { LINT_RULES } from './lint.js';
import { compressionFromExtension, defaultOutName, formatFromExtension } from './export-formats.js';
import { collectBuildArgs } from './build-args.js';
import { insecureRegistriesFromEnv, makeClient } from './base-image.js';
import { cacheLimitFromEnv } from './cache-limit.js';
import { parseCacheRef, RegistryCache } from './registry-cache.js';
import { MAX_OCI_LAYERS, parseLabels } from './oci.js';
import { EXPORT_FORMATS, parseExportFormat, parseOciCompression, type ExportFormat, type OciCompression } from './language/ast.js';
import { DEFAULT_FETCH_POLICY } from './resources/types.js';
import { parseLocalRef, refKey } from './ref.js';
import { EXTRA_HELP, isExtraCommand, runExtraCommand } from './cli/index.js';
import { evictAfterBuild, transformCacheDir } from './cli/cache.js';
import { storeBuildAsImage, validateImageTags } from './cli/build-image.js';
import { SITEFILE_VERSION } from './attest.js';
import { pollOption, watchLoop } from './cli/watch.js';
import { Profiler } from './profile.js';
import { watchMemo, type BuildMemo } from './memo.js';
import { promises as fs } from 'node:fs';
import { graphDot, graphText, planGraph, why, whyText } from './explain.js';
import { isGitContext, materializeGitContext, type GitContext } from './cli/git-context.js';
import { loadSecrets } from './secrets.js';
import { exportFile } from './exporter.js';
import { encodeManifest, loadTarget, manifestOf, resolveTarget } from './integrity.js';
import { addDictionaryDeltas } from './dictionary.js';
import { Store } from './store.js';
import type { Tree } from './tree.js';
import { NETWORK_OPTIONS, networkHelp, networkOption } from './cli/network.js';

const VERSION = SITEFILE_VERSION;

const HELP = `sitefile ${VERSION} — Docker-inspired static artifact builder

Usage:
  sitefile build [context] [options]     build and export to --out (default: <context>/dist, or dist.<ext>)
  sitefile validate [context] [options]  parse, validate and evaluate variables (no IO)
  sitefile inspect [context] [options]   build in memory and list the exported files
  sitefile graph [context] [options]     print the dependency graph of imports, stages and operations
                                         (text, --json or --dot; no IO, like validate)
  sitefile why <path> [context] [opts]   build in memory and show which instructions wrote an exported
                                         file, from what inputs, and what overwrote it
  The context may be a Git URL, https://host/repo.git#ref:dir (fetched to a temporary directory;
  the default --out is then ./dist; see docs/CLI.md).

Options:
  -f, --file <path>            Sitefile path (default: <context>/Sitefile)
      --target <stage>         stop after the named stage and export its whole tree instead of
                               the EXPORT path (EXPORT's format still applies; no _redirects/_headers)
  -o, --out <path>             export destination (build only): a directory for 'directory',
                               a file for zip, tar, tgz, wbn (*.wbn.gz: a gzipped bundle), car, html, mhtml, epub
  -t, --tag <name[:tag]>       also store the build as a local image (build only, repeatable);
                               with -t and no -o nothing is written to the context
      --format <fmt>           directory | zip | tar | tgz (alias tar.gz) | wbn | oci | car | html | mhtml | epub
                               (build only).
                               oci writes an OCI archive for -o *.tar (e.g. site.oci.tar), else an
                               OCI image layout directory.
                               Default: the --out extension (.zip .tar .tar.gz .tgz .wbn .wbn.gz .oci.tar .car .mhtml .epub),
                               else the Sitefile's EXPORT ... AS <fmt>
      --oci-root <path>        directory inside an oci image layer (default /srv/www; overrides EXPORT ... AT)
      --compression <c>        gzip or none (build only; overrides EXPORT ... COMPRESSION): the oci site layer
                               (default gzip; none: plain tar), or the whole wbn (default none; gzip:
                               gzip(application/webbundle), also chosen by -o *.wbn.gz)
      --oci-compression <c>    the same as --compression, for oci only
      --oci-layers <n>         split the oci image's files over n layers (1-64, default 1) by a hash of each
                               path, so pushing a new version uploads only the layers that changed
      --label <key=value>      add an OCI image config label (repeatable)
      --git-labels             add org.opencontainers.image.source/revision from the git checkout
                               (off by default: they change the image ID on every commit)
      --provenance             attach a SLSA provenance attestation to the oci image (an in-toto
                               statement: Sitefile digest, build args, imports, BASE, builder)
      --sbom                   attach an SBOM attestation (CycloneDX: every file with its SHA-256);
                               both are OCI 1.1 referrers of the image (its ID does not change),
                               written into the oci export and kept with -t images for push
      --platform <os/arch>     oci image platform (default linux/amd64); with a BASE, the platform pulled
      --base <image>           build the oci image on <image> (runnable; overrides EXPORT ... AS oci BASE);
                               a bare name means Docker Hub. Pin it: nginx:alpine@sha256:...
      --insecure-registry <h>  allow plain http to registry host[:port] (repeatable; localhost always may)
      --base-url <url>         https URL ending in '/' the files are mounted at (wbn, mhtml);
                               overrides EXPORT ... BASE
      --build-arg <NAME=value> set an ARG (repeatable); --build-arg NAME reads $NAME
      --secret id=<id>,env=<VAR> | id=<id>,src=<file>
                               a build secret for IMPORT ... SECRET <id> AS HEADER <Name> | AS BEARER
                               (repeatable); sent only to that import's origin, never recorded
      --plugin-timeout <ms>    per-call time limit for TRANSFORM plugins and tools (default 60000)
      --plugin-memory <MiB>    memory limit for each plugin: a JavaScript plugin's heap, a wasm
                               plugin's linear memory (default 1024)
      --node <path>            the Node binary JavaScript plugins run on (absolute path; default
                               $SITEFILE_NODE, else the default Node, see docs/PLUGINS.md); must be >= 26
      --allow-exec <name[=path]> allow a TOOL (external program) to run, resolved on PATH or at
                               the absolute path (repeatable); programs run with your privileges
      --allow-plugin <name>    allow a JavaScript PLUGIN to run (repeatable; wasm plugins need no flag)
      --allow-plugins          allow every JavaScript PLUGIN in the Sitefile
      --no-cache               do not read or write the TRANSFORM result cache
                               ($SITEFILE_HOME/cache/transform; see 'sitefile cache --help')
      --cache-from <ref>       also look up TRANSFORM results in this registry cache (build only,
                               repeatable, first match wins); results are fetched only on a hit
      --cache-to <ref>         after a successful build, push the TRANSFORM results it used to this
                               registry cache (build only, once); registry problems only warn
      --build-arg-file <path>  read ARGs from a .json, .yaml/.yml or .env file (repeatable;
                               later files win, --build-arg wins over files)
      --manifest <file>        also write a manifest of the exported files (build only): path, size and
                               SHA-256 of each, and their Merkle root; check copies with 'sitefile verify'
      --experimental-dictionary <previous>
                               Compression Dictionary Transport (experimental, build only): add a .dcz delta
                               against <previous> (image, directory or archive) for each changed .js/.css/...
                               and Use-As-Dictionary headers; 'serve' answers Available-Dictionary with them
      --force                  replace a non-empty --out outside the context
      --frozen-lockfile        fail instead of changing Sitefile.lock
      --update-lock            re-record unpinned URLs whose content changed
      --strict-pins            fail when a remote IMPORT has no INTEGRITY and no lockfile entry, or the
                               BASE image is neither @sha256:-pinned nor locked (build, validate, inspect)
      --lint-disable <rules>   turn lint rules off (validate; comma-separated rule ids, or all; repeatable)
      --no-lockfile            do not read or write Sitefile.lock
      --vendor-dir <dir>       read remote imports and BASE blobs from this vendor directory (relative
                               to the context; default vendor/sitefile when it exists; see 'sitefile vendor')
      --allow-http             permit plain http:// imports
      --allow-private-network  permit loopback/private network addresses
      --allow-hardlinks        permit hard-linked files in the context (you assert they are safe)
      --keep-os-metadata       keep __MACOSX, ._*, .DS_Store, Thumbs.db and desktop.ini in archives
                               imported AS tree (default: skipped with a warning)
      --max-download-bytes <n> per-resource size limit (default ${DEFAULT_FETCH_POLICY.maxBytes})
      --timeout <ms>           per-resource fetch timeout (default ${DEFAULT_FETCH_POLICY.timeoutMs})
${networkHelp(25).map((l) => `      ${l}`).join('\n')}
      --json                   machine-readable output (validate, inspect, graph, why); with any
                               command, a failure is also printed on stdout as JSON (code, span, needs)
      --dot                    Graphviz DOT output (graph only)
      --watch                  build, then rebuild when the context, the Sitefile or a
                               --build-arg-file changes (build only; Ctrl-C stops)
      --poll                   --watch: poll for changes (every 1000 ms) instead of file system
                               events (network shares, Docker bind mounts, WSL)
      --poll-interval <ms>     --watch: poll every <ms> milliseconds (implies --poll)
      --profile <file>         write a timing trace of the build (Chrome trace event JSON:
                               chrome://tracing, ui.perfetto.dev); --watch rewrites it per build
  -h, --help                   show this help
  -v, --version                show the version
${EXTRA_HELP}`;

/**
 * Run the CLI. `run.once` runs a --watch command a single time (the watch loop's
 * builds); `run.wrote` then receives the export destination, and `run.memo` carries
 * results between those builds (incremental rebuilds).
 */
export async function main(
  argv: readonly string[],
  run: { readonly once?: boolean; readonly wrote?: string[]; readonly memo?: BuildMemo | undefined } = {},
): Promise<number> {
  if (isExtraCommand(argv[0])) return runExtraCommand(argv);
  let parsed;
  try {
    parsed = parseArgs({
      args: [...argv],
      allowPositionals: true,
      options: {
        ...NETWORK_OPTIONS,
        file: { type: 'string', short: 'f' },
        target: { type: 'string' },
        out: { type: 'string', short: 'o' },
        tag: { type: 'string', short: 't', multiple: true },
        format: { type: 'string' },
        'base-url': { type: 'string' },
        'oci-root': { type: 'string' },
        'oci-compression': { type: 'string' },
        'oci-layers': { type: 'string' },
        compression: { type: 'string' },
        label: { type: 'string', multiple: true },
        'git-labels': { type: 'boolean' },
        provenance: { type: 'boolean' },
        sbom: { type: 'boolean' },
        platform: { type: 'string' },
        base: { type: 'string' },
        'insecure-registry': { type: 'string', multiple: true },
        'build-arg': { type: 'string', multiple: true },
        'build-arg-file': { type: 'string', multiple: true },
        secret: { type: 'string', multiple: true },
        'plugin-timeout': { type: 'string' },
        'plugin-memory': { type: 'string' },
        node: { type: 'string' },
        'allow-exec': { type: 'string', multiple: true },
        'allow-plugin': { type: 'string', multiple: true },
        'allow-plugins': { type: 'boolean' },
        'no-cache': { type: 'boolean' },
        'cache-from': { type: 'string', multiple: true },
        'cache-to': { type: 'string', multiple: true },
        force: { type: 'boolean' },
        'frozen-lockfile': { type: 'boolean' },
        'update-lock': { type: 'boolean' },
        'strict-pins': { type: 'boolean' },
        'lint-disable': { type: 'string', multiple: true },
        'no-lockfile': { type: 'boolean' },
        'vendor-dir': { type: 'string' },
        'allow-http': { type: 'boolean' },
        'allow-private-network': { type: 'boolean' },
        'allow-hardlinks': { type: 'boolean' },
        'keep-os-metadata': { type: 'boolean' },
        'max-download-bytes': { type: 'string' },
        timeout: { type: 'string' },
        json: { type: 'boolean' },
        dot: { type: 'boolean' },
        watch: { type: 'boolean' },
        poll: { type: 'boolean' },
        'poll-interval': { type: 'string' },
        profile: { type: 'string' },
        manifest: { type: 'string' },
        'experimental-dictionary': { type: 'string' },
        help: { type: 'boolean', short: 'h' },
        version: { type: 'boolean', short: 'v' },
      },
    });
  } catch (e) {
    return usageError((e as Error).message);
  }
  const { values: o, positionals } = parsed;
  if (o.version) {
    process.stdout.write(`${VERSION}\n`);
    return 0;
  }
  const [command, ...rest] = positionals;
  if (o.help || !command) {
    process.stdout.write(HELP);
    return o.help ? 0 : 2;
  }
  // why <path> [context]
  const whyPath = command === 'why' ? rest.shift() : undefined;
  if (command === 'why' && whyPath === undefined) return usageError('why expects a path in the build output: sitefile why <path> [context]');
  const [contextArg = '.', ...extra] = rest;
  if (extra.length) return usageError(`unexpected arguments: ${extra.join(' ')}`);
  const gitContext = isGitContext(contextArg);
  if (gitContext && o.watch) return usageError('--watch needs a local context directory, not a Git URL');
  if (gitContext && o.file !== undefined && (path.isAbsolute(o.file) || o.file.split(/[\\/]/).includes('..'))) {
    return usageError('with a Git URL context, -f/--file is a path inside the repository');
  }
  if (o.dot && command !== 'graph') return usageError('--dot is only for graph');
  if (o.dot && o.json) return usageError('--dot and --json conflict');
  if (o['frozen-lockfile'] && o['update-lock']) return usageError('--frozen-lockfile and --update-lock conflict');
  if (o['strict-pins'] && o['update-lock']) return usageError('--strict-pins and --update-lock conflict');
  const lintDisabled = (o['lint-disable'] ?? []).flatMap((v) => v.split(',').map((s) => s.trim()));
  if (o['lint-disable'] && command !== 'validate') return usageError('--lint-disable is only for validate');
  for (const id of lintDisabled) {
    if (id !== 'all' && !(LINT_RULES as readonly string[]).includes(id)) {
      return usageError(`--lint-disable: unknown lint rule '${id}' (rules: ${LINT_RULES.join(', ')}, or all)`);
    }
  }
  const cacheFrom = o['cache-from'] ?? [];
  const cacheTo = o['cache-to'] ?? [];
  if (cacheFrom.length || cacheTo.length) {
    if (command !== 'build') return usageError('--cache-from and --cache-to are only for build');
    if (o.watch) return usageError('--watch cannot be combined with --cache-from or --cache-to');
    if (o['no-cache']) return usageError('--no-cache cannot be combined with --cache-from or --cache-to');
    if (cacheTo.length > 1) return usageError('--cache-to can be given only once');
  }
  if (!o.watch && (o.poll || o['poll-interval'] !== undefined)) return usageError(`--${o['poll-interval'] !== undefined ? 'poll-interval' : 'poll'} needs --watch`);
  if (o.watch && !run.once) {
    if (command !== 'build') return usageError(`--watch is only for build (and serve --watch), not ${command}`);
    if ((o.tag ?? []).length) return usageError('--watch cannot be combined with -t');
    let pollMs: number | undefined;
    try {
      pollMs = pollOption(o.poll, o['poll-interval']);
    } catch (e) {
      return usageError((e as SitefileError).detail);
    }
    try {
      const memo = watchMemo();
      return await watchLoop({
        context: contextArg,
        ...(pollMs !== undefined ? { pollMs } : {}),
        ...(o.file ? { sitefile: o.file } : {}),
        argFiles: o['build-arg-file'] ?? [],
        build: async () => {
          const wrote: string[] = [];
          return { code: await main(argv, { once: true, wrote, memo }), wrote };
        },
      });
    } catch (e) {
      if (!isSitefileError(e)) throw e;
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      return 1;
    }
  }

  if (o.profile === '') return usageError('--profile expects a file path');
  if (o.manifest !== undefined && command !== 'build') return usageError('--manifest is only for build');
  if (o.manifest === '') return usageError('--manifest expects a file path');
  if (o.manifest !== undefined) run.wrote?.push(o.manifest);
  if (o['experimental-dictionary'] !== undefined && command !== 'build') return usageError('--experimental-dictionary is only for build');
  if (o['experimental-dictionary'] === '') return usageError('--experimental-dictionary expects an image, directory or archive');
  // --profile: a trace of this command (build, validate, inspect, graph, why), written when it ends.
  const profiler = o.profile !== undefined ? new Profiler(command) : undefined;
  if (o.profile !== undefined) run.wrote?.push(o.profile);
  const commandSpan = profiler?.begin(command, 'command');
  let failed = false;
  let fetched: GitContext | undefined;
  try {
    const network = networkOption(o);
    const fetchPolicy = {
      allowHttp: !!o['allow-http'],
      allowPrivateNetwork: !!o['allow-private-network'],
      ...(o['max-download-bytes'] ? { maxBytes: positiveInt(o['max-download-bytes'], '--max-download-bytes') } : {}),
      ...(o.timeout ? { timeoutMs: positiveInt(o.timeout, '--timeout') } : {}),
      ...(network ? { network } : {}),
    };
    if (gitContext) {
      // A Git URL context: fetched into a temporary directory, built from there, then removed.
      fetched = await materializeGitContext(contextArg, { ...DEFAULT_FETCH_POLICY, ...fetchPolicy });
      process.stderr.write(`context: ${contextArg} at commit ${fetched.commit}\n`);
    }
    const context = fetched?.dir ?? contextArg;
    const file = fetched && o.file !== undefined ? path.join(context, o.file) : o.file;
    const remoteCache =
      cacheFrom.length || cacheTo.length
        ? new RegistryCache({
            from: cacheFrom.map((r) => parseCacheRef('--cache-from', r)),
            to: cacheTo[0] !== undefined ? parseCacheRef('--cache-to', cacheTo[0]) : undefined,
            client: (registry) => makeClient(registry, { insecureRegistries: insecureRegistriesFromEnv(o['insecure-registry'] ?? []), allowPrivateNetwork: !!o['allow-private-network'], network }),
            limit: cacheLimitFromEnv(),
            warn: (m) => process.stderr.write(`warning: ${m}\n`),
            notice: (m) => process.stderr.write(`${m}\n`),
          })
        : undefined;
    // Precedence: Sitefile defaults < --build-arg-file (later files win) < --build-arg.
    const buildArgs = await collectBuildArgs(o['build-arg-file'] ?? [], o['build-arg'] ?? []);
    const secrets = await loadSecrets(o.secret ?? []);
    let formatOverride: ExportFormat | undefined;
    if (o.format !== undefined) {
      formatOverride = parseExportFormat(o.format);
      if (!formatOverride) return usageError(`--format expects one of ${EXPORT_FORMATS.join(', ')} (or tar.gz), got '${o.format}'`);
    }
    let ociLayers: number | undefined;
    if (o['oci-layers'] !== undefined) {
      if (command !== 'build') return usageError('--oci-layers is only for build');
      ociLayers = /^[0-9]{1,2}$/.test(o['oci-layers']) ? Number(o['oci-layers']) : 0;
      if (ociLayers < 1 || ociLayers > MAX_OCI_LAYERS) return usageError(`--oci-layers expects an integer from 1 to ${MAX_OCI_LAYERS}, got '${o['oci-layers']}'`);
    }
    let ociCompression: OciCompression | undefined;
    if (o['oci-compression'] !== undefined) {
      ociCompression = parseOciCompression(o['oci-compression']);
      if (!ociCompression) return usageError(`--oci-compression expects gzip or none, got '${o['oci-compression']}'`);
    }
    let compression: OciCompression | undefined;
    if (o.compression !== undefined) {
      compression = parseOciCompression(o.compression);
      if (!compression) return usageError(`--compression expects gzip or none, got '${o.compression}'`);
      if (ociCompression !== undefined) return usageError('--compression and --oci-compression set the same thing; give one');
    }
    // No --format: an archive extension on --out picks the format, so `-o site.zip`
    // writes a zip instead of a directory named site.zip.
    if (!formatOverride && o.out) formatOverride = formatFromExtension(o.out);
    // The default destination depends on the format, which the Sitefile may set.
    // (A wbn's default name also depends on its COMPRESSION: dist.wbn or dist.wbn.gz.)
    const sitefilePlan =
      command === 'build' && !o.out && (!formatOverride || formatOverride === 'wbn') ? await planBuild({ contextDir: context, ...(file ? { sitefile: file } : {}), buildArgs }) : undefined;
    const sitefileFormat = formatOverride ? undefined : sitefilePlan?.exportFormat;
    const defaultFormat = formatOverride ?? sitefileFormat ?? 'directory';
    const defaultCompression = defaultFormat === 'wbn' ? (compression ?? (sitefilePlan ? sitefileCompression(sitefilePlan, 'wbn') : undefined)) : undefined;
    // A Git context's default destination is in the current directory, not the temporary checkout.
    const out = o.out ?? path.join(fetched ? '.' : context, defaultOutName(defaultFormat, defaultCompression));
    run.wrote?.push(out);
    const options: BuildOptions = {
      contextDir: context,
      ...(file ? { sitefile: file } : {}),
      ...(fetched ? { label: o.file ?? 'Sitefile' } : {}),
      ...(o.target !== undefined ? { target: o.target } : {}),
      buildArgs,
      ...(secrets.size ? { secrets } : {}),
      outDir: out,
      fetchPolicy,
      ...(o['no-lockfile'] ? { lockfile: false as const } : {}),
      ...(o['vendor-dir'] !== undefined ? { vendorDir: o['vendor-dir'] } : {}),
      ...(o['plugin-timeout'] || o['plugin-memory']
        ? {
            pluginPolicy: {
              ...(o['plugin-timeout'] ? { timeoutMs: positiveInt(o['plugin-timeout'], '--plugin-timeout') } : {}),
              ...(o['plugin-memory'] ? { maxMemoryMiB: positiveInt(o['plugin-memory'], '--plugin-memory') } : {}),
            },
            ...(o['plugin-timeout'] ? { toolPolicy: { timeoutMs: positiveInt(o['plugin-timeout'], '--plugin-timeout') } } : {}),
          }
        : {}),
      ...pluginNode(o.node),
      ...(o['allow-exec'] ? { allowExec: o['allow-exec'] } : {}),
      ...(o['allow-plugin'] ? { allowPlugin: o['allow-plugin'] } : {}),
      allowAllPlugins: !!o['allow-plugins'],
      ...(o['no-cache'] ? {} : { cacheDir: transformCacheDir() }),
      ...(remoteCache ? { remoteCache } : {}),
      onNotice: (m) => process.stderr.write(`${m}\n`),
      allowHardlinks: !!o['allow-hardlinks'],
      keepOsMetadata: !!o['keep-os-metadata'],
      ...(o.platform !== undefined ? { platform: o.platform } : {}),
      ...(o.base !== undefined ? { baseImage: o.base } : {}),
      // inspect never pulls; a non-oci export (with no image tag) has no use for a BASE
      ...(command === 'inspect' || command === 'why' || (command === 'build' && formatOverride !== undefined && formatOverride !== 'oci' && !(o.tag ?? []).length) ? { skipBase: true } : {}),
      registry: { insecureRegistries: insecureRegistriesFromEnv(o['insecure-registry'] ?? []) },
      frozenLockfile: !!o['frozen-lockfile'],
      updateLock: !!o['update-lock'],
      strictPins: !!o['strict-pins'],
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
      ...(profiler ? { profiler, alsoWrites: [o.profile!] } : {}),
      ...(run.memo ? { memo: run.memo } : {}),
    };
    // --compression belongs to the export's own format, oci or wbn (-t without -o stores an oci image);
    // a *.wbn.gz destination means a gzipped bundle.
    let wbnCompression: OciCompression | undefined;
    if (command === 'build' && compression !== undefined) {
      const fmt = (o.tag ?? []).length && !o.out ? 'oci' : (formatOverride ?? sitefileFormat ?? (await planBuild(options)).exportFormat);
      if (fmt === 'oci') ociCompression = compression;
      else if (fmt === 'wbn') wbnCompression = compression;
      else return usageError(`--compression applies only to oci and wbn exports, not ${fmt}`);
    }
    if (formatOverride === 'wbn' && o.out) wbnCompression ??= compressionFromExtension(o.out, 'wbn');

    switch (command) {
      case 'build': {
        const tags = o.tag ?? [];
        validateImageTags(tags);
        const labels = parseLabels(o.label ?? []);
        const attest = { provenance: !!o.provenance, sbom: !!o.sbom };
        if ((attest.provenance || attest.sbom) && !tags.length && (formatOverride ?? sitefileFormat ?? (await planBuild(options)).exportFormat) !== 'oci') {
          return usageError('--provenance and --sbom attach attestations to an oci image: export one (--format oci, -o *.oci.tar, EXPORT ... AS oci) or store one with -t');
        }
        const imageInfo = {
          contextDir: context,
          ociRoot: o['oci-root'],
          ociCompression,
          ociLayers,
          platform: o.platform,
          labels,
          gitLabels: !!o['git-labels'],
          attest,
          build: { contextDir: context, ...(file ? { sitefile: file } : {}), ...(fetched ? { label: o.file ?? 'Sitefile' } : {}), buildArgs },
        };
        await remoteCache?.load();
        const dictionaryFrom = await previousBuild(o['experimental-dictionary']);
        if (tags.length && !o.out) {
          // -t without -o: store the image only; nothing is written to the context.
          const raw = await build(options);
          const built = dictionaryFrom ? { ...raw, output: addDictionaryDeltas(raw.output, dictionaryFrom).tree } : raw;
          if (o.manifest !== undefined) await writeManifest(built.output, o.manifest, context, !!o.force);
          await storeBuildAsImage(built, tags, imageInfo);
          await remoteCache?.push(transformCacheDir());
          return 0;
        }
        if (o.manifest !== undefined && (formatOverride ?? sitefileFormat ?? (await planBuild(options)).exportFormat) === 'directory' && isInsideDir(o.manifest, out)) {
          return usageError(`--manifest '${o.manifest}' is inside the exported directory '${out}': write it elsewhere`);
        }
        const r = await buildToDirectory({
          ...options,
          outDir: out,
          force: !!o.force,
          ...(formatOverride ? { format: formatOverride } : {}),
          ...(o['base-url'] !== undefined ? { baseUrl: o['base-url'] } : {}),
          ...(o['oci-root'] !== undefined ? { ociRoot: o['oci-root'] } : {}),
          ...(ociCompression !== undefined ? { ociCompression } : {}),
          ...(ociLayers !== undefined ? { ociLayers } : {}),
          ...(wbnCompression !== undefined ? { wbnCompression } : {}),
          ...(o.platform !== undefined ? { platform: o.platform } : {}),
          imageLabels: labels,
          gitLabels: !!o['git-labels'],
          ...(tags[0] ? { ociRefName: refKey(parseLocalRef(tags[0])) } : {}),
          attest,
          ...(dictionaryFrom ? { dictionaryFrom } : {}),
        });
        const where = path.relative(process.cwd(), r.outDir) || '.';
        const n = `${r.output.size} file${r.output.size === 1 ? '' : 's'}`;
        process.stdout.write(
          r.format === 'directory'
            ? `Exported ${n} to ${where} (${r.output.digest()})\n`
            : r.format === 'oci'
              ? `Exported ${n} to ${where} (oci image ${r.manifestDigest}${r.bytes !== undefined ? `, ${r.bytes} bytes` : ''})\n`
              : `Exported ${n} to ${where} (${r.format === 'tgz' ? 'tar.gz' : r.format}, ${r.bytes} bytes, ${r.fileDigest}${r.rootCid ? `, root ${r.rootCid}` : ''})\n`,
        );
        if (r.dictionaryDeltas) process.stdout.write(`Dictionary deltas: ${r.dictionaryDeltas.length} (.dcz)\n`);
        if (o.manifest !== undefined) await writeManifest(r.output, o.manifest, context, !!o.force);
        // A Git context's lockfile lives in the temporary checkout: it is used, never written back.
        if (r.lockfileWritten && !fetched) process.stdout.write('Updated Sitefile.lock\n');
        if (tags.length) await storeBuildAsImage(r, tags, imageInfo);
        await remoteCache?.push(transformCacheDir());
        return 0;
      }
      case 'validate': {
        const span = profiler?.begin('parse', 'build');
        const p = await planBuild(options);
        span?.end({ imports: p.imports.length, stages: p.stages.length, ...(p.target !== undefined ? { target: p.target } : {}) });
        p.warnings.forEach((m) => process.stderr.write(`warning: ${m}\n`));
        (await lintBuild(options, lintDisabled)).forEach((m) => process.stderr.write(`warning: ${m}\n`));
        if (o['strict-pins']) await checkPlanPins(options, p);
        if (o.json) {
          process.stdout.write(
            JSON.stringify(
              { file: p.file, vars: Object.fromEntries(p.vars), imports: p.imports, stages: p.stages, export: p.exportPath, ...(p.target !== undefined ? { target: p.target } : {}) },
              null,
              2,
            ) + '\n',
          );
        } else {
          const opCount = p.stages.reduce((n, st) => n + st.operations.length, 0);
          process.stdout.write(
            `${p.file}: OK (${p.imports.length} import${p.imports.length === 1 ? '' : 's'}, ${opCount} operation${opCount === 1 ? '' : 's'}${p.stages.length > 1 ? `, ${p.stages.length} stages` : ''}${p.target !== undefined ? `, target ${p.target}` : ''})\n`,
          );
        }
        return 0;
      }
      case 'inspect': {
        const r = await build({ ...options, frozenLockfile: true });
        const files = r.output.entries().map(([p, n]) => ({ path: p, mode: n.mode.toString(8), size: n.bytes.byteLength, digest: n.digest, origin: n.origin }));
        if (o.json) {
          process.stdout.write(JSON.stringify({ digest: r.output.digest(), files }, null, 2) + '\n');
        } else {
          for (const f of files) process.stdout.write(`${f.mode}  ${String(f.size).padStart(8)}  ${f.path}  (${f.origin})\n`);
          process.stdout.write(`${files.length} files, ${r.output.digest()}\n`);
        }
        return 0;
      }
      case 'graph': {
        const span = profiler?.begin('parse', 'build');
        const p = await planBuild(options);
        span?.end({ imports: p.imports.length, stages: p.stages.length, ...(p.target !== undefined ? { target: p.target } : {}) });
        p.warnings.forEach((m) => process.stderr.write(`warning: ${m}\n`));
        if (o['strict-pins']) await checkPlanPins(options, p);
        const g = planGraph(p);
        process.stdout.write(o.json ? JSON.stringify(g, null, 2) + '\n' : o.dot ? graphDot(g) : graphText(g));
        return 0;
      }
      case 'why': {
        const r = await build({ ...options, frozenLockfile: true, recordSteps: true });
        const report = why(r, whyPath!, context);
        process.stdout.write(o.json ? JSON.stringify(report, null, 2) + '\n' : whyText(report));
        return 0;
      }
      default:
        return usageError(`unknown command '${command}'`);
    }
  } catch (e) {
    failed = true;
    if (isSitefileError(e)) {
      if (e.code === 'E_USAGE') return usageError(e.detail);
      // With --json, the failure is also machine-readable on stdout (code, span, needs).
      if (o.json) process.stdout.write(errorJson(e));
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      return 1;
    }
    throw e;
  } finally {
    // Keep $SITEFILE_HOME under $SITEFILE_CACHE_MAX (never fails the command).
    if (command === 'build' || command === 'inspect' || command === 'why') await evictAfterBuild();
    await fetched?.dispose();
    commandSpan?.end();
    if (profiler && !(await fs.writeFile(o.profile!, profiler.toJSON()).then(() => true, () => false))) {
      // A failed build already reported its error; otherwise this is the command's error.
      if (!failed) {
        process.stderr.write(`error[E_EXPORT]: cannot write the --profile file '${o.profile}'\n`);
        // eslint-disable-next-line no-unsafe-finally
        return 1;
      }
    }
  }
}

/** `--node`, else `$SITEFILE_NODE` (ignored when empty): an absolute path. */
function pluginNode(flag: string | undefined): { node?: string } {
  const [value, from] = flag !== undefined ? [flag, '--node'] : [process.env['SITEFILE_NODE'] || undefined, 'SITEFILE_NODE'];
  if (value === undefined) return {};
  if (!path.isAbsolute(value)) throw new SitefileError('E_USAGE', `${from} expects an absolute path to a Node binary, got '${value}'`);
  return { node: value };
}

/** True when `file` would be written inside the directory `dir` (lexically, from the cwd). */
function isInsideDir(file: string, dir: string): boolean {
  const rel = path.relative(path.resolve(dir), path.resolve(file));
  return rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel);
}

/** `--experimental-dictionary <previous>`: the previous build's files (an image, directory or archive). */
async function previousBuild(arg: string | undefined): Promise<Tree | undefined> {
  if (arg === undefined) return undefined;
  let store: Promise<Store> | undefined;
  const open = (): Promise<Store> => (store ??= Store.open());
  return loadTarget(await resolveTarget(arg, open), open);
}

/** `--manifest`: the exported files' manifest, written with the export's destination rules (and --force). */
async function writeManifest(tree: Tree, file: string, contextDir: string, force: boolean): Promise<void> {
  await exportFile(encodeManifest(manifestOf(tree)), file, { contextDir, force });
}

function positiveInt(raw: string, flag: string): number {
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n <= 0) throw new SitefileError('E_USAGE', `${flag} expects a positive integer`);
  return n;
}

function usageError(message: string): number {
  process.stderr.write(`error: ${message}\nRun 'sitefile --help' for usage.\n`);
  return 2;
}

main(process.argv.slice(2)).then(
  (code) => {
    process.exitCode = code;
  },
  (e: unknown) => {
    process.stderr.write(`internal error: ${(e as Error)?.stack ?? String(e)}\n`);
    process.exitCode = 70;
  },
);
