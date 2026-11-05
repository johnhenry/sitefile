import path from 'node:path';
import { TransformCache } from '../plugins/cache.js';
import { resolveStoreHome } from '../store.js';
import { humanSize, requireArgs, usage, type Command } from './common.js';

/** Where TRANSFORM results are cached: beside the image store, in `$SITEFILE_HOME/cache/transform`. */
export function transformCacheDir(env: NodeJS.ProcessEnv = process.env): string {
  return path.join(resolveStoreHome(env), 'cache', 'transform');
}

const info: Command = {
  name: 'cache info',
  summary: 'show the TRANSFORM result cache location and size',
  usage: 'cache info [--json]',
  description: 'Print where plugin/tool results are cached, and how many entries and bytes it holds.',
  flags: ['    --json   machine-readable output'],
  options: { json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const i = await new TransformCache(transformCacheDir()).info();
    process.stdout.write(
      v['json'] ? JSON.stringify(i, null, 2) + '\n' : `Cache:   ${i.dir}\nEntries: ${i.entries}\nBlobs:   ${i.blobs}\nSize:    ${humanSize(i.bytes)}\n`,
    );
    return 0;
  },
};

const prune: Command = {
  name: 'cache prune',
  summary: 'delete cached TRANSFORM results',
  usage: 'cache prune [--older-than <days>]',
  description: 'Delete every cached result, or only entries not used for the given number of days, and the blobs no entry still uses.',
  flags: ['    --older-than <days>   keep entries used within this many days'],
  options: { 'older-than': { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const raw = v['older-than'];
    let ms = 0;
    if (typeof raw === 'string') {
      const days = Number(raw);
      if (!Number.isFinite(days) || days <= 0) usage('--older-than expects a positive number of days');
      ms = days * 86_400_000;
    }
    const r = await new TransformCache(transformCacheDir()).prune(ms);
    process.stdout.write(`Deleted ${r.entries} entr${r.entries === 1 ? 'y' : 'ies'} and ${r.blobs} blob${r.blobs === 1 ? '' : 's'}\nTotal reclaimed space: ${humanSize(r.bytes)}\n`);
    return 0;
  },
};

export const cacheSubcommands: ReadonlyMap<string, Command> = new Map([
  ['info', info],
  ['prune', prune],
]);
