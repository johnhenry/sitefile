import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, readFile, rm, cp, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// lib/test/cli.test.js -> repo root
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function sitefile(args: string[], cwd = root): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd }, (err, stdout, stderr) => {
      const code = err ? (typeof err.code === 'number' ? err.code : 1) : 0;
      resolve({ code, stdout, stderr });
    });
  });
}

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-cli-'));
  temps.push(d);
  return d;
}

test('MVP acceptance: examples/hello builds dist/index.html with the title and copied CSS', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'hello');
  await cp(path.join(root, 'examples', 'hello'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Exported 2 files/);
  const html = await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8');
  assert.match(html, /<title>Hello<\/title>/);
  assert.match(html, /Welcome to Hello\./);
  assert.equal(
    await readFile(path.join(ctx, 'dist', 'style.css'), 'utf8'),
    await readFile(path.join(root, 'examples', 'hello', 'public', 'style.css'), 'utf8'),
  );

  const again = await sitefile(['build', ctx, '--build-arg', 'TITLE=<Sitefile>']);
  assert.equal(again.code, 0, again.stderr);
  assert.match(await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8'), /<title>&lt;Sitefile&gt;<\/title>/);
});

test('validate and inspect', async () => {
  const hello = path.join(await tmp(), 'hello');
  await cp(path.join(root, 'examples', 'hello'), hello, { recursive: true });
  const v = await sitefile(['validate', hello]);
  assert.equal(v.code, 0, v.stderr);
  assert.match(v.stdout, /OK \(0 imports, 2 operations\)/);

  const i = await sitefile(['inspect', hello, '--json']);
  assert.equal(i.code, 0, i.stderr);
  const data = JSON.parse(i.stdout);
  assert.deepEqual(
    data.files.map((f: { path: string }) => f.path),
    ['/index.html', '/style.css'],
  );
  await assert.rejects(stat(path.join(hello, 'dist')), 'inspect must not write output');
});

test('errors print code and location and exit 1; usage errors exit 2', async () => {
  const dir = await tmp();
  const { writeFile } = await import('node:fs/promises');
  await writeFile(path.join(dir, 'Sitefile'), 'FROM scratch\nRUN rm -rf /\nEXPORT /\n');
  const r = await sitefile(['validate', dir]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /error\[E_UNSUPPORTED\]: .*Sitefile:2:1:/);

  const u = await sitefile(['build', dir, '--bogus']);
  assert.equal(u.code, 2);
  const n = await sitefile(['frobnicate']);
  assert.equal(n.code, 2);
});

test('refuses to replace a non-empty directory outside the context without --force', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'hello');
  await cp(path.join(root, 'examples', 'hello'), ctx, { recursive: true });
  const outside = path.join(work, 'precious');
  const { mkdir, writeFile } = await import('node:fs/promises');
  await mkdir(outside);
  await writeFile(path.join(outside, 'keep.txt'), 'keep');

  const r = await sitefile(['build', ctx, '-o', outside]);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /E_EXPORT_UNSAFE/);
  assert.equal(await readFile(path.join(outside, 'keep.txt'), 'utf8'), 'keep');

  const forced = await sitefile(['build', ctx, '-o', outside, '--force']);
  assert.equal(forced.code, 0, forced.stderr);
  await assert.rejects(stat(path.join(outside, 'keep.txt')));

  const unsafe = await sitefile(['build', ctx, '-o', work, '--force']);
  assert.equal(unsafe.code, 1);
  assert.match(unsafe.stderr, /E_EXPORT_UNSAFE/);
});

test('examples/multistage builds from a local stage and COPY --from', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'multistage');
  await cp(path.join(root, 'examples', 'multistage'), ctx, { recursive: true });
  const v = await sitefile(['validate', ctx]);
  assert.equal(v.code, 0, v.stderr);
  assert.match(v.stdout, /3 stages/);
  const r = await sitefile(['build', ctx, '--build-arg', 'TITLE=Staged']);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /Exported 3 files/);
  assert.equal(await readFile(path.join(ctx, 'dist', 'static', 'site.css'), 'utf8'), 'body { font-family: sans-serif; }\n');
  assert.equal(await readFile(path.join(ctx, 'dist', 'raw-index.html'), 'utf8'), '<h1>Home</h1>\n');
  assert.match(await readFile(path.join(ctx, 'dist', 'about.html'), 'utf8'), /<title>Staged<\/title>/);
});

test('examples/inline builds a site whose template and CSS live in the Sitefile', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'inline');
  await cp(path.join(root, 'examples', 'inline'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx, '--build-arg', 'TITLE=A & B']);
  assert.equal(r.code, 0, r.stderr);
  const html = await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8');
  assert.match(html, /<title>A &amp; B<\/title>/);
  assert.match(html, /<code>\$\{TITLE\}<\/code>/);
  assert.match(await readFile(path.join(ctx, 'dist', 'style.css'), 'utf8'), /\$\{not substituted\}/);
});

test('examples/blog builds an index, one page per post and an Atom feed', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'blog');
  await cp(path.join(root, 'examples', 'blog'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  const out = path.join(ctx, 'dist');
  const index = await readFile(path.join(out, 'index.html'), 'utf8');
  // newest first
  assert.ok(index.indexOf('Markdown that stays safe') < index.indexOf('Hello, world'));
  const post = await readFile(path.join(out, 'posts', 'markdown-that-stays-safe', 'index.html'), 'utf8');
  assert.match(post, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
  assert.doesNotMatch(post, /href="javascript:/i);
  assert.match(post, /<li>security<\/li>/);
  const feed = await readFile(path.join(out, 'feed.xml'), 'utf8');
  assert.match(feed, /<updated>2026-03-01T00:00:00Z<\/updated>/);
  assert.equal((feed.match(/<entry>/g) ?? []).length, 3);
});

test('examples/data reads YAML, CSV and a Web Bundle', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'data');
  await cp(path.join(root, 'examples', 'data'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  const out = path.join(ctx, 'dist');
  const index = await readFile(path.join(out, 'index.html'), 'utf8');
  assert.match(index, /<a href="\/hello\/">Unpacked bundle<\/a>/);
  // oldest first by the numeric 'joined' column
  assert.ok(index.indexOf('Ada Lovelace') < index.indexOf('Alan Turing') && index.indexOf('Alan Turing') < index.indexOf('Grace Hopper'));
  assert.match(index, /Computability &quot;and&quot; cryptanalysis/);
  assert.match(index, /version b2/);
  assert.match(index, /<td>https:\/\/hello\.example\/style\.css<\/td><td>text\/css; charset=utf-8<\/td>/);
  assert.match(await readFile(path.join(out, 'team', 'ada', 'index.html'), 'utf8'), /<h1>Ada Lovelace<\/h1>/);
  assert.match(await readFile(path.join(out, 'hello', 'index.html'), 'utf8'), /<title>Hello<\/title>/);
});

test('examples/motion renders animated SVGs through a sandboxed plugin', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'motion');
  await cp(path.join(root, 'examples', 'motion'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  const title = await readFile(path.join(ctx, 'dist', 'title.svg'), 'utf8');
  assert.match(title, /^<\?xml[\s\S]*<svg[\s\S]*@keyframes[\s\S]*<\/svg>\s*$/);
  assert.match(title, /prefers-reduced-motion/);
  const index = await readFile(path.join(ctx, 'dist', 'index.html'), 'utf8');
  for (const m of index.matchAll(/src="\/cards\/([^"]+)\.svg"/g)) {
    assert.match(await readFile(path.join(ctx, 'dist', 'cards', `${m[1]}.svg`), 'utf8'), /<svg/);
  }
});

test('examples/wasm draws badges with a WebAssembly plugin built from Rust', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'wasm');
  await cp(path.join(root, 'examples', 'wasm'), ctx, { recursive: true });
  const r = await sitefile(['build', ctx]);
  assert.equal(r.code, 0, r.stderr);
  const svg = await readFile(path.join(ctx, 'dist', 'badges', 'why-wasm.svg'), 'utf8');
  assert.match(svg, /^<svg [^>]*aria-label="reading time: 1 min · \d+ words"/);
  assert.match(await readFile(path.join(ctx, 'dist', 'badges', 'built-with.svg'), 'utf8'), /sitefile \+ wasm/);
});
