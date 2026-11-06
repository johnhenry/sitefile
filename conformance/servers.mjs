// Local servers for the network conformance cases (conformance/run.mjs).
//
// - An HTTPS server on 127.0.0.1 (ephemeral port) with a certificate issued by
//   a throwaway CA (generated with openssl, as in test/helpers/tls.ts). Both
//   CLIs are told to trust it through NODE_EXTRA_CA_CERTS: Node adds it to its
//   default store, and the Rust CLI honors the same variable.
// - The in-process mock OCI distribution registry from
//   test/helpers/mock-registry.ts (plain http on loopback, which both CLIs allow
//   for registries), seeded with a small base image.
//
// Nothing listens on a fixed port, nothing is written outside `dir`, and the
// servers are closed by `close()`.

import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const sha = (b) => 'sha256:' + createHash('sha256').update(b).digest('hex');

/** Fixed bodies served by the HTTPS server. Sitefiles in conformance/cases/net-* pin some of them. */
export const BODIES = {
  '/data.json': '{"title":"Remote data","items":[{"name":"alpha","n":2},{"name":"beta","n":1}]}\n',
  '/style.css': 'body { color: #123456; }\n',
  '/page.md': '---\ntitle: Remote page\n---\n# Hello from the network\n\nSome *emphasis*.\n',
  '/version.txt': 'version 2\n',
  '/big.bin': 'x'.repeat(4096),
};

/**
 * Start the servers. `root` is the repository root (for lib/), `dir` a scratch
 * directory. Returns placeholder values, environment for the CLIs and close().
 */
export async function startServers(root, dir) {
  const lib = (p) => import(pathToFileURL(path.join(root, 'lib', p)).href);
  const { createTestPki, opensslAvailable } = await lib('test/helpers/tls.js');
  const { startMockRegistry } = await lib('test/helpers/mock-registry.js');
  const { writeTar } = await lib('src/archive-writers.js');
  const { makeFile, Tree } = await lib('src/tree.js');
  if (!opensslAvailable()) throw new Error('openssl not found on PATH; the network cases need it to generate a throwaway CA');

  const pki = createTestPki();
  const caFile = path.join(dir, 'conformance-ca.pem');
  let server;
  let registry;
  try {
    await fs.writeFile(caFile, pki.caCert);
    const { cert, key } = pki.issue('IP:127.0.0.1,DNS:localhost');

    const enc = (s) => new TextEncoder().encode(s);
    const tar = Buffer.from(
      writeTar(Tree.fromFiles([['/docs/a.txt', makeFile(enc('archive a\n'), 'conformance')], ['/docs/sub/b.txt', makeFile(enc('archive b\n'), 'conformance')]]), { epoch: 0 }),
    );

    let base = '';
    server = https.createServer({ cert, key }, (req, res) => {
      const url = new URL(req.url ?? '/', base);
      const p = url.pathname;
      const redirect = (status, location) => {
        res.writeHead(status, { location });
        res.end();
      };
      if (p in BODIES) {
        const body = Buffer.from(BODIES[p]);
        res.writeHead(200, { 'content-type': p.endsWith('.json') ? 'application/json' : 'text/plain', 'content-length': String(body.length) });
        return res.end(body);
      }
      if (p === '/site.tar') {
        res.writeHead(200, { 'content-type': 'application/x-tar', 'content-length': String(tar.length) });
        return res.end(tar);
      }
      // /r/<path>: one relative redirect; /r2/<path>: two hops (301 then 302).
      if (p.startsWith('/r/')) return redirect(302, p.slice(2));
      if (p.startsWith('/r2/')) return redirect(301, `/r/${p.slice(4)}`);
      if (p === '/loop') return redirect(302, '/loop');
      if (p === '/to-http') return redirect(302, `http://127.0.0.1:${new URL(base).port}/data.json`);
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found\n');
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    base = `https://127.0.0.1:${server.address().port}`;

    registry = await startMockRegistry();
    const seeded = registry.seedImage('conf/base', '1', {
      layers: [{ '/etc/motd': 'base image\n', '/srv/www/index.html': 'base index\n', '/srv/www/keep.txt': 'kept from base\n' }],
      config: { Cmd: ['/bin/serve'], Env: ['PATH=/bin'], WorkingDir: '/srv/www', Labels: { 'org.example.base': 'yes' } },
    });
    // More tags for `sitefile outdated` (net-outdated-base): a newer one of the same shape and one of another shape.
    registry.seedImage('conf/base', '2', { layers: [{ '/etc/motd': 'base image 2\n' }], config: { Cmd: ['/bin/serve'] } });
    registry.seedImage('conf/base', '1.5', { layers: [{ '/etc/motd': 'base image 1.5\n' }], config: { Cmd: ['/bin/serve'] } });

    return {
      vars: { HTTPS: base, REGISTRY: registry.host },
      env: { NODE_EXTRA_CA_CERTS: caFile },
      info: { tarIntegrity: sha(tar), baseDigest: seeded.manifestDigest },
      async close() {
        server.closeAllConnections?.();
        await new Promise((r) => server.close(() => r()));
        await registry.close();
        pki.dispose();
      },
    };
  } catch (e) {
    server?.close();
    await registry?.close();
    pki.dispose();
    throw e;
  }
}
