// Checks that PRECOMPRESS br writes the same bytes in both implementations over a corpus
// (johnhenry/sitefile#203): node:zlib's Brotli (src/precompress.ts) against the vendored C encoder
// the Rust port compiles (rust/vendor/brotli, rust/src/brotli.rs). Run it after updating either.
//
//   npm ci && npm run build                     # at the repository root (builds lib/)
//   node rust/tests/fixtures/brotli/corpus.mjs  # runs `cargo test --release` for the Rust side
//
// The corpus: the repository's text files (src, docs, examples, conformance cases, plugins, editors'
// grammars) plus generated inputs: empty, one byte, 1 MiB of zeros, seeded random bytes from 100 bytes to
// 17 MB (64 KiB +-1 included), seeded text from 1 KB to 5 MB, and a 2 MB concatenation of repository files
// and three copies of it. Nothing is committed; it prints how many outputs are identical and exits 1 on
// any difference.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../..');
const { brotli } = await import(pathToFileURL(path.join(root, 'lib/src/precompress.js')).href);
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'sitefile-brotli-'));

const files = [];
const walk = (d) => {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    const p = path.join(d, e.name);
    if (e.isDirectory()) {
      if (!['node_modules', 'target', 'lib', '.git'].includes(e.name)) walk(p);
    } else if (/\.(ts|md|html|css|js|json|svg|txt|xml|mjs|wasm)$/.test(e.name) && fs.statSync(p).size < 3e6) files.push(p);
  }
};
for (const sub of ['src', 'docs', 'examples', 'conformance/cases', 'plugins', 'editors/vscode/syntaxes']) walk(path.join(root, sub));

let seed = 12345;
const rnd = () => (seed = (seed * 1103515245 + 12345) >>> 0) / 2 ** 32;
const add = (name, buf) => {
  const p = path.join(tmp, name);
  fs.writeFileSync(p, buf);
  files.push(p);
};
add('empty', Buffer.alloc(0));
add('one', Buffer.from('a'));
add('zeros', Buffer.alloc(1 << 20));
for (const n of [100, 4096, 65535, 65536, 65537, 300000, 2_000_000, 17_000_000]) add(`rand${n}`, Buffer.from(Array.from({ length: n }, () => Math.floor(rnd() * 256))));
for (const n of [1000, 70000, 1_500_000, 5_000_000]) add(`text${n}`, Buffer.from(Array.from({ length: n }, () => 'abcdefghij klmnop\n<div class="x">'[Math.floor(rnd() * 32)]).join('')));
const concat = Buffer.concat(files.slice(0, 400).map((f) => fs.readFileSync(f)));
add('concat', concat);
add('concat3', Buffer.concat([concat, concat, concat]));

const list = path.join(tmp, 'list.txt');
fs.writeFileSync(list, files.join('\n') + '\n');
const out = path.join(tmp, 'rust');
fs.mkdirSync(out);
execFileSync('cargo', ['test', '--release', '--lib', 'brotli_corpus_external', '--', '--ignored'], {
  cwd: path.join(root, 'rust'),
  env: { ...process.env, SITEFILE_BROTLI_LIST: list, SITEFILE_BROTLI_OUT: out },
  stdio: ['ignore', 'ignore', 'inherit'],
});
let same = 0;
const differ = [];
files.forEach((f, i) => {
  if (Buffer.compare(Buffer.from(brotli(fs.readFileSync(f))), fs.readFileSync(path.join(out, `${i}.br`))) === 0) same++;
  else differ.push(path.relative(root, f));
});
fs.rmSync(tmp, { recursive: true, force: true });
console.log(`${same}/${files.length} identical (node:zlib brotli ${process.versions.brotli})`);
if (differ.length) {
  console.log(`different: ${differ.slice(0, 20).join(', ')}${differ.length > 20 ? ', …' : ''}`);
  process.exit(1);
}
