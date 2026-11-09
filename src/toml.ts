import { SitefileError } from './errors.js';

/**
 * A TOML subset, enough for host config files (netlify.toml, wrangler.toml; issue #204). Ported as
 * rust/src/toml.rs; both give the same values and the same errors.
 *
 * Supported: comments, `[table]` and `[[array.of.tables]]` headers (a header path through an array of
 * tables goes into its last element, as `[headers.values]` after `[[headers]]`), bare and quoted keys,
 * dotted keys, basic strings (with `\b \t \n \f \r \" \\ \uXXXX \UXXXXXXXX`), literal strings, decimal
 * integers and floats (`_` separators), booleans, arrays (over several lines, trailing comma allowed) and
 * inline tables. Not supported (an E_DATA error): multi-line strings, dates and times, hex/octal/binary
 * integers, `inf`/`nan`.
 */
export type TomlValue = string | number | boolean | TomlValue[] | TomlTable;
export interface TomlTable {
  [key: string]: TomlValue;
}

const BARE = /[A-Za-z0-9_-]/;

class Parser {
  i = 0;
  line = 1;
  /** Tables created by a `[header]` (they may not be defined twice) and inline tables (closed). */
  readonly defined = new Set<TomlTable>();
  readonly closed = new Set<TomlTable | TomlValue[]>();
  /** Arrays created by `[[header]]` (only those take more tables). */
  readonly tableArrays = new Set<TomlValue[]>();

  constructor(
    readonly s: string,
    readonly file: string,
  ) {}

  fail(msg: string): never {
    throw new SitefileError('E_DATA', `${this.file}:${this.line}: ${msg}`);
  }

  peek(n = 0): string {
    return this.s[this.i + n] ?? '';
  }

  /** Spaces and tabs. */
  ws(): void {
    while (this.peek() === ' ' || this.peek() === '\t') this.i++;
  }

  comment(): void {
    if (this.peek() === '#') while (this.i < this.s.length && this.peek() !== '\n') this.i++;
  }

  /** Whitespace, comments and newlines. */
  blank(): void {
    for (;;) {
      this.ws();
      this.comment();
      if (this.peek() === '\r' && this.peek(1) === '\n') this.i++;
      if (this.peek() === '\n') {
        this.i++;
        this.line++;
        continue;
      }
      return;
    }
  }

  endOfLine(): void {
    this.ws();
    this.comment();
    if (this.peek() === '\r' && this.peek(1) === '\n') this.i++;
    if (this.i >= this.s.length) return;
    if (this.peek() !== '\n') this.fail('expected the end of the line');
  }

  key(): string {
    const c = this.peek();
    if (c === '"' || c === "'") {
      if (this.s.startsWith(c.repeat(3), this.i)) this.fail('multi-line strings are not supported');
      return c === '"' ? this.basic() : this.literal();
    }
    const start = this.i;
    while (BARE.test(this.peek())) this.i++;
    if (this.i === start) this.fail('expected a key');
    return this.s.slice(start, this.i);
  }

  keyPath(): string[] {
    const path = [this.key()];
    for (;;) {
      this.ws();
      if (this.peek() !== '.') return path;
      this.i++;
      this.ws();
      path.push(this.key());
    }
  }

  basic(): string {
    this.i++; // "
    let out = '';
    for (;;) {
      const c = this.peek();
      if (c === '' || c === '\n') this.fail('unterminated string');
      this.i++;
      if (c === '"') return out;
      if (c !== '\\') {
        out += c;
        continue;
      }
      const e = this.peek();
      this.i++;
      const simple: Record<string, string> = { b: '\b', t: '\t', n: '\n', f: '\f', r: '\r', '"': '"', '\\': '\\' };
      if (e in simple) out += simple[e];
      else if (e === 'u' || e === 'U') {
        const n = e === 'u' ? 4 : 8;
        const hex = this.s.slice(this.i, this.i + n);
        if (!new RegExp(`^[0-9A-Fa-f]{${n}}$`).test(hex)) this.fail('invalid escape in a string');
        const cp = parseInt(hex, 16);
        if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) this.fail('invalid escape in a string');
        out += String.fromCodePoint(cp);
        this.i += n;
      } else this.fail('invalid escape in a string');
    }
  }

  literal(): string {
    this.i++; // '
    const start = this.i;
    for (;;) {
      const c = this.peek();
      if (c === '' || c === '\n') this.fail('unterminated string');
      if (c === "'") {
        const v = this.s.slice(start, this.i);
        this.i++;
        return v;
      }
      this.i++;
    }
  }

  value(): TomlValue {
    const c = this.peek();
    if (c === '"' || c === "'") {
      if (this.s.startsWith(c.repeat(3), this.i)) this.fail('multi-line strings are not supported');
      return c === '"' ? this.basic() : this.literal();
    }
    if (c === '[') return this.array();
    if (c === '{') return this.inlineTable();
    const start = this.i;
    while (/[A-Za-z0-9_+\-.:]/.test(this.peek())) this.i++;
    const word = this.s.slice(start, this.i);
    if (word === 'true') return true;
    if (word === 'false') return false;
    if (/^[+-]?(0|[1-9](_?[0-9])*)$/.test(word)) return Number(word.replace(/_/g, ''));
    if (/^[+-]?(0|[1-9](_?[0-9])*)(\.[0-9](_?[0-9])*)?([eE][+-]?[0-9](_?[0-9])*)?$/.test(word)) return Number(word.replace(/_/g, ''));
    if (/^[0-9]{4}-[0-9]{2}-[0-9]{2}/.test(word) || /^[0-9]{2}:[0-9]{2}/.test(word)) this.fail('dates and times are not supported');
    if (word === '') this.fail('expected a value');
    this.fail(`unsupported value '${word}'`);
  }

  array(): TomlValue[] {
    this.i++; // [
    const out: TomlValue[] = [];
    for (;;) {
      this.blank();
      if (this.peek() === ']') {
        this.i++;
        break;
      }
      out.push(this.value());
      this.blank();
      if (this.peek() === ',') {
        this.i++;
        continue;
      }
      if (this.peek() === ']') {
        this.i++;
        break;
      }
      this.fail("expected ',' or ']' in an array");
    }
    this.closed.add(out);
    return out;
  }

  inlineTable(): TomlTable {
    this.i++; // {
    const out: TomlTable = {};
    this.ws();
    if (this.peek() === '}') {
      this.i++;
      this.closed.add(out);
      return out;
    }
    for (;;) {
      this.ws();
      const path = this.keyPath();
      this.ws();
      if (this.peek() !== '=') this.fail("expected '=' after a key");
      this.i++;
      this.ws();
      this.assign(out, path, this.value());
      this.ws();
      if (this.peek() === ',') {
        this.i++;
        continue;
      }
      if (this.peek() === '}') {
        this.i++;
        break;
      }
      this.fail("expected ',' or '}' in an inline table");
    }
    this.closed.add(out);
    return out;
  }

  /** Set `path` = `value` under `table`, creating the intermediate tables of a dotted key. */
  assign(table: TomlTable, path: readonly string[], value: TomlValue): void {
    let t = table;
    for (const k of path.slice(0, -1)) {
      const cur = Object.hasOwn(t, k) ? t[k] : undefined;
      if (cur === undefined) {
        const next: TomlTable = {};
        t[k] = next;
        t = next;
      } else if (isTable(cur) && !this.closed.has(cur) && !this.defined.has(cur)) t = cur;
      else this.fail(`key '${k}' is already defined`);
    }
    const last = path[path.length - 1] as string;
    if (Object.hasOwn(t, last)) this.fail(`key '${last}' is already defined`);
    t[last] = value;
  }

  /** The table a header path names (through the last element of arrays of tables), creating tables. */
  walk(root: TomlTable, path: readonly string[]): TomlTable {
    let t = root;
    for (const k of path) {
      const cur = Object.hasOwn(t, k) ? t[k] : undefined;
      if (cur === undefined) {
        const next: TomlTable = {};
        t[k] = next;
        t = next;
      } else if (Array.isArray(cur) && this.tableArrays.has(cur)) t = cur[cur.length - 1] as TomlTable;
      else if (isTable(cur) && !this.closed.has(cur)) t = cur;
      else this.fail(`key '${k}' is already defined`);
    }
    return t;
  }

  document(): TomlTable {
    const root: TomlTable = {};
    let current = root;
    for (;;) {
      this.blank();
      if (this.i >= this.s.length) return root;
      if (this.peek() === '[') {
        const many = this.peek(1) === '[';
        this.i += many ? 2 : 1;
        this.ws();
        const path = this.keyPath();
        this.ws();
        if (!this.s.startsWith(many ? ']]' : ']', this.i)) this.fail(many ? "expected ']]'" : "expected ']'");
        this.i += many ? 2 : 1;
        this.endOfLine();
        if (many) {
          const parent = this.walk(root, path.slice(0, -1));
          const k = path[path.length - 1] as string;
          const cur = Object.hasOwn(parent, k) ? parent[k] : undefined;
          let arr: TomlValue[];
          if (cur === undefined) {
            arr = [];
            parent[k] = arr;
            this.tableArrays.add(arr);
          } else if (Array.isArray(cur) && this.tableArrays.has(cur)) arr = cur;
          else this.fail(`key '${k}' is already defined`);
          const t: TomlTable = {};
          arr.push(t);
          current = t;
        } else {
          const t = this.walk(root, path);
          if (this.defined.has(t)) this.fail(`table '${path.join('.')}' is defined twice`);
          this.defined.add(t);
          current = t;
        }
        continue;
      }
      const path = this.keyPath();
      this.ws();
      if (this.peek() !== '=') this.fail("expected '=' after a key");
      this.i++;
      this.ws();
      const line = this.line;
      const v = this.value();
      const end = this.line;
      this.line = line;
      this.assign(current, path, v);
      this.line = end;
      this.endOfLine();
    }
  }
}

function isTable(v: TomlValue | undefined): v is TomlTable {
  return v !== undefined && typeof v === 'object' && !Array.isArray(v);
}

/** Parse a TOML document (the subset above). Errors are E_DATA `file:line: message`. */
export function parseToml(text: string, file: string): TomlTable {
  return new Parser(text.replace(/^﻿/, ''), file).document();
}
