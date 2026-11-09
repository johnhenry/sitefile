// ROBOTS and SECURITY_TXT (docs/LANGUAGE.md, "Site files").
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build } from '../src/executor.js';
import { isSitefileError } from '../src/errors.js';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { robotsTxt, securityTxt } from '../src/site.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function run(sitefile: string) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-robots-'));
  temps.push(dir);
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, 'Sitefile'), sitefile);
  return build({ contextDir: dir, lockfile: false, onWarning: () => {} });
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

test('robotsTxt and securityTxt', () => {
  assert.equal(robotsTxt([], []), 'User-agent: *\nDisallow:\n');
  assert.equal(robotsTxt([{ allow: false, path: '/a/' }, { allow: true, path: '/a/b' }], ['https://e.x/s.xml']), 'User-agent: *\nDisallow: /a/\nAllow: /a/b\n\nSitemap: https://e.x/s.xml\n');
  assert.equal(
    securityTxt({ contact: ['mailto:a@e.x', 'tel:+1'], expires: '2099-01-01T00:00:00Z', encryption: ['https://e.x/k'], policy: ['https://e.x/p'] }),
    'Contact: mailto:a@e.x\nContact: tel:+1\nExpires: 2099-01-01T00:00:00Z\nEncryption: https://e.x/k\nPolicy: https://e.x/p\n',
  );
});

test('parse: ROBOTS and SECURITY_TXT forms and errors', () => {
  const [r] = parseSitefile('robots --overwrite disallow /a allow /b sitemap to /r.txt');
  assert.ok(r?.kind === 'ROBOTS' && r.overwrite && r.rules.length === 2 && r.sitemap && r.dest);
  assert.throws(() => parseSitefile('ROBOTS SITEMAP DISALLOW /a'), /unexpected 'DISALLOW' \(expected ALLOW, DISALLOW, SITEMAP or TO, in this order\)/);
  assert.throws(() => parseSitefile('ROBOTS ALLOW'), /expected path/);
  const [s] = parseSitefile('SECURITY_TXT EXPIRES 2099-01-01 CONTACT mailto:a@e.x CONTACT https://e.x');
  assert.ok(s?.kind === 'SECURITY_TXT' && s.contact.length === 2);
  assert.throws(() => parseSitefile('SECURITY_TXT CONTACT mailto:a@e.x'), (e: unknown) => isSitefileError(e) && e.code === 'E_VALIDATION' && /requires an Expires field/.test(e.detail));
  assert.throws(() => parseSitefile('SECURITY_TXT EXPIRES 2099-01-01'), /requires at least one Contact field/);
  assert.throws(() => parseSitefile('SECURITY_TXT CONTACT x EXPIRES a EXPIRES b'), /EXPIRES given twice/);
  assert.throws(() => parseSitefile('SECURITY_TXT CONTACT x BOGUS'), /unexpected 'BOGUS'/);
});

test('plan: SITEMAP URLs, path, URI and date checks; near expiry warns', () => {
  const p = plan(parseProgram('FROM scratch\nROBOTS DISALLOW /x SITEMAP\nSITEMAP BASE https://e.x/\nSITEMAP BASE https://e.x TO /m/b.xml\nSITEMAP BASE https://e.x\nEXPORT /'));
  const op = p.stages[0]!.operations[0]!;
  assert.ok(op.kind === 'ROBOTS');
  assert.equal(op.text, 'User-agent: *\nDisallow: /x\n\nSitemap: https://e.x/sitemap.xml\nSitemap: https://e.x/m/b.xml\n');
  assert.throws(() => plan(parseProgram('FROM scratch\nROBOTS SITEMAP\nEXPORT /')), /the Sitefile has no SITEMAP to point to/);
  assert.throws(() => plan(parseProgram('FROM scratch\nROBOTS ALLOW a\nEXPORT /')), /ROBOTS ALLOW path 'a' must be an absolute path/);
  assert.throws(() => plan(parseProgram('FROM scratch\nSECURITY_TXT CONTACT ftp://x EXPIRES 2099-01-01\nEXPORT /')), /CONTACT 'ftp:\/\/x' must be a mailto:, tel:, https:\/\/… URI/);
  assert.throws(() => plan(parseProgram('FROM scratch\nSECURITY_TXT CONTACT mailto:a@e.x POLICY http://e.x EXPIRES 2099-01-01\nEXPORT /')), /POLICY 'http:\/\/e.x'/);
  assert.throws(() => plan(parseProgram('FROM scratch\nSECURITY_TXT CONTACT mailto:a@e.x EXPIRES P1Y\nEXPORT /')), /durations are not accepted/);
  const soon = new Date(Date.now() + 5 * 86400_000).toISOString().slice(0, 10);
  const w = plan(parseProgram(`FROM scratch\nSECURITY_TXT CONTACT mailto:a@e.x EXPIRES ${soon}\nEXPORT /`)).warnings;
  assert.ok(w.some((m) => m.includes('is less than 30 days away')), w.join('\n'));
  const past = plan(parseProgram('FROM scratch\nSECURITY_TXT CONTACT mailto:a@e.x EXPIRES 2001-01-01\nEXPORT /')).warnings;
  assert.ok(past.some((m) => m.includes('is in the past')), past.join('\n'));
});

test('build: robots.txt and .well-known/security.txt', async () => {
  const r = await run('FROM scratch\nROBOTS\nSECURITY_TXT CONTACT mailto:sec@e.x EXPIRES 2099-12-31T23:59:59Z ENCRYPTION openpgp4fpr:AB12\nEXPORT /\n');
  assert.equal(text(r.output.get('/robots.txt')?.bytes), 'User-agent: *\nDisallow:\n');
  assert.equal(text(r.output.get('/.well-known/security.txt')?.bytes), 'Contact: mailto:sec@e.x\nExpires: 2099-12-31T23:59:59Z\nEncryption: openpgp4fpr:AB12\n');
  assert.match(r.output.get('/robots.txt')!.origin, /Sitefile:2 ROBOTS$/);
});
