import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { parseSitefile } from '../src/language/parser.js';
import { parseProgram } from '../src/language/validate.js';
import { isInline, type InlineBody } from '../src/language/ast.js';
import { build } from '../src/executor.js';
import { isSitefileError, type ErrorCode } from '../src/errors.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

function syntaxError(src: string, code: ErrorCode, line: number, column?: number): void {
  assert.throws(
    () => parseProgram(src),
    (e: unknown) => {
      assert.ok(isSitefileError(e), String(e));
      assert.equal(e.code, code, e.message);
      assert.equal(e.span?.line, line, e.message);
      if (column !== undefined) assert.equal(e.span?.column, column, e.message);
      return true;
    },
  );
}

function inlineOf(src: string): InlineBody {
  const ins = parseSitefile(src).find((i) => i.kind === 'COPY' || i.kind === 'RENDER');
  assert.ok(ins && (ins.kind === 'COPY' || ins.kind === 'RENDER'));
  const body = ins.kind === 'COPY' ? ins.source : ins.template;
  assert.ok(isInline(body));
  return body;
}

async function buildSitefile(sitefile: string, buildArgs: Record<string, string> = {}) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-inline-'));
  temps.push(dir);
  await writeFile(path.join(dir, 'Sitefile'), sitefile);
  return build({ contextDir: dir, lockfile: false, buildArgs });
}

const text = (b: Uint8Array | undefined): string => new TextDecoder().decode(b);

test('a heredoc body is the lines up to the delimiter, kept verbatim', () => {
  const body = inlineOf(['FROM scratch', 'COPY <<EOF /a.txt', '# not a comment', '', '  indented \\', 'EOF', 'EXPORT /'].join('\n'));
  assert.equal(body.delimiter, 'EOF');
  assert.equal(body.text, '# not a comment\n\n  indented \\\n');
  assert.equal(body.bodyLine, 3);
  assert.deepEqual([body.span.line, body.span.column], [2, 6]);
});

test('<<- strips leading tabs from the body and the delimiter line', () => {
  const body = inlineOf('FROM scratch\nCOPY <<-EOF /a\n\t\tone\n\ttwo\n\tEOF\nEXPORT /\n');
  assert.equal(body.text, 'one\ntwo\n');
});

test('an empty body and CRLF line endings', () => {
  assert.equal(inlineOf('FROM scratch\nCOPY <<E /a\nE\nEXPORT /').text, '');
  assert.equal(inlineOf('FROM scratch\r\nCOPY <<E /a\r\nx\r\nE\r\nEXPORT /\r\n').text, 'x\n');
});

test('instructions after the body parse normally, and a heredoc can follow a continuation', () => {
  const prog = parseProgram('FROM scratch\nRENDER \\\n  <<T \\\n  TO /i.html\nhello\nT\nCOPY <<C /c\nc\nC\nEXPORT /\n');
  assert.deepEqual(prog.instructions.map((i) => i.kind), ['FROM', 'RENDER', 'COPY', 'EXPORT']);
  assert.equal(inlineOf('FROM scratch\nRENDER \\\n  <<T \\\n  TO /i.html\nhello\nT\nEXPORT /').bodyLine, 5);
});

test('quoted or mid-word << is not a heredoc', () => {
  const prog = parseProgram('FROM scratch\nCOPY "<<EOF" /a\nCOPY ./x<<y /b\nEXPORT /');
  assert.equal(prog.instructions.length, 4);
});

test('heredoc syntax errors', () => {
  syntaxError('FROM scratch\nCOPY <<EOF /a\nno end\n', 'E_SYNTAX', 2, 6);
  syntaxError('FROM scratch\nCOPY <<A <<B\nA\nEXPORT /', 'E_SYNTAX', 2, 10);
  syntaxError('FROM scratch\nCOPY ./a <<EOF\nx\nEOF\nEXPORT /', 'E_SYNTAX', 2, 10);
  syntaxError('FROM scratch AS s\nFROM scratch\nCOPY --from=s <<EOF /a\nx\nEOF\nEXPORT /', 'E_SYNTAX', 3, 6);
});

test('RENDER <<: the body is a template; errors point at Sitefile lines', async () => {
  const r = await buildSitefile(
    ['FROM scratch', 'ARG TITLE = "Hi"', 'RENDER --escape=html <<HTML TO /index.html', '<h1>${TITLE}</h1>', '$${raw}', 'HTML', 'EXPORT /'].join('\n'),
    { TITLE: '<b>' },
  );
  assert.equal(text(r.output.get('/index.html')?.bytes), '<h1>&lt;b&gt;</h1>\n${raw}\n');
  assert.match(r.output.get('/index.html')?.origin ?? '', /Sitefile:3 RENDER$/);

  await assert.rejects(
    buildSitefile('FROM scratch\nRENDER <<T TO /a\nfine\n  ${MISSING}\nT\nEXPORT /'),
    (e: unknown) => isSitefileError(e, 'E_UNDEFINED') && e.span?.file.endsWith('Sitefile') === true && e.span.line === 4 && e.span.column === 3,
  );
});

test('COPY <<: the body is written byte for byte', async () => {
  const r = await buildSitefile('FROM scratch\nCOPY <<CSS /css/site.css\na { content: "${not}"; }\nCSS\nEXPORT /');
  assert.equal(text(r.output.get('/css/site.css')?.bytes), 'a { content: "${not}"; }\n');
  assert.equal(r.output.get('/css/site.css')?.mode, 0o644);
});

test('COPY << needs a file destination and follows conflict rules', async () => {
  await assert.rejects(buildSitefile('FROM scratch\nCOPY <<E /dir/\nx\nE\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_PATH'));
  await assert.rejects(
    buildSitefile('FROM scratch\nCOPY <<E /d/a\nx\nE\nCOPY <<E /d\ny\nE\nEXPORT /'),
    (e: unknown) => isSitefileError(e, 'E_PATH'),
  );
  await assert.rejects(
    buildSitefile('FROM scratch\nCOPY <<E /a\nx\nE\nCOPY <<E /a\ny\nE\nEXPORT /'),
    (e: unknown) => isSitefileError(e, 'E_CONFLICT') && e.span?.line === 5,
  );
  const ok = await buildSitefile('FROM scratch\nCOPY <<E /a\nx\nE\nCOPY --overwrite <<E /a\ny\nE\nEXPORT /');
  assert.equal(text(ok.output.get('/a')?.bytes), 'y\n');
});

test('a << word in other instructions is not a heredoc', () => {
  const prog = parseProgram('FROM scratch\nLET X = <<EOF\nEXPORT /');
  assert.equal(prog.instructions.length, 3);
});
