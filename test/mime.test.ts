// The content-type table of serve and Web Bundle export (johnhenry/sitefile#179).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { MIME_TYPES } from '../src/mime-data.js';
import { DEFAULT_ARCHIVE_LIMITS } from '../src/resources/types.js';
import { readWebBundle } from '../src/resources/wbn-reader.js';
import { makeFile, Tree } from '../src/tree.js';
import { mimeTypeFor, writeWebBundle } from '../src/wbn.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');

test('src/mime-data.ts and rust/src/mime_data.rs are generated from tools/gen-mime.mjs and hold the same entries', () => {
  const r = spawnSync(process.execPath, [path.join(root, 'tools', 'gen-mime.mjs'), '--check'], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  const rust = readFileSync(path.join(root, 'rust', 'src', 'mime_data.rs'), 'utf8');
  const rows = [...rust.matchAll(/^ {4}\("([^"]+)", "([^"]+)"\),$/gm)].map((m) => [m[1], m[2]]);
  assert.deepEqual(rows, MIME_TYPES.map(([e, t]) => [e, t]));
  const exts = MIME_TYPES.map(([e]) => e);
  assert.deepEqual(exts, [...exts].sort(), 'sorted (the Rust lookup is a binary search)');
});

test('mimeTypeFor: common static types, charset on text types, case-insensitive, octet-stream otherwise', () => {
  const cases: Array<[string, string]> = [
    ['/captions/en.vtt', 'text/vtt; charset=utf-8'],
    ['/sitemap.xsl', 'application/xml; charset=utf-8'],
    ['/feed.atom', 'application/atom+xml; charset=utf-8'],
    ['/feed.rss', 'application/rss+xml; charset=utf-8'],
    ['/app.cjs', 'text/javascript; charset=utf-8'],
    ['/app.js.map', 'application/json; charset=utf-8'],
    ['/f.ttf', 'font/ttf'],
    ['/f.eot', 'application/vnd.ms-fontobject'],
    ['/v.m3u8', 'application/vnd.apple.mpegurl'],
    ['/a.OGG', 'audio/ogg'],
    ['/m.glb', 'model/gltf-binary'],
    ['/s.wbn', 'application/webbundle'],
    ['/data.csv', 'text/csv; charset=utf-8'],
    ['/index.html', 'text/html; charset=utf-8'],
    ['/blob.xyz', 'application/octet-stream'],
    ['/noext', 'application/octet-stream'],
  ];
  for (const [p, want] of cases) assert.equal(mimeTypeFor(p), want, p);
});

test('Web Bundle export records the same content types', () => {
  const tree = Tree.fromFiles([
    ['/index.html', makeFile(new TextEncoder().encode('<h1>x</h1>'), 't')],
    ['/en.vtt', makeFile(new TextEncoder().encode('WEBVTT\n'), 't')],
    ['/font.otf', makeFile(new TextEncoder().encode('OTTO'), 't')],
  ]);
  const b = readWebBundle(writeWebBundle(tree, 'https://example.com/'), 'test', DEFAULT_ARCHIVE_LIMITS);
  const type = (u: string) => b.entries.find((e) => e.url === u)?.headers['content-type'];
  assert.equal(type('https://example.com/en.vtt'), 'text/vtt; charset=utf-8');
  assert.equal(type('https://example.com/font.otf'), 'font/otf');
});
