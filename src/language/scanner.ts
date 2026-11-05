import { SitefileError, type ErrorCode, type SourceSpan } from '../errors.js';
import type { InlineBody, Operand, TemplatePart } from './ast.js';

export interface Pos {
  readonly line: number;
  readonly column: number;
}

/** One instruction's text after continuations are joined, with a source position per character. */
export interface LogicalLine {
  readonly text: string;
  readonly positions: readonly Pos[];
  /** Position just after the last character (for "expected X" errors at end of line). */
  readonly end: Pos;
  /** A heredoc body that followed this line (COPY and RENDER only). */
  readonly heredoc?: Heredoc;
}

export interface Heredoc {
  /** Index in `text` of the `<<` marker. */
  readonly at: number;
  /** Length of the marker text (`<<-NAME`). */
  readonly length: number;
  readonly delimiter: string;
  readonly text: string;
  readonly bodyLine: number;
}

/** Instructions whose source may be a heredoc. */
const HEREDOC_INSTRUCTIONS = new Set(['COPY', 'RENDER', 'TRANSFORM']);

/** `<<NAME` / `<<-NAME` markers that start a word outside quotes. */
function heredocMarkers(text: string): Array<{ at: number; length: number; strip: boolean; delimiter: string }> {
  const out: Array<{ at: number; length: number; strip: boolean; delimiter: string }> = [];
  let quote: string | undefined;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]!;
    if (quote) {
      if (ch === '\\' && quote === '"') i++;
      else if (ch === quote) quote = undefined;
      continue;
    }
    if (ch === '"' || ch === "'") { quote = ch; continue; }
    if (ch === '\\') { i++; continue; }
    if (ch !== '<' || (i > 0 && !WS.test(text[i - 1]!))) continue;
    const m = /^<<(-?)([A-Za-z_][A-Za-z0-9_]*)(?=\s|$)/.exec(text.slice(i));
    if (m) out.push({ at: i, length: m[0].length, strip: m[1] === '-', delimiter: m[2]! });
  }
  return out;
}

const IDENT_START = /[A-Za-z_]/;
const IDENT_CHAR = /[A-Za-z0-9_]/;
const WS = /\s/;

function trailingBackslashes(s: string): number {
  let n = 0;
  while (n < s.length && s[s.length - 1 - n] === '\\') n++;
  return n;
}

/**
 * Split source into logical lines. Blank lines and comment lines (first non-blank
 * char `#`) are skipped, also inside continuations. A line ending in an odd number
 * of `\` continues; pieces are trimmed at the join and joined with one space.
 */
export function logicalLines(source: string, file = 'Sitefile'): LogicalLine[] {
  const out: LogicalLine[] = [];
  let text = '';
  let positions: Pos[] = [];
  let end: Pos = { line: 1, column: 1 };
  let continuing = false;
  // Set while reading a heredoc body: lines are taken verbatim until the delimiter.
  let heredocBody: { marker: { at: number; length: number; strip: boolean; delimiter: string }; lines: string[]; start: number; owner: LogicalLine } | undefined;

  const physical = source.split(/\r?\n/);
  // A trailing newline is not an extra (empty) line of a heredoc body.
  if (physical.length > 1 && physical[physical.length - 1] === '') physical.pop();
  physical.forEach((raw, index) => {
    const line = index + 1;
    if (heredocBody) {
      const candidate = heredocBody.marker.strip ? raw.replace(/^\t+/, '') : raw;
      if (candidate === heredocBody.marker.delimiter) {
        const heredoc: Heredoc = {
          at: heredocBody.marker.at,
          length: heredocBody.marker.length,
          delimiter: heredocBody.marker.delimiter,
          text: heredocBody.lines.map((l) => l + '\n').join(''),
          bodyLine: heredocBody.start,
        };
        out[out.length - 1] = { ...heredocBody.owner, heredoc };
        heredocBody = undefined;
      } else {
        heredocBody.lines.push(candidate);
      }
      return;
    }
    const trimmed = raw.trim();
    if (trimmed === '' || trimmed.startsWith('#')) return;
    const cont = trailingBackslashes(raw) % 2 === 1;
    const body = cont ? raw.slice(0, -1) : raw;
    let start = 0;
    if (continuing) while (start < body.length && WS.test(body[start]!)) start++;
    let stop = body.length;
    if (cont) while (stop > start && WS.test(body[stop - 1]!)) stop--;
    if (continuing && text !== '' && stop > start) {
      text += ' ';
      positions.push(end);
    }
    for (let c = start; c < stop; c++) {
      text += body[c];
      positions.push({ line, column: c + 1 });
    }
    if (stop > start || !continuing) end = { line, column: stop + 1 };
    if (cont) {
      continuing = true;
    } else {
      const logical: LogicalLine = { text, positions, end };
      out.push(logical);
      const keyword = /^\S+/.exec(text)?.[0]?.toUpperCase() ?? '';
      if (HEREDOC_INSTRUCTIONS.has(keyword)) {
        const markers = heredocMarkers(text);
        if (markers.length > 1) {
          const p = positions[markers[1]!.at]!;
          throw new SitefileError('E_SYNTAX', 'only one inline (<<) body is allowed per instruction', { file, ...p });
        }
        if (markers[0]) heredocBody = { marker: markers[0], lines: [], start: line + 1, owner: logical };
      }
      text = '';
      positions = [];
      continuing = false;
    }
  });
  if (heredocBody) {
    const p = heredocBody.owner.positions[heredocBody.marker.at]!;
    throw new SitefileError('E_SYNTAX', `inline body is missing its closing '${heredocBody.marker.delimiter}' line`, { file, ...p });
  }
  if (continuing && text.trim() !== '') out.push({ text, positions, end });
  return out;
}

/** Reads tokens from one logical line. All methods skip leading whitespace. */
export class Cursor {
  private i = 0;
  private inlineUsed = false;
  private readonly text: string;

  constructor(
    private readonly line: LogicalLine,
    readonly file: string,
  ) {
    this.text = line.text;
  }

  spanAt(i: number): SourceSpan {
    const p = this.line.positions[i] ?? this.line.end;
    return { file: this.file, line: p.line, column: p.column };
  }

  /** Span of the next non-whitespace character (or end of line). */
  get span(): SourceSpan {
    this.skipWs();
    return this.spanAt(this.i);
  }

  error(code: ErrorCode, detail: string, at: number | SourceSpan = this.i): SitefileError {
    return new SitefileError(code, detail, typeof at === 'number' ? this.spanAt(at) : at);
  }

  skipWs(): void {
    while (this.i < this.text.length && WS.test(this.text[this.i]!)) this.i++;
  }

  atEnd(): boolean {
    this.skipWs();
    return this.i >= this.text.length;
  }

  private wordEnd(): number {
    let j = this.i;
    while (j < this.text.length && !WS.test(this.text[j]!)) j++;
    return j;
  }

  /** Consume raw text up to the next whitespace ('' at end of line). */
  word(): string {
    this.skipWs();
    const j = this.wordEnd();
    const w = this.text.slice(this.i, j);
    this.i = j;
    return w;
  }

  /** Consume `kw` (case-insensitive) if it is the next whole bare word. */
  keyword(kw: string): boolean {
    this.skipWs();
    const j = this.wordEnd();
    if (this.text.slice(this.i, j).toLowerCase() !== kw.toLowerCase()) return false;
    this.i = j;
    return true;
  }

  expectKeyword(kw: string): void {
    const at = this.span;
    if (!this.keyword(kw)) throw this.error('E_SYNTAX', `expected ${kw.toUpperCase()}`, at);
  }

  /** Consume `ch` if it is the next non-whitespace character. */
  eat(ch: string): boolean {
    this.skipWs();
    if (this.text[this.i] !== ch) return false;
    this.i++;
    return true;
  }

  /**
   * Read an identifier. It must be followed by whitespace, end of line, or one
   * of the characters in `stops`.
   */
  ident(what: string, stops = ''): string {
    this.skipWs();
    const start = this.i;
    if (!IDENT_START.test(this.text[this.i] ?? '')) throw this.error('E_SYNTAX', `expected ${what}`);
    while (IDENT_CHAR.test(this.text[this.i] ?? '')) this.i++;
    const next = this.text[this.i];
    if (next !== undefined && !WS.test(next) && !stops.includes(next)) {
      throw this.error('E_SYNTAX', `invalid ${what}: unexpected '${next}'`);
    }
    return this.text.slice(start, this.i);
  }

  /** Read `--name` or `--name=value` if the next word starts with `--`. */
  flag(): { name: string; value: string | undefined; span: SourceSpan } | undefined {
    this.skipWs();
    if (!this.text.startsWith('--', this.i)) return undefined;
    const span = this.spanAt(this.i);
    const w = this.word().slice(2);
    const eq = w.indexOf('=');
    return eq < 0 ? { name: w, value: undefined, span } : { name: w.slice(0, eq), value: w.slice(eq + 1), span };
  }

  expectEnd(): void {
    if (this.atEnd()) return;
    const at = this.i;
    throw this.error('E_SYNTAX', `unexpected '${this.word()}'`, at);
  }

  /** Consume this line's heredoc marker if it is the next word. */
  inline(): InlineBody | undefined {
    this.skipWs();
    const h = this.line.heredoc;
    if (!h || h.at !== this.i) return undefined;
    const span = this.spanAt(this.i);
    this.i += h.length;
    this.inlineUsed = true;
    return { kind: 'inline', delimiter: h.delimiter, text: h.text, span, bodyLine: h.bodyLine };
  }

  /** Fail if the line has a heredoc that was not used as the source. */
  expectInlineUsed(where: string): void {
    const h = this.line.heredoc;
    if (h && !this.inlineUsed) {
      throw this.error('E_SYNTAX', `an inline <<${h.delimiter} body can only be ${where}`, h.at);
    }
  }

  /** Read a bare, "double-quoted" or 'single-quoted' operand. */
  operand(what: string): Operand {
    this.skipWs();
    const start = this.i;
    const t = this.text;
    if (start >= t.length) throw this.error('E_SYNTAX', `expected ${what}`);
    const parts: TemplatePart[] = [];
    const addText = (s: string): void => {
      const last = parts[parts.length - 1];
      if (last?.kind === 'text') parts[parts.length - 1] = { kind: 'text', value: last.value + s };
      else parts.push({ kind: 'text', value: s });
    };
    const quote = t[start];

    if (quote === "'") {
      const close = t.indexOf("'", start + 1);
      if (close < 0) throw this.error('E_SYNTAX', 'unterminated single-quoted string', start);
      if (close > start + 1) addText(t.slice(start + 1, close));
      this.i = close + 1;
    } else if (quote === '"') {
      this.i++;
      for (;;) {
        const ch = t[this.i];
        if (ch === undefined) throw this.error('E_SYNTAX', 'unterminated double-quoted string', start);
        if (ch === '"') {
          this.i++;
          break;
        }
        if (ch === '\\') {
          const next = t[this.i + 1];
          if (next === undefined) throw this.error('E_SYNTAX', 'unterminated double-quoted string', start);
          addText(next === 'n' ? '\n' : next === 't' ? '\t' : next);
          this.i += 2;
        } else if (ch === '$' && t[this.i + 1] === '{') {
          parts.push(this.interpolation());
        } else {
          addText(ch);
          this.i++;
        }
      }
    } else {
      while (this.i < t.length && !WS.test(t[this.i]!)) {
        const ch = t[this.i]!;
        if (ch === '\\') {
          const next = t[this.i + 1];
          if (next === undefined) throw this.error('E_SYNTAX', 'dangling escape at end of line');
          addText(next);
          this.i += 2;
        } else if (ch === '$' && t[this.i + 1] === '{') {
          parts.push(this.interpolation());
        } else {
          addText(ch);
          this.i++;
        }
      }
      return { parts, span: this.spanAt(start) };
    }

    const after = t[this.i];
    if (after !== undefined && !WS.test(after)) throw this.error('E_SYNTAX', 'unexpected character after closing quote');
    return { parts, span: this.spanAt(start) };
  }

  /** At `${`: read `${NAME}`. */
  private interpolation(): TemplatePart {
    const at = this.i;
    const t = this.text;
    this.i += 2;
    if (!IDENT_START.test(t[this.i] ?? '')) throw this.error('E_SYNTAX', 'expected a name after ${', at);
    const nameStart = this.i;
    while (IDENT_CHAR.test(t[this.i] ?? '')) this.i++;
    const name = t.slice(nameStart, this.i);
    // Dotted paths are parsed here; the validator allows them only for a RENDER … FOR loop variable.
    const path: string[] = [];
    while (t[this.i] === '.') {
      this.i++;
      const segStart = this.i;
      while (/[A-Za-z0-9_-]/.test(t[this.i] ?? '')) this.i++;
      if (this.i === segStart) throw this.error('E_SYNTAX', `expected a key after '.' in \${${name}…}`, at);
      path.push(t.slice(segStart, this.i));
    }
    if (t[this.i] !== '}') throw this.error('E_SYNTAX', 'unterminated ${...}: expected }', at);
    this.i++;
    return { kind: 'var', name, path, span: this.spanAt(at) };
  }
}
