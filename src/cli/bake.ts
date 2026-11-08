import { spawn } from 'node:child_process';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { bakePlan, bakePlanJson, bakePlanText, buildArgv, findBakeFile, type BakeTarget } from '../bake.js';
import { CACHE_MAX_ENV } from '../cache-limit.js';
import { evictAfterBuild } from './cache.js';
import { str, strs, usage, type Command, type ParsedValues } from './common.js';

/** The CLI entry the targets run (lib/src/cli.js). */
const CLI = fileURLToPath(new URL('../cli.js', import.meta.url));

const DEFAULT_JOBS = 4;

/** Flags passed unchanged to every target's build. */
const PASS_BOOLEANS = ['allow-plugins', 'no-cache', 'strict-pins', 'frozen-lockfile', 'update-lock', 'no-lockfile', 'allow-http', 'allow-private-network', 'allow-hardlinks', 'keep-os-metadata', 'force'];
const PASS_STRINGS = ['vendor-dir', 'max-download-bytes', 'timeout', 'plugin-timeout'];
const PASS_LISTS = ['allow-exec', 'allow-plugin', 'cache-from', 'insecure-registry', 'build-arg-file', 'build-arg'];

function positive(raw: string | undefined, flag: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n <= 0) usage(`${flag} expects a positive integer`);
  return n;
}

/** The flags every target's build gets, in a fixed order. */
function passThrough(v: ParsedValues): string[] {
  const out: string[] = [];
  for (const f of PASS_BOOLEANS) if (v[f]) out.push(`--${f}`);
  for (const f of PASS_STRINGS) {
    const s = str(v[f]);
    if (s !== undefined) out.push(`--${f}`, s);
  }
  for (const f of PASS_LISTS) for (const s of strs(v[f])) out.push(`--${f}`, s);
  return out;
}

interface Outcome {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

function runBuild(argv: readonly string[]): Promise<Outcome> {
  return new Promise((resolve) => {
    // Each target evicts nothing; bake applies the cache size limit once, at the end.
    const child = spawn(process.execPath, [CLI, ...argv], { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, [CACHE_MAX_ENV]: '0' } });
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    child.stdout.on('data', (b: Buffer) => out.push(b));
    child.stderr.on('data', (b: Buffer) => err.push(b));
    child.on('error', (e) => resolve({ code: 1, stdout: '', stderr: `error: cannot run the build: ${e.message}\n` }));
    child.on('close', (code) => resolve({ code: code ?? 1, stdout: Buffer.concat(out).toString('utf8'), stderr: Buffer.concat(err).toString('utf8') }));
  });
}

/** Each line of `text` with `[name] ` in front. */
function prefixed(name: string, text: string): string {
  if (text === '') return '';
  const lines = text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return lines.map((l) => `[${name}] ${l}\n`).join('');
}

/** The Sitefile.lock a target's build may write (targets that share one never run at the same time). */
function lockKey(t: BakeTarget, v: ParsedValues): string | undefined {
  if (v['no-lockfile'] || v['frozen-lockfile']) return undefined;
  return path.resolve(path.dirname(t.file), 'Sitefile.lock');
}

const bakeCommand: Command = {
  name: 'bake',
  summary: 'build several variants (targets, matrix, groups) from a bake file',
  usage: 'bake [targets...] [-f sitefile.bake.yaml] [options]',
  description:
    "Build the targets of a bake file (default: sitefile.bake.yaml, .yml or .json in the current directory), like\n'docker buildx bake'. Each target is a 'sitefile build' with its own context, Sitefile, build args, --target\nstage, output and tags; a matrix expands one target into one per combination of values. Name targets or\ngroups to build only those (default: the 'default' group, else every target). Paths in the bake file are\nrelative to it. Up to --jobs builds run at once; each target's output is printed, prefixed with its\nname, in plan order. Every target runs even when one fails; then the exit code is 1.",
  flags: [
    '-f, --file <path>             bake file (default: sitefile.bake.yaml, .yml or .json)',
    '--print                       print the resolved targets instead of building (text, or --json)',
    '--json                        with --print: machine-readable plan',
    `-j, --jobs <n>                builds to run at once (default ${DEFAULT_JOBS})`,
    '--profile <dir>               write each target\'s timing trace to <dir>/<target>.json',
    '--build-arg <NAME=value>      set an ARG in every target (wins over the bake file; repeatable)',
    '--build-arg-file <path>       read ARGs for every target from a file (after the targets\' own; repeatable)',
    'Passed to every build: --allow-exec, --allow-plugin, --allow-plugins, --no-cache, --cache-from, --strict-pins,',
    '  --frozen-lockfile, --update-lock, --no-lockfile, --vendor-dir, --allow-http, --allow-private-network,',
    '  --allow-hardlinks, --keep-os-metadata, --max-download-bytes, --timeout, --plugin-timeout, --insecure-registry, --force',
  ],
  options: {
    file: { type: 'string', short: 'f' },
    print: { type: 'boolean' },
    json: { type: 'boolean' },
    jobs: { type: 'string', short: 'j' },
    profile: { type: 'string' },
    'cache-to': { type: 'string', multiple: true },
    ...Object.fromEntries(PASS_BOOLEANS.map((f) => [f, { type: 'boolean' as const }])),
    ...Object.fromEntries(PASS_STRINGS.map((f) => [f, { type: 'string' as const }])),
    ...Object.fromEntries(PASS_LISTS.map((f) => [f, { type: 'string' as const, multiple: true }])),
  },
  async run(v, pos) {
    if (v.json && !v.print) usage('bake: --json is only for --print');
    if (strs(v['cache-to']).length) usage("bake: --cache-to is not supported (each target would replace the others' results); push from one 'sitefile build --cache-to'");
    if (v['frozen-lockfile'] && v['update-lock']) usage('--frozen-lockfile and --update-lock conflict');
    if (v['strict-pins'] && v['update-lock']) usage('--strict-pins and --update-lock conflict');
    if (v['no-cache'] && strs(v['cache-from']).length) usage('--no-cache cannot be combined with --cache-from or --cache-to');
    const jobs = positive(str(v.jobs), '--jobs') ?? DEFAULT_JOBS;
    const profile = str(v.profile);
    if (profile === '') usage('--profile expects a directory path');
    const plan = await bakePlan(await findBakeFile(str(v.file)), pos);
    if (v.print) {
      process.stdout.write(v.json ? JSON.stringify(bakePlanJson(plan), null, 2) + '\n' : bakePlanText(plan));
      return 0;
    }
    if (profile !== undefined) await mkdir(profile, { recursive: true });
    const extra = passThrough(v);
    const targets = plan.targets;
    const results: Array<Outcome | undefined> = targets.map(() => undefined);
    const busy = new Set<string>();
    const pending = targets.map((_, i) => i);
    let printed = 0;
    const flush = (): void => {
      while (printed < targets.length && results[printed] !== undefined) {
        const r = results[printed]!;
        const name = targets[printed]!.name;
        process.stderr.write(prefixed(name, r.stderr));
        process.stdout.write(prefixed(name, r.stdout));
        printed++;
      }
    };
    await new Promise<void>((done) => {
      if (targets.length === 0) return done();
      let running = 0;
      const pump = (): void => {
        while (running < jobs) {
          // The first pending target whose Sitefile.lock no running build may write.
          const k = pending.findIndex((i) => {
            const key = lockKey(targets[i]!, v);
            return key === undefined || !busy.has(key);
          });
          if (k === -1) break;
          const i = pending.splice(k, 1)[0]!;
          const t = targets[i]!;
          const key = lockKey(t, v);
          if (key !== undefined) busy.add(key);
          running++;
          const argv = [...buildArgv(t), ...extra, ...(profile !== undefined ? ['--profile', path.join(profile, `${t.name}.json`)] : [])];
          void runBuild(argv).then((r) => {
            results[i] = r;
            if (key !== undefined) busy.delete(key);
            running--;
            flush();
            if (running === 0 && pending.length === 0) done();
            else pump();
          });
        }
      };
      pump();
    });
    await evictAfterBuild();
    const failed = targets.filter((_, i) => results[i]!.code !== 0).map((t) => t.name);
    const n = `${targets.length} target${targets.length === 1 ? '' : 's'}`;
    if (failed.length) {
      process.stderr.write(`error[E_BAKE]: ${failed.length} of ${n} failed: ${failed.join(', ')}\n`);
      return 1;
    }
    process.stdout.write(`Baked ${n}\n`);
    return 0;
  },
};

export const bakeCommands: ReadonlyMap<string, Command> = new Map([['bake', bakeCommand]]);
