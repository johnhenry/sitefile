// EXPORT … FOR <host> (issue #205).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { tmpDir } from './helpers/cli.js';

async function site(sitefile: string, extra: Record<string, string> = {}): Promise<string> {
  const dir = await tmpDir('sitefile-for-');
  const files: Record<string, string> = { 'index.html': 'home', '404.html': 'nf', 'blog/a.html': 'post', 'assets/a.css': 'css', ...extra };
  for (const [p, body] of Object.entries(files)) {
    await fs.mkdir(path.dirname(path.join(dir, 'site', p)), { recursive: true });
    await fs.writeFile(path.join(dir, 'site', p), body);
  }
  await fs.writeFile(path.join(dir, 'Sitefile'), `FROM ./site\n${sitefile}`);
  return dir;
}

async function out(sitefile: string, lossy = false, extra: Record<string, string> = {}): Promise<{ files: Record<string, string>; warnings: string[] }> {
  const warnings: string[] = [];
  const r = await build({ contextDir: await site(sitefile, extra), lossy, onWarning: (m) => warnings.push(m) });
  return { files: Object.fromEntries(r.output.entries().map(([p, n]) => [p, Buffer.from(n.bytes).toString()])), warnings };
}

async function fails(sitefile: string, code: string, detail: RegExp): Promise<void> {
  await assert.rejects(build({ contextDir: await site(sitefile) }), (e: unknown) => isSitefileError(e) && e.code === code && detail.test(e.detail));
}

test('parser: FOR <host> after the other clauses, case-insensitive; unknown hosts and misplaced clauses', () => {
  const ex = (src: string) => parseSitefile(`FROM scratch\n${src}\n`, 'S').at(-1);
  assert.deepEqual((ex('EXPORT / FOR Vercel') as { forHost?: { value: string } }).forHost?.value, 'vercel');
  assert.deepEqual((ex('EXPORT / AS zip BASE https://x.example/ FOR github-pages') as { forHost?: { value: string } }).forHost?.value, 'github-pages');
  assert.throws(() => ex('EXPORT / FOR'), (e: unknown) => isSitefileError(e, 'E_SYNTAX') && /expected a host after FOR \(netlify, cloudflare, vercel, firebase, azure, github-pages, s3\)/.test(e.detail));
  assert.throws(() => ex('EXPORT / FOR heroku'), (e: unknown) => isSitefileError(e, 'E_SYNTAX') && /unknown host 'heroku'/.test(e.detail));
  assert.throws(() => ex('EXPORT / FOR vercel AS zip'), (e: unknown) => isSitefileError(e, 'E_SYNTAX'));
});

test('netlify is _redirects and _headers as without FOR', async () => {
  const rules = 'REDIRECT /old /new\nHEADER /* X-A: 1\n';
  const a = await out(`${rules}EXPORT /\n`);
  const b = await out(`${rules}EXPORT / FOR netlify\n`);
  assert.deepEqual(a.files, b.files);
  assert.equal(b.files['/_redirects'], '/old /new 301\n');
});

test('cloudflare: the same files without the rules a file would shadow; the error names the rule', async () => {
  await fails('REDIRECT /blog/* /posts/:splat\nEXPORT / FOR cloudflare\n', 'E_EXPORT', /^EXPORT … FOR cloudflare: REDIRECT \/blog\/\* \/posts\/:splat 301: \/blog\/a\.html is a file in the export/);
  const r = await out('REDIRECT /old /new\nREDIRECT /blog/* /posts/:splat\nEXPORT / FOR cloudflare\n', true);
  assert.equal(r.files['/_redirects'], '/old /new 301\n');
  assert.match(r.warnings.join('\n'), /Sitefile:3 REDIRECT \/blog\/\* \/posts\/:splat 301: .*; left out \(--lossy\)/);
  // /about answers with about.html (a clean URL), so it shadows too.
  await assert.rejects(
    build({ contextDir: await site('REDIRECT /about /a\nEXPORT / FOR cloudflare\n', { 'about.html': 'x' }) }),
    (e: unknown) => isSitefileError(e, 'E_EXPORT') && /\/about\.html is a file/.test(e.detail),
  );
});

test('vercel: Build Output API v3 with the files under .vercel/output/static', async () => {
  const r = await out('REDIRECT /old/ /new STATUS 307\nREDIRECT /u/:id/* /users/:id/:splat\nHEADER /assets/* Cache-Control: a\nHEADER /assets/* Cache-Control: b\nEXPORT / FOR vercel\n');
  assert.deepEqual(Object.keys(r.files), ['/.vercel/output/config.json', '/.vercel/output/static/404.html', '/.vercel/output/static/assets/a.css', '/.vercel/output/static/blog/a.html', '/.vercel/output/static/index.html']);
  assert.deepEqual(JSON.parse(r.files['/.vercel/output/config.json'] as string), {
    version: 3,
    routes: [
      { src: '^/assets/.*$', headers: { 'Cache-Control': 'a, b' }, continue: true },
      { handle: 'filesystem' },
      { src: '^/old/?$', headers: { Location: '/new' }, status: 307 },
      { src: '^/u/(?<id>[^/]+)/(?<splat>.*)$', headers: { Location: '/users/$id/$splat' }, status: 301 },
      { handle: 'error' },
      { status: 404, src: '^/.*$', dest: '/404.html' },
    ],
  });
  await fails('REDIRECT /a /b/:nope\nEXPORT / FOR vercel\n', 'E_EXPORT', /:nope in the target is not in the source/);
  await fails('REDIRECT /a/*/b /c\nEXPORT / FOR vercel\n', 'E_EXPORT', /\* is supported only as a trailing \/\*/);
});

test('firebase: firebase.json and public/; 301/302 only; header globs', async () => {
  const r = await out('REDIRECT /old /new STATUS 302\nREDIRECT /x/* /y/:splat\nHEADER /assets/* X: 1\nEXPORT / FOR firebase\n');
  assert.ok(r.files['/public/index.html'] !== undefined);
  assert.deepEqual(JSON.parse(r.files['/firebase.json'] as string), {
    hosting: {
      public: 'public',
      redirects: [
        { source: '/old', destination: '/new', type: 302 },
        { source: '/x/:splat*', destination: '/y/:splat', type: 301 },
      ],
      headers: [{ source: '/assets/**', headers: [{ key: 'X', value: '1' }] }],
    },
  });
  await fails('REDIRECT /old /new STATUS 308\nEXPORT / FOR firebase\n', 'E_EXPORT', /Firebase Hosting redirects are 301 or 302/);
  await fails('HEADER /u/:id X: 1\nEXPORT / FOR firebase\n', 'E_EXPORT', /without :placeholders/);
});

test('azure: routes, globalHeaders and the 404 page; overlapping routes are refused', async () => {
  const r = await out('REDIRECT /old /new\nHEADER /* X: 1\nHEADER /assets/* Y: 2\nEXPORT / FOR azure\n');
  assert.deepEqual(JSON.parse(r.files['/staticwebapp.config.json'] as string), {
    routes: [
      { route: '/old', redirect: '/new', statusCode: 301 },
      { route: '/assets/*', headers: { Y: '2' } },
    ],
    globalHeaders: { X: '1' },
    responseOverrides: { '404': { rewrite: '/404.html' } },
  });
  await fails('REDIRECT /assets/x /y\nHEADER /assets/* Y: 2\nEXPORT / FOR azure\n', 'E_EXPORT', /only the first matching route applies, and REDIRECT \/assets\/x matches the same paths/);
  await fails('HEADER /* X: 1\nHEADER /assets/* X: 2\nEXPORT / FOR azure\n', 'E_EXPORT', /a route's X replaces the one for \/\*/);
  await fails('REDIRECT /a/* /b/:splat\nEXPORT / FOR azure\n', 'E_EXPORT', /cannot use :splat/);
});

test('github-pages: .nojekyll and CNAME; no redirects or headers', async () => {
  const r = await out('EXPORT / BASE https://Docs.Example.com FOR github-pages\n');
  assert.equal(r.files['/.nojekyll'], '');
  assert.equal(r.files['/CNAME'], 'docs.example.com\n');
  const pages = await out('EXPORT / BASE https://me.github.io/repo/ FOR github-pages\n');
  assert.equal(pages.files['/CNAME'], undefined);
  await fails('REDIRECT /a /b\nEXPORT / FOR github-pages\n', 'E_EXPORT', /GitHub Pages has no redirects/);
  await fails('EXPORT / BASE ftp://x.example/ FOR github-pages\n', 'E_VALIDATION', /BASE must be an http\(s\) URL/);
  // An existing different CNAME is a conflict, never overwritten.
  await assert.rejects(build({ contextDir: await site('EXPORT / BASE https://a.example/ FOR github-pages\n', { CNAME: 'b.example\n' }) }), (e: unknown) => isSitefileError(e, 'E_CONFLICT'));
});

test('s3: website.json routing rules and objects.json metadata', async () => {
  const r = await out('REDIRECT /old /new\nREDIRECT /docs/* https://Docs.example/v2/:splat STATUS 302\nREDIRECT /all/* /one\nHEADER /assets/* Cache-Control: max-age=60\nHEADER / Content-Language: en\nEXPORT / FOR s3\n');
  assert.deepEqual(JSON.parse(r.files['/_s3/website.json'] as string), {
    IndexDocument: { Suffix: 'index.html' },
    ErrorDocument: { Key: '404.html' },
    RoutingRules: [
      { Condition: { KeyPrefixEquals: 'docs/' }, Redirect: { Protocol: 'https', HostName: 'Docs.example', ReplaceKeyPrefixWith: 'v2/', HttpRedirectCode: '302' } },
      { Condition: { KeyPrefixEquals: 'all/' }, Redirect: { ReplaceKeyWith: 'one', HttpRedirectCode: '301' } },
    ],
  });
  assert.deepEqual(JSON.parse(r.files['/_s3/objects.json'] as string), [
    { Key: 'assets/a.css', CacheControl: 'max-age=60' },
    { Key: 'index.html', ContentLanguage: 'en' },
    { Key: 'old', WebsiteRedirectLocation: '/new' },
  ]);
  await fails('REDIRECT /old /new STATUS 302\nEXPORT / FOR s3\n', 'E_EXPORT', /S3 object redirects are always 301/);
  await fails('HEADER /* X-Frame-Options: DENY\nEXPORT / FOR s3\n', 'E_EXPORT', /S3 serves only Cache-Control/);
  await fails('REDIRECT /index.html /x\nEXPORT / FOR s3\n', 'E_EXPORT', /an S3 object is either a file or a redirect/);
});

test('FOR applies to directory, zip, tar and tgz only; --target exports the raw tree', async () => {
  await fails('EXPORT / AS oci FOR vercel\n', 'E_VALIDATION', /applies only to AS directory, zip, tar and tgz, not oci/);
  const dir = await site('REDIRECT /old /new\nEXPORT / FOR vercel\n');
  const r = await build({ contextDir: dir, target: 'site' }).catch(() => undefined);
  assert.equal(r, undefined); // the stage needs a name for --target
  await fs.writeFile(path.join(dir, 'Sitefile'), 'FROM ./site AS site\nREDIRECT /old /new\nEXPORT / FOR vercel\n');
  const t = await build({ contextDir: dir, target: 'site' });
  assert.ok(t.output.isFile('/index.html'));
  assert.ok(!t.output.isFile('/.vercel/output/config.json'));
});
