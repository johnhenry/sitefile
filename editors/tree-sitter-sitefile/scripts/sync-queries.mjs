#!/usr/bin/env node
// Copies the canonical queries in ../queries into each editor's layout,
// renaming highlight captures where the editor uses different names:
//
//   editors/zed/languages/sitefile/      highlights, injections, brackets, outline
//   editors/neovim/queries/sitefile/     highlights, injections, folds, locals
//   editors/helix/runtime/queries/sitefile/  highlights, injections, locals
//
// The canonical capture names follow nvim-treesitter. Run with --check to fail
// (exit 1) when a generated file is out of date instead of writing it.
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const grammarDir = fileURLToPath(new URL('..', import.meta.url));
const editors = path.resolve(grammarDir, '..');

/** Zed theme keys: https://zed.dev/docs/extensions/languages#syntax-highlighting */
const ZED = {
  'keyword.repeat': 'keyword',
  'keyword.conditional': 'keyword',
  'keyword.unsupported': 'keyword',
  'keyword.directive': 'preproc',
  module: 'variable.special',
  'function.call': 'function',
  'function.builtin': 'function',
  'variable.builtin': 'variable.special',
  'variable.member': 'property',
  'string.special.path': 'string.special',
  'string.special.url': 'link_uri',
};

/** Helix scopes: https://docs.helix-editor.com/themes.html#syntax-highlighting */
const HELIX = {
  'keyword.repeat': 'keyword.control.repeat',
  'keyword.conditional': 'keyword.control.conditional',
  'keyword.unsupported': 'keyword',
  module: 'namespace',
  'function.call': 'function',
  'variable.member': 'variable.other.member',
  property: 'variable.other.member',
  'string.escape': 'constant.character.escape',
  number: 'constant.numeric',
  boolean: 'constant.builtin.boolean',
  // locals: the suffix of a definition is the scope its references get
  'local.definition.var': 'local.definition.variable',
  'local.definition.import': 'local.definition.namespace',
  'local.definition.type': 'local.definition.label',
};

const TARGETS = [
  { dir: 'zed/languages/sitefile', files: ['highlights', 'injections', 'brackets', 'outline'], names: ZED },
  { dir: 'neovim/queries/sitefile', files: ['highlights', 'injections', 'folds', 'locals'], names: {} },
  { dir: 'helix/runtime/queries/sitefile', files: ['highlights', 'injections', 'locals'], names: HELIX },
];

function rename(text, names) {
  // @capture names (not @_private ones); longest match first so `@keyword.repeat` wins over `@keyword`.
  return text.replace(/@([A-Za-z][\w.]*)/g, (whole, name) => (Object.hasOwn(names, name) ? `@${names[name]}` : whole));
}

export function generate() {
  const out = new Map();
  for (const t of TARGETS) {
    for (const f of t.files) {
      const source = readFileSync(path.join(grammarDir, 'queries', `${f}.scm`), 'utf8');
      const header = `; Generated from editors/tree-sitter-sitefile/queries/${f}.scm by\n; editors/tree-sitter-sitefile/scripts/sync-queries.mjs. Do not edit.\n\n`;
      out.set(path.join(editors, t.dir, `${f}.scm`), header + rename(source, t.names));
    }
  }
  return out;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const check = process.argv.includes('--check');
  let stale = 0;
  for (const [file, text] of generate()) {
    const current = existsSync(file) ? readFileSync(file, 'utf8') : undefined;
    if (current === text) continue;
    if (check) {
      console.error(`out of date: ${path.relative(editors, file)}`);
      stale++;
    } else {
      mkdirSync(path.dirname(file), { recursive: true });
      writeFileSync(file, text);
      console.log(`wrote ${path.relative(editors, file)}`);
    }
  }
  if (stale) {
    console.error('run: npm run sync-queries (in editors/tree-sitter-sitefile)');
    process.exit(1);
  }
}
