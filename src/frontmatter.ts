import { SitefileError, type SourceSpan } from './errors.js';

/**
 * Frontmatter parser for a small, strict YAML subset.
 *
 * Frontmatter exists only when the text starts (after an optional BOM) with a
 * `---` line. It ends at the next line that is exactly `---` or `...`.
 *
 * Supported inside the block:
 * - `key: value` where key matches `[A-Za-z_][A-Za-z0-9_-]*`; duplicate keys fail.
 * - Scalars: "double" quoted (escapes \" \\ \n \t), 'single' quoted ('' escape),
 *   unquoted strings (trimmed; ` #` begins a comment), `true`/`false`,
 *   integers and decimals (numbers), `null` / `~` / empty (null).
 *   Dates such as 2026-01-02 stay strings.
 * - Inline lists of scalars: `[a, "b c", 3]`.
 * - Block lists: `key:` followed by `  - item` lines (scalars only).
 * - Blank lines and `# comment` lines.
 *
 * Everything else (nested maps, `|` / `>` multi-line strings, anchors, tags,
 * flow maps) fails with E_FRONTMATTER at the offending line and column.
 */
export type FrontmatterValue = string | number | boolean | null | Array<string | number | boolean | null>;

export interface ParsedFrontmatter {
  data: Record<string, FrontmatterValue>;
  body: string;
  /** 1-based line of the file on which `body` starts. */
  bodyLine: number;
}

type Scalar = string | number | boolean | null;

const KEY_RE = /^([A-Za-z_][A-Za-z0-9_-]*):(?:[ \t]+|$)/;
const INT_RE = /^-?\d+$/;
const DECIMAL_RE = /^-?\d+\.\d+$/;

export function parseFrontmatter(text: string, file: string): ParsedFrontmatter {
  const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;

  // Split into lines, remembering where each starts so the body can be sliced.
  const lines: string[] = [];
  const starts: number[] = [];
  for (let pos = 0; pos <= src.length; ) {
    let nl = src.indexOf('\n', pos);
    if (nl === -1) nl = src.length;
    starts.push(pos);
    lines.push(src.slice(pos, nl).replace(/\r$/, ''));
    pos = nl + 1;
  }

  if (lines[0] !== '---') return { data: {}, body: text, bodyLine: 1 };

  let close = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i] === '---' || lines[i] === '...') {
      close = i;
      break;
    }
  }
  if (close === -1) {
    throw new SitefileError('E_FRONTMATTER', "unterminated frontmatter: missing closing '---'", { file, line: 1, column: 1 });
  }

  const data = parseBlock(lines.slice(1, close), file);
  const bodyStart = close + 1 < starts.length ? (starts[close + 1] ?? src.length) : src.length;
  return { data, body: src.slice(bodyStart), bodyLine: close + 2 };
}

function parseBlock(lines: string[], file: string): Record<string, FrontmatterValue> {
  const data: Record<string, FrontmatterValue> = Object.create(null) as Record<string, FrontmatterValue>;
  // Key waiting for block-list items (declared with an empty value).
  let pending: string | null = null;
  let pendingList: Scalar[] | null = null;

  for (let idx = 0; idx < lines.length; idx++) {
    const raw = lines[idx] ?? '';
    const line = idx + 2; // file line (line 1 is the opening '---')
    const at = (column: number): SourceSpan => ({ file, line, column });
    const fail = (detail: string, column = 1): never => {
      throw new SitefileError('E_FRONTMATTER', detail, at(column));
    };

    if (raw.trim() === '' || raw.trimStart().startsWith('#')) continue;

    const item = /^([ \t]*)-(?:[ \t]+(.*)|[ \t]*)$/.exec(raw);
    if (item) {
      if (pending === null) fail('list item without a preceding key');
      const rest = item[2] ?? '';
      const col = raw.length - rest.length + 1;
      const value = parseScalar(rest, col, at, false) as Scalar;
      if (pendingList === null) {
        pendingList = [];
        data[pending as string] = pendingList;
      }
      pendingList.push(value);
      continue;
    }

    if (/^[ \t]/.test(raw)) {
      fail('unexpected indentation (nested maps are not supported)', (/^[ \t]*/.exec(raw)?.[0].length ?? 0) + 1);
    }

    const m = KEY_RE.exec(raw);
    if (!m) {
      const colon = raw.indexOf(':');
      fail(colon === -1 ? "expected 'key: value'" : `invalid key '${raw.slice(0, colon)}'`);
      continue;
    }
    const key = m[1] as string;
    if (key in data) fail(`duplicate key '${key}'`);
    pending = null;
    pendingList = null;

    const valueText = raw.slice(m[0].length);
    const col = m[0].length + 1;
    if (valueText.trim() === '' || valueText.trimStart().startsWith('#')) {
      data[key] = null;
      pending = key; // may be followed by block-list items
      continue;
    }
    const value = parseScalar(valueText, col, at, true);
    data[key] = value;
  }
  // Copy to an ordinary object; fromEntries defines own properties, so a
  // `__proto__` key stays plain data.
  return Object.fromEntries(Object.entries(data));
}

/**
 * Parse a scalar (or, when allowList, an inline list) from `s`. `col` is the
 * 1-based column of `s[0]` for error positions.
 */
function parseScalar(
  s: string,
  col: number,
  at: (column: number) => SourceSpan,
  allowList: boolean,
): FrontmatterValue {
  const lead = s.length - s.trimStart().length;
  const t = s.slice(lead);
  const c0 = col + lead;
  const err = (detail: string, column: number): never => {
    throw new SitefileError('E_FRONTMATTER', detail, at(column));
  };

  if (t.startsWith('[')) {
    if (!allowList) err('nested lists are not supported', c0);
    return parseInlineList(t, c0, at)[0];
  }
  const first = t[0];
  if (first === '"' || first === "'") {
    const [value, end] = parseQuoted(t, c0, at);
    assertTrailing(t.slice(end), c0 + end, at);
    return value;
  }
  if (first === '|' || first === '>') err('multi-line strings are not supported', c0);
  if (first === '{') err('inline maps are not supported', c0);
  if (first === '&' || first === '*' || first === '!') err('anchors, aliases and tags are not supported', c0);

  const plain = stripComment(t).trim();
  if (/:(?:\s|$)/.test(plain)) err('nested maps are not supported', c0);
  return plainScalar(plain);
}

function stripComment(t: string): string {
  const m = /(^|[ \t])#/.exec(t);
  return m ? t.slice(0, m.index) : t;
}

function plainScalar(p: string): Scalar {
  if (p === '' || p === '~' || p === 'null') return null;
  if (p === 'true') return true;
  if (p === 'false') return false;
  if (INT_RE.test(p) || DECIMAL_RE.test(p)) return Number(p);
  return p;
}

/** Only whitespace or a ` #` comment may follow a closed value. */
function assertTrailing(rest: string, col: number, at: (column: number) => SourceSpan): void {
  const r = rest.trim();
  if (r === '' || (r.startsWith('#') && /^[ \t]/.test(rest))) return;
  throw new SitefileError('E_FRONTMATTER', `unexpected text after value: '${r}'`, at(col + rest.length - rest.trimStart().length));
}

function parseQuoted(t: string, col: number, at: (column: number) => SourceSpan): [string, number] {
  const q = t[0] as string;
  let out = '';
  let i = 1;
  while (i < t.length) {
    const c = t[i] as string;
    if (q === '"' && c === '\\') {
      const n = t[i + 1];
      const map: Record<string, string> = { '"': '"', '\\': '\\', n: '\n', t: '\t' };
      if (n === undefined || map[n] === undefined) {
        throw new SitefileError('E_FRONTMATTER', `unsupported escape '\\${n ?? ''}'`, at(col + i));
      }
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
  throw new SitefileError('E_FRONTMATTER', 'unterminated quoted string', at(col));
}

function parseInlineList(t: string, col: number, at: (column: number) => SourceSpan): [Array<Scalar>, number] {
  const fail = (detail: string, i: number): never => {
    throw new SitefileError('E_FRONTMATTER', detail, at(col + i));
  };
  const items: Scalar[] = [];
  let i = 1;
  for (;;) {
    while (t[i] === ' ' || t[i] === '\t') i++;
    if (i >= t.length) fail("unterminated list: missing ']'", 0);
    if (t[i] === ']') {
      i++;
      break;
    }
    const c = t[i] as string;
    if (c === '"' || c === "'") {
      const [value, len] = parseQuoted(t.slice(i), col + i, at);
      items.push(value);
      i += len;
    } else if (c === '[' || c === '{') {
      fail('nested lists and maps are not supported', i);
    } else if (c === ',') {
      fail('empty list item', i);
    } else {
      let j = i;
      while (j < t.length && t[j] !== ',' && t[j] !== ']') j++;
      if (j >= t.length) fail("unterminated list: missing ']'", 0);
      const raw = t.slice(i, j).trim();
      if (/:(?:\s|$)/.test(raw) || /^[&*!|>]/.test(raw)) fail('unsupported list item', i);
      items.push(plainScalar(raw));
      i = j;
    }
    while (t[i] === ' ' || t[i] === '\t') i++;
    if (t[i] === ',') {
      i++;
    } else if (t[i] !== ']') {
      fail("expected ',' or ']'", i);
    }
  }
  assertTrailing(t.slice(i), col + i, at);
  return [items, t.length];
}
