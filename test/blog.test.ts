import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { parseRenderTemplate, renderTemplate, type TemplateLookup, type TemplateValueLookup } from '../src/render.js';
import { parseProgram } from '../src/language/validate.js';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { sortDocuments } from '../src/resources/documents.js';

// ------------------------------------------------------------ template blocks

const data: Record<string, unknown> = {
  T: 'Blog',
  posts: [
    { title: 'A & B', tags: ['x', 'y'], html: '<p>a</p>' },
    { title: 'C', tags: [], draft: true },
  ],
  none: [],
};
const lookup: TemplateLookup = (name, p, span) => {
  let v: unknown = data[name];
  for (const s of p) v = (v as Record<string, unknown>)?.[s];
  if (v === undefined) throw Object.assign(new Error(`undefined ${name}`), { span });
  return v as string;
};
const values: TemplateValueLookup = (name, p, _span, loose) => {
  let v: unknown = data[name];
  for (const s of p) v = (v as Record<string, unknown> | undefined)?.[s];
  if (v === undefined && !loose) throw new Error(`undefined ${name}`);
  return v;
};
const render = (t: string, escape: 'none' | 'html' = 'html'): string =>
  renderTemplate(parseRenderTemplate(t, 't.html'), lookup, escape, values);

test('#each repeats a block, shadows names, and renders ${else} for empty lists', () => {
  assert.equal(render('${#each posts as p}[${p.title}]${/each}'), '[A &amp; B][C]');
  assert.equal(render('${#each posts as T}${T.title}${/each} ${T}'), 'A &amp; BC Blog');
  assert.equal(render('${#each none as p}x${else}empty${/each}'), 'empty');
  assert.equal(render('${#each posts as p}${#each p.tags as t}${t},${/each};${/each}'), 'x,y,;;');
  assert.equal(render('${#each posts as p}${#each p.missing as t}${t}${else}-${/each}${/each}'), '--');
  assert.throws(() => render('${#each nope as p}${/each}'), /undefined nope/);
});

test('#if / ${else}, negation, and loose missing keys', () => {
  assert.equal(render('${#each posts as p}${#if p.draft}D${else}P${/if}${/each}'), 'PD');
  assert.equal(render('${#each posts as p}${#if !p.draft}${p.title}${/if}${/each}', 'none'), 'A & B');
  assert.equal(render('${#if missing.key}yes${else}no${/if}'), 'no');
  assert.equal(render('${#each posts as p}${#if p.tags}T${else}-${/if}${/each}'), 'T-');
});

test('|raw skips escaping; other text is unchanged', () => {
  assert.equal(render('${posts.0.html|raw} ${posts.0.html}'), '<p>a</p> &lt;p&gt;a&lt;/p&gt;');
});

test('a block tag alone on its line removes the line', () => {
  const t = '<ul>\n  ${#each posts as p}\n  <li>${p.title}</li>\n  ${/each}\n</ul>\n';
  assert.equal(render(t, 'none'), '<ul>\n  <li>A & B</li>\n  <li>C</li>\n</ul>\n');
});

test('template block errors', () => {
  const at = (t: string, line: number, column: number): void => {
    assert.throws(
      () => render(t),
      (e: unknown) => isSitefileError(e, 'E_TEMPLATE') && e.span?.line === line && e.span.column === column,
      t,
    );
  };
  at('${#each posts}x${/each}', 1, 1);
  at('a\n${#if T}x', 2, 1);
  at('${/each}', 1, 1);
  at('${#if T}x${/each}', 1, 10);
  at('${#if T}a${else}b${else}c${/if}', 1, 18);
  at('${T|upper}', 1, 1);
  assert.throws(() => render('${#each T as x}${/each}'), (e: unknown) => isSitefileError(e, 'E_TYPE'));
});

// ------------------------------------------------------------ language

test('SORT BY and FOR parse and validate', () => {
  const prog = parseProgram(
    'FROM scratch\nIMPORT posts FROM ./p AS markdown SORT BY date DESC\nRENDER ./t TO /p/${post.slug}.html FOR post IN posts\nEXPORT /',
  );
  const imp = prog.instructions[1]!;
  assert.ok(imp.kind === 'IMPORT' && imp.as === 'markdown' && imp.sort?.key === 'date' && imp.sort.descending);
  const ren = prog.instructions[2]!;
  assert.ok(ren.kind === 'RENDER' && ren.each?.as === 'post' && ren.each.list === 'posts');

  const fails = (src: string, code: ErrorCode, line: number): void =>
    assert.throws(() => parseProgram(src), (e: unknown) => isSitefileError(e, code) && e.span?.line === line, src);
  fails('FROM scratch\nIMPORT d FROM ./d.txt AS text SORT BY x\nEXPORT /', 'E_VALIDATION', 2);
  fails('FROM scratch\nRENDER ./t TO /a FOR p IN nothing\nEXPORT /', 'E_UNDEFINED', 2);
  fails('FROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./t TO /a FOR posts IN posts\nEXPORT /', 'E_DUPLICATE', 3);
  fails('FROM scratch\nIMPORT posts FROM ./p AS markdown\nRENDER ./t TO /${post.slug} \nEXPORT /', 'E_SYNTAX', 3);
  fails('FROM scratch\nARG A = x\nCOPY ./a /${A.b}\nEXPORT /', 'E_SYNTAX', 3);
});

test('sortDocuments: numbers, strings, missing last, path tiebreak', () => {
  const docs = [
    { path: '/c', date: '2026-01-02' },
    { path: '/a' },
    { path: '/b', date: '2026-03-01' },
    { path: '/d', date: '2026-01-02' },
  ];
  assert.deepEqual(sortDocuments(docs, 'date', true).map((d) => d.path), ['/b', '/c', '/d', '/a']);
  assert.deepEqual(sortDocuments(docs, 'date', false).map((d) => d.path), ['/c', '/d', '/b', '/a']);
  assert.deepEqual(sortDocuments([{ path: '/x', n: 10 }, { path: '/y', n: 9 }], 'n', false).map((d) => d.path), ['/y', '/x']);
});

// ------------------------------------------------------------ builds

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-blog-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
async function run(files: Record<string, string>, extra: Partial<BuildOptions> = {}) {
  return build({ contextDir: await project(files), lockfile: false, ...extra });
}

const posts = {
  'posts/2026-01-05-hello.md': '---\ntitle: Hello\ntags: [a, b]\n---\n\nFirst **post**.\n',
  'posts/2026-02-01-second.md': '---\ntitle: Second & more\nslug: two\n---\nBody <b>two</b>.\n',
  'posts/notes.txt': 'not markdown',
};

test('IMPORT AS markdown: documents with slug, date, content, excerpt and frontmatter', async () => {
  const r = await run({
    ...posts,
    Sitefile: 'FROM scratch\nIMPORT posts FROM ./posts AS markdown SORT BY date DESC\nRENDER ./t TO /index.html\nEXPORT /',
    t: '${#each posts as p}${p.slug}|${p.date}|${p.title}|${p.excerpt}|${p.content|raw}\n${/each}',
  });
  assert.equal(
    text(r.output.get('/index.html')?.bytes),
    'two|2026-02-01|Second & more|Body <b>two</b>.|<p>Body &lt;b&gt;two&lt;/b&gt;.</p>\n\n' +
      'hello|2026-01-05|Hello|First post.|<p>First <strong>post</strong>.</p>\n\n',
  );
});

test('RENDER … FOR renders one page per document, with the dest interpolated', async () => {
  const r = await run({
    ...posts,
    Sitefile: [
      'FROM scratch',
      'ARG BASE = "/blog"',
      'IMPORT posts FROM ./posts AS markdown',
      'RENDER --escape=html ./post.html TO ${BASE}/${post.slug}/index.html FOR post IN posts',
      'EXPORT /',
    ].join('\n'),
    'post.html': '<h1>${post.title}</h1>${post.content|raw}${#each post.tags as t}#${t}${/each}',
  });
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/blog/hello/index.html', '/blog/two/index.html']);
  assert.equal(text(r.output.get('/blog/two/index.html')?.bytes), '<h1>Second &amp; more</h1><p>Body &lt;b&gt;two&lt;/b&gt;.</p>\n');
  assert.equal(text(r.output.get('/blog/hello/index.html')?.bytes), '<h1>Hello</h1><p>First <strong>post</strong>.</p>\n#a#b');
});

test('FOR also works over a JSON array', async () => {
  const r = await run({
    Sitefile: 'FROM scratch\nIMPORT people FROM ./p.json AS json\nRENDER <<T TO /${person.id}.txt FOR person IN people\n${person.name}\nT\nEXPORT /',
    'p.json': '[{"id":"ada","name":"Ada"},{"id":"alan","name":"Alan"}]',
  });
  assert.equal(text(r.output.get('/ada.txt')?.bytes), 'Ada\n');
  assert.equal(text(r.output.get('/alan.txt')?.bytes), 'Alan\n');
});

test('FOR and markdown errors', async () => {
  const rejects = async (files: Record<string, string>, code: ErrorCode, match?: RegExp): Promise<void> => {
    await assert.rejects(run(files), (e: unknown) => {
      assert.ok(isSitefileError(e), String(e));
      assert.equal(e.code, code, e.message);
      if (match) assert.match(e.message, match);
      return true;
    });
  };
  // duplicate slugs collide
  await rejects(
    { 'p/a.md': 'x', 'p/2026-01-01-a.md': 'y', Sitefile: 'FROM scratch\nIMPORT p FROM ./p AS markdown\nRENDER <<T TO /${x.slug}.html FOR x IN p\n${x.content|raw}\nT\nEXPORT /' },
    'E_CONFLICT',
  );
  // a slug can't climb out of the destination
  await rejects({ 'p/a.md': '---\nslug: ../evil\n---\n', Sitefile: 'FROM scratch\nIMPORT p FROM ./p AS markdown\nEXPORT /' }, 'E_VALIDATION', /slug/);
  // FOR over a single document
  await rejects(
    { 'a.md': 'x', Sitefile: 'FROM scratch\nIMPORT p FROM ./a.md AS markdown\nRENDER <<T TO /x FOR d IN p\nT\nEXPORT /' },
    'E_TYPE',
    /not a list/,
  );
  // bad frontmatter is reported with its file and line
  await assert.rejects(
    run({ 'p/a.md': '---\ntitle: ok\nalias: &anchor value\n---\n', Sitefile: 'FROM scratch\nIMPORT p FROM ./p AS markdown\nEXPORT /' }),
    (e: unknown) => isSitefileError(e, 'E_FRONTMATTER') && e.span?.file.endsWith('p/a.md') === true && (e.span.line ?? 0) >= 3,
  );
  // markdown values can be read, not copied
  await rejects({ 'a.md': 'x', Sitefile: 'FROM scratch\nIMPORT p FROM ./a.md AS markdown\nCOPY p: /a\nEXPORT /' }, 'E_TYPE');
});

test('a single markdown file import is one document', async () => {
  const r = await run({
    'about.md': '---\ntitle: About\n---\nHi.\n',
    Sitefile: 'FROM scratch\nIMPORT about FROM ./about.md AS markdown\nRENDER <<T TO /about.html\n<h1>${about.title}</h1>${about.content|raw}\nT\nEXPORT /',
  });
  assert.equal(text(r.output.get('/about.html')?.bytes), '<h1>About</h1><p>Hi.</p>\n\n');
});
