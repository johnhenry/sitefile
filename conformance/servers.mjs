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
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
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
  let proxy; // an HTTP proxy (net-proxy-*): tunnels CONNECT, forwards absolute-form requests
  try {
    await fs.writeFile(caFile, pki.caCert);
    const { cert, key } = pki.issue('IP:127.0.0.1,DNS:localhost,DNS:proxied.test');

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
      const redirect = (status, location) => {
        res.writeHead(status, { location });
        res.end();
      };
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
    // A repository of shared stages for the net-sitefile-import-* cases (IMPORT … AS sitefile, issue #97). Its
    // Sitefile reads a file of its own checkout, an https URL and another Git import, so it is made once the
    // server's URL is known.
    const stages = new gitServer.MemRepo();
    const stagesSitefile = [
      'ARG ACCENT = "teal"',
      'IMPORT data FROM ./data.json AS json',
      `IMPORT version FROM ${base}/version.txt AS text`,
      `IMPORT docs FROM git+${base}/repo.git#v1:docs AS markdown SORT BY title`,
      'STAGE assets',
      'RENDER <<CSS TO /theme.css',
      ':root { --accent: ${ACCENT}; } /* ${data.name}: ${version} */',
      'CSS',
      'STAGE layout',
      'COPY ./layout/ /layout/',
      'RENDER <<HTML TO /docs.html',
      '${#each docs as d}<li>${d.title}</li>${/each}',
      'HTML',
      'FROM scratch',
      'EXPORT /',
      '',
    ].join('\n');
    const stagesV1 = stages.commit({
      'theme/Sitefile': stagesSitefile,
      'theme/data.json': '{"name": "stages v1"}\n',
      'theme/layout/page.html': '<main>${page}</main>\n',
      'README.md': 'shared stages\n',
    });
    stages.tag('v1', stagesV1);
    const stagesV2 = stages.commit({
      'theme/Sitefile': stagesSitefile,
      'theme/data.json': '{"name": "stages v2"}\n',
      'theme/layout/page.html': '<main>${page}</main>\n',
      'README.md': 'shared stages\n',
    });
    gitRepos.set('/stages.git', stages);
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

    // `*.test` names exist only for the proxy (it sends them to 127.0.0.1), so a fetch of
    // ${PROXIED} succeeds only through it; CONNECT to refused.test is answered 403.
    const toLoopback = (host) => (host.endsWith('.test') ? '127.0.0.1' : host);
    proxy = http.createServer((req, res) => {
      const target = new URL(req.url ?? '');
      const up = http.request({ host: toLoopback(target.hostname), port: target.port || 80, path: target.pathname + target.search, method: req.method, headers: req.headers }, (r) => {
        res.writeHead(r.statusCode ?? 502, r.headers);
        r.pipe(res);
      });
      up.on('error', () => res.destroy());
      req.pipe(up);
    });
    const tunnels = new Set();
    proxy.on('connect', (req, client, head) => {
      tunnels.add(client);
      client.on('close', () => tunnels.delete(client));
      const m = /^(.*):(\d+)$/.exec(req.url ?? '');
      const host = (m?.[1] ?? '').replace(/^\[|\]$/g, '');
      if (!m || host === 'refused.test') return void client.end('HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n');
      const upstream = net.connect(Number(m[2]), toLoopback(host), () => {
        client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
        if (head.length) upstream.write(head);
        upstream.pipe(client);
        client.pipe(upstream);
      });
      upstream.on('error', () => client.destroy());
      client.on('error', () => upstream.destroy());
    });
    await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
    const proxyUrl = `http://127.0.0.1:${proxy.address().port}`;

    return {
      vars: { HTTPS: base, PROXIED: base.replace('127.0.0.1', 'proxied.test'), PROXY: proxyUrl, REGISTRY: registry.host, GIT_V1: v1, GIT_V2: v2, STAGES_V1: stagesV1, STAGES_V2: stagesV2 },
      env: { NODE_EXTRA_CA_CERTS: caFile },
      info: { tarIntegrity: sha(tar), baseDigest: seeded.manifestDigest },
      /** sha256 of the manifest the mock registry holds under `repo:tag` (or `repo@digest`), or null. */
      manifestDigest: (key) => {
        const b = registry.manifests.get(key);
        return b ? sha(b) : null;
      },
      async close() {
        for (const t of tunnels) t.destroy();
        proxy.closeAllConnections?.();
        await new Promise((r) => proxy.close(() => r()));
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
    proxy?.close();
    await registry?.close();
    pki.dispose();
    throw e;
  }
}
