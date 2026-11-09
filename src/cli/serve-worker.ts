#!/usr/bin/env node
// Detached server process spawned by `sitefile serve -d` / `restart`.
// Usage: node serve-worker.js <server id>. State and logs live in $SITEFILE_HOME/servers.
import { readFile } from 'node:fs/promises';
import { collectBuildArgs } from '../build-args.js';
import { SitefileError } from '../errors.js';
import { build } from '../executor.js';
import { watchMemo } from '../memo.js';
import { transformCacheDir } from './cache.js';
import { exportBasePath, productionUrl } from './servers.js';
import { watchLoop, type BuildOutcome } from './watch.js';
import { BasicAuth, parseHtpasswd } from '../serve-auth.js';
import { BUILDING_EVENT, closeServer, createSiteServer, errorEvent, formatUrl, jsonLogLine, listen, LiveReload, type LiveReloadOptions, OK_EVENT, parseCidr, parseThrottle, reloadEventFor, startedLogLine, stoppedLogLine, routingFor } from '../server.js';
import { loadServedTree, servedLastModified, ServerRegistry, loadServedSite, checkSiteFiles, type ServerState } from '../servers.js';
import { Tree } from '../tree.js';
import { isIgnoredChange, RebuildScheduler, resolveReal, watchPaths } from '../watch.js';

let json = false; // `--log-format json`: every line is a JSON object
const stamp = (m: string): void => {
  const ts = new Date().toISOString();
  process.stdout.write(json ? `${m.startsWith('{') ? m : jsonLogLine(ts, m)}\n` : `${ts} ${m}\n`);
};

async function main(): Promise<void> {
  const id = process.argv[2] ?? '';
  const reg = await ServerRegistry.open();
  const state = await reg.read(id);
  if (!state) throw new SitefileError('E_SERVER', `no state for server '${id}'`);
  json = state.logFormat === 'json';
  try {
    const left = state.expires === undefined ? undefined : Date.parse(state.expires) - Date.now();
    if (left !== undefined && left <= 0) throw new SitefileError('E_SERVER', `server expired at ${state.expires} (--stop-after)`);
    const ipRules = state.allowIp !== undefined || state.denyIp !== undefined ? { allow: (state.allowIp ?? []).flatMap((t) => parseCidr(t) ?? []), deny: (state.denyIp ?? []).flatMap((t) => parseCidr(t) ?? []) } : undefined;
    const loopback = state.bind.host === 'localhost' || state.bind.host === '::1' || /^127\./.test(state.bind.host);
    // `-d --watch` on a build context (#306): built in memory by watchLoop below, served from an empty
    // tree until the first build finishes.
    const building = !!state.watch && state.source.kind === 'dir' && state.build !== undefined;
    const { tree, content } = building ? { tree: Tree.empty, content: undefined } : await loadServedSite(reg.store, state.source, { keepOsMetadata: !!state.keepOsMetadata, warn: (m) => stamp(`warning: ${m}`) });
    if (!building && routingFor(state)?.siteFiles !== false) checkSiteFiles(tree, !!state.strictSiteFiles, (m) => stamp(`warning: ${m}`), content);
    let modified = building ? undefined : await servedLastModified(reg.store, state.source);
    let current: Tree = tree;
    /** A build context's `--base` / `--rewrite-base`, else what the last good build's EXPORT BASE (or SITEMAP/FEED BASE) gives. */
    let base = state.base;
    let rewriteBase = state.rewriteBase;
    // `-d --watch`: the directory is re-read (or the context rebuilt) on change and open pages reload (or swap stylesheets).
    const watching = !!state.watch && state.source.kind === 'dir';
    // `-d --reload` (without --watch too): the script and stream, so open pages reload after a restart.
    const live = watching || state.reload !== undefined ? new LiveReload(liveOptionsOf(state)) : undefined;
    const authEntries = [...(state.auth ?? []), ...(state.authFile !== undefined ? parseHtpasswd(await readFile(state.authFile, 'utf8'), state.authFile) : [])];
    const server = createSiteServer(() => current, { ...(live ? { liveReload: live, warn: (m: string) => stamp(`warning: ${m}`) } : {}), spa: state.spa, allowedHosts: state.allowedHosts ?? [], cors: !!state.cors, ...(state.isolate ? { isolate: state.isolate } : {}), ...(state.cache ? { cache: state.cache } : {}), ...(json ? { logFormat: 'json' as const } : {}), logIp: !!state.logIp, quiet: !!state.quiet, ...(authEntries.length > 0 || state.authFile !== undefined ? { auth: new BasicAuth(authEntries) } : {}), ...(state.maxConnectionsPerIp ? { maxConnectionsPerIp: state.maxConnectionsPerIp } : {}), ...(state.hideBase ? { hideBase: true } : {}), ...(ipRules ? { ipRules } : {}), ...(loopback ? {} : { noindex: true }), clientInfo: true, compress: !!state.compress, ...(state.mime ? { mime: new Map(Object.entries(state.mime)) } : {}), ...(state.throttle !== undefined && parseThrottle(state.throttle) ? { throttle: parseThrottle(state.throttle)! } : {}), ...(state.tlsCert !== undefined && state.tlsKey !== undefined ? { tls: { cert: await readFile(state.tlsCert), key: await readFile(state.tlsKey) } } : {}), serverId: id, lastModified: () => modified, log: stamp, ...(content ? { content } : {}), ...(state.country !== undefined ? { country: state.country } : {}), ...(state.spaExclude ? { spaExclude: state.spaExclude } : {}), ...(state.spaFallback !== undefined ? { spaFallback: state.spaFallback } : {}), ...(building ? { base: () => base, rewriteBase: () => rewriteBase } : { ...(state.base !== undefined ? { base: state.base } : {}), ...(state.rewriteBase !== undefined ? { rewriteBase: state.rewriteBase } : {}) }), ...(routingFor(state) ? { profile: routingFor(state)! } : {}) });
    const addr = await listen(server, state.bind.host, state.bind.port);
    await reg.update(id, { pid: process.pid, host: addr.host, port: addr.port, ready: true, error: undefined });
    const root = formatUrl(addr.host, addr.port, state.tlsCert !== undefined ? 'https' : 'http');
    const served = (): void => {
      const url = `${root}${(base ?? '/').slice(1)}`;
      if (json) stamp(startedLogLine(new Date().toISOString(), state.image, current.size, url));
      else stamp(`serving ${state.image} (${current.size} files) at ${url}`);
    };
    if (!building) served();
    const unwatch =
      live && building
        ? watchBuild(state, live, stamp, {
            current: () => current,
            swap: (next) => {
              current = next;
              modified = Math.floor(Date.now() / 1000);
            },
            bases: (b, rb) => {
              if (state.base === undefined) base = b;
              if (state.rewriteBase === undefined && !state.build?.noRewriteBase) rewriteBase = rb;
            },
            started: served,
          })
        : live && watching
          ? await watchDirectory(reg, state, live, stamp, (next, lastModified) => {
              current = next;
              modified = lastModified;
            }, () => current)
          : () => undefined;
    const stop = (signal: string) => (): void => {
      unwatch();
      if (json) stamp(stoppedLogLine(new Date().toISOString(), signal));
      else stamp(`received ${signal}, shutting down`);
      void closeServer(server).then(async () => {
        await reg.update(id, { ready: false }).catch(() => undefined);
        process.exit(0);
      });
      setTimeout(() => process.exit(0), 3000).unref();
    };
    if (left !== undefined) {
      setTimeout(() => {
        unwatch();
        if (json) stamp(stoppedLogLine(new Date().toISOString(), 'expired'));
        else stamp('--stop-after reached, shutting down');
        void closeServer(server).then(async () => {
          await reg.update(id, { ready: false }).catch(() => undefined);
          process.exit(0);
        });
        setTimeout(() => process.exit(0), 3000).unref();
      }, Math.min(left, 2 ** 31 - 1));
    }
    process.on('SIGTERM', stop('SIGTERM'));
    process.on('SIGINT', stop('SIGINT'));
  } catch (e) {
    const message = e instanceof SitefileError ? `error[${e.code}]: ${e.message}` : `error: ${(e as Error).message}`;
    const ts = new Date().toISOString();
    process.stderr.write(json ? `${jsonLogLine(ts, message)}\n` : `${ts} ${message}\n`);
    await reg.update(id, { pid: 0, ready: false, error: message }).catch(() => undefined);
    process.exit(1);
  }
}

/** The live-reload options a detached server's record asks for (`--reload`, `--sync` and the `-d --watch` page-script flags). */
function liveOptionsOf(state: ServerState): LiveReloadOptions {
  return {
    resetServiceWorkers: !state.keepServiceWorkers,
    cssInject: !state.noCssInject,
    console: !!state.console,
    overlay: !state.noOverlay,
    navigateToChanged: !!state.navigateToChanged,
    reload: state.reload ?? 'morph',
    ...(state.sync === 'nav' || state.sync === 'scroll' ? { sync: state.sync } : {}),
  };
}

/**
 * `-d --watch` on a build context (johnhenry/sitefile#306): build it in memory (like `serve --watch`), then
 * rebuild whenever the context, the Sitefile or a `--build-arg-file` changes. A good build is served and open
 * pages are told what changed; a failed one is logged and shown in open pages, and the last good build stays.
 * The first build runs while the server already listens (on an empty tree). Returns a function that stops it.
 */
function watchBuild(
  state: ServerState,
  live: LiveReload,
  stamp: (m: string) => void,
  site: { current: () => Tree; swap: (next: Tree) => void; bases: (base: string | undefined, rewriteBase: string | undefined) => void; started: () => void },
): () => void {
  const b = state.build ?? {};
  const context = state.source.kind === 'dir' ? state.source.path : '.';
  const cssInject = !state.noCssInject;
  const navigate = !!state.navigateToChanged;
  const siteFilesOn = routingFor(state)?.siteFiles !== false;
  const warn = (m: string): void => stamp(`warning: ${m}`);
  const memo = watchMemo();
  let lastWarnings = '';
  let stop!: () => void;
  const stopped = new Promise<void>((r) => (stop = r));
  const run = async (): Promise<BuildOutcome> => {
    live.send(BUILDING_EVENT);
    try {
      const r = await build({
        contextDir: context,
        ...(b.sitefile !== undefined ? { sitefile: b.sitefile } : {}),
        ...(b.target !== undefined ? { target: b.target } : {}),
        buildArgs: await collectBuildArgs(b.buildArgFiles ?? [], b.buildArgs ?? []),
        ...(b.noLockfile ? { lockfile: false as const } : {}),
        ...(b.noCache ? {} : { cacheDir: transformCacheDir() }),
        skipBase: true,
        keepOsMetadata: !!state.keepOsMetadata,
        ...(memo ? { memo } : {}),
        onNotice: (m) => stamp(m),
        onWarning: warn,
      });
      stamp(`Built ${r.output.size} file${r.output.size === 1 ? '' : 's'} (${r.output.digest()})`);
      if (siteFilesOn) {
        const shown: string[] = [];
        checkSiteFiles(r.output, false, (m) => shown.push(m));
        const text = shown.join('\n');
        if (text !== lastWarnings) for (const m of shown) warn(m);
        lastWarnings = text;
        checkSiteFiles(r.output, !!state.strictSiteFiles, () => undefined);
      }
      site.bases(exportBasePath(r.plan.exportBase), productionUrl(r.plan));
      const event = reloadEventFor(site.current(), r.output, cssInject, navigate) ?? OK_EVENT;
      site.swap(r.output);
      live.send(event);
      return { code: 0 };
    } catch (e) {
      if (!(e instanceof SitefileError)) throw e;
      stamp(e.code === 'E_USAGE' ? `error: ${e.detail}` : `error[${e.code}]: ${e.message}`);
      live.send(errorEvent(e)); // pages show it (and pages opened later get it on connect)
      return { code: 1 };
    }
  };
  void watchLoop({
    context,
    ...(b.sitefile !== undefined ? { sitefile: b.sitefile } : {}),
    argFiles: b.buildArgFiles ?? [],
    ...(state.pollMs !== undefined ? { pollMs: state.pollMs } : {}),
    build: run,
    onStarted: async () => site.started(),
    stop: stopped,
    say: (line) => stamp(line.replace(/\n$/, '')),
  }).catch((e: unknown) => {
    stamp(e instanceof SitefileError ? `error[${e.code}]: ${e.message}` : `error: ${(e as Error).message}`);
  });
  return stop;
}

/**
 * `-d --watch`: watch the served directory (file system events, or polling every `state.pollMs`) and,
 * once changes settle, read it again: a good read is served and open pages are told what changed; a
 * failed one is logged, shown in open pages, and the last good copy stays. Returns a function that stops watching.
 */
async function watchDirectory(
  reg: ServerRegistry,
  state: ServerState,
  live: LiveReload,
  stamp: (m: string) => void,
  swap: (next: Tree, lastModified: number | undefined) => void,
  current: () => Tree,
): Promise<() => void> {
  if (state.source.kind !== 'dir') return () => undefined;
  const source = state.source;
  const root = source.path;
  const ignored = [await resolveReal(reg.store.home)];
  const relevant = (p: string): boolean => !isIgnoredChange(p, [root], ignored);
  const warn = (m: string): void => stamp(`warning: ${m}`);
  const siteFiles = routingFor(state)?.siteFiles !== false;
  /** The site-file warnings last logged: a re-read repeats them only when they change. */
  const warningsOf = (t: Tree): string => {
    const shown: string[] = [];
    if (siteFiles) checkSiteFiles(t, false, (m) => shown.push(m));
    return shown.join('\n');
  };
  let lastWarnings = warningsOf(current());
  const scheduler = new RebuildScheduler(async () => {
    stamp('Change detected, re-reading');
    live.send(BUILDING_EVENT);
    try {
      const next = await loadServedTree(reg.store, source, { keepOsMetadata: !!state.keepOsMetadata, warn });
      stamp(`Read ${next.size} file${next.size === 1 ? '' : 's'} (${next.digest()})`);
      const warnings = warningsOf(next);
      if (warnings !== lastWarnings && warnings !== '') for (const m of warnings.split('\n')) warn(m);
      lastWarnings = warnings;
      if (siteFiles) checkSiteFiles(next, !!state.strictSiteFiles, () => undefined);
      const event = reloadEventFor(current(), next, !state.noCssInject, !!state.navigateToChanged) ?? OK_EVENT;
      swap(next, await servedLastModified(reg.store, source));
      live.send(event);
    } catch (e) {
      if (!(e instanceof SitefileError)) throw e;
      stamp(`error[${e.code}]: ${e.message}`);
      live.send(errorEvent(e));
      stamp('Read failed; serving the last good copy');
    }
  });
  const close = watchPaths({
    dirs: [root],
    files: [],
    onChange: (p) => {
      if (relevant(p)) scheduler.notify(p);
    },
    ...(state.pollMs !== undefined ? { pollMs: state.pollMs, ignore: (p: string) => !relevant(p) } : {}),
  });
  return () => {
    close();
    scheduler.close();
  };
}

main().catch((e: unknown) => {
  process.stderr.write(`${(e as Error)?.stack ?? String(e)}\n`);
  process.exit(1);
});
