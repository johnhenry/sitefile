// Template helpers that write HTML for files (issue #105): `${file "/img/a.jpg" | img alt="…"}`.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { cp, mkdir, mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from '../src/executor.js';
import { BuildMemo } from '../src/memo.js';
import { isSitefileError, SitefileError } from '../src/errors.js';
import { fileUrl, imageSize, integrityOf, mediaType } from '../src/media.js';
import { parseRenderTemplate } from '../src/render.js';
import { parseSitefile } from '../src/language/parser.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

const PHOTOS = path.join(root, 'examples/image/photos');

async function project(files: Record<string, string | Uint8Array>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-media-'));
  temps.push(dir);
  await cp(PHOTOS, path.join(dir, 'photos'), { recursive: true });
  for (const [name, content] of Object.entries(files)) {
    await mkdir(path.dirname(path.join(dir, name)), { recursive: true });
    await writeFile(path.join(dir, name), content);
  }
  return dir;
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

async function page(body: string, extra = '', flags = '--escape=html'): Promise<string> {
  const dir = await project({ Sitefile: `ARG ALT = "Our <team> & co"\nCOPY ./photos/ /img/\n${extra}RENDER ${flags} <<HTML TO /index.html\n${body}\nHTML\nEXPORT /` });
  const r = await build({ contextDir: dir, lockfile: false, label: 'Sitefile' });
  return text(r.output.get('/index.html')?.bytes);
}

async function failure(body: string, extra = '', flags = '--escape=html'): Promise<{ code: string; message: string }> {
  try {
    await page(body, extra, flags);
  } catch (e) {
    assert.ok(isSitefileError(e), String(e));
    return { code: e.code, message: e.message };
  }
  assert.fail('expected the build to fail');
}

test('image sizes come from the file header', async () => {
  const sky = new Uint8Array(await readFile(path.join(PHOTOS, 'sky.jpg')));
  assert.deepEqual(imageSize(sky, '/sky.jpg'), { width: 1600, height: 1000 });
  assert.deepEqual(imageSize(new Uint8Array(await readFile(path.join(PHOTOS, 'badge.png'))), '/b.png'), { width: 256, height: 256 });
  assert.equal(imageSize(new Uint8Array([1, 2]), '/x.svg'), undefined);
  assert.throws(() => imageSize(new TextEncoder().encode('nope'), '/x.png'), (e: unknown) => e instanceof SitefileError && e.code === 'E_DATA' && e.detail === "'/x.png' is not a PNG, JPEG, GIF, WebP or AVIF image");
  assert.throws(() => imageSize(sky.subarray(0, 100), '/s.jpg'), (e: unknown) => e instanceof SitefileError && e.detail === "'/s.jpg' is not a valid JPEG image: its header is truncated or malformed");
});

test('the image header fuzz fixture matches (rust/tests/fixtures/media/generate.mjs)', async () => {
  const cases = JSON.parse(await readFile(path.join(root, 'rust/tests/fixtures/media/fuzz.json'), 'utf8')) as Array<{ name: string; bytes: string; ok?: [number, number] | null; error?: [string, string] }>;
  assert.ok(cases.length >= 1000);
  for (const c of cases) {
    let got: unknown;
    try {
      const s = imageSize(new Uint8Array(Buffer.from(c.bytes, 'base64')), '/' + c.name);
      got = { ok: s ? [s.width, s.height] : null };
    } catch (e) {
      got = { error: [(e as SitefileError).code, (e as SitefileError).detail] };
    }
    assert.deepEqual(got, c.error ? { error: c.error } : { ok: c.ok }, c.name);
  }
});

test('media types, URLs and integrity', () => {
  assert.equal(mediaType('/a/B.WEBP'), 'image/webp');
  assert.equal(mediaType('/a/b'), 'application/octet-stream');
  assert.equal(fileUrl('/img/my photo é.png'), '/img/my%20photo%20%C3%A9.png');
  assert.equal(integrityOf(new TextEncoder().encode('h1 { color: teal }\n')), 'sha384-sqPOxExJseEdur+S1v+wqJueVTMC7/XLWGSg0TFlWxyyxuVLvELs2g757ehC4aUg');
});

test('img writes src, width and height; attributes are escaped and can replace the defaults', async () => {
  assert.equal(
    await page('${file "/img/sky.jpg" | img alt="${ALT}" class=hero loading=eager}'),
    '<img src="/img/sky.jpg" width="1600" height="1000" alt="Our &lt;team&gt; &amp; co" class="hero" loading="eager" decoding="async">\n',
  );
  // A bare attribute, a quoted value with `}` in it, and the same markup under --escape=none.
  assert.equal(await page("${file '/img/badge.png' | img alt=\"a}b\" hidden}", '', '--escape=none'), '<img src="/img/badge.png" width="256" height="256" alt="a}b" hidden loading="lazy" decoding="async">\n');
});

test('value helpers are escaped like any value; tag helpers are markup', async () => {
  const css = 'COPY <<CSS "/css/my site.css"\nh1 { color: teal }\nCSS\n';
  const out = await page(
    [
      '${file "/css/my site.css"}|${file "/css/my site.css" | type}|${file "/css/my site.css" | size}',
      '${file /img/badge.png | width}x${file /img/badge.png | height}',
      '${file "/css/my site.css" | stylesheet media=print}',
      '${file "/css/my site.css" | preload}',
      '${file "/css/my site.css" | pre class=code}',
      '${file "/img/sky.jpg" | source media="(min-width: 40em)"}',
      '<a href="${file "/css/my site.css" | datauri}">',
    ].join('\n'),
    css,
  );
  const sri = 'sha384-sqPOxExJseEdur+S1v+wqJueVTMC7/XLWGSg0TFlWxyyxuVLvELs2g757ehC4aUg';
  assert.equal(
    out,
    [
      '/css/my%20site.css|text/css|19',
      '256x256',
      `<link rel="stylesheet" href="/css/my%20site.css" integrity="${sri}" media="print">`,
      `<link rel="preload" href="/css/my%20site.css" as="style" integrity="${sri}">`,
      '<pre class="code">h1 { color: teal }\n</pre>',
      '<source srcset="/img/sky.jpg" type="image/jpeg" media="(min-width: 40em)">',
      '<a href="data:text/css;base64,aDEgeyBjb2xvcjogdGVhbCB9Cg==">',
      '',
    ].join('\n'),
  );
});

test('script, video, audio, modulepreload and font preloads', async () => {
  const extra = 'COPY <<JS /app.js\nconsole.log(1)\nJS\nCOPY <<F /f.woff2\nx\nF\nCOPY <<V /v.mp4\nv\nV\nCOPY <<A /a.mp3\na\nA\n';
  const out = await page('${file /app.js | script type=module}${file /app.js | modulepreload}${file /f.woff2 | preload}${file /v.mp4 | video controls poster="/img/sky.jpg"}${file /a.mp3 | audio controls}', extra);
  const sri = integrityOf(new TextEncoder().encode('console.log(1)\n'));
  assert.equal(
    out,
    `<script src="/app.js" integrity="${sri}" type="module"></script><link rel="modulepreload" href="/app.js" integrity="${sri}"><link rel="preload" href="/f.woff2" as="font" crossorigin><video src="/v.mp4" controls poster="/img/sky.jpg"></video><audio src="/a.mp3" controls></audio>\n`,
  );
});

test('svg pastes SVG markup only with RENDER --inline-svg', async () => {
  const svg = 'COPY <<SVG /logo.svg\n<?xml version="1.0"?>\n<!-- logo -->\n<svg viewBox="0 0 1 1"><rect/></svg>\nSVG\n';
  assert.equal(await page('${file /logo.svg | svg class=logo}', svg, '--escape=html --inline-svg'), '<svg class="logo" viewBox="0 0 1 1"><rect/></svg>\n');
  const e = await failure('${file /logo.svg | svg}', svg);
  assert.equal(e.code, 'E_UNSUPPORTED');
  assert.match(e.message, /^Sitefile:9:1: the svg helper pastes '\/logo.svg' into the page, and SVG markup can run scripts; allow it with RENDER --inline-svg/);
  // An SVG as an img has no header size.
  assert.equal(await page('${file /logo.svg | img}', svg), '<img src="/logo.svg" loading="lazy" decoding="async">\n');
});

test('errors: missing files, directories, relative paths, non-images, size caps, syntax', async () => {
  assert.deepEqual(await failure('x ${file "/img/nope.jpg" | img}'), { code: 'E_NOT_FOUND', message: "Sitefile:4:3: '/img/nope.jpg' is not in the tree at this RENDER (write it with COPY, RENDER or TRANSFORM on an earlier line)" });
  assert.equal((await failure('${file /img}')).code, 'E_TYPE');
  assert.equal((await failure('${file img/sky.jpg}')).code, 'E_PATH');
  assert.equal((await failure('${file /x.txt | img}', 'COPY <<T /x.txt\nx\nT\n')).message, "Sitefile:7:1: '/x.txt' is not an image (img reads .png, .jpg, .jpeg, .gif, .webp, .avif and .svg files)");
  assert.equal((await failure('${file /bad.png | img}', 'COPY <<T /bad.png\nx\nT\n')).code, 'E_DATA');
  assert.equal((await failure('${file /img/sky.jpg | datauri}')).code, 'E_TOO_LARGE');
  for (const [t, d] of [
    ['${file }', 'expected a path after ${file, as in ${file "/img/a.png"}'],
    ['${file "/a | img}', 'unterminated string in ${file …}'],
    ['${file /a | imgg}', "unknown file helper 'imgg' (expected url, width, height, type, size, integrity, datauri, img, video, audio, source, script, stylesheet, preload, modulepreload, pre, svg)"],
    ['${file /a | width x}', 'the width helper takes no attributes'],
    ['${file /a | img alt=1 ALT=2}', "attribute 'ALT' given twice"],
    ['${file /a x}', "expected | or } after the path in ${file …}, got 'x'"],
    ['${file /a | img "x"}', `expected an attribute (name or name="value") in \${file … | img}, got '"'`],
    ['${file "/${a b}"}', 'expected ${name} or ${name.key} inside a ${file …} string'],
  ]) {
    assert.throws(() => parseRenderTemplate(t!, 't'), (e: unknown) => e instanceof SitefileError && e.code === 'E_TEMPLATE' && e.detail === d, t);
  }
  // `${file}` alone is still a variable named file.
  assert.deepEqual(parseRenderTemplate('${file}', 't'), [{ kind: 'var', name: 'file', path: [], span: { file: 't', line: 1, column: 1 } }]);
});

test('helpers in fragments and FOR loops; references in helper strings', async () => {
  const dir = await project({
    Sitefile: [
      'IMPORT pics FROM ./pics.json AS json',
      'COPY ./photos/ /img/',
      'LET pic: html = <figure>${file "/img/${item.file}" | img alt="${item.alt}"}</figure>',
      'RENDER --escape=html <<HTML TO /${item.slug}.html FOR item IN pics',
      '${pic}',
      'HTML',
      'EXPORT /',
    ].join('\n'),
    'pics.json': JSON.stringify([{ slug: 'a', file: 'sky.jpg', alt: '<sky>' }, { slug: 'b', file: 'badge.png', alt: 'badge' }]),
  });
  const r = await build({ contextDir: dir, lockfile: false });
  assert.equal(text(r.output.get('/a.html')?.bytes), '<figure><img src="/img/sky.jpg" width="1600" height="1000" alt="&lt;sky&gt;" loading="lazy" decoding="async"></figure>\n');
  assert.equal(text(r.output.get('/b.html')?.bytes), '<figure><img src="/img/badge.png" width="256" height="256" alt="badge" loading="lazy" decoding="async"></figure>\n');
});

test('--inline-svg parses as a RENDER flag', () => {
  const [r] = parseSitefile('RENDER --inline-svg ./t TO /x\nEXPORT /');
  assert.ok(r?.kind === 'RENDER' && r.inlineSvg === true);
});

test('incremental rebuilds see a changed file that a helper reads', async () => {
  const dir = await project({ Sitefile: 'COPY ./a.css /a.css\nRENDER <<H TO /i.html\n${file /a.css | integrity}\nH\nEXPORT /', 'a.css': 'one' });
  const memo = new BuildMemo();
  const once = async (): Promise<string> => text((await build({ contextDir: dir, lockfile: false, memo })).output.get('/i.html')?.bytes);
  const first = await once();
  await writeFile(path.join(dir, 'a.css'), 'two');
  const second = await once();
  assert.notEqual(first, second);
  assert.equal(second, integrityOf(new TextEncoder().encode('two')) + '\n');
});
