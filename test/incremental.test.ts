import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, copyFile, lstat, mkdir, mkdtemp, readdir, readFile, readlink, rm, symlink, unlink, utimes, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build, buildToDirectory, type BuildOptions, type BuildResult } from '../src/executor.js';
import { sha256 } from '../src/digest.js';
import { exportDirectory, ExportState } from '../src/exporter.js';
import { makeFile, Tree, type FileNode } from '../src/tree.js';
import { BuildMemo, memoKey, valueKey } from '../src/memo.js';
import { isSitefileError } from '../src/errors.js';
import { Profiler } from '../src/profile.js';

// Incremental rebuilds (src/memo.ts): a build with a memo must be byte-identical to a clean
// build: same files, modes, contents and origins, same warnings, same errors. The Rust
// implementation runs the same randomized test (rust/tests/incremental.rs).

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-incremental-'));
  temps.push(d);
  return d;
}

/** xorshift32, as in bench/generate.mjs (and the Rust port of this test). */
function rng(seed: number): () => number {
  let x = seed >>> 0 || 1;
  return () => {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    return x / 4294967296;
  };
}

async function put(file: string, text: string | Uint8Array): Promise<void> {
  await mkdir(path.dirname(file), { recursive: true });
  await writeFile(file, text);
}

const WORDS = 'alpha beta gamma delta tree stage render import export digest page post feed'.split(' ');

interface Outcome {
  readonly ok: boolean;
  readonly files?: string[];
  readonly tree?: string[];
  readonly warnings: string[];
  readonly error?: string;
  readonly result?: BuildResult;
}

async function outcome(options: BuildOptions, outDir?: string): Promise<Outcome & { incremental?: boolean }> {
  const warnings: string[] = [];
  try {
    const opts = { ...options, onWarning: (m: string) => warnings.push(m) };
    const r = outDir === undefined ? await build(opts) : await buildToDirectory({ ...opts, outDir, force: true });
    const list = (t: BuildResult['tree']): string[] => t.entries().map(([p, n]) => `${p} ${n.mode.toString(8)} ${n.digest} ${n.origin}`);
    return { ok: true, files: list(r.output), tree: list(r.tree), warnings, result: r, incremental: 'incremental' in r && r.incremental !== undefined };
  } catch (e) {
    if (!isSitefileError(e)) throw e;
    return { ok: false, warnings, error: `${e.code}: ${e.message}` };
  }
}

/** A site using every memoized step: imports of each kind, stages, COPY, RENDER (FOR), FEED, SITEMAP, TRANSFORM. */
class Site {
  posts = new Map<string, { title: string; date: string; body: string; tags: string[] }>();
  publicFiles = new Map<string, string>();
  postTemplate = 0;
  indexTemplate = 0;
  siteTitle = 'Site';
  rows: Array<[string, number]> = [];
  extraLine = false;
  swapped = false;
  badPost = false;
  arg = 'one';
  /** --target: build only the `assets` stage. */
  target: string | undefined = undefined;
  next = 0;

  constructor(
    readonly dir: string,
    readonly r: () => number,
  ) {}

  word(): string {
    return WORDS[Math.floor(this.r() * WORDS.length)]!;
  }

  words(n: number): string {
    return Array.from({ length: n }, () => this.word()).join(' ');
  }

  pick<T>(xs: readonly T[]): T {
    return xs[Math.floor(this.r() * xs.length)]!;
  }

  addPost(): void {
    const i = this.next++;
    const day = String(1 + (i % 28)).padStart(2, '0');
    this.posts.set(`2026-01-${day}-p${i}.md`, { title: `Post ${i} ${this.word()}`, date: `2026-02-${day}`, body: this.words(8), tags: [this.word()] });
  }

  async init(): Promise<void> {
    for (let i = 0; i < 4; i++) this.addPost();
    for (let i = 0; i < 3; i++) this.publicFiles.set(`d${i % 2}/f${i}.txt`, this.words(5));
    this.rows = [
      ['a', 1],
      ['b', 2],
    ];
    await copyFile(path.join(root, 'examples', 'wasm', 'plugins', 'badge.wasm'), path.join(await this.ensure('plugins'), 'badge.wasm'));
    await this.write();
  }

  async ensure(sub: string): Promise<string> {
    const d = path.join(this.dir, sub);
    await mkdir(d, { recursive: true });
    return d;
  }

  async write(): Promise<void> {
    await rm(path.join(this.dir, 'posts'), { recursive: true, force: true });
    await rm(path.join(this.dir, 'public'), { recursive: true, force: true });
    for (const [name, p] of this.posts) {
      await put(path.join(this.dir, 'posts', name), `---\ntitle: "${p.title}"\ndate: ${p.date}\ntags: [${p.tags.join(', ')}]\n---\n# ${p.title}\n\n${p.body}\n`);
    }
    if (this.badPost) await put(path.join(this.dir, 'posts', 'zz-bad.md'), '---\ntitle: [unclosed\n---\nbody\n');
    for (const [name, text] of this.publicFiles) await put(path.join(this.dir, 'public', name), text);
    const postTemplates = [
      '<h1>${post.title}</h1>${post.content|raw}<p>${post.date} ${SITE}</p>',
      '<article><h2>${post.title}</h2>${#each post.tags as t}<i>${t}</i>${/each}</article>',
      '<h1>${post.title}</h1><nav>${#each posts as o}<a href="/${o.slug}">${o.title}</a>${/each}</nav>',
      '<h1>${post.title}</h1><footer>${site.title}</footer>',
    ];
    const indexTemplates = ['<ul>${#each posts as p}<li>${p.title}</li>${/each}</ul>', '<h1>${site.title}</h1>${#if posts}<p>posts</p>${/if}'];
    await put(path.join(this.dir, 'templates', 'post.html'), postTemplates[this.postTemplate]!);
    await put(path.join(this.dir, 'templates', 'index.html'), indexTemplates[this.indexTemplate]!);
    await put(path.join(this.dir, 'data', 'site.json'), JSON.stringify({ title: this.siteTitle }));
    await put(path.join(this.dir, 'data', 'rows.csv'), 'name,n\n' + this.rows.map(([a, n]) => `${a},${n}`).join('\n') + '\n');
    const a = 'COPY ./public/ /static/';
    const b = 'COPY --from=assets /x/ /assets/';
    const lines = [
      'ARG SITE = "S"',
      'PLUGIN badge FROM ./plugins/badge.wasm',
      'IMPORT posts FROM ./posts AS markdown SORT BY date DESC',
      'IMPORT site FROM ./data/site.json AS json',
      'IMPORT rows FROM ./data/rows.csv AS csv',
      'IMPORT pub FROM ./public',
      'STAGE assets FROM scratch',
      'COPY pub:/ /x/',
      'FROM scratch',
      ...(this.swapped ? [b, a] : [a, b]),
      ...(this.extraLine ? ['COPY ./data/site.json /site.json'] : []),
      // A file, or a file in a directory once ./public has a d2/ (the input tree decides).
      'COPY ./data/rows.csv /static/d2',
      'RENDER --escape=html ./templates/index.html TO /index.html',
      'RENDER --escape=html ./templates/post.html TO /posts/${post.slug}.html FOR post IN posts',
      'RENDER <<T TO /rows/${row.name}.txt FOR row IN rows',
      '${row.name}=${row.n} ${SITE}',
      'T',
      'TRANSFORM WITH badge TO /badges/${post.slug}.svg FOR post IN posts',
      'FEED posts BASE https://example.com LINK /posts/${item.slug}.html TITLE "Feed"',
      'SITEMAP BASE https://example.com',
      'REDIRECT /old /index.html',
      'EXPORT /',
    ];
    await put(path.join(this.dir, 'Sitefile'), lines.join('\n') + '\n');
  }

  /** One random edit. Returns what it did, for failure messages. */
  async mutate(): Promise<string> {
    const names = [...this.posts.keys()];
    const kind = this.pick([
      'post-body',
      'post-title',
      'post-date',
      'add-post',
      'remove-post',
      'post-template',
      'index-template',
      'public-edit',
      'public-add',
      'public-remove',
      'site-json',
      'rows',
      'arg',
      'sitefile-line',
      'sitefile-swap',
      'bad-post',
      'target',
      'nothing',
    ] as const);
    switch (kind) {
      case 'post-body':
        this.posts.get(this.pick(names))!.body = this.words(6);
        break;
      case 'post-title':
        this.posts.get(this.pick(names))!.title = `T ${this.words(2)}`;
        break;
      case 'post-date':
        this.posts.get(this.pick(names))!.date = `2026-03-${String(1 + Math.floor(this.r() * 28)).padStart(2, '0')}`;
        break;
      case 'add-post':
        this.addPost();
        break;
      case 'remove-post':
        if (names.length > 1) this.posts.delete(this.pick(names));
        break;
      case 'post-template':
        this.postTemplate = Math.floor(this.r() * 4);
        break;
      case 'index-template':
        this.indexTemplate = 1 - this.indexTemplate;
        break;
      case 'public-edit': {
        const k = [...this.publicFiles.keys()];
        if (k.length) this.publicFiles.set(this.pick(k), this.words(4));
        break;
      }
      case 'public-add':
        this.publicFiles.set(`d${Math.floor(this.r() * 3)}/n${this.next++}.txt`, this.words(3));
        break;
      case 'public-remove': {
        const k = [...this.publicFiles.keys()];
        if (k.length > 1) this.publicFiles.delete(this.pick(k));
        break;
      }
      case 'site-json':
        this.siteTitle = this.words(2);
        break;
      case 'rows':
        this.rows = this.rows.map(([a, n]) => [a, this.r() < 0.5 ? n + 1 : n]);
        if (this.r() < 0.3) this.rows.push([`r${this.next++}`, 0]);
        break;
      case 'arg':
        this.arg = this.word();
        break;
      case 'sitefile-line':
        this.extraLine = !this.extraLine;
        break;
      case 'sitefile-swap':
        this.swapped = !this.swapped;
        break;
      case 'bad-post':
        this.badPost = !this.badPost;
        break;
      case 'target':
        this.target = this.target === undefined ? 'assets' : undefined;
        break;
      case 'nothing':
        break;
    }
    await this.write();
    return kind;
  }
}

/** Everything under `dir`, for byte-for-byte comparison: type, path, permission bits, contents. */
async function listDir(dir: string, rel = ''): Promise<string[]> {
  const out: string[] = [];
  for (const name of (await readdir(path.join(dir, rel))).sort()) {
    const p = `${rel}/${name}`;
    const st = await lstat(path.join(dir, p));
    const mode = (st.mode & 0o7777).toString(8);
    if (st.isSymbolicLink()) out.push(`l ${p} -> ${await readlink(path.join(dir, p))}`);
    else if (st.isDirectory()) out.push(`d ${p} ${mode}`, ...(await listDir(dir, p)));
    else out.push(`f ${p} ${mode} ${sha256(await readFile(path.join(dir, p)))}`);
  }
  return out;
}

const TAMPERS = ['rewrite-same-size', 'append', 'add-file', 'add-dir', 'delete-file', 'delete-dir', 'chmod', 'file-to-symlink', 'dir-to-symlink'] as const;

/** Modify an exported directory behind the exporter's back (`victim`: a directory outside it). */
async function tamper(out: string, victim: string, kind: (typeof TAMPERS)[number], pick: <T>(xs: readonly T[]) => T): Promise<void> {
  const all = await listDir(out);
  const files = all.filter((l) => l.startsWith('f ')).map((l) => l.split(' ')[1]!);
  const dirs = all.filter((l) => l.startsWith('d ')).map((l) => l.split(' ')[1]!);
  const at = (p: string): string => path.join(out, p);
  if (!files.length) return;
  const f = pick(files);
  switch (kind) {
    case 'rewrite-same-size': {
      // Same size, mtime put back: only the ctime tells.
      const st = await lstat(at(f));
      const bytes = await readFile(at(f));
      if (bytes.length) bytes[0] = bytes[0] === 0x58 ? 0x59 : 0x58;
      await writeFile(at(f), bytes);
      await utimes(at(f), st.atime, st.mtime);
      break;
    }
    case 'append':
      await writeFile(at(f), 'tampered', { flag: 'a' });
      break;
    case 'add-file':
      await writeFile(path.join(out, dirs.length ? pick(dirs) : '', 'stray.txt'), 'stray');
      break;
    case 'add-dir':
      await mkdir(path.join(out, 'stray-dir'), { recursive: true });
      break;
    case 'delete-file':
      await unlink(at(f));
      break;
    case 'delete-dir':
      if (dirs.length) await rm(at(pick(dirs)), { recursive: true });
      break;
    case 'chmod':
      await chmod(at(f), 0o600);
      break;
    case 'file-to-symlink':
      await unlink(at(f));
      await symlink(path.join(victim, 'secret.txt'), at(f));
      break;
    case 'dir-to-symlink':
      if (dirs.length) {
        const d = pick(dirs);
        await rm(at(d), { recursive: true });
        await symlink(victim, at(d));
      }
      break;
  }
}

for (const seed of [1, 7, 42, 1234, 99991]) {
  test(`incremental builds equal clean builds across random edits (seed ${seed})`, async () => {
    const dir = await tmp();
    const site = new Site(path.join(dir, 'ctx'), rng(seed));
    await site.init();
    // The exports (watch mode writes only the changes; the clean one rewrites everything),
    // and a directory outside them that tampering points symlinks at.
    const incOut = path.join(dir, 'out-incremental');
    const cleanOut = path.join(dir, 'out-clean');
    const victim = path.join(dir, 'victim');
    await put(path.join(victim, 'secret.txt'), 'secret');
    const victimBefore = await listDir(victim);
    let incrementalExports = 0;
    let tampered = 0;
    const memo = new BuildMemo();
    // One seed also uses the TRANSFORM disk cache (shared by both builds, as in watch mode).
    const cache = seed === 42 ? { cacheDir: path.join(dir, 'cache') } : {};
    const base = (): BuildOptions => ({ contextDir: site.dir, lockfile: false, buildArgs: { SITE: site.arg }, ...(site.target !== undefined ? { target: site.target } : {}), ...cache });
    let reused = 0;
    const history: string[] = [];
    for (let step = 0; step < 60; step++) {
      // One to three edits between builds, like saving several files at once.
      const edits: string[] = [];
      if (step > 0) for (let n = 1 + Math.floor(site.r() * 3); n > 0; n--) edits.push(await site.mutate());
      history.push(step === 0 ? 'initial' : edits.join('+'));
      // Sometimes the export is modified between rebuilds.
      if (step > 0 && site.r() < 0.2) {
        const kind = site.pick(TAMPERS);
        await tamper(incOut, victim, kind, (xs) => site.pick(xs));
        history.push(`tamper:${kind}`);
        tampered++;
      }
      const before = await listDir(incOut).catch(() => undefined);
      const incremental = await outcome({ ...base(), memo }, incOut);
      const clean = await outcome(base(), cleanOut);
      const where = `seed ${seed}, step ${step}: ${history.join(' > ')}`;
      if (incremental.ok) {
        // The export is byte for byte (and mode for mode) the clean one.
        assert.deepEqual(await listDir(incOut), await listDir(cleanOut), where);
        if (incremental.incremental) incrementalExports++;
      } else {
        // A failed build never touches the output.
        assert.deepEqual(await listDir(incOut).catch(() => undefined), before, where);
      }
      assert.deepEqual(await listDir(victim), victimBefore, where);
      assert.equal(incremental.ok, clean.ok, `${where}\n${incremental.error ?? ''}${clean.error ?? ''}`);
      assert.equal(incremental.error, clean.error, where);
      assert.deepEqual(incremental.warnings, clean.warnings, where);
      assert.deepEqual(incremental.files, clean.files, where);
      assert.deepEqual(incremental.tree, clean.tree, where);
      if (incremental.ok) reused += incremental.result!.memoStats!.hits;
    }
    // The memo was actually used, and exports wrote only the changes (the others: the first
    // export, large changes such as switching --target, and the exports after tampering).
    assert.ok(reused > 0);
    assert.ok(incrementalExports >= 8, `${incrementalExports} incremental exports`);
    assert.ok(tampered > 0);
  });
}

test('an unchanged rebuild reuses every step; an edit re-runs only what read it', async () => {
  const dir = await tmp();
  const ctx = path.join(dir, 'ctx');
  const site = new Site(ctx, rng(3));
  await site.init();
  const memo = new BuildMemo();
  const opts = (): BuildOptions => ({ contextDir: ctx, lockfile: false, memo, buildArgs: { SITE: site.arg } });
  const first = await build(opts());
  assert.equal(first.memoStats!.hits, 0);
  const again = await build(opts());
  assert.equal(again.memoStats!.misses, 0);
  assert.ok(again.memoStats!.hits > 0);
  assert.equal(again.transformStats.pluginCalls, 0); // TRANSFORMs were not run again
  assert.equal(again.output.digest(), first.output.digest());

  // Editing one post's body: that document is parsed again and its page rendered again;
  // with a template that does not list every post, the other pages are reused.
  site.postTemplate = 0;
  await site.write();
  await build(opts());
  const name = [...site.posts.keys()][1]!;
  site.posts.get(name)!.body = 'changed body';
  await site.write();
  const profiler = new Profiler('build');
  const edited = await build({ ...opts(), profiler });
  const clean = await build({ contextDir: ctx, lockfile: false, buildArgs: { SITE: site.arg } });
  assert.equal(edited.output.digest(), clean.output.digest());
  const states = (JSON.parse(profiler.toJSON()) as { traceEvents: Array<{ cat?: string; name: string; args: Record<string, unknown> }> }).traceEvents
    .filter((e) => e.cat === 'operation')
    .map((e) => `${e.name.replace(/^.*Sitefile:/, '')} ${String(e.args['memo'])}`);
  assert.deepEqual(states, [
    '8 COPY hit', // the stage reads ./public only
    '10 COPY hit',
    '11 COPY hit',
    '12 COPY hit',
    '13 RENDER miss', // the index lists the posts
    '14 RENDER miss', // ... and so does the FOR list, but only the edited post's page is rendered again
    '15 RENDER files', // the rows pages read nothing that changed: their files are reused, then added to the new tree
    '18 TRANSFORM miss',
    '19 FEED miss',
    '20 SITEMAP miss',
  ]);
  assert.equal(edited.transformStats.pluginCalls, 1); // one badge drawn again, the others reused
});

test('memo keys: valueKey tells undefined, NaN and Infinity from null; memoKey separates its parts', () => {
  assert.notEqual(valueKey([null]), valueKey([undefined]));
  assert.notEqual(valueKey({ a: null }), valueKey({ a: Number.NaN }));
  assert.notEqual(valueKey(Infinity), valueKey(-Infinity));
  assert.notEqual(valueKey({ a: 1 }), valueKey({ a: '1' }));
  assert.notEqual(memoKey('ab', 'c'), memoKey('a', 'bc'));
});

test('BuildMemo: entries unused by the last build are dropped, but not while builds fail', () => {
  const m = new BuildMemo();
  m.begin();
  m.set('a', 1);
  m.set('b', 2);
  m.finish(true);
  m.begin();
  assert.equal(m.get('a'), 1);
  m.finish(true);
  assert.equal(m.size, 1); // b was not used
  m.begin();
  m.set('c', 3);
  m.finish(false); // a failed build keeps the last good build's entries
  assert.equal(m.size, 2);
  m.begin();
  m.finish(true);
  assert.equal(m.size, 0);
});

test('build --watch keeps a memo; SITEFILE_NO_INCREMENTAL turns it off', async () => {
  const { watchMemo } = await import('../src/memo.js');
  const saved = process.env['SITEFILE_NO_INCREMENTAL'];
  try {
    delete process.env['SITEFILE_NO_INCREMENTAL'];
    assert.ok(watchMemo());
    process.env['SITEFILE_NO_INCREMENTAL'] = '0';
    assert.ok(watchMemo());
    process.env['SITEFILE_NO_INCREMENTAL'] = '1';
    assert.equal(watchMemo(), undefined);
  } finally {
    if (saved === undefined) delete process.env['SITEFILE_NO_INCREMENTAL'];
    else process.env['SITEFILE_NO_INCREMENTAL'] = saved;
  }
});

// --- incremental export (watch mode) -----------------------------------------------------

const enc = (s: string): Uint8Array => new TextEncoder().encode(s);

function files(n: number, extra: Record<string, string | [string, number]> = {}): Tree {
  const list: Array<[string, FileNode]> = [];
  for (let i = 0; i < n; i++) if (!(`/d${i % 3}/f${i}.txt` in extra)) list.push([`/d${i % 3}/f${i}.txt`, makeFile(enc(`file ${i}`), 'test')]);
  for (const [p, v] of Object.entries(extra)) list.push([p, typeof v === 'string' ? makeFile(enc(v), 'test') : makeFile(enc(v[0]), 'test', v[1])]);
  return Tree.fromFiles(list);
}

type Applied = { readonly incremental?: { readonly written: number; readonly removed: number } };

/** An incremental export (its state kept across calls), compared with a clean export after each. */
async function exportPair(): Promise<{ dir: string; out: string; run: (t: Tree, beforeApply?: () => Promise<void>) => Promise<Applied> }> {
  const dir = await tmp();
  const ctx = path.join(dir, 'ctx');
  const out = path.join(ctx, 'dist');
  await mkdir(ctx, { recursive: true });
  const state = new ExportState();
  const run = async (t: Tree, beforeApply?: () => Promise<void>): Promise<Applied> => {
    const r = await exportDirectory(t, out, { contextDir: ctx, incremental: state, ...(beforeApply ? { beforeApply } : {}) });
    const clean = path.join(ctx, 'clean');
    await exportDirectory(t, clean, { contextDir: ctx });
    assert.deepEqual(await listDir(out), await listDir(clean));
    return r;
  };
  return { dir, out, run };
}

const ino = async (p: string): Promise<number> => (await lstat(p)).ino;

test('incremental export: an unchanged export writes nothing; an edit writes one file', async () => {
  const { out, run } = await exportPair();
  assert.equal((await run(files(20))).incremental, undefined); // the first export is a full one
  const one = await ino(path.join(out, 'd1/f1.txt'));
  const other = await ino(path.join(out, 'd0/f0.txt'));
  assert.deepEqual((await run(files(20))).incremental, { written: 0, removed: 0 });
  assert.equal(await ino(path.join(out, 'd1/f1.txt')), one);
  assert.deepEqual((await run(files(20, { '/d1/f1.txt': 'edited' }))).incremental, { written: 1, removed: 0 });
  assert.notEqual(await ino(path.join(out, 'd1/f1.txt')), one);
  assert.equal(await ino(path.join(out, 'd0/f0.txt')), other);
  // A mode change alone is a change.
  assert.deepEqual((await run(files(20, { '/d1/f1.txt': ['edited', 0o755] }))).incremental, { written: 1, removed: 0 });
  assert.equal((await lstat(path.join(out, 'd1/f1.txt'))).mode & 0o777, 0o755);
});

test('incremental export: removed files go, directories that become empty go, a file can become a directory', async () => {
  const { out, run } = await exportPair();
  await run(files(20, { '/x/y/z.txt': 'z', '/p': 'file' }));
  assert.deepEqual((await run(files(20, { '/p/q.txt': 'now a directory' }))).incremental, { written: 1, removed: 2 });
  await assert.rejects(lstat(path.join(out, 'x')), { code: 'ENOENT' });
  // ... and back.
  assert.deepEqual((await run(files(20, { '/p': 'a file again' }))).incremental, { written: 1, removed: 1 });
});

test('incremental export: a large change is a full export; so is a change of letter case on a case-insensitive disk', async () => {
  const { run } = await exportPair();
  await run(files(20, { '/About.html': 'a' }));
  assert.equal((await run(files(5))).incremental, undefined);
  await run(files(20, { '/About.html': 'a' }));
  // /about.html may be /About.html on this file system: it is never renamed over in place.
  await run(files(20, { '/about.html': 'b' }));
});

test('incremental export: changes made to the output between exports are detected and undone', async () => {
  for (const kind of TAMPERS) {
    const { dir, out, run } = await exportPair();
    const victim = path.join(dir, 'victim');
    await put(path.join(victim, 'secret.txt'), 'secret');
    await run(files(20));
    let n = 0;
    await tamper(out, victim, kind, (xs) => xs[n++ % xs.length]!);
    assert.equal((await run(files(20, { '/d0/f0.txt': 'edit' }))).incremental, undefined, kind);
    assert.deepEqual(await listDir(victim), [`f /secret.txt 644 ${sha256(enc('secret'))}`], kind);
    // Then incremental again.
    assert.deepEqual((await run(files(20, { '/d0/f0.txt': 'edit 2' }))).incremental, { written: 1, removed: 0 }, kind);
  }
});

test('incremental export: a change made while the files are staged is caught before it is replaced', async () => {
  const { out, run } = await exportPair();
  await run(files(20));
  // The file about to be replaced is modified after the check: the full swap runs instead.
  const r = await run(files(20, { '/d0/f0.txt': 'edit' }), () => writeFile(path.join(out, 'd0/f0.txt'), 'someone else'));
  assert.equal(r.incremental, undefined);
  // A directory appears where one is about to be created.
  const r2 = await run(files(20, { '/d0/f0.txt': 'edit', '/new/a.txt': 'a' }), () => mkdir(path.join(out, 'new')));
  assert.equal(r2.incremental, undefined);
  assert.deepEqual((await readdir(path.dirname(out))).filter((n) => n.includes('sitefile-')), []); // no staging left behind
});

test('incremental export: another destination, a replaced destination, or no state is a full export', async () => {
  const { dir, run } = await exportPair();
  await run(files(20));
  const state = new ExportState();
  const ctx = path.join(dir, 'ctx');
  const exp = (to: string, s?: ExportState): Promise<Applied> => exportDirectory(files(20), path.join(ctx, to), { contextDir: ctx, ...(s ? { incremental: s } : {}) });
  await exp('a', state);
  assert.equal((await exp('b', state)).incremental, undefined);
  assert.ok((await exp('b', state)).incremental);
  // The same path, another directory.
  await rm(path.join(ctx, 'b'), { recursive: true });
  await exp('b');
  assert.equal((await exp('b', state)).incremental, undefined);
  assert.equal((await exp('b')).incremental, undefined);
});
