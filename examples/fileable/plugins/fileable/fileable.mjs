// A Sitefile plugin that renders a @johnhenry/fileable tree straight into the
// build (johnhenry/sitefile#145): no directory or zip on disk in between.
//
//   PLUGIN fileable FROM ./plugins/fileable/fileable.mjs
//   TRANSFORM docs: WITH fileable TO /docs/ OPTIONS template=docs.mjs
//
// `template` names a module in ./templates/ (next to this file). Its default
// export receives { options, data, item, input, dest } and returns a fileable
// tree (Dir/File/Rm descriptors, built with JSX compiled ahead of time or with
// fileable's h()/dir()/file() builders). The tree is rendered in memory with
// fileable's toFiles() and returned as { files }, so its paths land under the
// TRANSFORM's TO directory.
//
// fileable is not a dependency of Sitefile. The plugin runs sandboxed: it can
// read only this folder, so fileable is vendored into ./node_modules/ by
// examples/fileable/vendor.mjs (gitignored, rebuilt from the devDependency).
// It uses fileable's browser entry point, which needs no filesystem, process
// or environment access; whatever needs those (`cmd`, local `src=` paths,
// `env://`, `<Dir from>`, `encode="zip"`) is refused with a clear error.
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const apiVersion = 1;
// The same request always gives the same bytes, so results may be cached.
export const cache = true;

const here = path.dirname(fileURLToPath(import.meta.url));

let fileable;
async function loadFileable() {
  try {
    fileable ??= await import('@johnhenry/fileable/browser');
  } catch (e) {
    if (e?.code === 'ERR_MODULE_NOT_FOUND') {
      throw new Error('@johnhenry/fileable is not vendored into this plugin folder: run `node examples/fileable/vendor.mjs` first');
    }
    throw e;
  }
  return fileable;
}

const TEMPLATE_NAME = /^[A-Za-z0-9_][A-Za-z0-9_.-]*\.m?js$/;

// Where a fileable feature needs something the plugin sandbox does not give.
const REFUSED = [
  [/`cmd`/, "fileable's cmd attribute runs a shell command, and a Sitefile plugin cannot start processes. Compute the content in the template, or run the program with TOOL and --allow-exec (docs/PLUGINS.md)"],
  [/local filesystem `src=`|compiled-module `src=`|<Dir from|<Dir src decode/, 'fileable cannot read files here: a Sitefile plugin reads only its own folder. Pass the content in as the TRANSFORM source or a value import (request.input / request.data) instead'],
  [/env:\/\//, 'a Sitefile plugin gets an empty environment, so src="env://…" has nothing to read. Pass the value with OPTIONS or an ARG instead'],
  [/encode="(zip|wbn)"/, 'encode="zip" and encode="wbn" are not available in memory. Return loose files and let Sitefile write the archive (sitefile build -o site.zip)'],
];

export async function transform(request) {
  const name = request.options?.template;
  if (!name) throw new Error('OPTIONS template=<file> is required: a module in the plugin\'s templates/ folder');
  if (!TEMPLATE_NAME.test(name)) throw new Error(`template "${name}" must be a file name in templates/ ending in .mjs or .js (no directories)`);

  const { toFiles } = await loadFileable();
  const mod = await import(pathToFileURL(path.join(here, 'templates', name)).href);
  if (typeof mod.default !== 'function') throw new Error(`template ${name} must export a default function that returns a fileable tree`);

  const { options, data, item, input, dest } = request;
  try {
    const tree = await mod.default({ options, data, item, input, dest });
    return { files: await toFiles(tree) };
  } catch (e) {
    const message = String(e?.message ?? e);
    const hit = REFUSED.find(([re]) => re.test(message));
    if (hit) throw new Error(`template ${name}: ${hit[1]} (fileable: ${message.split('\n')[0]})`);
    throw new Error(`template ${name}: ${message}`);
  }
}
