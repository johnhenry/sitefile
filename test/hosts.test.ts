// `sitefile hosts` (issue #176): the plan-time host list.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { hosts, hostsJson } from '../src/hosts.js';
import { formatHosts } from '../src/cli/hosts.js';
import { runCli, tmpDir } from './helpers/cli.js';

const servers: Server[] = [];
after(() => servers.forEach((s) => s.close()));

async function ctx(sitefile: string): Promise<string> {
  const dir = await tmpDir('sitefile-hosts-');
  await fs.writeFile(path.join(dir, 'Sitefile'), sitefile);
  return dir;
}

const SITEFILE = `FROM scratch AS a
IMPORT css FROM https://cdn.example.com/a.css
IMPORT js FROM https://CDN.example.com./b.js SECRET tok AS BEARER SECRET tok AS HEADER X-Tok
IMPORT repo FROM git+https://git.example.org/r.git#main:site
IMPORT theme FROM git+https://themes.test:8443/t.git#main AS sitefile
IMPORT here FROM ./Sitefile AS text
STAGE img FROM image ghcr.io/x/site:1
STAGE mine FROM image site:1
FROM image localhost:5000/a/b:2
CHECKLINKS --external
EXPORT / AS oci BASE nginx:alpine
`;

test('hosts lists every remote input by host and port, from the plan only', async () => {
  const dir = await ctx(SITEFILE);
  const r = await hosts({ contextDir: dir });
  assert.equal(r.strict, false);
  assert.deepEqual(
    r.hosts.map((h) => [h.host, h.port, h.status, h.uses.map((u) => `${u.kind}:${u.name}`)]),
    [
      ['cdn.example.com', 443, 'allowed', ['import:css', 'import:js']],
      ['git.example.org', 443, 'allowed', ['import:repo']],
      ['themes.test', 8443, 'allowed', ['import:theme']],
      ['ghcr.io', 443, 'allowed', ['image:img']],
      ['localhost', 5000, 'allowed', ['image:#4']],
      ['registry-1.docker.io', 443, 'allowed', ['base:BASE']],
    ],
  );
  const js = r.hosts[0]?.uses[1];
  assert.deepEqual(js?.secrets, ['tok']);
  assert.equal(js?.span?.line, 3);
  assert.equal(r.hosts[1]?.uses[0]?.source, 'git+https://git.example.org/r.git#main:site');
  assert.equal(r.hosts[2]?.uses[0]?.nested, true);
  assert.equal(r.hosts.find((h) => h.host === 'registry-1.docker.io')?.uses[0]?.source, 'docker.io/library/nginx:alpine');
  assert.equal(r.hosts.find((h) => h.host === 'registry-1.docker.io')?.uses[0]?.span, null);
  assert.deepEqual(r.checklinksExternal, [10]);
  // --base replaces the EXPORT BASE; a non-oci export has none.
  const b = await hosts({ contextDir: dir, baseImage: 'quay.io/a/b:1' });
  assert.equal(b.hosts.at(-1)?.host, 'quay.io');
  const plain = await hosts({ contextDir: await ctx('FROM scratch\nIMPORT here FROM ./Sitefile AS text\nEXPORT /\n') });
  assert.deepEqual(plain, { strict: false, hosts: [], checklinksExternal: [] });
  assert.equal(formatHosts(plain), 'No remote inputs: the Sitefile names no host to contact\n');
});

test('hosts applies the rules: allowed, blocked (with needs), denied, and the secret rule in strict mode', async () => {
  const dir = await ctx(SITEFILE);
  const r = await hosts({
    contextDir: dir,
    fetchPolicy: { network: { strict: true, allowHosts: ['example.com', 'ghcr.io'], denyHosts: ['localhost'] } },
  });
  assert.equal(r.strict, true);
  const by = new Map(r.hosts.map((h) => [h.host, h]));
  assert.equal(by.get('cdn.example.com')?.status, 'allowed');
  assert.equal(by.get('cdn.example.com')?.needs, null);
  assert.equal(by.get('git.example.org')?.status, 'blocked');
  assert.deepEqual(by.get('git.example.org')?.needs, { kind: 'host', name: 'git.example.org', flag: '--allow-hosts' });
  assert.equal(by.get('localhost')?.status, 'denied');
  assert.equal(by.get('localhost')?.needs, null);
  // The secret import needs its rule; the other use of the host does not.
  assert.deepEqual(by.get('cdn.example.com')?.uses[1]?.needs, { kind: 'secret-host', name: 'tok=cdn.example.com', flag: '--allow-secret-host' });
  assert.equal(by.get('cdn.example.com')?.uses[0]?.needs, null);
  const ok = await hosts({ contextDir: dir, fetchPolicy: { network: { strict: true, allowHosts: ['example.com'], secretHosts: ['tok=cdn.example.com'] } } });
  assert.equal(ok.hosts[0]?.uses[1]?.needs, null);
  // Open mode: no secret rule needed.
  assert.equal((await hosts({ contextDir: dir, fetchPolicy: { network: { allowHosts: ['example.com'] } } })).hosts[0]?.uses[1]?.needs, null);
  const text = formatHosts(r);
  assert.match(text, /^HOST {18}PORT {2}STATUS {3}USED BY\n/);
  assert.match(text, /\ngit\.example\.org: needs --allow-hosts git\.example\.org\n/);
  assert.match(text, /IMPORT js \(\S*Sitefile:3\): secret needs --allow-secret-host tok=cdn\.example\.com\n/);
  assert.match(text, /IMPORT theme \(\S*Sitefile:5\): an imported Sitefile; the hosts it needs are known once it is fetched\n/);
  assert.match(text, /\n6 hosts \(strict network mode\): 2 allowed, 3 blocked, 1 denied\nCHECKLINKS --external \(line 10\) also contacts the hosts of external links\n$/);
  const j = JSON.parse(hostsJson(r)) as { hosts: Array<Record<string, unknown>> };
  assert.deepEqual(Object.keys(j), ['strict', 'hosts', 'checklinksExternal']);
  assert.deepEqual(Object.keys(j.hosts[0] ?? {}), ['host', 'port', 'status', 'needs', 'uses']);
});

test('sitefile hosts contacts nothing, even for hosts it reports allowed', async () => {
  let requests = 0;
  const server = createServer((_q, res) => {
    requests++;
    res.end('x');
  });
  servers.push(server);
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  const dir = await ctx(`FROM scratch\nIMPORT a FROM ${base}/a.css\nIMPORT g FROM git+${base}/r.git#main\nFROM image 127.0.0.1:${(server.address() as AddressInfo).port}/a/b:1\nEXPORT / AS oci BASE 127.0.0.1:${(server.address() as AddressInfo).port}/n:1\n`);
  const home = path.join(dir, 'home');
  const r = await runCli(home, ['hosts', dir, '--json']);
  assert.equal(r.code, 0, r.stderr);
  assert.equal((JSON.parse(r.stdout) as { hosts: unknown[] }).hosts.length, 1);
  const text = await runCli(home, ['hosts', dir, '--network', 'strict']);
  assert.equal(text.code, 0, text.stderr);
  assert.match(text.stdout, /127\.0\.0\.1: needs --allow-hosts 127\.0\.0\.1\n/);
  assert.equal(requests, 0);
  // Errors: --json gives the JSON error; a bad flag value is a usage error.
  await fs.writeFile(path.join(dir, 'Sitefile'), 'FROM image Bad Ref\nEXPORT /\n');
  const bad = await runCli(home, ['hosts', dir, '--json']);
  assert.equal(bad.code, 1);
  assert.equal((JSON.parse(bad.stdout) as { error: { code: string } }).error.code.startsWith('E_'), true);
  const usage = await runCli(home, ['hosts', dir, '--network', 'sometimes']);
  assert.equal(usage.code, 2);
});
