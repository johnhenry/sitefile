// Local servers for the network conformance cases (conformance/run.mjs).
//
// - An HTTPS server on 127.0.0.1 (ephemeral port) with a certificate issued by
//   a throwaway CA (generated with openssl, as in test/helpers/tls.ts). Both
//   CLIs are told to trust it through NODE_EXTRA_CA_CERTS: Node adds it to its
//   default store, and the Rust CLI honors the same variable.
// - The in-process mock OCI distribution registry from
//   test/helpers/mock-registry.ts (plain http on loopback, which both CLIs allow
//   for registries), seeded with a small base image and a theme site image.
//
// Nothing listens on a fixed port, nothing is written outside `dir`, and the
// servers are closed by `close()`.

import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import https from 'node:https';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const sha = (b) => 'sha256:' + createHash('sha256').update(b).digest('hex');

/** The build secret the /private/* endpoints and private.git accept (passed as API_TOKEN in net-secret-* cases). */
export const SECRET = 'conformance-secret-7c1e';

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
  const gitServer = await lib('test/helpers/git-server.js');
  if (!opensslAvailable()) throw new Error('openssl not found on PATH; the network cases need it to generate a throwaway CA');

  const pki = createTestPki();
  const caFile = path.join(dir, 'conformance-ca.pem');
  let server;
  let other; // a second HTTPS origin (another port): cross-origin redirects must not carry secrets
  let registry;
  try {
    await fs.writeFile(caFile, pki.caCert);
    const { cert, key } = pki.issue('IP:127.0.0.1,DNS:localhost');

    const enc = (s) => new TextEncoder().encode(s);
    const tar = Buffer.from(
      writeTar(Tree.fromFiles([['/docs/a.txt', makeFile(enc('archive a\n'), 'conformance')], ['/docs/sub/b.txt', makeFile(enc('archive b\n'), 'conformance')]]), { epoch: 0 }),
    );

    // Git smart HTTP (protocol v2) repositories for the net-git-* cases (test/helpers/git-server.ts).
    const repo = new gitServer.MemRepo();
    const shared = 'line shared by both versions\n'.repeat(200);
    const v1 = repo.commit({
      'Sitefile': 'FROM scratch\nCOPY ./site/ /\nEXPORT /\n',
      'site/index.html': '<h1>v1</h1>\n',
      'site/run.sh': { data: '#!/bin/sh\n', mode: '100755' },
      'site/home.html': { data: 'index.html', mode: '120000' },
      'docs/a.md': '---\ntitle: A\n---\n# A\n',
      'docs/b.md': '---\ntitle: B\n---\n# B\n',
      'docs/big.txt': shared + 'one\n',
      'docs/big2.txt': shared + 'two\n',
      'README.md': 'readme v1\n',
    });
    repo.tag('v1', v1);
    const v2 = repo.commit({ 'Sitefile': 'FROM scratch\nCOPY ./site/ /\nEXPORT /\n', 'site/index.html': '<h1>v2</h1>\n', 'README.md': 'readme v2\n' });
    const bad = new gitServer.MemRepo();
    bad.commit({ 'ok.txt': 'ok\n', 'mod': { gitlink: '1'.repeat(40) }, 'esc/link': { data: '../../outside', mode: '120000' } });
    // A private repository for the net-secret-* cases (issue #85): requests need the build secret.
    const priv = new gitServer.MemRepo();
    priv.commit({ 'docs/private.txt': 'private repository file\n' });
    priv.auth = `Bearer ${SECRET}`;
    const gitRepos = new Map([['/repo.git', repo], ['/bad.git', bad], ['/dumb.git', repo], ['/private.git', priv]]);

    let base = '';
    let otherBase = '';
    server = https.createServer({ cert, key }, (req, res) => {
      const url = new URL(req.url ?? '/', base);
      const p = url.pathname;
      if (gitServer.handleGit(req, res, url, { repos: gitRepos })) return;
      // Build secrets (net-secret-*): /private/* needs `x-api-key` or `authorization: Bearer` with SECRET.
      if (p.startsWith('/private/')) {
        const authed = req.headers['x-api-key'] === SECRET || req.headers.authorization === `Bearer ${SECRET}`;
        if (p === '/private/away') return redirect(302, `${otherBase}/private/echo`);
        if (p === '/private/same') return redirect(302, '/private/data.json');
        // Reflects the credential into a redirect URL, so it would end up in an error message.
        if (p === '/private/leak') return redirect(302, `/private/missing?k=${String(req.headers['x-api-key'])}`);
        if (!authed) return res.writeHead(401, { 'content-type': 'text/plain' }).end('unauthorized\n');
        if (p === '/private/data.json') return res.writeHead(200, { 'content-type': 'application/json' }).end('{"title":"Private data"}\n');
        if (p === '/private/note.txt') return res.writeHead(200, { 'content-type': 'text/plain' }).end('private note\n');
        return res.writeHead(404).end();
      }
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
    // The other origin reports whether any credential arrived.
    other = https.createServer({ cert, key }, (req, res) => {
      const got = req.headers['x-api-key'] !== undefined || req.headers.authorization !== undefined;
      res.writeHead(200, { 'content-type': 'text/plain' }).end(got ? 'credential received\n' : 'no credential\n');
    });
    await new Promise((resolve) => other.listen(0, '127.0.0.1', resolve));
    otherBase = `https://127.0.0.1:${other.address().port}`;

    registry = await startMockRegistry();
    const seeded = registry.seedImage('conf/base', '1', {
      layers: [{ '/etc/motd': 'base image\n', '/srv/www/index.html': 'base index\n', '/srv/www/keep.txt': 'kept from base\n' }],
      config: { Cmd: ['/bin/serve'], Env: ['PATH=/bin'], WorkingDir: '/srv/www', Labels: { 'org.example.base': 'yes' } },
    });
    // More tags for `sitefile outdated` (net-outdated-base): a newer one of the same shape and one of another shape.
    registry.seedImage('conf/base', '2', { layers: [{ '/etc/motd': 'base image 2\n' }], config: { Cmd: ['/bin/serve'] } });
    registry.seedImage('conf/base', '1.5', { layers: [{ '/etc/motd': 'base image 1.5\n' }], config: { Cmd: ['/bin/serve'] } });
    // A Sitefile theme image for `FROM image` (net-from-image-*): one layer under /srv/www with the root label, and a newer tag.
    const theme = { Labels: { 'dev.sitefile.root': '/srv/www', 'org.opencontainers.image.title': 'theme' } };
    registry.seedImage('conf/theme', '1', {
      layers: [{ '/srv/www/index.html': 'theme index\n', '/srv/www/theme.css': 'body { font: 16px serif; }\n', '/srv/www/layout/page.html': '<main>${content}</main>\n' }],
      config: theme,
    });
    registry.seedImage('conf/theme', '2', { layers: [{ '/srv/www/theme.css': 'body { font: 18px serif; }\n' }], config: theme });

    return {
      vars: { HTTPS: base, REGISTRY: registry.host, GIT_V1: v1, GIT_V2: v2 },
      env: { NODE_EXTRA_CA_CERTS: caFile },
      info: { tarIntegrity: sha(tar), baseDigest: seeded.manifestDigest },
      /** sha256 of the manifest the mock registry holds under `repo:tag` (or `repo@digest`), or null. */
      manifestDigest: (key) => {
        const b = registry.manifests.get(key);
        return b ? sha(b) : null;
      },
      async close() {
        server.closeAllConnections?.();
        await new Promise((r) => server.close(() => r()));
        other.closeAllConnections?.();
        await new Promise((r) => other.close(() => r()));
        await registry.close();
        pki.dispose();
      },
    };
  } catch (e) {
    server?.close();
    other?.close();
    await registry?.close();
    pki.dispose();
    throw e;
  }
}
