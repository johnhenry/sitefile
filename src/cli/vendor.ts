import { NETWORK_OPTIONS, networkHelp, networkOption } from './network.js';
import path from 'node:path';
import { insecureRegistriesFromEnv } from '../base-image.js';
import { collectBuildArgs } from '../build-args.js';
import { DEFAULT_FETCH_POLICY } from '../resources/types.js';
import { vendor } from '../vendor.js';
import { loadSecrets } from '../secrets.js';
import { requireArgs, str, strs, usage, type Command } from './common.js';

function positive(raw: string | undefined, flag: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n <= 0) usage(`${flag} expects a positive integer`);
  return n;
}

const plural = (n: number, word: string): string => `${n} ${word}${n === 1 ? '' : 's'}`;

const vendorCommand: Command = {
  name: 'vendor',
  summary: 'download remote imports and the BASE image for offline builds',
  usage: 'vendor [context] [options]',
  description:
    "Fetch every remote IMPORT and the BASE image (EXPORT ... AS oci BASE, or --base) as a build would, verify\nthem against INTEGRITY and Sitefile.lock (which is updated like a build updates it), and write them to\nthe vendor directory: content-addressed blobs (blobs/sha256/<hex>) and manifest.json. Later builds\nread vendored copies instead of the network (verified again) and never copy the directory into the\noutput. The directory is replaced as a whole; one that is not a vendor directory is refused.",
  flags: [
    '-f, --file <path>             Sitefile path (default: <context>/Sitefile)',
    '--vendor-dir <dir>            vendor directory, relative to the context (default vendor/sitefile)',
    '--build-arg <NAME=value>      set an ARG (repeatable); --build-arg NAME reads $NAME',
    '--build-arg-file <path>       read ARGs from a .json, .yaml/.yml or .env file (repeatable)',
    '--secret id=<id>,env=<VAR>    a build secret for IMPORT ... SECRET (or id=<id>,src=<file>; repeatable)',
    '--frozen-lockfile             fail instead of changing Sitefile.lock',
    '--update-lock                 re-record unpinned URLs whose content changed, re-resolve BASE tags',
    '--allow-http                  permit plain http:// imports',
    '--allow-private-network       permit loopback/private network addresses',
    `--max-download-bytes <n>      per-resource size limit (default ${DEFAULT_FETCH_POLICY.maxBytes})`,
    `--timeout <ms>                per-resource fetch timeout (default ${DEFAULT_FETCH_POLICY.timeoutMs})`,
    ...networkHelp(30),
    '--base <image>                vendor this BASE image (overrides EXPORT ... AS oci BASE)',
    '--platform <os/arch>          BASE platform to vendor (default linux/amd64)',
    '--insecure-registry <h>       allow plain http to registry host[:port] (repeatable)',
  ],
  options: {
    ...NETWORK_OPTIONS,
    file: { type: 'string', short: 'f' },
    'vendor-dir': { type: 'string' },
    'build-arg': { type: 'string', multiple: true },
    'build-arg-file': { type: 'string', multiple: true },
    secret: { type: 'string', multiple: true },
    'frozen-lockfile': { type: 'boolean' },
    'update-lock': { type: 'boolean' },
    'allow-http': { type: 'boolean' },
    'allow-private-network': { type: 'boolean' },
    'max-download-bytes': { type: 'string' },
    timeout: { type: 'string' },
    base: { type: 'string' },
    platform: { type: 'string' },
    'insecure-registry': { type: 'string', multiple: true },
  },
  async run(v, pos) {
    requireArgs(pos, 0, 1, 'a context directory');
    if (v['frozen-lockfile'] && v['update-lock']) usage('--frozen-lockfile and --update-lock conflict');
    const context = pos[0] ?? '.';
    const maxBytes = positive(str(v['max-download-bytes']), '--max-download-bytes');
    const timeoutMs = positive(str(v.timeout), '--timeout');
    const network = networkOption(v);
    const file = str(v.file);
    const vendorDir = str(v['vendor-dir']);
    const base = str(v.base);
    const platform = str(v.platform);
    const r = await vendor({
      contextDir: context,
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
      ...(vendorDir !== undefined ? { vendorDir } : {}),
      ...(base !== undefined ? { baseImage: base } : {}),
      ...(platform !== undefined ? { platform } : {}),
      registry: { insecureRegistries: insecureRegistriesFromEnv(strs(v['insecure-registry'])) },
      frozenLockfile: !!v['frozen-lockfile'],
      updateLock: !!v['update-lock'],
      onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
    });
    const where = path.relative(process.cwd(), r.dir) || '.';
    process.stdout.write(`Vendored ${plural(r.resources, 'resource')} and ${plural(r.images, 'image')} (${plural(r.blobs, 'blob')}, ${r.bytes} bytes) to ${where}\n`);
    if (r.lockfileWritten) process.stdout.write('Updated Sitefile.lock\n');
    return 0;
  },
};

export const vendorCommands: ReadonlyMap<string, Command> = new Map([['vendor', vendorCommand]]);
