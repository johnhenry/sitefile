#!/usr/bin/env node
// Vendors @johnhenry/fileable into the plugin's folder (johnhenry/sitefile#145).
//
//   node examples/fileable/vendor.mjs      # after npm ci at the repo root
//
// A JavaScript plugin can read only its own folder, so fileable has to be
// there. This copies exactly the files Node loads for
// `import "@johnhenry/fileable/browser"` (fileable's in-memory entry point and
// the parts of marked, minimatch and node-html-parser it uses), plus their
// package.json files, from the repo's node_modules (where fileable is a
// devDependency, never a runtime one) into
// plugins/fileable/node_modules/. The result is gitignored; rerunning it
// replaces it. No bundler and no network: the same installed version always
// gives the same files.
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const target = path.join(here, 'plugins', 'fileable', 'node_modules');

const loaded = new Set();
registerHooks({
  load(url, context, next) {
    if (url.startsWith('file:')) loaded.add(fileURLToPath(url));
    return next(url, context);
  },
});

let entry;
try {
  entry = import.meta.resolve('@johnhenry/fileable/browser');
} catch {
  console.error('vendor.mjs: @johnhenry/fileable is not installed; run `npm ci` at the repository root first');
  process.exit(1);
}
await import(entry);

// The node_modules directory fileable was resolved from.
const entryPath = fileURLToPath(entry);
const source = entryPath.slice(0, entryPath.lastIndexOf(`${path.sep}node_modules${path.sep}`) + `${path.sep}node_modules`.length);

const files = new Set();
for (const file of loaded) {
  const rel = path.relative(source, file);
  if (rel.startsWith('..') || path.isAbsolute(rel)) continue; // this script itself
  files.add(rel);
  // Every package.json between the file and its package root: Node reads them
  // for "exports" and "type".
  const parts = rel.split(path.sep);
  const rootDepth = parts[0].startsWith('@') ? 2 : 1;
  for (let i = parts.length - 1; i >= rootDepth; i--) {
    const pj = path.join(...parts.slice(0, i), 'package.json');
    if (existsSync(path.join(source, pj))) files.add(pj);
  }
  const license = ['LICENSE', 'LICENSE.md', 'LICENSE.txt'].map((l) => path.join(...parts.slice(0, rootDepth), l)).find((l) => existsSync(path.join(source, l)));
  if (license) files.add(license);
}

rmSync(target, { recursive: true, force: true });
const sorted = [...files].sort();
for (const rel of sorted) {
  mkdirSync(path.dirname(path.join(target, rel)), { recursive: true });
  cpSync(path.join(source, rel), path.join(target, rel));
}
const version = JSON.parse(readFileSync(path.join(source, '@johnhenry', 'fileable', 'package.json'), 'utf8')).version;
writeFileSync(path.join(target, '.vendored.json'), `${JSON.stringify({ '@johnhenry/fileable': version, files: sorted.map((f) => f.split(path.sep).join('/')) }, null, 2)}\n`);
console.log(`vendored @johnhenry/fileable ${version}: ${sorted.length} files into ${path.relative(process.cwd(), target) || target}`);
