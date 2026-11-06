import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from '../src/executor.js';
import { graphDot, graphText, planGraph, why, whyText } from '../src/explain.js';
import { parseProgram } from '../src/language/validate.js';
import { plan, targetPlan } from '../src/planner.js';
import { isSitefileError } from '../src/errors.js';

// `sitefile graph` and `sitefile why <path>` (issue #94).

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const cli = path.join(root, 'lib', 'src', 'cli.js');
const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function sitefile(args: string[], home: string): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(process.execPath, [cli, ...args], { cwd: root, env: { ...process.env, SITEFILE_HOME: home } }, (err, stdout, stderr) => {
      resolve({ code: err ? (typeof err.code === 'number' ? err.code : 1) : 0, stdout, stderr });
    });
  });
}

const SITEFILE = `STAGE build
IMPORT theme FROM ./theme AS tree
COPY ./src/ /out/
COPY --overwrite ./override.html /out/index.html
STAGE more FROM build
COPY <<X /out/extra.txt
extra
X
STAGE site
COPY --from=more /out/ /
COPY --overwrite theme: /
REDIRECT /old /new
EXPORT /
`;

async function project(): Promise<{ ctx: string; home: string }> {
  const work = await mkdtemp(path.join(os.tmpdir(), 'sitefile-explain-'));
  temps.push(work);
  const ctx = path.join(work, 'ctx');
  await mkdir(path.join(ctx, 'src'), { recursive: true });
  await mkdir(path.join(ctx, 'theme'), { recursive: true });
  await writeFile(path.join(ctx, 'Sitefile'), SITEFILE);
  await writeFile(path.join(ctx, 'src', 'index.html'), 'first\n');
  await writeFile(path.join(ctx, 'src', 'style.css'), 'a{}\n');
  await writeFile(path.join(ctx, 'override.html'), 'second\n');
  await writeFile(path.join(ctx, 'theme', 'style.css'), 'b{}\n');
  return { ctx, home: path.join(work, '.home') };
}

test('planGraph: nodes for imports, locals, operations, stages, site files and the export', () => {
  const p = plan(parseProgram(SITEFILE, 'Sitefile'));
  const g = planGraph(p);
  assert.deepEqual(
    g.nodes.map((n) => n.id),
    ['import:theme', 'local:./src', 'local:./override.html', 'op:3', 'op:4', 'stage:build', 'op:6', 'stage:more', 'op:10', 'op:11', 'stage:site', 'site:_redirects', 'export'],
  );
  assert.deepEqual(g.nodes.find((n) => n.id === 'op:4'), { id: 'op:4', kind: 'operation', label: 'COPY ./override.html /out/index.html', line: 4 });
  const edges = g.edges.map((e) => `${e.from} -${e.kind}-> ${e.to}`);
  assert.ok(edges.includes('stage:build -base-> op:6'));
  assert.ok(edges.includes('stage:more -input-> op:10'));
  assert.ok(edges.includes('import:theme -input-> op:11'));
  assert.ok(edges.includes('op:11 -next-> stage:site'));
  assert.ok(edges.includes('stage:site -export-> export'));
  assert.ok(edges.includes('site:_redirects -export-> export'));
  assert.match(graphText(g), /^import:theme {2}IMPORT theme \.\/theme AS tree\n/);
  assert.match(graphText(g), /\n13 nodes, 12 edges\n$/);
  const dot = graphDot(g);
  assert.match(dot, /^digraph sitefile \{\n {2}rankdir=LR;\n/);
  assert.match(dot, /\n {2}"stage:more" -> "op:10" \[style=dashed\];\n/);
  assert.match(dot, /\n {2}"op:3" -> "op:4";\n/);
  assert.equal(graphDot({ file: 'x', nodes: [{ id: 'a"b\\', kind: 'local', label: 'q"' }], edges: [] }).split('\n')[3], '  "a\\"b\\\\" [label="q\\"", shape=folder];');

  const t = planGraph(targetPlan(p, 'build'));
  assert.equal(t.target, 'build');
  assert.equal(t.nodes.at(-1)?.label, 'EXPORT / AS directory (--target build)');
  assert.ok(!t.nodes.some((n) => n.kind === 'site'));
});

test('why: every write and overwrite, through COPY --from and FROM <stage>', async () => {
  const { ctx } = await project();
  const r = await build({ contextDir: ctx, lockfile: false, recordSteps: true });
  const index = why(r, '/index.html', ctx);
  assert.equal(index.origin, 'Sitefile:10 COPY'.replace('Sitefile', path.relative(process.cwd(), path.join(ctx, 'Sitefile'))));
  assert.deepEqual(
    index.history.map((e) => [e.stage, e.action, e.path, e.inputs]),
    [
      ['build', 'write', '/out/index.html', ['./src/index.html']],
      ['build', 'overwrite', '/out/index.html', ['./override.html']],
      ['site', 'write', '/index.html', ['stage more:/out/index.html']],
    ],
  );
  const css = why(r, 'style.css', ctx);
  assert.deepEqual(css.history.map((e) => [e.action, e.instruction, e.inputs[0]]), [
    ['write', 'COPY ./src /out/', './src/style.css'],
    ['write', 'COPY --from=more /out /', 'stage more:/out/style.css'],
    ['overwrite', 'COPY theme: /', 'import theme:/style.css'],
  ]);
  assert.match(whyText(css), /^\/style\.css {2}644 {2}4 bytes {2}sha256:[0-9a-f]{64}\n {2}1\. .*Sitefile:3 COPY \.\/src \/out\/ {2}\[stage build\] {2}wrote \/out\/style\.css\n {7}from \.\/src\/style\.css\n/);
  const redirects = why(r, '/_redirects', ctx);
  assert.deepEqual(redirects.history.map((e) => [e.instruction, e.inputs.length]), [['REDIRECT', 1]]);

  const err = (p: string): string => {
    try {
      why(r, p, ctx);
    } catch (e) {
      if (isSitefileError(e)) return `${e.code} ${e.message}`;
    }
    return 'no error';
  };
  assert.equal(err('/nope'), "E_NOT_FOUND '/nope' is not in the build output");
  assert.equal(err('/'), "E_NOT_FOUND '/' is a directory in the build output; name a file");

  // FROM <stage>: the base stage's history comes first.
  const more = await build({ contextDir: ctx, lockfile: false, recordSteps: true, target: 'more' });
  assert.deepEqual(why(more, '/out/index.html', ctx).history.map((e) => e.stage), ['build', 'build']);
});

test('sitefile graph and why on the command line', async () => {
  const { ctx, home } = await project();
  const g = await sitefile(['graph', ctx, '--json'], home);
  assert.equal(g.code, 0, g.stderr);
  const json = JSON.parse(g.stdout);
  assert.deepEqual(Object.keys(json), ['file', 'nodes', 'edges']);
  assert.deepEqual(Object.keys(json.nodes[0]), ['id', 'kind', 'label', 'line']);
  assert.match((await sitefile(['graph', ctx, '--dot'], home)).stdout, /^digraph sitefile \{/);
  assert.match((await sitefile(['graph', ctx], home)).stdout, /\nexport {2}EXPORT \/ AS directory\n/);

  const w = await sitefile(['why', '/index.html', ctx, '--no-lockfile', '--json'], home);
  assert.equal(w.code, 0, w.stderr);
  const report = JSON.parse(w.stdout);
  assert.deepEqual(Object.keys(report), ['path', 'mode', 'size', 'digest', 'origin', 'history']);
  assert.deepEqual(Object.keys(report.history[0]), ['location', 'instruction', 'stage', 'path', 'action', 'digest', 'inputs']);
  assert.equal(report.history.length, 3);

  const missing = await sitefile(['why', '/nope', ctx, '--no-lockfile'], home);
  assert.equal(missing.code, 1);
  assert.equal(missing.stderr, "error[E_NOT_FOUND]: '/nope' is not in the build output\n");
  for (const [args, message] of [
    [['why'], 'why expects a path in the build output: sitefile why <path> [context]'],
    [['graph', ctx, '--dot', '--json'], '--dot and --json conflict'],
    [['inspect', ctx, '--dot'], '--dot is only for graph'],
    [['why', '/a', ctx, 'extra'], 'unexpected arguments: extra'],
  ] as const) {
    const r = await sitefile([...args], home);
    assert.equal(r.code, 2, args.join(' '));
    assert.equal(r.stderr, `error: ${message}\nRun 'sitefile --help' for usage.\n`);
  }
});
