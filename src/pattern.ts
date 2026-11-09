// `pattern` in schemas (docs/LANGUAGE.md, "Schemas"): a small regular expression engine that the Rust port
// (rust/src/pattern.rs) implements identically, so both accept the same patterns, report the same errors and
// match the same strings. Shared test vectors: rust/tests/fixtures/pattern/vectors.json.
//
// The syntax is a subset of JavaScript regular expressions with the `u` flag, with the same meaning:
//
//   literals          any character other than ^ $ \ . * + ? ( ) [ ] { } |
//   .                 any character except \n, \r, U+2028 and U+2029
//   [abc] [a-z] [^…]  classes (ranges, escapes; `-` is literal first or last; [] matches nothing, [^] anything)
//   \d \w \s          [0-9], [A-Za-z0-9_], JavaScript whitespace; \D \W \S their complements
//   \n \r \t          newline, carriage return, tab; \ before any of ^ $ \ . * + ? ( ) [ ] { } | / is that
//                     character (and \- inside a class)
//   * + ? {n} {n,} {n,m}   repetition (n, m ≤ 1000), optionally lazy with a trailing ? (same result here)
//   ( … ) (?: … )     groups
//   a|b               alternation
//   ^ $               start and end of the string (no multiline mode)
//
// A pattern matches when it matches somewhere in the string (like RegExp.test); anchor it with ^…$ to match
// the whole value. Characters are Unicode code points. Matching runs a Thompson NFA (Pike VM), so its time is
// linear in the string length whatever the pattern: no backtracking blow-up.

/** A compile error: `position` is the 1-based code point index in the pattern. */
export class PatternError extends Error {
  constructor(
    readonly reason: string,
    readonly position: number,
  ) {
    super(`at character ${position}: ${reason}`);
  }
}

type Range = readonly [number, number];
/** A set of code points: ranges, or their complement. */
interface CharSet {
  readonly ranges: readonly Range[];
  readonly negate: boolean;
}

type Node =
  | { readonly k: 'set'; readonly items: readonly CharSet[]; readonly negate: boolean }
  | { readonly k: 'bol' }
  | { readonly k: 'eol' }
  | { readonly k: 'cat'; readonly xs: readonly Node[] }
  | { readonly k: 'alt'; readonly xs: readonly Node[] }
  | { readonly k: 'rep'; readonly x: Node; readonly min: number; readonly max: number | undefined };

const MAX_COUNT = 1000;
/** Compiled programs larger than this are refused (a {n,m} of a {n,m} …). */
const MAX_PROGRAM = 20000;

const DIGIT: CharSet = { ranges: [[0x30, 0x39]], negate: false };
const WORD: CharSet = { ranges: [[0x30, 0x39], [0x41, 0x5a], [0x5f, 0x5f], [0x61, 0x7a]], negate: false };
/** JavaScript's \s: WhiteSpace and LineTerminator. */
const SPACE: CharSet = {
  ranges: [[0x09, 0x0d], [0x20, 0x20], [0xa0, 0xa0], [0x1680, 0x1680], [0x2000, 0x200a], [0x2028, 0x2029], [0x202f, 0x202f], [0x205f, 0x205f], [0x3000, 0x3000], [0xfeff, 0xfeff]],
  negate: false,
};
/** `.`: everything but line terminators. */
const DOT: CharSet = { ranges: [[0x0a, 0x0a], [0x0d, 0x0d], [0x2028, 0x2029]], negate: true };
const SYNTAX = '^$\\.*+?()[]{}|/';

const single = (c: number): CharSet => ({ ranges: [[c, c]], negate: false });
const inSet = (s: CharSet, c: number): boolean => s.ranges.some(([lo, hi]) => c >= lo && c <= hi) !== s.negate;

class Parser {
  i = 0;
  constructor(readonly cps: readonly number[]) {}
  peek(): string | undefined {
    const c = this.cps[this.i];
    return c === undefined ? undefined : String.fromCodePoint(c);
  }
  fail(reason: string, at = this.i): never {
    throw new PatternError(reason, at + 1);
  }

  alt(): Node {
    const xs = [this.cat()];
    while (this.peek() === '|') {
      this.i++;
      xs.push(this.cat());
    }
    return xs.length === 1 ? xs[0]! : { k: 'alt', xs };
  }

  cat(): Node {
    const xs: Node[] = [];
    for (;;) {
      const c = this.peek();
      if (c === undefined || c === '|' || c === ')') break;
      const start = this.i;
      const atom = this.atom();
      xs.push(this.quantified(atom, start));
    }
    return xs.length === 1 ? xs[0]! : { k: 'cat', xs };
  }

  /** A quantifier after `atom` (which began at `start`), if any. */
  quantified(atom: Node, start: number): Node {
    const c = this.peek();
    let min: number;
    let max: number | undefined;
    if (c === '*' || c === '+' || c === '?') {
      this.i++;
      [min, max] = c === '*' ? [0, undefined] : c === '+' ? [1, undefined] : [0, 1];
    } else if (c === '{') {
      [min, max] = this.counted();
    } else {
      return atom;
    }
    if (atom.k === 'bol' || atom.k === 'eol') this.fail('nothing to repeat', start);
    if (this.peek() === '?') this.i++; // lazy: the same answer for a yes/no match
    const next = this.peek();
    if (next === '*' || next === '+' || next === '?' || next === '{') this.fail('nothing to repeat');
    return { k: 'rep', x: atom, min, max };
  }

  /** `{n}`, `{n,}` or `{n,m}` at `{`. */
  counted(): [number, number | undefined] {
    const open = this.i;
    this.i++;
    const num = (): number | undefined => {
      const from = this.i;
      while (this.peek() !== undefined && /[0-9]/.test(this.peek()!)) this.i++;
      if (this.i === from) return undefined;
      const digits = String.fromCodePoint(...this.cps.slice(from, this.i));
      return digits.length > 4 ? MAX_COUNT + 1 : Number(digits);
    };
    const min = num();
    let max: number | undefined = min;
    if (min !== undefined && this.peek() === ',') {
      this.i++;
      max = num();
    }
    if (min === undefined || this.peek() !== '}') this.fail("'{' must be written \\{ unless it starts a repetition {n}, {n,} or {n,m}", open);
    this.i++;
    if (min > MAX_COUNT || (max !== undefined && max > MAX_COUNT)) this.fail(`repetition count above ${MAX_COUNT}`, open);
    if (max !== undefined && max < min) this.fail('numbers out of order in {} repetition', open);
    return [min, max];
  }

  atom(): Node {
    const start = this.i;
    const c = this.peek()!;
    this.i++;
    switch (c) {
      case '(': {
        if (this.peek() === '?') {
          if (this.cps[this.i + 1] !== 0x3a) this.fail("unsupported group '(?' (only ( … ) and (?: … ) are supported)", start);
          this.i += 2;
        }
        const inner = this.alt();
        if (this.peek() !== ')') this.fail("missing ')'", start);
        this.i++;
        return inner;
      }
      case ')':
        return this.fail("unmatched ')'", start);
      case '[':
        return this.cls(start);
      case '.':
        return { k: 'set', items: [DOT], negate: false };
      case '^':
        return { k: 'bol' };
      case '$':
        return { k: 'eol' };
      case '\\':
        return { k: 'set', items: [this.escape(false, start)], negate: false };
      case '*':
      case '+':
      case '?':
        return this.fail('nothing to repeat', start);
      case '{':
        return this.fail("'{' must be written \\{ unless it starts a repetition {n}, {n,} or {n,m}", start);
      case '}':
      case ']':
        return this.fail(`'${c}' must be written \\${c}`, start);
      default:
        return { k: 'set', items: [single(this.cps[start]!)], negate: false };
    }
  }

  /** After `\` (at `start`): a class escape or a single character. */
  escape(inClass: boolean, start: number): CharSet {
    const c = this.peek();
    if (c === undefined) return this.fail("pattern ends with '\\'", start);
    this.i++;
    switch (c) {
      case 'd':
        return DIGIT;
      case 'D':
        return { ...DIGIT, negate: true };
      case 'w':
        return WORD;
      case 'W':
        return { ...WORD, negate: true };
      case 's':
        return SPACE;
      case 'S':
        return { ...SPACE, negate: true };
      case 'n':
        return single(0x0a);
      case 'r':
        return single(0x0d);
      case 't':
        return single(0x09);
    }
    if (SYNTAX.includes(c) || (inClass && c === '-')) return single(c.codePointAt(0)!);
    return this.fail(`unsupported escape '\\${c}'`, start);
  }

  /** A class after `[` (at `start`). */
  cls(start: number): Node {
    let negate = false;
    if (this.peek() === '^') {
      negate = true;
      this.i++;
    }
    const items: CharSet[] = [];
    /** One class atom: a set (a class escape) or a single code point. */
    const classAtom = (): { set: CharSet; cp?: number; at: number } => {
      const at = this.i;
      const c = this.peek()!;
      this.i++;
      if (c === '\\') {
        const set = this.escape(true, at);
        const isSingle = !set.negate && set.ranges.length === 1 && set.ranges[0]![0] === set.ranges[0]![1];
        return isSingle ? { set, cp: set.ranges[0]![0], at } : { set, at };
      }
      return { set: single(this.cps[at]!), cp: this.cps[at]!, at };
    };
    for (;;) {
      const c = this.peek();
      if (c === undefined) this.fail('unterminated character class', start);
      if (c === ']') {
        this.i++;
        break;
      }
      const a = classAtom();
      if (this.peek() === '-' && this.cps[this.i + 1] !== undefined && this.cps[this.i + 1] !== 0x5d) {
        this.i++;
        const b = classAtom();
        if (a.cp === undefined || b.cp === undefined) this.fail('a class escape cannot start or end a range', a.at);
        if (b.cp! < a.cp!) this.fail('range out of order in character class', a.at);
        items.push({ ranges: [[a.cp!, b.cp!]], negate: false });
      } else {
        items.push(a.set);
      }
    }
    return { k: 'set', items, negate };
  }
}

type Inst =
  | { readonly op: 'set'; readonly items: readonly CharSet[]; readonly negate: boolean }
  | { readonly op: 'bol' | 'eol' | 'match' }
  | { readonly op: 'split'; x: number; y: number }
  | { readonly op: 'jmp'; x: number };

/** A compiled pattern. */
export interface Pattern {
  readonly source: string;
  readonly prog: readonly Inst[];
}

function emit(n: Node, prog: Inst[]): void {
  if (prog.length > MAX_PROGRAM) throw new PatternError('pattern is too large', 1);
  switch (n.k) {
    case 'set':
      prog.push({ op: 'set', items: n.items, negate: n.negate });
      return;
    case 'bol':
    case 'eol':
      prog.push({ op: n.k });
      return;
    case 'cat':
      for (const x of n.xs) emit(x, prog);
      return;
    case 'alt': {
      // split L1, next; L1: x0; jmp end; next: split L2, … ; last: xn
      const jumps: Array<{ op: 'jmp'; x: number }> = [];
      n.xs.forEach((x, i) => {
        if (i < n.xs.length - 1) {
          const split: Inst = { op: 'split', x: prog.length + 1, y: 0 };
          prog.push(split);
          emit(x, prog);
          const j = { op: 'jmp' as const, x: 0 };
          prog.push(j);
          jumps.push(j);
          split.y = prog.length;
        } else {
          emit(x, prog);
        }
      });
      for (const j of jumps) j.x = prog.length;
      return;
    }
    case 'rep': {
      for (let i = 0; i < n.min; i++) emit(n.x, prog);
      if (n.max === undefined) {
        // L: split body, end; body; jmp L
        const split: Inst = { op: 'split', x: prog.length + 1, y: 0 };
        const at = prog.length;
        prog.push(split);
        emit(n.x, prog);
        prog.push({ op: 'jmp', x: at });
        split.y = prog.length;
        return;
      }
      // (x(x(x)?)?)? for the optional copies.
      const splits: Array<{ op: 'split'; x: number; y: number }> = [];
      for (let i = n.min; i < n.max; i++) {
        const split = { op: 'split' as const, x: prog.length + 1, y: 0 };
        prog.push(split);
        splits.push(split);
        emit(n.x, prog);
      }
      for (const s of splits) s.y = prog.length;
      return;
    }
  }
}

/** Compile a pattern; throws PatternError. */
export function compilePattern(source: string): Pattern {
  const cps = Array.from(source, (c) => c.codePointAt(0)!);
  const p = new Parser(cps);
  const node = p.alt();
  if (p.i < cps.length) p.fail("unmatched ')'"); // alt() stops only at the end or a ')'
  const prog: Inst[] = [];
  emit(node, prog);
  if (prog.length > MAX_PROGRAM) throw new PatternError('pattern is too large', 1);
  prog.push({ op: 'match' });
  return { source, prog };
}

/** Whether the pattern matches somewhere in `text` (Pike VM over code points). */
export function patternMatches(p: Pattern, text: string): boolean {
  const cps = Array.from(text, (c) => c.codePointAt(0)!);
  const n = p.prog.length;
  let clist: number[] = [];
  let mark = new Uint32Array(n);
  let gen = 0;
  // Follow jumps, splits and assertions from `pc` at position `pos`; true when Match is reachable.
  const add = (list: number[], pc: number, pos: number): boolean => {
    const stack = [pc];
    while (stack.length > 0) {
      const at = stack.pop()!;
      if (mark[at] === gen) continue;
      mark[at] = gen;
      const ins = p.prog[at]!;
      switch (ins.op) {
        case 'match':
          return true;
        case 'jmp':
          stack.push(ins.x);
          break;
        case 'split':
          stack.push(ins.y, ins.x);
          break;
        case 'bol':
          if (pos === 0) stack.push(at + 1);
          break;
        case 'eol':
          if (pos === cps.length) stack.push(at + 1);
          break;
        case 'set':
          list.push(at);
          break;
      }
    }
    return false;
  };
  for (let pos = 0; ; pos++) {
    gen++;
    // The start state at every position: an unanchored search. Threads already in the list keep their marks.
    for (const pc of clist) mark[pc] = gen;
    if (add(clist, 0, pos)) return true;
    if (pos === cps.length) return false;
    const c = cps[pos]!;
    const next: number[] = [];
    gen++;
    for (const pc of clist) {
      const ins = p.prog[pc] as Extract<Inst, { op: 'set' }>;
      if (ins.items.some((s) => inSet(s, c)) !== ins.negate && add(next, pc + 1, pos + 1)) return true;
    }
    clist = next;
    if (gen > 0xfffffff0) {
      mark = new Uint32Array(n);
      gen = 0;
    }
  }
}
