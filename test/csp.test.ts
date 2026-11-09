// CSP: per-page Content-Security-Policy from the hashes of inline scripts and styles.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { cspHash } from '../src/csp.js';
import { inlineBlocks } from '../src/links.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { cspMeta, cspPolicy, pagePatterns } from '../src/site.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(files: Record<string, string>) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-csp-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  const warnings: string[] = [];
  const r = await build({ contextDir: dir, lockfile: false, onWarning: (m) => warnings.push(m) });
  return { ...r, printed: warnings.map((m) => m.replace(/[^\s']*Sitefile:/g, 'Sitefile:')) };
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

test('inlineBlocks, hashes and policies', () => {
  const b = inlineBlocks('<!-- <script>x</script> --><HEAD lang=en><Script type=module>a()</SCRIPT ><script src=x.js>ignored</script><style>p{}</style><textarea><script>no</script></textarea><script></script>');
  assert.deepEqual(b.scripts, ['a()', '']);
  assert.deepEqual(b.styles, ['p{}']);
  assert.equal(b.headEnd, 41);
  assert.equal(inlineBlocks('<p>').headEnd, undefined);
  assert.equal(cspHash('a()'), 'sha256-' + createHash('sha256').update('a()').digest('base64'));
  assert.equal(cspPolicy("default-src 'self'", [], []), "default-src 'self'");
  assert.equal(cspPolicy(" default-src 'self' ;; SCRIPT-SRC 'self'  ", ['sha256-a', 'sha256-a'], ['sha256-b']), "default-src 'self'; SCRIPT-SRC 'self' 'sha256-a'; style-src 'self' 'sha256-b'");
  assert.deepEqual(pagePatterns('/index.html'), ['/', '/index.html']);
  assert.deepEqual(pagePatterns('/a/b.html'), ['/a/b.html', '/a/b']);
  assert.deepEqual(pagePatterns('/a.htm'), ['/a.htm']);
  assert.equal(cspMeta(`a "b" & 'c'`), `<meta http-equiv="Content-Security-Policy" content="a &quot;b&quot; &amp; 'c'">`);
});

test('parse and plan: CSP', () => {
  const [c] = parseSitefile(`csp policy "default-src 'none'" meta`);
  assert.ok(c?.kind === 'CSP' && c.meta && c.policy);
  assert.throws(() => parseSitefile('CSP META POLICY x'), /unexpected 'POLICY'/);
  assert.deepEqual(plan(parseProgram('FROM scratch\nCSP\nEXPORT /')).csp, { policy: "default-src 'self'; base-uri 'self'; object-src 'none'", meta: false, span: { file: 'Sitefile', line: 2, column: 1 } });
  assert.throws(() => plan(parseProgram('FROM scratch\nCSP\nCSP META\nEXPORT /')), /CSP given twice \(line 2/);
  assert.throws(() => plan(parseProgram('FROM scratch\nCSP\nHEADER /* Content-Security-Policy: x\nEXPORT /')), /both set Content-Security-Policy/);
  assert.throws(() => plan(parseProgram('FROM scratch\nCSP POLICY "a, b"\nEXPORT /')), /without control characters or commas/);
  assert.equal(plan(parseProgram('FROM scratch\nCSP\nEXPORT /'), {}).csp?.meta, false);
});

test('build: headers per page and META', async () => {
  const files = {
    'public/index.html': '<head><script>go()</script><style>a{}</style></head>',
    'public/docs/guide.html': '<p>no head</p>',
  };
  const h = await run({ ...files, Sitefile: 'COPY ./public/ /\nCSP\nEXPORT /\n' });
  const p = `default-src 'self'; base-uri 'self'; object-src 'none'`;
  const index = `${p}; script-src 'self' '${cspHash('go()')}'; style-src 'self' '${cspHash('a{}')}'`;
  assert.equal(
    text(h.output.get('/_headers')?.bytes),
    `/docs/guide.html\n  Content-Security-Policy: ${p}\n/docs/guide\n  Content-Security-Policy: ${p}\n/\n  Content-Security-Policy: ${index}\n/index.html\n  Content-Security-Policy: ${index}\n`,
  );
  assert.match(h.output.get('/_headers')!.origin, /Sitefile:2 CSP$/);
  const m = await run({ ...files, Sitefile: 'COPY ./public/ /\nCSP META\nEXPORT /\n' });
  assert.equal(text(m.output.get('/index.html')?.bytes), `<head>${cspMeta(index)}<script>go()</script><style>a{}</style></head>`);
  assert.equal(m.output.get('/_headers'), undefined);
  assert.deepEqual(m.printed, ['Sitefile:2 CSP: 1 page has no <head>, so no policy was added: /docs/guide.html']);
});
