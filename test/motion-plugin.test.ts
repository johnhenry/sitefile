import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const pluginUrl = new URL('../../examples/motion/plugins/motion.mjs', import.meta.url);
const pluginFile = fileURLToPath(pluginUrl);

interface MotionResult {
  bytes: string;
  mediaType?: string;
}
type Request = {
  apiVersion: 1;
  dest: string;
  options: Record<string, string>;
  input?: { path: string; name: string; bytes: Uint8Array; text?: string };
  data?: unknown;
  item?: unknown;
};
const plugin = (await import(pluginUrl.href)) as {
  apiVersion: number;
  transform(req: Request): MotionResult;
  default: { apiVersion: number; transform: unknown };
};

const STYLES = ['typewriter', 'fade', 'slide', 'wave', 'glitch'];

function render(options: Record<string, string> = {}, text = 'Hello'): string {
  const req: Request = { apiVersion: 1, dest: '/t.svg', options, input: { path: '/t.txt', name: 't.txt', bytes: new Uint8Array(), text } };
  return plugin.transform(req).bytes;
}

/** Tiny well-formedness check: balanced tags, legal attribute quoting, no stray `<` or `&`. */
function assertWellFormed(xml: string): void {
  const body = xml.replace(/^<\?xml[^>]*\?>\s*/, '');
  const stack: string[] = [];
  const token = /<(\/?)([A-Za-z][\w:-]*)((?:\s+[\w:-]+="[^"<]*")*)\s*(\/?)>|([^<]+)/gy;
  let pos = 0;
  while (pos < body.length) {
    token.lastIndex = pos;
    const m = token.exec(body);
    assert.ok(m, `malformed markup near: ${body.slice(pos, pos + 60)}`);
    pos = token.lastIndex;
    if (m[5] !== undefined) {
      // Text (including <style> CSS): every & must start an entity.
      assert.ok(!/&(?!(?:amp|lt|gt|quot|apos);)/.test(m[5]), `bare & in text: ${m[5].slice(0, 60)}`);
      continue;
    }
    const [, closing, name, , selfClosing] = m;
    if (closing) assert.equal(stack.pop(), name, `unbalanced </${name}>`);
    else if (!selfClosing) stack.push(name!);
  }
  assert.deepEqual(stack, [], 'unclosed tags');
}

test('plugin declares apiVersion 1 and a default export', () => {
  assert.equal(plugin.apiVersion, 1);
  assert.equal(plugin.default.apiVersion, 1);
  assert.equal(plugin.default.transform, plugin.transform);
});

for (const style of STYLES) {
  test(`style=${style} produces well-formed, animated SVG`, () => {
    const svg = render({ style }, 'Line one\nLine two');
    assertWellFormed(svg);
    assert.match(svg, /<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg"/);
    assert.match(svg, /@keyframes k0/);
    assert.match(svg, /<title id="t">Line one \/ Line two<\/title>/);
    assert.doesNotMatch(svg, /<script|href=|url\(http|@import/);
  });
}

test('result carries the SVG media type', () => {
  assert.equal(plugin.transform({ apiVersion: 1, dest: '/a.svg', options: { text: 'x' } }).mediaType, 'image/svg+xml');
});

test('text is XML-escaped in every style', () => {
  for (const style of STYLES) {
    const svg = render({ style }, `<script>alert("x")</script> & 'q'`);
    assertWellFormed(svg);
    assert.doesNotMatch(svg, /<script/);
    assert.match(svg, /&lt;script&gt;/);
  }
});

test('output is deterministic', () => {
  for (const style of STYLES) {
    const opts = { style, duration: '1.5', accent: '#f0f' };
    assert.equal(render(opts, 'a\nb\nc'), render(opts, 'a\nb\nc'));
  }
});

test('multi-line text gets one keyframe set per line, staggered by duration', () => {
  const svg = render({ style: 'fade', duration: '2' }, 'one\ntwo\n\nthree\n');
  for (const k of ['k0', 'k1', 'k2']) assert.match(svg, new RegExp(`@keyframes ${k}\\b`));
  assert.doesNotMatch(svg, /@keyframes k3\b/);
  // 3 lines * 2s + 1.6s hold
  assert.match(svg, /animation-duration:7\.6s/);
});

test('loop=false plays once and has no hold', () => {
  const svg = render({ loop: 'false', duration: '2' }, 'one\ntwo');
  assert.match(svg, /animation-iteration-count:1/);
  assert.match(svg, /animation-duration:4s/);
  const looped = render({ loop: 'true' }, 'one');
  assert.match(looped, /animation-iteration-count:infinite/);
});

test('prefers-reduced-motion disables animation', () => {
  for (const style of STYLES) {
    assert.match(render({ style }), /@media \(prefers-reduced-motion:reduce\)\{\*\{animation:none!important\}/);
  }
});

test('width and height are applied (default is OpenGraph size)', () => {
  const dflt = render();
  assert.match(dflt, /width="1200" height="630" viewBox="0 0 1200 630"/);
  const custom = render({ width: '800', height: '400' });
  assert.match(custom, /width="800" height="400" viewBox="0 0 800 400"/);
});

test('colors are applied', () => {
  const svg = render({ background: 'navy', color: 'rgb(1, 2, 3)', accent: '#abc' });
  assert.match(svg, /fill="navy"/);
  assert.match(svg, /--fg:rgb\(1, 2, 3\)/);
  assert.match(svg, /--ac:#abc/);
});

test('font is sanitized to [A-Za-z0-9 ,\'"-]', () => {
  const svg = render({ font: `Georgia, 'Times New Roman'; } </style><script>x</script> {` });
  assertWellFormed(svg);
  assert.doesNotMatch(svg, /<script|<\/style><|;\s*\}\s*<\/style>/);
  assert.match(svg, /font:700 \d+px Georgia, 'Times New Roman' stylescriptxscript;/);
  // Nothing usable left: fall back to the default stack.
  assert.match(render({ font: ';{}<>' }), /system-ui/);
});

test('long text shrinks to fit; size caps the font', () => {
  const big = render({ size: '40' }, 'x');
  assert.match(big, /font:700 40px/);
  const long = render({}, 'a very long headline that would never fit on a single line at the default size');
  const px = Number(/font:700 (\d+)px/.exec(long)![1]);
  assert.ok(px < 76 && px >= 8, `font size ${px}`);
});

test('text comes from input, then options.text, then item/data title', () => {
  const t = (req: Partial<Request>): string => plugin.transform({ apiVersion: 1, dest: '/t.svg', options: {}, ...req }).bytes;
  const input = { path: '/a', name: 'a', bytes: new Uint8Array(), text: 'from input' };
  assert.match(t({ input, options: { text: 'from options' } }), /<title id="t">from input</);
  assert.match(t({ options: { text: 'from options' }, item: { title: 'item' } }), /from options</);
  assert.match(t({ item: { title: 'from item' }, data: { title: 'data' } }), /from item</);
  assert.match(t({ data: { title: 'from data' } }), /from data</);
});

test('invalid options fail with clear messages', () => {
  const bad: Array<[Record<string, string>, RegExp]> = [
    [{ style: 'spin' }, /"style" must be one of typewriter, fade, slide, wave, glitch, got "spin"/],
    [{ duration: '0' }, /"duration" must be a number from 0\.2 to 60/],
    [{ duration: 'fast' }, /"duration"/],
    [{ loop: 'yes' }, /"loop" must be true or false/],
    [{ width: '12.5' }, /"width" must be an integer/],
    [{ height: '99999' }, /"height"/],
    [{ size: '2' }, /"size"/],
    [{ color: 'red; fill:url(x)' }, /"color" must be a CSS color/],
    [{ accent: '#12' }, /"accent" must be a CSS color/],
    [{ background: '<x>' }, /"background"/],
    [{ colour: 'red' }, /unknown option "colour"/],
  ];
  for (const [options, message] of bad) {
    assert.throws(() => render(options), (e: Error) => message.test(e.message) && /^motion plugin:/.test(e.message), JSON.stringify(options));
  }
  assert.throws(() => plugin.transform({ apiVersion: 1, dest: '/t.svg', options: {} }), /no text/);
  assert.throws(() => render({}, '   \n  '), /no text/);
});

test('runs under node --permission with read access to its own folder only', () => {
  const dir = path.dirname(pluginFile);
  const script = `
    const m = await import(${JSON.stringify(pluginUrl.href)});
    const r = m.transform({ apiVersion: 1, dest: '/x.svg', options: { style: 'glitch' }, input: { path: 'x', name: 'x', bytes: new Uint8Array(), text: 'sandboxed' } });
    process.stdout.write(r.bytes);
  `;
  const run = spawnSync(process.execPath, ['--permission', `--allow-fs-read=${dir}`, '--input-type=module', '-e', script], {
    encoding: 'utf8',
    env: {},
  });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout, render({ style: 'glitch' }, 'sandboxed'));
  // Sanity: the permission model really is on (reading outside the folder is denied).
  const probe = spawnSync(process.execPath, ['--permission', `--allow-fs-read=${dir}`, '-e', `require('node:fs').readFileSync(${JSON.stringify(path.resolve(dir, '../Sitefile'))})`], { encoding: 'utf8', env: {} });
  assert.notEqual(probe.status, 0);
});
