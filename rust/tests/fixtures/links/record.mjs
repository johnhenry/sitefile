// What the TypeScript reference scanner (src/links.ts) does with one HTML or CSS text, as plain JSON.
// Shared by generate.mjs (which writes links.json) and test/links-fixture.test.ts (which checks it).
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export async function loadLib(root) {
  const imp = (p) => import(pathToFileURL(path.join(root, 'lib/src', p)).href);
  const [links, { hashedName }] = await Promise.all([imp('links.js'), imp('fingerprint.js')]);
  return { links, hashedName };
}

function targetText(t) {
  if (t.kind === 'none') return 'none';
  if (t.kind === 'external') return `external ${t.url}`;
  return `internal ${t.path}${t.dir && t.path !== '/' ? '/' : ''}`;
}

/** { kind, input, refs: [[line, raw, url, css, target, target with root /a]], rewritten, rewrittenBelow } (see generate.mjs). */
export function record(lib, kind, input) {
  const { links, hashedName } = lib;
  const from = kind === 'html' ? '/a/b.html' : '/a/b.css';
  const refs = kind === 'html' ? links.scanHtml(input) : links.scanCss(input);
  const rename = (p) => hashedName(p, '0123456789');
  return {
    kind,
    input,
    refs: refs.map((r) => [r.line, r.raw, r.url, r.css, targetText(links.resolveReference(r.url, from)), targetText(links.resolveReference(r.url, from, '/a'))]),
    rewritten: links.rewriteReferences(input, refs, from, '/', rename) ?? null,
    rewrittenBelow: links.rewriteReferences(input, refs, from, '/a', rename) ?? null,
  };
}
