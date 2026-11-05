import { spawn } from 'node:child_process';
import { promises as fs } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { SitefileError } from '../errors.js';
import { closeServer, createSiteServer, formatUrl, listen } from '../server.js';
import { isRunning, loadServedTree, pidAlive, ServerRegistry, type ServerState } from '../servers.js';
import { Store } from '../store.js';
import { requireArgs, str, table, usage, type Command, type ParsedValues } from './common.js';

const WORKER = fileURLToPath(new URL('./serve-worker.js', import.meta.url));

/** `-p` value: `port`, `host:port` or `[v6]:port`. Default bind is 127.0.0.1 with a free port. */
export function parsePublish(spec: string | undefined): { host: string; port: number } {
  if (spec === undefined) return { host: '127.0.0.1', port: 0 };
  const m = /^(?:(?:\[([0-9A-Fa-f:]+)\]|([^:[\]]+)):)?([0-9]{1,5})$/.exec(spec);
  if (!m) usage(`-p expects [host:]port, got '${spec}'`);
  const port = Number(m[3]);
  if (port > 65535) usage(`-p: port ${port} is out of range`);
  let host = m[1] ?? m[2] ?? '127.0.0.1';
  if (host === 'localhost') host = '127.0.0.1';
  if (!net.isIP(host)) usage(`-p: host must be an IP address or 'localhost', got '${host}'`);
  return { host, port };
}

const isWildcard = (h: string): boolean => h === '0.0.0.0' || h === '::';

async function resolveSource(store: Store, arg: string): Promise<{ label: string; source: ServerState['source'] }> {
  if (/^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$/.test(arg)) {
    const dir = path.resolve(arg);
    const st = await fs.stat(dir).catch(() => undefined);
    if (!st?.isDirectory()) throw new SitefileError('E_NOT_FOUND', `'${arg}' is not a directory`);
    return { label: dir, source: { kind: 'dir', path: await fs.realpath(dir) } };
  }
  try {
    return { label: arg, source: { kind: 'image', imageId: await store.resolve(arg) } };
  } catch (e) {
    if (e instanceof SitefileError && e.code === 'E_NOT_FOUND' && (await fs.stat(arg).then((s) => s.isDirectory(), () => false))) {
      throw new SitefileError('E_NOT_FOUND', `${e.message} (to serve the directory, write ./${arg})`);
    }
    throw e;
  }
}

function waitForSignal(): Promise<string> {
  return new Promise((resolve) => {
    process.once('SIGINT', () => resolve('SIGINT'));
    process.once('SIGTERM', () => resolve('SIGTERM'));
  });
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

/** Spawn the detached worker for `state` and wait until it listens (or fails). */
async function startDetached(reg: ServerRegistry, state: ServerState): Promise<ServerState> {
  const log = await fs.open(state.logFile, 'a', 0o600);
  const child = spawn(process.execPath, [WORKER, state.id], {
    detached: true,
    stdio: ['ignore', log.fd, log.fd],
    env: { ...process.env, SITEFILE_HOME: reg.store.home },
  });
  await log.close();
  let exited = false;
  child.once('exit', () => (exited = true));
  child.once('error', () => (exited = true));
  const deadline = Date.now() + 15_000;
  for (;;) {
    const cur = await reg.read(state.id);
    if (cur?.ready) {
      child.unref();
      return cur;
    }
    if (exited || cur?.error || Date.now() > deadline) {
      if (!exited) child.kill('SIGKILL');
      const why = cur?.error ?? (exited ? 'the server process exited before it was ready' : 'timed out waiting for the server to start');
      throw new SitefileError('E_SERVER', why.replace(/^error\[E_SERVER\]: /, ''));
    }
    await sleep(40);
  }
}

const run: Command = {
  name: 'run',
  summary: 'serve an image or directory over HTTP',
  usage: 'run [--name n] [-p [host:]port] [-d] [--spa] <ref|id|./dir>',
  description:
    'Serve an image from the local store, or a directory (a snapshot read at start; paths starting with ./, ../ or /),\nover HTTP. GET and HEAD only; no directory listings. Binds 127.0.0.1 on a free port unless -p is given;\n0.0.0.0 is bound only when you pass it explicitly. Foreground by default (Ctrl-C stops it); -d detaches and\nrecords the server for ps, stop, rm, logs and restart.',
  flags: [
    '--name <n>        name for the server (-d only; default site-<6 hex>)',
    '-p, --publish <[host:]port>  address to bind (default 127.0.0.1, a free port)',
    '-d, --detach      run in the background and print the server ID',
    '--spa             serve /index.html for unknown paths without a file extension',
  ],
  options: { name: { type: 'string' }, publish: { type: 'string', short: 'p' }, detach: { type: 'boolean', short: 'd' }, spa: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'an image reference or ./directory');
    const bind = parsePublish(str(v.publish));
    const name = str(v.name);
    if (name !== undefined && !v.detach) usage('--name needs -d (only detached servers are tracked)');
    const store = await Store.open();
    const { label, source } = await resolveSource(store, pos[0] as string);
    if (isWildcard(bind.host)) process.stderr.write(`warning: binding ${bind.host}: the site will be reachable from other machines\n`);

    if (!v.detach) {
      const tree = await loadServedTree(store, source);
      const server = createSiteServer(tree, { spa: !!v.spa, log: (l) => process.stdout.write(`${l}\n`) });
      const addr = await listen(server, bind.host, bind.port);
      process.stdout.write(`Serving ${label} (${tree.size} files) at ${formatUrl(addr.host, addr.port)}  (Ctrl-C to stop)\n`);
      await waitForSignal();
      await closeServer(server);
      return 0;
    }

    await loadServedTree(store, source); // fail fast on a bad image or directory
    const reg = await ServerRegistry.open(store);
    const { id, name: finalName } = await reg.allocate(name);
    const state: ServerState = {
      id,
      name: finalName,
      image: label,
      source,
      spa: !!v.spa,
      bind,
      host: bind.host,
      port: bind.port,
      pid: 0,
      started: new Date().toISOString(),
      logFile: reg.logPath(id),
      ready: false,
    };
    await reg.write(state);
    try {
      const up = await startDetached(reg, state);
      process.stdout.write(`Started ${up.name} (${up.id}) serving ${label} at ${formatUrl(up.host, up.port)}\n`);
      return 0;
    } catch (e) {
      await reg.remove(id);
      throw e;
    }
  },
};

const ps: Command = {
  name: 'ps',
  summary: 'list site servers',
  usage: 'ps [-a] [--json]',
  description: 'List running site servers (-a: also exited ones). A server is running only if its process is alive\nand its port answers; stale entries show as exited.',
  flags: ['-a, --all   include exited servers', '--json      machine-readable output'],
  options: { all: { type: 'boolean', short: 'a' }, json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const reg = await ServerRegistry.open();
    const views = await Promise.all((await reg.all()).map((s) => reg.view(s)));
    const shown = views.filter((s) => v.all || s.status === 'running');
    if (v.json) {
      process.stdout.write(JSON.stringify(shown, null, 2) + '\n');
      return 0;
    }
    process.stdout.write(
      table([
        ['ID', 'NAME', 'IMAGE', 'URL', 'STATUS'],
        ...shown.map((s) => [s.id, s.name, s.image, s.url || '-', s.status]),
      ]),
    );
    return 0;
  },
};

/** SIGTERM, wait, then SIGKILL. Returns false if the server was not running. */
export async function stopServer(reg: ServerRegistry, s: ServerState, graceMs: number): Promise<boolean> {
  if (!(await isRunning(s))) return false;
  const gone = async (ms: number): Promise<boolean> => {
    for (const end = Date.now() + ms; Date.now() < end; await sleep(50)) if (!pidAlive(s.pid)) return true;
    return !pidAlive(s.pid);
  };
  try {
    process.kill(s.pid, 'SIGTERM');
  } catch {
    return false;
  }
  if (!(await gone(graceMs))) {
    try {
      process.kill(s.pid, 'SIGKILL');
    } catch {
      /* already gone */
    }
    if (!(await gone(2000))) throw new SitefileError('E_SERVER', `could not stop ${s.name} (pid ${s.pid})`);
  }
  await reg.update(s.id, { ready: false }).catch(() => undefined);
  return true;
}

function graceOf(v: ParsedValues): number {
  const t = str(v.time);
  if (t === undefined) return 5000;
  const n = Number(t);
  if (!Number.isFinite(n) || n < 0) usage('--time expects a number of seconds');
  return n * 1000;
}

/** Run `fn` for each target; print each failure and keep going. */
async function eachTarget(reg: ServerRegistry, keys: string[], fn: (s: ServerState) => Promise<string>): Promise<number> {
  let failed = false;
  for (const key of keys) {
    try {
      process.stdout.write(`${await fn(await reg.find(key))}\n`);
    } catch (e) {
      if (!(e instanceof SitefileError)) throw e;
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      failed = true;
    }
  }
  return failed ? 1 : 0;
}

const stop: Command = {
  name: 'stop',
  summary: 'stop running site servers',
  usage: 'stop [--time secs] <id|name>...',
  description: 'Send SIGTERM, wait up to --time seconds (default 5), then SIGKILL. Stopping an exited server is a no-op.',
  flags: ['--time <secs>   grace period before SIGKILL (default 5)'],
  options: { time: { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 1, Infinity, 'a server id or name');
    const reg = await ServerRegistry.open();
    const grace = graceOf(v);
    return eachTarget(reg, pos, async (s) => {
      await stopServer(reg, s, grace);
      return s.name;
    });
  },
};

const rm: Command = {
  name: 'rm',
  summary: 'remove site servers',
  usage: 'rm [-f] <id|name>...',
  description: 'Delete the record and log of exited servers. Running servers are refused unless -f, which stops them first.',
  flags: ['-f, --force   stop and remove running servers'],
  options: { force: { type: 'boolean', short: 'f' } },
  async run(v, pos) {
    requireArgs(pos, 1, Infinity, 'a server id or name');
    const reg = await ServerRegistry.open();
    return eachTarget(reg, pos, async (s) => {
      if (await isRunning(s)) {
        if (!v.force) throw new SitefileError('E_SERVER', `${s.name} is running; stop it first or use -f`);
        await stopServer(reg, s, 5000);
      }
      await reg.remove(s.id);
      return s.name;
    });
  },
};

const logs: Command = {
  name: 'logs',
  summary: 'show a server\'s access log',
  usage: 'logs [-f] [-n lines] <id|name>',
  description: 'Print the log of a detached server: one line per request ("METHOD /path STATUS BYTES MSms").',
  flags: ['-f, --follow    keep printing new lines until Ctrl-C', '-n, --tail <n>  only the last n lines'],
  options: { follow: { type: 'boolean', short: 'f' }, tail: { type: 'string', short: 'n' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'a server id or name');
    const reg = await ServerRegistry.open();
    const s = await reg.find(pos[0] as string);
    let text = await fs.readFile(s.logFile, 'utf8').catch(() => '');
    const offset = Buffer.byteLength(text);
    const tail = str(v.tail);
    if (tail !== undefined) {
      const n = Number(tail);
      if (!Number.isSafeInteger(n) || n < 0) usage('--tail expects a non-negative integer');
      const lines = text.split('\n').slice(0, -1);
      text = (n === 0 ? [] : lines.slice(-n)).map((l) => `${l}\n`).join('');
    }
    process.stdout.write(text);
    if (!v.follow) return 0;
    let pos2 = offset;
    let done = false;
    void waitForSignal().then(() => (done = true));
    while (!done) {
      await sleep(150);
      const fh = await fs.open(s.logFile, 'r').catch(() => undefined);
      if (!fh) continue;
      try {
        const size = (await fh.stat()).size;
        if (size < pos2) pos2 = 0;
        if (size > pos2) {
          const buf = Buffer.alloc(size - pos2);
          await fh.read(buf, 0, buf.length, pos2);
          pos2 = size;
          process.stdout.write(buf);
        }
      } finally {
        await fh.close();
      }
    }
    return 0;
  },
};

const restart: Command = {
  name: 'restart',
  summary: 'restart a site server',
  usage: 'restart [--time secs] <id|name>',
  description: 'Stop the server if it is running, then start it again with the same name, image and bind address.\nImages are served by the same image ID; a directory is re-read.',
  flags: ['--time <secs>   grace period before SIGKILL (default 5)'],
  options: { time: { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'a server id or name');
    const reg = await ServerRegistry.open();
    const s = await reg.find(pos[0] as string);
    await stopServer(reg, s, graceOf(v));
    await loadServedTree(reg.store, s.source);
    const fresh: ServerState = { ...s, host: s.bind.host, port: s.bind.port, pid: 0, ready: false, started: new Date().toISOString() };
    delete (fresh as { error?: string }).error;
    await reg.write(fresh);
    const up = await startDetached(reg, fresh).catch(async (e: unknown) => {
      await reg.update(s.id, { ready: false });
      throw e;
    });
    process.stdout.write(`Restarted ${up.name} (${up.id}) serving ${up.image} at ${formatUrl(up.host, up.port)}\n`);
    return 0;
  },
};

export const serverCommands: ReadonlyMap<string, Command> = new Map([
  ['run', run],
  ['ps', ps],
  ['stop', stop],
  ['rm', rm],
  ['logs', logs],
  ['restart', restart],
]);
