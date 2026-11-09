import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { promises as fs } from 'node:fs';
import { gunzipSync } from 'node:zlib';
import path from 'node:path';
import { writeZip } from '../src/archive-writers.js';
import { closeServer, createSiteServer, listen, type SiteServerOptions } from '../src/server.js';
import { loadArchiveTree, openZipArchive } from '../src/servers.js';
import { makeFile, Tree } from '../src/tree.js';
import { tmpDir } from './helpers/cli.js';

const dirs: string[] = [];
after(async () => {
  for (const d of dirs) await fs.rm(d, { recursive: true, force: true });
});
const enc = (s: string) => new TextEncoder().encode(s);
const site = Tree.fromFiles(
  Object.entries({
    '/index.html': '<h1>home</h1>'.repeat(40),
    '/about/index.html': '<h1>about</h1>',
    '/404.html': 'missing',
    '/_redirects': '/old /about/ 301\n/rewrite /index.html 200\n',
    '/_headers': '/*\n  X-Test: yes\n',
    '/app.js': 'console.log(1);'.repeat(30),
    '/tiny.txt': 'x',
    '/.env': 'hidden',
  }).map(([p, c]) => [p, makeFile(enc(c), 't')] as const),
);

interface Res {
  status: number;
  headers: Record<string, unknown>;
  body: Buffer;
}
/** Where stored entry `name`'s data starts in zip `z` (after its local header's name and extra field). */
function dataOf(z: Buffer, name: string): number {
  const at = z.indexOf(name);
  return at + name.length + z.readUInt16LE(at - 2);
}
function get(port: number, p: string, headers: Record<string, string> = {}, method = 'GET'): Promise<Res> {
  return new Promise((resolve, reject) => {
    const r = http.request({ host: '127.0.0.1', port, path: p, method, headers, agent: false }, (res) => {
      const chunks: Buffer[] = [];
      res.on('data', (c: Buffer) => chunks.push(c));
      res.on('end', () => {
        const { date: _date, ...rest } = res.headers;
        resolve({ status: res.statusCode ?? 0, headers: rest, body: Buffer.concat(chunks) });
      });
    });
    r.on('error', reject);
    r.end();
  });
}
async function serving<T>(tree: Tree, opts: SiteServerOptions, fn: (port: number) => Promise<T>): Promise<T> {
  const server = createSiteServer(tree, opts);
  const { port } = await listen(server, '127.0.0.1', 0);
  try {
    return await fn(port);
  } finally {
    await closeServer(server);
  }
}

test('a zip served in place answers exactly as the extracted zip does, holding no file bytes (#213)', async () => {
  const dir = await tmpDir();
  dirs.push(dir);
  const file = path.join(dir, 'site.zip');
  await fs.writeFile(file, writeZip(site));
  const eager = await loadArchiveTree(file);
  const lazy = await openZipArchive(file);
  assert.ok(lazy?.content);
  try {
    assert.deepEqual(lazy.tree.entries().map(([p, n]) => [p, n.digest, n.mode]), eager.entries().map(([p, n]) => [p, n.digest, n.mode]));
    assert.ok(lazy.tree.entries().every(([, n]) => n.bytes.byteLength === 0));
    const paths = ['/', '/index.html', '/about', '/about/', '/old', '/rewrite', '/app.js', '/tiny.txt', '/nope', '/.env', '/_headers'];
    // Byte ranges and Last-Modified / If-Modified-Since work on entries read from the zip, as on extracted ones.
    const variants: Array<Record<string, string>> = [
      {},
      { range: 'bytes=0-9' },
      { range: 'bytes=5-' },
      { range: 'bytes=9999-' },
      { range: 'bytes=0-9', 'accept-encoding': 'gzip' },
      { 'if-modified-since': 'Tue, 14 Nov 2023 22:13:20 GMT' },
      { 'if-modified-since': 'Mon, 01 Jan 2001 00:00:00 GMT' },
    ];
    const lastModified = (): number => 1_700_000_000;
    for (const method of ['GET', 'HEAD']) {
      for (const h of variants) {
        const want: Res[] = await serving(eager, { lastModified }, (port) => Promise.all(paths.map((p) => get(port, p, h, method))));
        const got: Res[] = await serving(lazy.tree, { content: lazy.content, lastModified }, (port) => Promise.all(paths.map((p) => get(port, p, h, method))));
        assert.deepEqual(got, want, JSON.stringify(h));
      }
    }
  } finally {
    lazy.close();
  }
});

test('deflated zip entries go to gzip clients as gzip, without re-encoding (#213)', async () => {
  const dir = await tmpDir();
  dirs.push(dir);
  const file = path.join(dir, 'site.zip');
  await fs.writeFile(file, writeZip(site));
  const lazy = (await openZipArchive(file))!;
  try {
    await serving(lazy.tree, { content: lazy.content }, async (port) => {
      const plain = await get(port, '/app.js');
      const gz = await get(port, '/app.js', { 'accept-encoding': 'br, gzip;q=0.8' });
      assert.equal(gz.headers['content-encoding'], 'gzip');
      assert.equal(gz.headers['vary'], 'accept-encoding');
      assert.equal(gz.headers['etag'], (plain.headers['etag'] as string).replace(/"$/, '-gzip"'));
      assert.ok(gz.body.length < plain.body.length);
      assert.deepEqual(gunzipSync(gz.body), plain.body);
      assert.equal(Number(gz.headers['content-length']), gz.body.length);
      // 304 for its own ETag; the 404 page is gzipped too when it was deflated (it is stored here: too small).
      assert.equal((await get(port, '/app.js', { 'accept-encoding': 'gzip', 'if-none-match': gz.headers['etag'] as string })).status, 304);
      assert.equal((await get(port, '/nope', { 'accept-encoding': 'gzip' })).headers['content-encoding'], undefined);
      // Stored entries, q=0 and other encodings get the identity response.
      assert.equal((await get(port, '/tiny.txt', { 'accept-encoding': 'gzip' })).headers['content-encoding'], undefined);
      assert.equal((await get(port, '/app.js', { 'accept-encoding': 'gzip;q=0' })).headers['content-encoding'], undefined);
      assert.equal((await get(port, '/app.js', { 'accept-encoding': 'deflate, br' })).headers['content-encoding'], undefined);
    });
  } finally {
    lazy.close();
  }
});

test('a zip changed on disk after the server started is a 500, not wrong content (#213)', async () => {
  const dir = await tmpDir();
  dirs.push(dir);
  const file = path.join(dir, 'site.zip');
  const bytes = Buffer.from(writeZip(site));
  await fs.writeFile(file, bytes);
  const lazy = (await openZipArchive(file))!;
  try {
    // Flip a byte of the stored entry tiny.txt ('x').
    const at = dataOf(bytes, 'tiny.txt');
    assert.equal(bytes[at], 0x78);
    bytes[at] = 0x79;
    await fs.writeFile(file, bytes);
    const lines: string[] = [];
    await serving(lazy.tree, { content: lazy.content, log: (l) => lines.push(l) }, async (port) => {
      assert.equal((await get(port, '/tiny.txt')).status, 500);
    });
    assert.match(lines[0]!, /zip entry 'tiny\.txt' changed since the server started/);
  } finally {
    lazy.close();
  }
});

test('openZipArchive is undefined for other archives and reports zip errors as extraction does', async () => {
  const dir = await tmpDir();
  dirs.push(dir);
  const tar = path.join(dir, 'x.tar');
  await fs.writeFile(tar, Buffer.alloc(1024));
  assert.equal(await openZipArchive(tar), undefined);
  const bad = path.join(dir, 'bad.zip');
  const z = Buffer.from(writeZip(site));
  z[dataOf(z, 'tiny.txt')] = 0x79;
  await fs.writeFile(bad, z);
  await assert.rejects(openZipArchive(bad), /zip entry 'tiny\.txt' fails its CRC-32 check/);
  await assert.rejects(loadArchiveTree(bad), /zip entry 'tiny\.txt' fails its CRC-32 check/);
});
