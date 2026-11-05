// Regenerates the Markdown differential fixtures from the TypeScript renderer.
//
//   npm ci && npm run build        # at the repository root (builds lib/)
//   node rust/tests/fixtures/markdown/generate.mjs
//
// Writes, next to this script:
//   cases/<name>.md / .html / .excerpt  hand-written edge cases, the examples'
//                                        posts and the repository's docs
//   fuzz.json                            seeded random inputs [{input, html, excerpt}]
// The Rust test `markdown::tests::differential_fixtures` checks every one.
//
// For a larger local run (not committed):
//   FUZZ_OUT=/tmp/md-fuzz.json FUZZ_COUNT=100000 FUZZ_SEED=7 node rust/tests/fixtures/markdown/generate.mjs
//   SITEFILE_MD_FUZZ=/tmp/md-fuzz.json cargo test --release -- --ignored differential_external
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const { renderMarkdown } = await import(path.join(root, 'lib/src/markdown.js'));

const edge = [
  '', '\n\n  \n', 'one\ntwo\n\nthree', 'a\r\nb\r\n\r\nc', 'a\rb\r\rc', '# T\r\n\r\ntext',
  '# a\n## b ##\n###### f\n####### no', '#\n#nospace\n# *x* #', '# a # b', '#\t\ttab', '#  foo  ###  ', '# foo#', '## ###', '   ### x', '    # code',
  'Title\n=====\n\nSub\n---\n', 'multi\nline\n===', 'a\n  ===  ', 'a\n- - -', 'a\n***',
  'a  \nb', 'a\\\nb', 'a \nb', 'end  ', 'end\\', 'a   \n  b\\\nc',
  '---\n\n***\n\n___\n\n- - -', 'a\n\n--', ' ***', '    ***', '_ _ _ _', '*\t*\t*',
  '```js\nlet a = <b> & "c";\n\nx\n```', '~~~\n*not em*\n~~~', '````\n```\n````', '```\n```', '```c++ extra\nx\n```', '```a"b\nx\n```',
  '```<script>\nx\n```', '``` js `x\ny\n```', '```\nabc\n\ndef', '> ```\n> x', '  ```\n    indented\n  x\n  ```', '~~~~ ruby\n~~~\n~~~~~',
  '    a\n\n    b\n\npara', 'para\n    cont', '\ttabbed', '  \t mixed', '    a\n  \n    b\n\n\n',
  '> a\n> b', '> a\nlazy', '> # h\n>\n> - x\n>> deep', '> a\n\n> b', '>no space', '   > three', '> a\n# h', '> - a\nb', '>\ttab',
  '- a\n- b', '* a\n* b', '+ a', '- a\n* b', '1. a\n2. b', '1) a\n2) b', '3. a\n4. b', '0. a', '007. a', '1234567890. no',
  '- a\n\n- b', '- a\n\n  b\n- c', '- a\n  - b\n    - c\n- d', '1. a\n   - x\n2. b', '- a\n  - b\n\n  - c\n- d',
  '- one\n  two\n- three', '- one\ntwo', '- ```\n  x\n  ```', '- a\n\npara', 'text\n- a', 'text\n2. a', 'text\n1. a',
  '-\n- b', '-     five spaces', '-\tno', '- a\n---', '- a\n- - -', '1.  a\n\n    b', '- a\n\n\n- b', '- \n  x', '-\n\n  x',
  '*a* _b_ **c** __d__ ~~e~~', '***both***', '**a *b* c**', 'a*b*c', 'snake_case_word and 2*3*4', 'foo_bar_', '*a', '**a *b',
  'a * b * c', '~single~ ~~~triple~~~', '~~open', '*(a)*', '_(a)_', '**foo*bar**', '*foo**bar*', '***a** b*', '*a **b***',
  '_a_b', 'a_b_', '__a__b', '*"x"*', '"*x*"', 'é*x*é', '😀*x*😀', '*😀*', '_😀_', '**x**😀', '“*x*”', '*a *', '　*a*',
  '\\*not em\\* \\# \\\\ \\a', '\\<b\\>', '\\é', '`a *b* <i>` ``x ` y``', '` a `', '`unclosed', '`a\nb`', '`  `', '` `` `', '``a```b``',
  '[t](http://x.com)', '[t](/a/b "T \\"q\\"")', "[*t*](rel.html 'x')", '[t](#frag) [m](mailto:a@b.c) [p](http://x/(a))',
  '[t](<a b.html>)', '[t]()', '[t] (x) [u](x', '[unclosed(x)', '[a [b] c](u)', '[a `]` b](u)', '[a](u (t))', '[a](u"t")', '[a](  u  )',
  '[a](u\n"t")', '[x](\\(y\\))', '[a](b\\ c)', '[[a](b)](c)', '![a *b*](/i.png "T")', '![](x.png)', '![a"b](x.png)', '![a ![b](c)](d)',
  '<https://example.com/a?b=1&c=2>', '<me@example.com>', 'a < b > c', '<a@b>', '<x:y>', '<HTTP://X>', '<mailto:a@b.c>', '<a.b-c+d@e-f.g.h>',
  'a & b < c > "d" \'e\'', '&amp; &lt;', '<script>alert(1)</script>', '<div onclick="x">hi</div>', '*a* <img src=x onerror=alert(1)>',
  '# <b>x</b>', '> <iframe>', '<!-- c -->', '[x](javascript:alert(1))', '[x](JaVaScRiPt:alert(1))', '[x](<java\tscript:alert(1)>)',
  '[x](<  javascript:alert(1)>)', '[x](data:text/html;base64,AAAA)', '[x](vbscript:msgbox)', '[x](file:///etc/passwd)',
  '[x](<\u0001javascript:alert(1)>)', '[click *me*](javascript:alert(1))', '[x](<jav&#x09;ascript:alert(1)>)', '![alt text](javascript:alert(1))',
  '![a](data:image/svg+xml;base64,AAAA)', '<javascript:alert(1)>', '<data:text/html,x>', '[t](/a"onmouseover="x)', '[t](<a b"c> "ti\'tle")',
  "![a'b](x.png 'q\"r')", '[a](../x/y.html) [b](/abs) [c](?q=a:b) [d](x/y:z)', '[a](java­script:x)', '[a](​javascript:x)', '[a](ｊavascript:x)',
  '> '.repeat(40) + 'x', '['.repeat(40) + 'x' + '](y)'.repeat(40), '- '.repeat(40) + 'x', '*'.repeat(30) + 'a' + '*'.repeat(30),
  'x'.repeat(200), 'x'.repeat(201), 'y'.repeat(500), Array.from({ length: 60 }, (_, i) => `word${i}`).join(' '), '😀'.repeat(150),
  'a'.repeat(198) + '😀😀', 'a '.repeat(99) + '😀 b', 'nul\u0000byte', 'line sep', '# h x', '- a b', '  para',
  '# Title\n\nHello **bold** and `code` with [a link](/x)  \nand <b>raw</b>.\n\nSecond.', 'a   ![pic](x.png)\n\tb', '# Only heading',
];

const fuzzOut = process.env.FUZZ_OUT;
const casesDir = path.join(here, 'cases');
if (!fuzzOut) rmSync(casesDir, { recursive: true, force: true });
if (!fuzzOut) mkdirSync(casesDir, { recursive: true });

const inputs = [];
edge.forEach((src, i) => inputs.push([`edge-${String(i).padStart(3, '0')}`, src]));
for (const ex of readdirSync(path.join(root, 'examples'))) {
  const dir = path.join(root, 'examples', ex, 'posts');
  if (!statSync(dir, { throwIfNoEntry: false })?.isDirectory()) continue;
  for (const f of readdirSync(dir).sort()) if (f.endsWith('.md')) inputs.push([`example-${ex}-${f.replace(/\.md$/, '')}`, readFileSync(path.join(dir, f), 'utf8')]);
}
for (const f of ['README.md', ...readdirSync(path.join(root, 'docs')).sort().map((d) => `docs/${d}`)]) {
  if (!f.endsWith('.md')) continue;
  inputs.push([`doc-${f.replace(/[/.]/g, '-').replace(/-md$/, '')}`, readFileSync(path.join(root, f), 'utf8')]);
}
for (const [name, src] of fuzzOut ? [] : inputs) {
  const r = renderMarkdown(src);
  writeFileSync(path.join(casesDir, `${name}.md`), src);
  writeFileSync(path.join(casesDir, `${name}.html`), r.html);
  writeFileSync(path.join(casesDir, `${name}.excerpt`), r.excerpt);
}

// Seeded fuzz: mulberry32.
let seed = Number(process.env.FUZZ_SEED ?? 0x517ef11e);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const atoms = [
  '*', '**', '_', '__', '~~', '~', '`', '``', '[', ']', '(', ')', '](', '![', '<', '>', '#', '# ', '## ', '- ', '* ', '+ ', '1. ', '2) ',
  '> ', '  ', '    ', '\t', '\n', '\n', '\n\n', '\r\n', ' ', ' ', 'a', 'b', 'word', 'é', '😀', ' ', '　', '\\', '"', "'", ':', '!',
  'http://x.y/', 'javascript:', 'mailto:a@b.c', '<a@b.cd>', '<https://e.x/p>', '---', '***', '===', '```', '~~~', '&', '|', '.', ',', '\u0000',
  '(t)', '"t"', ' ', '“', '”', '—', '%', '$', '{', '}',
];
const fuzz = [];
for (let n = 0; n < Number(process.env.FUZZ_COUNT ?? 1500); n++) {
  const len = 1 + Math.floor(rand() * 40);
  let s = '';
  for (let k = 0; k < len; k++) s += atoms[Math.floor(rand() * atoms.length)];
  const r = renderMarkdown(s);
  fuzz.push({ input: s, html: r.html, excerpt: r.excerpt });
}
writeFileSync(fuzzOut ?? path.join(here, 'fuzz.json'), JSON.stringify(fuzz, null, 0) + '\n');
console.log(`${inputs.length} cases, ${fuzz.length} fuzz inputs`);
