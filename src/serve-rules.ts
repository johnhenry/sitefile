// `_redirects` and `_headers` as `sitefile serve` reads them (docs/CLI.md, "serve").
// Ported as the rules section of rust/src/server.rs.
import { safeSegments } from './server.js';

/** A `_redirects` rule `serve` understands. */
export interface ServeRedirect {
  /** Decoded path without a trailing slash (`/` for the root); for a splat rule, the prefix before `/*` (`''` for `/*`). */
  readonly from: string;
  readonly splat: boolean;
  readonly to: string;
  readonly status: number;
  /** `301!`: applies even when a file exists at the path. */
  readonly force: boolean;
}

export interface ServeHeaders {
  readonly pattern: string;
  /** Lowercase names. */
  readonly headers: ReadonlyArray<readonly [string, string]>;
}

const HEADER_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
/** Headers the server owns; `_headers` cannot set them. */
const PROTECTED_HEADERS = new Set(['content-length', 'transfer-encoding', 'connection', 'keep-alive', 'etag', 'location', 'accept-ranges', 'content-range', 'last-modified', 'www-authenticate', 'content-encoding']);
const SERVE_STATUSES = new Set([200, 301, 302, 303, 307, 308, 404]);

/**
 * Parse `_redirects`: `from to [status[!]]` per line, `#` comments. Supported: exact paths and a
 * trailing `/*` with `:splat` in the target; statuses 301, 302, 303, 307, 308 (redirect), 200
 * (rewrite) and 404. Other lines (placeholders, conditions, query matching) are ignored.
 */
export function parseRedirectRules(text: string): ServeRedirect[] {
  const out: ServeRedirect[] = [];
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (t === '' || t.startsWith('#')) continue;
    const fields = t.split(/[ \t]+/);
    if (fields.length < 2 || fields.length > 3) continue;
    const from = fields[0]!;
    const to = fields[1]!;
    const st = fields[2] ?? '301';
    const force = st.endsWith('!');
    const code = force ? st.slice(0, -1) : st;
    if (!/^[0-9]{3}$/.test(code) || !SERVE_STATUSES.has(Number(code))) continue;
    const status = Number(code);
    if (!/^[\x21-\x7e]+$/.test(to) || !(to.startsWith('/') || /^https?:\/\/[^/]/i.test(to))) continue;
    if ((status === 200 || status === 404) && !to.startsWith('/')) continue;
    if (!from.startsWith('/')) continue;
    const splat = from === '/*' || from.endsWith('/*');
    const prefix = splat ? from.slice(0, -2) : from;
    if (prefix.includes('*') || prefix.split('/').some((seg) => seg.startsWith(':'))) continue;
    const segs = safeSegments(prefix === '' ? '/' : prefix);
    if (!segs) continue;
    const decoded = '/' + segs.join('/');
    out.push({ from: splat && decoded === '/' ? '' : decoded, splat, to, status, force });
  }
  return out;
}

/** Parse `_headers`: a path pattern line (`*` matches anything), then indented `Name: value` lines. */
export function parseHeaderRules(text: string): ServeHeaders[] {
  const out: Array<{ pattern: string; headers: Array<[string, string]> }> = [];
  let cur: { pattern: string; headers: Array<[string, string]> } | undefined;
  for (const raw of text.split('\n')) {
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    const t = line.trim();
    if (t === '' || t.startsWith('#')) continue;
    if (line[0] === ' ' || line[0] === '\t') {
      const i = t.indexOf(':');
      if (!cur || i <= 0) continue;
      const name = t.slice(0, i).trim().toLowerCase();
      const value = t.slice(i + 1).trim();
      if (!HEADER_TOKEN.test(name) || !/^[\t\x20-\x7e]*$/.test(value) || PROTECTED_HEADERS.has(name)) continue;
      cur.headers.push([name, value]);
      continue;
    }
    cur = t.startsWith('/') ? { pattern: t, headers: [] } : undefined;
    if (cur) out.push(cur);
  }
  return out;
}

/** Glob match where `*` matches any run of characters (including `/`). */
export function globMatch(pattern: string, s: string): boolean {
  let p = 0;
  let i = 0;
  let star = -1;
  let mark = 0;
  while (i < s.length) {
    if (p < pattern.length && pattern[p] !== '*' && pattern[p] === s[i]) {
      p++;
      i++;
    } else if (p < pattern.length && pattern[p] === '*') {
      star = p++;
      mark = i;
    } else if (star >= 0) {
      p = star + 1;
      i = ++mark;
    } else {
      return false;
    }
  }
  while (pattern[p] === '*') p++;
  return p === pattern.length;
}

/** Every matching block's headers; a name set by several blocks gets its values joined with `, `. */
export function headersFor(rules: readonly ServeHeaders[], path: string): Record<string, string> {
  // No prototype: a header named `__proto__` is an ordinary name (as in the Rust port).
  const out: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const r of rules) {
    if (!globMatch(r.pattern, path)) continue;
    for (const [k, v] of r.headers) out[k] = out[k] === undefined ? v : `${out[k]}, ${v}`;
  }
  return out;
}

/** The remainder a rule matches for `path` (`''` for the prefix itself), or undefined. */
export function matchRedirect(r: ServeRedirect, path: string): string | undefined {
  if (!r.splat) return path === r.from ? '' : undefined;
  if (r.from === '') return path.slice(1);
  if (path === r.from) return '';
  return path.startsWith(r.from + '/') ? path.slice(r.from.length + 1) : undefined;
}
