// Keeps the grammar from falling behind the language: every keyword the
// TypeScript (and Rust) parser accepts must be in keywords.js, in the generated
// src/grammar.json (as a case-insensitive token), and every value word in
// queries/highlights.scm.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { KEYWORDS, caseInsensitive } from '../keywords.js';
import { grammarDir, repoRoot } from './helpers.mjs';

const src = (p) => readFileSync(path.join(repoRoot, p), 'utf8');
const parserTs = src('src/language/parser.ts');
const astTs = src('src/language/ast.ts');
const scannerTs = src('src/language/scanner.ts');
const parserRs = src('rust/src/language/parser.rs');
const grammar = JSON.parse(readFileSync(path.join(grammarDir, 'src/grammar.json'), 'utf8'));
const highlights = readFileSync(path.join(grammarDir, 'queries/highlights.scm'), 'utf8');

const all = (re, text) => [...text.matchAll(re)].map((m) => m[1]);
const strings = (text) => all(/'([^']+)'/g, text);
const constList = (name, text) => {
  const m = new RegExp(`const ${name}[^=]*=\\s*(?:new Set\\()?\\[([^\\]]*)\\]`).exec(text);
  assert.ok(m, `${name} not found`);
  return strings(m[1]);
};
const lower = (xs) => xs.map((x) => x.toLowerCase()).sort();

/** The token rule grammar.js generated for a keyword. */
function keywordToken(word) {
  const rule = grammar.rules[`_kw_${word.toLowerCase()}`];
  assert.ok(rule, `no token for ${word} in src/grammar.json`);
  assert.equal(rule.type, 'TOKEN');
  assert.equal(rule.content.value, caseInsensitive(word), word);
  return rule;
}

/** Highlighted as a keyword: listed as an anonymous node in highlights.scm. */
const inHighlights = (word) => highlights.includes(`"${word}"`);

test('instruction keywords match the parser (TypeScript and Rust)', () => {
  const body = parserTs.slice(parserTs.indexOf('function parseInstruction'), parserTs.indexOf('function parseFrom'));
  const ts = all(/case '([A-Z]+)':/g, body).filter((k) => k !== 'RUN');
  const rs = all(/"([A-Z]+)" => parse_/g, parserRs);
  assert.deepEqual([...ts].sort(), [...KEYWORDS.instructions].sort());
  assert.deepEqual([...rs].sort(), [...KEYWORDS.instructions].sort());
  for (const k of KEYWORDS.instructions) {
    keywordToken(k);
    assert.ok(inHighlights(k), `${k} missing from highlights.scm`);
    assert.ok(grammar.rules[`${k.toLowerCase()}_instruction`], `${k.toLowerCase()}_instruction rule`);
  }
});

test('unsupported instructions (RUN and NOT_YET) parse as unsupported_instruction', () => {
  const notYet = constList('NOT_YET', parserTs);
  assert.match(parserTs, /case 'RUN':/);
  assert.deepEqual(['RUN', ...notYet].sort(), [...KEYWORDS.unsupported].sort());
  const rule = JSON.stringify(grammar.rules.unsupported_instruction);
  for (const k of KEYWORDS.unsupported) {
    keywordToken(k);
    assert.ok(rule.includes(`"_kw_${k.toLowerCase()}"`), `${k} missing from unsupported_instruction`);
  }
});

test('every clause keyword the parser reads is a grammar token', () => {
  const used = new Set([...all(/\.keyword\('([A-Za-z]+)'\)/g, parserTs), ...all(/expectKeyword\('([A-Za-z]+)'\)/g, parserTs)]);
  used.delete(KEYWORDS.scratch);
  const known = new Set(KEYWORDS.clauses.map((k) => k.toLowerCase()));
  for (const k of used) {
    assert.ok(known.has(k.toLowerCase()), `clause keyword ${k} missing from KEYWORDS.clauses`);
    keywordToken(k);
    assert.ok(inHighlights(k.toUpperCase()), `${k} missing from highlights.scm`);
  }
  assert.match(parserTs, new RegExp(`keyword\\('${KEYWORDS.scratch}'\\)`));
  assert.equal(grammar.rules.scratch.content.value, caseInsensitive(KEYWORDS.scratch));
});

test('value words (types, import kinds, export formats, compressions, escape modes) are highlighted', () => {
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
    assert.ok(highlights.includes(`"${w}"`) && highlights.includes(`"${w.toUpperCase()}"`), `${w} missing from highlights.scm`);
  }
});

test('flags per instruction match parseFlags calls', () => {
  for (const [instr, fn] of [['COPY', 'parseCopy'], ['RENDER', 'parseRender'], ['TRANSFORM', 'parseTransform']]) {
    const body = parserTs.slice(parserTs.indexOf(`function ${fn}(`));
    const m = /parseFlags\(c, \{([^}]*)\}/.exec(body);
    assert.ok(m, `parseFlags not found in ${fn}`);
    const names = all(/([a-z]+):/g, m[1]);
    assert.deepEqual([...names].sort(), [...KEYWORDS.flags[instr]].sort(), instr);
    assert.ok(JSON.stringify(grammar.rules[`${instr.toLowerCase()}_instruction`]).includes('"flag"'), `${instr} takes flags`);
  }
  for (const f of ['from', 'escape']) assert.ok(highlights.includes(`"${f}"`), `--${f} value highlighting`);
});

test('heredocs are accepted on exactly COPY, RENDER and TRANSFORM', () => {
  const heredoc = constList('HEREDOC_INSTRUCTIONS', scannerTs);
  assert.deepEqual([...heredoc].sort(), [...KEYWORDS.heredocInstructions].sort());
  const withHeredoc = Object.entries(grammar.rules)
    .filter(([name, rule]) => name.endsWith('_instruction') && JSON.stringify(rule).includes('"heredoc_start"'))
    .map(([name]) => name.replace('_instruction', '').toUpperCase());
  assert.deepEqual(withHeredoc.sort(), [...heredoc].sort());
  // Only RENDER bodies are templates.
  assert.ok(JSON.stringify(grammar.rules.render_instruction).includes('_template_heredoc'));
  for (const k of ['copy', 'transform']) assert.ok(JSON.stringify(grammar.rules[`${k}_instruction`]).includes('_raw_heredoc'), k);
});
