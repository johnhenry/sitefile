#!/usr/bin/env node
// Compare the TypeScript and Rust CLIs.
//
//   node bench/run.mjs [--runs 5] [--sites dir] [--only name] [--rust path] [--out bench/results]
//
// Each scenario runs once to warm caches, then --runs times per implementation.
// Wall time is measured around the process (so it includes startup); peak RSS
// comes from `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux). Results
// go to <out>/latest.json and bench/REPORT.md.

import { spawn } from 'node:child_process';
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
const RUNS = Number(opt('--runs', '5'));
const sites = path.resolve(opt('--sites', path.join(root, 'bench', '.sites')));
const only = opt('--only');
const outDir = path.resolve(opt('--out', path.join(root, 'bench', 'results')));
const IMPLS = {
  typescript: [process.execPath, path.join(root, 'lib', 'src', 'cli.js')],
  rust: [opt('--rust', path.join(root, 'rust', 'target', 'release', 'sitefile'))],
};

const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-bench-'));
const out = (name, impl, file = 'dist') => path.join(tmp, impl, name.replace(/[^a-z0-9]+/gi, '_'), file);
const build = (ctx, file, extra = []) => (name, impl) => ['build', ctx, '-o', out(name, impl, file), '--force', '--no-lockfile', ...extra];

const ex = (n) => path.join(root, 'examples', n);
const s = (n) => path.join(sites, n);
const SCENARIOS = [
  { name: 'startup (--version)', args: () => ['--version'] },
  { name: 'validate hello', args: () => ['validate', ex('hello')] },
  { name: 'build hello', args: build(ex('hello')) },
  { name: 'build examples/blog', args: build(ex('blog')) },
  { name: 'build examples/data (yaml, csv, wbn)', args: build(ex('data')) },
  { name: 'build examples/wasm (wasm plugin)', args: build(ex('wasm'), 'dist', ['--no-cache']) },
  { name: 'blog: 100 posts', args: build(s('blog-100')) },
  { name: 'blog: 1000 posts', args: build(s('blog-1000')) },
  { name: 'blog: 5000 posts', args: build(s('blog-5000')) },
  { name: 'static copy: 5000 files', args: build(s('static-5000')) },
  { name: 'csv: 2000 rows → pages', args: build(s('csv-2000')) },
  { name: 'import tar.gz: 2000 files', args: build(s('archive-2000')) },
  { name: 'export zip: blog 1000', args: build(s('blog-1000'), 'site.zip') },
  { name: 'export tar.gz: blog 1000', args: build(s('blog-1000'), 'site.tar.gz') },
  { name: 'export wbn: blog 1000', args: build(s('blog-1000'), 'site.wbn', ['--base-url', 'https://bench.example/']) },
  { name: 'export oci: blog 1000', args: build(s('blog-1000'), 'site.oci.tar') },
  { name: 'export oci, uncompressed layer: blog 1000', args: build(s('blog-1000'), 'site.oci.tar', ['--oci-compression', 'none']) },
];

const timeFlag = process.platform === 'darwin' ? '-l' : '-v';
function once(cmd, args) {
  return new Promise((resolve) => {
    const start = process.hrtime.bigint();
    const child = spawn('/usr/bin/time', [timeFlag, ...cmd, ...args], { cwd: root, stdio: ['ignore', 'ignore', 'pipe'] });
    let err = '';
    child.stderr.on('data', (d) => (err += d));
    child.on('close', (code) => {
      const ms = Number(process.hrtime.bigint() - start) / 1e6;
      // macOS: "  123456  maximum resident set size" (bytes); Linux: "Maximum resident set size (kbytes): 1234"
      const mac = /(\d+)\s+maximum resident set size/.exec(err);
      const lin = /Maximum resident set size \(kbytes\):\s*(\d+)/.exec(err);
      const rss = mac ? Number(mac[1]) : lin ? Number(lin[1]) * 1024 : undefined;
      resolve({ ms, code, rss, stderr: err });
    });
  });
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

const available = {};
for (const [impl, cmd] of Object.entries(IMPLS)) {
  available[impl] = await fs.access(cmd[cmd.length - 1]).then(() => true, () => false);
}

const results = [];
for (const sc of SCENARIOS) {
  if (only && !sc.name.includes(only)) continue;
  const row = { name: sc.name };
  for (const [impl, cmd] of Object.entries(IMPLS)) {
    if (!available[impl]) continue;
    const args = sc.args(sc.name, impl);
    const warm = await once(cmd, args);
    if (warm.code !== 0) {
      row[impl] = { error: warm.stderr.split('\n').find((l) => /error/i.test(l)) ?? `exit ${warm.code}` };
      continue;
    }
    const runs = [];
    for (let i = 0; i < RUNS; i++) runs.push(await once(cmd, args));
    row[impl] = {
      medianMs: median(runs.map((r) => r.ms)),
      minMs: Math.min(...runs.map((r) => r.ms)),
      maxRssMb: Math.max(...runs.map((r) => r.rss ?? 0)) / 1024 / 1024,
    };
  }
  if (row.typescript?.medianMs && row.rust?.medianMs) row.speedup = row.typescript.medianMs / row.rust.medianMs;
  results.push(row);
  const f = (x) => (x?.medianMs !== undefined ? `${x.medianMs.toFixed(0).padStart(6)} ms ${x.maxRssMb.toFixed(0).padStart(4)} MB` : (x?.error ?? 'n/a').slice(0, 40));
  console.log(`${sc.name.padEnd(38)} ts ${f(row.typescript)}   rust ${f(row.rust)}${row.speedup ? `   ×${row.speedup.toFixed(1)}` : ''}`);
}
await fs.rm(tmp, { recursive: true, force: true });

const meta = {
  date: new Date().toISOString(),
  runs: RUNS,
  node: process.version,
  platform: `${process.platform} ${os.arch()}`,
  cpu: os.cpus()[0]?.model,
  cpus: os.cpus().length,
  memoryGb: Math.round(os.totalmem() / 2 ** 30),
};
await fs.mkdir(outDir, { recursive: true });
await fs.writeFile(path.join(outDir, 'latest.json'), JSON.stringify({ meta, results }, null, 2) + '\n');

const fmt = (x) => (x?.medianMs !== undefined ? `${x.medianMs.toFixed(0)} ms (min ${x.minMs.toFixed(0)}) · ${x.maxRssMb.toFixed(0)} MB` : x?.error ? `error` : 'n/a');
const lines = [
  '# Benchmarks: TypeScript vs Rust',
  '',
  `Generated by \`node bench/run.mjs\` on ${meta.date.slice(0, 10)}: ${meta.platform}, ${meta.cpu} (${meta.cpus} cores), ${meta.memoryGb} GB, Node ${meta.node}. ` +
    `Median of ${RUNS} runs after one warm-up; wall time includes process startup; memory is peak RSS.`,
  '',
  '| Scenario | TypeScript | Rust | Speed-up |',
  '| --- | --- | --- | --- |',
  ...results.map((r) => `| ${r.name} | ${fmt(r.typescript)} | ${fmt(r.rust)} | ${r.speedup ? `${r.speedup.toFixed(1)}×` : '–'} |`),
  '',
  'Inputs come from `node bench/generate.mjs` (deterministic). Reproduce with `npm run build`, `cargo build --release` in `rust/`, then `node bench/generate.mjs && node bench/run.mjs`.',
  '',
];
await fs.writeFile(path.join(root, 'bench', 'REPORT.md'), lines.join('\n'));
console.log(`\nwrote ${path.relative(root, path.join(outDir, 'latest.json'))} and bench/REPORT.md`);
