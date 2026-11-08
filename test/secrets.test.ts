import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import { createServer, type IncomingHttpHeaders, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { isSitefileError } from '../src/errors.js';
import { loadSecrets, redactSecrets } from '../src/secrets.js';
import { build } from '../src/executor.js';
import { BuildMemo } from '../src/memo.js';
import { MemRepo, handleGit } from './helpers/git-server.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

// Build secrets for remote IMPORTs (issue #85): `--secret id=…,env=…|src=…` and
// `IMPORT … SECRET <id> AS HEADER <Name>` / `AS BEARER`. The value must never reach the output,
// cache keys, Sitefile.lock, vendor manifests, provenance, --profile traces, logs or error
// messages, nor another origin on redirect.

const SECRET = 'tok-3f9c1d2e-SECRET-VALUE';
const seen: Array<{ port: string; path: string; headers: IncomingHttpHeaders }> = [];

function listen(handler: (req: import('node:http').IncomingMessage, res: import('node:http').ServerResponse) => void): Promise<{ server: Server; base: string }> {
  const server = createServer(handler);
  return new Promise((r) => server.listen(0, '127.0.0.1', () => r({ server, base: `http://127.0.0.1:${(server.address() as AddressInfo).port}` })));
}

const other = await listen((req, res) => {
  seen.push({ port: 'other', path: req.url ?? '', headers: req.headers });
  res.writeHead(200, { 'content-type': 'text/plain' }).end('from the other origin\n');
});
const repo = new MemRepo();
repo.commit({ 'docs/a.txt': 'private a\n' });
repo.auth = `Bearer ${SECRET}`;
const main = await listen((req, res) => {
  const url = new URL(req.url ?? '/', 'http://x');
  seen.push({ port: 'main', path: url.pathname, headers: req.headers });
  if (handleGit(req, res, url, { repos: new Map([['/private.git', repo]]) })) return;
  const ok = req.headers['x-api-key'] === SECRET || req.headers.authorization === `Bearer ${SECRET}`;
  if (url.pathname === '/data.json') {
    if (!ok) return void res.writeHead(401).end();
    return void res.writeHead(200, { 'content-type': 'application/json' }).end('{"title":"private data"}');
  }
  if (url.pathname === '/same') return void res.writeHead(302, { location: '/data.json' }).end();
  if (url.pathname === '/away') return void res.writeHead(302, { location: `${other.base}/landing` }).end();
  // A server that puts the credential it received into a redirect URL (so into an error message).
  if (url.pathname === '/leak') return void res.writeHead(302, { location: `/missing?k=${String(req.headers['x-api-key'])}` }).end();
  res.writeHead(404).end();
});
after(() => {
  main.server.close();
  other.server.close();
});
const NET = ['--allow-http', '--allow-private-network'];
const ENV = { API_TOKEN: SECRET };

test('parser: SECRET clauses (AS HEADER, AS BEARER), in any clause order, and their errors', () => {
  const p = parseSitefile(`IMPORT d FROM https://e.x/d.json SECRET k AS HEADER X-Api-Key AS json SECRET t AS BEARER\nEXPORT /\n`, 'Sitefile');
  const ins = p[0] as { secrets?: Array<{ id: string; header: string; bearer: boolean }>; as?: string };
  assert.equal(ins.as, 'json');
  assert.deepEqual(ins.secrets?.map((s) => [s.id, s.header, s.bearer]), [['k', 'X-Api-Key', false], ['t', 'Authorization', true]]);
  const err = (src: string): string => {
    try {
      parseProgram(src, 'Sitefile');
      plan(parseProgram(src, 'Sitefile'), {});
      return 'ok';
    } catch (e) {
      return isSitefileError(e) ? `${e.code} ${e.detail}` : String(e);
    }
  };
  assert.match(err('IMPORT d FROM https://e.x/d SECRET k\nEXPORT /'), /^E_SYNTAX expected AS/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET k AS COOKIE\nEXPORT /'), /^E_SYNTAX expected HEADER <name> or BEARER/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET k AS HEADER\nEXPORT /'), /^E_SYNTAX expected a header name/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET k AS HEADER X:y\nEXPORT /'), /^E_SYNTAX invalid header name/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET k AS HEADER Host\nEXPORT /'), /^E_SYNTAX header 'Host' is set by sitefile/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET a AS BEARER SECRET b AS HEADER authorization\nEXPORT /'), /^E_SYNTAX header 'authorization' is set by two SECRET clauses/);
  assert.match(err('IMPORT d FROM https://e.x/d SECRET 1x AS BEARER\nEXPORT /'), /^E_SYNTAX expected secret id/);
  assert.match(err('IMPORT d FROM ./local.json SECRET k AS BEARER\nEXPORT /'), /^E_VALIDATION IMPORT d: SECRET applies to remote imports/);
  assert.equal(err('IMPORT d FROM git+https://e.x/r.git SECRET k AS BEARER\nEXPORT /'), 'ok');
  // validate --json shows ids and header names, never values (there are none in the Sitefile).
  const spec = plan(parseProgram('IMPORT d FROM https://e.x/d secret K as header X-K\nEXPORT /', 'Sitefile'), {}).imports[0]!;
  assert.deepEqual(spec.secrets, [{ id: 'K', header: 'X-K', bearer: false }]);
});

test('loadSecrets: env= and src= (one trailing newline dropped); errors never show the value', async () => {
  const dir = await tmpDir();
  const file = path.join(dir, 'tok');
  await fs.writeFile(file, SECRET + '\n');
  const s = await loadSecrets(['id=a,env=API_TOKEN', `id=b,src=${file}`], { API_TOKEN: SECRET });
  assert.deepEqual([...s], [['a', SECRET], ['b', SECRET]]);
  const code = async (flags: string[], env: NodeJS.ProcessEnv = {}): Promise<string> => {
    try {
      await loadSecrets(flags, env);
      return 'ok';
    } catch (e) {
      assert.ok(isSitefileError(e));
      assert.ok(!e.message.includes(SECRET), e.message);
      return e.code;
    }
  };
  assert.equal(await code([SECRET]), 'E_USAGE');
  assert.equal(await code([`id=a,value=${SECRET}`]), 'E_USAGE');
  assert.equal(await code(['id=a']), 'E_USAGE');
  assert.equal(await code(['id=a,env=X,src=y']), 'E_USAGE');
  assert.equal(await code(['id=a,env=API_TOKEN', 'id=a,env=API_TOKEN'], ENV), 'E_USAGE');
  assert.equal(await code(['id=a,env=NOPE']), 'E_SECRET');
  assert.equal(await code([`id=a,src=${path.join(dir, 'missing')}`]), 'E_SECRET');
  assert.equal(await code(['id=a,env=V'], { V: '' }), 'E_SECRET');
  // Line breaks are allowed in a value (a PEM key) but never sent as a header (see test/swbn.test.ts);
  // other control characters are refused.
  assert.equal(await code(['id=a,env=V'], { V: `${SECRET}\r\nX-Injected: 1` }), 'ok');
  assert.equal(await code(['id=a,env=V'], { V: `${SECRET}\u0000` }), 'E_SECRET');
  assert.equal(redactSecrets(`a ${SECRET} b ${SECRET}`, s), 'a *** b ***');
});

test('a secret is sent as the named header (or Bearer), only to the import origin; missing and unused secrets', async () => {
  const ctx = await makeSite({}, `FROM scratch\nIMPORT d FROM ${main.base}/same SECRET k AS HEADER X-Api-Key AS json\nIMPORT b FROM ${main.base}/data.json AS json SECRET t AS BEARER\nIMPORT o FROM ${main.base}/away SECRET k AS HEADER X-Api-Key AS text\nRENDER <<T TO /i.txt\n\${d.title} \${b.title} \${o}\nT\nEXPORT /\n`);
  seen.length = 0;
  const warnings: string[] = [];
  const secrets = new Map([['k', SECRET], ['t', SECRET], ['extra', 'unused-value-123']]);
  const r = await build({ contextDir: ctx, fetchPolicy: { allowHttp: true, allowPrivateNetwork: true }, secrets, onWarning: (w) => warnings.push(w) });
  assert.equal(new TextDecoder().decode(r.output.get('/i.txt')!.bytes), 'private data private data from the other origin\n\n');
  // Same-origin redirect: still sent. Cross-origin redirect: dropped.
  assert.ok(seen.filter((s) => s.port === 'main' && s.path === '/data.json').every((s) => s.headers['x-api-key'] === SECRET || s.headers.authorization === `Bearer ${SECRET}`));
  const landing = seen.find((s) => s.port === 'other')!;
  assert.equal(landing.headers['x-api-key'], undefined);
  assert.equal(landing.headers.authorization, undefined);
  assert.ok(warnings.includes('--secret extra is not used by any IMPORT … SECRET or EXPORT … KEY'), warnings.join('\n'));
  await assert.rejects(build({ contextDir: ctx, fetchPolicy: { allowHttp: true, allowPrivateNetwork: true }, onWarning: () => {} }), (e: unknown) => isSitefileError(e, 'E_SECRET') && /secret 'k' was not given/.test((e as Error).message));
  // The memo (incremental rebuild keys) holds no secret.
  const memo = new BuildMemo();
  await build({ contextDir: ctx, fetchPolicy: { allowHttp: true, allowPrivateNetwork: true }, secrets, memo, onWarning: () => {} });
  assert.ok(!JSON.stringify([...(memo as unknown as { entries: Map<string, unknown> }).entries?.keys?.() ?? []]).includes(SECRET));
});

test('errors are redacted even when a server reflects the secret into a URL', async () => {
  const ctx = await makeSite({}, `FROM scratch\nIMPORT d FROM ${main.base}/leak SECRET k AS HEADER X-Api-Key\nEXPORT /\n`);
  const home = await tmpDir();
  const r = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'out'), ...NET, '--secret', 'id=k,env=API_TOKEN'], undefined, ENV);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /error\[E_FETCH_STATUS\]: .*\/missing\?k=\*\*\*/);
  assert.ok(!r.stderr.includes(SECRET) && !r.stdout.includes(SECRET));
});

async function grepTree(dir: string, needle: string): Promise<string[]> {
  const hits: string[] = [];
  const walk = async (d: string): Promise<void> => {
    for (const e of await fs.readdir(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) await walk(p);
      else if (e.isFile() && (await fs.readFile(p)).includes(needle)) hits.push(p);
    }
  };
  await walk(dir).catch(() => undefined);
  return hits;
}

test('the secret never reaches the output, Sitefile.lock, the vendor directory, provenance, the --profile trace, SITEFILE_HOME or the logs', async () => {
  const ctx = await makeSite(
    {},
    `FROM scratch\nIMPORT d FROM ${main.base}/data.json AS json SECRET k AS HEADER X-Api-Key\nIMPORT docs FROM git+${main.base}/private.git#main:docs SECRET k2 AS BEARER\nCOPY docs: /docs/\nRENDER <<T TO /i.txt\n\${d.title}\nT\nEXPORT / AS oci\n`,
  );
  const home = await tmpDir();
  const tokFile = path.join(await tmpDir(), 'k2');
  await fs.writeFile(tokFile, SECRET + '\n');
  const flags = ['--secret', 'id=k,env=API_TOKEN', '--secret', `id=k2,src=${tokFile}`];
  const out = await tmpDir();
  const runs = [
    await runCli(home, ['vendor', ctx, ...NET, ...flags], undefined, ENV),
    await runCli(home, ['build', ctx, ...NET, ...flags, '-o', path.join(out, 'site.oci.tar'), '--provenance', '--sbom', '--profile', path.join(out, 'trace.json'), '--no-cache'], undefined, ENV),
    await runCli(home, ['build', ctx, ...NET, ...flags, '-o', path.join(out, 'dir'), '--format', 'directory', '--update-lock'], undefined, ENV),
    await runCli(home, ['outdated', ctx, ...NET, ...flags, '--json'], undefined, ENV),
    await runCli(home, ['validate', ctx, '--json'], undefined, ENV),
  ];
  for (const r of runs) {
    assert.equal(r.code, 0, r.stderr);
    assert.ok(!r.stdout.includes(SECRET) && !r.stderr.includes(SECRET), r.stdout + r.stderr);
  }
  assert.equal(await fs.readFile(path.join(out, 'dir', 'docs', 'a.txt'), 'utf8'), 'private a\n');
  assert.match(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8'), /private\.git#main:docs/);
  assert.ok(JSON.parse(await fs.readFile(path.join(ctx, 'vendor/sitefile/manifest.json'), 'utf8')).git);
  assert.deepEqual(await grepTree(ctx, SECRET), [], 'context: Sitefile.lock, vendor directory');
  assert.deepEqual(await grepTree(out, SECRET), [], 'outputs, provenance and SBOM in the OCI archive, the profile trace');
  assert.deepEqual(await grepTree(home, SECRET), [], 'SITEFILE_HOME (caches, image store)');
  // Without the secret the private repository refuses the request (401), and that error has no secret either.
  const bare = await makeSite({}, `FROM scratch\nIMPORT docs FROM git+${main.base}/private.git#main:docs SECRET k2 AS BEARER\nEXPORT /\n`);
  const r = await runCli(home, ['build', bare, ...NET, '--secret', 'id=k2,env=WRONG'], undefined, { WRONG: 'not-it' });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_FETCH_STATUS.*HTTP 401/);
});
