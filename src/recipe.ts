import { statSync } from 'node:fs';
import path from 'node:path';
import { SitefileError } from './errors.js';

/**
 * The default recipe file name (issue #332; port of `rust/src/recipe.rs`).
 *
 * Without `-f`, a build reads the recipe from the context directory: the first of an ordered list of
 * names that is a file there, `["Sitefile"]` by default. Code that embeds the library can change the list
 * for one build (`BuildOptions.recipeNames`) or for the whole process (`setRecipeNames`); a CLI user can
 * opt in with `$SITEFILE_RECIPE_NAMES` (comma-separated). The list in force is, in order: the one given
 * for this call, the one set with `setRecipeNames`, `$SITEFILE_RECIPE_NAMES`, then the default.
 * An explicit `-f` always wins. Only the recipe is renamed: `Sitefile.lock` and the `IMPORT … AS sitefile`
 * lookup keep their names.
 */
export const DEFAULT_RECIPE_NAMES: readonly string[] = ['Sitefile'];

/** `$SITEFILE_RECIPE_NAMES`: comma-separated recipe names, searched in order. */
export const RECIPE_NAMES_ENV = 'SITEFILE_RECIPE_NAMES';

let processNames: string[] | undefined;

/** A recipe name is a bare file name: not empty, no path separator, not `.` or `..`. */
function isValidName(n: string): boolean {
  return n !== '' && n !== '.' && n !== '..' && !/[/\\\0]/.test(n);
}

/** The valid names of `names`, trimmed, without repeats, in order. */
function clean(names: readonly string[]): string[] {
  const out: string[] = [];
  for (const raw of names) {
    const n = raw.trim();
    if (isValidName(n) && !out.includes(n)) out.push(n);
  }
  return out;
}

/** Parse a comma-separated list (`$SITEFILE_RECIPE_NAMES`), dropping empty and invalid entries. */
export function parseRecipeNames(list: string): string[] {
  return clean(list.split(','));
}

/**
 * Set the recipe names for the whole process, replacing an earlier call. Entries that are not bare file
 * names are dropped; a list with none left restores the default (the environment, then `DEFAULT_RECIPE_NAMES`).
 */
export function setRecipeNames(names: readonly string[]): void {
  const v = clean(names);
  processNames = v.length > 0 ? v : undefined;
}

/** The names in force for a call that gave none. Never empty. */
export function recipeNames(): string[] {
  if (processNames) return [...processNames];
  const raw = process.env[RECIPE_NAMES_ENV];
  if (raw !== undefined) {
    const v = parseRecipeNames(raw);
    if (v.length > 0) return v;
  }
  return [...DEFAULT_RECIPE_NAMES];
}

/** `names` when given and non-empty after cleaning, else `recipeNames()`. */
export function effectiveRecipeNames(names?: readonly string[]): string[] {
  const v = names ? clean(names) : [];
  return v.length > 0 ? v : recipeNames();
}

/** Whether `names` is exactly the default list (the one whose "not found" message is the original one). */
export function isDefaultRecipeNames(names: readonly string[]): boolean {
  return names.length === DEFAULT_RECIPE_NAMES.length && names.every((n, i) => n === DEFAULT_RECIPE_NAMES[i]);
}

export function isFileSync(p: string): boolean {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
}

/** The first of `names` (else the names in force) that is a file in `dir`. */
export function findRecipeName(dir: string, names?: readonly string[]): string | undefined {
  return effectiveRecipeNames(names).find((n) => isFileSync(path.join(dir, n)));
}

/** `no recipe found in '<dir>' (tried: A, B)`; without a directory `no recipe found (tried: A, B)`. */
export function recipeNotFound(dir: string | undefined, names: readonly string[]): SitefileError {
  const tried = names.join(', ');
  return new SitefileError('E_NOT_FOUND', dir !== undefined ? `no recipe found in '${dir}' (tried: ${tried})` : `no recipe found (tried: ${tried})`);
}

/** The recipe of `contextDir`: the first of the names that exists there. `E_NOT_FOUND` naming every file tried when none does. */
export function resolveRecipe(contextDir: string, names?: readonly string[]): string {
  const list = effectiveRecipeNames(names);
  const found = list.find((n) => isFileSync(path.join(contextDir, n)));
  if (found === undefined) throw recipeNotFound(contextDir, list);
  return path.join(contextDir, found);
}

/** The recipe path a build of `contextDir` reads: the first existing name, else the first name (so the read that follows fails naming it). */
export function recipePath(contextDir: string, names?: readonly string[]): string {
  const list = effectiveRecipeNames(names);
  return path.join(contextDir, list.find((n) => isFileSync(path.join(contextDir, n))) ?? list[0]!);
}
