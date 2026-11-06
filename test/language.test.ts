import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isSitefileError, type ErrorCode } from '../src/errors.js';
import { isImageBase, literalText, type Instruction, type Operand, type TemplatePart } from '../src/language/ast.js';
import { parseSitefile } from '../src/language/parser.js';
import { logicalLines } from '../src/language/scanner.js';
import { parseProgram, validate } from '../src/language/validate.js';

const HEAD = 'FROM scratch\n';
const TAIL = '\nEXPORT /';

function one(line: string): Instruction {
  const all = parseSitefile(line);
  assert.equal(all.length, 1);
  return all[0]!;
}

function op(line: string, field: 'source' | 'dest' = 'source'): Operand {
  const ins = one(line);
  assert.equal(ins.kind, 'COPY');
  return ins.kind === 'COPY' ? (ins[field] as Operand) : assert.fail();
}

function parts(o: Operand): unknown[] {
  return o.parts.map((p: TemplatePart) =>
    p.kind === 'text' ? p.value : { var: p.name, line: p.span.line, column: p.span.column },
  );
}

function throwsAt(fn: () => unknown, code: ErrorCode, line: number, column: number, message?: RegExp): void {
  assert.throws(fn, (e: unknown) => {
    assert.ok(isSitefileError(e), `expected SitefileError, got ${String(e)}`);
    assert.equal(e.code, code, e.message);
    assert.deepEqual([e.span?.line, e.span?.column], [line, column], e.message);
    if (message) assert.match(e.detail, message);
    return true;
  });
}

const parseErr = (src: string, code: ErrorCode, line: number, column: number, message?: RegExp): void =>
  throwsAt(() => parseSitefile(src), code, line, column, message);
const progErr = (src: string, code: ErrorCode, line: number, column: number, message?: RegExp): void =>
  throwsAt(() => parseProgram(src), code, line, column, message);

// ---- scanner: logical lines -------------------------------------------------

test('blank and comment lines are skipped', () => {
  const lines = logicalLines('\n# comment\n   # indented comment\nFROM scratch\n\n  \nEXPORT /\n');
  assert.deepEqual(lines.map((l) => l.text), ['FROM scratch', 'EXPORT /']);
});

test('continuations join pieces with one space and keep positions', () => {
  const [l] = logicalLines('COPY \\\n    ./a   \\\n  /b');
  assert.equal(l!.text, 'COPY ./a /b');
  const at = (i: number) => [l!.positions[i]!.line, l!.positions[i]!.column];
  assert.deepEqual(at(0), [1, 1]);
  assert.deepEqual(at(5), [2, 5]); // ./a
  assert.deepEqual(at(9), [3, 3]); // /b
});

test('comment and blank lines inside a continuation are skipped', () => {
  const ins = parseSitefile('COPY \\\n# a comment\n\n  ./a \\\n  # another\n  /b\nEXPORT /');
  assert.equal(ins.length, 2);
  const c = ins[0]!;
  assert.equal(c.kind, 'COPY');
  if (c.kind === 'COPY') {
    assert.equal(literalText(c.source), './a');
    assert.deepEqual([c.dest.span.line, c.dest.span.column], [6, 3]);
  }
});

test('even number of trailing backslashes does not continue', () => {
  const ins = parseSitefile('COPY ./a /b\\\\\nEXPORT /');
  assert.equal(ins.length, 2);
  assert.equal(literalText((ins[0] as { dest: Operand }).dest), '/b\\');
});

test('odd number of trailing backslashes continues', () => {
  const ins = parseSitefile('COPY ./a\\\\\\\n /b');
  assert.equal(ins.length, 1);
  assert.equal(literalText((ins[0] as { source: Operand }).source), './a\\');
});

test('# in the middle of a line is ordinary text', () => {
  assert.equal(literalText(op('COPY ./a#b /c#d', 'dest')), '/c#d');
});

test('CRLF line endings', () => {
  assert.equal(parseSitefile('FROM scratch\r\nEXPORT /\r\n').length, 2);
});

test('error spans across continuation lines are accurate', () => {
  parseErr('COPY \\\n  ./a \\\n  /b extra', 'E_SYNTAX', 3, 6);
  progErr('FROM scratch\nCOPY \\\n  ./${NOPE} \\\n  /b\nEXPORT /', 'E_UNDEFINED', 3, 5);
});

// ---- operands ---------------------------------------------------------------

test('bare, double and single quoted operands', () => {
  assert.deepEqual(parts(op('COPY "a b" x')), ['a b']);
  assert.deepEqual(parts(op("COPY 'a b' x")), ['a b']);
  assert.deepEqual(parts(op('COPY a\\ b x')), ['a b']);
  assert.deepEqual(parts(op('COPY "" x')), []);
});

test('escapes in bare and double-quoted text', () => {
  assert.deepEqual(parts(op('COPY "a\\"b\\\\c\\n\\t\\$\\q" x')), ['a"b\\c\n\t$q']);
  assert.deepEqual(parts(op('COPY a\\$\\{b\\n x')), ['a${bn']);
});

test('single-quoted text is fully literal', () => {
  assert.deepEqual(parts(op("COPY 'a\\n${X}\\' x")), ['a\\n${X}\\']);
});

test('$ not followed by { is literal', () => {
  assert.deepEqual(parts(op('COPY $a$ x')), ['$a$']);
  assert.deepEqual(parts(op('COPY "cost: $5" x')), ['cost: $5']);
});

test('interpolation builds var parts with spans', () => {
  assert.deepEqual(parts(op('COPY a${X}b${Y_2} x')), ['a', { var: 'X', line: 1, column: 7 }, 'b', { var: 'Y_2', line: 1, column: 12 }]);
  assert.deepEqual(parts(op('COPY "${A} ${B}" x')), [{ var: 'A', line: 1, column: 7 }, ' ', { var: 'B', line: 1, column: 12 }]);
  const v = op('COPY ${X} y').parts[0]!;
  assert.equal(v.kind, 'var');
  if (v.kind === 'var') assert.deepEqual(v.path, []);
});

test('interpolation syntax errors', () => {
  // Dotted names parse, but outside a RENDER … FOR destination the validator rejects them.
  progErr('FROM scratch\nARG a = x\nCOPY ${a.b} /x\nEXPORT /', 'E_SYNTAX', 3, 6, /RENDER/);
  parseErr('COPY ${a.} x', 'E_SYNTAX', 1, 6);
  parseErr('COPY ${} x', 'E_SYNTAX', 1, 6);
  parseErr('COPY ${1A} x', 'E_SYNTAX', 1, 6);
  parseErr('COPY ab${X x', 'E_SYNTAX', 1, 8);
  parseErr('COPY "${X" x', 'E_SYNTAX', 1, 7);
});

test('unterminated quotes', () => {
  parseErr('COPY "abc x', 'E_SYNTAX', 1, 6, /unterminated/);
  parseErr("COPY x 'abc", 'E_SYNTAX', 1, 8, /unterminated/);
  parseErr('COPY "abc\\', 'E_SYNTAX', 1, 6, /unterminated/);
});

test('text right after a closing quote is a syntax error', () => {
  parseErr('COPY "a"b x', 'E_SYNTAX', 1, 9);
});

// ---- instructions -----------------------------------------------------------

test('keywords are case-insensitive', () => {
  const ins = parseSitefile('from Scratch as base\ncopy a b\nrender ./t to /x\nimport n from ./x as JSON integrity sha256:' + 'a'.repeat(64) + '\nexport / as Directory');
  assert.deepEqual(ins.map((i) => i.kind), ['FROM', 'COPY', 'RENDER', 'IMPORT', 'EXPORT']);
  assert.deepEqual(ins[0], { kind: 'FROM', span: { file: 'Sitefile', line: 1, column: 1 }, base: 'scratch', alias: 'base' });
  assert.equal((ins[3] as { as: string }).as, 'json');
});

test('instruction span is the keyword and file defaults to Sitefile', () => {
  const [ins] = parseSitefile('\n\n   COPY a b', 'x/Sitefile');
  assert.deepEqual(ins!.span, { file: 'x/Sitefile', line: 3, column: 4 });
  assert.equal(parseSitefile('COPY a b')[0]!.span.file, 'Sitefile');
});

test('FROM forms', () => {
  assert.deepEqual(one('FROM scratch'), { kind: 'FROM', span: { file: 'Sitefile', line: 1, column: 1 }, base: 'scratch' });
  const f = one('FROM "scratch"');
  assert.ok(f.kind === 'FROM' && f.base !== 'scratch' && !isImageBase(f.base) && literalText(f.base) === 'scratch');
  const g = one('FROM ./base AS site');
  assert.ok(g.kind === 'FROM' && g.base !== 'scratch' && g.alias === 'site');
  parseErr('FROM', 'E_SYNTAX', 1, 5);
  parseErr('FROM scratch AS', 'E_SYNTAX', 1, 16);
  parseErr('FROM scratch extra', 'E_SYNTAX', 1, 14);
});

test('FROM image forms', () => {
  const lit = (o: unknown): string | undefined => literalText(o as Parameters<typeof literalText>[0]);
  const a = one('FROM image ghcr.io/me/theme:1.2 AS theme');
  assert.ok(a.kind === 'FROM' && isImageBase(a.base) && lit(a.base.ref) === 'ghcr.io/me/theme:1.2' && a.base.at === undefined && a.alias === 'theme');
  const b = one('from IMAGE theme:1 at /srv/www');
  assert.ok(b.kind === 'FROM' && isImageBase(b.base) && lit(b.base.ref) === 'theme:1' && lit(b.base.at) === '/srv/www' && b.alias === undefined);
  const c = one('STAGE site FROM image localhost:5000/t@sha256:' + '0'.repeat(64) + ' AT /x');
  assert.ok(c.kind === 'FROM' && isImageBase(c.base) && lit(c.base.at) === '/x' && c.alias === 'site' && c.keyword === 'STAGE');
  // `image` followed by nothing or AS is still a stage or directory named image.
  const d = one('FROM image');
  assert.ok(d.kind === 'FROM' && d.base !== 'scratch' && !isImageBase(d.base) && literalText(d.base) === 'image');
  const e = one('FROM image AS x');
  assert.ok(e.kind === 'FROM' && !isImageBase(e.base) && e.alias === 'x');
  const f = one('STAGE s FROM image');
  assert.ok(f.kind === 'FROM' && !isImageBase(f.base) && f.alias === 's');
  parseErr('FROM image theme:1 AT', 'E_SYNTAX', 1, 22);
  parseErr('FROM image theme:1 extra', 'E_SYNTAX', 1, 20);
  parseErr('STAGE s FROM image t:1 AS x', 'E_SYNTAX', 1, 24);
});

test('STAGE forms', () => {
  // STAGE name == FROM scratch AS name; STAGE name FROM base == FROM base AS name (same IR, plus a keyword marker).
  assert.deepEqual(one('STAGE site'), { kind: 'FROM', span: { file: 'Sitefile', line: 1, column: 1 }, base: 'scratch', alias: 'site', keyword: 'STAGE' });
  assert.deepEqual(one('stage site from scratch'), one('STAGE site'));
  const g = one('STAGE site FROM ./base');
  assert.ok(g.kind === 'FROM' && g.base !== 'scratch' && !isImageBase(g.base) && literalText(g.base) === './base' && g.alias === 'site' && g.keyword === 'STAGE');
  const h = one('STAGE b FROM a');
  assert.ok(h.kind === 'FROM' && h.base !== 'scratch' && !isImageBase(h.base) && literalText(h.base) === 'a' && h.alias === 'b');
  // FROM … AS is unchanged and carries no marker.
  assert.equal((one('FROM scratch AS site') as { keyword?: string }).keyword, undefined);
  parseErr('STAGE', 'E_SYNTAX', 1, 6, /expected stage name/);
  parseErr('STAGE FROM scratch', 'E_SYNTAX', 1, 12, /unexpected 'scratch'/);
  parseErr('STAGE x FROM', 'E_SYNTAX', 1, 13, /expected base/);
  parseErr('STAGE x extra', 'E_SYNTAX', 1, 9, /unexpected 'extra'/);
  parseErr('STAGE x AS y', 'E_SYNTAX', 1, 9, /unexpected 'AS'/);
  parseErr('STAGE x FROM scratch extra', 'E_SYNTAX', 1, 22, /unexpected 'extra'/);
  parseErr('STAGE x-y', 'E_SYNTAX', 1, 8, /invalid stage name/);
  // Bare FROM without a base stays a syntax error.
  parseErr('FROM', 'E_SYNTAX', 1, 5);
});

test('STAGE validation', () => {
  const kinds = (src: string): string[] => parseProgram(src).instructions.map((i) => i.kind);
  // STAGE starts a stage: no implicit stage is added (also after ARG), and it mixes with FROM.
  assert.deepEqual(kinds('STAGE s\nCOPY a /b\nEXPORT /'), ['FROM', 'COPY', 'EXPORT']);
  assert.deepEqual(kinds('ARG X = 1\nSTAGE s\nCOPY a /b\nEXPORT /'), ['ARG', 'FROM', 'COPY', 'EXPORT']);
  assert.deepEqual(kinds('FROM scratch AS a\nSTAGE b FROM a\nFROM b AS c\nSTAGE d\nEXPORT /'), ['FROM', 'FROM', 'FROM', 'FROM', 'EXPORT']);
  // COPY --from a STAGE, and STAGE based on an earlier stage or a local directory.
  assert.ok(parseProgram('STAGE a\nCOPY x /x\nSTAGE b FROM a\nSTAGE c FROM ./dir\nSTAGE d\nCOPY --from=a /x /y' + TAIL));
  progErr('STAGE a\nCOPY --from=a /x /y' + TAIL, 'E_UNDEFINED', 2, 6, /not complete/);
  // Same name rules and errors as FROM … AS.
  progErr('STAGE a\nSTAGE a' + TAIL, 'E_DUPLICATE', 2, 1, /'a' is already defined/);
  progErr('FROM scratch AS a\nSTAGE a' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('ARG a\nSTAGE a' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('STAGE a\nFROM scratch AS a' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('STAGE a FROM a' + TAIL, 'E_UNDEFINED', 1, 14, /^STAGE a FROM a: stage 'a' is not defined before this line/);
  progErr('STAGE a FROM b\nSTAGE b' + TAIL, 'E_UNDEFINED', 1, 14, /^STAGE a FROM b: stage 'b'/);
  progErr('FROM b\nSTAGE b' + TAIL, 'E_UNDEFINED', 1, 6, /^FROM b: stage 'b'/);
  // a bare word naming no stage is a local directory
  assert.ok(parseProgram('STAGE a FROM dir\nSTAGE b FROM ./b' + TAIL));
});

test('ARG forms', () => {
  const cases: Array<[string, string, string, string | undefined]> = [
    ['ARG X', 'X', 'string', undefined],
    ['ARG X=v', 'X', 'string', 'v'],
    ['ARG X = "a b"', 'X', 'string', 'a b'],
    ['ARG N: number = 3', 'N', 'number', '3'],
    ['ARG N:number=3', 'N', 'number', '3'],
    ['ARG B : boolean', 'B', 'boolean', undefined],
    ['ARG S:STRING=x', 'S', 'string', 'x'],
  ];
  for (const [src, name, type, value] of cases) {
    const a = one(src);
    assert.equal(a.kind, 'ARG', src);
    if (a.kind !== 'ARG') continue;
    assert.equal(a.name, name, src);
    assert.equal(a.type, type, src);
    assert.equal(a.defaultValue && literalText(a.defaultValue), value, src);
  }
  parseErr('ARG', 'E_SYNTAX', 1, 4);
  parseErr('ARG 1X', 'E_SYNTAX', 1, 5);
  parseErr('ARG X-Y', 'E_SYNTAX', 1, 6);
  parseErr('ARG X: date', 'E_SYNTAX', 1, 8, /unknown type/);
  parseErr('ARG X:', 'E_SYNTAX', 1, 7);
  parseErr('ARG X =', 'E_SYNTAX', 1, 8);
  parseErr('ARG X 5', 'E_SYNTAX', 1, 7);
  parseErr('ARG X = a b', 'E_SYNTAX', 1, 11);
});

test('LET forms', () => {
  const l = one('LET G: boolean = true');
  assert.ok(l.kind === 'LET' && l.name === 'G' && l.type === 'boolean' && literalText(l.value) === 'true');
  const m = one('LET M="hi ${X}"');
  assert.ok(m.kind === 'LET' && m.type === 'string');
  parseErr('LET X', 'E_SYNTAX', 1, 1, /requires a value/);
  parseErr('LET X: number', 'E_SYNTAX', 1, 1, /requires a value/);
});

test('IMPORT forms', () => {
  const digest = 'sha256:' + '0123456789abcdef'.repeat(4);
  const a = one('IMPORT logo FROM https://example.com/logo.png');
  assert.ok(a.kind === 'IMPORT' && a.name === 'logo' && literalText(a.uri) === 'https://example.com/logo.png');
  assert.ok(!('as' in a) && !('integrity' in a));
  const b = one(`IMPORT d FROM ./data.json AS json INTEGRITY ${digest}`);
  assert.ok(b.kind === 'IMPORT' && b.as === 'json' && b.integrity === digest);
  const c = one(`IMPORT d FROM "./x y" INTEGRITY "${digest}" AS tree`);
  assert.ok(c.kind === 'IMPORT' && c.as === 'tree' && c.integrity === digest && literalText(c.uri) === './x y');
  parseErr('IMPORT d ./x', 'E_SYNTAX', 1, 10, /FROM/);
  parseErr('IMPORT FROM ./x', 'E_SYNTAX', 1, 13, /FROM/);
  parseErr('IMPORT d FROM', 'E_SYNTAX', 1, 14);
  parseErr('IMPORT d FROM ./x AS toml', 'E_SYNTAX', 1, 22, /import kind/);
  parseErr('IMPORT d FROM ./x AS text AS json', 'E_SYNTAX', 1, 27, /twice/);
  parseErr(`IMPORT d FROM ./x INTEGRITY ${digest} INTEGRITY ${digest}`, 'E_SYNTAX', 1, 101, /twice/);
  parseErr('IMPORT d FROM ./x INTEGRITY sha256:abc', 'E_SYNTAX', 1, 29, /sha256/);
  parseErr(`IMPORT d FROM ./x INTEGRITY ${digest.toUpperCase()}`, 'E_SYNTAX', 1, 29);
  parseErr('IMPORT d FROM ./x INTEGRITY ${X}', 'E_SYNTAX', 1, 29);
  parseErr('IMPORT d FROM ./x EXTRA', 'E_SYNTAX', 1, 19, /AS, INTEGRITY, SORT BY or SECRET/);
});

test('COPY forms', () => {
  const a = one('COPY ./public/ /');
  assert.ok(a.kind === 'COPY' && !a.overwrite && literalText(a.source) === './public/' && literalText(a.dest) === '/');
  assert.deepEqual([a.span.column, (a as { dest: Operand }).dest.span.column], [1, 16]);
  const b = one('COPY --overwrite site: /');
  assert.ok(b.kind === 'COPY' && b.overwrite && literalText(b.source) === 'site:');
  assert.ok(one('COPY --from=build /a /b').kind === 'COPY');
  parseErr('COPY --chown=1 /a /b', 'E_SYNTAX', 1, 6, /unknown flag/);
  parseErr('COPY --overwrite=yes /a /b', 'E_SYNTAX', 1, 6);
  parseErr('COPY --overwrite --overwrite /a /b', 'E_SYNTAX', 1, 18, /twice/);
  parseErr('COPY ./a', 'E_SYNTAX', 1, 9, /destination/);
  parseErr('COPY', 'E_SYNTAX', 1, 5, /source/);
  parseErr('COPY a b c', 'E_SYNTAX', 1, 10);
});

test('RENDER forms', () => {
  const a = one('RENDER ./t.html TO /index.html');
  assert.ok(a.kind === 'RENDER' && a.escape === 'none' && !a.overwrite);
  const b = one('RENDER --overwrite --escape=html "./t.html" to "/i.html"');
  assert.ok(b.kind === 'RENDER' && b.escape === 'html' && b.overwrite && literalText(b.dest) === '/i.html');
  assert.ok(one('RENDER --escape=none t TO /x').kind === 'RENDER');
  parseErr('RENDER --overwrite --escape=js t TO /x', 'E_SYNTAX', 1, 20, /escape/);
  parseErr('RENDER --escape t TO /x', 'E_SYNTAX', 1, 8, /requires a value/);
  parseErr('RENDER t /x', 'E_SYNTAX', 1, 10, /TO/);
  parseErr('RENDER t TO', 'E_SYNTAX', 1, 12);
  assert.ok(one('RENDER --from=a t TO /x').kind === 'RENDER');
});

test('EXPORT forms', () => {
  const a = one('EXPORT /');
  assert.ok(a.kind === 'EXPORT' && a.format === 'directory' && literalText(a.path) === '/');
  assert.ok(one('EXPORT /site AS directory').kind === 'EXPORT');
  assert.ok(one('EXPORT / AS zip').kind === 'EXPORT');
  parseErr('EXPORT / AS rar', 'E_UNSUPPORTED', 1, 13, /rar/);
  parseErr('EXPORT / AS', 'E_SYNTAX', 1, 12);
  parseErr('EXPORT', 'E_SYNTAX', 1, 7);
  parseErr('EXPORT / AS directory more', 'E_SYNTAX', 1, 23);
});

test('RUN and future instructions are unsupported; others unknown', () => {
  parseErr('RUN npm build', 'E_UNSUPPORTED', 1, 1, /never runs shell commands/);
  for (const kw of ['REMOVE', 'GENERATE', 'ASSERT', 'META', 'exec']) {
    parseErr(`  ${kw} x`, 'E_UNSUPPORTED', 1, 3, /not supported yet/);
  }
  parseErr('\nWORKDIR /x', 'E_UNKNOWN_INSTRUCTION', 2, 1, /WORKDIR/);
});

// ---- validation -------------------------------------------------------------

test('example Sitefile parses and validates', () => {
  const path = fileURLToPath(new URL('../../examples/hello/Sitefile', import.meta.url));
  const program = parseProgram(readFileSync(path, 'utf8'), path);
  assert.equal(program.file, path);
  assert.deepEqual(program.instructions.map((i) => i.kind), ['ARG', 'FROM', 'LET', 'COPY', 'RENDER', 'EXPORT']);
});

test('full program with imports and variables validates', () => {
  const src = [
    'ARG SITE = demo',
    'FROM scratch',
    'ARG VERSION: number = 1',
    'LET OUT = "/v${VERSION}"',
    'IMPORT assets FROM ./assets',
    'IMPORT data FROM https://example.com/${SITE}.json AS json',
    'COPY assets: ${OUT}/',
    'COPY assets:/img/logo.png /logo.png',
    'COPY ./foo:bar /x',
    'RENDER --escape=html ./t.html TO ${OUT}/index.html',
    'EXPORT ${OUT}',
  ].join('\n');
  assert.equal(parseProgram(src).instructions.length, 11);
});

test('validate() accepts parsed instructions directly', () => {
  const p = validate(parseSitefile(HEAD + 'COPY a /b' + TAIL, 'F'), 'F');
  assert.equal(p.file, 'F');
  assert.equal(p.instructions.length, 3);
});

test('empty Sitefile', () => {
  progErr('', 'E_VALIDATION', 1, 1, /no instructions/);
  progErr('# only comments\n\n', 'E_VALIDATION', 1, 1);
});

test('FROM rules', () => {
  assert.ok(parseProgram('FROM ./base\nEXPORT /'));
  assert.ok(parseProgram('FROM scratch\nFROM scratch AS b\nEXPORT /'));
  assert.equal(parseProgram('ARG X\nFROM scratch\nEXPORT /').instructions.length, 3);
});

test('implicit FROM scratch', () => {
  const kinds = (src: string): string[] => parseProgram(src).instructions.map((i) => i.kind);
  // No FROM at all: an implicit stage starts at the first instruction, with that instruction's span.
  const p = parseProgram('# c\nCOPY a /b\nEXPORT /');
  assert.deepEqual(kinds('COPY a /b\nEXPORT /'), ['FROM', 'COPY', 'EXPORT']);
  const f = p.instructions[0]!;
  assert.ok(f.kind === 'FROM' && f.base === 'scratch' && f.alias === undefined);
  assert.deepEqual([f.span.line, f.span.column], [2, 1]);
  assert.deepEqual(kinds('EXPORT /'), ['FROM', 'EXPORT']);
  // ARG may precede the implicit stage; it stays before it.
  assert.deepEqual(kinds('ARG X = 1\nARG Y = 2\nLET Z = 3\nCOPY a /b\nEXPORT /'), ['ARG', 'ARG', 'FROM', 'LET', 'COPY', 'EXPORT']);
  // Followed by named stages, and an explicit FROM scratch is unchanged.
  assert.deepEqual(kinds('COPY a /b\nFROM scratch AS two\nEXPORT /'), ['FROM', 'COPY', 'FROM', 'EXPORT']);
  assert.deepEqual(kinds('ARG X = 1\nFROM scratch\nEXPORT /'), ['ARG', 'FROM', 'EXPORT']);
  // Still errors: nothing but ARGs, or no EXPORT.
  progErr('ARG X', 'E_VALIDATION', 1, 1, /missing EXPORT/);
  progErr('COPY a /b', 'E_VALIDATION', 1, 1, /missing EXPORT/);
  // Variables used by the implicit stage still must be declared earlier.
  progErr('COPY ./${X} /b\nEXPORT /', 'E_UNDEFINED', 1, 8, /\$\{X\}/);
});

test('EXPORT rules', () => {
  progErr('FROM scratch\nCOPY a /b', 'E_VALIDATION', 2, 1, /missing EXPORT/);
  progErr('FROM scratch\nEXPORT /\nCOPY a /b', 'E_VALIDATION', 3, 1, /last/);
  progErr('FROM scratch\nEXPORT /\nEXPORT /', 'E_VALIDATION', 3, 1);
});

test('variables must be defined on an earlier line', () => {
  progErr(HEAD + 'COPY ./${X} /' + TAIL, 'E_UNDEFINED', 2, 8, /\$\{X\}/);
  progErr(HEAD + 'LET X = ${X}' + TAIL, 'E_UNDEFINED', 2, 9);
  progErr(HEAD + 'LET A = 1\nRENDER t TO "/a/${A}/${B}"' + TAIL, 'E_UNDEFINED', 3, 22);
  progErr(HEAD + 'IMPORT d FROM ./${U}' + TAIL, 'E_UNDEFINED', 2, 17);
  progErr(HEAD + 'EXPORT /${OUT}', 'E_UNDEFINED', 2, 9);
  progErr(HEAD + 'COPY a /${late}\nLET late = x' + TAIL, 'E_UNDEFINED', 2, 9);
  progErr(HEAD + 'IMPORT n FROM ./x\nCOPY a /${n}' + TAIL, 'E_UNDEFINED', 3, 9, /import/);
  progErr('ARG A = ${B}\nFROM scratch' + TAIL, 'E_UNDEFINED', 1, 9);
  progErr(HEAD + 'LET x = 1\nCOPY a /${X}' + TAIL, 'E_UNDEFINED', 3, 9); // names are case-sensitive
});

test('duplicate names', () => {
  progErr(HEAD + 'ARG X\nARG X' + TAIL, 'E_DUPLICATE', 3, 1);
  progErr('ARG X\n' + HEAD + 'LET X = 1' + TAIL, 'E_DUPLICATE', 3, 1);
  progErr(HEAD + 'LET X = 1\nIMPORT X FROM ./x' + TAIL, 'E_DUPLICATE', 3, 1);
  progErr(HEAD + 'IMPORT a FROM ./x\nIMPORT a FROM ./y' + TAIL, 'E_DUPLICATE', 3, 1);
  progErr(HEAD + 'IMPORT a FROM ./x\nLET a = 1' + TAIL, 'E_DUPLICATE', 3, 1);
});

test('import references in COPY/RENDER sources', () => {
  progErr(HEAD + 'COPY site: /' + TAIL, 'E_UNDEFINED', 2, 6, /site/);
  progErr(HEAD + 'RENDER tpl:/x.html TO /x' + TAIL, 'E_UNDEFINED', 2, 8, /tpl/);
  progErr(HEAD + 'COPY site:/a /\nIMPORT site FROM ./s' + TAIL, 'E_UNDEFINED', 2, 6);
  // non-literal sources and local paths are not import references
  assert.ok(parseProgram('ARG P = x\n' + HEAD + 'COPY ${P}: /\nCOPY ./a:b /\nCOPY a/b:c /' + TAIL));
});

test('URL sources are rejected', () => {
  progErr(HEAD + 'COPY https://example.com/a.css /' + TAIL, 'E_VALIDATION', 2, 6, /IMPORT then COPY/);
  progErr(HEAD + 'RENDER "http://x/t.html" TO /t' + TAIL, 'E_VALIDATION', 2, 8, /IMPORT/);
});

// ---- stages and --from ---------------------------------------------------------

test('multiple FROM stages validate; bindings stay global', () => {
  const p = parseProgram(
    'ARG A = 1\nFROM scratch AS one\nLET B = 2\nFROM ./dir AS two\nIMPORT i FROM ./x\nCOPY --from=one /a /b\nFROM two\nCOPY ${A}${B} /c\nEXPORT /',
  );
  assert.equal(p.instructions.filter((i) => i.kind === 'FROM').length, 3);
});

test('COPY and RENDER parse --from', () => {
  const c = one('COPY --from=build /out/ /');
  assert.ok(c.kind === 'COPY' && c.from?.name === 'build' && c.from.span.column === 6);
  const r = one('RENDER --from=build --escape=html /t.html TO /x');
  assert.ok(r.kind === 'RENDER' && r.from?.name === 'build');
  assert.equal((one('COPY a b') as { from?: unknown }).from, undefined);
  parseErr('COPY --from a b', 'E_SYNTAX', 1, 6, /requires a value/);
  parseErr('COPY --from=a-b x y', 'E_SYNTAX', 1, 6, /stage name/);
  parseErr('COPY --from=a --from=b x y', 'E_SYNTAX', 1, 15, /twice/);
});

test('stage names are separate from ARG/LET/IMPORT names', () => {
  progErr('FROM scratch AS a\nFROM scratch AS a' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('ARG a\nFROM scratch AS a' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('FROM scratch\nIMPORT a FROM ./x\nFROM scratch AS a' + TAIL, 'E_DUPLICATE', 3, 1);
  progErr('FROM scratch AS a\nLET a = 1' + TAIL, 'E_DUPLICATE', 2, 1);
  progErr('FROM scratch AS a\nIMPORT a FROM ./x' + TAIL, 'E_DUPLICATE', 2, 1);
});

test('forward and self references to stages are errors', () => {
  progErr('FROM scratch AS a\nCOPY --from=a /x /y' + TAIL, 'E_UNDEFINED', 2, 6, /not complete/);
  progErr('FROM scratch AS a\nCOPY --from=b /x /y\nFROM scratch AS b' + TAIL, 'E_UNDEFINED', 2, 6, /not complete/);
  progErr('FROM scratch\nRENDER --from=nope /t TO /x' + TAIL, 'E_UNDEFINED', 2, 8, /not defined/);
  progErr('FROM a AS a' + TAIL, 'E_UNDEFINED', 1, 6, /not defined before/);
  progErr('FROM b\nFROM scratch AS b' + TAIL, 'E_UNDEFINED', 1, 6);
  // a bare word that names no stage is a local directory; ./name always is
  assert.ok(parseProgram('FROM dir\nFROM ./a AS a\nFROM ./a' + TAIL));
});

test('EXPORT must still be last with several stages', () => {
  progErr('FROM scratch AS a\nEXPORT /\nFROM scratch', 'E_VALIDATION', 3, 1, /EXPORT must be the last/);
  progErr('FROM scratch AS a\nFROM scratch', 'E_VALIDATION', 2, 1, /missing EXPORT/);
});
