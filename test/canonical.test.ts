// CANONICAL links and split sitemaps (docs/LANGUAGE.md, "Site files").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { canonical, canonicalUrl } from '../src/canonical.js';
import { canonicalSlot } from '../src/links.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { sitemapFiles, sitemapPart } from '../src/site.js';
import { makeFile, Tree } from '../src/tree.js';

const tree = (files: Record<string, string>): Tree => Tree.fromFiles(Object.entries(files).map(([p, t]) => [p, makeFile(new TextEncoder().encode(t), 'test')]));
const text = (t: Tree, p: string): string => new TextDecoder().decode(t.get(p)!.bytes);
const span = { file: 'Sitefile', line: 1, column: 1 };

test('canonicalSlot: </head>, an existing canonical link, comments and raw text', () => {
  assert.equal(canonicalSlot('<head><title>a</title></head>'), 22);
  assert.equal(canonicalSlot('<head><link rel="alternate canonical" href=x></head>'), 'has');
  assert.equal(canonicalSlot('<head><link rel=stylesheet href=x></HEAD >'), 34);
  assert.equal(canonicalSlot('<!-- </head> --><script></head></script></head>'), 40);
  assert.equal(canonicalSlot('</header></head>'), 9);
  assert.equal(canonicalSlot('<p>x'), undefined);
  assert.equal(canonicalSlot('</head><link rel=canonical href=x>'), 0);
});

test('canonicalUrl and the CANONICAL step', () => {
  assert.equal(canonicalUrl('https://e.x', '/a/index.html', false), 'https://e.x/a/');
  assert.equal(canonicalUrl('https://e.x', '/a b.html', true), 'https://e.x/a%20b');
  assert.equal(canonicalUrl('https://e.x', '/index.html', true), 'https://e.x/');
  const warnings: string[] = [];
  const out = canonical(
    tree({ '/www/a.html': '<head></head>', '/www/404.html': '<head></head>', '/x.html': '<head></head>', '/www/b.htm': '<p>', '/www/c.html': '<head><link rel=canonical href=/c></head>' }),
    { base: 'https://e.x', pretty: false, root: '/www', span },
    'Sitefile:1 CANONICAL',
    (m) => warnings.push(m),
  );
  assert.equal(text(out, '/www/a.html'), '<head><link rel="canonical" href="https://e.x/a.html"></head>');
  assert.equal(text(out, '/www/404.html'), '<head></head>');
  assert.equal(text(out, '/x.html'), '<head></head>', 'outside the EXPORT path');
  assert.equal(text(out, '/www/c.html'), '<head><link rel=canonical href=/c></head>');
  assert.deepEqual(warnings, ['Sitefile:1 CANONICAL: 1 page has no </head>, so no canonical link was added: /www/b.htm']);
  assert.equal(out.get('/www/a.html')!.origin, 'Sitefile:1 CANONICAL');
});

test('parse and plan: CANONICAL', () => {
  const [c] = parseSitefile('canonical base https://e.x/ pretty');
  assert.ok(c?.kind === 'CANONICAL' && c.pretty);
  assert.throws(() => parseSitefile('CANONICAL'), /expected BASE/);
  assert.throws(() => parseSitefile('CANONICAL BASE https://e.x x'), /unexpected 'x'/);
  const p = plan(parseProgram('FROM scratch\nCANONICAL BASE https://e.x/\nEXPORT /www'));
  assert.deepEqual(p.stages[0]!.operations[0], { kind: 'CANONICAL', base: 'https://e.x', pretty: false, root: '/www', span: { file: 'Sitefile', line: 2, column: 1 } });
  assert.throws(() => plan(parseProgram('FROM scratch\nCANONICAL BASE e.x\nEXPORT /')), /CANONICAL BASE 'e.x' must be an absolute http\(s\) URL/);
});

/** The split for limits of 2 URLs (shared with rust/src/site.rs tests: same expected text). */
export const SPLIT_EXPECTED = [
  ['/maps/site.xml', '<?xml version="1.0" encoding="UTF-8"?>\n<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n  <sitemap><loc>https://e.x/maps/site-1.xml</loc></sitemap>\n  <sitemap><loc>https://e.x/maps/site-2.xml</loc></sitemap>\n</sitemapindex>\n'],
  ['/maps/site-1.xml', '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n  <url><loc>https://e.x/a.html</loc></url>\n  <url><loc>https://e.x/b/</loc></url>\n</urlset>\n'],
  ['/maps/site-2.xml', '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n  <url><loc>https://e.x/</loc></url>\n</urlset>\n'],
];

test('sitemapFiles: one file within the limits, else an index and parts', () => {
  const t = tree({ '/index.html': '', '/a.html': '', '/b/index.html': '', '/404.html': '', '/x.css': '' });
  assert.equal(sitemapFiles(t, 'https://e.x', '/sitemap.xml').length, 1);
  assert.deepEqual(sitemapFiles(t, 'https://e.x', '/maps/site.xml', { urls: 2, bytes: 1_000_000 }), SPLIT_EXPECTED);
  // By bytes: each part holds what fits.
  const byBytes = sitemapFiles(t, 'https://e.x', '/s.xml', { urls: 100, bytes: 150 });
  assert.deepEqual(byBytes.map(([p]) => p), ['/s.xml', '/s-1.xml', '/s-2.xml', '/s-3.xml']);
  assert.equal(sitemapPart('/sitemap.xml', 3), '/sitemap-3.xml');
  assert.equal(sitemapPart('/maps/sitemap', 1), '/maps/sitemap-1');
  assert.equal(sitemapPart('/a.b/map', 1), '/a.b/map-1');
});
