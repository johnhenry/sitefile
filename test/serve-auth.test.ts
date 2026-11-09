// serve --auth / --auth-file (johnhenry/sitefile#177).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { pbkdf2Sync } from 'node:crypto';
import { closeServer, createSiteServer, listen } from '../src/server.js';
import { AUTH_ITERATIONS, BasicAuth, basicCredentials, makeVerifier, parseHtpasswd, verifyPassword } from '../src/serve-auth.js';
import { makeFile, Tree } from '../src/tree.js';
import http from 'node:http';

test('verifiers: pbkdf2-sha256 (RFC 7914 vector) and htpasswd {SHA}', () => {
  assert.equal(pbkdf2Sync('passwd', 'salt', 1, 32, 'sha256').toString('hex'), '55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc');
  const v = makeVerifier('s3cret', Buffer.from('0123456789abcdef'), 1000);
  assert.ok(v.startsWith('pbkdf2-sha256$1000$MDEyMzQ1Njc4OWFiY2RlZg==$'));
  assert.ok(verifyPassword(v, 's3cret'));
  assert.ok(!verifyPassword(v, 's3cret!'));
  assert.ok(verifyPassword(makeVerifier('x', undefined, AUTH_ITERATIONS), 'x'));
  assert.ok(!verifyPassword('garbage', 'x'));
  const e = parseHtpasswd('# c\nalice:{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=\n\n', 'f');
  assert.ok(verifyPassword(e[0]!.verifier, 'password'));
  for (const bad of ['bob:$2y$05$abc', 'bob:$apr1$x$y', 'bob:plain', 'nouser']) assert.throws(() => parseHtpasswd(bad, 'f'), /E_USAGE|f:1/, bad);
  assert.deepEqual(basicCredentials('Basic YWxpY2U6cGFzczp3b3Jk'), { user: 'alice', password: 'pass:word' });
  for (const bad of ['Bearer x', 'Basic', 'Basic YWxpY2U', 'Basic YWxpY2U6cGFzcw']) assert.equal(basicCredentials(bad), undefined, bad);
  const a = new BasicAuth(e);
  assert.ok(a.allows('Basic YWxpY2U6cGFzc3dvcmQ='));
  assert.ok(a.allows('Basic YWxpY2U6cGFzc3dvcmQ='), 'cached');
  assert.ok(!a.allows('Basic YWxpY2U6d3Jvbmc='));
  assert.ok(!a.allows(undefined));
});

test('a server with auth answers 401 with WWW-Authenticate until the credentials are right; the probe id header is still sent', async () => {
  const tree = Tree.fromFiles([['/a.txt', makeFile(new TextEncoder().encode('a'), 't')]]);
  const s = createSiteServer(tree, { serverId: 'abcdef012345', auth: new BasicAuth([{ user: 'u', verifier: makeVerifier('p', undefined, 1000) }]) });
  const { port } = await listen(s, '127.0.0.1', 0);
  const get = (headers: Record<string, string>) =>
    new Promise<http.IncomingMessage>((resolve, reject) => {
      const r = http.request({ host: '127.0.0.1', port, path: '/a.txt', headers, agent: false }, (res) => {
        res.resume();
        res.on('end', () => resolve(res));
      });
      r.on('error', reject);
      r.end();
    });
  try {
    const no = await get({});
    assert.equal(no.statusCode, 401);
    assert.equal(no.headers['www-authenticate'], 'Basic realm="sitefile", charset="UTF-8"');
    assert.equal(no.headers['x-sitefile-server'], 'abcdef012345');
    assert.equal((await get({ authorization: `Basic ${Buffer.from('u:x').toString('base64')}` })).statusCode, 401);
    assert.equal((await get({ authorization: `Basic ${Buffer.from('u:p').toString('base64')}` })).statusCode, 200);
  } finally {
    await closeServer(s);
  }
});
