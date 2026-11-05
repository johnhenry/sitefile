import { SitefileError, type ErrorCode, type SourceSpan } from '../errors.js';
import type { Operand, TemplatePart } from './ast.js';

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
export function logicalLines(source: string): LogicalLine[] {
  const out: LogicalLine[] = [];
  let text = '';
  let positions: Pos[] = [];
  let end: Pos = { line: 1, column: 1 };
  let continuing = false;

  source.split(/\r?\n/).forEach((raw, index) => {
    const line = index + 1;
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
      out.push({ text, positions, end });
      text = '';
      positions = [];
      continuing = false;
    }
  });
  if (continuing && text.trim() !== '') out.push({ text, positions, end });
  return out;
}

/** Reads tokens from one logical line. All methods skip leading whitespace. */
export class Cursor {
  private i = 0;
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
    if (t[this.i] === '.') {
      throw this.error('E_SYNTAX', `dotted access \${${name}.…} is only allowed inside RENDER templates`, at);
    }
    if (t[this.i] !== '}') throw this.error('E_SYNTAX', 'unterminated ${...}: expected }', at);
    this.i++;
    return { kind: 'var', name, path: [], span: this.spanAt(at) };
  }
}
