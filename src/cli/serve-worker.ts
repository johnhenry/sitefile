#!/usr/bin/env node
// Detached server process spawned by `sitefile serve -d` / `restart`.
// Usage: node serve-worker.js <server id>. State and logs live in $SITEFILE_HOME/servers.
import { SitefileError } from '../errors.js';
import { closeServer, createSiteServer, routingFor, formatUrl, listen } from '../server.js';
import { checkSiteFiles, loadServedSite, servedLastModified, ServerRegistry } from '../servers.js';

const stamp = (m: string): void => void process.stdout.write(`${new Date().toISOString()} ${m}\n`);

async function main(): Promise<void> {
  const id = process.argv[2] ?? '';
  const reg = await ServerRegistry.open();
  const state = await reg.read(id);
  if (!state) throw new SitefileError('E_SERVER', `no state for server '${id}'`);
  try {
    const { tree, content } = await loadServedSite(reg.store, state.source, { keepOsMetadata: !!state.keepOsMetadata, warn: (m) => stamp(`warning: ${m}`) });
    if (routingFor(state)?.siteFiles !== false) checkSiteFiles(tree, !!state.strictSiteFiles, (m) => stamp(`warning: ${m}`), content);
    const modified = await servedLastModified(reg.store, state.source);
    const server = createSiteServer(tree, { spa: state.spa, ...(state.spaExclude ? { spaExclude: state.spaExclude } : {}), ...(state.spaFallback !== undefined ? { spaFallback: state.spaFallback } : {}), ...(state.base !== undefined ? { base: state.base } : {}), ...(state.rewriteBase !== undefined ? { rewriteBase: state.rewriteBase } : {}), ...(routingFor(state) ? { profile: routingFor(state)! } : {}), ...(state.country !== undefined ? { country: state.country } : {}), serverId: id, lastModified: () => modified, log: stamp, ...(content ? { content } : {}) });
    const addr = await listen(server, state.bind.host, state.bind.port);
    await reg.update(id, { pid: process.pid, host: addr.host, port: addr.port, ready: true, error: undefined });
    stamp(`serving ${state.image} (${tree.size} files) at ${formatUrl(addr.host, addr.port)}${(state.base ?? '/').slice(1)}`);
    const stop = (signal: string) => (): void => {
      stamp(`received ${signal}, shutting down`);
      void closeServer(server).then(async () => {
        await reg.update(id, { ready: false }).catch(() => undefined);
        process.exit(0);
      });
      setTimeout(() => process.exit(0), 3000).unref();
    };
    process.on('SIGTERM', stop('SIGTERM'));
    process.on('SIGINT', stop('SIGINT'));
  } catch (e) {
    const message = e instanceof SitefileError ? `error[${e.code}]: ${e.message}` : `error: ${(e as Error).message}`;
    process.stderr.write(`${new Date().toISOString()} ${message}\n`);
    await reg.update(id, { pid: 0, ready: false, error: message }).catch(() => undefined);
    process.exit(1);
  }
}

main().catch((e: unknown) => {
  process.stderr.write(`${(e as Error)?.stack ?? String(e)}\n`);
  process.exit(1);
});
