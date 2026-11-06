#!/usr/bin/env node
// Incremental rebuilds: how long a `build --watch` rebuild takes after one edit,
// with the in-memory memo and incremental export (default) and without them
// (SITEFILE_NO_INCREMENTAL=1), in both implementations.
//
//   node bench/generate.mjs && node bench/incremental.mjs [--runs 3] [--only text] [--rust path]
//
// Each case copies a generated site to a temporary directory, starts
// `sitefile build --watch <copy> -o <tmp> --profile <trace>`, waits for the first
// build, then makes the same edit --runs times (alternating its content) and reads
// each rebuild's duration from the trace (the `command` event: the whole build and
// export, not the watcher's debounce). Prints the medians as a Markdown table and
// writes bench/results/incremental.json. The last rebuild's output is also checked
// against a clean build of the edited site: the same tree digest, and the exported
// directory the same file for file (paths, modes, bytes, no other entries).

import { spawn, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i === -1 ? dflt : argv[i + 1];
};
const RUNS = Number(opt('--runs', '3'));
const only = opt('--only');
const sites = path.join(root, 'bench', '.sites');
const IMPLS = {
  typescript: [process.execPath, path.join(root, 'lib', 'src', 'cli.js')],
  rust: [opt('--rust', path.join(root, 'rust', 'target', 'release', 'sitefile'))],
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** [site, edit name, edit(dir, run) → writes one file] */
const firstPost = async (dir) => path.join(dir, 'posts', (await fs.readdir(path.join(dir, 'posts'))).sort()[0]);
const CASES = [
  ['blog-1000', 'one post body', async (d, i) => fs.appendFile(await firstPost(d), `\nedit ${i}\n`)],
  ['blog-1000', 'index template', async (d, i) => fs.appendFile(path.join(d, 'templates', 'index.html'), `<!-- ${i} -->\n`)],
  ['blog-1000', 'a file no step reads', async (d, i) => fs.writeFile(path.join(d, 'NOTES.txt'), `note ${i}\n`)],
  ['blog-5000', 'one post body', async (d, i) => fs.appendFile(await firstPost(d), `\nedit ${i}\n`)],
  ['static-5000', 'one static file', async (d, i) => fs.appendFile(path.join(d, 'public', 'd0', 'f0.txt'), `edit ${i}\n`)],
  ['csv-2000', 'one CSV row', async (d, i) => {
    const f = path.join(d, 'people.csv');
    const lines = (await fs.readFile(f, 'utf8')).split('\n');
    lines[1] = lines[1].replace(/,\d+,/, `,${900 + i},`);
    await fs.writeFile(f, lines.join('\n'));
  }],
  ['archive-2000', 'a file no step reads', async (d, i) => fs.writeFile(path.join(d, 'NOTES.txt'), `note ${i}\n`)],
];

async function copyDir(from, to) {
  await fs.cp(from, to, { recursive: true });
}

async function trace(file) {
  try {
    return JSON.parse(await fs.readFile(file, 'utf8')).traceEvents;
  } catch {
    return undefined;
  }
}

const durOf = (events, cat) => (events.find((e) => e.cat === cat)?.dur ?? 0) / 1000;

/** Every entry under `dir`: type, path, mode and (files) content digest. */
async function listing(dir, rel = '') {
  const out = [];
  for (const e of (await fs.readdir(path.join(dir, rel), { withFileTypes: true })).sort((a, b) => (a.name < b.name ? -1 : 1))) {
    const p = `${rel}/${e.name}`;
    const st = await fs.lstat(path.join(dir, p));
    if (e.isDirectory()) out.push(`d ${p}`, ...(await listing(dir, p)));
    else out.push(`${e.isFile() ? 'f' : 'x'} ${p} ${(st.mode & 0o777).toString(8)} ${createHash('sha256').update(await fs.readFile(path.join(dir, p))).digest('hex')}`);
  }
  return out;
}

/** One watch session: the first build, then RUNS edits; returns rebuild times in ms. */
async function session(impl, site, edit, incremental, tmp) {
  const ctx = path.join(tmp, `${impl}-${incremental ? 'inc' : 'full'}-${site}`);
  await fs.rm(ctx, { recursive: true, force: true });
  await copyDir(path.join(sites, site), ctx);
  const out = `${ctx}.out`;
  const tr = `${ctx}.trace.json`;
  const env = { ...process.env, SITEFILE_HOME: path.join(tmp, 'home'), ...(incremental ? {} : { SITEFILE_NO_INCREMENTAL: '1' }) };
  const cmd = IMPLS[impl];
  const child = spawn(cmd[0], [...cmd.slice(1), 'build', '--watch', ctx, '-o', out, '--force', '--no-lockfile', '--profile', tr], { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let text = '';
  child.stdout.on('data', (c) => (text += c));
  child.stderr.on('data', (c) => (text += c));
  const exported = () => (text.match(/^Exported /gm) ?? []).length;
  const waitFor = async (pred, what) => {
    const end = Date.now() + 120000;
    while (!(await pred())) {
      if (Date.now() > end) throw new Error(`timed out: ${what}\n${text}`);
      await sleep(20);
    }
  };
  const times = [];
  const exports = [];
  try {
    await waitFor(() => /Watching /.test(text), 'first build');
    const first = await trace(tr);
    for (let i = 0; i < RUNS; i++) {
      const before = JSON.stringify(await trace(tr));
      const n = exported();
      await edit(ctx, i);
      await waitFor(() => exported() > n, 'rebuild');
      await waitFor(async () => {
        const t = await trace(tr);
        return t !== undefined && JSON.stringify(t) !== before;
      }, 'trace');
      const ev = await trace(tr);
      times.push(durOf(ev, 'command'));
      exports.push(durOf(ev, 'export'));
    }
    // The last rebuild must equal a clean build of the edited site.
    const clean = execFileSync(cmd[0], [...cmd.slice(1), 'build', ctx, '-o', `${ctx}.clean`, '--force', '--no-lockfile'], { env: { ...env, SITEFILE_NO_INCREMENTAL: '1' } }).toString();
    const digest = (s) => /\((sha256:[0-9a-f]+)\)/.exec(s)?.[1];
    const lastLine = text.trim().split('\n').filter((l) => l.startsWith('Exported ')).pop();
    if (digest(lastLine) !== digest(clean)) throw new Error(`${impl} ${site}: incremental output ${digest(lastLine)} != clean ${digest(clean)}`);
    if (JSON.stringify(await listing(out)) !== JSON.stringify(await listing(`${ctx}.clean`))) throw new Error(`${impl} ${site}: the exported directory differs from a clean export`);
    return { first: durOf(first, 'command'), times, exports };
  } finally {
    child.kill('SIGINT');
    await new Promise((r) => child.once('exit', r));
  }
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-incremental-'));
const results = [];
try {
  for (const [site, name, edit] of CASES) {
    if (only && !`${site} ${name}`.includes(only)) continue;
    for (const impl of Object.keys(IMPLS)) {
      const full = await session(impl, site, edit, false, tmp);
      const inc = await session(impl, site, edit, true, tmp);
      const row = {
        site,
        edit: name,
        impl,
        fullMs: median(full.times),
        incrementalMs: median(inc.times),
        speedup: median(full.times) / median(inc.times),
        // The build without the export (parse, imports, operations): what the memo can skip.
        fullBuildMs: median(full.times.map((t, i) => t - full.exports[i])),
        incrementalBuildMs: median(inc.times.map((t, i) => t - inc.exports[i])),
        fullExportMs: median(full.exports),
        incrementalExportMs: median(inc.exports),
      };
      row.exportSpeedup = row.fullExportMs / row.incrementalExportMs;
      row.buildSpeedup = row.fullBuildMs / row.incrementalBuildMs;
      results.push(row);
      console.error(
        `${site} / ${name} / ${impl}: ${row.fullMs.toFixed(0)} -> ${row.incrementalMs.toFixed(0)} ms (${row.speedup.toFixed(1)}x); build ${row.fullBuildMs.toFixed(0)} -> ${row.incrementalBuildMs.toFixed(0)} ms (${row.buildSpeedup.toFixed(1)}x); export ${row.fullExportMs.toFixed(0)} -> ${row.incrementalExportMs.toFixed(0)} ms (${row.exportSpeedup.toFixed(1)}x)`,
      );
    }
  }
} finally {
  await fs.rm(tmp, { recursive: true, force: true });
}

const f = (n) => `${n.toFixed(0)} ms`;
console.log('| Site | Edit | Impl | Build, full | Build, incremental | Speedup | Export, full | Export, incremental | Speedup | Rebuild, full | Rebuild, incremental | Speedup |');
console.log('| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |');
for (const r of results) {
  console.log(
    `| ${r.site} | ${r.edit} | ${r.impl} | ${f(r.fullBuildMs)} | ${f(r.incrementalBuildMs)} | ${r.buildSpeedup.toFixed(1)}× | ${f(r.fullExportMs)} | ${f(r.incrementalExportMs)} | ${r.exportSpeedup.toFixed(1)}× | ${f(r.fullMs)} | ${f(r.incrementalMs)} | ${r.speedup.toFixed(1)}× |`,
  );
}
await fs.mkdir(path.join(root, 'bench', 'results'), { recursive: true });
await fs.writeFile(path.join(root, 'bench', 'results', 'incremental.json'), JSON.stringify({ date: new Date().toISOString(), runs: RUNS, results }, null, 2) + '\n');
