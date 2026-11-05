#!/usr/bin/env node
// Conformance: run the TypeScript and Rust CLIs on the same inputs and require
// the same result.
//
//   node conformance/run.mjs [--rust path/to/sitefile] [--only name] [--json out.json]
//
// For each case: same exit code; for failures the same `error[CODE]: …` line;
// for successes an identical output tree (paths, modes, file digests). Archive
// outputs are compared by content (unpacked with the TypeScript reader), since
// the two deflate implementations may compress differently; uncompressed
// formats (wbn) must match byte for byte.
//
// Needs `npm run build` (for lib/) and `cargo build --release` in rust/.

import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i === -1 ? undefined : args[i + 1];
};
const TS = [process.execPath, path.join(root, 'lib', 'src', 'cli.js')];
const RUST = [flag('--rust') ?? path.join(root, 'rust', 'target', 'release', 'sitefile')];
const only = flag('--only');

const { extractArchive } = await import(pathToFileURL(path.join(root, 'lib', 'src', 'resources', 'archive.js')).href);

/** name → { context, args, env, out: file name or directory, compare: 'tree' | 'archive' | 'bytes' | 'oci' } */
function cases() {
  const ex = (name, extra = {}) => ({ name: `example/${name}${extra.suffix ?? ''}`, context: path.join(root, 'examples', name), args: [], ...extra });
  const list = [
    ex('hello'),
    ex('hello', { suffix: ':build-arg', args: ['--build-arg', 'TITLE=Tom & <Jerry>'] }),
    ex('inline'),
    ex('multistage'),
    ex('blog'),
    ex('data'),
    ex('motion'),
    ex('wasm'),
    ex('hello', { suffix: ':zip', out: 'site.zip', compare: 'archive' }),
    ex('hello', { suffix: ':tar', out: 'site.tar', compare: 'archive' }),
    ex('hello', { suffix: ':tgz', out: 'site.tar.gz', compare: 'archive' }),
    ex('blog', { suffix: ':wbn', out: 'site.wbn', args: ['--base-url', 'https://blog.example/'], compare: 'bytes' }),
    ex('blog', { suffix: ':tar-bytes', out: 'site.tar', compare: 'bytes' }),
  ];
  return list;
}

async function caseDirs() {
  const dir = path.join(root, 'conformance', 'cases');
  const out = [];
  for (const name of (await fs.readdir(dir)).sort()) {
    const context = path.join(dir, name);
    let extra = {};
    try {
      extra = JSON.parse(await fs.readFile(path.join(context, 'case.json'), 'utf8'));
    } catch {}
    out.push({ name: `case/${name}`, context, args: [], ...extra });
  }
  return out;
}

function run(cmd, argv, env) {
  return new Promise((resolve) => {
    const start = process.hrtime.bigint();
    execFile(cmd[0], [...cmd.slice(1), ...argv], { cwd: root, env: { ...process.env, ...env }, maxBuffer: 64 << 20 }, (err, stdout, stderr) => {
      const ms = Number(process.hrtime.bigint() - start) / 1e6;
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr, ms, spawnError: err && typeof err.code === 'string' ? err.message : undefined });
    });
  });
}

/** Tree digest of a directory on disk, identical to Tree.digest() in both implementations. */
async function dirDigest(dir) {
  const files = [];
  async function walk(d, rel) {
    for (const e of (await fs.readdir(d, { withFileTypes: true })).sort((a, b) => (a.name < b.name ? -1 : 1))) {
      const p = path.join(d, e.name);
      const r = `${rel}/${e.name}`;
      if (e.isDirectory()) await walk(p, r);
      else {
        const st = await fs.stat(p);
        const bytes = await fs.readFile(p);
        files.push([r, st.mode & 0o111 ? '755' : '644', 'sha256:' + createHash('sha256').update(bytes).digest('hex')]);
      }
    }
  }
  await walk(dir, '');
  files.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return { digest: 'sha256:' + createHash('sha256').update(files.map((f) => `${f[0]}\0${f[1]}\0${f[2]}\n`).join('')).digest('hex'), files: files.length };
}

async function outputOf(kind, outPath) {
  if (kind === 'tree') return dirDigest(outPath);
  const bytes = await fs.readFile(outPath);
  if (kind === 'bytes') return { digest: 'sha256:' + createHash('sha256').update(bytes).digest('hex'), files: 1 };
  const tree = await extractArchive(new Uint8Array(bytes), 'conformance');
  return { digest: tree.digest(), files: tree.size };
}

const errorLine = (stderr) => stderr.split('\n').find((l) => l.startsWith('error')) ?? '';

const results = [];
const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-conformance-'));
try {
  for (const c of [...cases(), ...(await caseDirs())]) {
    if (only && !c.name.includes(only)) continue;
    const compare = c.compare ?? 'tree';
    const r = {};
    for (const [impl, cmd] of [['ts', TS], ['rust', RUST]]) {
      const out = path.join(tmp, impl, c.name.replace(/[/:]/g, '_'), c.out ?? 'dist');
      await fs.mkdir(path.dirname(out), { recursive: true });
      const res = await run(cmd, ['build', c.context, '-o', out, '--force', '--no-lockfile', ...(c.args ?? [])], c.env ?? {});
      r[impl] = { code: res.code, error: errorLine(res.stderr), ms: res.ms, spawnError: res.spawnError };
      if (res.code === 0) r[impl].output = await outputOf(compare, out).catch((e) => ({ digest: `unreadable: ${e.message}` }));
    }
    const same =
      !r.rust.spawnError &&
      r.ts.code === r.rust.code &&
      (r.ts.code === 0 ? r.ts.output?.digest === r.rust.output?.digest : r.ts.error === r.rust.error);
    results.push({ name: c.name, compare, pass: same, ...r });
    const tag = same ? 'PASS' : 'FAIL';
    console.log(`${tag}  ${c.name}`);
    if (!same) {
      for (const impl of ['ts', 'rust']) {
        const x = r[impl];
        console.log(`        ${impl.padEnd(4)} exit ${x.code} ${x.spawnError ?? (x.code === 0 ? `${x.output?.files} files ${x.output?.digest}` : x.error)}`);
      }
    }
  }
} finally {
  await fs.rm(tmp, { recursive: true, force: true });
}

const passed = results.filter((r) => r.pass).length;
console.log(`\n${passed}/${results.length} cases match`);
const jsonOut = flag('--json');
if (jsonOut) await fs.writeFile(jsonOut, JSON.stringify({ date: new Date().toISOString(), passed, total: results.length, results }, null, 2) + '\n');
process.exitCode = passed === results.length ? 0 : 1;
