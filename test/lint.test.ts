// Lint rules (src/lint.ts): one test per rule, plus disabling.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { lint, LINT_RULES } from '../src/lint.js';

function check(src: string, disabled: string[] = [], args: Record<string, string> = {}): string[] {
  const program = parseProgram(src);
  return lint(program, plan(program, args), src, disabled);
}

/** `[rule] message` of each warning, without the `Sitefile:<line>: ` prefix but with the line. */
const rules = (ws: string[]): string[] => ws.map((w) => w.replace(/^Sitefile:(\d+): \[([a-z-]+)\].*$/, '$1 $2'));

test('rule ids are stable', () => {
  assert.deepEqual([...LINT_RULES], ['unused-name', 'unpinned-import', 'unpinned-image', 'render-escape', 'sitemap-order', 'shadowed-write', 'serve-ignored']);
});

test('unused-name: ARG, LET, IMPORT, PLUGIN and TOOL nobody refers to', () => {
  const ws = check(
    [
      'ARG A = 1',
      'ARG USED = x',
      'LET L = "${USED}"',
      'IMPORT t FROM ./t.json AS json',
      'IMPORT d FROM ./d',
      'IMPORT posts FROM ./posts AS markdown',
      'PLUGIN p FROM ./p/p.mjs',
      'TOOL cp',
      'COPY d: /d/',
      'RENDER <<H TO /i.txt',
      '${L} ${#each posts as x}${x.title}${/each}',
      'H',
      'EXPORT /',
    ].join('\n'),
  );
  assert.deepEqual(ws, [
    'Sitefile:1: [unused-name] ARG A is never used',
    'Sitefile:4: [unused-name] IMPORT t is never used',
    'Sitefile:7: [unused-name] PLUGIN p is never used',
    'Sitefile:8: [unused-name] TOOL cp is never used',
  ]);
  // A template file may read any ARG, LET or value import, so only the others are reported.
  const opaque = check('ARG A\nIMPORT t FROM ./t.json AS json\nIMPORT d FROM ./d\nRENDER ./page.html TO /p.txt\nEXPORT /', [], { A: '1' });
  assert.deepEqual(rules(opaque), ['3 unused-name']);
  // Uses: ${} in any operand (EXPORT BASE too), FOR … IN, FEED, WITH, name: sources.
  assert.deepEqual(
    check(
      [
        'ARG B = https://e.x/',
        'IMPORT posts FROM ./posts AS markdown',
        'IMPORT img FROM ./img',
        'PLUGIN og FROM ./og/og.mjs',
        'TRANSFORM img:/a.png WITH og TO /a.png',
        'RENDER --escape=html <<H TO /p/${p.slug}.html FOR p IN posts',
        'x',
        'H',
        'FEED posts BASE https://e.x LINK /p/${item.slug}.html',
        'EXPORT / AS wbn BASE ${B}',
      ].join('\n'),
    ),
    [],
  );
});

test('unpinned-import: a remote IMPORT without INTEGRITY', () => {
  const pin = 'sha256:' + 'a'.repeat(64);
  const ws = check(`IMPORT a FROM https://e.x/a.json AS json\nIMPORT b FROM https://e.x/b.json AS json INTEGRITY ${pin}\nIMPORT c FROM ./c.json AS json\nRENDER <<H TO /x.txt\n\${a} \${b} \${c}\nH\nEXPORT /`);
  assert.deepEqual(ws, ['Sitefile:1: [unpinned-import] IMPORT a has no INTEGRITY, so https://e.x/a.json is trusted on first use (pin it with INTEGRITY sha256:…)']);
});

test('unpinned-image: FROM image and an oci BASE without a digest', () => {
  const pin = '@sha256:' + 'b'.repeat(64);
  const ws = check(`FROM image ghcr.io/a/site:1 AS a\nFROM image ghcr.io/a/site${pin} AS b\nFROM image local-site:1 AS c\nFROM scratch\nCOPY --from=a / /\nCOPY --from=b / /b/\nCOPY --from=c / /c/\nEXPORT / AS oci BASE nginx:alpine`);
  assert.deepEqual(ws, [
    'Sitefile:1: [unpinned-image] FROM image ghcr.io/a/site:1 has no @sha256: digest, so the tag is trusted on first use (pin it as ghcr.io/a/site:1@sha256:…)',
    'Sitefile:8: [unpinned-image] EXPORT … AS oci BASE nginx:alpine has no @sha256: digest, so the tag is trusted on first use (pin it as nginx:alpine@sha256:…)',
  ]);
  assert.deepEqual(check(`EXPORT / AS oci BASE nginx:alpine${pin}`), []);
  assert.deepEqual(check('EXPORT / AS zip BASE https://e.x/'), []);
});

test('render-escape: an HTML page rendered without --escape=html', () => {
  const ws = check(
    ['RENDER <<H TO /a.html', 'x', 'H', 'RENDER <<H TO /b.HTM', 'x', 'H', 'RENDER --escape=none <<H TO /c.html', 'x', 'H', 'RENDER --escape=html <<H TO /d.html', 'x', 'H', 'RENDER <<H TO /e.txt', 'x', 'H', 'EXPORT /'].join('\n'),
  );
  assert.deepEqual(rules(ws), ['1 render-escape', '4 render-escape']);
  assert.match(ws[0]!, /RENDER writes \/a\.html without --escape=html, so substituted values are not HTML-escaped/);
  // FOR destinations are checked as written.
  assert.deepEqual(rules(check('IMPORT posts FROM ./p AS markdown\nRENDER <<H TO /p/${post.slug}.html FOR post IN posts\nx\nH\nEXPORT /')), ['2 render-escape']);
});

test('sitemap-order: a page written after the SITEMAP of its stage', () => {
  const ws = check('SITEMAP BASE https://e.x\nRENDER --escape=html <<H TO /a.html\nx\nH\nCOPY <<H /b.html\ny\nH\nEXPORT /');
  assert.deepEqual(ws, ['Sitefile:1: [sitemap-order] SITEMAP lists the .html files written before it, but line 2 writes /a.html after it (move SITEMAP after the pages)']);
  assert.deepEqual(check('RENDER --escape=html <<H TO /a.html\nx\nH\nSITEMAP BASE https://e.x\nCOPY <<H /robots.txt\ny\nH\nEXPORT /'), []);
  // Another stage is another tree.
  assert.deepEqual(check('STAGE a\nSITEMAP BASE https://e.x\nFROM scratch\nRENDER --escape=html <<H TO /a.html\nx\nH\nCOPY --from=a / /\nEXPORT /'), []);
});

test('shadowed-write: a file replaced by a later --overwrite', () => {
  const ws = check('RENDER --escape=html <<H TO /a.html\nx\nH\nCOPY --overwrite <<H /a.html\ny\nH\nEXPORT /');
  assert.deepEqual(ws, ['Sitefile:1: [shadowed-write] /a.html written here is replaced by line 4 (--overwrite), so this write has no effect']);
  assert.deepEqual(check('COPY <<H /a.txt\nx\nH\nCOPY <<H /b.txt\ny\nH\nEXPORT /'), []);
});

test('serve-ignored: REDIRECT and HEADER lines sitefile serve would ignore', () => {
  const ws = check('REDIRECT /a/*/b /c\nREDIRECT /blog/:year/:slug /posts/:slug\nHEADER /x Content-Length: 5\nHEADER /* X-Ok: 1\nARG T = "/t"\nREDIRECT ${T}/*/x /y\nEXPORT /');
  assert.deepEqual(ws, [
    "Sitefile:1: [serve-ignored] sitefile serve ignores the line REDIRECT /a/*/b /c writes: '*' is only supported as the last segment of the source",
    'Sitefile:3: [serve-ignored] sitefile serve ignores the line HEADER /x Content-Length writes: the server sets this header itself',
  ]);
});

test('disabling: comments, next-line comments, the caller, all; bad comments warn', () => {
  const src = (head: string): string => `${head}ARG A = 1\nARG B = 1\nRENDER <<H TO /a.html\nx\nH\nEXPORT /`;
  assert.deepEqual(rules(check(src(''))), ['1 unused-name', '2 unused-name', '3 render-escape']);
  assert.deepEqual(rules(check(src('# sitefile-lint: disable=unused-name\n'))), ['4 render-escape']);
  assert.deepEqual(rules(check(src('# SITEFILE-LINT : disable = all\n'))), []);
  assert.deepEqual(rules(check(src('# sitefile-lint: disable-next-line=unused-name, render-escape\n'))), ['3 unused-name', '4 render-escape']);
  assert.deepEqual(rules(check(src(''), ['render-escape', 'unused-name'])), []);
  assert.deepEqual(rules(check(src(''), ['all'])), []);
  // A comment anywhere disables for the whole file; a next-line comment skips blank lines and other comments.
  assert.deepEqual(rules(check('ARG A = 1\n# sitefile-lint: disable-next-line=unused-name\n\n# note\nARG B = 1\nEXPORT /\n# sitefile-lint: disable=render-escape')), ['1 unused-name']);
  assert.deepEqual(check('# sitefile-lint: disable=nope\n# sitefile-lint: off\nEXPORT /'), [
    "Sitefile:1: [lint-comment] unknown lint rule 'nope' (rules: unused-name, unpinned-import, unpinned-image, render-escape, sitemap-order, shadowed-write, serve-ignored, or all)",
    'Sitefile:2: [lint-comment] unrecognized # sitefile-lint comment (expected disable=<rule>[,<rule>…] or disable-next-line=<rule>[,<rule>…])',
  ]);
  // Lint comments in a heredoc body are body text.
  assert.deepEqual(rules(check('ARG A = 1\nCOPY <<H /a.txt\n# sitefile-lint: disable=all\nH\nEXPORT /')), ['1 unused-name']);
});
