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

import { spawn } from 'node:child_process';
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

// ---------------------------------------------------------------- serve cases
//
// A case with case.json `"command": "serve"` starts `sitefile serve <site>` (default
// `site`, relative to the case directory; `args` go before it) with each CLI, sends
// every request in `requests` over its own connection (`connection: close`), and
// requires the same answer from both: interim (1xx) and final status lines, every
// header but `date`, `connection` and `keep-alive` (sorted by name), and the body
// bytes. A request is `{ method = "GET", target, headers = {}, version = "1.1", expect }`;
// `target` is sent as written (so an absolute-form target works). `expect` checks both
// answers: `status`, `headers` (name → exact value, null for absent, true for present), `body`
// (exact text) and `interim` (the list of 1xx statuses).

const IGNORED_HEADERS = new Set(['date', 'connection', 'keep-alive']);

/** Send one raw HTTP/1.1 request; resolve with { interim, status, headers, body }. */
function rawRequest(port, { method = 'GET', target, headers = {}, version = '1.1' }) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(port, '127.0.0.1');
    const chunks = [];
    const lines = [`${method} ${target} HTTP/${version}`, `host: 127.0.0.1:${port}`, ...Object.entries(headers).map(([k, v]) => `${k}: ${v}`), 'connection: close', '', ''];
    sock.on('connect', () => sock.write(lines.join('\r\n')));
    sock.on('data', (d) => chunks.push(d));
    sock.on('error', reject);
    sock.setTimeout(10000, () => sock.destroy(new Error(`timeout: ${method} ${target}`)));
    sock.on('close', () => {
      let buf = Buffer.concat(chunks);
      const interim = [];
      for (;;) {
        const end = buf.indexOf('\r\n\r\n');
        if (end === -1) return reject(new Error(`incomplete response to ${method} ${target}`));
        const head = buf.subarray(0, end).toString('latin1').split('\r\n');
        buf = buf.subarray(end + 4);
        const status = Number(head[0].split(' ')[1]);
        const hs = head
          .slice(1)
          .map((l) => [l.slice(0, l.indexOf(':')).trim().toLowerCase(), l.slice(l.indexOf(':') + 1).trim()])
          .filter(([k]) => !IGNORED_HEADERS.has(k))
          .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
        if (status >= 100 && status < 200) {
          interim.push({ status, headers: hs });
          continue;
        }
        return resolve({ interim, status, headers: hs, body: buf });
      }
    });
  });
}

/** Start `serve` with one CLI and resolve with { port, stop }. */
function startServe(cmd, argv, env, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd[0], [...cmd.slice(1), ...argv], { cwd, env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`serve did not start: ${err || out}`));
    }, 30000);
    child.stderr.on('data', (d) => (err += d));
    child.stdout.on('data', (d) => {
      out += d;
      const m = / at http:\/\/127\.0\.0\.1:(\d+)\//.exec(out);
      if (!m) return;
      clearTimeout(timer);
      resolve({
        port: Number(m[1]),
        stop: () => new Promise((r) => (child.exitCode !== null ? r() : (child.once('exit', () => r()), child.kill()))),
      });
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`serve exited (${code}): ${err || out}`));
    });
  });
}

const show = (r) =>
  [
    ...r.interim.map((i) => `${i.status} ${i.headers.map(([k, v]) => `${k}: ${v}`).join(' | ')}`),
    `${r.status}`,
    ...r.headers.map(([k, v]) => `${k}: ${v}`),
    `body ${r.body.length} bytes sha256:${createHash('sha256').update(r.body).digest('hex').slice(0, 16)}`,
  ].join('\n');

/** Run a `command: "serve"` case with both CLIs. Returns { name, compare, pass, detail, ts, rust }. */
export async function runServeCase(c, { impls, tmp }) {
  const detail = [];
  const answers = {};
  for (const [impl, cmd] of impls) {
    const home = path.join(tmp, impl, '.home');
    await fs.mkdir(home, { recursive: true });
    let server;
    try {
      server = await startServe(cmd, ['serve', ...(c.args ?? []), `./${c.site ?? 'site'}`], { SITEFILE_HOME: home, ...(c.env ?? {}) }, c.context);
      answers[impl] = [];
      for (const r of c.requests) answers[impl].push(await rawRequest(server.port, r));
    } catch (e) {
      detail.push(`${impl}: ${e.message}`);
    } finally {
      await server?.stop();
    }
  }
  if (detail.length === 0) {
    c.requests.forEach((r, i) => {
      const label = `${r.method ?? 'GET'} ${r.target}${r.headers ? ' ' + JSON.stringify(r.headers) : ''}`;
      const [a, b] = impls.map(([impl]) => answers[impl][i]);
      const sa = show(a);
      const sb = show(b);
      if (sa !== sb || !a.body.equals(b.body)) detail.push(`${label}: answers differ\nts:\n${sa}\nrust:\n${sb}`);
      const want = r.expect ?? {};
      for (const [impl] of impls) {
        const got = answers[impl][i];
        const get = (n) => got.headers.find(([k]) => k === n)?.[1] ?? null;
        if (want.status !== undefined && got.status !== want.status) detail.push(`${impl} ${label}: status ${got.status}, expected ${want.status}`);
        for (const [n, v] of Object.entries(want.headers ?? {})) if (v === true ? get(n) === null : get(n) !== v) detail.push(`${impl} ${label}: ${n} ${JSON.stringify(get(n))}, expected ${JSON.stringify(v)}`);
        if (want.body !== undefined && got.body.toString('utf8') !== want.body) detail.push(`${impl} ${label}: body ${JSON.stringify(got.body.toString('utf8').slice(0, 200))}, expected ${JSON.stringify(want.body)}`);
        if (want.interim !== undefined && JSON.stringify(got.interim.map((x) => x.status)) !== JSON.stringify(want.interim)) {
          detail.push(`${impl} ${label}: interim ${JSON.stringify(got.interim.map((x) => x.status))}, expected ${JSON.stringify(want.interim)}`);
        }
      }
    });
  }
  const summary = (impl) => ({ requests: c.requests.length, statuses: (answers[impl] ?? []).map((a) => a.status) });
  return { name: c.name, compare: 'serve', pass: detail.length === 0, detail, ts: summary('ts'), rust: summary('rust') };
}
