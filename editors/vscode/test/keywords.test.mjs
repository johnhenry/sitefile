// Keeps the grammar from falling behind the language: every keyword the
// TypeScript (and Rust) parser accepts must be in scripts/grammar.mjs and in
// the committed grammar JSON.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { KEYWORDS } from '../scripts/grammar.mjs';

const root = new URL('../../../', import.meta.url);
const src = (p) => readFileSync(new URL(p, root), 'utf8');
const parserTs = src('src/language/parser.ts');
const astTs = src('src/language/ast.ts');
const scannerTs = src('src/language/scanner.ts');
const parserRs = src('rust/src/language/parser.rs');
const grammarJson = readFileSync(new URL('../syntaxes/sitefile.tmLanguage.json', import.meta.url), 'utf8');

const all = (re, text) => [...text.matchAll(re)].map((m) => m[1]);
const strings = (text) => all(/'([^']+)'/g, text);
const constList = (name, text) => {
  const m = new RegExp(`const ${name}[^=]*=\\s*(?:new Set\\()?\\[([^\\]]*)\\]`).exec(text);
  assert.ok(m, `${name} not found`);
  return strings(m[1]);
};
const lower = (xs) => xs.map((x) => x.toLowerCase()).sort();

/** True if `word` appears as an alternative inside a regex in the grammar JSON. */
function inGrammar(word) {
  const w = word.replace(/[.]/g, '\\\\\\\\.');
  return new RegExp(`[(|:]${w}[|)]`, 'i').test(grammarJson);
}

test('instruction keywords match the parser (TypeScript and Rust)', () => {
  const body = parserTs.slice(parserTs.indexOf('function parseInstruction'), parserTs.indexOf('function parseFrom'));
  const ts = all(/case '([A-Z]+)':/g, body).filter((k) => k !== 'RUN');
  const rs = all(/"([A-Z]+)" => parse_/g, parserRs);
  assert.deepEqual([...ts].sort(), [...KEYWORDS.instructions].sort());
  assert.deepEqual([...rs].sort(), [...KEYWORDS.instructions].sort());
  for (const k of KEYWORDS.instructions) assert.ok(inGrammar(k), `${k} missing from grammar JSON`);
});

test('unsupported instructions (RUN and NOT_YET) are scoped as invalid', () => {
  const notYet = constList('NOT_YET', parserTs);
  assert.match(parserTs, /case 'RUN':/);
  assert.deepEqual(['RUN', ...notYet].sort(), [...KEYWORDS.unsupported].sort());
  for (const k of KEYWORDS.unsupported) assert.ok(inGrammar(k), `${k} missing from grammar JSON`);
});

test('every clause keyword the parser reads is in the grammar', () => {
  const used = new Set([...all(/\.keyword\('([A-Za-z]+)'\)/g, parserTs), ...all(/expectKeyword\('([A-Za-z]+)'\)/g, parserTs)]);
  used.delete(KEYWORDS.scratch);
  const known = new Set(KEYWORDS.clauses.map((k) => k.toLowerCase()));
  for (const k of used) {
    assert.ok(known.has(k.toLowerCase()), `clause keyword ${k} missing from KEYWORDS.clauses`);
    assert.ok(inGrammar(k), `${k} missing from grammar JSON`);
  }
  assert.match(parserTs, new RegExp(`keyword\\('${KEYWORDS.scratch}'\\)`));
  assert.ok(grammarJson.includes(`(${KEYWORDS.scratch})`));
});

test('value words: types, import kinds, export formats, compressions, escape modes', () => {
  assert.deepEqual(lower(constList('VALUE_TYPES', parserTs)), lower(KEYWORDS.valueTypes));
  assert.deepEqual(lower(constList('IMPORT_KINDS', parserTs)), lower(KEYWORDS.importKinds));
  const formats = constList('EXPORT_FORMATS', astTs);
  if (astTs.includes("=== 'tar.gz'")) formats.push('tar.gz');
  assert.deepEqual(lower(formats), lower(KEYWORDS.exportFormats));
  assert.deepEqual(lower(constList('OCI_COMPRESSIONS', astTs)), lower(KEYWORDS.compressions));
  const escape = /type EscapeMode = ([^;]+);/.exec(astTs);
  assert.ok(escape);
  assert.deepEqual(lower(strings(escape[1])), lower(KEYWORDS.escapeModes));
  for (const w of [...KEYWORDS.valueTypes, ...KEYWORDS.importKinds, ...KEYWORDS.exportFormats, ...KEYWORDS.compressions, ...KEYWORDS.escapeModes]) {
    assert.ok(inGrammar(w), `${w} missing from grammar JSON`);
  }
});

test('flags per instruction match parseFlags calls', () => {
  for (const [instr, fn] of [['COPY', 'parseCopy'], ['RENDER', 'parseRender'], ['TRANSFORM', 'parseTransform']]) {
    const body = parserTs.slice(parserTs.indexOf(`function ${fn}(`));
    const m = /parseFlags\(c, \{([^}]*)\}/.exec(body);
    assert.ok(m, `parseFlags not found in ${fn}`);
    const names = all(/([a-z]+):/g, m[1]);
    assert.deepEqual([...names].sort(), [...KEYWORDS.flags[instr]].sort(), instr);
  }
});

test('heredocs are scanned on exactly COPY, RENDER, TRANSFORM and html LET', () => {
  const heredoc = constList('HEREDOC_INSTRUCTIONS', scannerTs);
  assert.deepEqual(heredoc.sort(), ['COPY', 'RENDER', 'TRANSFORM']);
  // `LET name: html` (scanner.ts isHtmlLet) also takes a heredoc.
  assert.match(scannerTs, /HEREDOC_INSTRUCTIONS\.has\(keyword\) \|\| isHtmlLet\(text\)/);
  assert.match(scannerTs, /toLowerCase\(\) !== '(\w+)'/);
  assert.equal(/toLowerCase\(\) !== '(\w+)'/.exec(scannerTs)[1], KEYWORDS.fragmentType);
  const g = JSON.parse(grammarJson);
  const withHeredoc = g.repository.instructions.patterns
    .filter((p) => p.patterns?.some((q) => q.include?.startsWith('#heredoc-')))
    .map((p) => p.name.replace('meta.instruction.', '').replace('.sitefile', '').toUpperCase());
  assert.deepEqual(withHeredoc.sort(), [...heredoc, 'LET'].sort());
  for (const k of [...heredoc, 'LET']) {
    assert.ok(g.repository[`heredoc-${k.toLowerCase()}`].patterns.every((p) => p.name.startsWith('meta.heredoc')), k);
  }
});
