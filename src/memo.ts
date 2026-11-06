import { sha256 } from './digest.js';

/**
 * Incremental rebuilds (watch mode): results of build steps, memoized in memory by
 * content digests of everything each step reads, like Bazel's action cache.
 *
 * Keys are SHA-256 digests of a step's identity (the planned operation with its
 * Sitefile position, the ARG/LET values) and the digests of its inputs (file and
 * tree digests, import keys, earlier steps' keys). A step whose key is unchanged
 * returns its previous result without running; anything else runs as in a clean
 * build. Failures are never memoized. Every entry holds the warnings the step
 * printed, which a hit prints again, so an incremental build prints and writes
 * exactly what a clean build does.
 *
 * The memo lives only in memory, for one `build --watch` / `serve --watch`
 * session (see docs/ARCHITECTURE.md, "Incremental rebuilds"). Entries not used by
 * the latest build (or by the last successful one, while builds fail) are dropped.
 */
export interface MemoStats {
  /** Steps whose result was reused (operations, FOR elements, imports, documents). */
  hits: number;
  /** Steps that ran (and were memoized when they succeeded). */
  misses: number;
}

interface Entry {
  gen: number;
  readonly value: unknown;
  /** Entries computed inside this one (FOR elements, documents): reusing it uses them too. */
  readonly children: readonly string[];
}

export class BuildMemo {
  private readonly entries = new Map<string, Entry>();
  private gen = 0;
  private goodGen = 0;
  stats: MemoStats = { hits: 0, misses: 0 };

  /** Start a build: new statistics, a new generation. */
  begin(): void {
    this.gen++;
    this.stats = { hits: 0, misses: 0 };
  }

  /** End a build: keep what it used (and, after a failure, what the last good build used). */
  finish(ok: boolean): void {
    if (ok) this.goodGen = this.gen;
    const keep = Math.min(this.gen, this.goodGen);
    for (const [k, e] of this.entries) if (e.gen < keep) this.entries.delete(k);
  }

  get size(): number {
    return this.entries.size;
  }

  /** Mark an entry and everything inside it as used by this build. */
  private touch(e: Entry): void {
    if (e.gen === this.gen) return;
    e.gen = this.gen;
    for (const c of e.children) {
      const child = this.entries.get(c);
      if (child) this.touch(child);
    }
  }

  /** The value memoized under `key`; `parent` (an entry being computed) records it as a child. */
  get<T>(key: string, parent?: string[]): T | undefined {
    parent?.push(key);
    const e = this.entries.get(key);
    if (!e) return undefined;
    this.touch(e);
    this.stats.hits++;
    return e.value as T;
  }

  set(key: string, value: unknown, children: readonly string[] = []): void {
    this.stats.misses++;
    this.entries.set(key, { gen: this.gen, value, children });
  }

  /** `compute(children)` unless `key` is memoized (a throw is not memoized). */
  memoize<T>(key: string, compute: (children: string[]) => T, parent?: string[]): T {
    const hit = this.get<T>(key, parent);
    if (hit !== undefined) return hit;
    const children: string[] = [];
    const v = compute(children);
    this.set(key, v, children);
    return v;
  }

  /** Per-document memo for Markdown and data collections (one entry per file digest, path and label), children of `parent`. */
  documents(parent: string[]): DocumentMemo {
    return (parts, compute) => this.memoize(memoKey('document', ...parts), compute, parent);
  }
}

export type DocumentMemo = <T>(parts: readonly string[], compute: () => T) => T;

/** Set to a non-empty value other than `0` to make watch mode rebuild everything every time. */
export const NO_INCREMENTAL_ENV = 'SITEFILE_NO_INCREMENTAL';

/** The memo for a watch session: undefined when $SITEFILE_NO_INCREMENTAL turns it off. */
export function watchMemo(): BuildMemo | undefined {
  const v = process.env[NO_INCREMENTAL_ENV];
  return v !== undefined && v !== '' && v !== '0' ? undefined : new BuildMemo();
}

/** A memo key: the digest of its parts (strings joined unambiguously). */
export function memoKey(...parts: readonly string[]): string {
  return sha256(parts.map((p) => `${p.length}:${p}`).join(''));
}

/**
 * An unambiguous text form of a JSON-like value (imported data, ARG/LET values, plans).
 * Unlike JSON.stringify alone it tells `undefined`, NaN and ±Infinity apart from null.
 */
export function valueKey(v: unknown): string {
  return (
    JSON.stringify(v, (_k, x: unknown) => {
      if (x === undefined) return { '\u0000': 'undefined' };
      if (typeof x === 'number' && !Number.isFinite(x)) return { '\u0000': String(x) };
      if (x instanceof Map) return { '\u0000': 'map', entries: [...x] };
      return x;
    }) ?? 'undefined'
  );
}
