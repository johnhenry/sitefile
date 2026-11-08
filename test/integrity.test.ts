import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { cp, mkdtemp, readFile, rm, symlink, unlink, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { encodeManifest, manifestOf, merkleRoot, parseManifest, verifyTree, type ManifestFile } from '../src/integrity.js';
import { Tree, makeFile } from '../src/tree.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function sitefile(args: string[], cwd = root): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

async function tmp(): Promise<string> {
  const d = await mkdtemp(path.join(os.tmpdir(), 'sitefile-integrity-'));
  temps.push(d);
  return d;
}

const h = (...parts: (Uint8Array | string)[]): Buffer => {
  const x = createHash('sha256');
  for (const p of parts) x.update(p);
  return x.digest();
};

test('merkleRoot is the RFC 6962 tree hash of path\\0digest leaves', () => {
  const f = (p: string, d: string): ManifestFile => ({ path: p, size: 1, digest: d });
  const leaf = (s: string): Buffer => h(new Uint8Array([0]), s);
  assert.equal(merkleRoot([]), 'sha256:' + h().toString('hex'));
  const [a, b, c] = [f('/a', 'sha256:00'), f('/b', 'sha256:11'), f('/c', 'sha256:22')];
  const ab = h(new Uint8Array([1]), leaf('/a\0sha256:00'), leaf('/b\0sha256:11'));
  assert.equal(merkleRoot([a, b]), 'sha256:' + ab.toString('hex'));
  // Three leaves split 2 + 1.
  assert.equal(merkleRoot([a, b, c]), 'sha256:' + h(new Uint8Array([1]), ab, leaf('/c\0sha256:22')).toString('hex'));
});

test('manifests round-trip; a tampered manifest or malformed entry is refused', () => {
  const tree = Tree.fromFiles([
    ['/b.txt', makeFile(new TextEncoder().encode('b'), 't')],
    ['/a.txt', makeFile(new TextEncoder().encode('aa'), 't')],
  ]);
  const m = manifestOf(tree);
  assert.deepEqual(m.files.map((f) => f.path), ['/a.txt', '/b.txt']);
  assert.equal(m.files[0]?.size, 2);
  assert.deepEqual(parseManifest(encodeManifest(m), 'm'), m);
  const text = new TextDecoder().decode(encodeManifest(m));
  assert.throws(() => parseManifest(new TextEncoder().encode(text.replace('/a.txt', '/z.txt')), 'm'), /does not match its files/);
  assert.throws(() => parseManifest(new TextEncoder().encode('{"mediaType":"x"}'), 'm'), /not a Sitefile manifest/);
  assert.throws(() => parseManifest(new TextEncoder().encode('nope'), 'm'), /invalid JSON/);
  const dup = JSON.parse(text);
  dup.files.push(dup.files[0]);
  assert.throws(() => parseManifest(new TextEncoder().encode(JSON.stringify(dup)), 'm'), /twice/);
});

test('verifyTree reports missing, changed and extra files', () => {
  const enc = new TextEncoder();
  const before = Tree.fromFiles([
    ['/a', makeFile(enc.encode('a'), 't')],
    ['/b', makeFile(enc.encode('b'), 't')],
  ]);
  const after = Tree.fromFiles([
    ['/b', makeFile(enc.encode('B'), 't')],
    ['/c', makeFile(enc.encode('c'), 't')],
  ]);
  const r = verifyTree(after, manifestOf(before));
  assert.equal(r.ok, false);
  assert.deepEqual(r.missing, ['/a']);
  assert.deepEqual(r.changed.map((c) => c.path), ['/b']);
  assert.deepEqual(r.extra, ['/c']);
  assert.equal(verifyTree(after, manifestOf(before), '/c').extra.length, 0);
  assert.equal(verifyTree(before, manifestOf(before)).ok, true);
});

test('build --manifest and verify catch every change kind in a directory, an archive and an image', async () => {
  const work = await tmp();
  const ctx = path.join(work, 'hello');
  await cp(path.join(root, 'examples', 'hello'), ctx, { recursive: true });
  const out = path.join(work, 'site');
  const m = path.join(work, 'm.json');
  const b = await sitefile(['build', ctx, '-o', out, '--manifest', m, '--no-lockfile', '-t', 'verify-me:1']);
  assert.equal(b.code, 0, b.stderr);
  const manifest = parseManifest(new Uint8Array(await readFile(m)), m);
  assert.deepEqual(manifest.files.map((f) => f.path), ['/index.html', '/style.css']);

  const ok = await sitefile(['verify', out, '--manifest', m]);
  assert.equal(ok.code, 0, ok.stderr);
  assert.equal(ok.stdout, `OK: 2 files match ${manifest.root}\n`);

  // A manifest kept inside the verified directory is not an extra file.
  await cp(m, path.join(out, 'manifest.json'));
  assert.equal((await sitefile(['verify', out, '--manifest', path.join(out, 'manifest.json')])).code, 0);
  await unlink(path.join(out, 'manifest.json'));

  await writeFile(path.join(out, 'index.html'), 'changed');
  await unlink(path.join(out, 'style.css'));
  await writeFile(path.join(out, '.hidden'), 'x');
  const bad = await sitefile(['verify', out, '--manifest', m]);
  assert.equal(bad.code, 1);
  assert.equal(bad.stdout, 'extra    /.hidden\nchanged  /index.html\nmissing  /style.css\n');
  assert.match(bad.stderr, /error\[E_INTEGRITY\]: .* does not match the manifest: 1 missing, 1 changed, 1 extra/);
  const jsonRun = await sitefile(['verify', out, '--manifest', m, '--json']);
  // One JSON document on stdout; the error line goes to stderr only.
  assert.equal(jsonRun.code, 1);
  assert.match(jsonRun.stderr, /error\[E_INTEGRITY\]: .* does not match the manifest/);
  const json = JSON.parse(jsonRun.stdout);
  assert.equal(json.ok, false);
  assert.deepEqual(json.changed[0].expected, { size: manifest.files[0]?.size, digest: manifest.files[0]?.digest });

  // A symlink out of the directory is refused, not followed.
  await writeFile(path.join(work, 'outside.txt'), 'secret');
  await symlink(path.join(work, 'outside.txt'), path.join(out, 'link.txt'));
  const esc = await sitefile(['verify', out, '--manifest', m]);
  assert.equal(esc.code, 1);
  assert.match(esc.stderr, /error\[E_PATH_ESCAPE\]/);

  const zip = path.join(work, 'site.zip');
  const mz = path.join(work, 'mz.json');
  assert.equal((await sitefile(['build', ctx, '-o', zip, '--manifest', mz, '--no-lockfile'])).code, 0);
  assert.deepEqual(await readFile(mz), await readFile(m), 'the manifest does not depend on the format');
  assert.equal((await sitefile(['verify', zip, '--manifest', m])).code, 0);

  // Images: against a manifest, or alone (every stored blob is checked).
  assert.equal((await sitefile(['verify', 'verify-me:1', '--manifest', m])).code, 0);
  assert.equal((await sitefile(['verify', 'verify-me:1'])).code, 0);
  const sm = path.join(work, 'saved.json');
  assert.equal((await sitefile(['save', 'verify-me:1', '-o', path.join(work, 'saved'), '--manifest', sm])).code, 0);
  assert.deepEqual(await readFile(sm), await readFile(m));
});

test('verify and --manifest usage errors', async () => {
  const work = await tmp();
  const r = await sitefile(['verify', work]);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /verify needs --manifest <file> for a directory or an archive/);
  assert.equal((await sitefile(['verify', './no-such-dir', '--manifest', 'x'])).code, 1);
  const inside = await sitefile(['build', path.join(root, 'examples', 'hello'), '-o', path.join(work, 'd'), '--manifest', path.join(work, 'd', 'm.json'), '--no-lockfile']);
  assert.equal(inside.code, 2);
  assert.match(inside.stderr, /is inside the exported directory/);
  assert.equal((await sitefile(['validate', path.join(root, 'examples', 'hello'), '--manifest', 'x'])).code, 2);
  await writeFile(path.join(work, 'bad.json'), '{}');
  const bad = await sitefile(['verify', work, '--manifest', path.join(work, 'bad.json')]);
  assert.equal(bad.code, 1);
  assert.match(bad.stderr, /error\[E_INTEGRITY\]: .*not a Sitefile manifest/);
});
