// SITEMAP, FEED, REDIRECT and HEADER (docs/LANGUAGE.md, "Site files").
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { checkBaseUrl, headersFile, pageUrl, parseFeedDate, redirectsFile, xmlEscape } from '../src/site.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(files: Record<string, string>) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-site-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return build({ contextDir: dir, lockfile: false });
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

function fails(fn: () => unknown, code: ErrorCode, line: number, column: number, contains: string): void {
  assert.throws(fn, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.deepEqual([e.span?.line, e.span?.column], [line, column], e.message);
    assert.ok(e.detail.includes(contains), `${e.message} lacks ${contains}`);
    return true;
  });
}

async function rejects(p: Promise<unknown>, code: ErrorCode, contains: string): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.ok(e.detail.includes(contains), `${e.message} lacks ${contains}`);
    return true;
  });
}

const planOf = (src: string) => plan(parseProgram(src));

test('parse: SITEMAP, FEED, REDIRECT and HEADER forms', () => {
  const [s] = parseSitefile('SITEMAP --overwrite BASE https://x.example TO /s.xml');
  assert.equal(s?.kind, 'SITEMAP');
  assert.ok(s?.kind === 'SITEMAP' && s.overwrite && s.dest);
  const [f] = parseSitefile('feed posts base https://x link /p/${item.slug}.html to /atom.xml title "My blog"');
  assert.ok(f?.kind === 'FEED' && f.list === 'posts' && f.dest && f.title);
  const [r] = parseSitefile('REDIRECT /old /new STATUS 308');
  assert.ok(r?.kind === 'REDIRECT' && r.status === 308);
  const [r2] = parseSitefile('REDIRECT /old https://x.example/new');
  assert.ok(r2?.kind === 'REDIRECT' && r2.status === 301);
  const [h] = parseSitefile('HEADER /assets/* Cache-Control: public, max-age=60');
  assert.ok(h?.kind === 'HEADER' && h.value.length === 2);
  fails(() => parseSitefile('SITEMAP TO /a.xml'), 'E_SYNTAX', 1, 9, 'expected BASE');
  fails(() => parseSitefile('SITEMAP BASE'), 'E_SYNTAX', 1, 13, 'expected base URL');
  fails(() => parseSitefile('SITEMAP BASE https://x extra'), 'E_SYNTAX', 1, 24, "unexpected 'extra'");
  fails(() => parseSitefile('SITEMAP --from=a BASE https://x'), 'E_SYNTAX', 1, 9, 'unknown flag --from');
  fails(() => parseSitefile('FEED posts BASE https://x'), 'E_SYNTAX', 1, 26, 'expected LINK');
  fails(() => parseSitefile('FEED posts BASE https://x LINK /a TITLE t TO /f.xml'), 'E_SYNTAX', 1, 43, "unexpected 'TO'");
  fails(() => parseSitefile('FEED 1x BASE https://x LINK /a'), 'E_SYNTAX', 1, 6, 'expected import name');
  fails(() => parseSitefile('REDIRECT /old /new STATUS 200'), 'E_SYNTAX', 1, 27, "unknown redirect status '200'");
  fails(() => parseSitefile('REDIRECT /old /new STATUS'), 'E_SYNTAX', 1, 26, 'expected a status after STATUS');
  fails(() => parseSitefile('REDIRECT /old'), 'E_SYNTAX', 1, 14, 'expected redirect target');
  fails(() => parseSitefile('HEADER /* X-Frame-Options DENY'), 'E_SYNTAX', 1, 11, "expected 'Name:'");
  fails(() => parseSitefile('HEADER /* : DENY'), 'E_SYNTAX', 1, 11, 'expected a header name before the colon');
  fails(() => parseSitefile('HEADER /* X-Frame-Options:'), 'E_SYNTAX', 1, 27, 'expected header value');
});

test('validate: FEED needs an earlier import; LINK may use ${item.key}, other operands may not', () => {
  fails(() => parseProgram('FEED posts BASE https://x LINK /a\nEXPORT /'), 'E_UNDEFINED', 1, 6, "'posts' is not an IMPORT");
  assert.ok(parseProgram('IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x LINK /p/${item.slug}.html\nEXPORT /'));
  fails(
    () => parseProgram('IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x/${item.a} LINK /a\nEXPORT /'),
    'E_SYNTAX',
    2,
    27,
    'dotted access',
  );
  fails(() => parseProgram('SITEMAP BASE ${URL}\nEXPORT /'), 'E_UNDEFINED', 1, 14, 'undefined variable ${URL}');
  // the instructions start the implicit stage like any other
  assert.deepEqual(
    parseProgram('REDIRECT /a /b\nEXPORT /').instructions.map((i) => i.kind),
    ['FROM', 'REDIRECT', 'EXPORT'],
  );
});

test('plan: BASE, destinations, redirect and header checks', () => {
  const p = planOf('ARG URL = "https://x.example/"\nSITEMAP BASE ${URL}\nREDIRECT /a /b\nHEADER /* X-A: 1\nEXPORT /');
  const op = p.stages[0]!.operations[0]!;
  assert.ok(op.kind === 'SITEMAP' && op.base === 'https://x.example' && op.dest === '/sitemap.xml');
  assert.deepEqual(p.redirects.map((r) => [r.from, r.to, r.status]), [['/a', '/b', 301]]);
  assert.deepEqual(p.headers.map((h) => [h.pattern, h.name, h.value]), [['/*', 'X-A', '1']]);
  fails(() => planOf('SITEMAP BASE ftp://x\nEXPORT /'), 'E_VALIDATION', 1, 14, 'must be an absolute http(s) URL');
  fails(() => planOf('SITEMAP BASE https://u@x\nEXPORT /'), 'E_VALIDATION', 1, 14, 'must be an absolute http(s) URL');
  fails(() => planOf('SITEMAP BASE https://x/?q\nEXPORT /'), 'E_VALIDATION', 1, 14, 'must be an absolute http(s) URL');
  fails(() => planOf('SITEMAP BASE https://\nEXPORT /'), 'E_VALIDATION', 1, 14, 'must be an absolute http(s) URL');
  fails(() => planOf('SITEMAP BASE https://x TO /maps/\nEXPORT /'), 'E_PATH', 1, 27, 'must be a file path');
  fails(() => planOf('SITEMAP BASE https://x TO maps.xml\nEXPORT /'), 'E_PATH', 1, 27, 'must be an absolute artifact path');
  fails(() => planOf('REDIRECT old /b\nEXPORT /'), 'E_VALIDATION', 1, 10, "REDIRECT source 'old' must be an absolute path");
  fails(() => planOf('REDIRECT "/a b" /b\nEXPORT /'), 'E_VALIDATION', 1, 10, 'must not contain spaces or control characters');
  fails(() => planOf('REDIRECT /a "/b\\nX-Evil: 1"\nEXPORT /'), 'E_VALIDATION', 1, 13, 'REDIRECT target must not contain');
  fails(() => planOf('REDIRECT /a b\nEXPORT /'), 'E_VALIDATION', 1, 13, 'must be an absolute path or an http(s) URL');
  fails(() => planOf('REDIRECT /a javascript:alert(1)\nEXPORT /'), 'E_VALIDATION', 1, 13, 'must be an absolute path or an http(s) URL');
  fails(() => planOf('HEADER /* "X A:" 1\nEXPORT /'), 'E_VALIDATION', 1, 11, 'HEADER name must not contain spaces');
  fails(() => planOf('HEADER /* X(A): 1\nEXPORT /'), 'E_VALIDATION', 1, 11, "HEADER name 'X(A)' is not a valid header name");
  fails(() => planOf('HEADER /* X-A: "1\\nSet-Cookie: a=b"\nEXPORT /'), 'E_VALIDATION', 1, 16, 'must not contain control characters');
  fails(() => planOf('HEADER /* X-A: " "\nEXPORT /'), 'E_VALIDATION', 1, 16, 'the value is empty');
  fails(() => planOf('HEADER assets/* X-A: 1\nEXPORT /'), 'E_VALIDATION', 1, 8, 'HEADER path pattern');
});

test('helpers: URLs, escaping, dates and the _redirects/_headers formats', () => {
  assert.equal(pageUrl('https://x', '/index.html'), 'https://x/');
  assert.equal(pageUrl('https://x', '/a/index.html'), 'https://x/a/');
  assert.equal(pageUrl('https://x/blog', '/a b/ü.html'), 'https://x/blog/a%20b/%C3%BC.html');
  assert.equal(xmlEscape(`<a href="x">&'\u0001`), '&lt;a href=&quot;x&quot;&gt;&amp;&apos;�');
  assert.equal(checkBaseUrl('HTTPS://x.example//', 'B', { file: 'S', line: 1, column: 1 }), 'HTTPS://x.example');
  assert.equal(parseFeedDate('2026-03-01')?.text, '2026-03-01T00:00:00Z');
  assert.equal(parseFeedDate('2026-03-01 10:20z')?.text, '2026-03-01T10:20:00Z');
  assert.equal(parseFeedDate('2026-03-01T10:20:30.5+02:00')?.text, '2026-03-01T10:20:30.5+02:00');
  assert.equal(parseFeedDate('2026-03-01T10:20:30+02:00')?.seconds, parseFeedDate('2026-03-01T08:20:30Z')?.seconds);
  for (const bad of ['2026-02-30', '2026-13-01', '2026-3-1', 'March 1', '2026-03-01T25:00Z', '2026-03-01T10:00']) {
    assert.equal(parseFeedDate(bad), undefined, bad);
  }
  assert.equal(parseFeedDate('2024-02-29')?.seconds, 1709164800);
  const span = { file: 'S', line: 1, column: 1 };
  assert.equal(
    redirectsFile([{ from: '/a', to: '/b', status: 301 }, { from: '/c/*', to: 'https://x/:splat', status: 302 }]),
    '/a /b 301\n/c/* https://x/:splat 302\n',
  );
  assert.equal(
    headersFile([{ pattern: '/*', name: 'A', value: '1' }, { pattern: '/x', name: 'B', value: '2' }, { pattern: '/*', name: 'C', value: '3' }]),
    '/*\n  A: 1\n  C: 3\n/x\n  B: 2\n',
  );
  void span;
});

const POSTS = {
  'posts/2026-01-05-first.md': '---\ntitle: First & <one>\nauthor: Ann\n---\nHello *there*.\n',
  'posts/2026-02-01-second.md': '---\ntitle: Second\ndate: 2026-02-01T09:30:00+01:00\n---\nTwo.\n',
};

test('build: SITEMAP lists the .html files written so far, sorted, without /404.html', async () => {
  const r = await run({
    Sitefile: [
      'COPY <<X /index.html\nX',
      'COPY <<X /404.html\nX',
      'COPY <<X /b/index.html\nX',
      'COPY <<X /a&b.html\nX',
      'COPY <<X /style.css\nX',
      'SITEMAP BASE https://x.example/root/',
      'COPY <<X /later.html\nX',
      'EXPORT /',
    ].join('\n'),
  });
  assert.equal(
    text(r.output.get('/sitemap.xml')?.bytes),
    '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n' +
      '  <url><loc>https://x.example/root/a%26b.html</loc></url>\n' +
      '  <url><loc>https://x.example/root/b/</loc></url>\n' +
      '  <url><loc>https://x.example/root/</loc></url>\n' +
      '</urlset>\n',
  );
  assert.equal(r.output.get('/sitemap.xml')?.origin.endsWith(':11 SITEMAP'), true);
});

test('build: FEED writes Atom with entry URLs, the newest date and escaped content', async () => {
  const r = await run({
    ...POSTS,
    Sitefile: [
      'ARG URL = "https://blog.example"',
      'IMPORT posts FROM ./posts AS markdown SORT BY date DESC',
      'FEED posts BASE ${URL} LINK /p/${item.slug}/index.html TO /atom.xml TITLE "Ann\'s & notes"',
      'EXPORT /',
    ].join('\n'),
  });
  const xml = text(r.output.get('/atom.xml')?.bytes);
  assert.equal(
    xml,
    [
      '<?xml version="1.0" encoding="utf-8"?>',
      '<feed xmlns="http://www.w3.org/2005/Atom">',
      '  <title>Ann&apos;s &amp; notes</title>',
      '  <link href="https://blog.example/"/>',
      '  <link rel="self" href="https://blog.example/atom.xml"/>',
      '  <id>https://blog.example/atom.xml</id>',
      '  <updated>2026-02-01T09:30:00+01:00</updated>',
      '  <entry>',
      '    <title>Second</title>',
      '    <link href="https://blog.example/p/second/"/>',
      '    <id>https://blog.example/p/second/</id>',
      '    <updated>2026-02-01T09:30:00+01:00</updated>',
      '    <summary>Two.</summary>',
      '    <content type="html">&lt;p&gt;Two.&lt;/p&gt;\n</content>',
      '  </entry>',
      '  <entry>',
      '    <title>First &amp; &lt;one&gt;</title>',
      '    <link href="https://blog.example/p/first/"/>',
      '    <id>https://blog.example/p/first/</id>',
      '    <updated>2026-01-05T00:00:00Z</updated>',
      '    <author><name>Ann</name></author>',
      '    <summary>Hello there.</summary>',
      '    <content type="html">&lt;p&gt;Hello &lt;em&gt;there&lt;/em&gt;.&lt;/p&gt;\n</content>',
      '  </entry>',
      '</feed>',
      '',
    ].join('\n'),
  );
});

test('build: FEED errors name the document and the missing key', async () => {
  await rejects(
    run({ 'posts/x.md': '---\ntitle: X\n---\nbody\n', Sitefile: 'IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /' }),
    'E_VALIDATION',
    "FEED posts: document '/x.md' has no 'date' (add it to its frontmatter)",
  );
  await rejects(
    run({ 'posts/2026-01-01-x.md': 'body\n', Sitefile: 'IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /' }),
    'E_VALIDATION',
    "has no 'title'",
  );
  await rejects(
    run({ 'posts/x.md': '---\ntitle: X\ndate: soon\n---\n', Sitefile: 'IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /' }),
    'E_VALIDATION',
    "has date 'soon'; expected YYYY-MM-DD or an RFC 3339 date-time",
  );
  await rejects(
    run({ 'one.md': '---\ntitle: X\n---\n', Sitefile: 'IMPORT posts FROM ./one.md AS markdown\nFEED posts BASE https://x LINK /x\nEXPORT /' }),
    'E_TYPE',
    "'posts' is not a list",
  );
  await rejects(
    run({ ...POSTS, Sitefile: 'IMPORT posts FROM ./posts AS markdown\nFEED posts BASE https://x LINK ${item.slug}\nEXPORT /' }),
    'E_PATH',
    'FEED LINK',
  );
});

test('build: REDIRECT and HEADER write _redirects/_headers at the exported root, in source order', async () => {
  const r = await run({
    Sitefile: [
      'FROM scratch AS a',
      'REDIRECT /old /new',
      'HEADER /* X-Frame-Options: DENY',
      'FROM scratch',
      'COPY <<X /site/index.html\nX',
      'REDIRECT /blog/* https://blog.example/:splat STATUS 302',
      'HEADER /assets/* Cache-Control: public, max-age=31536000, immutable',
      'HEADER /* Referrer-Policy: no-referrer',
      'EXPORT /site',
    ].join('\n'),
  });
  assert.equal(text(r.output.get('/_redirects')?.bytes), '/old /new 301\n/blog/* https://blog.example/:splat 302\n');
  assert.equal(
    text(r.output.get('/_headers')?.bytes),
    '/*\n  X-Frame-Options: DENY\n  Referrer-Policy: no-referrer\n/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n',
  );
  assert.equal(r.output.get('/_redirects')?.origin.endsWith(':2 REDIRECT'), true);
});

test('build: a copied _redirects conflicts unless identical or --overwrite', async () => {
  await rejects(
    run({ '_redirects': '/x /y 301\n', Sitefile: 'COPY ./_redirects /_redirects\nREDIRECT /a /b\nEXPORT /' }),
    'E_CONFLICT',
    "'/_redirects' was already written with different content by ",
  );
  const same = await run({ '_redirects': '/a /b 301\n', Sitefile: 'COPY ./_redirects /_redirects\nREDIRECT /a /b\nEXPORT /' });
  assert.equal(same.output.get('/_redirects')?.origin.endsWith(':1 COPY'), true);
  const over = await run({ '_headers': 'x', Sitefile: 'COPY ./_headers /_headers\nHEADER --overwrite /* A: 1\nEXPORT /' });
  assert.equal(text(over.output.get('/_headers')?.bytes), '/*\n  A: 1\n');
});
