import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { SitefileError } from './errors.js';
import { collectBuildArgs } from './build-args.js';
import { planBuild, sitefileCompression } from './executor.js';
import { defaultOutName } from './export-formats.js';
import { EXPORT_FORMATS, parseExportFormat } from './language/ast.js';
import { parseYaml } from './yaml.js';

/**
 * `sitefile bake`: build several variants of a site from one bake file
 * (`sitefile.bake.yaml`, `.yml` or `.json`), like `docker buildx bake`.
 *
 * The bake file names targets (each one a `sitefile build`: context, Sitefile,
 * build args, arg files, `--target` stage, output, format, tags), optional
 * `matrix` values that expand one target into several, `inherits` and groups.
 * This module only reads and resolves the file into a {@link BakePlan}; the CLI
 * (src/cli/bake.ts) runs the builds. No IO besides reading the bake file.
 */

/** The default bake file names, looked up in this order in the current directory. */
export const BAKE_FILES: readonly string[] = ['sitefile.bake.yaml', 'sitefile.bake.yml', 'sitefile.bake.json'];

/** One build of a bake plan. Paths are as the CLI passes them to `build` (joined to the bake file's directory). */
export interface BakeTarget {
  readonly name: string;
  readonly context: string;
  readonly file: string;
  /** Build args in definition order (inherited ones first). */
  readonly args: Readonly<Record<string, string>>;
  readonly argFiles: readonly string[];
  /** `--target <stage>`. */
  readonly target: string | null;
  readonly output: string;
  /** No `output` in the bake file: `output` is the default name, `dist/<name>` plus the format's extension. */
  readonly defaultOutput: boolean;
  readonly format: string | null;
  readonly tags: readonly string[];
  /** The matrix values of an expanded target. */
  readonly matrix: Readonly<Record<string, string>> | null;
}

export interface BakePlan {
  /** The bake file as given (or found). */
  readonly file: string;
  readonly targets: readonly BakeTarget[];
}

const NAME_RE = /^[A-Za-z0-9_][A-Za-z0-9._-]*$/;
const KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
const TARGET_KEYS = ['context', 'file', 'args', 'arg-files', 'target', 'output', 'format', 'tags', 'matrix', 'inherits'];

/** A target as written (after type checks), before inheritance. */
interface RawTarget {
  context?: string;
  file?: string;
  args?: Record<string, string>;
  'arg-files'?: string[];
  target?: string;
  output?: string;
  format?: string;
  tags?: string[];
  matrix?: Array<[string, string[]]>;
  inherits: string[];
}

type Merged = Omit<RawTarget, 'matrix' | 'inherits'>;

interface Definition {
  readonly label: string;
  readonly targets: ReadonlyMap<string, RawTarget>;
  readonly groups: ReadonlyMap<string, readonly string[]>;
}

function fail(label: string, detail: string): never {
  throw new SitefileError('E_DATA', `${label}: ${detail}`);
}

const isMapping = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);
const isScalar = (v: unknown): v is string | number | boolean => typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean';

/** Find the bake file: `file` when given, else the first of {@link BAKE_FILES} in the current directory. */
export async function findBakeFile(file: string | undefined): Promise<string> {
  if (file !== undefined) return file;
  for (const name of BAKE_FILES) {
    const found = await readFile(name).then(
      () => true,
      () => false,
    );
    if (found) return name;
  }
  throw new SitefileError('E_NOT_FOUND', `no bake file: ${BAKE_FILES.join(', ')} not found in the current directory (or pass -f)`);
}

/** Read and parse a bake file into its definition (no resolution yet). */
export async function loadBakeFile(file: string): Promise<unknown> {
  const base = path.basename(file).toLowerCase();
  const json = base.endsWith('.json');
  if (!json && !base.endsWith('.yaml') && !base.endsWith('.yml')) {
    throw new SitefileError('E_USAGE', `bake file '${file}': unknown format (use a .yaml, .yml or .json file)`);
  }
  let text: string;
  try {
    text = await readFile(file, 'utf8');
  } catch (e) {
    throw new SitefileError('E_NOT_FOUND', `cannot read bake file '${file}'`, undefined, { cause: e });
  }
  if (!json) return parseYaml(text, file);
  try {
    return JSON.parse(text.replace(/^﻿/, '')) as unknown;
  } catch (e) {
    throw new SitefileError('E_DATA', `${file} is not valid JSON: ${(e as Error).message}`, undefined, { cause: e });
  }
}

function stringField(label: string, where: string, key: string, v: unknown): string {
  if (typeof v !== 'string') fail(label, `${where}: ${key} must be a string`);
  return v;
}

function stringList(label: string, where: string, key: string, v: unknown): string[] {
  if (!Array.isArray(v) || !v.every((x) => typeof x === 'string')) fail(label, `${where}: ${key} must be a list of strings`);
  return v as string[];
}

function parseTarget(label: string, name: string, v: unknown): RawTarget {
  const where = `target '${name}'`;
  if (v === null) return { inherits: [] };
  if (!isMapping(v)) fail(label, `${where} must be a mapping`);
  const t: RawTarget = { inherits: [] };
  for (const [key, value] of Object.entries(v)) {
    switch (key) {
      case 'context':
      case 'file':
      case 'target':
      case 'output':
      case 'format':
        t[key] = stringField(label, where, key, value);
        break;
      case 'arg-files':
      case 'tags':
        t[key] = stringList(label, where, key, value);
        break;
      case 'inherits':
        t.inherits = typeof value === 'string' ? [value] : stringList(label, where, key, value);
        break;
      case 'args': {
        if (!isMapping(value)) fail(label, `${where}: args must be a mapping of NAME: value`);
        const args: Record<string, string> = {};
        for (const [k, a] of Object.entries(value)) {
          if (!KEY_RE.test(k)) fail(label, `${where}: '${k}' is not a valid ARG name`);
          if (!isScalar(a)) fail(label, `${where}: args.${k} must be a string, number or boolean`);
          args[k] = String(a);
        }
        t.args = args;
        break;
      }
      case 'matrix': {
        if (!isMapping(value) || Object.keys(value).length === 0) fail(label, `${where}: matrix must be a mapping of name: [values]`);
        const m: Array<[string, string[]]> = [];
        for (const [k, list] of Object.entries(value)) {
          if (!KEY_RE.test(k)) fail(label, `${where}: '${k}' is not a valid matrix variable name`);
          if (!Array.isArray(list) || list.length === 0 || !list.every(isScalar)) {
            fail(label, `${where}: matrix.${k} must be a non-empty list of strings, numbers or booleans`);
          }
          m.push([k, list.map((x) => String(x))]);
        }
        t.matrix = m;
        break;
      }
      default:
        fail(label, `${where}: unknown key '${key}' (expected ${TARGET_KEYS.join(', ')})`);
    }
  }
  return t;
}

/** Check the shape of a parsed bake file. */
export function parseBakeDefinition(value: unknown, label: string): Definition {
  if (!isMapping(value)) fail(label, 'a bake file must be a mapping with targets (and optional groups)');
  for (const key of Object.keys(value)) {
    if (key !== 'targets' && key !== 'groups') fail(label, `unknown key '${key}' (expected targets, groups)`);
  }
  const rawTargets = value['targets'];
  if (!isMapping(rawTargets) || Object.keys(rawTargets).length === 0) fail(label, 'targets must be a non-empty mapping of name: target');
  const targets = new Map<string, RawTarget>();
  for (const [name, t] of Object.entries(rawTargets)) {
    if (!NAME_RE.test(name)) fail(label, `'${name}' is not a valid target name (letters, digits, '.', '_', '-')`);
    targets.set(name, parseTarget(label, name, t));
  }
  const groups = new Map<string, string[]>();
  const rawGroups = value['groups'];
  if (rawGroups !== undefined && rawGroups !== null) {
    if (!isMapping(rawGroups)) fail(label, 'groups must be a mapping of name: [targets]');
    for (const [name, list] of Object.entries(rawGroups)) {
      if (!NAME_RE.test(name)) fail(label, `'${name}' is not a valid group name (letters, digits, '.', '_', '-')`);
      if (targets.has(name)) fail(label, `'${name}' is both a target and a group`);
      groups.set(name, stringList(label, `group '${name}'`, 'the group', list));
    }
  }
  return { label, targets, groups };
}

function merge(into: Merged, from: Merged): void {
  for (const key of ['context', 'file', 'target', 'output', 'format', 'arg-files', 'tags'] as const) {
    if (from[key] !== undefined) (into as Record<string, unknown>)[key] = from[key];
  }
  if (from.args) into.args = Object.assign(into.args ?? {}, from.args);
}

/** A target's fields with its `inherits` applied (parents in order, then its own fields). */
function inherited(def: Definition, name: string, stack: string[]): Merged {
  const t = def.targets.get(name)!;
  const out: Merged = {};
  for (const parent of t.inherits) {
    if (!def.targets.has(parent)) fail(def.label, `target '${name}' inherits unknown target '${parent}'`);
    if (parent === name || stack.includes(parent)) fail(def.label, `inherits cycle: ${[...stack, name, parent].join(' -> ')}`);
    merge(out, inherited(def, parent, [...stack, name]));
  }
  merge(out, t); // matrix and inherits are not inherited
  if (out.args) out.args = { ...out.args };
  return out;
}

/** Every combination of the matrix values, the first variable varying slowest. */
function combinations(matrix: ReadonlyArray<[string, string[]]>): Array<Array<[string, string]>> {
  let out: Array<Array<[string, string]>> = [[]];
  for (const [k, values] of matrix) out = out.flatMap((c) => values.map((v): Array<[string, string]> => [...c, [k, v]]));
  return out;
}

function interpolate(def: Definition, name: string, field: string, s: string, vars: ReadonlyMap<string, string>): string {
  return s.replace(/\$\{([^}]*)\}/g, (whole, v: string) => {
    const value = vars.get(v);
    if (value === undefined) fail(def.label, `target '${name}': ${field}: '${whole}' is not a matrix variable (${[...vars.keys()].join(', ')})`);
    return value;
  });
}

const joinTo = (dir: string, p: string): string => (path.isAbsolute(p) ? path.normalize(p) : path.join(dir, p));

/** Expand and resolve every target of the definition, in file order (matrix targets in place). */
function expand(def: Definition, bakeDir: string): Map<string, BakeTarget[]> {
  const out = new Map<string, BakeTarget[]>();
  const seen = new Map<string, string>();
  for (const [base, raw] of def.targets) {
    const fields = inherited(def, base, []);
    const combos = raw.matrix ? combinations(raw.matrix) : [[]];
    const list: BakeTarget[] = [];
    for (const combo of combos) {
      const name = [base, ...combo.map(([, v]) => v.replace(/[^A-Za-z0-9._-]/g, '_'))].join('-');
      const prior = seen.get(name);
      if (prior !== undefined) fail(def.label, `two targets are named '${name}' (from '${prior}' and '${base}')`);
      seen.set(name, base);
      const vars = new Map(combo);
      const sub = (field: string, s: string): string => (raw.matrix ? interpolate(def, name, field, s, vars) : s);
      const format = fields.format !== undefined ? sub('format', fields.format) : null;
      if (format !== null && !parseExportFormat(format)) {
        fail(def.label, `target '${name}': format must be one of ${EXPORT_FORMATS.join(', ')} (or tar.gz), got '${format}'`);
      }
      const context = joinTo(bakeDir, sub('context', fields.context ?? '.'));
      const args: Record<string, string> = {};
      for (const [k, v] of Object.entries(fields.args ?? {})) args[k] = sub(`args.${k}`, v);
      const defaultOut = path.join('dist', name + defaultOutName(format !== null ? parseExportFormat(format)! : 'directory').slice('dist'.length));
      list.push({
        name,
        context,
        file: fields.file !== undefined ? joinTo(bakeDir, sub('file', fields.file)) : path.join(context, 'Sitefile'),
        args,
        argFiles: (fields['arg-files'] ?? []).map((f) => joinTo(bakeDir, sub('arg-files', f))),
        target: fields.target !== undefined ? sub('target', fields.target) : null,
        output: joinTo(bakeDir, fields.output !== undefined ? sub('output', fields.output) : defaultOut),
        defaultOutput: fields.output === undefined,
        format,
        tags: (fields.tags ?? []).map((t) => sub('tags', t)),
        matrix: raw.matrix ? Object.fromEntries(combo) : null,
      });
    }
    out.set(base, list);
  }
  return out;
}

/**
 * Resolve a bake definition into the targets to build: `selected` names targets
 * (a matrix target's name selects all its variants; a variant's own name selects
 * it) and groups, in order, without duplicates. With none, the `default` group if
 * there is one, else every target in file order. Unknown names are usage errors;
 * targets whose outputs overlap, or that share a tag, are E_CONFLICT.
 */
export function resolveBake(def: Definition, file: string, selected: readonly string[]): BakePlan {
  const bakeDir = path.dirname(file);
  const all = expand(def, bakeDir);
  const byName = new Map<string, BakeTarget>();
  for (const list of all.values()) for (const t of list) byName.set(t.name, t);
  const picked: BakeTarget[] = [];
  const add = (t: BakeTarget): void => {
    if (!picked.includes(t)) picked.push(t);
  };
  const visit = (name: string, stack: string[], fromGroup: string | undefined): void => {
    const group = def.groups.get(name);
    if (group) {
      if (stack.includes(name)) fail(def.label, `group '${name}' includes itself (${[...stack, name].join(' -> ')})`);
      for (const member of group) visit(member, [...stack, name], name);
      return;
    }
    const list = all.get(name);
    if (list) return list.forEach(add);
    const one = byName.get(name);
    if (one) return add(one);
    if (fromGroup !== undefined) fail(def.label, `group '${fromGroup}': no target or group named '${name}'`);
    const groups = [...def.groups.keys()];
    throw new SitefileError(
      'E_USAGE',
      `bake: no target or group named '${name}' (targets: ${[...def.targets.keys()].join(', ')}${groups.length ? `; groups: ${groups.join(', ')}` : ''})`,
    );
  };
  if (selected.length) for (const n of selected) visit(n, [], undefined);
  else if (def.groups.has('default')) visit('default', [], undefined);
  else for (const list of all.values()) list.forEach(add);
  checkConflicts(picked);
  return { file, targets: picked };
}

function checkConflicts(targets: readonly BakeTarget[]): void {
  const abs = targets.map((t) => path.resolve(t.output));
  const under = (a: string, b: string): boolean => a.startsWith(b.endsWith(path.sep) ? b : b + path.sep);
  for (let i = 0; i < targets.length; i++) {
    for (let j = i + 1; j < targets.length; j++) {
      const [a, b] = [targets[i]!, targets[j]!];
      if (abs[i] === abs[j]) throw new SitefileError('E_CONFLICT', `bake targets '${a.name}' and '${b.name}' both write '${a.output}'`);
      if (under(abs[j]!, abs[i]!) || under(abs[i]!, abs[j]!)) {
        throw new SitefileError('E_CONFLICT', `bake targets '${a.name}' and '${b.name}' write overlapping outputs '${a.output}' and '${b.output}'`);
      }
      const tag = a.tags.find((t) => b.tags.includes(t));
      if (tag !== undefined) throw new SitefileError('E_CONFLICT', `bake targets '${a.name}' and '${b.name}' both tag '${tag}'`);
    }
  }
}

/** Read, check and resolve a bake file. */
export async function bakePlan(file: string, selected: readonly string[]): Promise<BakePlan> {
  const plan = resolveBake(parseBakeDefinition(await loadBakeFile(file), file), file, selected);
  const targets = await Promise.all(plan.targets.map(compressedDefault));
  if (targets.some((t, i) => t !== plan.targets[i])) checkConflicts(targets);
  return { file: plan.file, targets };
}

/**
 * A `wbn` or `warc` target with the default output gets the name `build` would give it: `dist/<name>.wbn.gz`
 * (`.warc.gz`) when its Sitefile's EXPORT in that format says `COMPRESSION gzip` (#151). A Sitefile that cannot
 * be planned keeps the plain name; its build reports why.
 */
async function compressedDefault(t: BakeTarget): Promise<BakeTarget> {
  const format = t.format !== null ? parseExportFormat(t.format) : undefined;
  if (!t.defaultOutput || (format !== 'wbn' && format !== 'warc')) return t;
  try {
    const buildArgs = await collectBuildArgs(t.argFiles, Object.entries(t.args).map(([k, v]) => `${k}=${v}`));
    const plan = await planBuild({ contextDir: t.context, sitefile: t.file, buildArgs });
    if (sitefileCompression(plan, format) !== 'gzip') return t;
    return { ...t, output: t.output + defaultOutName(format, 'gzip').slice(defaultOutName(format).length) };
  } catch (e) {
    if (e instanceof SitefileError) return t;
    throw e;
  }
}

/** `--print --json`. */
export function bakePlanJson(plan: BakePlan): unknown {
  return {
    file: plan.file,
    targets: plan.targets.map((t) => ({
      name: t.name,
      context: t.context,
      file: t.file,
      args: t.args,
      argFiles: t.argFiles,
      target: t.target,
      output: t.output,
      format: t.format,
      tags: t.tags,
      matrix: t.matrix,
    })),
  };
}

/** `--print`: one block per target, then a count. */
export function bakePlanText(plan: BakePlan): string {
  const line = (label: string, value: string): string => `  ${label.padEnd(10)}${value}\n`;
  let s = '';
  for (const t of plan.targets) {
    s += `${t.name}\n`;
    s += line('context', t.context);
    s += line('file', t.file);
    if (t.target !== null) s += line('target', t.target);
    s += line('output', t.output);
    if (t.format !== null) s += line('format', t.format);
    for (const [k, v] of Object.entries(t.args)) s += line('arg', `${k}=${v}`);
    for (const f of t.argFiles) s += line('arg-file', f);
    for (const tag of t.tags) s += line('tag', tag);
    if (t.matrix) s += line('matrix', Object.entries(t.matrix).map(([k, v]) => `${k}=${v}`).join(' '));
  }
  return s + `${plan.targets.length} target${plan.targets.length === 1 ? '' : 's'} from ${plan.file}\n`;
}

/** The `sitefile build` arguments of one target (before the CLI's pass-through flags). */
export function buildArgv(t: BakeTarget): string[] {
  return [
    'build',
    t.context,
    '-f',
    t.file,
    '-o',
    t.output,
    ...(t.target !== null ? ['--target', t.target] : []),
    ...(t.format !== null ? ['--format', t.format] : []),
    ...t.argFiles.flatMap((f) => ['--build-arg-file', f]),
    ...Object.entries(t.args).flatMap(([k, v]) => ['--build-arg', `${k}=${v}`]),
    ...t.tags.flatMap((tag) => ['-t', tag]),
  ];
}
