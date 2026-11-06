import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { SitefileError } from './errors.js';
import { parseYaml } from './yaml.js';

const KEY_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/**
 * Read build-arg values from a file, by extension:
 *
 * - `.json`: a flat object, `{"TITLE": "Docs", "PORT": 8080}`
 * - `.yaml` / `.yml`: a flat mapping, `TITLE: Docs`
 * - `.env` (or a file named `.env`, `.env.*`, `*.env`): `KEY=value` lines
 *
 * Values must be strings, numbers or booleans; they become strings and are
 * checked against each ARG's declared type later, like `--build-arg`.
 * `.env` files are parsed, never executed: no `$VAR` or `$(…)` expansion.
 */
export async function loadBuildArgFile(file: string): Promise<Record<string, string>> {
  const kind = buildArgFileKind(file); // an unknown extension is a usage error, before any IO
  let text: string;
  try {
    text = await readFile(file, 'utf8');
  } catch (e) {
    throw new SitefileError('E_NOT_FOUND', `cannot read build-arg file '${file}'`, undefined, { cause: e });
  }
  const label = path.relative(process.cwd(), path.resolve(file)) || file;
  if (kind === 'env') return parseEnvFile(text, label);
  let value: unknown;
  if (kind === 'json') {
    try {
      value = JSON.parse(text.replace(/^﻿/, ''));
    } catch (e) {
      throw new SitefileError('E_DATA', `${label} is not valid JSON: ${(e as Error).message}`, undefined, { cause: e });
    }
  } else {
    value = parseYaml(text, label);
  }
  return flatScalars(value, label);
}

export function buildArgFileKind(file: string): 'json' | 'yaml' | 'env' {
  const base = path.basename(file).toLowerCase();
  if (base.endsWith('.json')) return 'json';
  if (base.endsWith('.yaml') || base.endsWith('.yml')) return 'yaml';
  if (base === '.env' || base.startsWith('.env.') || base.endsWith('.env')) return 'env';
  throw new SitefileError(
    'E_USAGE',
    `--build-arg-file '${file}': unknown format (use a .json, .yaml, .yml or .env file)`,
  );
}

function flatScalars(value: unknown, label: string): Record<string, string> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new SitefileError('E_DATA', `${label}: build-arg files must contain a mapping of NAME: value`);
  }
  const out: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const [key, v] of Object.entries(value as Record<string, unknown>)) {
    if (!KEY_RE.test(key)) throw new SitefileError('E_DATA', `${label}: '${key}' is not a valid ARG name`);
    if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') out[key] = String(v);
    else {
      throw new SitefileError(
        'E_DATA',
        `${label}: ${key} must be a string, number or boolean (ARGs can't hold ${v === null ? 'null' : Array.isArray(v) ? 'lists' : 'objects'})`,
      );
    }
  }
  return out;
}

/**
 * `.env` syntax: one `KEY=value` per line, optional `export ` prefix, `#`
 * comment lines and blank lines. Unquoted values are trimmed and a ` #` starts
 * a comment; `'single'` quotes are literal; `"double"` quotes understand
 * `\n \t \r \" \\` and may span lines. Nothing is expanded or executed.
 */
export function parseEnvFile(text: string, label: string): Record<string, string> {
  const out: Record<string, string> = Object.create(null) as Record<string, string>;
  const src = text.replace(/^﻿/, '').replace(/\r\n?/g, '\n');
  const err = (line: number, msg: string): SitefileError =>
    new SitefileError('E_DATA', msg, { file: label, line, column: 1 });
  let i = 0;
  let line = 1;
  while (i < src.length) {
    const nl = src.indexOf('\n', i);
    const end = nl === -1 ? src.length : nl;
    const raw = src.slice(i, end);
    const trimmed = raw.trim();
    const startLine = line;
    if (trimmed === '' || trimmed.startsWith('#')) {
      i = end + 1;
      line++;
      continue;
    }
    const m = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*/.exec(raw);
    if (!m) throw err(line, `expected KEY=value, got '${trimmed}'`);
    let j = i + m[0].length;
    let value: string;
    const q = src[j];
    if (q === "'" || q === '"') {
      j++;
      value = '';
      let closed = false;
      while (j < src.length) {
        const c = src[j]!;
        if (c === q) {
          closed = true;
          j++;
          break;
        }
        if (c === '\n') line++;
        if (q === '"' && c === '\\' && j + 1 < src.length) {
          const n = src[j + 1]!;
          value += n === 'n' ? '\n' : n === 't' ? '\t' : n === 'r' ? '\r' : n;
          j += 2;
          continue;
        }
        value += c;
        j++;
      }
      if (!closed) throw err(startLine, `unterminated ${q} quote for ${m[1]}`);
      const restEnd = src.indexOf('\n', j) === -1 ? src.length : src.indexOf('\n', j);
      const rest = src.slice(j, restEnd).trim();
      if (rest !== '' && !rest.startsWith('#')) throw err(line, `unexpected text after the quoted value of ${m[1]}`);
      i = restEnd + 1;
    } else {
      value = src.slice(j, end).replace(/\s+#.*$/, '').trim();
      i = end + 1;
    }
    line++;
    out[m[1]!] = value;
  }
  return out;
}

/**
 * The CLI's build args: `--build-arg-file` files in order (later files win), then
 * `--build-arg NAME=value` (or `NAME`, read from the environment), which win over files.
 * Malformed flags are E_USAGE errors.
 */
export async function collectBuildArgs(files: readonly string[], flags: readonly string[], env: NodeJS.ProcessEnv = process.env): Promise<Record<string, string>> {
  const out: Record<string, string> = {};
  for (const file of files) Object.assign(out, await loadBuildArgFile(file));
  for (const kv of flags) {
    const eq = kv.indexOf('=');
    if (eq === -1) {
      // --build-arg NAME: take the value from the environment, as Docker does.
      if (!KEY_RE.test(kv)) throw new SitefileError('E_USAGE', `--build-arg expects NAME=value or NAME, got '${kv}'`);
      const v = env[kv];
      if (v === undefined) throw new SitefileError('E_USAGE', `--build-arg ${kv}: environment variable ${kv} is not set`);
      out[kv] = v;
      continue;
    }
    if (eq === 0) throw new SitefileError('E_USAGE', `--build-arg expects NAME=value, got '${kv}'`);
    out[kv.slice(0, eq)] = kv.slice(eq + 1);
  }
  return out;
}
