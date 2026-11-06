// HTML fragments (issue #106): `LET name: html = …`, `LET name: html <<HTML`, `IMPORT … AS html`.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { lint } from '../src/lint.js';
import { build } from '../src/executor.js';
import { BuildMemo } from '../src/memo.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function fails(src: string, code: ErrorCode, line: number, column: number, detail?: string | RegExp): void {
  assert.throws(
    () => plan(parseProgram(src)),
    (e: unknown) => {
      assert.ok(isSitefileError(e), String(e));
      assert.equal(e.code, code, e.message);
      assert.deepEqual([e.span?.line, e.span?.column], [line, column], e.message);
      if (typeof detail === 'string') assert.equal(e.detail, detail);
      else if (detail) assert.match(e.detail, detail);
      return true;
    },
  );
}

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-fragments-'));
  temps.push(dir);
  for (const [name, content] of Object.entries(files)) {
    await mkdir(path.dirname(path.join(dir, name)), { recursive: true });
    await writeFile(path.join(dir, name), content);
  }
  return dir;
}

async function buildFiles(files: Record<string, string>, extra: { memo?: BuildMemo } = {}) {
  const dir = await project(files);
  return build({ contextDir: dir, lockfile: false, label: 'Sitefile', ...extra });
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

test('a one-line html LET and a heredoc html LET parse to a fragment', () => {
  const [one, two] = parseSitefile('LET a: html = <b>${T}</b>  \nLET b: HTML <<H\n<i>${a}</i>\nH\nEXPORT /');
  assert.deepEqual(one, { kind: 'LET', span: { file: 'Sitefile', line: 1, column: 1 }, name: 'a', type: 'html', body: { kind: 'line', text: '<b>${T}</b>', span: { file: 'Sitefile', line: 1, column: 15 } } });
  assert.equal(two?.kind, 'LET');
  assert.ok(two?.kind === 'LET' && two.type === 'html' && two.body.kind === 'inline');
  assert.equal(two.body.text, '<i>${a}</i>\n');
  // `= <<H` works too, and `<<-` strips tabs.
  const [three] = parseSitefile('LET c: html = <<-H\n\t<p>\n\tH\nEXPORT /');
  assert.ok(three?.kind === 'LET' && three.type === 'html' && three.body.kind === 'inline' && three.body.text === '<p>\n');
});

test('other LETs are not scanned for heredocs', () => {
  const [one] = parseSitefile('LET a = <<EOF\nEXPORT /');
  assert.ok(one?.kind === 'LET' && one.type === 'string');
});

test('syntax errors of html LETs', () => {
  fails('ARG a: html\nEXPORT /', 'E_SYNTAX', 1, 8, 'ARG a cannot be html: an html value is markup written in the Sitefile (LET a: html = …)');
  fails('LET a: html\nEXPORT /', 'E_SYNTAX', 1, 12, /^LET a: html requires a value/);
  fails('LET a: html <b>\nEXPORT /', 'E_SYNTAX', 1, 13, /^LET a: html requires a value/);
  fails('LET a: html = <i> \\\n <b>\nEXPORT /', 'E_SYNTAX', 1, 15, /cannot continue onto the next line/);
  fails('LET a: html = <b> <<X\nfoo\nX\nEXPORT /', 'E_SYNTAX', 1, 19, 'an inline <<X body can only be the value of LET a: html');
  fails('LET a: html <<X\nfoo\nEXPORT /', 'E_SYNTAX', 1, 13, "inline body is missing its closing 'X' line");
  fails('LET a: nope = 1\nEXPORT /', 'E_SYNTAX', 1, 8, "unknown type 'nope' (expected string, number or boolean, or html)");
});

test('template errors point into the LET: one-line columns and heredoc lines', () => {
  fails('LET a: html = <b>${#each}</b>\nEXPORT /', 'E_TEMPLATE', 1, 18);
  fails('ARG X\nLET a: html <<H\nok\n  ${x y}\nH\nEXPORT /', 'E_TEMPLATE', 4, 3);
});

test('names a fragment uses are checked where it is declared', () => {
  fails('LET a: html = <b>${a}</b>\nEXPORT /', 'E_TEMPLATE', 1, 18, "html fragment 'a' uses itself");
  fails('LET a: html = <b>${B}</b>\nLET B = 1\nEXPORT /', 'E_UNDEFINED', 1, 18, "'B' is declared on line 2, after LET a: html; a fragment can only use names declared before it");
  fails('ARG T\nLET a: html = ${T.x}\nEXPORT /', 'E_TYPE', 2, 15, "'T' is an ARG/LET value; dotted access only works on JSON imports");
  fails('LET a: html = x\nLET b: html = ${#each a as i}${/each}\nEXPORT /', 'E_TYPE', 2, 15, "'a' is an html fragment, not a value; insert it with ${a}");
  fails('LET a: html = x\nLET b: html = ${a.k}\nEXPORT /', 'E_TYPE', 2, 15);
  fails('STAGE s\nLET a: html = ${s}\nEXPORT /', 'E_UNDEFINED', 2, 15, "'s' is not an ARG, LET or IMPORT");
  // Undeclared names wait for the use site (a FOR loop variable), as do #each variables of the fragment itself.
  plan(parseProgram('LET a: html = ${item.title}${#each item.tags as t}${t}${/each}\nEXPORT /'));
});

test('html values are templates only: other operands, FOR and FEED are E_TYPE', () => {
  fails('LET a: html = x\nCOPY ./a /${a}\nEXPORT /', 'E_TYPE', 2, 11, "'a' is an html fragment; html values can only be used in RENDER templates");
  fails('IMPORT h FROM ./h.html AS html\nEXPORT /${h}', 'E_TYPE', 2, 9);
  fails('IMPORT h FROM ./h.html AS html\nRENDER ./t TO /x FOR i IN h\nEXPORT /', 'E_TYPE', 2, 18, "FOR i IN h: 'h' is an html fragment, not a list");
  fails('LET h: html = x\nFEED h BASE https://e.x LINK /a\nEXPORT /', 'E_TYPE', 2, 6, "FEED h: 'h' is an html fragment, not a list");
  fails('LET h: html = x\nLET h = 1\nEXPORT /', 'E_DUPLICATE', 2, 1);
  fails('IMPORT h FROM ./h.html AS html SORT BY x\nEXPORT /', 'E_VALIDATION', 1, 32);
});

test('fragments are not in the plan values; they are planned with their parsed template', () => {
  const p = plan(parseProgram('ARG T = x\nLET a: html = <b>${T}</b>\nEXPORT /'));
  assert.deepEqual([...p.vars.keys()], ['T']);
  assert.deepEqual([...p.fragments.keys()], ['a']);
  assert.equal(p.fragments.get('a')!.text, '<b>${T}</b>');
});

test('lint: a fragment nothing uses is unused-name; names inside fragments count as used', () => {
  const src = 'ARG T = x\nLET a: html = <b>${T}</b>\nLET b: html = ${a}\nRENDER --escape=html <<H TO /i.html\nhi\nH\nEXPORT /';
  const p = parseProgram(src);
  assert.deepEqual(lint(p, plan(p), src), ['Sitefile:3: [unused-name] LET b is never used']);
});

test('a fragment is inserted as markup; values inside it follow the RENDER escape mode', async () => {
  const r = await buildFiles({
    Sitefile: [
      'ARG SITE = "Tom & Jerry"',
      'LET h: html = <h1 title="${SITE}">${SITE}</h1>',
      'RENDER --escape=html <<HTML TO /a.html',
      '${h}|${SITE}',
      'HTML',
      'RENDER --escape=none <<HTML TO /b.html',
      '${h}|${h|raw}',
      'HTML',
      'EXPORT /',
    ].join('\n'),
  });
  assert.equal(text(r.output.get('/a.html')?.bytes), '<h1 title="Tom &amp; Jerry">Tom &amp; Jerry</h1>|Tom &amp; Jerry\n');
  assert.equal(text(r.output.get('/b.html')?.bytes), '<h1 title="Tom & Jerry">Tom & Jerry</h1>|<h1 title="Tom & Jerry">Tom & Jerry</h1>\n');
});

test('fragments in FOR loops, nested fragments, #each scope and imported partials', async () => {
  const r = await buildFiles({
    Sitefile: [
      'IMPORT posts FROM ./posts AS markdown SORT BY title',
      'IMPORT foot FROM ./partials/foot.html AS html',
      'LET nav: html = <nav>${#each posts as p}<a href="/${p.slug}.html">${p.title}</a>${/each}</nav>',
      'LET header: html <<HTML',
      '<header>${nav}</header>',
      'HTML',
      'LET card: html = <h2>${item.title}</h2>${#each item.tags as t}<i>${t}</i>${/each}',
      'RENDER --escape=html <<HTML TO /${item.slug}.html FOR item IN posts',
      '${header}${card}${foot}',
      'HTML',
      'EXPORT /',
    ].join('\n'),
    'posts/a.md': '---\ntitle: A & B\ntags: [x, "<y>"]\n---\nhi\n',
    'posts/b.md': '---\ntitle: C\n---\nho\n',
    'partials/foot.html': '<footer>${#each posts as p}${p.slug}${/each}</footer>\n',
  });
  const nav = '<nav><a href="/a.html">A &amp; B</a><a href="/b.html">C</a></nav>';
  assert.equal(text(r.output.get('/a.html')?.bytes), `<header>${nav}</header><h2>A &amp; B</h2><i>x</i><i>&lt;y&gt;</i><footer>ab</footer>\n`);
  assert.equal(text(r.output.get('/b.html')?.bytes), `<header>${nav}</header><h2>C</h2><footer>ab</footer>\n`);
});

test('a #each variable of the using template is visible inside the fragment', async () => {
  const r = await buildFiles({
    Sitefile: 'IMPORT d FROM ./d.json AS json\nLET card: html = <li>${x.n}</li>\nRENDER --escape=html <<H TO /a.html\n${#each d as x}${card}${/each}\nH\nEXPORT /',
    'd.json': '[{"n":1},{"n":"<2>"}]',
  });
  assert.equal(text(r.output.get('/a.html')?.bytes), '<li>1</li><li>&lt;2&gt;</li>\n');
});

async function buildError(files: Record<string, string>): Promise<{ code: string; message: string }> {
  try {
    await buildFiles(files);
  } catch (e) {
    assert.ok(isSitefileError(e), String(e));
    return { code: e.code, message: e.message };
  }
  assert.fail('expected the build to fail');
}

test('errors inside a fragment name the fragment line and the use', async () => {
  const e = await buildError({ Sitefile: 'LET card: html = <i>${item.title}</i>\nRENDER --escape=html <<H TO /a.html\nx ${card}\nH\nEXPORT /' });
  assert.deepEqual(e, { code: 'E_UNDEFINED', message: "Sitefile:1:21: 'item' is not an ARG, LET or IMPORT (in html fragment 'card' used at Sitefile:3:3)" });
  const t = await buildError({ Sitefile: 'IMPORT p FROM ./p.html AS html\nRENDER <<H TO /a.txt\n${p}\nH\nEXPORT /', 'p.html': 'ok\n${#if}\n' });
  assert.deepEqual(t, { code: 'E_TEMPLATE', message: "./p.html:2:1: expected ${#if value} or ${#if !value}, got '${#if}' (in html fragment 'p' used at Sitefile:3:1)" });
  const v = await buildError({ Sitefile: 'LET a: html = x\nRENDER <<H TO /a.txt\n${#if a}y${/if}\nH\nEXPORT /' });
  assert.deepEqual(v, { code: 'E_TYPE', message: "Sitefile:3:1: 'a' is an html fragment, not a value; insert it with ${a}" });
});

test('a partial that reaches itself is E_TEMPLATE, naming the chain', async () => {
  const e = await buildError({
    Sitefile: 'IMPORT a FROM ./a.html AS html\nLET b: html = <b>${a}</b>\nRENDER <<H TO /x.txt\n${b}\nH\nEXPORT /',
    'a.html': 'x ${b}\n',
  });
  assert.equal(e.code, 'E_TEMPLATE');
  assert.equal(e.message, "./a.html:1:3: html fragment 'b' uses itself (b → a → b) (in html fragment 'a' used at Sitefile:2:18) (in html fragment 'b' used at Sitefile:4:1)");
});

test('an html import of a directory is E_TYPE; a partial must be UTF-8', async () => {
  const d = await buildError({ Sitefile: 'IMPORT p FROM ./dir AS html\nEXPORT /', 'dir/a.html': 'x' });
  assert.equal(d.code, 'E_TYPE');
  const dir = await project({ Sitefile: 'IMPORT p FROM ./p.html AS html\nEXPORT /' });
  await writeFile(path.join(dir, 'p.html'), new Uint8Array([0xff, 0xfe]));
  await assert.rejects(build({ contextDir: dir, lockfile: false }), (e: unknown) => isSitefileError(e) && e.code === 'E_ENCODING');
});

test('incremental rebuilds see edits of html LETs and partials', async () => {
  const memo = new BuildMemo();
  const files = {
    Sitefile: 'IMPORT p FROM ./p.html AS html\nLET a: html = <b>${p}</b>\nRENDER <<H TO /x.txt\n${a}\nH\nEXPORT /',
    'p.html': 'one',
  };
  const dir = await project(files);
  const once = async () => text((await build({ contextDir: dir, lockfile: false, memo })).output.get('/x.txt')?.bytes);
  assert.equal(await once(), '<b>one</b>\n');
  await writeFile(path.join(dir, 'p.html'), 'two');
  assert.equal(await once(), '<b>two</b>\n');
  await writeFile(path.join(dir, 'Sitefile'), files.Sitefile.replace('<b>', '<i>'));
  assert.equal(await once(), '<i>two</b>\n');
});
