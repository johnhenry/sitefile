import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { build, lintBuild, type BuildOptions } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { importedStageRefs, plan } from '../src/planner.js';
import { planGraph, why } from '../src/explain.js';
import { outdated } from '../src/outdated.js';
import { vendor } from '../src/vendor.js';
import { BuildMemo } from '../src/memo.js';
import { MemRepo, handleGit } from './helpers/git-server.js';
import { repoRoot, tmpDir } from './helpers/cli.js';

// IMPORT … AS sitefile (issue #97): stages of another Sitefile, local or from Git, named <import>+<stage>.

async function ctx(files: Record<string, string>): Promise<string> {
  const dir = await tmpDir('sitefile-sfimport-');
  for (const [p, c] of Object.entries(files)) {
    await fs.mkdir(path.dirname(path.join(dir, p)), { recursive: true });
    await fs.writeFile(path.join(dir, p), c);
  }
  return dir;
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

async function failure(p: Promise<unknown>): Promise<string> {
  try {
    await p;
  } catch (e) {
    if (isSitefileError(e)) return `${e.code}: ${e.message}`;
    throw e;
  }
  return 'ok';
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

const THEME = {
  'theme/Sitefile': 'ARG ACCENT = "teal"\nSTAGE assets\nRENDER ./a.css TO /a.css\nSTAGE layout\nCOPY ./page.html /page.html\nFROM scratch\nEXPORT /\n',
  'theme/a.css': 'h1 { color: ${ACCENT} }\n',
  'theme/page.html': '<h1>${TITLE}</h1>\n',
};

test('parse: AS sitefile, ARG clauses and <import>+<stage> references', () => {
  const p = parseProgram('IMPORT t FROM ./theme AS sitefile ARG A=1 ARG B = "x y"\nSTAGE s FROM t+assets\nCOPY --from=t+layout / /l/\nEXPORT /');
  const imp = p.instructions[0]!;
  assert.equal(imp.kind, 'IMPORT');
  assert.deepEqual(imp.kind === 'IMPORT' && imp.args?.map((a) => a.name), ['A', 'B']);
  // The IMPORT AS sitefile came before the first stage without starting an implicit one.
  assert.equal(p.instructions[1]!.kind, 'FROM');
  const pl = plan(p);
  assert.deepEqual(pl.imports[0]!.args?.map((a) => [a.name, a.value]), [['A', '1'], ['B', 'x y']]);
  assert.deepEqual(pl.stages[0]!.base, { kind: 'stage', name: 't+assets' });
  assert.deepEqual(importedStageRefs(pl.stages).map((r) => r.name), ['t+assets', 't+layout']);
});

test('validate: errors about Sitefile imports', () => {
  const err = (src: string): string => {
    try {
      plan(parseProgram(src));
      return 'ok';
    } catch (e) {
      if (isSitefileError(e)) return `${e.code} ${e.span?.line}:${e.span?.column} ${e.detail}`;
      throw e;
    }
  };
  assert.equal(err('IMPORT t FROM ./t AS json ARG A=1\nEXPORT /'), "E_VALIDATION 1:27 IMPORT t: ARG sets a build arg of an imported Sitefile, so it needs AS sitefile");
  assert.equal(err('IMPORT t FROM ./t AS sitefile ARG A=1 ARG A=2\nEXPORT /'), 'E_SYNTAX 1:39 ARG A given twice');
  assert.equal(err('IMPORT t FROM ./t AS sitefile ARG A\nEXPORT /'), 'E_SYNTAX 1:36 expected = after ARG A (ARG A=<value>)');
  assert.match(err(`IMPORT t FROM ./t AS sitefile INTEGRITY sha256:${'a'.repeat(64)}\nEXPORT /`), /^E_VALIDATION 1:1 IMPORT t: INTEGRITY does not apply to AS sitefile/);
  assert.equal(err('IMPORT t FROM https://e.x/t AS sitefile\nEXPORT /'), 'E_PROTOCOL 1:15 IMPORT t: AS sitefile reads a context directory or a git+https:// repository, not a URL');
  assert.equal(err('FROM scratch\nCOPY --from=t+a / /\nEXPORT /'), "E_UNDEFINED 2:6 --from=t+a: 't' is not an IMPORT … AS sitefile declared on an earlier line");
  assert.equal(err('IMPORT t FROM ./t AS sitefile\nFROM scratch\nCOPY t: /\nEXPORT /'), "E_TYPE 3:6 't' is a Sitefile import; read one of its stages with --from=t+<stage>");
  assert.equal(err('IMPORT t FROM ./t AS sitefile\nFROM scratch\nFEED t BASE https://e.x LINK /a\nEXPORT /'), "E_TYPE 3:6 FEED t: 't' is a Sitefile import, not a list");
  assert.equal(err('IMPORT t FROM ./t AS sitefile\nSTAGE t\nEXPORT /'), "E_DUPLICATE 2:1 't' is already defined");
  assert.equal(err('IMPORT t FROM ./t AS sitefile ARG X=${NOPE}\nEXPORT /'), 'E_UNDEFINED 1:37 undefined variable ${NOPE}: declare it with ARG or LET on an earlier line');
  // Without an AS sitefile import, `a+b` is still a directory name, as before.
  assert.deepEqual(plan(parseProgram('FROM t+a\nEXPORT /')).stages[0]!.base, { kind: 'local', path: 't+a' });
  // `--from=` takes only one `+`.
  assert.equal(err('IMPORT t FROM ./t AS sitefile\nFROM scratch\nCOPY --from=t+a+b / /\nEXPORT /'), "E_SYNTAX 3:6 --from needs a stage name, got 't+a+b'");
});

test('build: stages of a local Sitefile, with its own ARGs, paths and origins', async () => {
  const dir = await ctx({
    ...THEME,
    Sitefile:
      'ARG ACCENT = "red"\nARG TITLE = "Site"\nIMPORT theme FROM ./theme AS sitefile ARG ACCENT=${ACCENT}\nSTAGE site FROM theme+assets\nRENDER --escape=html --from=theme+layout /page.html TO /index.html\nEXPORT /\n',
  });
  const warnings: string[] = [];
  const r = await build({ contextDir: dir, onWarning: (m) => warnings.push(m) });
  assert.deepEqual(paths(r.output), ['/a.css', '/index.html']);
  assert.equal(text(r.output.get('/a.css')?.bytes), 'h1 { color: red }\n');
  // The importer's TITLE is a template variable of its own RENDER; the theme's ARGs are the theme's.
  assert.equal(text(r.output.get('/index.html')?.bytes), '<h1>Site</h1>\n');
  const label = path.relative(process.cwd(), path.join(dir, 'theme', 'Sitefile'));
  assert.equal(r.output.get('/a.css')?.origin, `${label}:3 RENDER`);
  assert.deepEqual(warnings, []);
  // The theme's defaults apply without ARG clauses; this build's --build-args never reach it.
  await fs.writeFile(path.join(dir, 'Sitefile'), 'IMPORT theme FROM ./theme AS sitefile\nFROM theme+assets\nEXPORT /\n');
  const r2 = await build({ contextDir: dir, buildArgs: { ACCENT: 'blue' }, onWarning: () => {} });
  assert.equal(text(r2.output.get('/a.css')?.bytes), 'h1 { color: teal }\n');
  assert.ok(r2.warnings.includes('--build-arg ACCENT does not match any ARG and was ignored'));
});

test('build: unknown stage, unknown and missing ARGs, typed ARGs, no Sitefile, files, plugins', async () => {
  const dir = await ctx({ ...THEME, 'theme/x.txt': 'x', 'empty/x.txt': 'x', 'req/Sitefile': 'ARG N: number\nSTAGE s\nEXPORT /\n' });
  const run = async (sitefile: string): Promise<string> => {
    await fs.writeFile(path.join(dir, 'Sitefile'), sitefile);
    return failure(build({ contextDir: dir, onWarning: () => {} }));
  };
  const label = path.relative(process.cwd(), path.join(dir, 'theme', 'Sitefile'));
  const req = path.relative(process.cwd(), path.join(dir, 'req', 'Sitefile'));
  const top = path.relative(process.cwd(), path.join(dir, 'Sitefile'));
  assert.equal(await run('IMPORT t FROM ./theme AS sitefile\nFROM t+nope\nEXPORT /\n'), `E_UNDEFINED: ${top}:2:1: t+nope: ${label} has no stage 'nope' (stages: assets, layout)`);
  assert.equal(await run('IMPORT t FROM ./theme AS sitefile ARG NOPE=1\nFROM t+assets\nEXPORT /\n'), `E_UNDEFINED: ${top}:1:35: IMPORT t: ${label} has no ARG NOPE`);
  assert.equal(await run('IMPORT t FROM ./req AS sitefile\nFROM t+s\nEXPORT /\n'), `E_MISSING_ARG: ${top}:1:1: IMPORT t: ARG N of ${req} has no default; set it with ARG N=<value>`);
  assert.equal(await run('IMPORT t FROM ./req AS sitefile ARG N=x\nFROM t+s\nEXPORT /\n'), `E_TYPE: ${req}:1:1: IMPORT t ARG N expects a number, got 'x'`);
  assert.equal(await run('IMPORT t FROM ./empty AS sitefile\nFROM t+s\nEXPORT /\n'), `E_NOT_FOUND: ${top}:1:1: IMPORT t: './empty' has no Sitefile`);
  assert.equal(await run('IMPORT t FROM ./theme/x.txt AS sitefile\nFROM t+s\nEXPORT /\n'), `E_TYPE: ${top}:1:1: IMPORT t: './theme/x.txt' is a file; AS sitefile needs the directory that holds the Sitefile`);
  assert.equal(await run('IMPORT t FROM ./nope AS sitefile\nFROM t+s\nEXPORT /\n'), `E_NOT_FOUND: ${top}:1:1: './nope' does not exist in the build context`);
  await fs.writeFile(path.join(dir, 'req', 'Sitefile'), 'TOOL cp\nSTAGE s\nEXPORT /\n');
  assert.equal(await run('IMPORT t FROM ./req AS sitefile\nFROM t+s\nEXPORT /\n'), `E_UNSUPPORTED: ${req}:1:1: TOOL cp: a Sitefile imported AS sitefile cannot run plugins or tools`);
});

test('build: an imported Sitefile is sandboxed to its directory', async () => {
  const dir = await ctx({ 'theme/Sitefile': 'STAGE s\nCOPY ./link/ /x/\nEXPORT /\n', 'secret/key.txt': 'secret', Sitefile: 'IMPORT t FROM ./theme AS sitefile\nFROM t+s\nEXPORT /\n' });
  await fs.symlink('../secret', path.join(dir, 'theme', 'link'));
  assert.match(await failure(build({ contextDir: dir })), /^E_PATH_ESCAPE: .*theme\/Sitefile:2:6: '\.\/link' resolves outside the build context$/);
  await fs.writeFile(path.join(dir, 'theme', 'Sitefile'), 'STAGE s\nCOPY ../secret/key.txt /k\nEXPORT /\n');
  assert.equal(await code(build({ contextDir: dir })), 'E_PATH_ESCAPE');
});

test('build: nested imports, cycles and the depth limit', async () => {
  const files: Record<string, string> = {
    Sitefile: 'IMPORT a FROM ./a AS sitefile\nFROM a+out\nEXPORT /\n',
    'a/Sitefile': 'IMPORT b FROM ./b AS sitefile ARG WHO=a\nSTAGE out FROM b+inner\nCOPY ./a.txt /a.txt\nEXPORT /\n',
    'a/a.txt': 'a\n',
    'a/b/Sitefile': 'ARG WHO = "nobody"\nSTAGE inner\nCOPY <<T /inner.txt\nbuilt for ${WHO}\nT\nRENDER <<T TO /who.txt\n${WHO}\nT\nEXPORT /\n',
  };
  const dir = await ctx(files);
  const r = await build({ contextDir: dir, recordSteps: true });
  assert.deepEqual(paths(r.output), ['/a.txt', '/inner.txt', '/who.txt']);
  assert.equal(text(r.output.get('/who.txt')?.bytes), 'a\n');
  // why follows the file into the imported Sitefiles.
  const w = why(r, '/who.txt', dir);
  assert.deepEqual(w.history.map((e) => [e.location.replace(/^.*\/(a\/b\/Sitefile:)/, '$1'), e.stage]), [['a/b/Sitefile:6', 'a+b+inner']]);
  assert.deepEqual([...r.resources.keys()], ['a', 'a+b']);
  // A Sitefile importing itself is a cycle.
  await fs.writeFile(path.join(dir, 'Sitefile'), 'IMPORT me FROM ./ AS sitefile\nFROM me+x\nEXPORT /\n');
  assert.match(await failure(build({ contextDir: dir })), /E_VALIDATION: .*IMPORT me: Sitefile import cycle: (.*)Sitefile -> \1Sitefile$/);
  // Nine levels deep is one too many.
  const deep: Record<string, string> = {};
  let rel = '';
  for (let i = 0; i <= 9; i++) {
    deep[`${rel}Sitefile`] = i < 9 ? `IMPORT n FROM ./n AS sitefile\nSTAGE s FROM n+s\nEXPORT /\n` : 'STAGE s\nEXPORT /\n';
    rel += 'n/';
  }
  const d2 = await ctx(deep);
  assert.match(await failure(build({ contextDir: d2, onWarning: () => {} })), /E_VALIDATION: .*n\/n\/n\/n\/n\/n\/n\/n\/Sitefile:1:1: IMPORT n: Sitefile imports nest more than 8 levels deep$/);
  delete deep['n/n/n/n/n/n/n/n/n/Sitefile'];
  deep['n/n/n/n/n/n/n/n/Sitefile'] = 'STAGE s\nEXPORT /\n';
  const d3 = await ctx(deep);
  assert.equal(await code(build({ contextDir: d3, onWarning: () => {} })), 'ok');
});

test('--target builds only the imported stages its stages read; lint, graph', async () => {
  const dir = await ctx({
    ...THEME,
    'other/Sitefile': 'STAGE s\nCOPY ./missing.txt /m\nEXPORT /\n',
    Sitefile: 'IMPORT theme FROM ./theme AS sitefile\nIMPORT other FROM ./other AS sitefile\nIMPORT unused FROM ./theme AS sitefile\nSTAGE a FROM theme+assets\nSTAGE b FROM other+s\nEXPORT /\n',
  });
  // Stage b reads other+s, which fails; --target a never builds it.
  assert.equal(await code(build({ contextDir: dir })), 'E_NOT_FOUND');
  const r = await build({ contextDir: dir, target: 'a' });
  assert.deepEqual(paths(r.output), ['/a.css']);
  const lint = await lintBuild({ contextDir: dir });
  assert.deepEqual(lint.map((l) => l.replace(/^.*Sitefile:/, '')), ['3: [unused-name] IMPORT unused is never used']);
  const g = planGraph(plan(parseProgram(await fs.readFile(path.join(dir, 'Sitefile'), 'utf8'))));
  const node = g.nodes.find((n) => n.id === 'stage:theme+assets');
  assert.deepEqual(node, { id: 'stage:theme+assets', kind: 'stage', label: 'stage assets of IMPORT theme', line: 1 });
  assert.ok(g.edges.some((e) => e.from === 'import:theme' && e.to === 'stage:theme+assets'));
  assert.ok(g.edges.some((e) => e.from === 'stage:theme+assets' && e.to === 'stage:a' && e.kind === 'base'));
});

test('watch memo: an edit in the imported Sitefile is seen, and an unchanged build is reused', async () => {
  const dir = await ctx({ ...THEME, Sitefile: 'IMPORT theme FROM ./theme AS sitefile ARG ACCENT=red\nFROM theme+assets\nEXPORT /\n' });
  const memo = new BuildMemo();
  const o: BuildOptions = { contextDir: dir, memo };
  const a = await build(o);
  const b = await build(o);
  assert.equal(b.output.digest(), a.output.digest());
  assert.equal(b.memoStats?.misses, 0);
  await fs.writeFile(path.join(dir, 'theme', 'a.css'), 'h2 { color: ${ACCENT} }\n');
  const c = await build(o);
  assert.equal(text(c.output.get('/a.css')?.bytes), 'h2 { color: red }\n');
  assert.equal(c.output.digest(), (await build({ contextDir: dir })).output.digest());
});

test('the example: examples/shared-stages', async () => {
  const r = await build({ contextDir: path.join(repoRoot, 'examples', 'shared-stages') });
  assert.deepEqual(paths(r.output), ['/about.html', '/assets/reset.css', '/assets/theme.css', '/assets/theme.css.in', '/index.html']);
  assert.match(text(r.output.get('/assets/theme.css')?.bytes), /--accent: rebeccapurple/);
  assert.deepEqual(r.warnings, []);
});

// ---------------------------------------------------------------- Git

const repo = new MemRepo();
const c1 = repo.commit({
  'theme/Sitefile': 'ARG ACCENT = "teal"\nIMPORT data FROM ./data.json AS json\nSTAGE assets\nRENDER <<C TO /a.css\nh1 { color: ${ACCENT} } /* ${data.v} */\nC\nEXPORT /\n',
  'theme/data.json': '{"v": 1}',
});
repo.tag('v1', c1);
const c2 = repo.commit({
  'theme/Sitefile': 'ARG ACCENT = "teal"\nSTAGE assets\nCOPY <<C /a.css\nv2\nC\nEXPORT /\n',
});
const server: Server = createServer((req, res) => {
  if (!handleGit(req, res, new URL(req.url ?? '/', 'http://x'), { repos: new Map([['/t.git', repo]]) })) res.writeHead(404).end();
});
await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
after(() => server.close());
const policy = { allowHttp: true, allowPrivateNetwork: true };

test('Git: the lockfile records the commit; frozen, strict pins, vendor and outdated', async () => {
  const url = `git+${base}/t.git#main:theme`;
  const dir = await ctx({ Sitefile: `IMPORT theme FROM ${url} AS sitefile ARG ACCENT=red\nFROM theme+assets\nEXPORT /\n` });
  const opts = (more: Partial<BuildOptions> = {}): BuildOptions => ({ contextDir: dir, fetchPolicy: policy, onWarning: () => {}, ...more });
  assert.equal(await code(build(opts({ frozenLockfile: true }))), 'E_LOCK_MISSING');
  assert.equal(await code(build(opts({ strictPins: true }))), 'E_UNPINNED');
  const r = await build(opts());
  assert.equal(text(r.output.get('/a.css')?.bytes), 'v2\n');
  assert.equal(r.output.get('/a.css')?.origin, `git+${base}/t.git#${c2}:theme/Sitefile:3 COPY`);
  const lock = JSON.parse(await fs.readFile(path.join(dir, 'Sitefile.lock'), 'utf8'));
  assert.deepEqual(lock.git, { [url]: { commit: c2 } });
  // Pinned by the lockfile now: frozen and strict builds pass.
  assert.equal(await code(build(opts({ frozenLockfile: true, strictPins: true }))), 'ok');

  // A tag: the theme's own import (a local file of the repository) is resolved inside the checkout.
  const v1 = `git+${base}/t.git#v1:theme`;
  await fs.writeFile(path.join(dir, 'Sitefile'), `IMPORT theme FROM ${v1} AS sitefile\nFROM theme+assets\nEXPORT /\n`);
  const r1 = await build(opts());
  assert.equal(text(r1.output.get('/a.css')?.bytes), 'h1 { color: teal } /* 1 */\n');
  assert.deepEqual([...r1.resources.keys()], ['theme', 'theme+data']);
  assert.equal(r1.resources.get('theme')?.provenance.gitCommit, c1);

  // vendor stores the checkout; an offline build (no network policy) reads it.
  const v = await vendor(opts());
  assert.equal(v.resources, 1);
  const off = await build({ contextDir: dir, frozenLockfile: true, onWarning: () => {} });
  assert.equal(text(off.output.get('/a.css')?.bytes), 'h1 { color: teal } /* 1 */\n');

  // outdated: the tag is current; the theme's local import is not a remote input.
  const o = await outdated(opts());
  assert.deepEqual(o.inputs.map((i) => [i.name, i.status, i.pin]), [['theme', 'current', c1]]);
});

function paths(t: { entries(): Array<[string, unknown]> }): string[] {
  return t.entries().map(([p]) => p);
}
