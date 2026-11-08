#!/usr/bin/env node
// Conformance: run the TypeScript and Rust CLIs on the same inputs and require
// the same result.
//
//   node conformance/run.mjs [--rust path/to/sitefile] [--only name] [--json out.json]
//
// For each case: same exit code; for failures the same `error[CODE]: …` line;
// for successes an identical output tree (paths, modes, file digests).
// 'archive' compares an archive's content (unpacked with the TypeScript
// reader); 'bytes' compares the file itself. Both implementations share one
// deterministic deflate encoder (src/deflate.ts, rust/src/deflate.rs), so
// compressed formats (zip, tar.gz, OCI layers) are checked byte for byte too.
//
// OCI outputs (compare: 'oci') are compared by manifest digest (the image ID)
// and config digest (diff_ids, labels, platform), plus the digest and
// artifactType of every other index.json entry (attestations: --provenance,
// --sbom), which are byte-identical in-toto statements in both implementations.
//
// A case directory may hold case.json: { args, env, out, compare, command, operands,
// links, files, copy, expectPaths }.
// `command` ("validate", "inspect", "graph" or "why") runs that command instead of
// build and compares exit code and stdout; `operands` go between the command and
// the context (`why <path> <context>`). `links` lists [existing, new] hard links to
// create before the case runs and remove after it. `files` maps context paths
// to contents, created before the case and removed after it (with the
// top-level directory each one creates), for names that should not be
// committed as written (NFD file names: write them with JSON \u escapes).
// `copy` maps context paths to repository paths copied there the same way, so a
// case can use a binary that is committed once (plugins/image/image.wasm).
// `expectPaths` lists output paths (exact code points, as read back from the
// directory listing) that both implementations' tree output must contain.
// `command: "bake"` copies the case directory once per implementation and runs
// `sitefile bake <args>` there (so the default sitefile.bake.yaml and relative paths
// apply); it compares exit code, the whole stdout and stderr, and the tree of `out`
// (default `dist`) in the copy when it exists.
// `command: "serve"` starts `sitefile serve` on a directory of the case with both
// CLIs and compares their answers to a list of raw HTTP requests (format in
// conformance/servers.mjs, runServeCase).
// `profile: true` adds `--profile <file>` and requires the same trace from both,
// ignoring each event's `ts` and `dur` (names, categories, threads, args and order
// must match; also when the build fails).
//
// Step cases: a case.json with "steps" (and no "network") runs a list of CLI steps like a
// network case (format in conformance/network.mjs), offline: verify, diff, save chains.
//
// Network cases (case.json "network": true, conformance/cases/net-*; format in
// conformance/network.mjs) run a list of CLI steps (build, push, pull, save)
// against local servers started on the first such case (conformance/servers.mjs):
// an HTTPS server with a throwaway CA trusted through NODE_EXTRA_CA_CERTS, and
// an in-process mock OCI registry. They compare every step's exit code and
// error line, output trees/OCI configs and Sitefile.lock byte for byte. Each
// implementation gets its own context copy and SITEFILE_HOME/HOME/DOCKER_CONFIG.
// They need `openssl` on PATH.
//
// The run prints how many offline and network cases there are.
//
// Needs `npm run build` (for lib/) and `cargo build --release` in rust/.

import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { runNetworkCase } from './network.mjs';
import { runServeCase, startServers } from './servers.mjs';

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

// examples/fileable's plugin reads only its own folder, so @johnhenry/fileable (a devDependency) is vendored into it.
await new Promise((ok, fail) =>
  execFile(process.execPath, [path.join(root, 'examples', 'fileable', 'vendor.mjs')], (e, _out, err) => (e ? fail(new Error(`examples/fileable/vendor.mjs: ${err || e.message}`)) : ok())),
);

/** name → { context, args, env, out: file name or directory, compare: 'tree' | 'archive' | 'bytes' | 'oci' } */
function cases() {
  const ex = (name, extra = {}) => ({ name: `example/${name}${extra.suffix ?? ''}`, context: path.join(root, 'examples', name), args: [], ...extra });
  const list = [
    ex('hello'),
    ex('hello', { suffix: ':build-arg', args: ['--build-arg', 'TITLE=Tom & <Jerry>'] }),
    ex('inline'),
    ex('multistage'),
    // IMPORT … AS sitefile (issue #97): a site on the stages of ./theme/Sitefile, and the nested case's commands.
    ex('shared-stages'),
    ex('shared-stages', { suffix: ':build-arg', args: ['--build-arg', 'ACCENT=teal'] }),
    ex('shared-stages', { suffix: ':inspect', command: 'inspect' }),
    ex('shared-stages', { suffix: ':validate-json', command: 'validate', args: ['--json'] }),
    ex('shared-stages', { suffix: ':graph-json', command: 'graph', args: ['--json'] }),
    ex('shared-stages', { suffix: ':why', command: 'why', operands: ['/assets/theme.css'] }),
    ...['inspect', 'why'].map((command) => ({
      name: `case/sitefile-import:${command}`,
      context: path.join(root, 'conformance', 'cases', 'sitefile-import'),
      command,
      operands: command === 'why' ? ['/who.txt'] : [],
      args: ['--json'],
    })),
    // FINGERPRINT inside an imported Sitefile: why follows the rename into it.
    {
      name: 'case/sitefile-import-fingerprint:why',
      context: path.join(root, 'conformance', 'cases', 'sitefile-import-fingerprint'),
      command: 'why',
      operands: ['/assets/site.0a8af8e447.css'],
      args: ['--json'],
    },
    ex('blog'),
    ex('data'),
    ex('motion', { args: ['--allow-plugin', 'motion'] }),
    // fileable vendored into the plugin folder (above, before the cases run); both CLIs run the plugin with node.
    ex('fileable', { args: ['--allow-plugin', 'fileable'] }),
    ex('fileable', { suffix: ':cmd', args: ['--allow-plugin', 'fileable', '--build-arg', 'TEMPLATE=cmd.mjs'] }),
    ex('wasm'),
    ex('image'),
    ex('hello', { suffix: ':zip', out: 'site.zip', compare: 'archive' }),
    ex('hello', { suffix: ':tar', out: 'site.tar', compare: 'archive' }),
    ex('hello', { suffix: ':tgz', out: 'site.tar.gz', compare: 'archive' }),
    ex('hello', { suffix: ':zip-bytes', out: 'site.zip', compare: 'bytes' }),
    ex('hello', { suffix: ':tgz-bytes', out: 'site.tar.gz', compare: 'bytes' }),
    ex('blog', { suffix: ':tgz-bytes', out: 'site.tar.gz', compare: 'bytes' }),
    ex('hello', { suffix: ':oci-id', out: 'site.oci.tar', compare: 'oci' }),
    ex('blog', { suffix: ':oci-bytes', out: 'site.oci.tar', compare: 'bytes' }),
    ex('blog', { suffix: ':oci-uncompressed-bytes', out: 'site.oci.tar', args: ['--oci-compression', 'none'], compare: 'bytes' }),
    ex('blog', { suffix: ':wbn', out: 'site.wbn', args: ['--base-url', 'https://blog.example/'], compare: 'bytes' }),
    ex('blog', { suffix: ':wbn-gzip', out: 'site.wbn.gz', args: ['--base-url', 'https://blog.example/'], compare: 'bytes' }),
    ex('blog', { suffix: ':tar-bytes', out: 'site.tar', compare: 'bytes' }),
    ex('blog', { suffix: ':car-bytes', out: 'site.car', compare: 'bytes' }),
    ex('data', { suffix: ':car-format', out: 'site-car', args: ['--format', 'car'], compare: 'bytes' }),
    // Attestations (--provenance, --sbom): OCI 1.1 referrers of an unchanged image.
    ex('hello', { suffix: ':oci-attest-bytes', out: 'site.oci.tar', args: ['--provenance', '--sbom', '--build-arg', 'TITLE=Attested', '--build-arg', 'API_TOKEN=hunter2'], compare: 'bytes' }),
    ex('blog', { suffix: ':oci-attest-id', out: 'site.oci.tar', args: ['--sbom', '--provenance'], compare: 'oci' }),
    ex('blog', { suffix: ':oci-attest-epoch-bytes', out: 'site.oci.tar', args: ['--provenance', '--sbom'], env: { SOURCE_DATE_EPOCH: '1700000000' }, compare: 'bytes' }),
    ex('data', { suffix: ':oci-attest-layout', out: 'site-oci', args: ['--format', 'oci', '--provenance', '--sbom', '--oci-root', '/usr/share/nginx/html'], compare: 'tree' }),
    ex('blog', { suffix: ':profile', profile: true }),
    ex('multistage', { suffix: ':profile', profile: true }),
    ex('wasm', { suffix: ':profile', profile: true }),
    ex('data', { suffix: ':profile-zip', out: 'site.zip', compare: 'archive', profile: true }),
    // sitefile bake: four variants of one Sitefile (matrix, inherits, groups); --print shows the plan.
    ex('bake', { command: 'bake' }),
    ex('bake', { suffix: ':en', command: 'bake', args: ['en', '-j', '1'] }),
    ex('bake', { suffix: ':print', command: 'bake', args: ['--print'] }),
    ex('bake', { suffix: ':print-json', command: 'bake', args: ['--print', '--json', 'site-fr-production', 'en'] }),
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

function run(cmd, argv, env, cwd = root) {
  return new Promise((resolve) => {
    const start = process.hrtime.bigint();
    execFile(cmd[0], [...cmd.slice(1), ...argv], { cwd, env: { ...process.env, ...env }, maxBuffer: 64 << 20 }, (err, stdout, stderr) => {
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
  return { paths: files.map((f) => f[0]), digest: 'sha256:' + createHash('sha256').update(files.map((f) => `${f[0]}\0${f[1]}\0${f[2]}\n`).join('')).digest('hex'), files: files.length };
}

async function outputOf(kind, outPath) {
  if (kind === 'tree') return dirDigest(outPath);
  const bytes = await fs.readFile(outPath);
  if (kind === 'bytes') return { digest: 'sha256:' + createHash('sha256').update(bytes).digest('hex'), files: 1 };
  const tree = await extractArchive(new Uint8Array(bytes), 'conformance');
  if (kind === 'oci') {
    // The manifest digest is the image ID: equal IDs mean byte-identical layers.
    // The config holds each layer's diff_id and the labels/platform/root.
    const text = (p) => new TextDecoder().decode(tree.get(p).bytes);
    const blob = (d) => `/blobs/sha256/${d.slice(7)}`;
    const index = JSON.parse(text('/index.json'));
    const manifest = JSON.parse(text(blob(index.manifests[0].digest)));
    const refName = index.manifests[0].annotations?.['org.opencontainers.image.ref.name'] ?? '';
    const referrers = index.manifests.slice(1).map((m) => ` + ${m.artifactType ?? m.mediaType} ${m.digest}`).join('');
    return { digest: `id ${index.manifests[0].digest} config ${manifest.config.digest} ${manifest.layers.length} layer(s) ${refName}${referrers}`, files: tree.size };
  }
  return { digest: tree.digest(), files: tree.size };
}

const errorLine = (stderr) => stderr.split('\n').find((l) => l.startsWith('error')) ?? '';
const warningLines = (stderr) => stderr.split('\n').filter((l) => l.startsWith('warning')).join('\n');

/** A `command: "bake"` case: run both implementations in their own copy of the case directory. */
async function runBakeCase(c) {
  const r = {};
  for (const [impl, cmd] of [['ts', TS], ['rust', RUST]]) {
    const work = path.join(tmp, impl, c.name.replace(/[/:]/g, '_'));
    await fs.rm(work, { recursive: true, force: true });
    await fs.cp(c.context, work, { recursive: true, filter: (src) => path.basename(src) !== 'dist' });
    const res = await run(cmd, ['bake', ...(c.args ?? [])], { SITEFILE_HOME: path.join(tmp, impl, '.home'), ...(c.env ?? {}) }, work);
    const tree = await outputOf('tree', path.join(work, c.out ?? 'dist')).catch(() => ({ digest: 'none', files: 0 }));
    r[impl] = { code: res.code, stdout: res.stdout, stderr: res.stderr, spawnError: res.spawnError, output: tree };
  }
  const detail = [];
  if (r.ts.code !== r.rust.code) detail.push(`exit ts ${r.ts.code}, rust ${r.rust.code}`);
  for (const k of ['stdout', 'stderr']) {
    if (r.ts[k] === r.rust[k]) continue;
    const a = r.ts[k].split('\n');
    const b = r.rust[k].split('\n');
    const i = a.findIndex((l, n) => l !== b[n]);
    detail.push(`first ${k} difference at line ${i + 1}:\nts   ${a[i]}\nrust ${b[i]}`);
  }
  if (r.ts.output.digest !== r.rust.output.digest) detail.push(`output ts ${r.ts.output.files} files ${r.ts.output.digest}, rust ${r.rust.output.files} files ${r.rust.output.digest}`);
  if (r.rust.spawnError) detail.push(`rust: ${r.rust.spawnError}`);
  const summary = (x) => ({ code: x.code, error: errorLine(x.stderr), output: { digest: x.output.digest, files: x.output.files } });
  return { name: c.name, compare: 'bake', pass: detail.length === 0, detail, ts: summary(r.ts), rust: summary(r.rust) };
}

const NO_SERVERS = { vars: {}, env: {}, manifestDigest: () => null };
const results = [];
const tmp = await fs.mkdtemp(path.join(os.tmpdir(), 'sitefile-conformance-'));
let servers; // started on the first network case (conformance/servers.mjs)
try {
  for (const c of [...cases(), ...(await caseDirs())]) {
    if (only && !c.name.includes(only)) continue;
    if (c.network || c.steps) {
      // Offline step cases (case.json "steps" without "network") use the same runner with no servers.
      if (c.network) servers ??= await startServers(root, tmp);
      const res = await runNetworkCase(c, { impls: [['ts', TS], ['rust', RUST]], tmp, cwd: root, servers: c.network ? servers : NO_SERVERS, run, outputOf, errorLine });
      results.push({ name: res.name, compare: res.compare, pass: res.pass, ts: res.ts, rust: res.rust });
      console.log(`${res.pass ? 'PASS' : 'FAIL'}  ${c.name}`);
      for (const line of res.detail) console.log(`        ${line.replace(/\n/g, '\n        ')}`);
      continue;
    }
    if (c.command === 'serve') {
      const res = await runServeCase(c, { impls: [['ts', TS], ['rust', RUST]], tmp });
      results.push(res);
      console.log(`${res.pass ? 'PASS' : 'FAIL'}  ${c.name}`);
      if (!res.pass) for (const line of res.detail) console.log(`        ${line.replace(/\n/g, '\n        ')}`);
      continue;
    }
    if (c.command === 'bake') {
      const res = await runBakeCase(c);
      results.push(res);
      console.log(`${res.pass ? 'PASS' : 'FAIL'}  ${c.name}`);
      if (!res.pass) for (const line of res.detail) console.log(`        ${line.replace(/\n/g, '\n        ')}`);
      continue;
    }
    const command = c.command ?? 'build';
    const compare = command === 'build' ? (c.compare ?? 'tree') : 'stdout';
    const r = {};
    // Hard links are created here: git does not keep them.
    const links = (c.links ?? []).map(([from, to]) => [path.join(c.context, from), path.join(c.context, to)]);
    for (const [from, to] of links) await fs.link(from, to).catch(() => undefined);
    // Generated files: remove each top-level entry they create (and only those).
    const generated = [];
    for (const [rel, content] of Object.entries(c.files ?? {})) {
      const top = path.join(c.context, rel.split('/')[0]);
      if (!generated.includes(top) && (await fs.lstat(top).catch(() => undefined)) === undefined) generated.push(top);
      await fs.mkdir(path.dirname(path.join(c.context, rel)), { recursive: true });
      await fs.writeFile(path.join(c.context, rel), content);
    }
    for (const [rel, from] of Object.entries(c.copy ?? {})) {
      const top = path.join(c.context, rel.split('/')[0]);
      if (!generated.includes(top) && (await fs.lstat(top).catch(() => undefined)) === undefined) generated.push(top);
      await fs.mkdir(path.dirname(path.join(c.context, rel)), { recursive: true });
      await fs.copyFile(path.join(root, from), path.join(c.context, rel));
    }
    try {
      for (const [impl, cmd] of [['ts', TS], ['rust', RUST]]) {
        const out = path.join(tmp, impl, c.name.replace(/[/:]/g, '_'), c.out ?? 'dist');
        await fs.mkdir(path.dirname(out), { recursive: true });
        const profile = path.join(path.dirname(out), 'profile.json');
        const argv = [
          ...(command === 'build'
            ? ['build', c.context, '-o', out, '--force', '--no-lockfile', ...(c.args ?? [])]
            : [command, ...(c.operands ?? []), c.context, '--no-lockfile', ...(c.args ?? [])]),
          ...(c.profile ? ['--profile', profile] : []),
        ];
        // Each implementation gets its own SITEFILE_HOME: builds write the TRANSFORM cache and apply
        // the cache size limit there, and must never touch ~/.sitefile.
        const res = await run(cmd, argv, { SITEFILE_HOME: path.join(tmp, impl, '.home'), ...(c.env ?? {}) });
        r[impl] = { code: res.code, error: errorLine(res.stderr), warnings: warningLines(res.stderr), ms: res.ms, spawnError: res.spawnError };
        // Timings differ between implementations (and runs); everything else in the trace must not.
        if (c.profile) r[impl].profile = (await fs.readFile(profile, 'utf8').catch(() => 'missing')).replace(/"ts":\d+,"dur":\d+,/g, '');
        if (res.code === 0) {
          r[impl].output =
            compare === 'stdout'
              ? { digest: 'sha256:' + createHash('sha256').update(res.stdout).digest('hex'), files: res.stdout.split('\n').length - 1, stdout: res.stdout }
              : await outputOf(compare, out).catch((e) => ({ digest: `unreadable: ${e.message}` }));
        }
      }
    } finally {
      for (const [, to] of links) await fs.rm(to, { force: true });
      for (const top of generated) await fs.rm(top, { recursive: true, force: true });
    }
    const missing = {};
    for (const impl of ['ts', 'rust']) {
      const have = r[impl].output?.paths;
      const absent = have ? (c.expectPaths ?? []).filter((p) => !have.includes(p)) : [];
      if (absent.length) missing[impl] = absent;
      delete r[impl].output?.paths;
    }
    const same =
      !r.rust.spawnError &&
      r.ts.code === r.rust.code &&
      r.ts.warnings === r.rust.warnings &&
      (r.ts.code === 0 ? r.ts.output?.digest === r.rust.output?.digest : r.ts.error === r.rust.error) &&
      r.ts.profile === r.rust.profile &&
      Object.keys(missing).length === 0;
    if (r.ts.profile !== r.rust.profile) {
      const a = r.ts.profile.split('\n');
      const b = (r.rust.profile ?? '').split('\n');
      const i = a.findIndex((l, k) => l !== b[k]);
      console.log(`        first --profile difference at line ${i + 1}:\n        ts   ${a[i]}\n        rust ${b[i]}`);
    }
    for (const impl of ['ts', 'rust']) delete r[impl].profile;
    if (!same && compare === 'stdout' && r.ts.output?.stdout !== undefined && r.rust.output?.stdout !== undefined) {
      const a = r.ts.output.stdout.split('\n');
      const b = r.rust.output.stdout.split('\n');
      const i = a.findIndex((l, k) => l !== b[k]);
      console.log(`        first difference at stdout line ${i + 1}:\n        ts   ${a[i]}\n        rust ${b[i]}`);
    }
    for (const impl of ['ts', 'rust']) delete r[impl].output?.stdout;
    results.push({ name: c.name, compare, pass: same, ...r });
    const tag = same ? 'PASS' : 'FAIL';
    console.log(`${tag}  ${c.name}`);
    if (!same) {
      for (const [impl, absent] of Object.entries(missing)) console.log(`        ${impl.padEnd(4)} output lacks ${absent.map((p) => JSON.stringify(p)).join(', ')}`);
      for (const impl of ['ts', 'rust']) {
        const x = r[impl];
        console.log(`        ${impl.padEnd(4)} exit ${x.code} ${x.spawnError ?? (x.code === 0 ? `${x.output?.files} files ${x.output?.digest}` : x.error)}${x.warnings ? ` [${x.warnings.replace(/\n/g, ' | ')}]` : ''}`);
      }
    }
  }
} finally {
  await servers?.close();
  await fs.rm(tmp, { recursive: true, force: true });
}

const passed = results.filter((r) => r.pass).length;
console.log(`\n${passed}/${results.length} cases match`);
const jsonOut = flag('--json');
if (jsonOut) await fs.writeFile(jsonOut, JSON.stringify({ date: new Date().toISOString(), passed, total: results.length, results }, null, 2) + '\n');
process.exitCode = passed === results.length ? 0 : 1;
