// Seeded random patterns and inputs for the pattern engine (src/pattern.ts), shared by
// test/pattern.test.ts and rust/tests/fixtures/pattern/generate.mjs (which writes fuzz.json for Rust).
import { compilePattern, PatternError, patternMatches } from '../../src/pattern.js';

export interface FuzzCase {
  readonly pattern: string;
  readonly inputs: readonly string[];
  /** The error message, or whether each input matches. */
  readonly result: string | readonly boolean[];
}

export function fuzzCases(count: number, seed: number): FuzzCase[] {
  let s = seed >>> 0;
  const rand = (): number => {
    // mulberry32
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rand() * xs.length)]!;
  const ATOMS = ['a', 'b', 'c', '.', '\\d', '\\w', '\\s', '\\D', '\\W', '\\S', '[ab]', '[^a]', '[a-c]', '[\\d-]', '[^\\w]', '[]', '[^]', '\\.', '\\n', 'é', '😀', '-', ' ', '0', '1'];
  const QUANTS = ['', '', '', '*', '+', '?', '{2}', '{1,}', '{0,2}', '*?', '+?'];
  const NOISE = ['(', ')', '[', ']', '{', '}', '|', '*', '\\', '^', '$', '{3,1}', '(?:', '(?=', '\\b', '\\-', '[z-a]', '[\\d-z]'];
  const expr = (depth: number): string => {
    const n = 1 + Math.floor(rand() * 4);
    let out = '';
    for (let i = 0; i < n; i++) {
      const r = rand();
      if (r < 0.15 && depth < 3) out += `(${rand() < 0.3 ? '?:' : ''}${expr(depth + 1)}${rand() < 0.4 ? '|' + expr(depth + 1) : ''})${pick(QUANTS)}`;
      else if (r < 0.2) out += pick(NOISE);
      else out += pick(ATOMS) + pick(QUANTS);
    }
    return out;
  };
  const CHARS = ['a', 'b', 'c', 'd', '0', '1', '-', '_', ' ', '\n', '\t', 'é', '😀', '.', 'A'];
  const input = (): string => Array.from({ length: Math.floor(rand() * 7) }, () => pick(CHARS)).join('');
  const cases: FuzzCase[] = [];
  for (let i = 0; i < count; i++) {
    let pattern = expr(0);
    if (rand() < 0.3) pattern = '^' + pattern;
    if (rand() < 0.3) pattern += '$';
    if (rand() < 0.15) pattern += '|' + expr(1);
    const inputs = Array.from({ length: 6 }, input);
    let result: string | boolean[];
    try {
      const p = compilePattern(pattern);
      result = inputs.map((x) => patternMatches(p, x));
    } catch (e) {
      if (!(e instanceof PatternError)) throw e;
      result = e.message;
    }
    cases.push({ pattern, inputs, result });
  }
  return cases;
}
