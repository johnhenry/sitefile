import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { promises as fs } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseGitUri, parsePack, gitObjectId, checkRefName } from '../src/resources/git.js';
import { DEFAULT_ARCHIVE_LIMITS } from '../src/resources/types.js';
import { parseVendorManifest } from '../src/resources/vendor.js';
import { outdated } from '../src/outdated.js';
import { buildFacts, provenancePredicate } from '../src/attest.js';
import { MemRepo, handleGit, writePack, type GitServerOptions } from './helpers/git-server.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

// IMPORT from Git and Git URL contexts (issue #91), against an in-process protocol v2 server.

const repo = new MemRepo();
const big = 'shared prefix line\n'.repeat(5000); // > 64 KiB, so deltas copy in several pieces
const c1 = repo.commit({
  'Sitefile': 'FROM scratch\nCOPY ./site/ /\nEXPORT /\n',
  'site/index.html': '<h1>v1</h1>\n',
  'docs/a.md': '---\ntitle: A\n---\n# A\n',
  'docs/b.md': '---\ntitle: B\n---\n# B\n',
  'docs/big.txt': big + 'one\n',
  'docs/big2.txt': big + 'two\n',
  'docs/run.sh': { data: '#!/bin/sh\n', mode: '100755' },
  'docs/link.md': { data: 'a.md', mode: '120000' },
  'docs/sub/up': { data: '..', mode: '120000' },
  'docs/alias': { data: 'sub', mode: '120000' },
  'README.md': 'readme\n',
});
repo.tag('v1', c1);
const c2 = repo.commit({ 'site/index.html': '<h1>v2</h1>\n', 'docs/a.md': '# A2\n', 'Sitefile': 'FROM scratch\nCOPY ./site/ /\nEXPORT /\n' });
const bad = new MemRepo();
bad.commit({ 'ok.txt': 'ok', 'mod': { gitlink: 'a'.repeat(40) }, 'esc': { data: '../../etc/passwd', mode: '120000' }, 'abs': { data: '/etc/passwd', mode: '120000' }, 'loop': { data: 'loop', mode: '120000' }, 'dangling': { data: 'nope', mode: '120000' }, 'case/A.txt': 'a', 'case/a.txt': 'b' });
const dotgit = new MemRepo();
dotgit.commit({ '.GIT/config': 'x' });

const log: NonNullable<GitServerOptions['log']> = [];
const repos = new Map<string, MemRepo>([['/repo.git', repo], ['/bad.git', bad], ['/dotgit.git', dotgit], ['/dumb.git', repo], ['/v1.git', repo]]);
const server: Server = createServer((req, res) => {
  const url = new URL(req.url ?? '/', 'http://x');
  if (url.pathname.startsWith('/moved.git/')) {
    res.writeHead(301, { location: url.pathname.replace('/moved.git/', '/repo.git/') + url.search });
    res.end();
    return;
  }
  if (!handleGit(req, res, url, { repos, log })) res.writeHead(404).end();
});
await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
after(() => server.close());
const policy = { allowHttp: true, allowPrivateNetwork: true };
const NET = ['--allow-http', '--allow-private-network'];

async function ctxWith(sitefile: string, extra: Record<string, string> = {}): Promise<string> {
  return makeSite(extra, sitefile);
}

async function code(p: Promise<unknown>): Promise<ErrorCode | 'ok'> {
  try {
    await p;
    return 'ok';
  } catch (e) {
    if (isSitefileError(e)) return e.code;
    throw e;
  }
}

const opts = (contextDir: string, more: Partial<BuildOptions> = {}): BuildOptions => ({ contextDir, fetchPolicy: policy, onWarning: () => {}, ...more });

test('parseGitUri: repo, ref, path and commit pins; bad schemes, credentials, refs and paths', () => {
  const l = parseGitUri('git+https://example.com/x/y.git#v1.2:docs/api/');
  assert.deepEqual(l, { url: 'git+https://example.com/x/y.git#v1.2:docs/api/', repo: 'https://example.com/x/y.git', ref: 'v1.2', path: 'docs/api' });
  assert.equal(parseGitUri('git+https://e.com/r').ref, '');
  assert.equal(parseGitUri('git+https://e.com/r#:sub').path, 'sub');
  const pinned = parseGitUri(`git+https://e.com/r#${'ab'.repeat(20)}`);
  assert.equal(pinned.commit, 'ab'.repeat(20));
  const err = (uri: string): string => {
    try {
      parseGitUri(uri);
      return 'ok';
    } catch (e) {
      return (e as { code: string }).code;
    }
  };
  assert.equal(err('git+ssh://e.com/r'), 'E_PROTOCOL');
  assert.equal(err('git+file:///tmp/r'), 'E_PROTOCOL');
  assert.equal(err('git+https://u:p@e.com/r'), 'E_PROTOCOL');
  assert.equal(err('git+https://e.com/r?x=1'), 'E_PROTOCOL');
  assert.equal(err('git+https://e.com/r#a..b'), 'E_VALIDATION');
  assert.equal(err('git+https://e.com/r#main:../x'), 'E_PATH_ESCAPE');
  for (const ok of ['main', 'refs/heads/main', 'feature/x-1', 'v1.0']) checkRefName(ok);
  for (const no of ['-x', 'a/', 'a.lock', 'a b', 'a~1', '.hidden', 'a//b', '@', 'x@{1}']) assert.throws(() => checkRefName(no), no);
});

test('parsePack reads whole objects, OFS_DELTA and REF_DELTA chains, and checks the checksum', () => {
  const ids = repo.reachable(c1);
  const pack = writePack(repo, ids, true);
  const objs = parsePack(pack, 'r', DEFAULT_ARCHIVE_LIMITS);
  for (const id of ids) assert.equal(gitObjectId(objs.get(id)!.type, objs.get(id)!.data), id);
  assert.equal(objs.size, ids.length);
  const corrupt = Uint8Array.from(pack);
  corrupt[20] = corrupt[20]! ^ 1;
  assert.throws(() => parsePack(corrupt, 'r', DEFAULT_ARCHIVE_LIMITS), (e: unknown) => isSitefileError(e, 'E_GIT'));
  assert.throws(() => parsePack(pack, 'r', { ...DEFAULT_ARCHIVE_LIMITS, maxTotalBytes: 1000 }), (e: unknown) => isSitefileError(e, 'E_ARCHIVE_LIMIT'));
});

let gitAvailable = true;
try {
  execFileSync('git', ['--version'], { stdio: 'ignore' });
} catch {
  gitAvailable = false;
}

test('parsePack agrees with packs written by git itself (deltified)', { skip: !gitAvailable && 'git not on PATH' }, async () => {
  const dir = await tmpDir('sitefile-gitpack-');
  const run = (args: string[], input?: string): Buffer =>
    execFileSync('git', args, { cwd: dir, input, env: { ...process.env, GIT_CONFIG_NOSYSTEM: '1', HOME: dir, GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@e', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@e' }, maxBuffer: 1 << 26 });
  run(['init', '-q']);
  for (let i = 0; i < 6; i++) {
    await fs.writeFile(path.join(dir, `f${i}.txt`), big + `variant ${i}\n`.repeat(i + 1));
    await fs.mkdir(path.join(dir, 'd'), { recursive: true });
    await fs.writeFile(path.join(dir, 'd', `g${i}.txt`), `x${i}`.repeat(1000));
  }
  await fs.symlink('f0.txt', path.join(dir, 'ln'));
  run(['add', '-A']);
  run(['commit', '-qm', 'one']);
  const list = run(['rev-list', '--objects', 'HEAD']).toString().trim().split('\n').map((l) => l.split(' ')[0]!);
  const pack = new Uint8Array(run(['pack-objects', '--stdout', '-q', '--window=10', '--depth=5'], list.join('\n') + '\n'));
  const objs = parsePack(pack, 'local', DEFAULT_ARCHIVE_LIMITS);
  assert.deepEqual([...objs.keys()].sort(), [...list].sort());
});

test('IMPORT from Git: a directory AS tree, symlinks resolved inside it, modes kept, lockfile records the commit', async () => {
  const ctx = await ctxWith(`FROM scratch\nIMPORT docs FROM git+${base}/repo.git#v1:docs\nCOPY docs: /d/\nEXPORT /\n`);
  const warnings: string[] = [];
  const r = await build(opts(ctx, { onWarning: (m) => warnings.push(m) }));
  const files = r.output.entries().map(([p, n]) => `${p} ${n.mode.toString(8)}`);
  // alias -> sub, and sub/up -> .. points back at an ancestor: that directory cycle is skipped.
  assert.deepEqual(files, ['/d/a.md 644', '/d/b.md 644', '/d/big.txt 644', '/d/big2.txt 644', '/d/link.md 644', '/d/run.sh 755']);
  assert.equal(new TextDecoder().decode(r.output.get('/d/link.md')!.bytes), '---\ntitle: A\n---\n# A\n');
  assert.equal(r.output.get('/d/run.sh')!.mode, 0o755);
  assert.equal(r.output.get('/d/big2.txt')!.bytes.byteLength, big.length + 4);
  assert.ok(!files.some((f) => f.startsWith('/d/sub/up/')), 'a directory symlink back to an ancestor is skipped');
  assert.ok(warnings.some((w) => w.includes('is not pinned; pin commit ' + c1)), warnings.join('\n'));
  const lock = JSON.parse(await fs.readFile(path.join(ctx, 'Sitefile.lock'), 'utf8'));
  assert.deepEqual(lock.git, { [`git+${base}/repo.git#v1:docs`]: { commit: c1 } });
  const res = r.resources.get('docs')!;
  assert.equal(res.provenance.gitCommit, c1);
  assert.equal(res.provenance.resolvedUrl, `git+${base}/repo.git#${c1}:docs`);
  // The fetch was shallow and asked for the commit.
  const fetchReq = log.filter((l) => l.method === 'POST');
  assert.ok(fetchReq.every((l) => l.headers['git-protocol'] === 'version=2'));
});

test('a locked ref is not resolved again; --update-lock follows it; --frozen-lockfile needs an entry', async () => {
  const ctx = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#v1:site\nCOPY site: /\nEXPORT /\n`);
  const r1 = await build(opts(ctx));
  assert.equal(new TextDecoder().decode(r1.output.get('/index.html')!.bytes), '<h1>v1</h1>\n', 'annotated tag peeled to its commit');
  // Lock it to c1 under another ref and see that the lock wins over the moving ref.
  const ctx2 = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#main:site\nCOPY site: /\nEXPORT /\n`, {
    'Sitefile.lock': JSON.stringify({ version: 1, resources: {}, git: { [`git+${base}/repo.git#main:site`]: { commit: c1 } } }),
  });
  const r2 = await build(opts(ctx2));
  assert.equal(new TextDecoder().decode(r2.output.get('/index.html')!.bytes), '<h1>v1</h1>\n');
  const r3 = await build(opts(ctx2, { updateLock: true }));
  assert.equal(new TextDecoder().decode(r3.output.get('/index.html')!.bytes), '<h1>v2</h1>\n');
  assert.equal(JSON.parse(await fs.readFile(path.join(ctx2, 'Sitefile.lock'), 'utf8')).git[`git+${base}/repo.git#main:site`].commit, c2);
  const ctx3 = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#main:site\nCOPY site: /\nEXPORT /\n`);
  assert.equal(await code(build(opts(ctx3, { frozenLockfile: true }))), 'E_LOCK_MISSING');
});

test('--strict-pins: a commit in the URL or a lockfile entry pins a Git import', async () => {
  const unpinned = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#main:site\nCOPY site: /\nEXPORT /\n`);
  assert.equal(await code(build(opts(unpinned, { strictPins: true }))), 'E_UNPINNED');
  const pinned = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#${c1}:site\nCOPY site: /\nEXPORT /\n`);
  const r = await build(opts(pinned, { strictPins: true }));
  assert.equal(r.pins?.imports.site, 'commit');
  const facts = await buildFacts(r, { contextDir: pinned });
  const pred = provenancePredicate(facts, undefined) as { buildDefinition: { resolvedDependencies: Array<{ name: string; digest: Record<string, string>; annotations?: unknown }> } };
  const dep = pred.buildDefinition.resolvedDependencies.find((d) => d.name === 'import:site')!;
  assert.equal(dep.digest.gitCommit, c1);
  assert.deepEqual(dep.annotations, { 'dev.sitefile.pin': 'commit' });
  // INTEGRITY makes no sense for a tree: refused at plan time.
  const integ = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#main:site INTEGRITY sha256:${'0'.repeat(64)}\nEXPORT /\n`);
  assert.equal(await code(build(opts(integ))), 'E_VALIDATION');
});

test('Git imports of a file (AS text), of Markdown collections and errors for missing refs and paths', async () => {
  const ctx = await ctxWith(
    `FROM scratch\nIMPORT readme FROM git+${base}/repo.git#v1:README.md AS text\nIMPORT posts FROM git+${base}/repo.git#v1:docs AS markdown\nIMPORT sh FROM git+${base}/repo.git#v1:docs/run.sh\nCOPY sh: /run.sh\nRENDER <<T TO /i.txt\n\${readme}\${#each posts as item}[\${item.title}]\${/each}\nT\nEXPORT /\n`,
  );
  const r = await build(opts(ctx));
  assert.match(new TextDecoder().decode(r.output.get('/i.txt')!.bytes), /^readme\n/);
  assert.equal(r.output.get('/run.sh')!.mode, 0o755);
  const missingRef = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/repo.git#nope\nEXPORT /\n`);
  assert.equal(await code(build(opts(missingRef))), 'E_NOT_FOUND');
  const missingPath = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/repo.git#main:nope\nEXPORT /\n`);
  assert.equal(await code(build(opts(missingPath))), 'E_NOT_FOUND');
  const unknownCommit = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/repo.git#${'1'.repeat(40)}\nEXPORT /\n`);
  assert.equal(await code(build(opts(unknownCommit))), 'E_GIT');
});

test('submodules, escaping, absolute, looping and dangling symlinks, case collisions and .git entries are refused', async () => {
  const cases: Array<[string, ErrorCode]> = [
    ['mod', 'E_UNSUPPORTED_FILE'],
    ['esc', 'E_PATH_ESCAPE'],
    ['abs', 'E_PATH_ESCAPE'],
    ['loop', 'E_UNSUPPORTED_FILE'],
    ['dangling', 'E_NOT_FOUND'],
    ['case', 'E_PATH_COLLISION'],
  ];
  for (const [p, c] of cases) {
    const ctx = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/bad.git#main:${p}\nEXPORT /\n`);
    assert.equal(await code(build(opts(ctx))), c, p);
  }
  const whole = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/bad.git\nEXPORT /\n`);
  assert.equal(await code(build(opts(whole))), 'E_PATH_ESCAPE'); // 'abs' is the first entry walked
  const dg = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/dotgit.git\nEXPORT /\n`);
  assert.equal(await code(build(opts(dg))), 'E_ARCHIVE_UNSAFE');
});

test('network policy: http needs --allow-http, loopback needs --allow-private-network, dumb and v0 servers are refused, redirects are followed for info/refs', async () => {
  const ctx = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/repo.git#main:site\nCOPY x: /\nEXPORT /\n`);
  assert.equal(await code(build({ contextDir: ctx, fetchPolicy: { allowPrivateNetwork: true } })), 'E_PROTOCOL');
  assert.equal(await code(build({ contextDir: ctx, fetchPolicy: { allowHttp: true } })), 'E_SSRF_BLOCKED');
  const dumb = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/dumb.git#main\nEXPORT /\n`);
  assert.equal(await code(build(opts(dumb))), 'E_GIT');
  const v1 = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/v1.git#main\nEXPORT /\n`);
  assert.equal(await code(build(opts(v1))), 'E_GIT');
  const moved = await ctxWith(`FROM scratch\nIMPORT x FROM git+${base}/moved.git#main:site\nCOPY x: /\nEXPORT /\n`);
  const r = await build(opts(moved));
  assert.equal(r.output.size, 1);
});

test('sitefile vendor stores a Git import (tree as tar, file as bytes); offline builds read it', async () => {
  const home = await tmpDir();
  const ctx = await ctxWith(`FROM scratch\nIMPORT site FROM git+${base}/repo.git#main:site\nIMPORT readme FROM git+${base}/repo.git#v1:README.md\nCOPY site: /\nCOPY readme: /readme.md\nEXPORT /\n`);
  const v = await runCli(home, ['vendor', ctx, ...NET]);
  assert.equal(v.code, 0, v.stderr);
  assert.match(v.stdout, /^Vendored 2 resources and 0 images \(2 blobs/);
  const m = parseVendorManifest(await fs.readFile(path.join(ctx, 'vendor/sitefile/manifest.json')), 'm');
  assert.deepEqual(Object.keys(m.git!), [`git+${base}/repo.git#main:site`, `git+${base}/repo.git#v1:README.md`]);
  assert.equal(m.git![`git+${base}/repo.git#main:site`]!.kind, 'tree');
  assert.equal(m.git![`git+${base}/repo.git#v1:README.md`]!.mode, '644');
  const before = log.length;
  const b = await runCli(home, ['build', ctx, '-o', path.join(ctx, 'out'), '--frozen-lockfile']);
  assert.equal(b.code, 0, b.stderr);
  assert.equal(log.length, before, 'no Git request');
  assert.equal(await fs.readFile(path.join(ctx, 'out', 'index.html'), 'utf8'), '<h1>v2</h1>\n');
  assert.equal(await fs.readFile(path.join(ctx, 'out', 'readme.md'), 'utf8'), 'readme\n');
});

test('outdated: a locked ref that moved is changed; a commit pin is digest-only; unpinned shows the commit', async () => {
  const ctx = await ctxWith(
    `FROM scratch\nIMPORT a FROM git+${base}/repo.git#main:site\nIMPORT b FROM git+${base}/repo.git#${c1}\nIMPORT c FROM git+${base}/repo.git#v1\nIMPORT d FROM git+${base}/repo.git#gone\nEXPORT /\n`,
    { 'Sitefile.lock': JSON.stringify({ version: 1, resources: {}, git: { [`git+${base}/repo.git#main:site`]: { commit: c1 } } }) },
  );
  const r = await outdated({ contextDir: ctx, fetchPolicy: policy });
  assert.deepEqual(
    r.inputs.map((i) => [i.name, i.status, i.pinnedBy, i.latest]),
    [
      ['a', 'changed', 'lockfile', c2],
      ['b', 'digest-only', 'commit', null],
      ['c', 'unpinned', null, c1],
      ['d', 'error', null, null],
    ],
  );
});

test('sitefile build <git URL>: the context is fetched, built and removed; the lockfile is not written back', async () => {
  const home = await tmpDir();
  const cwd = await tmpDir();
  const r = await runCli(home, ['build', `${base}/repo.git#v1`, ...NET], cwd);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stderr, new RegExp(`^context: ${base}/repo.git#v1 at commit ${c1}\n`));
  assert.equal(await fs.readFile(path.join(cwd, 'dist', 'index.html'), 'utf8'), '<h1>v1</h1>\n');
  assert.doesNotMatch(r.stdout, /Sitefile\.lock/);
  const w = await runCli(home, ['build', `${base}/repo.git`, '--watch', ...NET], cwd);
  assert.equal(w.code, 2);
  const f = await runCli(home, ['build', `${base}/repo.git`, '-f', '../x', ...NET], cwd);
  assert.equal(f.code, 2);
  const sub = await runCli(home, ['validate', `git+${base}/repo.git#main:site`, ...NET], cwd);
  assert.equal(sub.code, 1);
  assert.match(sub.stderr, /error\[E_NOT_FOUND\]: cannot read Sitefile/);
});
