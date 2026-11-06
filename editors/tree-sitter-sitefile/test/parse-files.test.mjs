// Every Sitefile in the repository (examples/ and conformance/cases/) must
// parse without ERROR or MISSING nodes, except the ones the real parser
// rejects with E_SYNTAX, and the ones whose inline RENDER template has a tag
// that src/render.ts rejects with E_TEMPLATE (the grammar parses template tags,
// so it reports those too). Both lists are below; when the TypeScript sources
// have been built (`npm run build` in the repository root), they are checked
// against the real parser and template parser so they cannot drift.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { findSitefiles, grammarDir, parseOk, repoRoot } from './helpers.mjs';

/** Rejected by src/language/parser.ts with E_SYNTAX (relative to the repository root). */
const EXPECTED_SYNTAX_ERRORS = [
  'conformance/cases/err-compression/Sitefile', // COMPRESSION zstd: the grammar accepts any word here
  'conformance/cases/err-from-missing-base/Sitefile',
  'conformance/cases/err-redirect-status/Sitefile', // STATUS 200: the grammar accepts any word here
  'conformance/cases/err-secret-reserved/Sitefile', // SECRET … AS HEADER Content-Length: the grammar accepts any header name
  'conformance/cases/err-sitemap-syntax/Sitefile',
  'conformance/cases/err-stage-missing-base/Sitefile',
  'conformance/cases/err-stage-missing-name/Sitefile',
  'conformance/cases/err-stage-unexpected/Sitefile',
  'conformance/cases/err-syntax-directive-late/Sitefile', // a `# sitefile=` comment after an instruction: the grammar allows a directive anywhere
  'conformance/cases/err-syntax-version-invalid/Sitefile', // `# sitefile = 1.1`: the grammar accepts any directive value
  'conformance/cases/err-syntax/Sitefile',
];

/** An inline RENDER template with an invalid tag (E_TEMPLATE in src/render.ts). */
const EXPECTED_TEMPLATE_ERRORS = ['conformance/cases/err-template/Sitefile'];

/** E_SYNTAX files the grammar is lenient about (the parser checks a value word). */
const LENIENT = new Set([
  'conformance/cases/err-compression/Sitefile',
  'conformance/cases/err-redirect-status/Sitefile',
  'conformance/cases/err-secret-reserved/Sitefile',
  'conformance/cases/err-syntax-directive-late/Sitefile',
  'conformance/cases/err-syntax-version-invalid/Sitefile',
]);

const repoFiles = [...findSitefiles(path.join(repoRoot, 'examples')), ...findSitefiles(path.join(repoRoot, 'conformance/cases'))];
// Our fixtures, plus the VS Code grammar's (when present): the same rules, two grammars.
const fixtureDirs = [path.join(grammarDir, 'test/fixtures'), path.join(repoRoot, 'editors/vscode/test/fixtures')].filter((d) => existsSync(d));
const fixtures = fixtureDirs.flatMap((d) =>
  readdirSync(d)
    .filter((f) => f.endsWith('.Sitefile'))
    .map((f) => path.join(d, f)),
);
const rel = (p) => path.relative(repoRoot, p).split(path.sep).join('/');
const results = parseOk([...repoFiles, ...fixtures]);

test('finds the repository Sitefiles', () => {
  assert.ok(repoFiles.length >= 80, `only ${repoFiles.length} Sitefiles found`);
});

test('examples and conformance cases parse without errors', () => {
  const expected = [...EXPECTED_SYNTAX_ERRORS, ...EXPECTED_TEMPLATE_ERRORS];
  const failed = repoFiles.filter((f) => !expected.includes(rel(f)) && results.get(f) !== true);
  assert.deepEqual(failed.map(rel), []);
});

test('files with syntax errors produce ERROR or MISSING nodes', () => {
  for (const f of [...EXPECTED_SYNTAX_ERRORS, ...EXPECTED_TEMPLATE_ERRORS]) {
    if (LENIENT.has(f)) continue;
    assert.equal(results.get(path.join(repoRoot, f)), false, `${f} parsed without errors`);
  }
});

test('fixtures: *.Sitefile parse cleanly, invalid-*.Sitefile do not', () => {
  for (const f of fixtures) {
    const invalid = path.basename(f).startsWith('invalid-');
    assert.equal(results.get(f), !invalid, `${path.basename(f)}: expected ${invalid ? 'errors' : 'a clean parse'}`);
  }
});

const builtParser = path.join(repoRoot, 'lib/src/language/parser.js');
test('the E_SYNTAX list matches the TypeScript parser', { skip: !existsSync(builtParser) && 'run `npm run build` in the repository root' }, async () => {
  const { parseSitefile } = await import(pathToFileURL(builtParser).href);
  const syntax = [];
  for (const f of repoFiles) {
    try {
      parseSitefile(readFileSync(f, 'utf8'), f);
    } catch (err) {
      if (err?.code === 'E_SYNTAX') syntax.push(rel(f));
    }
  }
  assert.deepEqual(syntax.sort(), [...EXPECTED_SYNTAX_ERRORS].sort());
});

test('the E_TEMPLATE list matches the template parser', { skip: !existsSync(builtParser) && 'run `npm run build` in the repository root' }, async () => {
  const { parseSitefile } = await import(pathToFileURL(builtParser).href);
  const { parseRenderTemplate } = await import(pathToFileURL(path.join(repoRoot, 'lib/src/render.js')).href);
  const template = [];
  for (const f of repoFiles) {
    let instructions;
    try {
      instructions = parseSitefile(readFileSync(f, 'utf8'), f);
    } catch {
      continue;
    }
    for (const i of instructions) {
      if (i.kind !== 'RENDER' || i.template?.kind !== 'inline') continue;
      try {
        parseRenderTemplate(i.template.text, f, i.template.bodyLine);
      } catch (err) {
        if (err?.code === 'E_TEMPLATE') template.push(rel(f));
      }
    }
  }
  assert.deepEqual([...new Set(template)].sort(), [...EXPECTED_TEMPLATE_ERRORS].sort());
});

test('fixtures that parse cleanly are accepted by the TypeScript parser', { skip: !existsSync(builtParser) && 'run `npm run build` in the repository root' }, async () => {
  const { parseSitefile } = await import(pathToFileURL(builtParser).href);
  for (const f of fixtures.filter((f) => !path.basename(f).startsWith('invalid-'))) {
    assert.doesNotThrow(() => parseSitefile(readFileSync(f, 'utf8'), f), path.basename(f));
  }
});
