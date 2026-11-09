// LANGUAGES: hreflang alternates in pages (CANONICAL) and sitemaps (docs/LANGUAGE.md, "Site files").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { canonical } from '../src/canonical.js';
import { alternates, isLanguageTag, type Languages } from '../src/hreflang.js';
import { headLinks } from '../src/links.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { sitemapFiles } from '../src/site.js';
import { makeFile, Tree } from '../src/tree.js';

const tree = (files: Record<string, string>): Tree => Tree.fromFiles(Object.entries(files).map(([p, t]) => [p, makeFile(new TextEncoder().encode(t), 'test')]));
const text = (t: Tree, p: string): string => new TextDecoder().decode(t.get(p)!.bytes);
const span = { file: 'Sitefile', line: 1, column: 1 };
const langs = (codes: string[], dflt?: string, docs: Record<string, { lang: string; key: string }> = {}): Languages => ({ codes, ...(dflt ? { default: dflt } : {}), docs: new Map(Object.entries(docs)) });

test('isLanguageTag: a basic BCP 47 check', () => {
  for (const ok of ['en', 'fr', 'pt-BR', 'zh-Hant-TW', 'es-419', 'EN']) assert.ok(isLanguageTag(ok), ok);
  for (const bad of ['', 'e', 'english', 'en_US', 'en-', 'x-default', 'en-toolongtag', '1a']) assert.ok(!isLanguageTag(bad), bad);
});

test('headLinks: existing canonical and hreflang links before </head>', () => {
  assert.deepEqual(headLinks('<head><link rel=alternate hreflang=fr href=/fr/></head>'), { at: 48, canonical: false, hreflang: true });
  assert.deepEqual(headLinks('<head><link rel="ALTERNATE canonical" href=/x></head>'), { at: 46, canonical: true, hreflang: false });
  assert.deepEqual(headLinks('<head><link rel=alternate type=application/rss+xml href=/feed></head>'), { at: 62, canonical: false, hreflang: false });
  assert.deepEqual(headLinks('<head></head><link rel=alternate hreflang=fr href=/fr/>'), { at: 6, canonical: false, hreflang: false });
  assert.deepEqual(headLinks('<p><link rel=canonical href=x>'), { at: undefined, canonical: true, hreflang: false });
});

test('alternates: folders, DEFAULT, translationKey groups and duplicates', () => {
  const pages = (...ps: string[]): Array<[string, string]> => ps.map((p) => [p, p]);
  const l = langs(['en', 'fr'], 'en', { '/blog/b.html': { lang: 'fr', key: 'k' }, '/blog/a.html': { lang: 'en', key: 'k' } });
  const out = alternates(pages('/fr/x.html', '/x.html', '/blog/a.html', '/blog/b.html', '/only.html'), l, 'SITEMAP', span);
  assert.deepEqual(out.get('/x.html'), [['en', '/x.html'], ['fr', '/fr/x.html'], ['x-default', '/x.html']]);
  assert.deepEqual(out.get('/blog/b.html'), [['en', '/blog/a.html'], ['fr', '/blog/b.html'], ['x-default', '/blog/a.html']]);
  assert.equal(out.has('/only.html'), false, 'a page alone in its group has no alternates');
  // Without DEFAULT, pages outside the language folders have no language, and there is no x-default.
  const plain = alternates(pages('/en/a.html', '/fr/a.html', '/a.html'), langs(['en', 'fr']), 'SITEMAP', span);
  assert.deepEqual(plain.get('/a.html'), undefined);
  assert.deepEqual(plain.get('/fr/a.html'), [['en', '/en/a.html'], ['fr', '/fr/a.html']]);
  assert.throws(
    () => alternates(pages('/en/x.html', '/x.html'), l, 'CANONICAL', span),
    /CANONICAL: \/en\/x.html and \/x.html are both the 'en' page of '\/x.html'; a translation group has one page per language/,
  );
});

test('CANONICAL with languages: alternates next to the canonical link, existing ones kept', () => {
  const t = tree({
    '/www/en/a.html': '<head></head>',
    '/www/fr/a.html': '<head><link rel=canonical href=/x></head>',
    '/www/de/a.html': '<head><link rel=alternate hreflang=de href=/x></head>',
    '/www/en/b.html': '<head></head>',
  });
  const out = canonical(t, { base: 'https://e.x', pretty: true, root: '/www', span }, 'Sitefile:1 CANONICAL', () => {}, langs(['en', 'fr', 'de'], 'en'));
  const alts = '<link rel="alternate" hreflang="de" href="https://e.x/de/a"><link rel="alternate" hreflang="en" href="https://e.x/en/a"><link rel="alternate" hreflang="fr" href="https://e.x/fr/a"><link rel="alternate" hreflang="x-default" href="https://e.x/en/a">';
  assert.equal(text(out, '/www/en/a.html'), `<head><link rel="canonical" href="https://e.x/en/a">${alts}</head>`);
  assert.equal(text(out, '/www/fr/a.html'), `<head><link rel=canonical href=/x>${alts}</head>`);
  assert.equal(text(out, '/www/de/a.html'), '<head><link rel=alternate hreflang=de href=/x><link rel="canonical" href="https://e.x/de/a"></head>');
  assert.equal(text(out, '/www/en/b.html'), '<head><link rel="canonical" href="https://e.x/en/b"></head>');
});

/** The split with alternates for limits of 2 URLs (shared with rust/src/site.rs tests: same expected text). */
export const SPLIT_ALTERNATES_EXPECTED = [
  ['/s.xml', '<?xml version="1.0" encoding="UTF-8"?>\n<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n  <sitemap><loc>https://e.x/s-1.xml</loc></sitemap>\n  <sitemap><loc>https://e.x/s-2.xml</loc></sitemap>\n</sitemapindex>\n'],
  [
    '/s-1.xml',
    '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">\n  <url><loc>https://e.x/a.html</loc></url>\n  <url><loc>https://e.x/en/</loc><xhtml:link rel="alternate" hreflang="en" href="https://e.x/en/"/><xhtml:link rel="alternate" hreflang="fr" href="https://e.x/fr/"/></url>\n</urlset>\n',
  ],
  [
    '/s-2.xml',
    '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">\n  <url><loc>https://e.x/fr/</loc><xhtml:link rel="alternate" hreflang="en" href="https://e.x/en/"/><xhtml:link rel="alternate" hreflang="fr" href="https://e.x/fr/"/></url>\n</urlset>\n',
  ],
];

test('sitemapFiles with alternates: xhtml:link elements and namespace, also when split', () => {
  const t = tree({ '/en/index.html': '', '/fr/index.html': '', '/a.html': '' });
  const alts = alternates(['/en/index.html', '/fr/index.html', '/a.html'].map((p) => [p, p] as const), langs(['en', 'fr']), 'SITEMAP', span);
  assert.deepEqual(sitemapFiles(t, 'https://e.x', '/s.xml', { urls: 2, bytes: 1_000_000 }, alts), SPLIT_ALTERNATES_EXPECTED);
  const one = sitemapFiles(t, 'https://e.x', '/s.xml', undefined, new Map())[0]![1];
  assert.ok(one.startsWith('<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n'), 'no alternates: the plain urlset');
});

test('parse and plan: LANGUAGES', () => {
  const [l] = parseSitefile('languages en ${B} default en');
  assert.ok(l?.kind === 'LANGUAGES' && l.codes.length === 2 && l.default);
  assert.throws(() => parseSitefile('LANGUAGES'), /expected a language code/);
  assert.throws(() => parseSitefile('LANGUAGES en DEFAULT en fr'), /unexpected 'fr'/);
  const p = plan(parseProgram('FROM scratch\nIMPORT posts FROM ./posts AS markdown\nCANONICAL BASE https://e.x\nRENDER ./t.html TO /${p.slug}.html FOR p IN posts\nSITEMAP BASE https://e.x\nLANGUAGES en fr DEFAULT fr\nEXPORT /'));
  const [c, , s] = p.stages[0]!.operations;
  assert.ok(c?.kind === 'CANONICAL' && s?.kind === 'SITEMAP');
  assert.deepEqual(c.languages && { ...c.languages }, { codes: ['en', 'fr'], default: 'fr', sources: [] }, 'a RENDER after CANONICAL is not one of its sources');
  assert.deepEqual(s.languages?.sources.map((x) => [x.as, x.list]), [['p', 'posts']]);
  assert.equal('languages' in plan(parseProgram('FROM scratch\nCANONICAL BASE https://e.x\nEXPORT /')).stages[0]!.operations[0]!, false, 'no LANGUAGES: no languages field');
  assert.throws(() => plan(parseProgram('FROM scratch\nLANGUAGES en english\nEXPORT /')), /LANGUAGES: 'english' is not a language code/);
  assert.throws(() => plan(parseProgram('FROM scratch\nLANGUAGES en Fr fr\nEXPORT /')), /LANGUAGES: 'fr' is listed twice/);
  assert.throws(() => plan(parseProgram('FROM scratch\nLANGUAGES en DEFAULT fr\nEXPORT /')), /'fr' is not one of the languages listed \(en\)/);
  assert.throws(() => plan(parseProgram('FROM scratch\nLANGUAGES en\nLANGUAGES fr\nEXPORT /')), /LANGUAGES given twice \(line 2/);
});
