import { NETWORK_OPTIONS, networkHelp, networkOption } from './network.js';
import { insecureRegistriesFromEnv } from '../base-image.js';
import { collectBuildArgs } from '../build-args.js';
import { outdated, type OutdatedReport, type OutdatedStatus } from '../outdated.js';
import { loadSecrets } from '../secrets.js';
import { DEFAULT_FETCH_POLICY } from '../resources/types.js';
import { shortId } from '../store.js';
import { requireArgs, str, strs, table, usage, type Command } from './common.js';

function positive(raw: string | undefined, flag: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n <= 0) usage(`${flag} expects a positive integer`);
  return n;
}

/** The order statuses are counted in the summary line. */
const ORDER: readonly OutdatedStatus[] = ['changed', 'unpinned', 'error', 'digest-only', 'current'];

/** The text report: a table, one line per error, and a summary line. */
export function formatOutdated(r: OutdatedReport): string {
  if (r.inputs.length === 0) return 'No remote inputs: the Sitefile has no remote IMPORT, no registry FROM image and no BASE image\n';
  const short = (d: string | null): string => (d === null ? '-' : shortId(d));
  const rows = [['KIND', 'NAME', 'SOURCE', 'PINNED', 'BY', 'LATEST', 'STATUS', 'NEWER TAG']];
  for (const i of r.inputs) rows.push([i.kind, i.name, i.source, short(i.pin), i.pinnedBy ?? '-', short(i.latest), i.status, i.newerTag ?? '-']);
  let out = table(rows);
  for (const i of r.inputs) if (i.error) out += `${i.name}: error[${i.error.code}]: ${i.error.message}\n`;
  const n = r.inputs.length;
  const counts = ORDER.filter((s) => r.summary[s] > 0).map((s) => `${r.summary[s]} ${s === 'error' && r.summary[s] !== 1 ? 'errors' : s}`);
  return `${out}${n} remote input${n === 1 ? '' : 's'}: ${counts.join(', ')}\n`;
}

const outdatedCommand: Command = {
  name: 'outdated',
  summary: 'check whether pinned remote imports and BASE images have changed upstream',
  usage: 'outdated [context] [options]',
  description:
    "Report, for every remote IMPORT and the BASE image, what it is pinned to (INTEGRITY, @sha256: or\nSitefile.lock) and what it resolves to now. Never changes anything: use 'sitefile build --update-lock'\nto accept new content. An https URL is fetched again and its sha256 compared (there is no notion of a\nnewer version of a URL); a BASE tag is resolved again (changed = the tag moved), and for a version-like\ntag (1.2, v1.2.3-alpine) the registry's tag list is searched for a greater tag of the same shape.",
  flags: [
    '-f, --file <path>             Sitefile path (default: <context>/Sitefile)',
    '--json                        machine-readable output',
    '--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME',
    '--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)',
    '--secret id=<id>,env=<VAR>    a build secret for IMPORT ... SECRET (or id=<id>,src=<file>; repeatable)',
    '--no-lockfile                 ignore Sitefile.lock (only INTEGRITY and @sha256: count as pins)',
    '--allow-http                  permit plain http:// imports',
    '--allow-private-network       permit loopback/private network addresses',
    `--max-download-bytes <n>      per-resource size limit (default ${DEFAULT_FETCH_POLICY.maxBytes})`,
    `--timeout <ms>                per-resource fetch timeout (default ${DEFAULT_FETCH_POLICY.timeoutMs})`,
    ...networkHelp(30),
    '--base <image>                check this BASE image (overrides EXPORT ... AS oci BASE)',
    '--insecure-registry <h>       allow plain http to registry host[:port] (repeatable)',
  ],
  options: {
    ...NETWORK_OPTIONS,
    file: { type: 'string', short: 'f' },
    json: { type: 'boolean' },
    'build-arg': { type: 'string', multiple: true },
    'build-arg-file': { type: 'string', multiple: true },
    secret: { type: 'string', multiple: true },
    'no-lockfile': { type: 'boolean' },
    'allow-http': { type: 'boolean' },
    'allow-private-network': { type: 'boolean' },
    'max-download-bytes': { type: 'string' },
    timeout: { type: 'string' },
    base: { type: 'string' },
    'insecure-registry': { type: 'string', multiple: true },
  },
  async run(v, pos) {
    requireArgs(pos, 0, 1, 'a context directory');
    const maxBytes = positive(str(v['max-download-bytes']), '--max-download-bytes');
    const timeoutMs = positive(str(v.timeout), '--timeout');
    const network = networkOption(v);
    const file = str(v.file);
    const base = str(v.base);
    const r = await outdated({
      contextDir: pos[0] ?? '.',
      ...(file ? { sitefile: file } : {}),
      buildArgs: await collectBuildArgs(strs(v['build-arg-file']), strs(v['build-arg'])),
      secrets: await loadSecrets(strs(v.secret)),
      fetchPolicy: {
        allowHttp: !!v['allow-http'],
        allowPrivateNetwork: !!v['allow-private-network'],
        ...(maxBytes ? { maxBytes } : {}),
        ...(timeoutMs ? { timeoutMs } : {}),
        ...(network ? { network } : {}),
      },
      ...(v['no-lockfile'] ? { lockfile: false as const } : {}),
      ...(base !== undefined ? { baseImage: base } : {}),
      registry: { insecureRegistries: insecureRegistriesFromEnv(strs(v['insecure-registry'])) },
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
    });
    process.stdout.write(v.json ? JSON.stringify(r, null, 2) + '\n' : formatOutdated(r));
    return 0;
  },
};

export const outdatedCommands: ReadonlyMap<string, Command> = new Map([['outdated', outdatedCommand]]);
