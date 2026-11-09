#!/usr/bin/env node
// Detached server process spawned by `sitefile serve -d` / `restart`.
// Usage: node serve-worker.js <server id>. State and logs live in $SITEFILE_HOME/servers.
import { readFile } from 'node:fs/promises';
import { SitefileError } from '../errors.js';
import { BasicAuth, parseHtpasswd } from '../serve-auth.js';
import { closeServer, createSiteServer, formatUrl, jsonLogLine, listen, parseCidr, startedLogLine, stoppedLogLine } from '../server.js';
import { loadServedTree, servedLastModified, ServerRegistry } from '../servers.js';

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
    const tree = await loadServedTree(reg.store, state.source, { keepOsMetadata: !!state.keepOsMetadata, warn: (m) => stamp(`warning: ${m}`) });
    const modified = await servedLastModified(reg.store, state.source);
    const authEntries = [...(state.auth ?? []), ...(state.authFile !== undefined ? parseHtpasswd(await readFile(state.authFile, 'utf8'), state.authFile) : [])];
    const server = createSiteServer(tree, { spa: state.spa, allowedHosts: state.allowedHosts ?? [], cors: !!state.cors, ...(state.isolate ? { isolate: state.isolate } : {}), ...(state.cache ? { cache: state.cache } : {}), ...(json ? { logFormat: 'json' as const } : {}), logIp: !!state.logIp, quiet: !!state.quiet, ...(authEntries.length > 0 || state.authFile !== undefined ? { auth: new BasicAuth(authEntries) } : {}), ...(state.maxConnectionsPerIp ? { maxConnectionsPerIp: state.maxConnectionsPerIp } : {}), ...(state.basePath !== undefined ? { basePath: state.basePath } : {}), ...(ipRules ? { ipRules } : {}), ...(loopback ? {} : { noindex: true }), clientInfo: true, compress: !!state.compress, ...(state.tlsCert !== undefined && state.tlsKey !== undefined ? { tls: { cert: await readFile(state.tlsCert), key: await readFile(state.tlsKey) } } : {}), serverId: id, lastModified: () => modified, log: stamp });
    const addr = await listen(server, state.bind.host, state.bind.port);
    await reg.update(id, { pid: process.pid, host: addr.host, port: addr.port, ready: true, error: undefined });
    const root = formatUrl(addr.host, addr.port, state.tlsCert !== undefined ? 'https' : 'http');
    const url = state.basePath === undefined ? root : `${root.slice(0, -1)}${state.basePath}/`;
    if (json) stamp(startedLogLine(new Date().toISOString(), state.image, tree.size, url));
    else stamp(`serving ${state.image} (${tree.size} files) at ${url}`);
    const stop = (signal: string) => (): void => {
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

main().catch((e: unknown) => {
  process.stderr.write(`${(e as Error)?.stack ?? String(e)}\n`);
  process.exit(1);
});
