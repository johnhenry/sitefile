// Regenerates the HTML/CSS reference scanner differential fixture from the TypeScript reference
// (src/links.ts; its Rust port is rust/src/links.rs).
//
//   npm run build                                  # at the repository root (builds lib/)
//   node rust/tests/fixtures/links/generate.mjs
//
// Writes links.json next to this script: [{ kind, input, refs, rewritten, rewrittenBelow }], where
//   refs:      [[line, raw, url, css, target, target below /a], …] what the scanner finds (target:
//              resolveReference from /a/b.<ext> with the root / and with the root /a: "none", "external <url>",
//              or "internal <path>" with a trailing "/" for a directory)
//   rewritten: the text after renaming every internal file target to hashedName(path, "0123456789")
//              (null when nothing changed); rewrittenBelow: the same with the root /a.
// Checked by the Rust test `links_differential` (rust/tests/links.rs) and kept up to date by
// test/links-fixture.test.ts. For a larger local run (not committed):
//   FUZZ_OUT=/tmp/links.json FUZZ_COUNT=20000 FUZZ_SEED=9 node rust/tests/fixtures/links/generate.mjs
//   SITEFILE_LINKS_FUZZ=/tmp/links.json cargo test --test links
import { writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadLib, record } from './record.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '../../../..');
const lib = await loadLib(root);

let seed = Number(process.env.FUZZ_SEED ?? 0x11c5);
const rand = () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const int = (n) => Math.floor(rand() * n);
const pick = (xs) => xs[int(xs.length)];
const chance = (p) => rand() < p;

const URLS = [
  'a.css', '/style.css', 'css/site.css?v=1', '../img/x.png#frag', './a b.png', 'a%20b.png', 'caf%C3%A9.css', 'café.css', 'cafe\u0301.css',
  'img/logo@2x.png', 'caf&#xe9;&#46;png', '/a/b%2Ecss?v', 'x&#47;y.css', 'a.css&#63;v', 'https://e.x/a.css', 'HTTP://E.X/', '//cdn.e.x/a.js', 'mailto:me@e.x', 'data:image/png;base64,AAAA', '#top', '?q=1', '',
  'dir/', '/', '..', '../../../../x.js', 'LICENSE', '.env', 'a.min.js', 'x?a=1&amp;b=2', 'a&#46;css', 'a&#x2e;css', 'a&bogus;css', '%2e%2e/x.css',
  'a%2Ecss', 'a%ZZ.css', '%FF.css', 'x\\2e css', 'x\\).css', 'x\\\ny.css', '\\61 .css', 'a😀.png', 'javascript:alert(1)', 'C:/x.png', ' padded.css ',
];
const url = () => pick(URLS);
const quote = () => pick(['"', "'", '']);
const ws = () => pick(['', ' ', '  ', '\n', '\t', '\r\n']);

const HTML_PIECES = [
  () => `<link rel=stylesheet href=${quote()}${url()}${quote()}>`,
  () => { const q = pick(['"', "'"]); return `<img${ws() || ' '}src=${q}${url()}${q} alt="x">`; },
  () => `<img srcset="${url()} 1x,${ws()}${url()} 2x, ${url()}" sizes="(max-width: 1px) 100vw">`,
  () => `<source srcset="${url()},${url()} 100w">`,
  () => `<source srcset="${url()} (a,b) 2x">`,
  () => `<a href="${url()}">x</a>`,
  () => `<A HREF='${url()}'>`,
  () => `<base href="${url()}">`,
  () => `<object data="${url()}"></object><div data="${url()}">`,
  () => `<video poster=${url()} src="${url()}"></video>`,
  () => `<svg><use xlink:href="${url()}#i"/></svg>`,
  () => `<link rel=preload imagesrcset="${url()} 1x">`,
  () => `<script src="${url()}"></script>`,
  () => `<script>var s = '<img src="${url()}">';</script>`,
  () => `<style>${css()}</style>`,
  () => `<STYLE media=x>${css()}</style >`,
  () => `<p style="background:url(${url()})">`,
  () => `<p style='background: url("${url()}")'>`,
  () => `<!-- <img src="${url()}"> -->`,
  () => `<!doctype html>`,
  () => `<textarea><img src="${url()}"></textarea>`,
  () => `<title>${url()} <img src=x></title>`,
  () => `</div><img src = "${url()}" / >`,
  () => `<img =src="${url()}" src>`,
  () => `<ä src="${url()}">`,
  () => `<img src="${url()}`,
  () => `<`,
  () => `<!`,
  () => `<?xml ${url()}?>`,
  () => `text & more ${url()}`,
  () => `<img\nsrc\n=\n"${url()}"\n>`,
];
const CSS_PIECES = [
  () => `@import "${url()}";`,
  () => `@import '${url()}' screen;`,
  () => `@IMPORT url(${url()});`,
  () => `@import url("${url()}") layer(x);`,
  () => `a{background:url(${url()})}`,
  () => `a { background: URL( '${url()}' ) }`,
  () => `a{b:url(  ${url()}  )}`,
  () => `/* url(${url()}) */`,
  () => `a{content:"url(${url()})"}`,
  () => `.myurl(${url()})`,
  () => `x-url(${url()})`,
  () => `a{b:url("${url()}`,
  () => `a{b:url(${url()}`,
  () => `'unterminated\n`,
  () => `@media screen { a { b: url(${url()}) } }`,
  () => `@font-face{src:url(${url()}) format("woff2"),url('${url()}')}`,
  () => `a{b:image-set(url(${url()}) 1x)}`,
  () => `\\@import "${url()}";`,
  () => `@import${ws()}"${url()}";`,
  () => `\\😀url(${url()})`,
  () => `é url(${url()})`,
];
function css() {
  return Array.from({ length: 1 + int(4) }, () => pick(CSS_PIECES)() + ws()).join('');
}
function html() {
  return Array.from({ length: 1 + int(6) }, () => pick(HTML_PIECES)() + ws()).join('');
}

const NOISE = ['<', '>', '"', "'", '=', '/', '\\', '(', ')', ',', ';', '{', '}', '@', '&', '#', '*', ' ', '\n', 'é', '😀', 'url(', 'src=', '<!--', '-->', '</style>', '</script>'];
function mutate(s) {
  const n = int(3);
  for (let k = 0; k < n; k++) {
    const i = int(s.length + 1);
    const cp = [...s];
    const j = Math.min(cp.length, Math.floor((i / (s.length + 1)) * cp.length));
    if (chance(0.5)) cp.splice(j, 0, pick(NOISE));
    else cp.splice(j, 1 + int(3));
    s = cp.join('');
  }
  return s;
}

const count = Number(process.env.FUZZ_COUNT ?? 600);
const out = [];
for (let i = 0; i < count; i++) {
  const kind = chance(0.6) ? 'html' : 'css';
  let input = kind === 'html' ? html() : css();
  if (chance(0.35)) input = mutate(input);
  out.push(record(lib, kind, input));
}
const file = process.env.FUZZ_OUT ?? path.join(here, 'links.json');
writeFileSync(file, JSON.stringify(out, null, 0).replace(/\},\{"kind"/g, '},\n{"kind"') + '\n');
console.log(`wrote ${out.length} cases to ${file}`);
