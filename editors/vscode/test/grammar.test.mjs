// Snapshot and targeted tests for syntaxes/sitefile.tmLanguage.json.
// UPDATE_SNAPSHOTS=1 npm test rewrites test/snapshots/*.snap.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { buildGrammar, buildInjection } from '../scripts/grammar.mjs';
import { scopesOf, tokenize } from './tokenize.mjs';

const fixtures = new URL('./fixtures/', import.meta.url);
const snapshots = new URL('./snapshots/', import.meta.url);
const read = (name) => readFileSync(new URL(name, fixtures), 'utf8');
const files = readdirSync(fixtures).filter((f) => f.endsWith('.Sitefile')).sort();

test('committed grammar JSON matches scripts/grammar.mjs (run npm run build:grammar)', () => {
  const syntaxes = new URL('../syntaxes/', import.meta.url);
  assert.deepEqual(JSON.parse(readFileSync(new URL('sitefile.tmLanguage.json', syntaxes), 'utf8')), buildGrammar());
  assert.deepEqual(JSON.parse(readFileSync(new URL('sitefile-template.injection.tmLanguage.json', syntaxes), 'utf8')), buildInjection());
});

for (const file of files) {
  test(`snapshot: ${file}`, async () => {
    const result = await tokenize(read(file));
    const snap = Object.fromEntries(result.map((l) => [String(l.line), { text: l.text, tokens: l.tokens }]));
    const path = new URL(file.replace(/\.Sitefile$/, '.snap'), snapshots);
    if (process.env.UPDATE_SNAPSHOTS || !existsSync(path)) {
      writeFileSync(path, JSON.stringify(snap, null, 1) + '\n');
      return;
    }
    assert.deepEqual(snap, JSON.parse(readFileSync(path, 'utf8')), `${file} tokens changed; rerun with UPDATE_SNAPSHOTS=1 if intended`);
  });
}

// The fixtures (other than invalid-*) must be real Sitefiles, so the snapshots
// show how valid input is highlighted. Needs the root `npm run build`.
const parserJs = new URL('../../../lib/src/language/parser.js', import.meta.url);
test('valid fixtures parse with the Sitefile parser', { skip: !existsSync(parserJs) && 'root lib/ not built' }, async () => {
  const { parseSitefile } = await import(parserJs.href);
  for (const file of files.filter((f) => !f.startsWith('invalid-'))) {
    assert.doesNotThrow(() => parseSitefile(read(file), file), file);
  }
});

const has = (scopes, part) => {
  assert.ok(scopes, `token not found (wanted ${part})`);
  assert.ok(scopes.split(' ').some((s) => s.startsWith(part)), `${scopes} has no ${part}`);
};
const hasNot = (scopes, part) => assert.ok(!scopes?.includes(part), `${scopes} should not have ${part}`);

test('instructions are case-insensitive and may be indented', async () => {
  const r = await tokenize('copy ./a /b\n  Render ./t TO /x\nrun x\nWHATEVER x\n');
  has(scopesOf(r, 1, 'copy'), 'keyword.control.instruction');
  has(scopesOf(r, 2, 'Render'), 'keyword.control.instruction');
  has(scopesOf(r, 3, 'run'), 'invalid.illegal.unsupported-instruction');
  assert.equal(scopesOf(r, 4, 'WHATEVER x'), '');
});

test('# is a comment only as the first non-blank character', async () => {
  const r = await tokenize('  # note\nLET U = https://x.test/#frag\n');
  has(r[0].tokens.at(-1)[1], 'comment.line.number-sign');
  for (const [, s] of r[1].tokens) hasNot(s, 'comment');
});

test('continuations keep the instruction open, also across comments and blank lines', async () => {
  const r = await tokenize('IMPORT a FROM ./a \\\n  # c\n\n  AS json\nAS\n');
  has(scopesOf(r, 1, '\\'), 'constant.character.escape.line-continuation');
  has(r[1].tokens.at(-1)[1], 'comment.line');
  has(scopesOf(r, 4, 'json'), 'support.type.import-kind');
  // The instruction ended on line 4, so line 5 is not a clause keyword.
  assert.equal(scopesOf(r, 5, 'AS'), '');
});

test('heredoc bodies embed a language chosen by the delimiter and end exactly at the delimiter', async () => {
  const r = await tokenize('RENDER <<HTML TO /i.html\n<div class="x\nHTML\nCOPY <<CSS /s.css\nHTML\nCSS\nEXPORT /\n');
  has(scopesOf(r, 1, 'TO'), 'keyword.other.clause');
  has(r[1].tokens[0][1], 'meta.embedded.block.html');
  has(scopesOf(r, 3, 'HTML'), 'entity.name.tag.heredoc');
  hasNot(scopesOf(r, 3, 'HTML'), 'meta.tag.stub'); // the unclosed `<div` did not swallow it
  has(scopesOf(r, 5, 'HTML'), 'meta.embedded.block.css'); // not COPY's delimiter
  has(scopesOf(r, 7, 'EXPORT'), 'keyword.control.instruction');
});

test('<<- strips tabs (not spaces) before the closing line', async () => {
  const r = await tokenize('COPY <<-EOF /a\n\tbody\n  EOF\n\tEOF\nEXPORT /\n');
  has(scopesOf(r, 3, '  EOF'), 'string.unquoted.heredoc');
  has(scopesOf(r, 4, 'EOF'), 'entity.name.tag.heredoc');
  has(scopesOf(r, 5, 'EXPORT'), 'keyword.control.instruction');
  const plain = await tokenize('COPY <<EOF /a\n\tEOF\nEOF\n');
  has(scopesOf(plain, 2, '\tEOF'), 'string.unquoted.heredoc');
});

test('RENDER bodies are templates; COPY and TRANSFORM bodies are raw', async () => {
  const r = await tokenize('RENDER <<T TO /a\n${x.y|raw} $${z}\nT\nCOPY <<T /b\n${x}\nT\nTRANSFORM <<T WITH p TO /c\n${x}\nT\n');
  has(scopesOf(r, 2, 'raw'), 'support.function.filter');
  has(scopesOf(r, 2, 'y'), 'variable.other.property');
  has(scopesOf(r, 2, '$${'), 'constant.character.escape.template');
  hasNot(r[4].tokens[0][1], 'interpolation');
  hasNot(r[7].tokens[0][1], 'interpolation');
});

test('template substitutions are highlighted inside embedded strings (injection)', async () => {
  const r = await tokenize('RENDER <<HTML TO /a\n<a href="${url}">\nHTML\n');
  has(scopesOf(r, 2, 'url'), 'variable.other.readwrite');
});

test('template blocks', async () => {
  const r = await tokenize('RENDER <<T TO /a\n${#each posts as p}${#if !p.draft}${else}${/if}${/each}${a + b}\nT\n');
  has(scopesOf(r, 2, '#each'), 'keyword.control.loop');
  has(scopesOf(r, 2, 'p'), 'variable.other.definition.loop');
  has(scopesOf(r, 2, '!'), 'keyword.operator.logical');
  has(scopesOf(r, 2, 'else'), 'keyword.control.template');
  has(scopesOf(r, 2, '/each'), 'keyword.control.template');
  has(scopesOf(r, 2, '${a + b}'), 'invalid.illegal.template');
});

test('FROM image <ref> [AT <dir>]: image is a keyword only when a reference follows', async () => {
  const r = await tokenize(
    [
      'FROM image ghcr.io/me/theme:${V} AT /srv/www AS theme',
      'STAGE site FROM IMAGE theme:1 AT /x',
      'FROM image AS other',
      'FROM image',
    ].join('\n'),
  );
  has(scopesOf(r, 1, 'image'), 'keyword.other.clause');
  has(scopesOf(r, 1, 'V'), 'variable.other.readwrite');
  has(scopesOf(r, 1, 'AT'), 'keyword.other.clause');
  has(scopesOf(r, 1, 'theme'), 'entity.name.section.stage');
  has(scopesOf(r, 2, 'IMAGE'), 'keyword.other.clause');
  has(scopesOf(r, 2, 'AT'), 'keyword.other.clause');
  // a stage (or directory) named image
  has(scopesOf(r, 3, 'image'), 'entity.name.section.stage');
  has(scopesOf(r, 4, 'image'), 'entity.name.section.stage');
});

test('operands: strings, escapes, interpolation, integrity, flags and values', async () => {
  const r = await tokenize(
    [
      `LET A = "x\\"\${B.c}" 'lit \${no}'`,
      'IMPORT d FROM https://h.test/${P} INTEGRITY sha256:' + 'a'.repeat(64),
      'IMPORT e FROM ./e INTEGRITY sha256:BAD',
      'RENDER --escape=html --overwrite --from=base --bogus ./t TO /o',
      'EXPORT / AS tar.gz BASE https://x.test/ AT /srv COMPRESSION none',
      'ARG N: number = 3',
    ].join('\n'),
  );
  has(scopesOf(r, 1, '\\"'), 'constant.character.escape');
  has(scopesOf(r, 1, 'c'), 'variable.other.property');
  has(scopesOf(r, 1, "lit ${no}"), 'string.quoted.single');
  has(scopesOf(r, 2, 'P'), 'variable.other.readwrite');
  has(scopesOf(r, 2, 'sha256:' + 'a'.repeat(64)), 'constant.other.integrity');
  has(scopesOf(r, 3, 'sha256:BAD'), 'invalid.illegal.integrity');
  has(scopesOf(r, 4, 'html'), 'constant.language.escape-mode');
  has(scopesOf(r, 4, 'overwrite'), 'variable.parameter.flag');
  has(scopesOf(r, 4, 'base'), 'entity.name.section.stage');
  has(scopesOf(r, 4, '--bogus'), 'invalid.illegal.unknown-flag');
  has(scopesOf(r, 5, 'tar.gz'), 'support.type.export-format');
  has(scopesOf(r, 5, 'BASE'), 'keyword.other.clause');
  has(scopesOf(r, 5, 'none'), 'constant.language.compression');
  has(scopesOf(r, 6, 'number'), 'support.type.primitive');
  has(scopesOf(r, 6, '3'), 'constant.numeric');
});

test('# sitefile=N is highlighted as the version directive', async () => {
  const r = await tokenize('# sitefile=1\n  #SiteFile = 2 \n# sitefile=1.0\n# sitefile: 1\nCOPY a /b\n');
  has(scopesOf(r, 1, 'sitefile'), 'keyword.other.directive');
  has(scopesOf(r, 1, '1'), 'constant.numeric.version');
  has(scopesOf(r, 2, 'SiteFile'), 'keyword.other.directive');
  has(scopesOf(r, 2, '2'), 'constant.numeric.version');
  has(scopesOf(r, 3, '1.0'), 'invalid.illegal.version');
  for (const [, s] of r[3].tokens) hasNot(s, 'directive');
});

test('COPY --exclude, --chmod and --parents', async () => {
  const r = await tokenize('COPY --exclude=*.md --exclude=drafts/ --chmod=755 --parents ./src /out/\nCOPY --chmod=600 a /b\n');
  has(scopesOf(r, 1, 'exclude'), 'variable.parameter.flag');
  has(scopesOf(r, 1, '*.md'), 'string.unquoted.glob');
  has(scopesOf(r, 1, 'drafts/'), 'string.unquoted.glob');
  has(scopesOf(r, 1, '755'), 'constant.numeric.mode');
  has(scopesOf(r, 1, 'parents'), 'variable.parameter.flag');
  has(scopesOf(r, 2, '600'), 'invalid.illegal.mode');
});

test('# sitefile-lint: comments are highlighted as directives', async () => {
  const r = await tokenize('# sitefile-lint: disable=unused-name,render-escape\n# sitefile-lint: disable-next-line=all\n# sitefile-linter: x\n');
  has(scopesOf(r, 1, 'sitefile-lint'), 'keyword.other.directive');
  has(scopesOf(r, 1, 'disable'), 'keyword.other.directive');
  has(scopesOf(r, 1, 'unused-name,render-escape'), 'constant.other.rule');
  has(scopesOf(r, 2, 'disable-next-line'), 'keyword.other.directive');
  for (const [, s] of r[2].tokens) hasNot(s, 'directive');
});

test('${file …} helpers: path, helper, attributes; ${file} alone is a variable', async () => {
  const r = await tokenize('RENDER --inline-svg <<HTML TO /i.html\n<p>${file "/img/${x}.jpg" | img alt="a}b" class=hero hidden} ${file} ${file /a | bogus}</p>\nHTML\n');
  has(scopesOf(r, 1, 'inline-svg'), 'variable.parameter.flag');
  has(scopesOf(r, 2, 'file'), 'support.function.file');
  has(scopesOf(r, 2, 'img'), 'support.function.helper');
  has(scopesOf(r, 2, 'alt'), 'entity.other.attribute-name.helper');
  has(scopesOf(r, 2, 'x'), 'variable.other.readwrite');
  has(scopesOf(r, 2, 'hero'), 'string.unquoted');
  has(scopesOf(r, 2, 'hidden'), 'entity.other.attribute-name.helper');
  has(scopesOf(r, 2, 'bogus'), 'invalid.illegal.helper');
  has(scopesOf(r, 3, 'HTML'), 'entity.name.tag.heredoc');
});

test('html fragments: LET name: html = … is a template line; LET name: html <<NAME a template heredoc', async () => {
  const r = await tokenize('LET nav: html = <a href="${p.url}">${#each xs as x}${x}${/each}</a>\nLET h: HTML <<T\n<b>${SITE}</b>\nT\nLET c: html = <<-T\n\t${c2}\n\tT\nLET plain = <<EOF\nARG a: html\nEXPORT /\n');
  has(scopesOf(r, 1, 'LET'), 'keyword.control.instruction');
  has(scopesOf(r, 1, 'html'), 'support.type.primitive');
  has(scopesOf(r, 1, 'url'), 'variable.other.property'); // inside an embedded attribute (injection)
  has(scopesOf(r, 1, '#each'), 'keyword.control.loop');
  has(scopesOf(r, 2, 'HTML'), 'support.type.primitive');
  has(scopesOf(r, 2, '<<'), 'keyword.operator.heredoc');
  has(scopesOf(r, 3, 'SITE'), 'variable.other.readwrite');
  has(scopesOf(r, 4, 'T'), 'entity.name.tag.heredoc');
  has(scopesOf(r, 6, 'c2'), 'variable.other.readwrite');
  // A plain LET is not scanned for heredocs, and ARG cannot be html.
  hasNot(scopesOf(r, 8, '<<EOF'), 'heredoc');
  has(scopesOf(r, 9, 'html'), 'invalid.illegal.type');
  has(scopesOf(r, 10, 'EXPORT'), 'keyword.control.instruction');
});

test('TRANSFORM --include and --exclude', async () => {
  const r = await tokenize('TRANSFORM --include=*.jpg --exclude=drafts/ ./photos/ WITH image TO /img/*.png\n');
  has(scopesOf(r, 1, 'include'), 'variable.parameter.flag');
  has(scopesOf(r, 1, '*.jpg'), 'string.unquoted.glob');
  has(scopesOf(r, 1, 'drafts/'), 'string.unquoted.glob');
});
