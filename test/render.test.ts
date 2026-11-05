import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isSitefileError, type SourceSpan } from '../src/errors.js';
import {
  decodeUtf8,
  escapeHtml,
  parseRenderTemplate,
  renderTemplate,
  resolveJsonPath,
  type TemplateLookup,
} from '../src/render.js';

const vars: Record<string, string | number | boolean> = { TITLE: 'Hello', N: 3, B: true, X: '<a href="x">&\'</a>' };
const lookup: TemplateLookup = (name, path) => {
  if (path.length) return `${name}.${path.join('.')}`;
  const v = vars[name];
  if (v === undefined) throw new Error(`undefined ${name}`);
  return v;
};
const render = (t: string, escape: 'none' | 'html' = 'none') => renderTemplate(parseRenderTemplate(t, 't.html'), lookup, escape);

function errorAt(fn: () => unknown, code: string, line?: number, column?: number): void {
  assert.throws(fn, (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code);
    if (line !== undefined) {
      assert.equal(e.span?.file, 't.html');
      assert.equal(e.span?.line, line);
      assert.equal(e.span?.column, column);
    }
    return true;
  });
}

test('substitutes variables', () => {
  assert.equal(render('<h1>${TITLE}</h1> ${N} ${B}'), '<h1>Hello</h1> 3 true');
  assert.equal(render('${TITLE}${TITLE}'), 'HelloHello');
  assert.equal(render(''), '');
});

test('parses dotted paths with spans and merges text', () => {
  const parts = parseRenderTemplate('a\n  ${data.site.0.my-key} b', 'f');
  assert.deepEqual(parts, [
    { kind: 'text', value: 'a\n  ' },
    { kind: 'var', name: 'data', path: ['site', '0', 'my-key'], span: { file: 'f', line: 2, column: 3 } },
    { kind: 'text', value: ' b' },
  ]);
  assert.deepEqual(parseRenderTemplate('x $ $${y} z', 'f'), [{ kind: 'text', value: 'x $ ${y} z' }]);
});

test('$${ is a literal ${ and a lone $ is literal', () => {
  assert.equal(render('$${TITLE}'), '${TITLE}');
  assert.equal(render('cost: $5, $ $a $}'), 'cost: $5, $ $a $}');
  assert.equal(render('end$'), 'end$');
  assert.equal(render('$$${TITLE}'), '$${TITLE}');
});

test('invalid or unterminated expressions report line and column', () => {
  errorAt(() => parseRenderTemplate('ok\nab ${1bad}', 't.html'), 'E_TEMPLATE', 2, 4);
  errorAt(() => parseRenderTemplate('ok\r\nline\r\n  ${a b}', 't.html'), 'E_TEMPLATE', 3, 3);
  errorAt(() => parseRenderTemplate('x\r\n${unterminated', 't.html'), 'E_TEMPLATE', 2, 1);
  errorAt(() => parseRenderTemplate('${a\n}', 't.html'), 'E_TEMPLATE', 1, 1);
  errorAt(() => parseRenderTemplate('${}', 't.html'), 'E_TEMPLATE', 1, 1);
  errorAt(() => parseRenderTemplate('${a.}', 't.html'), 'E_TEMPLATE', 1, 1);
  errorAt(() => parseRenderTemplate('${a..b}', 't.html'), 'E_TEMPLATE', 1, 1);
  errorAt(() => parseRenderTemplate('${ a }', 't.html'), 'E_TEMPLATE', 1, 1);
});

test('var spans on CRLF templates', () => {
  const parts = parseRenderTemplate('a\r\nb\r\n xx${V}', 'f');
  const v = parts.find((p) => p.kind === 'var');
  assert.ok(v && v.kind === 'var');
  assert.deepEqual(v.span, { file: 'f', line: 3, column: 4 } satisfies SourceSpan);
});

test('html escaping applies to values only', () => {
  assert.equal(render('<p title="${X}">&amp;</p>', 'html'), '<p title="&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;">&amp;</p>');
  assert.equal(render('<p>${X}</p>', 'none'), '<p><a href="x">&\'</a></p>');
  assert.equal(escapeHtml(`&<>"'x`), '&amp;&lt;&gt;&quot;&#39;x');
});

test('lookup errors propagate', () => {
  assert.throws(() => render('${MISSING}'), /undefined MISSING/);
});

test('decodeUtf8 strips a BOM and rejects invalid bytes', () => {
  assert.equal(decodeUtf8(new Uint8Array([0xef, 0xbb, 0xbf, 0x68, 0x69]), 'tpl'), 'hi');
  assert.equal(decodeUtf8(new TextEncoder().encode('héllo ✓'), 'tpl'), 'héllo ✓');
  const span = { file: 'Sitefile', line: 4, column: 1 };
  assert.throws(
    () => decodeUtf8(new Uint8Array([0x68, 0xff, 0x69]), "template './x.html'", span),
    (e: unknown) => isSitefileError(e, 'E_ENCODING') && e.span === span && /x\.html/.test(e.message),
  );
  assert.throws(() => decodeUtf8(new Uint8Array([0xc3]), 'tpl'), (e: unknown) => isSitefileError(e, 'E_ENCODING'));
});

test('resolveJsonPath', () => {
  const span = { file: 'Sitefile', line: 1, column: 1 };
  const data = { site: { title: 'T', n: 2, ok: false, tags: ['a', 'b'], nil: null, obj: {} }, 'my-key': 'k' };
  assert.equal(resolveJsonPath(data, ['site', 'title'], 'data', span), 'T');
  assert.equal(resolveJsonPath(data, ['site', 'n'], 'data', span), 2);
  assert.equal(resolveJsonPath(data, ['site', 'ok'], 'data', span), false);
  assert.equal(resolveJsonPath(data, ['site', 'tags', '1'], 'data', span), 'b');
  assert.equal(resolveJsonPath(data, ['my-key'], 'data', span), 'k');
  assert.equal(resolveJsonPath('plain', [], 'data', span), 'plain');

  const fails = (path: string[], code: string, re: RegExp) =>
    assert.throws(
      () => resolveJsonPath(data, path, 'data', span),
      (e: unknown) => isSitefileError(e) && e.code === code && re.test(e.message) && e.span === span,
    );
  fails(['site', 'missing'], 'E_UNDEFINED', /data\.site\.missing/);
  fails(['site', 'tags', '5'], 'E_UNDEFINED', /data\.site\.tags\.5/);
  fails(['site', 'tags', 'x'], 'E_UNDEFINED', /data\.site\.tags\.x/);
  fails(['site', 'tags', '01'], 'E_UNDEFINED', /data\.site\.tags\.01/);
  fails(['site', 'title', 'x'], 'E_UNDEFINED', /data\.site\.title\.x/);
  fails(['site', 'toString'], 'E_UNDEFINED', /data\.site\.toString/);
  fails(['site', 'nil'], 'E_TYPE', /data\.site\.nil/);
  fails(['site', 'obj'], 'E_TYPE', /data\.site\.obj/);
  fails(['site', 'tags'], 'E_TYPE', /data\.site\.tags/);
  fails(['site', 'nil', 'x'], 'E_UNDEFINED', /data\.site\.nil\.x/);
  assert.throws(() => resolveJsonPath(data, [], 'data', span), (e: unknown) => isSitefileError(e, 'E_TYPE'));
});
