// Host config import (issue #204): the TOML subset and each format's mapping.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { isSitefileError } from '../src/errors.js';
import { formatOf, headersText, importHostConfig, redirectsText, type HostFormat } from '../src/host-config.js';
import { parseHeaderRules, parseRedirectRules } from '../src/serve-rules.js';
import { parseToml } from '../src/toml.js';
import { repoRoot, runCli, tmpDir } from './helpers/cli.js';

test('TOML subset: tables, arrays of tables, values (the same cases as rust/src/toml.rs)', () => {
  assert.equal(
    JSON.stringify(
      parseToml(
        'a = 1\nb = "x\\ty"\nc = \'lit\\n\'\nd = [1, 2_000,\n  3.5, true, # c\n]\ne = { f = "g", h.i = false }\n[t.u]\nk = -2\n[[r]]\nfrom = "/a"\n[r.values]\nX = "1"\n[[r]]\nfrom = "/b"\n',
        'f.toml',
      ),
    ),
    '{"a":1,"b":"x\\ty","c":"lit\\\\n","d":[1,2000,3.5,true],"e":{"f":"g","h":{"i":false}},"t":{"u":{"k":-2}},"r":[{"from":"/a","values":{"X":"1"}},{"from":"/b"}]}',
  );
  assert.equal(JSON.stringify(parseToml('"q k" = 1\n\'l\' = 2\nx.y = 3\nx.z = 4\n', 'f.toml')), '{"q k":1,"l":2,"x":{"y":3,"z":4}}');
  assert.equal(JSON.stringify(parseToml('e = 1e3\nu = "\\u00e9\\U0001F600"\n', 'f.toml')), '{"e":1000,"u":"é😀"}');
  for (const [t, msg] of [
    ['a = 1\na = 2\n', "f.toml:2: key 'a' is already defined"],
    ['[t]\n[t]\n', "f.toml:2: table 't' is defined twice"],
    ['a = "x\n', 'f.toml:1: unterminated string'],
    ['a = """x"""\n', 'f.toml:1: multi-line strings are not supported'],
    ['a = 1979-05-27\n', 'f.toml:1: dates and times are not supported'],
    ['a = 0x1F\n', "f.toml:1: unsupported value '0x1F'"],
    ['a = 01\n', "f.toml:1: unsupported value '01'"],
    ['a\n', "f.toml:1: expected '=' after a key"],
    ['a = 1 b\n', 'f.toml:1: expected the end of the line'],
    ['a = [1 2]\n', "f.toml:1: expected ',' or ']' in an array"],
    ['a = { b = 1 }\n[a]\n', "f.toml:2: key 'a' is already defined"],
    ['a = [1]\n[[a]]\n', "f.toml:2: key 'a' is already defined"],
    ['a = "\\x"\n', 'f.toml:1: invalid escape in a string'],
    ['a =\n', 'f.toml:1: expected a value'],
    ['[a\n', "f.toml:1: expected ']'"],
  ] as const) {
    assert.throws(() => parseToml(t, 'f.toml'), (e: unknown) => isSitefileError(e, 'E_DATA') && e.detail === msg, JSON.stringify(t));
  }
});

const run = (text: string, format: HostFormat, file = 'x') => importHostConfig(text, file, format);

test('every rule written is one serve applies as written', async () => {
  const dir = path.join(repoRoot, 'conformance', 'cases', 'import-config');
  for (const [name, format] of [
    ['netlify.toml', 'netlify'],
    ['vercel.json', 'vercel'],
    ['firebase.json', 'firebase'],
    ['staticwebapp.config.json', 'azure'],
    ['wrangler.toml', 'wrangler'],
    ['htaccess', 'htaccess'],
  ] as const) {
    const c = run(await fs.readFile(path.join(dir, name), 'utf8'), format, name);
    assert.equal(parseRedirectRules(redirectsText(c)).length, c.redirects.length, name);
    assert.deepEqual(
      parseHeaderRules(headersText(c)).map((b) => b.headers.length),
      c.headers.map((b) => b.headers.length),
      name,
    );
    assert.ok(c.unsupported.length > 0, name);
  }
});

test('netlify: status, force, splats, placeholders, query parameters, conditions, 410 and 451', () => {
  const c = run(
    '[[redirects]]\nfrom = "/a"\nto = "/b"\n[[redirects]]\nfrom = "/x/*"\nto = "/y/:splat"\nstatus = 200\nforce = true\n[[redirects]]\nfrom = "/p/:id"\nto = "/q/:id"\n[[redirects]]\nfrom = "/s"\nto = "/t/:id"\n[[redirects]]\nfrom = "/q"\nto = "/r"\nquery = { a = ":a" }\n[[redirects]]\nfrom = "/a/*/b"\nto = "/c"\n[[redirects]]\nfrom = "/o"\nto = "/n"\nstatus = 418\n',
    'netlify',
  );
  assert.deepEqual(c.redirects, ['/a /b 301', '/x/* /y/:splat 200!', '/p/:id /q/:id 301', '/q a=:a /r 301']);
  assert.deepEqual(c.unsupported, [
    { entry: 'redirects[3]', reason: ':id in the target is not in the source' },
    { entry: 'redirects[5]', reason: '* is supported only as a trailing /*' },
    { entry: 'redirects[6]', reason: 'status 418 is not supported' },
  ]);
  const q = run(
    '[[redirects]]\nfrom = "/s"\nto = "/s/:v"\nquery = { v = ":v", lang = "en" }\n[[redirects]]\nfrom = "/fr/*"\nto = "/fr/index.html"\nstatus = 200\nconditions = { Language = ["fr", "fr-CA"], Country = "FR" }\n[[redirects]]\nfrom = "/old"\nto = "/gone.html"\nstatus = 410\n[[redirects]]\nfrom = "/x"\nto = "/451.html"\nstatus = 451\n[[redirects]]\nfrom = "/r"\nto = "/s"\nconditions = { Role = "admin" }\n[[redirects]]\nfrom = "/h"\nto = "https://p.example/"\nstatus = 200\nheaders = { X = "1" }\n',
    'netlify',
  );
  assert.deepEqual(q.redirects, ['/s v=:v lang=en /s/:v 301', '/fr/* /fr/index.html 200 Language=fr,fr-CA Country=FR', '/old /gone.html 410', '/x /451.html 451']);
  assert.deepEqual(q.unsupported, [
    { entry: 'redirects[4]', reason: 'conditions on Role are not supported' },
    { entry: 'redirects[5]', reason: 'proxy headers and signing are not supported' },
  ]);
  const h = run('[[headers]]\nfor = "/*"\n[headers.values]\nETag = "x"\n[[headers]]\nfor = "/a"\n[headers.values]\nX-A = "1"\n[[headers]]\nfor = "/a"\n[headers.values]\nX-B = "2"\n', 'netlify');
  assert.deepEqual(h.unsupported, [{ entry: 'headers[0]', reason: 'ETag cannot be set by _headers' }]);
  // Consecutive blocks for one pattern are merged.
  assert.equal(headersText(h), '/a\n  X-A: 1\n  X-B: 2\n');
});

test('vercel: path-to-regexp sources, permanent, has/missing, proxies', () => {
  const c = run(
    JSON.stringify({
      redirects: [
        { source: '/a', destination: '/b', permanent: true },
        { source: '/d/:rest*', destination: '/e/:rest*' },
        { source: '/p/:id/edit', destination: '/q/:id' },
        { source: '/f/:rest*', destination: 'https://x.example/:rest' },
        { source: '/g', destination: '/h', statusCode: 302 },
        { source: '/i', destination: '/j', missing: [{ type: 'cookie', key: 'c' }] },
        { source: '/k(\\d+)', destination: '/l' },
      ],
      rewrites: [{ source: '/app/(.*)', destination: '/app/$1' }],
    }),
    'vercel',
  );
  assert.deepEqual(c.redirects, ['/a /b 308', '/d/* /e/:splat 308', '/p/:id/edit /q/:id 308', '/f/* https://x.example/:splat 308', '/g /h 302', '/app/* /app/:splat 200']);
  assert.deepEqual(c.unsupported.map((u) => [u.entry, u.reason]), [
    ['redirects[5]', 'conditions are not supported'],
    ['redirects[6]', 'regular expressions are not supported'],
  ]);
});

test('azure and wrangler: fallbacks come after every other rule', () => {
  const a = run(JSON.stringify({ navigationFallback: { rewrite: '/index.html' }, routes: [{ route: '/old', redirect: '/new' }] }), 'azure');
  assert.deepEqual(a.redirects, ['/old /new 302', '/* /index.html 200']);
  const w = run('[assets]\nnot_found_handling = "404-page"\n', 'wrangler');
  assert.deepEqual([w.redirects, w.unsupported], [[], []]);
  assert.deepEqual(run('[assets]\nnot_found_handling = "spa"\n', 'wrangler').unsupported, [{ entry: 'assets.not_found_handling', reason: "unknown value 'spa'" }]);
  assert.deepEqual(run('{"assets": {"not_found_handling": "single-page-application"}}', 'wrangler', 'wrangler.json').redirects, ['/* /index.html 200']);
});

test('.htaccess: Redirect matches the path and below; quotes; containers', () => {
  const c = run('Redirect permanent /docs/ /guide/\nRedirect seeother "/a b" /c\nRedirectMatch temp ^/x/(.*)$ /y/$1/$2\n<Directory /var>\nRedirect /in /out\n</Directory>\nRedirect /after /later\n', 'htaccess');
  assert.deepEqual(c.redirects, ['/docs/ /guide/ 301', '/docs/* /guide/:splat 301', '/after /later 302', '/after/* /later/:splat 302']);
  assert.deepEqual(c.unsupported.map((u) => u.entry), ['line 2', 'line 3', 'line 4']);
});

test('formatOf and file errors', async () => {
  assert.equal(formatOf('/a/b/netlify.toml'), 'netlify');
  assert.equal(formatOf('STATICWEBAPP.config.json'), 'azure');
  assert.equal(formatOf('site.toml'), undefined);
  assert.throws(() => run('[1]', 'vercel', 'vercel.json'), (e: unknown) => isSitefileError(e, 'E_DATA') && e.detail === 'vercel.json: expected an object at the top level');
  const dir = await tmpDir();
  const home = path.join(dir, 'home');
  let r = await runCli(home, ['import-config', path.join(dir, 'site.toml')]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /cannot tell the format of '.*site\.toml' from its name; pass --format \(netlify, vercel, firebase, azure, wrangler, htaccess\)/);
  r = await runCli(home, ['import-config', path.join(dir, 'netlify.toml')]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /^error\[E_NOT_FOUND\]: cannot read '.*netlify\.toml'\n$/);
  await fs.writeFile(path.join(dir, 'netlify.toml'), '[[redirects]]\nfrom = "/a"\nto = "/b"\n');
  r = await runCli(home, ['import-config', path.join(dir, 'netlify.toml'), '--redirects']);
  assert.deepEqual([r.code, r.stdout, r.stderr], [0, '/a /b 301\n', '']);
  r = await runCli(home, ['import-config', path.join(dir, 'netlify.toml'), '--headers']);
  assert.deepEqual([r.code, r.stdout], [0, '']);
  r = await runCli(home, ['import-config', path.join(dir, 'netlify.toml'), '--redirects', '--headers']);
  assert.equal(r.code, 2);
});
