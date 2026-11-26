import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build, planBuild, type BuildOptions } from '../src/executor.js';
import { DEFAULT_RECIPE_NAMES, findRecipeName, parseRecipeNames, recipeNames, resolveRecipe, resolveRecipe as resolve, setRecipeNames } from '../src/recipe.js';
import { SitefileError } from '../src/errors.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

/** A context holding a.txt and one recipe per [file name, text it writes]. */
async function site(recipes: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-recipe-'));
  temps.push(dir);
  await writeFile(path.join(dir, 'a.txt'), 'A');
  for (const [name, out] of Object.entries(recipes)) await writeFile(path.join(dir, name), `FROM scratch\nCOPY ./a.txt /${out}.txt\nEXPORT /\n`);
  return dir;
}

function opts(contextDir: string, recipeNames?: string[]): BuildOptions {
  return { contextDir, lockfile: false, ...(recipeNames ? { recipeNames } : {}) };
}

async function built(o: BuildOptions): Promise<string[]> {
  const r = await build(o);
  return [...r.output.entries()].map(([p]) => p).sort();
}

test('the default is Sitefile, and a process-wide list is used in order', async () => {
  // No other test in this file sets the list; each test file runs in its own process.
  assert.deepEqual(DEFAULT_RECIPE_NAMES, ['Sitefile']);
  assert.deepEqual(recipeNames(), ['Sitefile']);
  const dir = await site({ Sitefile: 'plain' });
  assert.equal(resolveRecipe(dir), path.join(dir, 'Sitefile'));
  assert.deepEqual(await built(opts(dir)), ['/plain.txt']);

  setRecipeNames(['Stockerfile', 'Sitefile']);
  try {
    assert.deepEqual(recipeNames(), ['Stockerfile', 'Sitefile']);
    assert.deepEqual(await built(opts(dir)), ['/plain.txt']);
    await writeFile(path.join(dir, 'Stockerfile'), 'FROM scratch\nCOPY ./a.txt /stocker.txt\nEXPORT /\n');
    assert.equal(resolveRecipe(dir), path.join(dir, 'Stockerfile'));
    assert.deepEqual(await built(opts(dir)), ['/stocker.txt']);
    assert.ok((await planBuild(opts(dir))).file.endsWith('Stockerfile'));
    // An explicit list on the options beats the process-wide one.
    assert.deepEqual(await built(opts(dir, ['Sitefile'])), ['/plain.txt']);
    setRecipeNames(['', 'a/b', '..', 'Stockerfile']);
    assert.deepEqual(recipeNames(), ['Stockerfile']);
  } finally {
    setRecipeNames([]);
  }
  assert.deepEqual(recipeNames(), ['Sitefile']);
});

test('names are tried in order, and a directory of that name is skipped', async () => {
  const dir = await site({ Third: 'third', Second: 'second' });
  assert.equal(resolve(dir, ['First', 'Second', 'Third']), path.join(dir, 'Second'));
  assert.equal(findRecipeName(dir, ['First', 'Second']), 'Second');
  assert.deepEqual(await built(opts(dir, ['Third', 'Second'])), ['/third.txt']);
  await mkdir(path.join(dir, 'Dir'));
  assert.equal(resolve(dir, ['Dir', 'Second']), path.join(dir, 'Second'));
  assert.deepEqual(parseRecipeNames(' Stockerfile, ,Sitefile,Stockerfile,a/b,..,x\\y'), ['Stockerfile', 'Sitefile']);
});

test('-f wins over the names', async () => {
  const dir = await site({ Stockerfile: 'stocker', Other: 'other' });
  assert.deepEqual(await built({ ...opts(dir, ['Stockerfile']), sitefile: path.join(dir, 'Other') }), ['/other.txt']);
  await assert.rejects(build({ ...opts(dir, ['Stockerfile']), sitefile: path.join(dir, 'Missing') }), (e: unknown) => e instanceof SitefileError && e.code === 'E_NOT_FOUND' && e.message.startsWith("cannot read Sitefile '"));
});

test('the error names the files tried; the default list keeps its message', async () => {
  const dir = await site({});
  await assert.rejects(build(opts(dir, ['Stockerfile', 'Sitefile'])), (e: unknown) => e instanceof SitefileError && e.code === 'E_NOT_FOUND' && e.message === `no recipe found in '${dir}' (tried: Stockerfile, Sitefile)`);
  assert.throws(() => resolve(dir, ['Stockerfile', 'Sitefile']), /\(tried: Stockerfile, Sitefile\)$/);
  await assert.rejects(build(opts(dir, [...DEFAULT_RECIPE_NAMES])), (e: unknown) => e instanceof SitefileError && e.message === `cannot read Sitefile '${path.join(dir, 'Sitefile')}'`);
});
