// sitefile up / down / ps and the servers file (issue #223).
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import { writeZip } from '../src/archive-writers.js';
import { isSitefileError } from '../src/errors.js';
import { parseServersDefinition } from '../src/servers-file.js';
import { makeFile, Tree } from '../src/tree.js';
import { makeSite, runCli, tmpDir } from './helpers/cli.js';

const homes: string[] = [];
after(async () => {
  for (const h of homes) {
    const r = await runCli(h, ['servers', '-a', '--json']);
    try {
      for (const s of JSON.parse(r.stdout) as Array<{ id: string }>) await runCli(h, ['server', 'rm', '-f', s.id]);
    } catch {
      /* nothing to clean */
    }
  }
});

async function home(): Promise<string> {
  const h = await tmpDir();
  homes.push(h);
  return h;
}

/** A project directory: two site folders and a servers file. */
async function project(yaml: string): Promise<string> {
  const dir = await tmpDir('sitefile-up-');
  for (const [d, body] of [['docs', 'docs page'], ['app', 'app page']] as const) {
    await fs.mkdir(path.join(dir, d));
    await fs.writeFile(path.join(dir, d, 'index.html'), body);
  }
  await fs.writeFile(path.join(dir, 'sitefile.servers.yaml'), yaml);
  return dir;
}

const urlOf = (stdout: string, name: string): string => {
  const m = new RegExp(`${name} \\([0-9a-f]{12}\\)[^\\n]* at (http://\\S+)`).exec(stdout);
  assert.ok(m, stdout);
  return m[1] as string;
};

interface Row {
  name: string;
  site: string;
  url: string;
  status: string;
  id: string | null;
  listed: boolean;
}
const psJson = async (h: string, cwd: string): Promise<Row[]> => JSON.parse((await runCli(h, ['ps', '--json'], cwd)).stdout) as Row[];

test('the servers file: shape, names, publish and spa', () => {
  const f = parseServersDefinition({ servers: { b: { source: './b', publish: 8081, spa: true }, a: { source: 'img:1', publish: '0.0.0.0:80' } } }, 'f.yaml', '/d');
  assert.deepEqual(f.servers, [
    { name: 'b', source: './b', publish: '8081', spa: true },
    { name: 'a', source: 'img:1', publish: '0.0.0.0:80', spa: false },
  ]);
  for (const [v, msg] of [
    [[], 'f.yaml: a servers file must be a mapping with a servers key'],
    [{ x: 1 }, "f.yaml: unknown key 'x' (expected servers)"],
    [{ servers: {} }, 'f.yaml: servers must be a non-empty mapping of name: server'],
    [{ servers: { '-a': { source: 'x' } } }, "f.yaml: '-a' is not a valid server name (letters, digits, '_', '.', '-'; at most 63)"],
    [{ servers: { '12': { source: 'x' } } }, "f.yaml: '12' is not a valid server name here: use at least one letter"],
    [{ servers: { a: 'x' } }, "f.yaml: server 'a' must be a mapping with a source"],
    [{ servers: { a: { source: 'x', port: 1 } } }, "f.yaml: server 'a': unknown key 'port' (expected source, publish, spa)"],
    [{ servers: { a: {} } }, "f.yaml: server 'a': source must be a non-empty string"],
    [{ servers: { a: { source: 'x', publish: 1.5 } } }, "f.yaml: server 'a': publish must be [host:]port (a string or a port number)"],
    [{ servers: { a: { source: 'x', spa: 'yes' } } }, "f.yaml: server 'a': spa must be true or false"],
  ] as const) {
    assert.throws(() => parseServersDefinition(v, 'f.yaml', '/d'), (e: unknown) => isSitefileError(e, 'E_DATA') && e.detail === msg, msg);
  }
});

test('up starts every server, is idempotent, restarts exited ones, replaces changed ones; ps and down', async () => {
  const h = await home();
  const dir = await project('servers:\n  docs:\n    source: ./docs\n  app:\n    source: ./app/\n    spa: true\n');
  // Before up: not created.
  assert.deepEqual(
    (await psJson(h, dir)).map((r) => [r.name, r.site, r.status, r.id]),
    [
      ['docs', './docs', 'not created', null],
      ['app', './app/', 'not created', null],
    ],
  );
  let r = await runCli(h, ['up'], dir);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, new RegExp(`^Started docs \\([0-9a-f]{12}\\) serving ${path.join(dir, 'docs').replace(/[./]/g, '\\$&')} at http://127\\.0\\.0\\.1:\\d+/\\nStarted app `));
  const docs = urlOf(r.stdout, 'docs');
  assert.equal(await (await fetch(docs)).text(), 'docs page');
  assert.equal(await (await fetch(urlOf(r.stdout, 'app') + 'some/route')).text(), 'app page'); // spa
  // Again: nothing changes.
  r = await runCli(h, ['up'], dir);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /^docs \([0-9a-f]{12}\) is up to date at http:\/\/127\.0\.0\.1:\d+\/\napp \([0-9a-f]{12}\) is up to date at /);
  assert.equal(urlOf(r.stdout, 'docs'), docs);
  let rows = await psJson(h, dir);
  assert.deepEqual(rows.map((x) => [x.name, x.status, x.listed]), [
    ['docs', 'running', true],
    ['app', 'running', true],
  ]);
  // A stopped server is restarted with the same ID.
  const docsId = rows[0]?.id as string;
  assert.equal((await runCli(h, ['server', 'stop', 'docs'])).code, 0);
  r = await runCli(h, ['up'], dir);
  assert.match(r.stdout, new RegExp(`^Restarted docs \\(${docsId}\\) serving `));
  // A changed entry is replaced; a removed one is reported, kept, and listed as not in the file.
  await fs.writeFile(path.join(dir, 'sitefile.servers.yaml'), 'servers:\n  docs:\n    source: ./app\n');
  r = await runCli(h, ['up'], dir);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /^Recreated docs \([0-9a-f]{12}\) serving /);
  assert.notEqual(new RegExp('docs \\(([0-9a-f]{12})\\)').exec(r.stdout)?.[1], docsId);
  assert.equal(await (await fetch(urlOf(r.stdout, 'docs'))).text(), 'app page');
  assert.match(r.stderr, /^warning: app was started from sitefile\.servers\.yaml but is no longer in it; "sitefile down" stops it\n$/);
  rows = await psJson(h, dir);
  assert.deepEqual(rows.map((x) => [x.name, x.status, x.listed]), [
    ['docs', 'running', true],
    ['app', 'running', false],
  ]);
  const text = (await runCli(h, ['ps'], dir)).stdout;
  assert.match(text, /^NAME {2}SITE +URL +STATUS +ID\n/);
  assert.match(text, /\napp .* running \(not in the file\) {2}[0-9a-f]{12}\n$/);
  // down removes both (the orphan too); a server started otherwise is untouched.
  const other = await runCli(h, ['serve', '-d', '--name', 'other', path.join(dir, 'docs')]);
  assert.equal(other.code, 0, other.stderr);
  r = await runCli(h, ['down'], dir);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /^Removed docs \([0-9a-f]{12}\)\nRemoved app \([0-9a-f]{12}\)\n$/);
  await assert.rejects(fetch(docs));
  const left = JSON.parse((await runCli(h, ['servers', '-a', '--json'])).stdout) as Array<{ name: string; status: string }>;
  assert.deepEqual(left.map((s) => [s.name, s.status]), [['other', 'running']]);
  assert.deepEqual((await psJson(h, dir)).map((x) => x.status), ['not created']);
  // down with nothing to do.
  r = await runCli(h, ['down'], dir);
  assert.deepEqual([r.code, r.stdout], [0, '']);
});

test('up refuses a name it did not start, and starts nothing when an entry is bad', async () => {
  const h = await home();
  const dir = await project('servers:\n  docs:\n    source: ./docs\n  app:\n    source: ./app\n');
  assert.equal((await runCli(h, ['serve', '-d', '--name', 'app', path.join(dir, 'app')])).code, 0);
  let r = await runCli(h, ['up'], dir);
  assert.equal(r.code, 1);
  assert.equal(r.stderr, `error[E_SERVER]: sitefile.servers.yaml: a server named 'app' already exists and was not started from this file; remove it with "sitefile rm app"\n`);
  assert.deepEqual((await psJson(h, dir)).map((x) => x.status), ['not created', 'not created']);
  // A missing directory, a bad publish: nothing starts.
  for (const [yaml, err] of [
    ['servers:\n  docs:\n    source: ./docs\n  gone:\n    source: ./gone\n', /^error\[E_NOT_FOUND\]: sitefile\.servers\.yaml: server 'gone': '.*\/gone' is not a directory or a file\n$/],
    ['servers:\n  docs:\n    source: ./docs\n  bad:\n    source: ./app\n    publish: nope\n', /^error\[E_DATA\]: sitefile\.servers\.yaml: server 'bad': publish: -p expects \[host:\]port, got 'nope'\n$/],
    ['servers:\n  docs:\n    source: nosuchimage:1\n', /^error\[E_NOT_FOUND\]: sitefile\.servers\.yaml: server 'docs': /],
  ] as const) {
    await fs.writeFile(path.join(dir, 'sitefile.servers.yaml'), yaml);
    r = await runCli(h, ['up'], dir);
    assert.equal(r.code, 1, yaml);
    assert.match(r.stderr, err);
    assert.equal(r.stdout, '');
  }
  const all = JSON.parse((await runCli(h, ['servers', '-a', '--json'])).stdout) as Array<{ name: string }>;
  assert.deepEqual(all.map((s) => s.name), ['app']);
  // No file at all.
  const empty = await tmpDir('sitefile-up-');
  r = await runCli(h, ['ps'], empty);
  assert.equal(r.code, 1);
  assert.equal(r.stderr, 'error[E_NOT_FOUND]: no servers file: sitefile.servers.yaml, sitefile.servers.yml, sitefile.servers.json not found in the current directory (or pass -f)\n');
  r = await runCli(h, ['up', '-f', path.join(empty, 'x.toml')]);
  assert.equal(r.code, 2);
});

test('-f: sources are relative to the file; JSON files; images and archive files by name', async () => {
  const h = await home();
  const dir = await project('');
  const ctx = await makeSite();
  assert.equal((await runCli(h, ['build', ctx, '-t', 'site:1', '--no-lockfile'])).code, 0);
  await fs.writeFile(path.join(dir, 'site.zip'), writeZip(Tree.fromFiles([['/index.html', makeFile(new TextEncoder().encode('zipped'), 't')]])));
  const file = path.join(dir, 'servers.json');
  await fs.writeFile(file, JSON.stringify({ servers: { img: { source: 'site:1' }, zip: { source: 'site.zip' }, docs: { source: 'docs' } } }));
  const elsewhere = await tmpDir('sitefile-cwd-');
  // 'docs' is not path-like, is no image, and is a directory, not a file: refused like serve does.
  let r = await runCli(h, ['up', '-f', file], elsewhere);
  assert.equal(r.code, 1);
  assert.match(r.stderr, /^error\[E_NOT_FOUND\]: \S*servers\.json: server 'docs': /);
  await fs.writeFile(file, JSON.stringify({ servers: { img: { source: 'site:1' }, zip: { source: 'site.zip' }, docs: { source: './docs' } } }));
  r = await runCli(h, ['up', '-f', file], elsewhere);
  assert.equal(r.code, 0, r.stderr);
  assert.match(r.stdout, /^Started img \([0-9a-f]{12}\) serving site:1 at /);
  assert.match(r.stdout, new RegExp(`\\nStarted zip \\([0-9a-f]{12}\\) serving ${path.join(dir, 'site.zip').replace(/[./]/g, '\\$&')} at `));
  assert.equal(await (await fetch(urlOf(r.stdout, 'zip'))).text(), 'zipped');
  assert.equal(await (await fetch(urlOf(r.stdout, 'docs'))).text(), 'docs page');
  assert.match(await (await fetch(urlOf(r.stdout, 'img'))).text(), /<html|Home|index/i);
  // The same file by another path is the same project.
  r = await runCli(h, ['ps', '-f', path.relative(elsewhere, file), '--json'], elsewhere);
  assert.deepEqual((JSON.parse(r.stdout) as Row[]).map((x) => x.status), ['running', 'running', 'running']);
  assert.equal((await runCli(h, ['down', '-f', file])).code, 0);
});
