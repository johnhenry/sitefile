import { promises as fs } from 'node:fs';
import path from 'node:path';
import { SitefileError } from '../errors.js';
import { formatUrl } from '../server.js';
import { isRunning, ServerRegistry, type ServerState } from '../servers.js';
import { findServersFile, loadServersFile, type ServersFile } from '../servers-file.js';
import { Store } from '../store.js';
import { requireArgs, str, table, type Command } from './common.js';
import { archiveSource, graceOf, isFile, parsePublish, resolveSource, restartDetached, serveDetached, stopServer, type DetachedServe } from './servers.js';

/** `^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$`, as serve: an argument that names a path. */
const PATH_LIKE = /^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$/;

/**
 * A servers-file source: a path (`./dist`, `../site.zip`, `/abs`) relative to the file's directory; otherwise
 * an image reference or ID, else an archive file by that name next to the file.
 */
async function fileSource(store: Store, dir: string, src: string): Promise<Pick<DetachedServe, 'label' | 'source'>> {
  if (PATH_LIKE.test(src)) return resolveSource(store, path.resolve(dir, src));
  try {
    return { label: src, source: { kind: 'image', imageId: await store.resolve(src) } };
  } catch (e) {
    const file = path.resolve(dir, src);
    if (e instanceof SitefileError && e.code === 'E_NOT_FOUND' && (await isFile(file))) return archiveSource(file);
    throw e;
  }
}

/** The servers file and its real path (the `project` its servers are recorded with). */
async function open(v: Record<string, unknown>): Promise<{ file: ServersFile; project: string }> {
  const name = await findServersFile(str(v.file as string | undefined));
  const file = await loadServersFile(name);
  return { file, project: await fs.realpath(name) };
}

const sameSource = (a: ServerState['source'], b: ServerState['source']): boolean => JSON.stringify(a) === JSON.stringify(b);
const warn = (m: string): void => void process.stderr.write(`warning: ${m}\n`);

const FILE_FLAG = '-f, --file <path>   servers file (default: sitefile.servers.yaml, .yml or .json here)';

const up: Command = {
  name: 'up',
  summary: 'start the servers of a servers file (detached; idempotent)',
  usage: 'up [-f sitefile.servers.yaml] [--time secs]',
  description:
    "Start every server of the servers file in the background, as serve -d would, named by its key. Running again\nis safe: a server that is running with the same source, address and options is left alone, an exited one is\nrestarted, and one whose entry changed is replaced. Every source is checked before anything starts. A server\nwith the same name that this file did not start is refused (remove it with \"sitefile rm <name>\").",
  flags: [FILE_FLAG, '--time <secs>       grace period before SIGKILL when a server is replaced (default 5)'],
  options: { file: { type: 'string', short: 'f' }, time: { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const grace = graceOf(v);
    const { file, project } = await open(v);
    const store = await Store.open();
    // Resolve everything first: a typo or a missing directory starts nothing.
    const wanted: DetachedServe[] = [];
    for (const s of file.servers) {
      let resolved;
      try {
        resolved = await fileSource(store, file.dir, s.source);
      } catch (e) {
        if (e instanceof SitefileError) throw new SitefileError(e.code, `${file.label}: server '${s.name}': ${e.detail}`, e.span, { cause: e });
        throw e;
      }
      wanted.push({ ...resolved, name: s.name, bind: parsePublishFor(file, s.name, s.publish), spa: s.spa, project });
    }
    const reg = await ServerRegistry.open(store);
    const all = await reg.all();
    for (const w of wanted) {
      const other = all.find((s) => s.name === w.name && s.project !== project);
      if (other) throw new SitefileError('E_SERVER', `${file.label}: a server named '${w.name}' already exists and was not started from this file; remove it with "sitefile rm ${w.name}"`);
    }
    let failed = false;
    for (const w of wanted) {
      try {
        const cur = all.find((s) => s.name === w.name);
        if (cur && sameSource(cur.source, w.source) && cur.spa === w.spa && !cur.keepOsMetadata && cur.bind.host === w.bind.host && cur.bind.port === w.bind.port) {
          if (await isRunning(cur)) {
            process.stdout.write(`${cur.name} (${cur.id}) is up to date at ${formatUrl(cur.host, cur.port)}\n`);
            continue;
          }
          const again = await restartDetached(reg, cur, grace);
          process.stdout.write(`Restarted ${again.name} (${again.id}) serving ${again.image} at ${formatUrl(again.host, again.port)}\n`);
          continue;
        }
        if (cur) {
          await stopServer(reg, cur, grace);
          await reg.remove(cur.id);
        }
        const started = await serveDetached(reg, w, warn);
        process.stdout.write(`${cur ? 'Recreated' : 'Started'} ${started.name} (${started.id}) serving ${w.label} at ${formatUrl(started.host, started.port)}\n`);
      } catch (e) {
        if (!(e instanceof SitefileError)) throw e;
        process.stderr.write(`error[${e.code}]: ${w.name}: ${e.message}\n`);
        failed = true;
      }
    }
    for (const s of all) {
      if (s.project === project && !wanted.some((w) => w.name === s.name)) {
        process.stderr.write(`warning: ${s.name} was started from ${file.label} but is no longer in it; "sitefile down" stops it\n`);
      }
    }
    return failed ? 1 : 0;
  },
};

function parsePublishFor(file: ServersFile, name: string, publish: string | undefined): DetachedServe['bind'] {
  try {
    return parsePublish(publish);
  } catch (e) {
    if (e instanceof SitefileError) throw new SitefileError('E_DATA', `${file.label}: server '${name}': publish: ${e.detail}`);
    throw e;
  }
}

/** The records this file started: its servers in file order, then any no longer in it (oldest first). */
function ofProject(all: readonly ServerState[], file: ServersFile, project: string): ServerState[] {
  const mine = all.filter((s) => s.project === project);
  const named = file.servers.flatMap((s) => mine.filter((m) => m.name === s.name));
  return [...named, ...mine.filter((m) => !named.includes(m))];
}

const down: Command = {
  name: 'down',
  summary: 'stop and remove the servers a servers file started',
  usage: 'down [-f sitefile.servers.yaml] [--time secs]',
  description:
    'Stop (SIGTERM, then SIGKILL after --time seconds) and remove every server that "sitefile up" started from the\nservers file, also ones no longer listed in it. Servers started otherwise are not touched.',
  flags: [FILE_FLAG, '--time <secs>       grace period before SIGKILL (default 5)'],
  options: { file: { type: 'string', short: 'f' }, time: { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const grace = graceOf(v);
    const { file, project } = await open(v);
    const reg = await ServerRegistry.open();
    let failed = false;
    for (const s of ofProject(await reg.all(), file, project)) {
      try {
        await stopServer(reg, s, grace);
        await reg.remove(s.id);
        process.stdout.write(`Removed ${s.name} (${s.id})\n`);
      } catch (e) {
        if (!(e instanceof SitefileError)) throw e;
        process.stderr.write(`error[${e.code}]: ${s.name}: ${e.message}\n`);
        failed = true;
      }
    }
    return failed ? 1 : 0;
  },
};

const ps: Command = {
  name: 'ps',
  summary: 'list the servers of a servers file',
  usage: 'ps [-f sitefile.servers.yaml] [--json]',
  description:
    'List the servers of the servers file in file order: running, exited, or not created (never started by "sitefile up"),\nthen servers it started that are no longer listed in it.',
  flags: [FILE_FLAG, '--json              machine-readable output'],
  options: { file: { type: 'string', short: 'f' }, json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const { file, project } = await open(v);
    const reg = await ServerRegistry.open();
    const all = await reg.all();
    const rows: Array<{ name: string; site: string; url: string; status: string; id: string | null; listed: boolean }> = [];
    for (const s of file.servers) {
      const rec = all.find((x) => x.name === s.name && x.project === project);
      if (!rec) {
        rows.push({ name: s.name, site: s.source, url: '', status: 'not created', id: null, listed: true });
        continue;
      }
      const view = await reg.view(rec);
      rows.push({ name: s.name, site: rec.image, url: view.url, status: view.status, id: rec.id, listed: true });
    }
    for (const rec of ofProject(all, file, project).filter((r) => !file.servers.some((s) => s.name === r.name))) {
      const view = await reg.view(rec);
      rows.push({ name: rec.name, site: rec.image, url: view.url, status: view.status, id: rec.id, listed: false });
    }
    if (v.json) {
      process.stdout.write(JSON.stringify(rows, null, 2) + '\n');
      return 0;
    }
    process.stdout.write(
      table([
        ['NAME', 'SITE', 'URL', 'STATUS', 'ID'],
        ...rows.map((r) => [r.name, r.site, r.url || '-', r.listed ? r.status : `${r.status} (not in the file)`, r.id ?? '-']),
      ]),
    );
    return 0;
  },
};

export const upCommands: ReadonlyMap<string, Command> = new Map([
  ['up', up],
  ['down', down],
  ['ps', ps],
]);
