// Network gating (issue #176): the `needs` of E_HOST_BLOCKED, strict network mode, the secret-host
// rule and `--json` error output.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createServer, type IncomingMessage, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { checkHostRules, checkSecretHosts, hostAllowed, mergeNetwork, networkFromEnv, networkProblem, type NetworkSettings } from '../src/resources/network.js';
import { networkOption } from '../src/cli/network.js';
import { secretHeaders } from '../src/secrets.js';
import { errorJson, isSitefileError, needsHost, needsSecretHost, SitefileError, type Needs } from '../src/errors.js';
import * as index from '../src/index.js';
import { runCli, tmpDir } from './helpers/cli.js';

function thrown(f: () => void): SitefileError {
  try {
    f();
  } catch (e) {
    assert.ok(isSitefileError(e), String(e));
    return e;
  }
  assert.fail('expected an error');
}

const url = (s: string) => new URL(s);

test('a host outside the allow list fails with needs { kind: host } and names the flag', () => {
  const e = thrown(() => checkHostRules(url('https://cdn.example.com/a.css'), { allowHosts: ['example.org'] }, 'https://cdn.example.com/a.css'));
  assert.equal(e.code, 'E_HOST_BLOCKED');
  assert.equal(e.detail, 'refusing to connect to cdn.example.com: it matches no allow-hosts rule (https://cdn.example.com/a.css); add --allow-hosts cdn.example.com');
  assert.deepEqual(e.needs, { kind: 'host', name: 'cdn.example.com', flag: '--allow-hosts' } satisfies Needs);
  // Hosts are normalized (case, trailing dot); an IPv6 literal is named without brackets.
  assert.equal(thrown(() => checkHostRules(url('https://CDN.Example.com./'), { allowHosts: ['x.test'] }, 'u')).needs?.name, 'cdn.example.com');
  assert.equal(thrown(() => checkHostRules(url('https://[::1]:8443/'), { allowHosts: ['x.test'] }, 'u')).needs?.name, '::1');
});

test('a deny-hosts match carries no needs: no flag overrides it', () => {
  const e = thrown(() => checkHostRules(url('https://cdn.example.com/'), { allowHosts: ['example.com'], denyHosts: ['cdn.example.com'] }, 'u'));
  assert.equal(e.code, 'E_HOST_BLOCKED');
  assert.equal(e.needs, undefined);
  // Also in strict mode, and also when the host is allowed too.
  assert.equal(thrown(() => checkHostRules(url('https://a.test/'), { strict: true, allowHosts: ['a.test'], denyHosts: ['a.test'] }, 'u')).needs, undefined);
});

test('strict mode: an empty allow list allows nothing; listed hosts pass; open mode is unchanged', () => {
  const e = thrown(() => checkHostRules(url('https://cdn.example.com/x'), { strict: true }, 'https://cdn.example.com/x'));
  assert.equal(e.detail, 'refusing to connect to cdn.example.com: it matches no allow-hosts rule (strict network mode) (https://cdn.example.com/x); add --allow-hosts cdn.example.com');
  assert.deepEqual(e.needs, needsHost('cdn.example.com'));
  checkHostRules(url('https://cdn.example.com/x'), { strict: true, allowHosts: ['example.com'] }, 'u');
  checkHostRules(url('https://cdn.example.com/x'), { strict: true, allowHosts: ['*'] }, 'u');
  checkHostRules(url('https://cdn.example.com/x'), {}, 'u');
  checkHostRules(url('https://cdn.example.com/x'), { strict: false }, 'u');
  checkHostRules(url('https://cdn.example.com/x'), undefined, 'u');
  // A port-specific rule does not allow another port.
  assert.equal(thrown(() => checkHostRules(url('https://a.test:8443/'), { strict: true, allowHosts: ['a.test:443'] }, 'u')).code, 'E_HOST_BLOCKED');
});

test('hostAllowed mirrors checkHostRules', () => {
  const cases: Array<[NetworkSettings | undefined, string, number, boolean]> = [
    [undefined, 'a.test', 443, true],
    [{}, 'a.test', 443, true],
    [{ strict: true }, 'a.test', 443, false],
    [{ strict: true, allowHosts: ['a.test'] }, 'sub.a.test', 443, true],
    [{ allowHosts: ['b.test'] }, 'a.test', 443, false],
    [{ denyHosts: ['a.test'] }, 'a.test', 443, false],
    [{ allowHosts: ['a.test'], denyHosts: ['a.test'] }, 'a.test', 443, false],
  ];
  for (const [n, h, p, want] of cases) {
    assert.equal(hostAllowed(h, p, n), want, JSON.stringify([n, h, p]));
    let threw = false;
    try {
      checkHostRules(new URL(`https://${h}:${p}/`), n, 'u');
    } catch {
      threw = true;
    }
    assert.equal(!threw, want, JSON.stringify([n, h, p]));
  }
});

test('SITEFILE_NETWORK and --network: open or strict; the flag replaces the variable', () => {
  assert.equal(networkFromEnv({ SITEFILE_NETWORK: 'strict' }).strict, true);
  assert.equal(networkFromEnv({ SITEFILE_NETWORK: ' open ' }).strict, undefined);
  assert.equal(networkFromEnv({ SITEFILE_NETWORK: '' }).strict, undefined);
  assert.throws(() => networkFromEnv({ SITEFILE_NETWORK: 'ask' }), (e: unknown) => isSitefileError(e, 'E_USAGE') && e.detail === "SITEFILE_NETWORK expects open or strict, got 'ask'");
  assert.equal(networkOption({}, { SITEFILE_NETWORK: 'strict' })?.strict, true);
  assert.equal(networkOption({ network: 'open' }, { SITEFILE_NETWORK: 'strict' }), undefined);
  assert.equal(networkOption({ network: 'strict' }, {})?.strict, true);
  assert.equal(networkOption({ network: 'open' }, {}), undefined);
  for (const bad of ['', 'Strict', 'closed']) {
    assert.throws(() => networkOption({ network: bad }, {}), (e: unknown) => isSitefileError(e, 'E_USAGE') && /^--network expects open or strict/.test(e.detail), bad);
  }
  // Strict mode keeps the host lists.
  assert.deepEqual(networkOption({ network: 'strict', 'allow-hosts': ['a.test,b.test'] }, { SITEFILE_ALLOW_HOSTS: 'c.test' }), { allowHosts: ['c.test', 'a.test', 'b.test'], strict: true });
});

test('mergeNetwork: strict when either side is', () => {
  assert.equal(mergeNetwork({ strict: true }, {}).strict, true);
  assert.equal(mergeNetwork({}, { strict: true }).strict, true);
  assert.equal(mergeNetwork({}, {}).strict, undefined);
  assert.deepEqual(mergeNetwork({ secretHosts: ['a=x.test'] }, { secretHosts: ['a=x.test', 'b=y.test'] }).secretHosts, ['a=x.test', 'b=y.test']);
});

test('--allow-secret-host is <id>=<pattern>', () => {
  assert.equal(networkProblem({ secretHosts: ['tok=api.example.com', '_a1=*.example.com:8443', 'k=10.0.0.0/8'] }), undefined);
  for (const bad of ['tok', '=api.example.com', 'tok=', '1tok=a.test', 'to-k=a.test', 'tok=a b', 'tok=a.test/x']) {
    assert.equal(networkProblem({ secretHosts: [bad] }), `invalid --allow-secret-host '${bad}': expected <secret id>=<host pattern> (a pattern as for --allow-hosts)`, bad);
  }
  assert.deepEqual(networkOption({ 'allow-secret-host': ['a=x.test,b=y.test'] }, {}), { secretHosts: ['a=x.test', 'b=y.test'] });
  assert.throws(() => networkOption({ 'allow-secret-host': ['oops'] }, {}), (e: unknown) => isSitefileError(e, 'E_USAGE'));
});

test('strict mode: a secret goes only to a host its --allow-secret-host rule matches', () => {
  const span = { file: 'Sitefile', line: 3, column: 1 };
  const strict: NetworkSettings = { strict: true, allowHosts: ['example.com'] };
  const e = thrown(() => checkSecretHosts('data', ['tok'], 'https://api.example.com/v1', strict, span));
  assert.equal(e.code, 'E_HOST_BLOCKED');
  assert.equal(e.message, "Sitefile:3:1: IMPORT data: refusing to send secret 'tok' to api.example.com (strict network mode); add --allow-secret-host tok=api.example.com");
  assert.deepEqual(e.needs, needsSecretHost('tok', 'api.example.com'));
  assert.deepEqual(e.needs, { kind: 'secret-host', name: 'tok=api.example.com', flag: '--allow-secret-host' });
  checkSecretHosts('data', ['tok'], 'https://api.example.com/v1', { ...strict, secretHosts: ['tok=api.example.com'] }, span);
  checkSecretHosts('data', ['tok'], 'https://api.example.com/v1', { ...strict, secretHosts: ['tok=example.com'] }, span);
  // A rule for another secret, or another host, does not count.
  assert.equal(thrown(() => checkSecretHosts('data', ['tok'], 'https://api.example.com/', { ...strict, secretHosts: ['other=api.example.com'] }, span)).needs?.name, 'tok=api.example.com');
  assert.equal(thrown(() => checkSecretHosts('data', ['tok'], 'https://api.example.com/', { ...strict, secretHosts: ['tok=evil.test'] }, span)).needs?.name, 'tok=api.example.com');
  // Every secret of the import needs its own rule; the first missing one is named.
  assert.equal(thrown(() => checkSecretHosts('data', ['a', 'b'], 'https://api.example.com/', { ...strict, secretHosts: ['a=api.example.com'] }, span)).needs?.name, 'b=api.example.com');
  // Open mode: no rule needed.
  checkSecretHosts('data', ['tok'], 'https://api.example.com/', { allowHosts: ['example.com'] }, span);
  checkSecretHosts('data', ['tok'], 'https://api.example.com/', undefined, span);
  // A host the host rules refuse is left to the request (whose error asks for --allow-hosts first).
  checkSecretHosts('data', ['tok'], 'https://api.example.org/', strict, span);
});

test('secretHeaders applies the secret-host rule only when given a target', () => {
  const spec = { name: 'data', secrets: [{ id: 'tok', header: 'Authorization', bearer: true }], span: { file: 'Sitefile', line: 2, column: 1 } };
  const secrets = new Map([['tok', 's3cret']]);
  assert.deepEqual(secretHeaders(spec, secrets), { authorization: 'Bearer s3cret' });
  const target = { url: 'https://api.example.com/', network: { strict: true, allowHosts: ['api.example.com'] } };
  const e = thrown(() => secretHeaders(spec, secrets, target));
  assert.equal(e.code, 'E_HOST_BLOCKED');
  assert.ok(!e.message.includes('s3cret'));
  assert.deepEqual(secretHeaders(spec, secrets, { ...target, network: { ...target.network, secretHosts: ['tok=api.example.com'] } }), { authorization: 'Bearer s3cret' });
  // A missing secret is still E_SECRET first.
  assert.equal(thrown(() => secretHeaders(spec, new Map(), target)).code, 'E_SECRET');
});

test('errorJson: code, message, span and needs (null when absent)', () => {
  const e = new SitefileError('E_HOST_BLOCKED', 'refusing', { file: 'Sitefile', line: 2, column: 1 }, { needs: needsHost('a.test') });
  assert.deepEqual(JSON.parse(errorJson(e)), {
    error: { code: 'E_HOST_BLOCKED', message: 'Sitefile:2:1: refusing', span: { file: 'Sitefile', line: 2, column: 1 }, needs: { kind: 'host', name: 'a.test', flag: '--allow-hosts' } },
  });
  assert.equal(errorJson(new SitefileError('E_FETCH', 'x')), '{\n  "error": {\n    "code": "E_FETCH",\n    "message": "x",\n    "span": null,\n    "needs": null\n  }\n}\n');
  assert.equal(typeof index.errorJson, 'function');
  assert.equal(typeof index.needsHost, 'function');
  assert.equal(typeof index.needsSecretHost, 'function');
});

// ------------------------------------------------------------------ CLI

const servers: Server[] = [];
after(() => {
  for (const s of servers) {
    s.closeAllConnections();
    s.close();
  }
});

/** A local http server that records every request's path and Authorization header. */
async function recorder(): Promise<{ base: string; seen: Array<{ path: string; auth: string | undefined }> }> {
  const seen: Array<{ path: string; auth: string | undefined }> = [];
  const server = createServer((req: IncomingMessage, res) => {
    seen.push({ path: req.url ?? '', auth: req.headers.authorization });
    res.end('body { color: red }\n');
  });
  servers.push(server);
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  return { base: `http://127.0.0.1:${(server.address() as AddressInfo).port}`, seen };
}

async function context(sitefile: string): Promise<{ dir: string; home: string }> {
  const dir = await tmpDir('sitefile-gate-');
  await fs.writeFile(path.join(dir, 'Sitefile'), sitefile);
  return { dir, home: path.join(dir, 'home') };
}

test('CLI: strict mode refuses every host before connecting, and --json prints the needs', async () => {
  const srv = await recorder();
  const { dir, home } = await context(`FROM scratch\nIMPORT css FROM ${srv.base}/a.css\nCOPY css: /a.css\nEXPORT /\n`);
  const flags = ['--allow-http', '--allow-private-network', '--no-lockfile'];
  for (const [args, env] of [
    [['--network', 'strict'], {}],
    [[], { SITEFILE_NETWORK: 'strict' }],
  ] as Array<[string[], Record<string, string>]>) {
    const r = await runCli(home, ['build', dir, '-o', path.join(dir, 'out'), ...flags, ...args, '--json'], undefined, env);
    assert.equal(r.code, 1, r.stderr);
    const detail = `refusing to connect to 127.0.0.1: it matches no allow-hosts rule (strict network mode) (${srv.base}/a.css); add --allow-hosts 127.0.0.1`;
    assert.ok(r.stderr.includes(`error[E_HOST_BLOCKED]: ${path.join(dir, 'Sitefile')}:2:1: ${detail}\n`) || r.stderr.includes(`: ${detail}\n`), r.stderr);
    const j = JSON.parse(r.stdout) as { error: { code: string; message: string; span: { line: number }; needs: Needs } };
    assert.equal(j.error.code, 'E_HOST_BLOCKED');
    assert.equal(j.error.span.line, 2);
    assert.ok(j.error.message.endsWith(detail));
    assert.deepEqual(j.error.needs, { kind: 'host', name: '127.0.0.1', flag: '--allow-hosts' });
  }
  assert.deepEqual(srv.seen, []);
  // Without --json, stdout stays empty.
  const plain = await runCli(home, ['build', dir, '-o', path.join(dir, 'out'), ...flags, '--network', 'strict']);
  assert.equal(plain.code, 1);
  assert.equal(plain.stdout, '');
  // Allowing the host (the flag from needs) builds; --network open undoes SITEFILE_NETWORK.
  const ok = await runCli(home, ['build', dir, '-o', path.join(dir, 'out'), ...flags, '--network', 'strict', '--allow-hosts', '127.0.0.1']);
  assert.equal(ok.code, 0, ok.stderr);
  const open = await runCli(home, ['build', dir, '-o', path.join(dir, 'out2'), ...flags, '--network', 'open'], undefined, { SITEFILE_NETWORK: 'strict' });
  assert.equal(open.code, 0, open.stderr);
  assert.equal(srv.seen.length, 2);
  // A bad mode is a usage error.
  const bad = await runCli(home, ['build', dir, '--network', 'ask']);
  assert.equal(bad.code, 2);
  assert.match(bad.stderr, /^error: --network expects open or strict, got 'ask'/);
  const badEnv = await runCli(home, ['validate', dir], undefined, { SITEFILE_NETWORK: 'closed' });
  assert.equal(badEnv.code, 2);
  assert.match(badEnv.stderr, /SITEFILE_NETWORK expects open or strict, got 'closed'/);
});

test('CLI: in strict mode a secret needs --allow-secret-host, and nothing is sent without it', async () => {
  const srv = await recorder();
  const { dir, home } = await context(`FROM scratch\nIMPORT css FROM ${srv.base}/a.css SECRET tok AS BEARER\nCOPY css: /a.css\nEXPORT /\n`);
  const base = ['build', dir, '-o', path.join(dir, 'out'), '--allow-http', '--allow-private-network', '--no-lockfile', '--secret', 'id=tok,env=TOK', '--network', 'strict', '--allow-hosts', '127.0.0.1', '--json'];
  const env = { TOK: 'gate-secret-91f' };
  const r = await runCli(home, base, undefined, env);
  assert.equal(r.code, 1, r.stderr);
  assert.match(r.stderr, /error\[E_HOST_BLOCKED\]: \S*Sitefile:2:1: IMPORT css: refusing to send secret 'tok' to 127\.0\.0\.1 \(strict network mode\); add --allow-secret-host tok=127\.0\.0\.1\n/);
  assert.deepEqual(JSON.parse(r.stdout).error.needs, { kind: 'secret-host', name: 'tok=127.0.0.1', flag: '--allow-secret-host' });
  assert.ok(!r.stdout.includes(env.TOK) && !r.stderr.includes(env.TOK));
  assert.deepEqual(srv.seen, []);
  // A rule for another host does not help.
  const other = await runCli(home, [...base, '--allow-secret-host', 'tok=example.com'], undefined, env);
  assert.equal(other.code, 1);
  assert.deepEqual(srv.seen, []);
  const ok = await runCli(home, [...base, '--allow-secret-host', 'tok=127.0.0.1'], undefined, env);
  assert.equal(ok.code, 0, ok.stderr);
  assert.deepEqual(srv.seen, [{ path: '/a.css', auth: `Bearer ${env.TOK}` }]);
  // Open mode needs no rule (as before).
  const open = await runCli(home, base.filter((a) => a !== 'strict' && a !== '--network'), undefined, env);
  assert.equal(open.code, 0, open.stderr);
});

test('CLI: --json errors on validate and outdated; outdated rows carry needs', async () => {
  const { dir, home } = await context('FROM scratch\nIMPORT css FROM https://cdn.example.com/a.css\nCOPY css: /a.css\nEXPORT /\n');
  const out = await runCli(home, ['outdated', dir, '--json', '--network', 'strict', '--no-lockfile']);
  assert.equal(out.code, 0, out.stderr);
  const row = (JSON.parse(out.stdout) as { inputs: Array<{ error: { code: string; needs?: Needs } }> }).inputs[0];
  assert.equal(row?.error.code, 'E_HOST_BLOCKED');
  assert.deepEqual(row?.error.needs, { kind: 'host', name: 'cdn.example.com', flag: '--allow-hosts' });
  // A deny-hosts row has no needs key at all.
  const denied = await runCli(home, ['outdated', dir, '--json', '--deny-hosts', 'example.com', '--no-lockfile']);
  assert.equal((JSON.parse(denied.stdout) as { inputs: Array<{ error: object }> }).inputs[0]?.error && 'needs' in (JSON.parse(denied.stdout).inputs[0].error as object), false);
  // A command-level failure with --json (a syntax error).
  await fs.writeFile(path.join(dir, 'Sitefile'), 'FROM scratch\nBOGUS\n');
  for (const cmd of [['validate', dir, '--json'], ['outdated', dir, '--json']]) {
    const r = await runCli(home, cmd);
    assert.equal(r.code, 1, cmd.join(' '));
    const j = JSON.parse(r.stdout) as { error: { code: string; span: unknown; needs: unknown } };
    assert.equal(j.error.code, 'E_UNKNOWN_INSTRUCTION');
    assert.equal(j.error.needs, null);
    assert.match(r.stderr, /^error\[E_UNKNOWN_INSTRUCTION\]/);
  }
  // Usage errors stay text only.
  const usage = await runCli(home, ['validate', dir, '--json', '--network', 'x']);
  assert.equal(usage.code, 2);
  assert.equal(usage.stdout, '');
});
