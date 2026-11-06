// Issue #71: case-only path collisions and Unicode normalization of paths.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readdir, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { occupied } from '../src/exporter.js';
import { buildOciImage } from '../src/oci.js';
import { foldCase, nfc, normalizeLocal, normalizeVirtual } from '../src/paths.js';
import { LocalContext } from '../src/resources/local.js';
import { safeSegments } from '../src/server.js';
import { makeFile, Tree } from '../src/tree.js';
import { tar, zip } from './archive-fixtures.js';
import { createHash } from 'node:crypto';
import { extractArchive } from '../src/resources/archive.js';

const NFC = 'café'; // café, precomposed
const NFD = 'café'; // café, e + combining acute

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-paths-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}

const run = async (files: Record<string, string>) => build({ contextDir: await project(files), lockfile: false });
const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

function isCode(code: ErrorCode, re?: RegExp, line?: number) {
  return (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    if (re) assert.match(e.message, re);
    if (line !== undefined) assert.equal(e.span?.line, line, e.message);
    return true;
  };
}

const f = (s: string) => makeFile(new TextEncoder().encode(s), `Sitefile:1 COPY ${s}`);

test('foldCase is Unicode simple case folding', () => {
  assert.equal(foldCase('/Docs/A.HTML'), '/docs/a.html');
  // Final sigma, capital sigma and sigma share a fold (toLowerCase keeps ς).
  assert.equal(foldCase('Σ'), foldCase('ς'));
  assert.equal(foldCase('σ'), foldCase('ς'));
  assert.equal(foldCase('K'), 'k'); // KELVIN SIGN
  assert.equal(foldCase('ſ'), 's'); // LATIN SMALL LETTER LONG S
  assert.equal(foldCase('ẞ'), 'ß'); // simple folding: not 'ss'
  assert.notEqual(foldCase('ß'), foldCase('ss'));
  assert.equal(foldCase('İ'), 'İ'); // İ: only full/Turkic mappings
  assert.equal(foldCase('Ꭰ'), 'Ꭰ'); // Cherokee folds to upper case
  assert.equal(foldCase('ꭰ'), 'Ꭰ');
  assert.equal(foldCase('\u{10400}'), '\u{10428}'); // Deseret (astral)
  assert.equal(foldCase('Ǆ'), 'ǆ');
  assert.equal(foldCase('ǅ'), 'ǆ');
});

// The same digests are asserted in rust/src/paths.rs (nfc_matches_typescript,
// fold_matches_typescript), so both implementations normalize and fold alike.
test('NFC and case folding agree with the Rust implementation', () => {
  // Combining marks icu_normalizer_data assigns a class and Node's ICU does not yet
  // (docs/RUST.md, "Known differences"): left out, as in the Rust test.
  const unsettled = (c: number) => (c >= 0x5c8 && c <= 0x5c9) || (c >= 0x1ade && c <= 0x1af0) || (c >= 0xd800 && c <= 0xdfff);
  const h = createHash('sha256');
  const add = (s: string) => h.update(nfc(s) + '\n');
  for (let c = 0; c <= 0x10ffff; c++) if (!unsettled(c)) add(String.fromCodePoint(c));
  for (let b = 0; b < 0x2000; b++) {
    if (unsettled(b)) continue;
    for (let m = 0x300; m <= 0x36f; m++) add(String.fromCodePoint(b, m));
  }
  for (let l = 0x1100; l < 0x1113; l++) {
    for (let v = 0x1161; v < 0x1176; v++) {
      add(String.fromCodePoint(l, v));
      add(String.fromCodePoint(l, v, 0x11a8));
    }
  }
  assert.equal('sha256:' + h.digest('hex'), 'sha256:fe17995393e0d09199fd78c8aedca5d57273bdde642335534485565355b364a1');
  const g = createHash('sha256');
  for (let c = 0; c <= 0x10ffff; c++) if (c < 0xd800 || c > 0xdfff) g.update(foldCase(String.fromCodePoint(c)) + '\n');
  assert.equal('sha256:' + g.digest('hex'), 'sha256:e6e9bc02adedd2b908bbc65d7996e3a36e68294e4dde23c0f04f7aa240e69659');
});

test('virtual and local paths are NFC', () => {
  assert.equal(nfc(NFD), NFC);
  assert.equal(normalizeVirtual(`/${NFD}/x`).path, `/${NFC}/x`);
  assert.equal(normalizeLocal(`./${NFD}.txt`).path, `${NFC}.txt`);
  assert.deepEqual(safeSegments(`/${encodeURIComponent(NFD)}`), [NFC]);
});

test('a tree refuses a path that differs only in case, at the second write', () => {
  const t = Tree.fromFiles([['/A.html', f('A')]]);
  assert.throws(
    () => t.withFiles([['/a.html', f('b')]], 'error'),
    isCode('E_PATH_COLLISION', /^'\/a\.html' differs only in case from '\/A\.html' \(written by Sitefile:1 COPY A\)/),
  );
  // --overwrite is about the same path; a differently cased one is still refused.
  assert.throws(() => t.withFiles([['/a.html', f('b')]], 'overwrite'), isCode('E_PATH_COLLISION'));
  // Identical content does not make it acceptable either: they are two paths.
  assert.throws(() => t.withFiles([['/a.html', f('A')]], 'error'), isCode('E_PATH_COLLISION'));
  // The same spelling is the usual same-path logic.
  assert.equal(t.withFiles([['/A.html', f('A')]], 'error').size, 1);
  assert.throws(() => t.withFiles([['/A.html', f('B')]], 'error'), isCode('E_CONFLICT'));
});

test('directories collide by case too, in both directions', () => {
  const t = Tree.fromFiles([['/Docs/x', f('x')]]);
  assert.throws(
    () => t.withFiles([['/docs/y', f('y')]], 'error'),
    isCode('E_PATH_COLLISION', /cannot write '\/docs\/y': '\/docs' differs only in case from '\/Docs' \(created by Sitefile:1 COPY x\)/),
  );
  assert.throws(() => t.withFiles([['/docs', f('y')]], 'overwrite'), isCode('E_PATH_COLLISION', /'\/docs' differs only in case from '\/Docs'/));
  assert.throws(() => t.withFiles([['/DOCS/x/z', f('y')]], 'error'), isCode('E_PATH_COLLISION'));
  const u = Tree.fromFiles([['/readme', f('r')]]);
  assert.throws(() => u.withFiles([['/README/a', f('a')]], 'error'), isCode('E_PATH_COLLISION', /'\/README' differs only in case from '\/readme'/));
  // Unrelated names under a shared directory are fine.
  assert.equal(t.withFiles([['/Docs/y', f('y')]], 'error').size, 2);
  // Folding is Unicode-aware.
  const s = Tree.fromFiles([['/Σ.html', f('s')]]);
  assert.throws(() => s.withFiles([['/ς.html', f('s')]], 'error'), isCode('E_PATH_COLLISION'));
  // subtree and fromFiles keep the check.
  assert.throws(() => Tree.fromFiles([['/a', f('1')], ['/A', f('2')]]), isCode('E_PATH_COLLISION'));
});

test('NFC and NFD spellings are one tree path with one digest and image ID', () => {
  const a = Tree.fromFiles([[`/${NFC}/index.html`, f('x')]]);
  const b = Tree.fromFiles([[`/${NFD}/index.html`, f('x')]]);
  assert.deepEqual(a.entries().map(([p]) => p), [`/${NFC}/index.html`]);
  assert.deepEqual(b.entries().map(([p]) => p), [`/${NFC}/index.html`]);
  assert.equal(a.digest(), b.digest());
  assert.equal(buildOciImage(a).manifestDigest, buildOciImage(b).manifestDigest);
  // Writing the other spelling is the same path: identical content is a no-op.
  assert.equal(a.withFiles([[`/${NFD}/index.html`, f('x')]], 'error').size, 1);
  assert.throws(() => a.withFiles([[`/${NFD}/index.html`, f('y')]], 'error'), isCode('E_CONFLICT'));
  // Normalization and case together: 'CAFÉ' (NFD) collides with 'café' (NFC).
  assert.throws(() => a.withFiles([[`/CAFÉ/index.html`, f('x')]], 'error'), isCode('E_PATH_COLLISION'));
});

test('build: the issue #71 reproduction fails at the second COPY with both paths', async () => {
  await assert.rejects(
    run({ 'src/A.html': 'A', 'src/a2.html': 'b', Sitefile: 'FROM scratch\nCOPY ./src/A.html /A.html\nCOPY ./src/a2.html /a.html\nEXPORT / AS directory\n' }),
    isCode('E_PATH_COLLISION', /Sitefile:3:1: '\/a\.html' differs only in case from '\/A\.html' \(written by \S*Sitefile:2 COPY\)/, 3),
  );
  await assert.rejects(
    run({ x: 'x', y: 'y', Sitefile: 'FROM scratch\nCOPY ./x /Docs/x\nCOPY --overwrite ./y /docs/y\nEXPORT /\n' }),
    isCode('E_PATH_COLLISION', /'\/docs' differs only in case from '\/Docs'/, 3),
  );
  // NFD in the Sitefile: same output as NFC.
  const a = await run({ x: 'x', Sitefile: `FROM scratch\nCOPY ./x /${NFC}.html\nEXPORT /\n` });
  const b = await run({ x: 'x', Sitefile: `FROM scratch\nCOPY ./x /${NFD}.html\nEXPORT /\n` });
  assert.deepEqual(b.output.entries().map(([p]) => p), [`/${NFC}.html`]);
  assert.equal(a.output.digest(), b.output.digest());
  await assert.rejects(
    run({ x: 'x', y: 'y', Sitefile: `FROM scratch\nCOPY ./x /${NFC}.html\nCOPY ./y /CAFÉ.html\nEXPORT /\n` }),
    isCode('E_PATH_COLLISION', /'\/CAFÉ\.html' differs only in case from '\/café\.html'/, 3),
  );
});

test('archive entries are NFC; case-only duplicates collide', async () => {
  const t = await extractArchive(tar([{ name: `${NFD}.txt`, data: '1' }]), 'a.tar');
  assert.deepEqual(t.entries().map(([p]) => p), [`/${NFC}.txt`]);
  await assert.rejects(extractArchive(zip([{ name: `${NFC}.txt`, data: '1' }, { name: `${NFD}.txt`, data: '2' }]), 'a.zip'), isCode('E_CONFLICT', /duplicate/));
  await assert.rejects(extractArchive(tar([{ name: 'A.txt', data: '1' }, { name: 'a.txt', data: '2' }]), 'a.tar'), isCode('E_PATH_COLLISION'));
});

/** True when this file system can hold `a` and `b` as two names in one directory. */
async function holdsBoth(dir: string, a: string, b: string): Promise<boolean> {
  const probe = await mkdtemp(path.join(dir, 'probe-'));
  try {
    await writeFile(path.join(probe, a), '1');
    await writeFile(path.join(probe, b), '2', { flag: 'wx' });
    return (await readdir(probe)).length === 2;
  } catch {
    return false;
  } finally {
    await rm(probe, { recursive: true, force: true });
  }
}

test('local files are found whichever normalization the disk and the Sitefile use', async () => {
  // On macOS the file system finds either spelling; on Linux LocalContext matches NFC forms.
  for (const [onDisk, named] of [[NFD, NFC], [NFC, NFD], [NFD, NFD]] as const) {
    const dir = await project({ [`${onDisk}/${onDisk}.txt`]: 'hi' });
    const c = await LocalContext.open(dir);
    const r = await c.readFile(normalizeLocal(`${named}/${named}.txt`).path);
    assert.equal(text(r.bytes), 'hi');
    assert.equal(r.basename, `${NFC}.txt`);
    const t = await c.readTree('', 'test');
    assert.deepEqual(t.tree.entries().map(([p]) => p), [`/${NFC}/${NFC}.txt`]);
  }
});

test('build copies an NFD-named context file to an NFC path', async () => {
  const dir = await project({ [`${NFD}.txt`]: 'hi', Sitefile: `FROM scratch\nCOPY ./${NFC}.txt /a/\nCOPY . /all/\nEXPORT /\n` });
  const r = await build({ contextDir: dir, lockfile: false });
  assert.ok(r.output.get(`/a/${NFC}.txt`));
  assert.ok(r.output.get(`/all/${NFC}.txt`));
  assert.ok(!r.output.entries().some(([p]) => p.includes('́')));
});

test('two context names that differ only in normalization are a collision', async () => {
  const dir = await project({});
  if (!(await holdsBoth(dir, NFC, NFD))) return; // macOS: the file system refuses the second name
  await writeFile(path.join(dir, NFC), '1');
  await writeFile(path.join(dir, NFD), '2');
  const c = await LocalContext.open(dir);
  await assert.rejects(c.readTree('', 'test'), isCode('E_PATH_COLLISION', /differ only in Unicode normalization/));
  // Naming one of them explicitly is ambiguous too, in either spelling.
  await assert.rejects(c.readFile(NFC), isCode('E_PATH_COLLISION'));
  await assert.rejects(c.readFile(normalizeLocal(NFD).path), isCode('E_PATH_COLLISION'));
});

test('two context names that differ only in case collide in the tree', async () => {
  const dir = await project({});
  if (!(await holdsBoth(dir, 'A.txt', 'a.txt'))) return; // case-insensitive file system
  await mkdir(path.join(dir, 'src'));
  await writeFile(path.join(dir, 'src', 'A.txt'), '1');
  await writeFile(path.join(dir, 'src', 'a.txt'), '2');
  await writeFile(path.join(dir, 'Sitefile'), 'FROM scratch\nCOPY ./src/ /x/\nEXPORT /\n');
  await assert.rejects(build({ contextDir: dir, lockfile: false }), isCode('E_PATH_COLLISION', /'\/a\.txt' differs only in case from '\/A\.txt'/, 2));
});

test('the directory exporter reports an occupied name by artifact path, not temp path', () => {
  const e = occupied(Object.assign(new Error("EEXIST: file already exists, open '/out/.dist.sitefile-tmp-ab/a.html'"), { code: 'EEXIST' }), '/a.html');
  assert.ok(isSitefileError(e, 'E_EXPORT'));
  assert.match(e.message, /^cannot write '\/a\.html': the destination file system already has an entry by that name/);
  assert.doesNotMatch(e.message, /sitefile-tmp/);
  const other = new Error('boom');
  assert.equal(occupied(other, '/a'), other);
});
