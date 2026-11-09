#!/usr/bin/env node
// Examples smoke test: builds every example with the TypeScript CLI and checks
// what each one is meant to show. Exits non-zero on the first failure.
//
//   npm run examples                 # all of them
//   node examples/run.mjs blog       # one
//
// Needs `npm run build` first (the `examples` script does it). Every build gets
// its own temporary output directory and SITEFILE_HOME, so nothing is written
// to the examples or to ~/.sitefile (except `bake`, whose outputs are its own
// gitignored examples/bake/dist/, removed afterwards).
// Both implementations building every example identically is checked by
// `npm run conformance`, not here.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const cli = path.join(here, '..', 'lib', 'src', 'cli.js');
const home = mkdtempSync(path.join(os.tmpdir(), 'sitefile-examples-home-'));
const temps = [home];

function run(args, cwd = here) {
  return execFileSync(process.execPath, [cli, ...args], {
    cwd,
    env: { ...process.env, SITEFILE_HOME: home, SOURCE_DATE_EPOCH: '' },
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

function build(name, args = []) {
  const out = mkdtempSync(path.join(os.tmpdir(), `sitefile-example-${name}-`));
  temps.push(out);
  run(['build', path.join(here, name), '-o', path.join(out, 'site'), '--force', ...args]);
  return path.join(out, 'site');
}

function files(dir, prefix = '') {
  return readdirSync(dir).flatMap((n) => {
    const abs = path.join(dir, n);
    return statSync(abs).isDirectory() ? files(abs, `${prefix}${n}/`) : [`${prefix}${n}`];
  });
}

const read = (dir, p) => readFileSync(path.join(dir, p), 'utf8');

const examples = {
  hello() {
    const site = build('hello', ['--build-arg', 'TITLE=Tom & <Jerry>']);
    assert.match(read(site, 'index.html'), /Tom &amp; &lt;Jerry&gt;/, 'build arg is substituted and HTML-escaped');
    assert.ok(files(site).includes('style.css'));
  },
  book() {
    const out = mkdtempSync(path.join(os.tmpdir(), 'sitefile-example-book-'));
    temps.push(out);
    run(['build', path.join(here, 'book'), '-o', path.join(out, 'book.epub'), '--force']);
    const epub = readFileSync(path.join(out, 'book.epub'));
    assert.equal(epub.subarray(30, 38).toString('latin1'), 'mimetype', 'EXPORT AS epub writes mimetype first');
    assert.equal(epub.subarray(38, 58).toString('latin1'), 'application/epub+zip', 'stored');
  },
  inline() {
    assert.ok(files(build('inline')).includes('index.html'), 'a template written inside the Sitefile renders');
  },
  multistage() {
    assert.ok(files(build('multistage')).length > 0, 'the last stage is exported');
  },
  'shared-stages'() {
    const site = build('shared-stages', ['--build-arg', 'ACCENT=teal']);
    assert.match(read(site, 'assets/theme.css'), /teal/, "an ARG reaches the imported Sitefile's stages");
  },
  blog() {
    const list = files(build('blog'));
    for (const p of ['index.html', 'feed.xml', 'sitemap.xml', 'asset-manifest.json']) assert.ok(list.includes(p), `blog writes ${p}`);
    assert.ok(list.some((p) => p.startsWith('posts/')) && list.some((p) => p.startsWith('tags/')), 'one page per post and per tag');
  },
  precompress() {
    const list = files(build('precompress'));
    assert.ok(list.includes('index.html.gz') && list.includes('style.css.gz'), 'PRECOMPRESS writes .gz sidecars');
    assert.ok(!list.includes('logo.svg.gz'), 'files under MIN are left alone');
  },
  data() {
    assert.ok(files(build('data')).length > 0, 'YAML, CSV and Web Bundle data render');
  },
  motion() {
    assert.throws(() => build('motion'), 'a JavaScript plugin does not run without --allow-plugin');
    assert.ok(files(build('motion', ['--allow-plugin', 'motion'])).some((p) => p.endsWith('.svg')), 'the plugin writes SVG');
  },
  canonical() {
    const site = build('canonical');
    assert.match(read(site, 'index.html'), /<link rel="canonical" href="https:\/\/example.com\/"><\/head>/, 'CANONICAL links the home page');
    assert.match(read(site, 'about.html'), /href="https:\/\/example.com\/about"/, 'PRETTY drops .html');
    assert.equal(read(site, 'old.html').match(/rel="canonical"/g).length, 1, 'a page with its own canonical link keeps it');
  },
  fileable() {
    // The plugin reads only its own folder, so fileable is vendored into it first (gitignored).
    execFileSync(process.execPath, [path.join(here, 'fileable', 'vendor.mjs')], { stdio: 'ignore' });
    assert.throws(() => build('fileable'), /--allow-plugin fileable/, 'a JavaScript plugin does not run without --allow-plugin');
    const site = build('fileable', ['--allow-plugin', 'fileable']);
    assert.deepEqual(files(site).sort(), ['docs/index.html', 'docs/pages/getting-started.html', 'docs/pages/limits.html', 'docs/pages/sandbox.html', 'docs/search.json', 'docs/style.css'], 'the fileable tree lands under TO /docs/');
    assert.match(read(site, 'docs/pages/sandbox.html'), /<strong>vendored<\/strong>/, 'markdown from the value import is rendered');
    assert.throws(
      () => build('fileable', ['--allow-plugin', 'fileable', '--build-arg', 'TEMPLATE=cmd.mjs']),
      (e) => /E_PLUGIN/.test(e.stderr) && /cannot start processes/.test(e.stderr),
      "fileable's cmd attribute fails clearly in the sandbox",
    );
  },
  wasm() {
    assert.ok(files(build('wasm')).some((p) => p.endsWith('.svg')), 'an import-free WebAssembly plugin runs with no flag');
  },
  image() {
    const list = files(build('image'));
    assert.ok(list.some((p) => /\.(png|jpe?g|webp)$/.test(p)), 'the image plugin writes resized images');
  },
  theme() {
    // Stored as an image only: themed-site starts FROM it.
    run(['build', path.join(here, 'theme'), '-t', 'theme:1']);
    assert.match(run(['images']), /theme\s+1/, 'the theme is in the image store');
  },
  'themed-site'() {
    if (!run(['images']).match(/theme\s+1/)) examples.theme();
    assert.ok(files(build('themed-site')).includes('index.html'), 'a site builds FROM a stored theme image');
  },
  bake() {
    const dist = path.join(here, 'bake', 'dist');
    rmSync(dist, { recursive: true, force: true });
    try {
      run(['bake'], path.join(here, 'bake'));
      const outs = readdirSync(dist).flatMap((env) => readdirSync(path.join(dist, env)).map((l) => `${env}/${l}`));
      assert.equal(outs.length, 4, `one output per matrix combination (got ${outs.join(', ')})`);
    } finally {
      rmSync(dist, { recursive: true, force: true });
    }
  },
};

// Every example directory is covered, and nothing else is listed.
const dirs = readdirSync(here).filter((n) => existsSync(path.join(here, n, 'Sitefile'))).sort();
assert.deepEqual(Object.keys(examples).sort(), dirs, 'examples/run.mjs lists exactly the example directories');

const only = process.argv.slice(2);
let failed = 0;
try {
  for (const [name, check] of Object.entries(examples)) {
    if (only.length && !only.includes(name)) continue;
    try {
      check();
      console.log(`ok   ${name}`);
    } catch (e) {
      failed++;
      console.log(`FAIL ${name}: ${e.stderr || e.message}`);
    }
  }
} finally {
  for (const d of temps) rmSync(d, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
