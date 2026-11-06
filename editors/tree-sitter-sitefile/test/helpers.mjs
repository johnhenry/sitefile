import { execFileSync } from 'node:child_process';
import { mkdtempSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const grammarDir = fileURLToPath(new URL('..', import.meta.url));
export const repoRoot = path.resolve(grammarDir, '../..');
const cli = path.join(grammarDir, 'node_modules', '.bin', process.platform === 'win32' ? 'tree-sitter.cmd' : 'tree-sitter');

/** Run the tree-sitter CLI in the grammar directory; returns stdout (also on a non-zero exit). */
export function treeSitter(args, { allowFailure = false } = {}) {
  try {
    return execFileSync(cli, args, { cwd: grammarDir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 * 1024 * 1024 });
  } catch (err) {
    if (allowFailure && typeof err.stdout === 'string') return err.stdout;
    throw new Error(`tree-sitter ${args.join(' ')} failed:\n${err.stdout ?? ''}${err.stderr ?? ''}`);
  }
}

let lib;
/** Compile the parser once per test process; returns CLI arguments that select it. */
export function language() {
  if (!lib) {
    lib = path.join(mkdtempSync(path.join(tmpdir(), 'tree-sitter-sitefile-')), process.platform === 'win32' ? 'sitefile.dll' : 'sitefile.so');
    treeSitter(['build', '-o', lib, '.']);
  }
  return ['--lib-path', lib, '--lang-name', 'sitefile'];
}

/** Parse files with the grammar in this directory: path -> true when the tree has no ERROR/MISSING node. */
export function parseOk(files) {
  const out = treeSitter(['parse', ...language(), '--quiet', '--json-summary', ...files], { allowFailure: true });
  const json = JSON.parse(out.slice(out.indexOf('{')));
  return new Map(json.parse_summaries.map((s) => [path.resolve(grammarDir, s.file), s.successful]));
}

/** Every file named exactly `Sitefile` under `dir`. */
export function findSitefiles(dir) {
  const out = [];
  for (const name of readdirSync(dir).sort()) {
    const p = path.join(dir, name);
    if (statSync(p).isDirectory()) {
      if (name !== 'node_modules' && name !== 'dist') out.push(...findSitefiles(p));
    } else if (name === 'Sitefile') {
      out.push(p);
    }
  }
  return out;
}
