// Writes the gzip-wrapped Web Bundle fixtures (johnhenry/sitefile#141) with the tools that write
// that format, so a format change there fails a test here:
//
//   node test/fixtures/gzip-wbn/generate.mjs <fileable checkout, built> <packfile package dir> <out dir>
//
// then copy <out>/site.wbn to fileable-encode-wbn.wbn and <out>/packfile-toarchive.wbn here, and
// update expected.json (the files and tree digest IMPORT … AS tree reads; test/archive.test.ts,
// rust/src/resources/archive.rs and conformance/cases/import-gzip-wbn check it).
// Last written with @johnhenry/fileable 13749b2 (<Dir encode="wbn">) and @johnhenry/packfile 0.2.0 (toArchive).
import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
const [fileableDir, packfileDir, out] = process.argv.slice(2);
const files = { 'index.html': '<!doctype html><h1>gzip bundle</h1>\n', 'about/index.html': '<h1>About</h1>\n', 'css/site.css': 'body{margin:0}\n' };
const { render } = await import(path.join(fileableDir, 'dist/src/render.js'));
const file = (name, text) => ({ tag: 'file', props: { name }, children: [text] });
const site = { tag: 'dir', props: { name: 'site', encode: 'wbn' }, children: [file('index.html', files['index.html']), { tag: 'dir', props: { name: 'about' }, children: [file('index.html', files['about/index.html'])] }, { tag: 'dir', props: { name: 'css' }, children: [file('site.css', files['css/site.css'])] }] };
await render(site, { outDir: out, cache: false });
const { fromFiles, toArchive } = await import(path.join(packfileDir, 'index.mjs'));
await writeFile(path.join(out, 'packfile-toarchive.wbn'), await toArchive(await fromFiles(files)));
console.log('ok');
