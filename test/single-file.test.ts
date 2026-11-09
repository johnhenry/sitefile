import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeMhtml, writeSingleFile } from '../src/single-file.js';
import { encodeTree, formatFromExtension, defaultOutName } from '../src/export-formats.js';
import { Tree, makeFile } from '../src/tree.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const b64 = (s: string | Uint8Array): string => Buffer.from(s).toString('base64');

function site(extra: Array<[string, string]> = []): Tree {
  const files: Array<[string, string]> = [
    [
      '/index.html',
      '<!doctype html><link rel="stylesheet" href="css/site.css"><link rel="canonical" href="/index.html">' +
        '<script src="/app.js"></script><img src="img/a.png" srcset="img/a.png 1x, img/b.png 2x" alt="a">' +
        '<a href="/about.html">about</a><a href="https://example.com/">ext</a><img src="https://cdn.example/x.png">' +
        '<div style="background:url(img/a.png)"></div><svg><use xlink:href="icons.svg#i"/></svg>',
    ],
    ['/css/site.css', '@import "fonts.css";\nbody { background: url("../img/b.png") }'],
    ['/css/fonts.css', '@font-face { src: url(/f.woff2) }'],
    ['/app.js', 'console.log(1)'],
    ['/img/a.png', 'PNG-A'],
    ['/img/b.png', 'PNG-B'],
    ['/f.woff2', 'FONT'],
    ['/icons.svg', '<svg/>'],
    ['/about.html', 'about'],
    ...extra,
  ];
  return Tree.fromFiles(files.map(([p, s]) => [p, makeFile(enc.encode(s), 't')]));
}

test('html: every asset becomes a data: URI; navigation, pages and external URLs stay', () => {
  const out = dec.decode(writeSingleFile(site()));
  const png = (s: string): string => `data:image/png;base64,${b64(s)}`;
  const fonts = `data:text/css;charset=utf-8;base64,${b64('@font-face { src: url(data:font/woff2;base64,' + b64('FONT') + ') }')}`;
  const css = `data:text/css;charset=utf-8;base64,${b64(`@import "${fonts}";\nbody { background: url("${png('PNG-B')}") }`)}`;
  assert.equal(
    out,
    `<!doctype html><link rel="stylesheet" href="${css}"><link rel="canonical" href="/index.html">` +
      `<script src="data:text/javascript;charset=utf-8;base64,${b64('console.log(1)')}"></script><img src="${png('PNG-A')}" srcset="${png('PNG-A')} 1x, ${png('PNG-B')} 2x" alt="a">` +
      '<a href="/about.html">about</a><a href="https://example.com/">ext</a><img src="https://cdn.example/x.png">' +
      `<div style="background:url(${png('PNG-A')})"></div><svg><use xlink:href="data:image/svg+xml;charset=utf-8;base64,${b64('<svg/>')}#i"/></svg>`,
  );
  assert.deepEqual(encodeTree(site(), 'html'), writeSingleFile(site()));
});

test('html and mhtml: a missing asset, a directory or an import cycle fails with E_EXPORT', () => {
  const t = (html: string, extra: Array<[string, string]> = []): Tree =>
    Tree.fromFiles([['/index.html', html], ...extra].map(([p, s]) => [p as string, makeFile(enc.encode(s as string), 't')]));
  assert.throws(() => writeSingleFile(t('<img src="nope.png">')), /cannot inline 'nope.png' in \/index.html:1: no file \/nope.png/);
  assert.throws(() => writeMhtml(t('<img src="nope.png">')), /no file \/nope.png/);
  assert.throws(() => writeSingleFile(t('<img src="img/">', [['/img/a', 'x']])), /names a directory/);
  assert.throws(() => writeSingleFile(t('<link rel=stylesheet href=a.css>', [['/a.css', '@import "b.css";'], ['/b.css', '@import "a.css";']])), /cycle/);
  assert.throws(() => writeSingleFile(Tree.fromFiles([['/x.html', makeFile(enc.encode(''), 't')]])), /has no \/index.html/);
});

test('mhtml: the page and each asset once, base64, under the base URL', () => {
  const out = dec.decode(writeMhtml(site(), undefined, 'https://example.org/'));
  assert.match(out, /^From: <Saved by sitefile>\r\nSnapshot-Content-Location: https:\/\/example.org\/index.html\r\nMIME-Version: 1.0\r\nContent-Type: multipart\/related;\r\n\ttype="text\/html";\r\n\tboundary="----MultipartBoundary--sitefile----"\r\n\r\n/);
  const locations = [...out.matchAll(/^Content-Location: (\S+)/gm)].map((m) => m[1]);
  assert.deepEqual(locations, [
    'https://example.org/index.html',
    'https://example.org/css/site.css',
    'https://example.org/css/fonts.css',
    'https://example.org/f.woff2',
    'https://example.org/img/b.png',
    'https://example.org/app.js',
    'https://example.org/img/a.png',
    'https://example.org/icons.svg',
  ]);
  assert.ok(out.endsWith('------MultipartBoundary--sitefile------\r\n'));
  assert.ok(dec.decode(writeMhtml(site())).includes('Content-Location: https://site.invalid/index.html'));
  assert.throws(() => writeMhtml(site(), undefined, 'http://x/'), /must be https/);
});

test('format names and extensions', () => {
  assert.equal(formatFromExtension('a.MHTML'), 'mhtml');
  assert.equal(formatFromExtension('a.mht'), 'mhtml');
  assert.equal(formatFromExtension('a.html'), undefined);
  assert.equal(defaultOutName('html'), 'dist.html');
});
