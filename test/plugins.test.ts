import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm, symlink } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { sha256 } from '../src/digest.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-plugins-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);
// These tests exercise JS plugins, so they allow them (as --allow-plugins would) unless a test says otherwise.
async function run(files: Record<string, string>, extra: Partial<BuildOptions> = {}) {
  return build({ contextDir: await project(files), lockfile: false, allowAllPlugins: true, ...extra });
}
async function rejects(files: Record<string, string>, code: ErrorCode, match?: RegExp, extra: Partial<BuildOptions> = {}): Promise<void> {
  await assert.rejects(run(files, extra), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    if (match) assert.match(e.message, match);
    return true;
  });
}

/** Echoes what it received, so tests can see the request. */
const ECHO = `export async function transform(req) {
  return JSON.stringify({ dest: req.dest, options: req.options, input: req.input && { path: req.input.path, name: req.input.name, text: req.input.text, size: req.input.bytes.length }, data: req.data, item: req.item });
}`;

test('PLUGIN / TRANSFORM parse and validate', () => {
  const prog = parseProgram(
    'FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nIMPORT posts FROM ./p AS markdown\nTRANSFORM ./a.txt WITH up TO /a.txt OPTIONS x=1 y="two words"\nTRANSFORM WITH up TO /c/${post.slug}.svg OPTIONS t=${post.title} FOR post IN posts\nEXPORT /',
  );
  const t = prog.instructions[3]!;
  assert.ok(t.kind === 'TRANSFORM' && t.plugin === 'up' && t.options.length === 2 && t.source !== undefined);
  const f = prog.instructions[4]!;
  assert.ok(f.kind === 'TRANSFORM' && f.source === undefined && f.each?.as === 'post');

  const fails = (src: string, code: ErrorCode, line: number): void =>
    assert.throws(() => parseProgram(src), (e: unknown) => isSitefileError(e, code) && e.span?.line === line, src);
  fails('FROM scratch\nTRANSFORM ./a WITH nope TO /a\nEXPORT /', 'E_UNDEFINED', 2);
  fails('FROM scratch\nARG p = x\nPLUGIN p FROM ./plugins/p.mjs\nEXPORT /', 'E_DUPLICATE', 3);
  fails('FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a OPTIONS a=1 OPTIONS b=2\nEXPORT /', 'E_SYNTAX', 3);
  fails('FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /a OPTIONS a=1 a=2\nEXPORT /', 'E_SYNTAX', 3);
  fails('FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a WITH p TO /${x.y}\nEXPORT /', 'E_SYNTAX', 3);
});

test('TRANSFORM sends a file, options and dest; the plugin result is written', async () => {
  const r = await run({
    'plugins/echo.mjs': ECHO,
    'notes.txt': 'hello',
    Sitefile: 'FROM scratch\nARG N = 3\nPLUGIN echo FROM ./plugins/echo.mjs\nTRANSFORM ./notes.txt WITH echo TO /out.json OPTIONS n=${N} label="a b"\nEXPORT /',
  });
  assert.deepEqual(JSON.parse(text(r.output.get('/out.json')?.bytes)), {
    dest: '/out.json',
    options: { n: '3', label: 'a b' },
    input: { path: './notes.txt', name: 'notes.txt', text: 'hello', size: 5 },
  });
});

test('TRANSFORM with an inline body, a value import, and FOR over a list', async () => {
  const r = await run({
    'plugins/echo.mjs': ECHO,
    'people.csv': 'slug,name\nada,Ada\nalan,Alan\n',
    'cfg.json': '{"k":1}',
    Sitefile: [
      'FROM scratch',
      'PLUGIN echo FROM ./plugins/echo.mjs',
      'IMPORT people FROM ./people.csv AS csv',
      'IMPORT cfg FROM ./cfg.json AS json',
      'TRANSFORM <<T WITH echo TO /inline.json',
      'inline text',
      'T',
      'TRANSFORM cfg: WITH echo TO /data.json',
      'TRANSFORM WITH echo TO /p/${p.slug}.json OPTIONS who=${p.name} FOR p IN people',
      'EXPORT /',
    ].join('\n'),
  });
  assert.equal(JSON.parse(text(r.output.get('/inline.json')?.bytes)).input.text, 'inline text\n');
  assert.deepEqual(JSON.parse(text(r.output.get('/data.json')?.bytes)).data, { k: 1 });
  const ada = JSON.parse(text(r.output.get('/p/ada.json')?.bytes));
  assert.deepEqual(ada.options, { who: 'Ada' });
  assert.deepEqual(ada.item, { slug: 'ada', name: 'Ada' });
  assert.ok(r.output.get('/p/alan.json'));
});

test('a plugin can return several files under a directory destination, as bytes or strings', async () => {
  const r = await run({
    'plugins/many.mjs': "export default { apiVersion: 1, transform: () => ({ files: { 'a.txt': 'A', 'sub/b.bin': new Uint8Array([1, 2, 3]) } }) };",
    'xs.json': '[1]',
    Sitefile: 'FROM scratch\nPLUGIN many FROM ./plugins/many.mjs\nIMPORT xs FROM ./xs.json AS json\nTRANSFORM WITH many TO /out/ FOR x IN xs\nEXPORT /',
  });
  assert.deepEqual(r.output.entries().map(([p]) => p), ['/out/a.txt', '/out/sub/b.bin']);
  assert.deepEqual([...(r.output.get('/out/sub/b.bin')?.bytes ?? [])], [1, 2, 3]);
});

test('sandbox: no reads outside the plugin folder, no writes, processes, network or environment', async () => {
  const probe = `export async function transform() {
    const out = {};
    const t = async (k, f) => { try { await f(); out[k] = 'allowed'; } catch { out[k] = 'denied'; } };
    const fs = await import('node:fs/promises');
    await t('readOwn', () => fs.readFile(new URL('./helper.txt', import.meta.url)));
    await t('readContext', () => fs.readFile(new URL('../secret.txt', import.meta.url)));
    await t('readEtc', () => fs.readFile('/etc/hosts'));
    await t('write', () => fs.writeFile(new URL('./written.txt', import.meta.url), 'x'));
    await t('spawn', async () => (await import('node:child_process')).execFileSync('/bin/echo', ['x']));
    await t('net', () => new Promise(async (res, rej) => { const s = (await import('node:net')).connect(80, '1.1.1.1'); s.on('connect', () => { s.destroy(); res(); }); s.on('error', rej); }));
    out.env = Object.keys(process.env).filter((k) => k !== '__CF_USER_TEXT_ENCODING');
    return JSON.stringify(out);
  }`;
  const r = await run({
    'plugins/probe.mjs': probe,
    'plugins/helper.txt': 'ok',
    'secret.txt': 'TOP SECRET',
    'xs.json': '[1]',
    Sitefile: 'FROM scratch\nPLUGIN probe FROM ./plugins/probe.mjs\nIMPORT xs FROM ./xs.json AS json\nTRANSFORM WITH probe TO /r.json FOR x IN xs\nEXPORT /',
  });
  assert.deepEqual(JSON.parse(text(r.output.get('/r.json')?.bytes)), {
    readOwn: 'allowed',
    readContext: 'denied',
    readEtc: 'denied',
    write: 'denied',
    spawn: 'denied',
    net: 'denied',
    env: [],
  });
});

test('plugin errors, bad results, timeouts and placement rules', async () => {
  const site = (plugin: string, extra = ''): Record<string, string> => ({
    'plugins/p.mjs': plugin,
    'a.txt': 'a',
    Sitefile: `FROM scratch\nPLUGIN p FROM ./plugins/p.mjs${extra}\nTRANSFORM ./a.txt WITH p TO /a.out\nEXPORT /`,
  });
  await rejects(site("export function transform() { throw new Error('boom: bad input'); }"), 'E_PLUGIN', /boom: bad input/);
  await rejects(site('export function transform() { return 42; }'), 'E_PLUGIN', /must return/);
  await rejects(site('export const x = 1;'), 'E_PLUGIN', /must export transform/);
  await rejects(site('export const apiVersion = 2; export function transform() { return ""; }'), 'E_PLUGIN', /apiVersion/);
  await rejects(site('export function transform() { return new Promise(() => {}); }'), 'E_PLUGIN', /timed out/, { pluginPolicy: { timeoutMs: 300 } });
  await rejects(site("export function transform() { return { files: { '../escape.txt': 'x' } }; }"), 'E_PATH');
  await rejects(site('export function transform() { return "x"; }', ` INTEGRITY ${sha256('something else')}`), 'E_INTEGRITY');
  // A plugin may read its whole folder, so it can't sit at the context root.
  await rejects(
    { 'p.mjs': 'export function transform() { return "x"; }', 'a.txt': 'a', Sitefile: 'FROM scratch\nPLUGIN p FROM ./p.mjs\nTRANSFORM ./a.txt WITH p TO /a.out\nEXPORT /' },
    'E_VALIDATION',
    /own folder/,
  );
  await rejects(
    { Sitefile: 'FROM scratch\nPLUGIN p FROM https://example.com/p.mjs\nEXPORT /' },
    'E_PROTOCOL',
  );
  await rejects({ 'plugins/p.js': '', Sitefile: 'FROM scratch\nPLUGIN p FROM ./plugins/p.js\nEXPORT /' }, 'E_VALIDATION', /\.mjs/);
  // Pinned and correct: fine.
  const good = 'export function transform() { return "ok"; }';
  const r = await run(site(good, ` INTEGRITY ${sha256(good)}`));
  assert.equal(text(r.output.get('/a.out')?.bytes), 'ok');
});

test('a JS PLUGIN needs --allow-plugin NAME or --allow-plugins; it fails before anything starts', async () => {
  const site = (body: string): Record<string, string> => ({
    'plugins/up.mjs': 'export function transform(r) { return r.input.text.toUpperCase(); }',
    'plugins/other.mjs': 'export function transform() { return "o"; }',
    'a.txt': 'a',
    Sitefile: `FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\n${body}TRANSFORM ./a.txt WITH up TO /a.out\nEXPORT /`,
  });
  const denied = /: PLUGIN up runs JavaScript, which must be allowed explicitly: add --allow-plugin up \(or --allow-plugins to allow every PLUGIN\); WebAssembly plugins need no flag/;
  // Denied before any import is resolved: the (unreachable) URL import is never fetched.
  await rejects(site('IMPORT x FROM https://127.0.0.1:1/x.txt\n'), 'E_UNSUPPORTED', denied, { allowAllPlugins: false });
  await assert.rejects(run(site(''), { allowAllPlugins: false }), (e: unknown) => isSitefileError(e) && e.span?.line === 2);
  // Naming another plugin does not allow this one, and the declared order decides which fails first.
  await rejects(site('PLUGIN other FROM ./plugins/other.mjs\n'), 'E_UNSUPPORTED', denied, { allowAllPlugins: false, allowPlugin: ['other'] });
  await rejects(site('PLUGIN other FROM ./plugins/other.mjs\n'), 'E_UNSUPPORTED', /: PLUGIN other runs JavaScript/, { allowAllPlugins: false, allowPlugin: ['up'] });
  await rejects(site(''), 'E_USAGE', /--allow-plugin expects a PLUGIN name/, { allowAllPlugins: false, allowPlugin: ['bad-name'] });
  const warnings: string[] = [];
  const named = await run(site(''), { allowAllPlugins: false, allowPlugin: ['up', 'unused'], onWarning: (m) => warnings.push(m) });
  assert.equal(text(named.output.get('/a.out')?.bytes), 'A');
  assert.ok(warnings.includes('--allow-plugin unused does not match any PLUGIN and was ignored'), warnings.join('\n'));
  const all = await run(site('PLUGIN other FROM ./plugins/other.mjs\n'), { allowAllPlugins: true });
  assert.equal(text(all.output.get('/a.out')?.bytes), 'A');
  // A '.wasm' name that is a symlink to a JS module still runs as JavaScript, so it still needs the flag.
  const dir = await project({ 'plugins/up.mjs': 'export function transform() { return "js"; }', 'a.txt': 'a', Sitefile: 'FROM scratch\nPLUGIN up FROM ./plugins/up.wasm\nTRANSFORM ./a.txt WITH up TO /a.out\nEXPORT /' });
  await symlink('up.mjs', path.join(dir, 'plugins', 'up.wasm'));
  await assert.rejects(build({ contextDir: dir, lockfile: false }), (e: unknown) => isSitefileError(e) && e.code === 'E_UNSUPPORTED' && denied.test(e.message));
});

test('permission errors carry what must be allowed (needs), with the message unchanged', async () => {
  const dir = await project({
    'plugins/up.mjs': 'export function transform() { return "x"; }',
    'a.txt': 'a',
    Sitefile: 'FROM scratch\nPLUGIN up FROM ./plugins/up.mjs\nTRANSFORM ./a.txt WITH up TO /a.out\nEXPORT /',
  });
  await assert.rejects(build({ contextDir: dir, lockfile: false }), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_UNSUPPORTED'));
    assert.deepEqual(e.needs, { kind: 'plugin', name: 'up', flag: '--allow-plugin' });
    assert.match(e.message, /: PLUGIN up runs JavaScript, which must be allowed explicitly: add --allow-plugin up /);
    return true;
  });
  const tool = await project({ 'a.txt': 'a', Sitefile: 'FROM scratch\nTOOL ffmpeg\nTRANSFORM ./a.txt WITH ffmpeg TO /a.out ARGS {input} {output}\nEXPORT /' });
  await assert.rejects(build({ contextDir: tool, lockfile: false }), (e: unknown) => {
    assert.ok(isSitefileError(e, 'E_UNSUPPORTED'));
    assert.deepEqual(e.needs, { kind: 'tool', name: 'ffmpeg', flag: '--allow-exec' });
    assert.match(e.message, /TOOL ffmpeg runs an external program, which must be allowed explicitly/);
    return true;
  });
  // Other errors carry none.
  const other = await project({ Sitefile: 'FROM scratch\nCOPY ./missing.txt /m.txt\nEXPORT /' });
  await assert.rejects(build({ contextDir: other, lockfile: false }), (e: unknown) => isSitefileError(e) && e.needs === undefined);
});

test('plugin console output becomes build warnings, not output', async () => {
  const warnings: string[] = [];
  const r = await run(
    {
      'plugins/p.mjs': "export function transform() { console.log('working…'); return 'done'; }",
      'a.txt': 'a',
      Sitefile: 'FROM scratch\nPLUGIN p FROM ./plugins/p.mjs\nTRANSFORM ./a.txt WITH p TO /a.out\nEXPORT /',
    },
    { onWarning: (m) => warnings.push(m) },
  );
  assert.equal(text(r.output.get('/a.out')?.bytes), 'done');
  assert.ok(warnings.some((w) => w === '[plugin p] working…'), warnings.join('\n'));
});
