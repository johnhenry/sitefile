import path from 'node:path';
import { CACHE_MAX_ENV, cacheLimitFromEnv, cacheUsage, enforceCacheLimit, formatLimit, parseSize } from '../cache-limit.js';
import { TransformCache } from '../plugins/cache.js';
import { resolveStoreHome } from '../store.js';
import { humanSize, requireArgs, str, table, usage, type Command } from './common.js';

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
  usage: 'cache prune [--older-than <days> | --max-size <size>]',
  description:
    'Delete every cached TRANSFORM result, or only entries not used for the given number of days, and the blobs no\nentry still uses. With --max-size, instead apply the automatic size limit now with that size: least recently\nused evictable data (TRANSFORM results, compiled wasm modules, untagged images and unused blobs) is deleted\nuntil the rest fits. Tagged images are never touched.',
  flags: ['    --older-than <days>   keep entries used within this many days', '    --max-size <size>     shrink evictable data to this size (e.g. 500M, 2G)'],
  options: { 'older-than': { type: 'string' }, 'max-size': { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const raw = v['older-than'];
    const max = str(v['max-size']);
    if (max !== undefined) {
      if (raw !== undefined) usage('--older-than and --max-size conflict');
      const n = parseSize(max);
      if (n === undefined || n === 0) usage('--max-size expects a size greater than 0, such as 500M or 2G');
      const r = await enforceCacheLimit(resolveStoreHome(), n);
      process.stdout.write(`Evicted ${r.items} item${r.items === 1 ? '' : 's'}\nTotal reclaimed space: ${humanSize(r.bytes)}\n`);
      return 0;
    }
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

const du: Command = {
  name: 'cache du',
  summary: 'show the size of every cache under $SITEFILE_HOME',
  usage: 'cache du [--json]',
  description:
    'Print the size of each cache and store under $SITEFILE_HOME, how much of it may be evicted automatically\n(EVICTABLE), and the size limit ($SITEFILE_CACHE_MAX). Alias: cache ls.',
  flags: ['    --json   machine-readable output (sizes in bytes; limit 0 = unlimited)'],
  options: { json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const home = resolveStoreHome();
    const limit = cacheLimitFromEnv(process.env, (m) => process.stderr.write(`warning: ${m}\n`));
    const caches = await cacheUsage(home);
    const bytes = caches.reduce((a, c) => a + c.bytes, 0);
    const evictable = caches.reduce((a, c) => a + c.evictable, 0);
    if (v['json']) {
      process.stdout.write(JSON.stringify({ home, limit, caches, bytes, evictable }, null, 2) + '\n');
      return 0;
    }
    const rows = [
      ['CACHE', 'ITEMS', 'SIZE', 'EVICTABLE', 'PATH'],
      ...caches.map((c) => [c.name, String(c.items), humanSize(c.bytes), humanSize(c.evictable), c.path]),
      ['total', '', humanSize(bytes), humanSize(evictable), home],
    ];
    process.stdout.write(table(rows));
    process.stdout.write(`Limit: ${formatLimit(limit)} (${CACHE_MAX_ENV}; evictable data over it is deleted after each build, least recently used first)\n`);
    return 0;
  },
};

/**
 * End of `build`/`inspect`: apply $SITEFILE_CACHE_MAX. Never fails: problems are warnings.
 */
export async function evictAfterBuild(env: NodeJS.ProcessEnv = process.env): Promise<void> {
  const warn = (m: string): void => void process.stderr.write(`warning: ${m}\n`);
  try {
    const limit = cacheLimitFromEnv(env, warn);
    if (limit === 0) return;
    const r = await enforceCacheLimit(resolveStoreHome(env), limit);
    if (r.items > 0) {
      process.stderr.write(`Evicted ${r.items} cache item${r.items === 1 ? '' : 's'} (${humanSize(r.bytes)}) to stay under ${CACHE_MAX_ENV}=${formatLimit(limit)}\n`);
    }
  } catch (e) {
    warn(`cache size limit not applied: ${(e as Error).message}`);
  }
}

export const cacheSubcommands: ReadonlyMap<string, Command> = new Map([
  ['info', info],
  ['prune', prune],
  ['du', du],
  ['ls', du],
]);
