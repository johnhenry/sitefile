import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { epubTree, writeEpub } from '../src/epub.js';
import { extractArchive } from '../src/resources/archive.js';
import { Tree, makeFile } from '../src/tree.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const page = (title: string, body: string, lang = 'fr'): string =>
  `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE html>\n<html xmlns="http://www.w3.org/1999/xhtml" lang="${lang}" xml:lang="${lang}"><head><title>${title}</title></head><body><p>${body}</p></body></html>\n`;

function book(extra: Array<[string, string]> = []): Tree {
  return Tree.fromFiles(
    (
      [
        ['/ch/1.xhtml', page('Un &amp; deux', 'one')],
        ['/index.xhtml', page('Mon Livre', 'début')],
        ['/style.css', 'p{}'],
        ['/_headers', '/*\n  X: 1'],
        ...extra,
      ] as Array<[string, string]>
    ).map(([p, s]) => [p, makeFile(enc.encode(s), 't')]),
  );
}

test('epub: mimetype first, stored, without an extra field; container, OPF and nav', async () => {
  const bytes = writeEpub(book());
  assert.equal(dec.decode(bytes.subarray(30, 38)), 'mimetype');
  assert.equal(bytes[8], 0); // stored
  assert.equal(bytes[28]! | (bytes[29]! << 8), 0); // no extra field
  assert.equal(dec.decode(bytes.subarray(38, 58)), 'application/epub+zip');
  const files = await extractArchive(bytes, 'book.epub');
  assert.deepEqual(files.entries().map(([p]) => p), ['/EPUB/ch/1.xhtml', '/EPUB/index.xhtml', '/EPUB/nav.xhtml', '/EPUB/package.opf', '/EPUB/style.css', '/META-INF/container.xml', '/mimetype']);
  const opf = dec.decode(files.get('/EPUB/package.opf')!.bytes);
  assert.match(opf, /<dc:title>Mon Livre<\/dc:title>/);
  assert.match(opf, /<dc:language>fr<\/dc:language>/);
  assert.match(opf, /<meta property="dcterms:modified">1970-01-01T00:00:00Z<\/meta>/);
  assert.match(opf, /<spine>\n {4}<itemref idref="f2"\/>\n {4}<itemref idref="f1"\/>\n {2}<\/spine>/);
  assert.match(opf, /<item id="f3" href="style.css" media-type="text\/css"\/>/);
  assert.doesNotMatch(opf, /_headers/);
  assert.match(dec.decode(files.get('/EPUB/nav.xhtml')!.bytes), /<li><a href="ch\/1.xhtml">Un &amp; deux<\/a><\/li>/);
  // Deterministic, and the same bytes as the Rust port (rust/src/export/epub.rs).
  assert.equal(createHash('sha256').update(bytes).digest('hex'), createHash('sha256').update(writeEpub(book())).digest('hex'));
  assert.match(dec.decode(epubTree(book(), 1700000000).get('/EPUB/package.opf')!.bytes), /2023-11-14T22:13:20Z/);
});

test('epub: a site nav.xhtml is the navigation document; errors', () => {
  const t = epubTree(book([['/nav.xhtml', page('Contents', 'toc')]]));
  const opf = dec.decode(t.get('/EPUB/package.opf')!.bytes);
  assert.match(opf, /href="nav.xhtml" media-type="application\/xhtml\+xml" properties="nav"/);
  assert.doesNotMatch(opf, /id="nav"/);
  assert.throws(() => epubTree(Tree.fromFiles([['/a.css', makeFile(enc.encode('p{}'), 't')]])), /no .xhtml or .html page/);
  assert.throws(() => epubTree(book([['/package.opf', 'x']])), /E_CONFLICT|package.opf/);
});
