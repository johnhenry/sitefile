// COPY --exclude, --chmod and --parents (docs/LANGUAGE.md, COPY).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { link, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { parseSitefile } from '../src/language/parser.js';
import { build, type BuildResult } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

const FILES: Record<string, string> = {
  'src/index.html': '<h1>hi</h1>',
  'src/a.md': '# a',
  'src/css/site.css': 'body{}',
  'src/css/draft.css': 'x{}',
  'src/drafts/b.html': 'draft',
  'src/node_modules/m/index.js': 'js',
  'tools/run.sh': '#!/bin/sh',
};

async function context(files: Record<string, string> = FILES): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-copy-flags-'));
  temps.push(dir);
  for (const [p, body] of Object.entries(files)) {
    await mkdir(path.dirname(path.join(dir, p)), { recursive: true });
    await writeFile(path.join(dir, p), body);
  }
  return dir;
}

async function run(sitefile: string, dir?: string, warnings: string[] = []): Promise<BuildResult> {
  const ctx = dir ?? (await context());
  await writeFile(path.join(ctx, 'Sitefile'), sitefile);
  return build({ contextDir: ctx, lockfile: false, onWarning: (m) => warnings.push(m) });
}

const paths = (r: BuildResult): string[] => r.output.entries().map(([p]) => p);
const modes = (r: BuildResult): Record<string, string> => Object.fromEntries(r.output.entries().map(([p, n]) => [p, n.mode.toString(8)]));

function fails(fn: () => unknown, code: ErrorCode, line: number, column: number, message?: RegExp): void {
  assert.throws(fn, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.deepEqual([e.span?.line, e.span?.column], [line, column], e.message);
    if (message) assert.match(e.detail, message);
    return true;
  });
}

async function rejects(p: Promise<unknown>, code: ErrorCode, message?: RegExp): Promise<void> {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    if (message) assert.match(e.detail, message);
    return true;
  });
}

test('COPY flags parse into the IR', () => {
  const [ins] = parseSitefile('COPY --exclude=*.md --exclude=drafts/ --chmod=0755 --parents --overwrite ./src /out/');
  assert.ok(ins?.kind === 'COPY');
  assert.deepEqual(ins.exclude, ['*.md', 'drafts/']);
  assert.equal(ins.chmod, 0o755);
  assert.equal(ins.parents, true);
  assert.equal(ins.overwrite, true);
  const [plain] = parseSitefile('COPY ./src /');
  assert.ok(plain?.kind === 'COPY');
  assert.ok(!('exclude' in plain) && !('chmod' in plain) && !('parents' in plain));
  for (const [mode, value] of [['644', 0o644], ['0644', 0o644], ['755', 0o755]] as const) {
    const [c] = parseSitefile(`COPY --chmod=${mode} a /b`);
    assert.ok(c?.kind === 'COPY' && c.chmod === value);
  }
});

test('COPY flag syntax errors', () => {
  fails(() => parseSitefile('COPY --chmod=600 a /b'), 'E_SYNTAX', 1, 6, /--chmod must be 644 or 755/);
  fails(() => parseSitefile('COPY --chmod=constructor a /b'), 'E_SYNTAX', 1, 6, /--chmod must be 644 or 755/);
  fails(() => parseSitefile('COPY --chmod=644 --chmod=644 a /b'), 'E_SYNTAX', 1, 18, /--chmod given twice/);
  fails(() => parseSitefile('COPY --chmod a /b'), 'E_SYNTAX', 1, 6, /--chmod requires a value/);
  fails(() => parseSitefile('COPY --exclude a /b'), 'E_SYNTAX', 1, 6, /--exclude requires a value/);
  fails(() => parseSitefile('COPY --exclude=#x a /b'), 'E_SYNTAX', 1, 6, /--exclude=#x is not a pattern/);
  fails(() => parseSitefile('COPY --exclude=/ a /b'), 'E_SYNTAX', 1, 6, /is not a pattern/);
  fails(() => parseSitefile('COPY --parents=1 a /b'), 'E_SYNTAX', 1, 6, /--parents takes no value/);
  fails(() => parseSitefile('COPY --parents --parents a /b'), 'E_SYNTAX', 1, 16, /given twice/);
  fails(() => parseSitefile('COPY --exclude=*.md <<EOF /b\nx\nEOF'), 'E_SYNTAX', 1, 6, /--exclude cannot be combined with an inline/);
  fails(() => parseSitefile('COPY --parents <<EOF /b\nx\nEOF'), 'E_SYNTAX', 1, 6, /--parents cannot be combined with an inline/);
  // --chmod works on an inline body; the flags are COPY-only.
  assert.equal(parseSitefile('COPY --chmod=755 <<EOF /run.sh\nx\nEOF').length, 1);
  fails(() => parseSitefile('RENDER --exclude=x t TO /x'), 'E_SYNTAX', 1, 8, /unknown flag --exclude/);
});

test('--exclude filters a directory with .sitefileignore rules, relative to the source', async () => {
  const r = await run('COPY --exclude=*.md --exclude=drafts/ --exclude=node_modules --exclude=/css/draft.css ./src /\nEXPORT /');
  assert.deepEqual(paths(r), ['/css/site.css', '/index.html']);
  // Negation, anchoring and ** work as in .sitefileignore; an excluded directory is not re-entered.
  const n = await run('COPY --exclude=**/*.css --exclude=!site.css --exclude=drafts --exclude=!drafts/b.html ./src/ /s/\nEXPORT /');
  assert.deepEqual(paths(n), ['/s/a.md', '/s/css/site.css', '/s/index.html', '/s/node_modules/m/index.js']);
});

test('--exclude skips directories without reading them (hard links in node_modules)', async () => {
  const dir = await context({ 'site/index.html': 'x', 'store/pkg.js': 'js' });
  await mkdir(path.join(dir, 'site/node_modules'));
  await link(path.join(dir, 'store/pkg.js'), path.join(dir, 'site/node_modules/pkg.js'));
  await rejects(run('COPY ./site /\nEXPORT /', dir), 'E_HARDLINK');
  assert.deepEqual(paths(await run('COPY --exclude=node_modules/ ./site /\nEXPORT /', dir)), ['/index.html']);
});

test('--exclude on imports and --from stages; a file source is an error', async () => {
  const r = await run(
    [
      'IMPORT src FROM ./src',
      'FROM scratch AS a',
      'COPY ./src /www/',
      'FROM scratch',
      'COPY --exclude=*.css --exclude=node_modules src:/ /i/',
      'COPY --from=a --exclude=drafts/ --exclude=node_modules --exclude=*.md /www/ /s/',
      'EXPORT /',
    ].join('\n'),
  );
  assert.deepEqual(paths(r), ['/i/a.md', '/i/drafts/b.html', '/i/index.html', '/s/css/draft.css', '/s/css/site.css', '/s/index.html']);
  await rejects(run('COPY --exclude=*.md ./src/a.md /\nEXPORT /'), 'E_TYPE', /--exclude filters a directory, but '\.\/src\/a\.md' is a file/);
});

test('--exclude that leaves nothing warns', async () => {
  const warnings: string[] = [];
  const r = await run('COPY --exclude=* ./src /\nEXPORT /', undefined, warnings);
  assert.deepEqual(paths(r), []);
  assert.ok(warnings.some((w) => w.endsWith("--exclude leaves no files of './src' to copy")), warnings.join('\n'));
});

test('--chmod sets the mode of every copied file', async () => {
  const r = await run(
    ['COPY --chmod=755 ./tools/run.sh /bin/', 'COPY --chmod=0644 ./src/css /css/', 'COPY --chmod=755 <<EOF /hello.sh', 'echo hi', 'EOF', 'EXPORT /'].join('\n'),
  );
  assert.deepEqual(modes(r), { '/bin/run.sh': '755', '/css/draft.css': '644', '/css/site.css': '644', '/hello.sh': '755' });
  // The mode is part of a write: the same bytes with another mode are a conflict.
  await rejects(run('COPY ./tools/run.sh /r.sh\nCOPY --chmod=755 ./tools/run.sh /r.sh\nEXPORT /'), 'E_CONFLICT');
});

test('--parents keeps the source path under the destination', async () => {
  const r = await run(
    [
      'IMPORT src FROM ./src',
      'FROM scratch AS a',
      'COPY ./src/css/site.css /x/y.css',
      'FROM scratch',
      'COPY --parents ./src/css/site.css /out',
      'COPY --parents ./src/drafts /out/',
      'COPY --parents src:/css/draft.css /imp/',
      'COPY --parents --from=a /x/y.css /st/',
      'COPY --parents ./tools/ /',
      'EXPORT /',
    ].join('\n'),
  );
  assert.deepEqual(paths(r), ['/imp/css/draft.css', '/out/src/css/site.css', '/out/src/drafts/b.html', '/st/x/y.css', '/tools/run.sh']);
  // With the whole context (or a whole import) there is no path to keep.
  const root = await run('IMPORT src FROM ./src\nCOPY --parents --exclude=node_modules --exclude=src/ ./ /c/\nCOPY --parents --exclude=* --exclude=!index.html src: /i/\nEXPORT /');
  assert.deepEqual(paths(root), ['/c/Sitefile', '/c/tools/run.sh', '/i/index.html']);
});
