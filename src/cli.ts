#!/usr/bin/env node
import { parseArgs } from 'node:util';
import path from 'node:path';
import { SitefileError, isSitefileError } from './errors.js';
import { build, buildToDirectory, checkPlanPins, planBuild, type BuildOptions } from './executor.js';
import { defaultOutName, formatFromExtension } from './export-formats.js';
import { collectBuildArgs } from './build-args.js';
import { insecureRegistriesFromEnv } from './base-image.js';
import { parseLabels } from './oci.js';
import { EXPORT_FORMATS, parseExportFormat, parseOciCompression, type ExportFormat, type OciCompression } from './language/ast.js';
import { DEFAULT_FETCH_POLICY } from './resources/types.js';
import { parseLocalRef, refKey } from './ref.js';
import { EXTRA_HELP, isExtraCommand, runExtraCommand } from './cli/index.js';
import { evictAfterBuild, transformCacheDir } from './cli/cache.js';
import { storeBuildAsImage, validateImageTags } from './cli/build-image.js';
import { watchLoop } from './cli/watch.js';

const VERSION = '0.1.0';

const HELP = `sitefile ${VERSION} — Docker-inspired static artifact builder

Usage:
  sitefile build [context] [options]     build and export to --out (default: <context>/dist, or dist.<ext>)
  sitefile validate [context] [options]  parse, validate and evaluate variables (no IO)
  sitefile inspect [context] [options]   build in memory and list the exported files

Options:
  -f, --file <path>            Sitefile path (default: <context>/Sitefile)
  -o, --out <path>             export destination (build only): a directory for 'directory',
                               a file for zip, tar, tgz, wbn
  -t, --tag <name[:tag]>       also store the build as a local image (build only, repeatable);
                               with -t and no -o nothing is written to the context
      --format <fmt>           directory | zip | tar | tgz (alias tar.gz) | wbn | oci (build only).
                               oci writes an OCI archive for -o *.tar (e.g. site.oci.tar), else an
                               OCI image layout directory.
                               Default: the --out extension (.zip .tar .tar.gz .tgz .wbn .oci.tar),
                               else the Sitefile's EXPORT ... AS <fmt>
      --oci-root <path>        directory inside an oci image layer (default /srv/www; overrides EXPORT ... AT)
      --oci-compression <c>    oci site layer: gzip (default) or none (plain tar; overrides EXPORT ... COMPRESSION)
      --label <key=value>      add an OCI image config label (repeatable)
      --git-labels             add org.opencontainers.image.source/revision from the git checkout
                               (off by default: they change the image ID on every commit)
      --platform <os/arch>     oci image platform (default linux/amd64); with a BASE, the platform pulled
      --base <image>           build the oci image on <image> (runnable; overrides EXPORT ... AS oci BASE);
                               a bare name means Docker Hub. Pin it: nginx:alpine@sha256:...
      --insecure-registry <h>  allow plain http to registry host[:port] (repeatable; localhost always may)
      --base-url <url>         https URL ending in '/' the files are mounted at (wbn only);
                               overrides EXPORT ... BASE
      --build-arg <NAME=value> set an ARG (repeatable); --build-arg NAME reads $NAME
      --plugin-timeout <ms>    per-call time limit for TRANSFORM plugins and tools (default 60000)
      --allow-exec <name[=path]> allow a TOOL (external program) to run, resolved on PATH or at
                               the absolute path (repeatable); programs run with your privileges
      --allow-plugin <name>    allow a JavaScript PLUGIN to run (repeatable; wasm plugins need no flag)
      --allow-plugins          allow every JavaScript PLUGIN in the Sitefile
      --no-cache               do not read or write the TRANSFORM result cache
                               ($SITEFILE_HOME/cache/transform; see 'sitefile cache --help')
      --build-arg-file <path>  read ARGs from a .json, .yaml/.yml or .env file (repeatable;
                               later files win, --build-arg wins over files)
      --force                  replace a non-empty --out outside the context
      --frozen-lockfile        fail instead of changing Sitefile.lock
      --update-lock            re-record unpinned URLs whose content changed
      --strict-pins            fail when a remote IMPORT has no INTEGRITY and no lockfile entry, or the
                               BASE image is neither @sha256:-pinned nor locked (build, validate, inspect)
      --no-lockfile            do not read or write Sitefile.lock
      --allow-http             permit plain http:// imports
      --allow-private-network  permit loopback/private network addresses
      --allow-hardlinks        permit hard-linked files in the context (you assert they are safe)
      --max-download-bytes <n> per-resource size limit (default ${DEFAULT_FETCH_POLICY.maxBytes})
      --timeout <ms>           per-resource fetch timeout (default ${DEFAULT_FETCH_POLICY.timeoutMs})
      --json                   machine-readable output (validate, inspect)
      --watch                  build, then rebuild when the context, the Sitefile or a
                               --build-arg-file changes (build only; Ctrl-C stops)
  -h, --help                   show this help
  -v, --version                show the version
${EXTRA_HELP}`;

/**
 * Run the CLI. `run.once` runs a --watch command a single time (the watch loop's
 * builds); `run.wrote` then receives the export destination.
 */
export async function main(argv: readonly string[], run: { readonly once?: boolean; readonly wrote?: string[] } = {}): Promise<number> {
  if (isExtraCommand(argv[0])) return runExtraCommand(argv);
  let parsed;
  try {
    parsed = parseArgs({
      args: [...argv],
      allowPositionals: true,
      options: {
        file: { type: 'string', short: 'f' },
        out: { type: 'string', short: 'o' },
        tag: { type: 'string', short: 't', multiple: true },
        format: { type: 'string' },
        'base-url': { type: 'string' },
        'oci-root': { type: 'string' },
        'oci-compression': { type: 'string' },
        label: { type: 'string', multiple: true },
        'git-labels': { type: 'boolean' },
        platform: { type: 'string' },
        base: { type: 'string' },
        'insecure-registry': { type: 'string', multiple: true },
        'build-arg': { type: 'string', multiple: true },
        'build-arg-file': { type: 'string', multiple: true },
        'plugin-timeout': { type: 'string' },
        'allow-exec': { type: 'string', multiple: true },
        'allow-plugin': { type: 'string', multiple: true },
        'allow-plugins': { type: 'boolean' },
        'no-cache': { type: 'boolean' },
        force: { type: 'boolean' },
        'frozen-lockfile': { type: 'boolean' },
        'update-lock': { type: 'boolean' },
        'strict-pins': { type: 'boolean' },
        'no-lockfile': { type: 'boolean' },
        'allow-http': { type: 'boolean' },
        'allow-private-network': { type: 'boolean' },
        'allow-hardlinks': { type: 'boolean' },
        'max-download-bytes': { type: 'string' },
        timeout: { type: 'string' },
        json: { type: 'boolean' },
        watch: { type: 'boolean' },
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
  const [command, context = '.', ...extra] = positionals;
  if (o.help || !command) {
    process.stdout.write(HELP);
    return o.help ? 0 : 2;
  }
  if (extra.length) return usageError(`unexpected arguments: ${extra.join(' ')}`);
  if (o['frozen-lockfile'] && o['update-lock']) return usageError('--frozen-lockfile and --update-lock conflict');
  if (o['strict-pins'] && o['update-lock']) return usageError('--strict-pins and --update-lock conflict');
  if (o.watch && !run.once) {
    if (command !== 'build') return usageError(`--watch is only for build (and serve --watch), not ${command}`);
    if ((o.tag ?? []).length) return usageError('--watch cannot be combined with -t');
    try {
      return await watchLoop({
        context,
        ...(o.file ? { sitefile: o.file } : {}),
        argFiles: o['build-arg-file'] ?? [],
        build: async () => {
          const wrote: string[] = [];
          return { code: await main(argv, { once: true, wrote }), wrote };
        },
      });
    } catch (e) {
      if (!isSitefileError(e)) throw e;
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      return 1;
    }
  }

  try {
    // Precedence: Sitefile defaults < --build-arg-file (later files win) < --build-arg.
    const buildArgs = await collectBuildArgs(o['build-arg-file'] ?? [], o['build-arg'] ?? []);
    let formatOverride: ExportFormat | undefined;
    if (o.format !== undefined) {
      formatOverride = parseExportFormat(o.format);
      if (!formatOverride) return usageError(`--format expects one of ${EXPORT_FORMATS.join(', ')} (or tar.gz), got '${o.format}'`);
    }
    let ociCompression: OciCompression | undefined;
    if (o['oci-compression'] !== undefined) {
      ociCompression = parseOciCompression(o['oci-compression']);
      if (!ociCompression) return usageError(`--oci-compression expects gzip or none, got '${o['oci-compression']}'`);
    }
    // No --format: an archive extension on --out picks the format, so `-o site.zip`
    // writes a zip instead of a directory named site.zip.
    if (!formatOverride && o.out) formatOverride = formatFromExtension(o.out);
    // The default destination depends on the format, which the Sitefile may set.
    const sitefileFormat = command === 'build' && !o.out && !formatOverride ? (await planBuild({ contextDir: context, ...(o.file ? { sitefile: o.file } : {}), buildArgs })).exportFormat : undefined;
    const out = o.out ?? path.join(context, defaultOutName(formatOverride ?? sitefileFormat ?? 'directory'));
    run.wrote?.push(out);
    const options: BuildOptions = {
      contextDir: context,
      ...(o.file ? { sitefile: o.file } : {}),
      buildArgs,
      outDir: out,
      fetchPolicy: {
        allowHttp: !!o['allow-http'],
        allowPrivateNetwork: !!o['allow-private-network'],
        ...(o['max-download-bytes'] ? { maxBytes: positiveInt(o['max-download-bytes'], '--max-download-bytes') } : {}),
        ...(o.timeout ? { timeoutMs: positiveInt(o.timeout, '--timeout') } : {}),
      },
      ...(o['no-lockfile'] ? { lockfile: false as const } : {}),
      ...(o['plugin-timeout']
        ? { pluginPolicy: { timeoutMs: positiveInt(o['plugin-timeout'], '--plugin-timeout') }, toolPolicy: { timeoutMs: positiveInt(o['plugin-timeout'], '--plugin-timeout') } }
        : {}),
      ...(o['allow-exec'] ? { allowExec: o['allow-exec'] } : {}),
      ...(o['allow-plugin'] ? { allowPlugin: o['allow-plugin'] } : {}),
      allowAllPlugins: !!o['allow-plugins'],
      ...(o['no-cache'] ? {} : { cacheDir: transformCacheDir() }),
      onNotice: (m) => process.stderr.write(`${m}\n`),
      allowHardlinks: !!o['allow-hardlinks'],
      ...(o.platform !== undefined ? { platform: o.platform } : {}),
      ...(o.base !== undefined ? { baseImage: o.base } : {}),
      // inspect never pulls; a non-oci export (with no image tag) has no use for a BASE
      ...(command === 'inspect' || (command === 'build' && formatOverride !== undefined && formatOverride !== 'oci' && !(o.tag ?? []).length) ? { skipBase: true } : {}),
      registry: { insecureRegistries: insecureRegistriesFromEnv(o['insecure-registry'] ?? []) },
      frozenLockfile: !!o['frozen-lockfile'],
      updateLock: !!o['update-lock'],
      strictPins: !!o['strict-pins'],
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
    };

    switch (command) {
      case 'build': {
        const tags = o.tag ?? [];
        validateImageTags(tags);
        const labels = parseLabels(o.label ?? []);
        const imageInfo = { contextDir: context, ociRoot: o['oci-root'], ociCompression, platform: o.platform, labels, gitLabels: !!o['git-labels'] };
        if (tags.length && !o.out) {
          // -t without -o: store the image only; nothing is written to the context.
          await storeBuildAsImage(await build(options), tags, imageInfo);
          return 0;
        }
        const r = await buildToDirectory({
          ...options,
          outDir: out,
          force: !!o.force,
          ...(formatOverride ? { format: formatOverride } : {}),
          ...(o['base-url'] !== undefined ? { baseUrl: o['base-url'] } : {}),
          ...(o['oci-root'] !== undefined ? { ociRoot: o['oci-root'] } : {}),
          ...(ociCompression !== undefined ? { ociCompression } : {}),
          ...(o.platform !== undefined ? { platform: o.platform } : {}),
          imageLabels: labels,
          gitLabels: !!o['git-labels'],
          ...(tags[0] ? { ociRefName: refKey(parseLocalRef(tags[0])) } : {}),
        });
        const where = path.relative(process.cwd(), r.outDir) || '.';
        const n = `${r.output.size} file${r.output.size === 1 ? '' : 's'}`;
        process.stdout.write(
          r.format === 'directory'
            ? `Exported ${n} to ${where} (${r.output.digest()})\n`
            : r.format === 'oci'
              ? `Exported ${n} to ${where} (oci image ${r.manifestDigest}${r.bytes !== undefined ? `, ${r.bytes} bytes` : ''})\n`
              : `Exported ${n} to ${where} (${r.format === 'tgz' ? 'tar.gz' : r.format}, ${r.bytes} bytes, ${r.fileDigest})\n`,
        );
        if (r.lockfileWritten) process.stdout.write('Updated Sitefile.lock\n');
        if (tags.length) await storeBuildAsImage(r, tags, imageInfo);
        return 0;
      }
      case 'validate': {
        const p = await planBuild(options);
        p.warnings.forEach((m) => process.stderr.write(`warning: ${m}\n`));
        if (o['strict-pins']) await checkPlanPins(options, p);
        if (o.json) {
          process.stdout.write(
            JSON.stringify(
              { file: p.file, vars: Object.fromEntries(p.vars), imports: p.imports, stages: p.stages, export: p.exportPath },
              null,
              2,
            ) + '\n',
          );
        } else {
          const opCount = p.stages.reduce((n, st) => n + st.operations.length, 0);
          process.stdout.write(
            `${p.file}: OK (${p.imports.length} import${p.imports.length === 1 ? '' : 's'}, ${opCount} operation${opCount === 1 ? '' : 's'}${p.stages.length > 1 ? `, ${p.stages.length} stages` : ''})\n`,
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
      default:
        return usageError(`unknown command '${command}'`);
    }
  } catch (e) {
    if (isSitefileError(e)) {
      if (e.code === 'E_USAGE') return usageError(e.detail);
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      return 1;
    }
    throw e;
  } finally {
    // Keep $SITEFILE_HOME under $SITEFILE_CACHE_MAX (never fails the command).
    if (command === 'build' || command === 'inspect') await evictAfterBuild();
  }
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
