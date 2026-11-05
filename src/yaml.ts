import { SitefileError } from './errors.js';

/**
 * A small, safe YAML subset parser (no dependencies, no code execution).
 *
 * Supported:
 * - Block style by indentation (spaces only; a tab in indentation is an error).
 * - Mappings `key: value`, nested mappings, sequences `- item`, sequences of
 *   mappings (`- name: a` / `  url: /a`), mappings of sequences (including
 *   the indentless form), arbitrary nesting up to 64 levels.
 * - Scalars: "double" quoted (escapes \" \\ \n \t), 'single' quoted ('' escape),
 *   plain strings (trimmed; ` #` begins a comment), `true`/`false`, integers
 *   and decimals (numbers), `null` / `~` / empty (null). Dates such as
 *   2026-01-02 stay strings.
 * - Flow collections on a single line: `[a, b, {x: 1}]`, `{a: 1, b: [2, 3]}`.
 * - Block scalars `|` (literal) and `>` (folded) with optional `-` / `+`
 *   chomping; the indentation comes from the first content line.
 * - Comments, blank lines, an optional leading `---` and a trailing `...`.
 * - Keys are plain or quoted scalars and are always strings. Duplicate keys
 *   are an error. A `__proto__` key is stored as ordinary own data.
 * - An empty document is `null`.
 *
 * Not supported (E_DATA at the offending line and column): anchors and aliases
 * (`&` `*`), tags (`!`), multiple documents, complex keys (`? `), merge keys
 * (`<<`), multi-line flow collections, multi-line plain or quoted scalars,
 * block scalar indentation indicators, tabs for indentation.
 *
 * `firstLine` is the 1-based file line of the first line of `text`, so errors
 * point at the right place when the YAML is embedded in another file.
 */
export function parseYaml(text: string, file: string, firstLine = 1): unknown {
  return parseYamlDocument(text, file, firstLine, false);
}

const MAX_DEPTH = 64;
const INT_RE = /^-?\d+$/;
const DECIMAL_RE = /^-?\d+\.\d+$/;

type Fail = (detail: string, column: number) => never;

interface Line {
  /** 1-based line within the parsed text. */
  readonly no: number;
  readonly raw: string;
  /** Column offset (0-based) of `text` within the line. */
  indent: number;
  text: string;
  /** Indent of the enclosing block parent (for block scalars). */
  parent: number;
}

interface KeyInfo {
  key: string;
  /** Index in `line.text` just after the colon. */
  valueIdx: number;
}

/**
 * Like parseYaml; with `rootMapping` the document must be a mapping (or empty)
 * and indented content at the top is rejected. Used by frontmatter.
 */
export function parseYamlDocument(text: string, file: string, firstLine: number, rootMapping: boolean): unknown {
  const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  const base = firstLine - 1;
  const lines: Line[] = src.split('\n').map((r, i) => {
    const raw = r.replace(/\r$/, '');
    let indent = 0;
    while (raw[indent] === ' ') indent++;
    return { no: i + 1, raw, indent, text: raw.slice(indent), parent: -1 };
  });
  const failAt = (line: Line, detail: string, column: number): never => {
    throw new SitefileError('E_DATA', detail, { file, line: line.no + base, column });
  };

  // Document markers.
  let started = false;
  let sawStart = false;
  let end = lines.length;
  for (let i = 0; i < lines.length; i++) {
    const l = lines[i] as Line;
    if (l.indent === 0) {
      const t = l.raw.trimEnd();
      if (t === '---' || l.raw.startsWith('--- ')) {
        if (t !== '---') failAt(l, 'content on the document marker line is not supported', 5);
        if (started || sawStart) failAt(l, 'multiple documents are not supported', 1);
        sawStart = true;
        l.text = '';
        continue;
      }
      if (t === '...') {
        for (let j = i + 1; j < lines.length; j++) {
          const rest = (lines[j] as Line).text.trim();
          if (rest !== '' && !rest.startsWith('#')) failAt(lines[j] as Line, 'multiple documents are not supported', 1);
        }
        end = i;
        break;
      }
    }
    const tt = l.text.trim();
    if (!started && tt !== '' && !tt.startsWith('#')) started = true;
  }
  lines.length = end;

  const p = new Parser(lines, base, failAt);
  const first = p.peek();
  if (!first) return null;
  if (first.indent > 0 && rootMapping) failAt(first, 'unexpected indentation', first.indent + 1);
  if (rootMapping) {
    if (isItem(first.text)) failAt(first, 'list item without a preceding key', first.indent + 1);
    if (!p.tryKey(first)) failAt(first, "expected 'key: value'", first.indent + 1);
  }
  const value = p.nodeAt(first);
  const extra = p.peek();
  if (extra) failAt(extra, extra.indent > first.indent ? 'unexpected indentation' : 'unexpected content', extra.indent + 1);
  return value;
}

function isItem(text: string): boolean {
  return /^-(?:[ \t]|$)/.test(text);
}

class Parser {
  pos = 0;
  depth = 0;

  constructor(
    readonly lines: Line[],
    readonly base: number,
    readonly failAt: (line: Line, detail: string, column: number) => never,
  ) {}

  /** Next significant line (skipping blanks and comments); leaves pos on it. */
  peek(): Line | undefined {
    while (this.pos < this.lines.length) {
      const l = this.lines[this.pos] as Line;
      const t = l.text.trim();
      if (t === '' || t.startsWith('#')) {
        this.pos++;
        continue;
      }
      if (l.text.startsWith('\t')) this.failAt(l, 'tabs are not allowed for indentation', l.indent + 1);
      return l;
    }
    return undefined;
  }

  private fail(line: Line): Fail {
    return (detail, column) => this.failAt(line, detail, column);
  }

  private enter(line: Line): void {
    if (++this.depth > MAX_DEPTH) this.failAt(line, `nesting is deeper than ${MAX_DEPTH} levels`, line.indent + 1);
  }

  /** Parse the next node if it is indented at least `minIndent`, else null. */
  private parseNode(minIndent: number): unknown {
    const line = this.peek();
    if (!line || line.indent < minIndent) return null;
    return this.nodeAt(line);
  }

  /** Parse the node starting on `line` (which must be the current line). */
  nodeAt(line: Line): unknown {
    if (isItem(line.text)) return this.parseSeq(line.indent);
    if (this.tryKey(line)) return this.parseMap(line.indent);
    return this.inlineValue(line, 0, line.parent);
  }

  private parseSeq(n: number): unknown[] {
    const first = this.lines[this.pos] as Line;
    this.enter(first);
    const out: unknown[] = [];
    for (;;) {
      const line = this.peek();
      if (!line || line.indent < n) break;
      if (line.indent > n) this.failAt(line, 'unexpected indentation', line.indent + 1);
      if (!isItem(line.text)) break;
      const m = /^-( *)(.*)$/.exec(line.text) as RegExpExecArray;
      const spaces = (m[1] as string).length;
      const rest = m[2] as string;
      if (rest === '' || (spaces > 0 && rest.startsWith('#'))) {
        this.pos++;
        out.push(this.parseNode(n + 1));
      } else {
        line.indent = n + 1 + spaces;
        line.text = rest;
        line.parent = n;
        out.push(this.nodeAt(line));
      }
    }
    this.depth--;
    return out;
  }

  private parseMap(n: number): unknown {
    const first = this.lines[this.pos] as Line;
    this.enter(first);
    const map = new Map<string, unknown>();
    for (;;) {
      const line = this.peek();
      if (!line || line.indent < n) break;
      if (line.indent > n) this.failAt(line, 'unexpected indentation', line.indent + 1);
      if (isItem(line.text)) this.failAt(line, 'list item without a preceding key', line.indent + 1);
      const k = this.tryKey(line);
      if (!k) this.failAt(line, "expected 'key: value'", line.indent + 1);
      const { key, valueIdx } = k as KeyInfo;
      if (map.has(key)) this.failAt(line, `duplicate key '${key}'`, line.indent + 1);
      const rest = line.text.slice(valueIdx);
      const trimmed = rest.trim();
      let value: unknown;
      if (trimmed === '' || trimmed.startsWith('#')) {
        this.pos++;
        const next = this.peek();
        if (next && next.indent > n) value = this.nodeAt(next);
        else if (next && next.indent === n && isItem(next.text)) value = this.parseSeq(n);
        else value = null;
      } else {
        value = this.inlineValue(line, valueIdx, n);
      }
      map.set(key, value);
    }
    this.depth--;
    // fromEntries defines own data properties, so a `__proto__` key is safe.
    return Object.fromEntries(map);
  }

  /** If `line` is `key: ...`, return the key; throws for unsupported forms. */
  tryKey(line: Line): KeyInfo | null {
    const t = line.text;
    const col = line.indent + 1;
    const fail = this.fail(line);
    if (t[0] === '?' && (t.length === 1 || /\s/.test(t[1] as string))) fail('complex keys are not supported', col);
    const c0 = t[0];
    if (c0 === '"' || c0 === "'") {
      const [key, end] = parseQuoted(t, col, fail);
      let i = end;
      while (t[i] === ' ' || t[i] === '\t') i++;
      if (t[i] === ':' && (i + 1 >= t.length || t[i + 1] === ' ' || t[i + 1] === '\t')) return { key, valueIdx: i + 1 };
      return null;
    }
    if (c0 === '[' || c0 === '{') return null;
    const ci = commentIndex(t);
    const head = ci === -1 ? t : t.slice(0, ci);
    const m = /:(?:[ \t]|$)/.exec(head);
    if (!m) return null;
    const key = head.slice(0, m.index).trim();
    if (key === '') fail('empty key', col);
    if (/^[&*!]/.test(key)) fail('anchors, aliases and tags are not supported', col);
    if (key === '<<') fail('merge keys are not supported', col);
    return { key, valueIdx: m.index + 1 };
  }

  /**
   * Parse the scalar/flow/block-scalar value that starts at `line.text[idx]`.
   * Advances pos past everything consumed.
   */
  private inlineValue(line: Line, idx: number, parentIndent: number): unknown {
    const t = line.text.slice(idx);
    const lead = t.length - t.trimStart().length;
    const s = t.slice(lead);
    const col = line.indent + idx + lead + 1;
    const fail = this.fail(line);
    const first = s[0];

    if (first === '|' || first === '>') return this.blockScalar(line, s, col, parentIndent);
    if (first === '[' || first === '{') {
      const [v, end] = parseFlow(s, 0, col, fail, this.depth + 1);
      assertTrailing(s.slice(end), col + end, fail);
      this.pos++;
      return v;
    }
    if (first === '"' || first === "'") {
      const [v, end] = parseQuoted(s, col, fail);
      assertTrailing(s.slice(end), col + end, fail);
      this.pos++;
      return v;
    }
    if (first === '&' || first === '*' || first === '!') fail('anchors, aliases and tags are not supported', col);
    if (/^-(?:[ \t]|$)/.test(s)) fail('a list item is not allowed here', col);
    if (/^\?(?:[ \t]|$)/.test(s)) fail('complex keys are not supported', col);
    const plain = stripComment(s).trim();
    if (/:(?:[ \t]|$)/.test(plain)) fail('nested maps on one line are not supported', col);
    this.pos++;
    return plainScalar(plain);
  }

  private blockScalar(line: Line, s: string, col: number, parentIndent: number): string {
    const fail = this.fail(line);
    const m = /^([|>])([+-]?)([0-9]?)([ \t]+#.*|[ \t]*)$/.exec(s);
    if (!m) return fail('invalid block scalar header', col);
    if (m[3]) fail('block scalar indentation indicators are not supported', col);
    const style = m[1] as string;
    const chomp = m[2] as string;
    this.pos++;

    const content: string[] = [];
    let blockIndent = -1;
    while (this.pos < this.lines.length) {
      const l = this.lines[this.pos] as Line;
      const raw = l.raw;
      if (raw.trim() === '') {
        content.push('');
        this.pos++;
        continue;
      }
      let ind = 0;
      while (raw[ind] === ' ') ind++;
      if (blockIndent < 0) {
        if (ind <= parentIndent) break;
        blockIndent = ind;
      } else if (ind < blockIndent) {
        if (ind <= parentIndent) break;
        this.failAt(l, 'bad indentation in block scalar', ind + 1);
      }
      content.push(raw.slice(blockIndent));
      this.pos++;
    }

    let end = content.length;
    while (end > 0 && content[end - 1] === '') end--;
    const trailing = content.length - end;
    const body = content.slice(0, end);
    if (body.length === 0) return chomp === '+' ? '\n'.repeat(trailing) : '';
    const text = style === '|' ? body.join('\n') : fold(body);
    const suffix = chomp === '-' ? '' : chomp === '+' ? '\n'.repeat(trailing + 1) : '\n';
    return text + suffix;
  }
}

function fold(lines: string[]): string {
  let out = '';
  let blanks = 0;
  let prev: string | null = null;
  for (const line of lines) {
    if (line === '') {
      blanks++;
      continue;
    }
    if (prev === null) {
      out = '\n'.repeat(blanks) + line;
    } else {
      const more = /^[ \t]/.test(prev) || /^[ \t]/.test(line);
      if (more) out += '\n'.repeat(blanks + 1);
      else if (blanks === 0) out += ' ';
      else out += '\n'.repeat(blanks);
      out += line;
    }
    blanks = 0;
    prev = line;
  }
  return out;
}

function commentIndex(t: string): number {
  const m = /(^|[ \t])#/.exec(t);
  return m ? m.index : -1;
}

function stripComment(t: string): string {
  const i = commentIndex(t);
  return i === -1 ? t : t.slice(0, i);
}

function plainScalar(p: string): string | number | boolean | null {
  if (p === '' || p === '~' || p === 'null') return null;
  if (p === 'true') return true;
  if (p === 'false') return false;
  if (INT_RE.test(p) || DECIMAL_RE.test(p)) return Number(p);
  return p;
}

/** Only whitespace or a ` #` comment may follow a closed value. */
function assertTrailing(rest: string, col: number, fail: Fail): void {
  const r = rest.trim();
  if (r === '' || (r.startsWith('#') && /^[ \t]/.test(rest))) return;
  fail(`unexpected text after value: '${r}'`, col + rest.length - rest.trimStart().length);
}

/** Parse a quoted string at t[0]; returns the value and the index after it. */
function parseQuoted(t: string, col: number, fail: Fail): [string, number] {
  const q = t[0] as string;
  let out = '';
  let i = 1;
  while (i < t.length) {
    const c = t[i] as string;
    if (q === '"' && c === '\\') {
      const n = t[i + 1];
      const map: Record<string, string> = { '"': '"', '\\': '\\', n: '\n', t: '\t' };
      if (n === undefined || map[n] === undefined) return fail(`unsupported escape '\\${n ?? ''}'`, col + i);
      out += map[n];
      i += 2;
    } else if (q === "'" && c === "'" && t[i + 1] === "'") {
      out += "'";
      i += 2;
    } else if (c === q) {
      return [out, i + 1];
    } else {
      out += c;
      i++;
    }
  }
  return fail('unterminated quoted string', col);
}

const WS = (c: string | undefined): boolean => c === ' ' || c === '\t';

/**
 * Parse a flow collection or value starting at t[start]. `col` is the column
 * of t[0]. Returns the value and the index just after it.
 */
function parseFlow(t: string, start: number, col: number, fail: Fail, depth: number): [unknown, number] {
  if (depth > MAX_DEPTH) return fail(`nesting is deeper than ${MAX_DEPTH} levels`, col + start);
  const c = t[start];
  if (c === '[') return parseFlowSeq(t, start, col, fail, depth);
  if (c === '{') return parseFlowMap(t, start, col, fail, depth);
  return fail('expected a flow collection', col + start);
}

function parseFlowSeq(t: string, start: number, col: number, fail: Fail, depth: number): [unknown, number] {
  const open = col + start;
  const items: unknown[] = [];
  let i = start + 1;
  for (;;) {
    while (WS(t[i])) i++;
    if (i >= t.length) return fail("unterminated list: missing ']'", open);
    if (t[i] === ']') {
      i++;
      break;
    }
    if (t[i] === ',') return fail('empty list item', col + i);
    const [v, ni] = parseFlowValue(t, i, col, fail, depth, open, "unterminated list: missing ']'", false);
    items.push(v);
    i = ni;
    while (WS(t[i])) i++;
    if (t[i] === ',') i++;
    else if (i >= t.length) return fail("unterminated list: missing ']'", open);
    else if (t[i] !== ']') return fail("expected ',' or ']'", col + i);
  }
  return [items, i];
}

function parseFlowMap(t: string, start: number, col: number, fail: Fail, depth: number): [unknown, number] {
  const open = col + start;
  const unterminated = "unterminated map: missing '}'";
  const map = new Map<string, unknown>();
  let i = start + 1;
  for (;;) {
    while (WS(t[i])) i++;
    if (i >= t.length) return fail(unterminated, open);
    if (t[i] === '}') {
      i++;
      break;
    }
    if (t[i] === ',') return fail('empty map entry', col + i);

    // key
    const keyCol = col + i;
    let key: string;
    if (t[i] === '"' || t[i] === "'") {
      const [k, len] = parseQuoted(t.slice(i), col + i, fail);
      key = k;
      i += len;
      while (WS(t[i])) i++;
      if (t[i] !== ':') return fail("expected ':' after key", col + i);
      i++;
    } else {
      let j = i;
      while (j < t.length && t[j] !== ',' && t[j] !== '}' && t[j] !== ']') {
        if (t[j] === ':' && (j + 1 >= t.length || ' \t,}]'.includes(t[j + 1] as string))) break;
        j++;
      }
      if (j >= t.length) return fail(unterminated, open);
      if (t[j] !== ':') return fail("expected ':' after key", keyCol);
      key = t.slice(i, j).trim();
      if (key === '') return fail('empty key', keyCol);
      if (/^[&*!\[{]/.test(key)) return fail('anchors, aliases, tags and complex keys are not supported', keyCol);
      if (key === '<<') return fail('merge keys are not supported', keyCol);
      i = j + 1;
    }
    if (map.has(key)) return fail(`duplicate key '${key}'`, keyCol);

    // value
    while (WS(t[i])) i++;
    let value: unknown = null;
    if (i >= t.length) return fail(unterminated, open);
    if (t[i] !== ',' && t[i] !== '}') {
      const [v, ni] = parseFlowValue(t, i, col, fail, depth, open, unterminated, true);
      value = v;
      i = ni;
    }
    map.set(key, value);
    while (WS(t[i])) i++;
    if (t[i] === ',') i++;
    else if (i >= t.length) return fail(unterminated, open);
    else if (t[i] !== '}') return fail("expected ',' or '}'", col + i);
  }
  return [Object.fromEntries(map), i];
}

function parseFlowValue(
  t: string,
  i: number,
  col: number,
  fail: Fail,
  depth: number,
  open: number,
  unterminated: string,
  inMap: boolean,
): [unknown, number] {
  const c = t[i] as string;
  if (c === '[' || c === '{') return parseFlow(t, i, col, fail, depth + 1);
  if (c === '"' || c === "'") {
    const [v, len] = parseQuoted(t.slice(i), col + i, fail);
    return [v, i + len];
  }
  let j = i;
  while (j < t.length && t[j] !== ',' && t[j] !== ']' && t[j] !== '}') j++;
  if (j >= t.length) return fail(unterminated, open);
  const raw = t.slice(i, j).trim();
  if (/^[&*!]/.test(raw)) return fail('anchors, aliases and tags are not supported', col + i);
  if (/:(?:\s|$)/.test(raw) || /^[|>]/.test(raw)) {
    return fail(inMap ? 'nested maps on one line are not supported' : 'unsupported list item', col + i);
  }
  return [plainScalar(raw), j];
}
