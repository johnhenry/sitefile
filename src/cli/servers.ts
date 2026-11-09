import { spawn } from 'node:child_process';
import { promises as fs } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { SitefileError } from '../errors.js';
import { collectBuildArgs } from '../build-args.js';
import { build } from '../executor.js';
import { BUILDING_EVENT, closeServer, createSiteServer, errorEvent, formatUrl, jsonLogLine, listen, logLineAsJson, startedLogLine, type CachePreview, LiveReload, OK_EVENT, parseAllowedHost, reloadEventFor } from '../server.js';
import { isRunning, loadDirectoryTree, loadServedTree, pidAlive, servedLastModified, ServerRegistry, type ServerState, loadServedSite, checkSiteFiles } from '../servers.js';
import { Store } from '../store.js';
import { Tree } from '../tree.js';
import { transformCacheDir } from './cache.js';
import { requireArgs, str, strs, table, usage, usageError, type Command, type ParsedValues } from './common.js';
import { watchMemo } from '../memo.js';
import { pollOption, watchLoop } from './watch.js';

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

/** `--country`: an ISO 3166-1 alpha-2 code (any case). */
export function parseCountry(v: string | undefined): string | undefined {
  if (v === undefined) return undefined;
  if (!/^[A-Za-z]{2}$/.test(v)) usage(`--country expects a two-letter country code such as nz, got '${v}'`);
  return v;
}


async function isFile(p: string): Promise<boolean> {
  return fs.stat(p).then((s) => s.isFile(), () => false);
}

/** An archive file source: labelled by its absolute path, stored by its real path. */
export async function archiveSource(arg: string): Promise<{ label: string; source: ServerState['source'] }> {
  const file = path.resolve(arg);
  return { label: file, source: { kind: 'archive', path: await fs.realpath(file).catch(() => file) } };
}

/**
 * A directory or archive file when `arg` looks like a path, else an image reference or ID, else an
 * archive file by that name (`site.zip`). Whether a file is an archive is checked when it is loaded.
 */
export async function resolveSource(store: Store, arg: string): Promise<{ label: string; source: ServerState['source'] }> {
  if (/^(\.\.?|~.*|\.\.?\/.*|\/.*|.*\/)$/.test(arg)) {
    const dir = path.resolve(arg);
    if (await isFile(dir)) return archiveSource(arg);
    const st = await fs.stat(dir).catch(() => undefined);
    if (!st?.isDirectory()) throw new SitefileError('E_NOT_FOUND', `'${arg}' is not a directory or a file`);
    return { label: dir, source: { kind: 'dir', path: await fs.realpath(dir) } };
  }
  try {
    return { label: arg, source: { kind: 'image', imageId: await store.resolve(arg) } };
  } catch (e) {
    if (e instanceof SitefileError && e.code === 'E_NOT_FOUND' && (await fs.stat(arg).then((s) => s.isDirectory(), () => false))) {
      throw new SitefileError('E_NOT_FOUND', `${e.message} (to serve the directory, write ./${arg})`);
    }
    if (await isFile(arg)) return archiveSource(arg);
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
  name: 'serve',
  summary: 'serve an image, directory or archive file over HTTP',
  usage: 'serve [--name n] [-p [host:]port] [-d] [--spa] <ref|id|./dir|file>  |  serve --watch [options] [context]',
  description:
    'Serve an image from the local store, a directory (paths starting with ./, ../ or /), or an archive file over HTTP:\nzip, tar, tar.gz, wbn, or an OCI archive as build -o x.oci.tar and save write it (the format is read from the\nbytes). A directory or archive is a snapshot read at start. GET and HEAD only; no directory listings.\nBinds 127.0.0.1 on a free port unless -p is given;\n0.0.0.0 is bound only when you pass it explicitly. Foreground by default (Ctrl-C stops it); -d detaches and\nrecords the server for servers, server stop, rm, logs and restart.\n\nWith --watch the argument is a build context (default .): its Sitefile is built in memory and served, rebuilt\nwhen the context, the Sitefile or a --build-arg-file changes, and open pages reload after each good rebuild\n(page loads get a small script, /__sitefile/reload.js). A failed rebuild keeps serving the last\ngood build. Nothing is written to the context except Sitefile.lock. A directory with no Sitefile (and no -f,\n--target or --build-arg*) is served as it is and re-read on change. When only stylesheets changed, pages\nswap them in place.',
  flags: [
    '--name <n>        name for the server (-d only; default site-<6 hex>)',
    '-p, --publish <[host:]port>  address to bind (default 127.0.0.1, a free port)',
    '-d, --detach      run in the background and print the server ID',
    '--spa             serve /index.html for unknown paths without a file extension',
    '--cors            Access-Control-Allow-Origin: * on every response and OPTIONS preflights (204)',
    '--isolate         cross-origin isolation headers (COOP same-origin, COEP, CORP same-origin) on every response',
    '--coep <require-corp|credentialless>  the Cross-Origin-Embedder-Policy of --isolate (default require-corp)',
    '--max-age <secs>  preview production caching: Cache-Control: public, max-age=<secs> on files other than HTML',
    '--immutable       add immutable to the --max-age value (fingerprinted assets)',
    '--max-age-html <secs>        Cache-Control: public, max-age=<secs> on HTML pages too (default: they keep no-cache)',
    '--log-format <text|json>     access-log lines as text (default) or one JSON object per line (a stable format; docs/CLI.md)',
    '--log-ip          add the client address to access-log lines',
    '--quiet           no access-log lines',
    '--allowed-host <name>        also answer requests for this Host (repeatable; .example.com: and subdomains; *: any)',
    '--country <code>  the country _redirects Country= conditions match (there is no geolocation locally)',
    '--strict-site-files  refuse to serve when a _redirects or _headers line would be ignored',
    '--keep-os-metadata  serve __MACOSX, ._*, Thumbs.db and desktop.ini of a directory or archive (default: skipped)',
    '--watch           build the context, serve it, rebuild on change and live-reload pages (foreground only)',
    '-f, --file <path>            Sitefile path (--watch; default <context>/Sitefile)',
    '--target <stage>             build up to this stage and serve its whole tree (--watch)',
    '--build-arg <NAME=value>     set an ARG (--watch; repeatable)',
    '--build-arg-file <path>      read ARGs from a .json, .yaml/.yml or .env file (--watch; repeatable)',
    '--no-cache        do not use the TRANSFORM result cache (--watch)',
    '--no-lockfile     do not read or write Sitefile.lock (--watch)',
    '--keep-service-workers       do not unregister service workers and clear caches before a reload (--watch)',
    '--poll            poll for changes (every 1000 ms) instead of file system events (--watch)',
    '--poll-interval <ms>         poll every <ms> milliseconds (--watch; implies --poll)',
    '--no-css-inject   reload the page when only stylesheets changed, instead of swapping them (--watch)',
    '--console         also forward console.log/info from pages to the log, not only errors and warnings (--watch)',
    '--no-overlay      no Rebuilding badge or build-error overlay in pages (--watch)',
    '--navigate-to-changed        after a rebuild that changed one page, open pages go to it (--watch)',
    '--reload <full|morph>        morph (default): update a page in place when only HTML changed; full: always reload (--watch)',
  ],
  options: {
    name: { type: 'string' },
    publish: { type: 'string', short: 'p' },
    detach: { type: 'boolean', short: 'd' },
    spa: { type: 'boolean' },
    country: { type: 'string' },
    'strict-site-files': { type: 'boolean' },
    'keep-os-metadata': { type: 'boolean' },
    'allowed-host': { type: 'string', multiple: true },
    cors: { type: 'boolean' },
    isolate: { type: 'boolean' },
    coep: { type: 'string' },
    'max-age': { type: 'string' },
    immutable: { type: 'boolean' },
    'max-age-html': { type: 'string' },
    'log-format': { type: 'string' },
    'log-ip': { type: 'boolean' },
    quiet: { type: 'boolean' },
    watch: { type: 'boolean' },
    file: { type: 'string', short: 'f' },
    target: { type: 'string' },
    'build-arg': { type: 'string', multiple: true },
    'build-arg-file': { type: 'string', multiple: true },
    'no-cache': { type: 'boolean' },
    'no-lockfile': { type: 'boolean' },
    'keep-service-workers': { type: 'boolean' },
    poll: { type: 'boolean' },
    'poll-interval': { type: 'string' },
    'no-css-inject': { type: 'boolean' },
    console: { type: 'boolean' },
    'no-overlay': { type: 'boolean' },
    'navigate-to-changed': { type: 'boolean' },
    reload: { type: 'string' },
  },
  async run(v, pos) {
    if (v.watch) return serveWatch(v, pos);
    for (const f of WATCH_ONLY) if (v[f] !== undefined) usage(`--${f} needs --watch`);
    requireArgs(pos, 1, 1, 'an image reference, ./directory or archive file');
    const bind = parsePublish(str(v.publish));
    const allowedHosts = allowedHostsOf(v);
    const isolate = isolateOf(v);
    const cache = cacheOf(v);
    const logging = loggingOf(v);
    const country = parseCountry(str(v.country));
    const strictSiteFiles = !!v['strict-site-files'];
    const name = str(v.name);
    if (name !== undefined && !v.detach) usage('--name needs -d (only detached servers are tracked)');
    const store = await Store.open();
    const { label, source } = await resolveSource(store, pos[0] as string);
    if (isWildcard(bind.host)) process.stderr.write(`warning: binding ${bind.host}: the site will be reachable from other machines\n`);

    const keepOsMetadata = !!v['keep-os-metadata'];
    const warn = (m: string): void => void process.stderr.write(`warning: ${m}\n`);
    if (!v.detach) {
      const { tree, content, close } = await loadServedSite(store, source, { keepOsMetadata, warn });
      checkSiteFiles(tree, strictSiteFiles, warn, content);
      const modified = await servedLastModified(store, source);
      const server = createSiteServer(tree, { spa: !!v.spa, allowedHosts, cors: !!v.cors, ...(isolate ? { isolate } : {}), ...(cache ? { cache } : {}), ...logging, lastModified: () => modified, log: logWriter(logging), ...(content ? { content } : {}), ...(country !== undefined ? { country } : {}) });
      const addr = await listen(server, bind.host, bind.port);
      if (logging.logFormat === 'json') process.stdout.write(`${startedLogLine(new Date().toISOString(), label, tree.size, formatUrl(addr.host, addr.port))}\n`);
      else process.stdout.write(`Serving ${label} (${tree.size} files) at ${formatUrl(addr.host, addr.port)}  (Ctrl-C to stop)\n`);
      await waitForSignal();
      await closeServer(server);
      close();
      return 0;
    }

    const reg = await ServerRegistry.open(store);
    const up = await serveDetached(reg, { label, source, name, bind, spa: !!v.spa, keepOsMetadata, allowedHosts, cors: !!v.cors, ...(isolate ? { isolate } : {}), ...(cache ? { cache } : {}), ...logging, country, strictSiteFiles }, warn);
    process.stdout.write(`Started ${up.name} (${up.id}) serving ${label} at ${formatUrl(up.host, up.port)}\n`);
    return 0;
  },
};

/** What a detached server serves and how (`serve -d`, `sitefile up`). */
export interface DetachedServe {
  readonly label: string;
  readonly source: ServerState['source'];
  readonly name?: string | undefined;
  readonly bind: { readonly host: string; readonly port: number };
  readonly spa: boolean;
  /** `--keep-os-metadata`. */
  readonly keepOsMetadata?: boolean | undefined;
  /** `--allowed-host` names. */
  readonly allowedHosts?: readonly string[] | undefined;
  /** `--cors`. */
  readonly cors?: boolean | undefined;
  /** `--isolate`: the COEP value. */
  readonly isolate?: 'require-corp' | 'credentialless' | undefined;
  /** `--max-age`, `--immutable`, `--max-age-html`. */
  readonly cache?: CachePreview | undefined;
  /** `--log-format json`. */
  readonly logFormat?: 'json' | undefined;
  /** `--log-ip`. */
  readonly logIp?: boolean | undefined;
  /** `--quiet`. */
  readonly quiet?: boolean | undefined;
  /** `--country`. */
  readonly country?: string | undefined;
  /** `--strict-site-files`. */
  readonly strictSiteFiles?: boolean | undefined;
  /** The servers file it belongs to (`sitefile up`). */
  readonly project?: string | undefined;
}

/**
 * `serve -d`: check that the source loads, allocate an ID and name, record the server, start its worker and
 * wait until it listens. On failure the record is removed.
 */
export async function serveDetached(reg: ServerRegistry, d: DetachedServe, warn?: (m: string) => void): Promise<ServerState> {
  // fail fast on a bad image, directory or archive (or an ignored site-file line with --strict-site-files)
  const site = await loadServedSite(reg.store, d.source, { keepOsMetadata: !!d.keepOsMetadata, ...(warn ? { warn } : {}) });
  try {
    checkSiteFiles(site.tree, !!d.strictSiteFiles, warn ?? (() => undefined), site.content);
  } finally {
    site.close();
  }
  const { id, name: finalName } = await reg.allocate(d.name);
  const state: ServerState = {
    id,
    name: finalName,
    image: d.label,
    source: d.source,
    spa: d.spa,
    ...(d.keepOsMetadata ? { keepOsMetadata: true } : {}),
    ...(d.allowedHosts !== undefined && d.allowedHosts.length > 0 ? { allowedHosts: d.allowedHosts } : {}),
    ...(d.cors ? { cors: true } : {}),
    ...(d.isolate ? { isolate: d.isolate } : {}),
    ...(d.cache ? { cache: d.cache } : {}),
    ...(d.logFormat === 'json' ? { logFormat: 'json' as const } : {}),
    ...(d.logIp ? { logIp: true } : {}),
    ...(d.quiet ? { quiet: true } : {}),
    ...(d.country !== undefined ? { country: d.country } : {}),
    ...(d.strictSiteFiles ? { strictSiteFiles: true } : {}),
    bind: d.bind,
    host: d.bind.host,
    port: d.bind.port,
    pid: 0,
    started: new Date().toISOString(),
    logFile: reg.logPath(id),
    ready: false,
    ...(d.project !== undefined ? { project: d.project } : {}),
  };
  await reg.write(state);
  try {
    return await startDetached(reg, state);
  } catch (e) {
    await reg.remove(id);
    throw e;
  }
}

/**
 * `server restart`: stop `s` if it is running, then start it again with the same ID, name, source and
 * requested bind address. On failure the record stays, marked not ready.
 */
export async function restartDetached(reg: ServerRegistry, s: ServerState, graceMs: number): Promise<ServerState> {
  await stopServer(reg, s, graceMs);
  (await loadServedSite(reg.store, s.source, { keepOsMetadata: !!s.keepOsMetadata })).close();
  const fresh: ServerState = { ...s, host: s.bind.host, port: s.bind.port, pid: 0, ready: false, started: new Date().toISOString() };
  delete (fresh as { error?: string }).error;
  await reg.write(fresh);
  return startDetached(reg, fresh).catch(async (e: unknown) => {
    await reg.update(s.id, { ready: false });
    throw e;
  });
}

/** serve flags that only mean something with --watch. */
const WATCH_ONLY = ['file', 'target', 'build-arg', 'build-arg-file', 'no-cache', 'no-lockfile', 'keep-service-workers', 'poll', 'poll-interval', 'no-css-inject', 'console', 'no-overlay', 'navigate-to-changed', 'reload'] as const;

/**
 * `serve --watch [context]`: build in memory, serve, rebuild on change, live reload. A directory
 * without a `Sitefile`, given none of -f, --target, --build-arg and --build-arg-file, is served
 * as it is (re-read on change) instead of built.
 */
/** `--allowed-host` values, validated and lowercased (the server always adds localhost names and IP addresses). */
function allowedHostsOf(v: ParsedValues): string[] {
  const out: string[] = [];
  for (const raw of strs(v['allowed-host'])) {
    const h = parseAllowedHost(raw);
    if (h === undefined) usage(`--allowed-host: invalid host name '${raw}'`);
    if (!out.includes(h)) out.push(h);
  }
  return out;
}

/** `--isolate [--coep require-corp|credentialless]`: the Cross-Origin-Embedder-Policy to send, or undefined. */
function isolateOf(v: ParsedValues): 'require-corp' | 'credentialless' | undefined {
  const coep = str(v.coep);
  if (coep !== undefined && !v.isolate) usage('--coep needs --isolate');
  if (coep !== undefined && coep !== 'require-corp' && coep !== 'credentialless') usage(`--coep must be require-corp or credentialless, not '${coep}'`);
  return v.isolate ? (coep ?? 'require-corp') : undefined;
}

/** `--max-age`, `--immutable`, `--max-age-html`, or undefined when none is given. */
function cacheOf(v: ParsedValues): CachePreview | undefined {
  const secs = (flag: string): number | undefined => {
    const raw = str(v[flag]);
    if (raw === undefined) return undefined;
    if (!/^[0-9]{1,10}$/.test(raw) || Number(raw) > 2147483647) usage(`--${flag} must be a number of seconds (0 to 2147483647), not '${raw}'`);
    return Number(raw);
  };
  const maxAge = secs('max-age');
  const maxAgeHtml = secs('max-age-html');
  if (v.immutable && maxAge === undefined) usage('--immutable needs --max-age');
  if (maxAge === undefined && maxAgeHtml === undefined) return undefined;
  return { ...(maxAge !== undefined ? { maxAge } : {}), ...(v.immutable ? { immutable: true } : {}), ...(maxAgeHtml !== undefined ? { maxAgeHtml } : {}) };
}

/** `--log-format`, `--log-ip`, `--quiet`: the server options (and record fields) they set, only those not at their default. */
function loggingOf(v: ParsedValues): { logFormat?: 'json'; logIp?: true; quiet?: true } {
  const format = str(v['log-format']);
  if (format !== undefined && format !== 'text' && format !== 'json') usage(`--log-format must be text or json, not '${format}'`);
  return { ...(format === 'json' ? { logFormat: 'json' as const } : {}), ...(v['log-ip'] ? { logIp: true as const } : {}), ...(v.quiet ? { quiet: true as const } : {}) };
}

/** Where a foreground server's log lines go: stdout, other lines wrapped as JSON objects under `--log-format json`. */
function logWriter(logging: { logFormat?: 'json' }): (line: string) => void {
  return (l) => void process.stdout.write(`${logging.logFormat === 'json' && !l.startsWith('{') ? jsonLogLine(new Date().toISOString(), l) : l}\n`);
}

async function serveWatch(v: ParsedValues, pos: string[]): Promise<number> {
  requireArgs(pos, 0, 1, 'a build context directory');
  if (v.detach) usage('--watch cannot be combined with -d');
  if (v.name !== undefined) usage('--name needs -d (only detached servers are tracked)');
  const bind = parsePublish(str(v.publish));
  const allowedHosts = allowedHostsOf(v);
  const isolate = isolateOf(v);
  const cache = cacheOf(v);
  const logging = loggingOf(v);
  const context = pos[0] ?? '.';
  const file = str(v.file);
  const target = str(v.target);
  const argFiles = strs(v['build-arg-file']);
  const argFlags = strs(v['build-arg']);
  const pollMs = pollOption(!!v.poll, str(v['poll-interval']));
  const buildFlags = !!file || target !== undefined || argFiles.length > 0 || argFlags.length > 0;
  const plain = !buildFlags && !(await isFile(path.join(context, 'Sitefile')));
  const cssInject = !v['no-css-inject'];
  const navigate = !!v['navigate-to-changed'];
  const reloadMode = str(v.reload) ?? 'morph';
  const country = parseCountry(str(v.country));
  const strictSiteFiles = !!v['strict-site-files'];
  /** The site-file warnings last printed: a rebuild repeats them only when they change. */
  let lastSiteWarnings = '';
  const siteFiles = (tree: Tree): void => {
    const shown: string[] = [];
    checkSiteFiles(tree, false, (m) => shown.push(m));
    const text = shown.join('\n');
    if (text !== lastSiteWarnings) for (const m of shown) process.stderr.write(`warning: ${m}\n`);
    lastSiteWarnings = text;
    checkSiteFiles(tree, strictSiteFiles, () => undefined);
  };
  if (reloadMode !== 'full' && reloadMode !== 'morph') usage(`--reload expects full or morph, got '${reloadMode}'`);
  if (isWildcard(bind.host)) process.stderr.write(`warning: binding ${bind.host}: the site will be reachable from other machines\n`);

  let current: Tree = Tree.empty;
  let builtAt: number | undefined; // Last-Modified: the time of the build being served
  let started = false;
  const memo = watchMemo();
  const live = new LiveReload({ resetServiceWorkers: !v['keep-service-workers'], cssInject, console: !!v.console, overlay: !v['no-overlay'], navigateToChanged: navigate, reload: reloadMode });
  /** Serve `next` and tell open pages what changed. */
  const swap = (next: Tree): void => {
    const event = started ? (reloadEventFor(current, next, cssInject, navigate) ?? OK_EVENT) : undefined;
    current = next;
    builtAt = Math.floor(Date.now() / 1000);
    if (event !== undefined) live.send(event);
  };
  const server = createSiteServer(() => current, { spa: !!v.spa, allowedHosts, cors: !!v.cors, ...(isolate ? { isolate } : {}), ...(cache ? { cache } : {}), ...logging, liveReload: live, lastModified: () => builtAt, log: logWriter(logging), warn: (m) => process.stderr.write(`warning: ${m}\n`), ...(country !== undefined ? { country } : {}) });
  try {
    return await watchLoop({
      context,
      ...(pollMs !== undefined ? { pollMs } : {}),
      ...(file ? { sitefile: file } : {}),
      ...(plain ? { plain } : {}),
      argFiles,
      build: async () => {
        if (started) live.send(BUILDING_EVENT);
        try {
          if (plain) {
            const tree = await loadDirectoryTree(context);
            process.stdout.write(`Read ${tree.size} file${tree.size === 1 ? '' : 's'} (${tree.digest()})\n`);
            siteFiles(tree);
            swap(tree);
            return { code: 0 };
          }
          const r = await build({
            contextDir: context,
            ...(file ? { sitefile: file } : {}),
            ...(target !== undefined ? { target } : {}),
            buildArgs: await collectBuildArgs(argFiles, argFlags),
            ...(v['no-lockfile'] ? { lockfile: false as const } : {}),
            ...(v['no-cache'] ? {} : { cacheDir: transformCacheDir() }),
            skipBase: true,
            keepOsMetadata: !!v['keep-os-metadata'],
            ...(memo ? { memo } : {}),
            onNotice: (m) => process.stderr.write(`${m}\n`),
            onWarning: (m) => process.stderr.write(`warning: ${m}\n`),
          });
          process.stdout.write(`Built ${r.output.size} file${r.output.size === 1 ? '' : 's'} (${r.output.digest()})\n`);
          siteFiles(r.output);
          swap(r.output);
          return { code: 0 };
        } catch (e) {
          if (!(e instanceof SitefileError)) throw e;
          if (e.code === 'E_USAGE') return { code: usageError(e.detail) };
          process.stderr.write(`error[${e.code}]: ${e.message}\n`);
          live.send(errorEvent(e)); // pages show it (and pages opened later get it on connect)
          return { code: 1 };
        }
      },
      onStarted: async () => {
        const addr = await listen(server, bind.host, bind.port);
        started = true;
        if (logging.logFormat === 'json') process.stdout.write(`${startedLogLine(new Date().toISOString(), context, current.size, formatUrl(addr.host, addr.port))}\n`);
        else process.stdout.write(`Serving ${context} (${current.size} files) at ${formatUrl(addr.host, addr.port)}  (Ctrl-C to stop)\n`);
      },
    });
  } finally {
    if (started) await closeServer(server);
  }
}

const ps: Command = {
  name: 'servers',
  summary: 'list site servers (also: server ls)',
  usage: 'servers [-a] [--json]',
  description: 'List running site servers (-a: also exited ones). A server is running only if its process is alive\nand its port answers; stale entries show as exited.',
  flags: ['-a, --all   include exited servers', '--json      machine-readable output'],
  options: { all: { type: 'boolean', short: 'a' }, json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 0, 0, 'no arguments');
    const reg = await ServerRegistry.open();
    const views = await Promise.all((await reg.all()).map((s) => reg.view(s)));
    const shown = views.filter((s) => v.all || s.status === 'running');
    if (v.json) {
      // 'site' is the image reference or directory being served.
      const out = shown.map(({ image, ...rest }) => ({ ...rest, site: image }));
      process.stdout.write(JSON.stringify(out, null, 2) + '\n');
      return 0;
    }
    process.stdout.write(
      table([
        ['NAME', 'SITE', 'URL', 'STATUS', 'ID'],
        ...shown.map((s) => [s.name, s.image, s.url || '-', s.status, s.id]),
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

export function graceOf(v: ParsedValues): number {
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
  usage: 'server stop [--time secs] <name|id>...',
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
  usage: 'server rm [-f] <name|id>...',
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

/** The last n lines of text, each newline-terminated; a trailing newline is not an extra empty line. */
export function tailLines(text: string, n: number): string {
  const lines = text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return (n === 0 ? [] : lines.slice(-n)).map((l) => `${l}\n`).join('');
}

const logs: Command = {
  name: 'logs',
  summary: 'show a server\'s access log',
  usage: 'server logs [-f] [-n lines] [--json] <name|id>',
  description: 'Print the log of a detached server: one line per request ("METHOD /path STATUS BYTES MSms").',
  flags: ['-f, --follow    keep printing new lines until Ctrl-C', '-n, --tail <n>  only the last n lines', '--json          one JSON object per line (text lines of older servers are converted)'],
  options: { follow: { type: 'boolean', short: 'f' }, tail: { type: 'string', short: 'n' }, json: { type: 'boolean' } },
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
      text = tailLines(text, n);
    }
    let carry = '';
    /** Write log bytes; with --json, whole lines only, each converted. */
    const emit = (chunk: string): void => {
      if (!v.json) return void process.stdout.write(chunk);
      const lines = (carry + chunk).split('\n');
      carry = lines.pop() ?? '';
      if (lines.length > 0) process.stdout.write(lines.map((l) => `${logLineAsJson(l)}\n`).join(''));
    };
    emit(text);
    if (!v.follow) {
      if (v.json && carry !== '') process.stdout.write(`${logLineAsJson(carry)}\n`);
      return 0;
    }
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
          emit(buf.toString('utf8'));
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
  usage: 'server restart [--time secs] <name|id>',
  description: 'Stop the server if it is running, then start it again with the same name, image and bind address.\nImages are served by the same image ID; a directory or archive file is re-read.',
  flags: ['--time <secs>   grace period before SIGKILL (default 5)'],
  options: { time: { type: 'string' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'a server id or name');
    const reg = await ServerRegistry.open();
    const s = await reg.find(pos[0] as string);
    const up = await restartDetached(reg, s, graceOf(v));
    process.stdout.write(`Restarted ${up.name} (${up.id}) serving ${up.image} at ${formatUrl(up.host, up.port)}\n`);
    return 0;
  },
};

export { isFile };

/** Top-level: `sitefile serve`, `sitefile servers`. */
export const serverCommands: ReadonlyMap<string, Command> = new Map([
  ['serve', run],
  ['servers', ps],
]);

/** `sitefile server <ls|stop|rm|logs|restart>`. */
export const serverSubcommands: ReadonlyMap<string, Command> = new Map([
  ['ls', ps],
  ['stop', stop],
  ['rm', rm],
  ['logs', logs],
  ['restart', restart],
]);
