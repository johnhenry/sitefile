#!/usr/bin/env node
import { parseArgs } from 'node:util';
import path from 'node:path';
import { SitefileError, isSitefileError } from './errors.js';
import { build, buildToDirectory, planBuild, type BuildOptions } from './executor.js';
import { defaultOutName } from './export-formats.js';
import { EXPORT_FORMATS, parseExportFormat, type ExportFormat } from './language/ast.js';
import { DEFAULT_FETCH_POLICY } from './resources/types.js';

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
      --format <fmt>           directory | zip | tar | tgz (alias tar.gz) | wbn; overrides the
                               Sitefile's EXPORT ... AS <fmt> (build only)
      --base-url <url>         https URL ending in '/' the files are mounted at (wbn only);
                               overrides EXPORT ... BASE
      --build-arg <NAME=value> set an ARG (repeatable)
      --force                  replace a non-empty --out outside the context
      --frozen-lockfile        fail instead of changing Sitefile.lock
      --update-lock            re-record unpinned URLs whose content changed
      --no-lockfile            do not read or write Sitefile.lock
      --allow-http             permit plain http:// imports
      --allow-private-network  permit loopback/private network addresses
      --allow-hardlinks        permit hard-linked files in the context (you assert they are safe)
      --max-download-bytes <n> per-resource size limit (default ${DEFAULT_FETCH_POLICY.maxBytes})
      --timeout <ms>           per-resource fetch timeout (default ${DEFAULT_FETCH_POLICY.timeoutMs})
      --json                   machine-readable output (validate, inspect)
  -h, --help                   show this help
  -v, --version                show the version
`;

export async function main(argv: readonly string[]): Promise<number> {
  let parsed;
  try {
    parsed = parseArgs({
      args: [...argv],
      allowPositionals: true,
      options: {
        file: { type: 'string', short: 'f' },
        out: { type: 'string', short: 'o' },
        format: { type: 'string' },
        'base-url': { type: 'string' },
        'build-arg': { type: 'string', multiple: true },
        force: { type: 'boolean' },
        'frozen-lockfile': { type: 'boolean' },
        'update-lock': { type: 'boolean' },
        'no-lockfile': { type: 'boolean' },
        'allow-http': { type: 'boolean' },
        'allow-private-network': { type: 'boolean' },
        'allow-hardlinks': { type: 'boolean' },
        'max-download-bytes': { type: 'string' },
        timeout: { type: 'string' },
        json: { type: 'boolean' },
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

  try {
    const buildArgs: Record<string, string> = {};
    for (const kv of o['build-arg'] ?? []) {
      const eq = kv.indexOf('=');
      if (eq <= 0) return usageError(`--build-arg expects NAME=value, got '${kv}'`);
      buildArgs[kv.slice(0, eq)] = kv.slice(eq + 1);
    }
    let formatOverride: ExportFormat | undefined;
    if (o.format !== undefined) {
      formatOverride = parseExportFormat(o.format);
      if (!formatOverride) return usageError(`--format expects one of ${EXPORT_FORMATS.join(', ')} (or tar.gz), got '${o.format}'`);
    }
    // The default destination depends on the format, which the Sitefile may set.
    const sitefileFormat = command === 'build' && !o.out && !formatOverride ? (await planBuild({ contextDir: context, ...(o.file ? { sitefile: o.file } : {}), buildArgs })).exportFormat : undefined;
    const out = o.out ?? path.join(context, defaultOutName(formatOverride ?? sitefileFormat ?? 'directory'));
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
      allowHardlinks: !!o['allow-hardlinks'],
      frozenLockfile: !!o['frozen-lockfile'],
      updateLock: !!o['update-lock'],
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
    };

    switch (command) {
      case 'build': {
        const r = await buildToDirectory({
          ...options,
          outDir: out,
          force: !!o.force,
          ...(formatOverride ? { format: formatOverride } : {}),
          ...(o['base-url'] !== undefined ? { baseUrl: o['base-url'] } : {}),
        });
        const where = path.relative(process.cwd(), r.outDir) || '.';
        const n = `${r.output.size} file${r.output.size === 1 ? '' : 's'}`;
        process.stdout.write(
          r.format === 'directory'
            ? `Exported ${n} to ${where} (${r.output.digest()})\n`
            : `Exported ${n} to ${where} (${r.format === 'tgz' ? 'tar.gz' : r.format}, ${r.bytes} bytes, ${r.fileDigest})\n`,
        );
        if (r.lockfileWritten) process.stdout.write('Updated Sitefile.lock\n');
        return 0;
      }
      case 'validate': {
        const p = await planBuild(options);
        p.warnings.forEach((m) => process.stderr.write(`warning: ${m}\n`));
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
