// FINGERPRINT (docs/LANGUAGE.md, "Fingerprinting") and the HTML/CSS reference scanner it uses (src/links.ts).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { why } from '../src/explain.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { globMatches, hashedName, manifestJson } from '../src/fingerprint.js';
import { integrityTags, renameReference, resolveReference, scanCss, scanHtml, decodeCssText } from '../src/links.js';
import { cacheHeaderRules } from '../src/site.js';
import { createHash } from 'node:crypto';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { BuildMemo } from '../src/memo.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function context(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-fp-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

async function run(files: Record<string, string>, extra: { memo?: BuildMemo; recordSteps?: boolean } = {}) {
  const dir = await context(files);
  const warnings: string[] = [];
  const r = await build({ contextDir: dir, lockfile: false, onWarning: (m) => warnings.push(m), ...extra });
  return { ...r, dir, printed: warnings };
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
/** Messages and origins name the Sitefile by its path relative to the working directory. */
const rel = (s: string): string => s.replace(/[^\s']*Sitefile:/g, 'Sitefile:');

async function rejects(p: Promise<unknown>, code: ErrorCode, contains: string): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.ok(rel(e.detail).includes(contains), `${e.message} lacks ${contains}`);
    return true;
  });
}

const urls = (refs: ReadonlyArray<{ line: number; url: string }>) => refs.map((r) => [r.line, r.url]);

test('scanner: HTML attributes, srcset, <style>, style=, comments and raw text', () => {
  const html =
    "<link href='a.css'>\n<img src=\" b.png \" srcset=\"c.png 1x, d%20e.png 2x\">\n<base href=/x/><object data=o.svg></object>\n<!-- <img src=no.png> --><script src=s.js>var x = '<img src=no.png>'</script>\n<style>@import 'i.css'; p{background:url(\"bg.png\")}</style><p style=\"background: url(p.png)\">\n<a href=\"x?a=1&amp;b=2\">";
  assert.deepEqual(urls(scanHtml(html)), [
    [1, 'a.css'],
    [2, 'b.png'],
    [2, 'c.png'],
    [2, 'd%20e.png'],
    [3, 'o.svg'],
    [4, 's.js'],
    [5, 'i.css'],
    [5, 'bg.png'],
    [5, 'p.png'],
    [6, 'x?a=1&b=2'],
  ]);
});

test('scanner: CSS comments, strings that are not URLs, and escapes', () => {
  const css = "/* url(no.png) */ a { content: \"url(no.png)\"; background: url( 'x\\2e png' ) }\n@import url(i.css);\n.myurl(no) { b: url(y\\).png) }";
  assert.deepEqual(urls(scanCss(css)), [
    [1, 'x.png'],
    [2, 'i.css'],
    [3, 'y).png'],
  ]);
  assert.equal(decodeCssText('\\61 b\\\nc\\0'), 'abc�');
});

test('scanner: resolution against the referring file', () => {
  assert.deepEqual(resolveReference('../img/a.png?x#y', '/css/site.css'), { kind: 'internal', path: '/img/a.png', dir: false });
  assert.deepEqual(resolveReference('/../../a/', '/x.html'), { kind: 'internal', path: '/a', dir: true });
  assert.deepEqual(resolveReference('caf%C3%A9.css', '/x.html'), { kind: 'internal', path: '/café.css', dir: false });
  assert.deepEqual(resolveReference('mailto:x@y', '/x.html'), { kind: 'none' });
  assert.deepEqual(resolveReference('#top', '/x.html'), { kind: 'none' });
  assert.deepEqual(resolveReference('data:image/png;base64,AA', '/x.html'), { kind: 'none' });
  assert.deepEqual(resolveReference('//cdn.example/a.js', '/x.html'), { kind: 'external', url: '//cdn.example/a.js' });
  assert.deepEqual(resolveReference('HTTPS://e.x/', '/x.html'), { kind: 'external', url: 'HTTPS://e.x/' });
  // Below a root (the EXPORT path), absolute paths start at the root and `..` stops there.
  assert.deepEqual(resolveReference('/img/a.png', '/www/x.html', '/www'), { kind: 'internal', path: '/www/img/a.png', dir: false });
  assert.deepEqual(resolveReference('../../img/a.png', '/www/x.html', '/www'), { kind: 'internal', path: '/www/img/a.png', dir: false });
  assert.deepEqual(resolveReference('../../img/a.png', '/src/x.html', '/www'), { kind: 'internal', path: '/img/a.png', dir: false });
});

test('names, globs, manifest and reference rewriting', () => {
  assert.equal(hashedName('/a/app.min.js', '0123456789'), '/a/app.min.0123456789.js');
  assert.equal(hashedName('/LICENSE', '0123456789'), '/LICENSE.0123456789');
  assert.equal(hashedName('/.env', '0123456789'), '/.env.0123456789');
  assert.ok(globMatches('/assets/**', '/assets/a/b.css'));
  assert.ok(globMatches('/**/*.css', '/a.css'));
  assert.ok(globMatches('/**/*.css', '/x/y/a.css'));
  assert.ok(!globMatches('/*.css', '/x/a.css'));
  assert.ok(globMatches('/a?.js', '/ab.js') && !globMatches('/a?.js', '/a.js'));
  assert.ok(globMatches('/é*', '/éa'));
  assert.equal(manifestJson([]), '{}\n');
  assert.equal(manifestJson([['/a.css', '/a.1.css'], ['/"q".js', '/"q".2.js']]), '{\n  "/a.css": "/a.1.css",\n  "/\\"q\\".js": "/\\"q\\".2.js"\n}\n');
  assert.equal(renameReference('css/a.css?v=1', false, '/x.html', '/css/a.css', '/css/a.0123456789.css'), 'css/a.0123456789.css?v=1');
  assert.equal(renameReference('LICENSE', false, '/x.html', '/LICENSE', '/LICENSE.0123456789'), 'LICENSE.0123456789');
  // Character references, escapes and percent-encoding are kept; the hash goes before the extension's dot.
  assert.equal(renameReference('caf&#xe9;&#46;png#x', false, '/x.html', '/café.png', '/café.0123456789.png'), 'caf&#xe9;.0123456789&#46;png#x');
  assert.equal(renameReference(' a\\2e png ', true, '/x.css', '/a.png', '/a.0123456789.png'), ' a.0123456789\\2e png ');
  assert.equal(renameReference('caf%C3%A9.png', false, '/x.html', '/café.png', '/café.0123456789.png'), 'caf%C3%A9.0123456789.png');
  // A percent-encoded dot can't take the hash in front of it: the path becomes a relative URL to the new name.
  assert.equal(renameReference('/a/b%2Ecss?v', false, '/a/x/y.html', '/a/b.css', '/a/b.0123456789.css'), '../b.0123456789.css?v');
});

test('parse and plan: FINGERPRINT forms and errors', () => {
  const [f] = parseSitefile('fingerprint --overwrite /assets/ "/**/*.css" manifest /m.json');
  assert.ok(f?.kind === 'FINGERPRINT' && f.overwrite && f.patterns.length === 2 && f.manifest);
  assert.throws(() => parseSitefile('FINGERPRINT'), /expected a path pattern/);
  assert.throws(() => parseSitefile('FINGERPRINT MANIFEST /m.json'), /expected a path pattern/);
  assert.throws(() => parseSitefile('FINGERPRINT /a MANIFEST'), /expected manifest destination/);
  assert.throws(() => parseSitefile('FINGERPRINT /a MANIFEST /m.json x'), /unexpected 'x'/);
  assert.throws(() => parseSitefile('FINGERPRINT --from=a /a'), /unknown flag --from/);
  const p = plan(parseProgram('ARG D = assets\nCOPY ./x /x\nFINGERPRINT /${D}/ /*.js MANIFEST /m.json\nEXPORT /'));
  assert.deepEqual(p.stages[0]!.operations[1], { kind: 'FINGERPRINT', patterns: ['/assets/**', '/*.js'], manifest: '/m.json', root: '/', overwrite: false, span: { file: 'Sitefile', line: 3, column: 1 } });
  const sub = plan(parseProgram('COPY ./x /www/x\nFINGERPRINT /www/a/\nEXPORT /www'));
  assert.equal(sub.stages[0]!.operations[1]!.kind === 'FINGERPRINT' && sub.stages[0]!.operations[1]!.root, '/www');
  assert.throws(() => plan(parseProgram('COPY ./x /x\nFINGERPRINT assets/\nEXPORT /')), /must be an absolute path starting with '\/'/);
  assert.throws(() => plan(parseProgram('COPY ./x /x\nFINGERPRINT /a MANIFEST /m/\nEXPORT /')), /must be a file path/);
  assert.throws(() => parseProgram('FINGERPRINT /${X}\nEXPORT /'), /undefined variable/);
});

const SITE = {
  'public/index.html': '<!doctype html>\n<link rel="stylesheet" href="css/site.css?v=1">\n<img src="/img/logo.png" srcset="img/logo.png 1x, img/logo%402x.png 2x">\n<a href="about.html">About</a>\n',
  'public/about.html': '<style>body{background:url(\'img/bg.png\')}</style>\n',
  'public/css/site.css': '@import "base.css";\nbody { background: url(../img/bg.png) }\n',
  'public/css/base.css': 'h1{color:red}\n',
  'public/img/logo.png': 'PNG',
  'public/img/logo@2x.png': 'PNG2',
  'public/img/bg.png': 'BG',
  'public/robots.txt': 'User-agent: *\n',
};

test('build: renames matching files, rewrites references, writes the manifest; HTML pages keep their names', async () => {
  const r = await run({
    ...SITE,
    Sitefile: 'COPY ./public/ /\nFINGERPRINT /css/ /img/ /*.html /*.txt MANIFEST /assets.json\nEXPORT /\n',
  });
  const paths = r.output.entries().map(([p]) => p);
  const base = paths.find((p) => /^\/css\/base\.[0-9a-f]{10}\.css$/.test(p))!;
  const site = paths.find((p) => /^\/css\/site\.[0-9a-f]{10}\.css$/.test(p))!;
  const bg = paths.find((p) => /^\/img\/bg\.[0-9a-f]{10}\.png$/.test(p))!;
  const logo2 = paths.find((p) => /^\/img\/logo@2x\.[0-9a-f]{10}\.png$/.test(p))!;
  assert.ok(base && site && bg && logo2, paths.join(' '));
  assert.ok(paths.includes('/index.html') && paths.includes('/about.html'), 'pages keep their names');
  assert.ok(paths.some((p) => /^\/robots\.[0-9a-f]{10}\.txt$/.test(p)), 'other matching files are renamed');
  assert.ok(!paths.includes('/css/site.css'));
  // The CSS file's hash covers its rewritten references.
  const siteCss = text(r.output.get(site)!.bytes);
  assert.equal(siteCss, `@import "${base.slice(5)}";\nbody { background: url(../img/${bg.slice(5)}) }\n`);
  assert.ok(site.includes(r.output.get(site)!.digest.slice(7, 17)));
  const index = text(r.output.get('/index.html')!.bytes);
  assert.ok(index.includes(`href="css/${site.slice(5)}?v=1"`), index);
  assert.ok(index.includes(`, img/${logo2.slice(5).replace('@', '%40')} 2x`), index);
  assert.ok(index.includes('href="about.html"'));
  assert.ok(text(r.output.get('/about.html')!.bytes).includes(`url('img/${bg.slice(5)}')`));
  const manifest = JSON.parse(text(r.output.get('/assets.json')!.bytes)) as Record<string, string>;
  assert.equal(manifest['/css/site.css'], site);
  assert.equal(Object.keys(manifest).length, 6);
  assert.equal(rel(r.output.get(site)!.origin), 'Sitefile:2 FINGERPRINT');
});

test('build: identical builds give identical names; a changed asset changes its name and its referrers', async () => {
  const sitefile = 'COPY ./public/ /\nFINGERPRINT /css/ /img/\nEXPORT /\n';
  const a = await run({ ...SITE, Sitefile: sitefile });
  const b = await run({ ...SITE, Sitefile: sitefile });
  assert.equal(a.output.digest(), b.output.digest());
  const c = await run({ ...SITE, 'public/img/bg.png': 'BG2', Sitefile: sitefile });
  const names = (t: typeof a.output) => t.entries().map(([p]) => p).filter((p) => p.startsWith('/css/site.') || p.startsWith('/img/bg.'));
  assert.notDeepEqual(names(a.output), names(c.output), 'bg.png and the site.css that references it change names');
});

test('build: HEADER patterns and REDIRECT targets naming a renamed file follow it', async () => {
  const r = await run({
    ...SITE,
    Sitefile: 'COPY ./public/ /www/\nFINGERPRINT /www/css/\nHEADER /css/site.css Cache-Control: immutable\nHEADER /css/* X-A: 1\nREDIRECT /old.css /css/site.css\nEXPORT /www\n',
  });
  const site = r.output.entries().map(([p]) => p).find((p) => p.startsWith('/css/site.'))!;
  assert.equal(text(r.output.get('/_headers')!.bytes), `${site}\n  Cache-Control: immutable\n/css/*\n  X-A: 1\n`);
  assert.equal(text(r.output.get('/_redirects')!.bytes), `/old.css ${site} 301\n`);
});

test('build: a cycle of CSS references is an error; nothing matching warns', async () => {
  await rejects(
    run({ 'a.css': '@import "b.css";', 'b.css': '@import url(a.css);', Sitefile: 'COPY ./a.css /\nCOPY ./b.css /\nFINGERPRINT /*.css\nEXPORT /\n' }),
    'E_VALIDATION',
    '/a.css -> /b.css -> /a.css is a cycle of references',
  );
  const r = await run({ 'a.html': 'x', Sitefile: 'COPY ./a.html /\nFINGERPRINT /*.html /x/\nEXPORT /\n' });
  assert.deepEqual(r.printed.map(rel), ['Sitefile:2 FINGERPRINT: no files match /*.html /x/**']);
});

test('build: a renamed file that would replace a different one is a conflict unless --overwrite', async () => {
  const files = { 'a.css': 'x', Sitefile: '' };
  const first = await run({ ...files, Sitefile: 'COPY ./a.css /\nFINGERPRINT /a.css\nEXPORT /\n' });
  const hashed = first.output.entries().map(([p]) => p).find((p) => p.startsWith('/a.'))!;
  await rejects(
    run({ ...files, 'other.css': 'y', Sitefile: `COPY ./a.css /\nCOPY ./other.css ${hashed}\nFINGERPRINT /a.css\nEXPORT /\n` }),
    'E_CONFLICT',
    `was already written with different content by Sitefile:2 COPY`,
  );
  const ok = await run({ ...files, 'other.css': 'y', Sitefile: `COPY ./a.css /\nCOPY ./other.css ${hashed}\nFINGERPRINT --overwrite /a.css\nEXPORT /\n` });
  assert.equal(text(ok.output.get(hashed)!.bytes), 'x');
});

test('why: a renamed file shows its history under the old name, then the rename', async () => {
  const r = await run({ ...SITE, Sitefile: 'COPY ./public/ /\nFINGERPRINT /css/ MANIFEST /m.json\nEXPORT /\n' }, { recordSteps: true });
  const site = r.output.entries().map(([p]) => p).find((p) => p.startsWith('/css/site.'))!;
  const w = why(r, site, r.dir);
  assert.deepEqual(
    w.history.map((e) => [rel(e.location), e.instruction, e.path, e.action, e.inputs]),
    [
      ['Sitefile:1', 'COPY ./public /', '/css/site.css', 'write', ['./public/css/site.css']],
      ['Sitefile:2', 'FINGERPRINT /css/**', site, 'write', ['/css/site.css (renamed)']],
    ],
  );
  const index = why(r, '/index.html', r.dir);
  assert.deepEqual(index.history[1]?.inputs, ['references to renamed files']);
  assert.deepEqual(why(r, '/m.json', r.dir).history[0]?.inputs, ['the files it renamed']);
});

test('incremental: an unchanged FINGERPRINT step is reused, and its warnings are printed again', async () => {
  const memo = new BuildMemo();
  const files = { 'a.html': 'x', Sitefile: 'COPY ./a.html /\nFINGERPRINT /none/\nEXPORT /\n' };
  const dir = await context(files);
  for (let i = 0; i < 2; i++) {
    const warnings: string[] = [];
    memo.begin();
    await build({ contextDir: dir, lockfile: false, memo, onWarning: (m) => warnings.push(m) });
    memo.finish(true);
    assert.deepEqual(warnings.map(rel), ['Sitefile:2 FINGERPRINT: no files match /none/**']);
    if (i === 1) assert.ok(memo.stats.hits >= 1, JSON.stringify(memo.stats));
  }
});

test('parse and plan: FINGERPRINT INTEGRITY and --no-cache-headers', () => {
  const [f] = parseSitefile('FINGERPRINT --no-cache-headers /a/ MANIFEST /m.json integrity');
  assert.ok(f?.kind === 'FINGERPRINT' && f.integrity === true && f.noCacheHeaders === true && f.manifest && f.patterns.length === 1);
  const [g] = parseSitefile('FINGERPRINT /a/ INTEGRITY');
  assert.ok(g?.kind === 'FINGERPRINT' && g.integrity && !g.manifest && g.noCacheHeaders === undefined);
  assert.throws(() => parseSitefile('FINGERPRINT INTEGRITY'), /expected a path pattern/);
  assert.throws(() => parseSitefile('FINGERPRINT /a/ INTEGRITY MANIFEST /m.json'), /unexpected 'MANIFEST'/);
  assert.throws(() => parseSitefile('FINGERPRINT --no-cache-headers=1 /a/'), /takes no value/);
  const on = plan(parseProgram('COPY ./x /x\nFINGERPRINT /a/ INTEGRITY\nFINGERPRINT /b/\nEXPORT /'));
  assert.deepEqual(on.cacheHeaders, { span: { file: 'Sitefile', line: 2, column: 1 } });
  assert.equal(on.stages[0]!.operations[1]!.kind === 'FINGERPRINT' && on.stages[0]!.operations[1]!.integrity, true);
  assert.equal(plan(parseProgram('COPY ./x /x\nFINGERPRINT /a/\nFINGERPRINT --no-cache-headers /b/\nEXPORT /')).cacheHeaders, undefined);
  assert.equal(plan(parseProgram('COPY ./x /x\nFINGERPRINT /a/\nHEADER /* cache-control: no-store\nEXPORT /')).cacheHeaders, undefined);
  assert.equal(plan(parseProgram('COPY ./x /x\nEXPORT /')).cacheHeaders, undefined);
});

test('integrityTags: scripts and stylesheet/modulepreload links without integrity', () => {
  const html = '<!-- <script src=a.js></script> --><script src=" a&amp;b.js "></script><LINK rel="Preload StyleSheet" HREF=c.css>' +
    '<link rel=modulepreload href=m.js><link rel=preload href=p.js><link rel=icon href=i.png><script src=x.js integrity=y></script>' +
    '<script src=first.js src=second.js></script><textarea><script src=t.js></script></textarea><script></script><img src=i.png>';
  const tags = integrityTags(html);
  assert.deepEqual(tags.map((t) => t.url), ['a&b.js', 'c.css', 'm.js', 'first.js']);
  assert.deepEqual(tags.map((t) => html.slice(0, t.at).split('<').pop()!.toLowerCase()), ['script', 'link', 'link', 'script']);
});

test('cacheHeaderRules: directories of renamed files, single files, pages', () => {
  const files = ['/a/x.1.css', '/a/s/y.2.js', '/b/keep.txt', '/b/z.3.js', '/blog/index.html', '/index.html', '/q r.4.js', '/top.5.svg'];
  const hashed = new Set(['/a/x.1.css', '/a/s/y.2.js', '/b/z.3.js', '/q r.4.js', '/top.5.svg']);
  assert.deepEqual(cacheHeaderRules(files, hashed).map((r) => `${r.pattern} ${r.value}`), [
    '/a/* public, max-age=31536000, immutable',
    '/b/z.3.js public, max-age=31536000, immutable',
    '/top.5.svg public, max-age=31536000, immutable',
    '/ no-cache',
    '/*/ no-cache',
    '/*.html no-cache',
  ]);
  assert.deepEqual(cacheHeaderRules(files, new Set()), []);
  assert.deepEqual(cacheHeaderRules(['/a.1.js', '/p.html'], new Set(['/a.1.js'])).map((r) => r.pattern), ['/a.1.js', '/*.html']);
});

test('build: INTEGRITY adds sha384 SRI; EXPORT writes Cache-Control lines to /_headers', async () => {
  const r = await run({
    'public/index.html': '<link rel="stylesheet" href="assets/a.css"><script type=module src="/assets/m.js"></script><img src="assets/i.png">\n',
    'public/assets/a.css': 'a{background:url(i.png)}',
    'public/assets/m.js': 'export {}',
    'public/assets/i.png': 'PNG',
    Sitefile: 'COPY ./public/ /\nFINGERPRINT /assets/ INTEGRITY\nEXPORT /\n',
  }, { recordSteps: true });
  const paths = r.output.entries().map(([p]) => p);
  const css = paths.find((p) => p.startsWith('/assets/a.'))!;
  const js = paths.find((p) => p.startsWith('/assets/m.'))!;
  const sri = (p: string) => 'sha384-' + createHash('sha384').update(r.output.get(p)!.bytes).digest('base64');
  assert.equal(
    text(r.output.get('/index.html')!.bytes),
    `<link integrity="${sri(css)}" rel="stylesheet" href="${css.slice(1)}"><script integrity="${sri(js)}" type=module src="${js}"></script><img src="${paths.find((p) => p.startsWith('/assets/i.'))!.slice(1)}">\n`,
  );
  assert.equal(text(r.output.get('/_headers')!.bytes), '/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n/\n  Cache-Control: no-cache\n/*.html\n  Cache-Control: no-cache\n');
  assert.equal(rel(r.output.get('/_headers')!.origin), 'Sitefile:2 FINGERPRINT');
  const w = why(r, '/_headers', r.dir);
  assert.deepEqual(w.history.map((e) => [e.instruction, e.inputs.map(rel)]), [['FINGERPRINT', ['Sitefile:2 FINGERPRINT']]]);
  // Without INTEGRITY the pages only get the new names; --no-cache-headers writes no /_headers.
  const plain = await run({ 'public/a.js': 'x', 'public/index.html': '<script src=a.js></script>', Sitefile: 'COPY ./public/ /\nFINGERPRINT --no-cache-headers /a.js\nEXPORT /\n' });
  assert.ok(!text(plain.output.get('/index.html')!.bytes).includes('integrity'));
  assert.equal(plain.output.get('/_headers'), undefined);
});

test('build: a COPY\'d /_headers is kept (with a warning); HEADER lines come first; --target writes none', async () => {
  const files = { 'public/a.js': 'x', 'public/_headers': '/*\n  X: 1\n', Sitefile: 'COPY ./public/ /\nFINGERPRINT /a.js\nEXPORT /\n' };
  const kept = await run(files);
  assert.equal(text(kept.output.get('/_headers')!.bytes), '/*\n  X: 1\n');
  assert.ok(kept.printed.some((m) => rel(m).includes('Sitefile:2 FINGERPRINT: /_headers already exists (not written by HEADER)')), kept.printed.join('\n'));
  const mixed = await run({ 'public/a.js': 'x', Sitefile: 'COPY ./public/ /\nFINGERPRINT /a.js\nHEADER /* X: 1\nEXPORT /\n' });
  const a = mixed.output.entries().map(([p]) => p).find((p) => p.startsWith('/a.'))!;
  assert.equal(text(mixed.output.get('/_headers')!.bytes), `/*\n  X: 1\n${a}\n  Cache-Control: public, max-age=31536000, immutable\n`);
  assert.equal(rel(mixed.output.get('/_headers')!.origin), 'Sitefile:3 HEADER');
});
