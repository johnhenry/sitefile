/**
 * `.sitefileignore`: a gitignore-style subset, implemented without dependencies.
 *
 * - Blank lines and lines starting with `#` are skipped (`\#` escapes a literal `#`).
 * - `!` negates a rule (`\!` escapes a literal `!`). The last matching rule wins.
 * - A trailing `/` makes the rule match directories only.
 * - A leading `/` or a `/` in the middle anchors the rule to the context root;
 *   otherwise it matches at any depth.
 * - `*` and `?` match within one path segment; `**` as a whole segment matches
 *   any number of segments.
 * - Unescaped trailing spaces are ignored. Character classes are not supported.
 *
 * As in gitignore, a file cannot be re-included once a parent directory is
 * excluded; callers get this by not descending into excluded directories.
 */

interface Rule {
  readonly negate: boolean;
  readonly dirOnly: boolean;
  readonly re: RegExp;
}

export class IgnoreRules {
  private constructor(private readonly rules: readonly Rule[]) {}

  static readonly empty = new IgnoreRules([]);

  static parse(text: string): IgnoreRules {
    const rules: Rule[] = [];
    for (const raw of text.split(/\r?\n/)) {
      const rule = parseLine(raw);
      if (rule) rules.push(rule);
    }
    return new IgnoreRules(rules);
  }

  get size(): number {
    return this.rules.length;
  }

  /** Whether the context-relative POSIX path (no leading `./`) is excluded by the last matching rule. */
  ignores(rel: string, isDirectory: boolean): boolean {
    let ignored = false;
    for (const r of this.rules) {
      if (r.dirOnly && !isDirectory) continue;
      if (r.re.test(rel)) ignored = !r.negate;
    }
    return ignored;
  }
}

function parseLine(raw: string): Rule | undefined {
  let line = raw.replace(/(?<!\\)\s+$/, '');
  if (line === '' || line.startsWith('#')) return undefined;
  let negate = false;
  if (line.startsWith('!')) {
    negate = true;
    line = line.slice(1);
  }
  const dirOnly = line.endsWith('/');
  if (dirOnly) line = line.replace(/\/+$/, '');
  const anchored = line.includes('/');
  line = line.replace(/^\/+/, '');
  if (line === '') return undefined;

  const segs = line.split('/');
  let src = '';
  segs.forEach((seg, i) => {
    const last = i === segs.length - 1;
    if (seg === '**') {
      // A trailing `**` needs at least one more segment; elsewhere it may match none.
      src += last ? '.+' : '(?:[^/]+/)*';
    } else {
      src += globSegment(seg) + (last ? '' : '/');
    }
  });
  return { negate, dirOnly, re: new RegExp(`^${anchored ? '' : '(?:.*/)?'}${src}$`) };
}

function globSegment(seg: string): string {
  let out = '';
  for (let i = 0; i < seg.length; i++) {
    const c = seg[i]!;
    if (c === '\\' && i + 1 < seg.length) {
      out += escapeRe(seg[++i]!);
    } else if (c === '*') {
      while (seg[i + 1] === '*') i++; // `**` inside a segment behaves like `*`
      out += '[^/]*';
    } else if (c === '?') {
      out += '[^/]';
    } else {
      out += escapeRe(c);
    }
  }
  return out;
}

function escapeRe(c: string): string {
  return c.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
}
