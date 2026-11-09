import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { operationLabel } from '../src/explain.js';
import { BuildMemo } from '../src/memo.js';
import { groupBy, paginate } from '../src/pages.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-pages-'));
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
async function rejects(files: Record<string, string>, code: ErrorCode, message: string): Promise<void> {
  await assert.rejects(run(files), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.equal(e.detail, message);
    return true;
  });
}

const post = (title: string, tags: string, extra = ''): string => `---\ntitle: ${title}\ntags: [${tags}]\n${extra}---\nBody of ${title}.\n`;
const POSTS: Record<string, string> = {
  'posts/2026-01-01-a.md': post('A', 'web, rust'),
  'posts/2026-01-02-b.md': post('B', 'rust', 'draft: true\n'),
  'posts/2026-01-03-c.md': post('C', 'web, 10, 9'),
  'posts/2026-01-04-d.md': post('D', ''),
  'posts/2026-01-05-e.md': post('E', 'web'),
};
const files = (sitefile: string): Record<string, string> => ({ ...POSTS, Sitefile: sitefile });
const outputs = (r: Awaited<ReturnType<typeof run>>): Record<string, string> => Object.fromEntries(r.output.entries().map(([p, n]) => [p, text(n.bytes)]));

test('DRAFTS: false leaves out draft: true; true (or no clause) keeps everything', async () => {
  const sf = (d: string): string => `ARG DRAFTS: boolean = ${d}\nIMPORT posts FROM ./posts AS markdown DRAFTS \${DRAFTS}\nRENDER <<T TO /i.html\n\${#each posts as p}\${p.title}\${/each}\nT\nEXPORT /`;
  assert.equal(outputs(await run(files(sf('false'))))['/i.html'], 'ACDE\n');
  assert.equal(outputs(await run(files(sf('true'))))['/i.html'], 'ABCDE\n');
  assert.equal(outputs(await run(files(sf('false')), { buildArgs: { DRAFTS: 'true' } }))['/i.html'], 'ABCDE\n');
  // CSV cells: the text `true` is a draft.
  const csv = await run({ 'p.csv': 'name,draft\na,true\nb,false\nc,\n', Sitefile: 'IMPORT p FROM ./p.csv AS csv DRAFTS false\nRENDER <<T TO /i.html\n${#each p as x}${x.name}${/each}\nT\nEXPORT /' });
  assert.equal(outputs(csv)['/i.html'], 'bc\n');
  await rejects({ 'p.yaml': 'a: 1\n', Sitefile: 'IMPORT p FROM ./p.yaml AS yaml DRAFTS false\nEXPORT /' }, 'E_TYPE', 'IMPORT p: DRAFTS needs a list (a directory of documents, a CSV file or a JSON/YAML array), not a single value');
  assert.throws(() => plan(parseProgram('IMPORT p FROM ./p AS json DRAFTS yes\nEXPORT /')), (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.detail === "IMPORT p: DRAFTS must be true or false, got 'yes'");
  assert.throws(() => parseProgram('IMPORT p FROM ./p DRAFTS false\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.detail === 'IMPORT p: DRAFTS applies to lists: AS markdown, json, yaml or csv');
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json DRAFTS false DRAFTS true\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_SYNTAX') && e.detail === 'DRAFTS given twice');
});

test('BY: one page per tag, keys in SORT BY order, items in list order', async () => {
  const r = await run(
    files('IMPORT posts FROM ./posts AS markdown SORT BY date DESC DRAFTS false\nRENDER <<T TO /tags/${tag.key}.html FOR tag IN posts BY tags\n${tag.key} ${tag.count}:${#each tag.items as p}${p.title}${/each}\nT\nEXPORT /'),
  );
  assert.deepEqual(outputs(r), { '/tags/10.html': '10 1:C\n', '/tags/9.html': '9 1:C\n', '/tags/rust.html': 'rust 1:A\n', '/tags/web.html': 'web 3:ECA\n' });
  await rejects(
    { 'p.yaml': '- tags: [{a: 1}]\n', Sitefile: 'IMPORT p FROM ./p.yaml AS yaml\nRENDER <<T TO /${t.key}.html FOR t IN p BY tags\nx\nT\nEXPORT /' },
    'E_TYPE',
    "FOR t IN p BY tags: 'tags' of document '#1' is not text or a list of text",
  );
});

test('PAGE: pages with number, total, items and prev/next URLs; FIRST names page 1', async () => {
  const r = await run(
    files(
      'IMPORT posts FROM ./posts AS markdown SORT BY date DESC\nLET N: number = 2\nRENDER <<T TO /page/${page.number}/index.html FOR page IN posts PAGE ${N} FIRST /index.html\n${page.number}/${page.total} of ${page.count} [${page.url}] <${page.prev}> <${page.next}>:${#each page.items as p}${p.title}${/each}\nT\nEXPORT /',
    ),
  );
  assert.deepEqual(outputs(r), {
    '/index.html': '1/3 of 5 [/] <> </page/2/>:ED\n',
    '/page/2/index.html': '2/3 of 5 [/page/2/] </> </page/3/>:CB\n',
    '/page/3/index.html': '3/3 of 5 [/page/3/] </page/2/> <>:A\n',
  });
  // An empty list is one page with no items.
  const empty = await run({ 'p.json': '[]', Sitefile: 'IMPORT p FROM ./p.json AS json\nRENDER <<T TO /p${page.number}.html FOR page IN p PAGE 3\n${page.total}${#each page.items as x}x${else}none${/each}\nT\nEXPORT /' });
  assert.deepEqual(outputs(empty), { '/p1.html': '1none\n' });
  assert.throws(() => plan(parseProgram('IMPORT p FROM ./p AS json\nRENDER ./t TO /${x.number}.html FOR x IN p PAGE 0\nEXPORT /')), (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.detail === "RENDER … PAGE: '0' is not a page size (a whole number from 1 to 999999999)");
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json\nRENDER ./t TO /a.html FOR x IN p PAGE 2 BY tags\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_SYNTAX'));
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json\nRENDER ./t TO /a.html FOR x IN p PAGE ${x.n}\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_SYNTAX'));
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json\nRENDER ./t TO /a.html FOR x IN p PAGE ${N}\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_UNDEFINED'));
  // The destination cannot use the URLs (they come from the destinations).
  await rejects({ 'p.json': '[1]', Sitefile: 'IMPORT p FROM ./p.json AS json\nRENDER <<T TO /${x.url}.html FOR x IN p PAGE 1\nx\nT\nEXPORT /' }, 'E_UNDEFINED', "'x.url' is undefined: 'x' has no key 'url'");
});

test('BY … PAGE: tag pages, paginated per tag', async () => {
  const r = await run(
    files(
      'IMPORT posts FROM ./posts AS markdown SORT BY date DESC\nRENDER <<T TO /tags/${tag.key}/${tag.number}.html FOR tag IN posts BY tags PAGE 2 FIRST /tags/${tag.key}/index.html\n${tag.key} ${tag.number}/${tag.total} ${tag.prev}|${tag.next}:${#each tag.items as p}${p.title}${/each}\nT\nEXPORT /',
    ),
  );
  const out = outputs(r);
  assert.equal(out['/tags/web/index.html'], 'web 1/2 |/tags/web/2.html:EC\n');
  assert.equal(out['/tags/web/2.html'], 'web 2/2 /tags/web/|:A\n');
  assert.equal(out['/tags/rust/index.html'], 'rust 1/1 |:BA\n');
  const p = plan(parseProgram('IMPORT posts FROM ./posts AS markdown\nRENDER ./t.html TO /t/${t.key}/${t.number}.html FOR t IN posts BY tags PAGE 10 FIRST /t/${t.key}/index.html\nEXPORT /'));
  assert.equal(operationLabel(p.stages[0]!.operations[0]!), 'RENDER ./t.html FOR t IN posts BY tags PAGE 10');
});

test('incremental: tag and page renders match a clean build after edits', async () => {
  const dir = await project(files('ARG D = "false"\nIMPORT posts FROM ./posts AS markdown SORT BY date DESC DRAFTS ${D}\nRENDER <<T TO /tags/${t.key}/${t.number}.html FOR t IN posts BY tags PAGE 1\n${t.key}:${#each t.items as p}${p.title}${/each} ${t.next}\nT\nRENDER <<T TO /p/${p.number}.html FOR p IN posts PAGE 2\n${#each p.items as x}${x.title}${/each}\nT\nEXPORT /'));
  const memo = new BuildMemo();
  const same = async (args: Record<string, string> = {}): Promise<void> => {
    const inc = await build({ contextDir: dir, lockfile: false, memo, buildArgs: args });
    const clean = await build({ contextDir: dir, lockfile: false, buildArgs: args });
    assert.deepEqual(outputs(inc), outputs(clean));
  };
  await same();
  await writeFile(path.join(dir, 'posts/2026-01-03-c.md'), post('C2', 'rust'));
  await same();
  await same({ D: 'true' });
  await writeFile(path.join(dir, 'posts/2026-01-06-f.md'), post('F', 'web, new'));
  await same({ D: 'true' });
  await same();
});

test('groupBy and paginate directly', () => {
  const span = { file: 'Sitefile', line: 1, column: 1 };
  const docs = [{ t: ['b', 10, 'a'] }, { t: 9 }, { t: ['a', 'a', null, ''] }, { x: 1 }];
  assert.deepEqual(groupBy(docs, 't', 'FOR', span).map((g) => [g['key'], g['count']]), [['9', 1], ['10', 1], ['a', 2], ['b', 1]]);
  assert.deepEqual(paginate([{ items: [1, 2, 3] }], 2), [[{ count: 3, items: [1, 2], number: 1, total: 2 }, { count: 3, items: [3], number: 2, total: 2 }]]);
});

test('DRAFTS and BY in an imported Sitefile (AS sitefile)', async () => {
  const r = await run({
    'blog/posts/a.md': post('A', 'x'),
    'blog/posts/b.md': post('B', 'x', 'draft: true\n'),
    'blog/Sitefile': 'ARG D = "false"\nSTAGE pages\nIMPORT posts FROM ./posts AS markdown DRAFTS ${D}\nRENDER <<T TO /t/${t.key}.html FOR t IN posts BY tags\n${#each t.items as p}${p.title}${/each}\nT\nEXPORT /',
    Sitefile: 'IMPORT blog FROM ./blog AS sitefile\nIMPORT all FROM ./blog AS sitefile ARG D=true\nFROM scratch\nCOPY --from=blog+pages / /\nCOPY --from=all+pages /t/ /all/\nEXPORT /',
  });
  assert.deepEqual(outputs(r), { '/all/x.html': 'AB\n', '/t/x.html': 'A\n' });
  assert.throws(() => parseProgram('IMPORT t FROM ./t AS sitefile DRAFTS false\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_VALIDATION'));
});

test('${#each list by key as g}: groups in templates, for a page listing every tag', async () => {
  const r = await run(
    files('IMPORT posts FROM ./posts AS markdown SORT BY date DESC DRAFTS false\nRENDER <<T TO /tags/index.html\n${#each posts by tags as tag}\n${tag.key} ${tag.count}:${#each tag.items as p}${p.title}${/each}\n${/each}\n${#each posts by nothing as g}${g.key}${else}none${/each}\nT\nEXPORT /'),
  );
  assert.deepEqual(outputs(r), { '/tags/index.html': '9 1:C\n10 1:C\nrust 1:A\nweb 3:ECA\nnone\n' });
  // A dotted list, `by` as a list name, and the error of a value that cannot be a key.
  const nested = await run({ 'p.json': '{"posts":[{"t":"x"},{"t":["x","y"]}],"by":[1]}', Sitefile: 'IMPORT d FROM ./p.json AS json\nRENDER <<T TO /i.html\n${#each d.posts by t as g}${g.key}=${g.count} ${/each}${#each d.by as x}${x}${/each}\nT\nEXPORT /' });
  assert.equal(outputs(nested)['/i.html'], 'x=2 y=1 1\n');
  await rejects(
    { 'p.yaml': '- tags: [{a: 1}]\n', Sitefile: 'IMPORT p FROM ./p.yaml AS yaml\nRENDER <<T TO /i.html\n${#each p by tags as t}${t.key}${/each}\nT\nEXPORT /' },
    'E_TYPE',
    "${#each p by tags}: 'tags' of document '#1' is not text or a list of text",
  );
  await rejects({ Sitefile: 'RENDER <<T TO /i.html\n${#each p by as t}${/each}\nT\nEXPORT /' }, 'E_TEMPLATE', "expected ${#each list as item} or ${#each list by key as group}, got '${#each p by as t}'");
});

test('FEED tag … FOR tag IN list BY key: a feed per group', async () => {
  const sf = 'ARG SITE = "Blog"\nIMPORT posts FROM ./posts AS markdown SORT BY date DESC DRAFTS false\nFEED tag BASE https://x.example LINK /p/${item.slug}/ TO /tags/${tag.key}/feed.xml TITLE "${SITE}: ${tag.key}" FOR tag IN posts BY tags\nEXPORT /';
  const r = await run(files(sf));
  const out = outputs(r);
  assert.deepEqual(Object.keys(out).sort(), ['/tags/10/feed.xml', '/tags/9/feed.xml', '/tags/rust/feed.xml', '/tags/web/feed.xml']);
  const web = out['/tags/web/feed.xml']!;
  assert.match(web, /<title>Blog: web<\/title>/);
  assert.match(web, /<id>https:\/\/x\.example\/tags\/web\/feed\.xml<\/id>/);
  assert.deepEqual([...web.matchAll(/<title>([A-Z])<\/title>/g)].map((m) => m[1]), ['E', 'C', 'A']);
  assert.equal(operationLabel(plan(parseProgram(sf)).stages[0]!.operations[0]!), 'FEED FOR tag IN posts BY tags');
  const err = (src: string, code: ErrorCode, detail: string): void =>
    assert.throws(() => parseProgram(src), (e: unknown) => isSitefileError(e, code) && e.detail === detail);
  err('IMPORT posts FROM ./p AS markdown\nFEED posts BASE https://x LINK /a TO /t/${t.key}.xml FOR t IN posts BY tags\nEXPORT /', 'E_SYNTAX', 'FEED posts … FOR t IN posts BY tags writes a feed per group; its subject must be the loop variable: FEED t …');
  err('IMPORT posts FROM ./p AS markdown\nFEED t BASE https://x LINK /a FOR t IN posts BY tags\nEXPORT /', 'E_SYNTAX', 'FEED t … FOR t IN posts writes a feed per group, so it needs TO <dest> (such as /tags/${t.key}/feed.xml)');
  err('IMPORT posts FROM ./p AS markdown\nFEED t BASE https://x LINK /a TO /t.xml FOR t IN posts\nEXPORT /', 'E_SYNTAX', "expected BY");
  err('IMPORT posts FROM ./p AS markdown\nFEED item BASE https://x LINK /a TO /${item.key}.xml FOR item IN posts BY tags\nEXPORT /', 'E_DUPLICATE', "FOR item: 'item' is the element in FEED's LINK; pick another loop variable name");
  err('IMPORT posts FROM ./p AS markdown\nFEED t BASE https://x/${t.key} LINK /a TO /${t.key}.xml FOR t IN posts BY tags\nEXPORT /', 'E_SYNTAX', 'dotted access ${t.key} is only allowed in RENDER templates and for a RENDER … FOR loop variable');
  err('IMPORT posts FROM ./p AS markdown\nFEED t BASE https://x LINK /a TO /${t.key}.xml FOR t IN nope BY tags\nEXPORT /', 'E_UNDEFINED', "FEED nope: 'nope' is not an IMPORT declared on an earlier line");
  // Incremental rebuilds match a clean build.
  const dir = await project(files(sf));
  const memo = new BuildMemo();
  const same = async (): Promise<void> => {
    assert.deepEqual(outputs(await build({ contextDir: dir, lockfile: false, memo })), outputs(await build({ contextDir: dir, lockfile: false })));
  };
  await same();
  await writeFile(path.join(dir, 'posts/2026-01-03-c.md'), post('C2', 'rust'));
  await same();
});
