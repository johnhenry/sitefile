// `_redirects` and `_headers` as `sitefile serve` reads them (docs/CLI.md, "serve").
// Ported as the rules section of rust/src/server.rs.
import { safeSegments } from './server.js';
import type { Tree } from './tree.js';

/** A segment of a `_redirects` source: a literal (decoded) or a `:placeholder`. */
export type RedirectSegment = { readonly lit: string } | { readonly param: string };

/** A `_redirects` condition: `Language=`, `Country=` or `Cookie=` (lowercase key) and its values. */
export interface RedirectCondition {
  readonly key: 'language' | 'country' | 'cookie';
  /** ASCII-lowercased for language and country; cookie names as written. */
  readonly values: readonly string[];
}

/** A `_redirects` rule `serve` understands. */
export interface ServeRedirect {
  /** 1-based line in `_redirects` (for the access log). */
  readonly line: number;
  /**
   * Decoded path without a trailing slash (`/` for the root), placeholders written `:name`; for a
   * splat rule, the prefix before `/*` (`''` for `/*`).
   */
  readonly from: string;
  readonly segments: readonly RedirectSegment[];
  readonly splat: boolean;
  /** Query parameters the request must have: `[name, value]` (decoded); a value `:name` captures it. */
  readonly query: ReadonlyArray<readonly [string, string]>;
  readonly to: string;
  readonly status: number;
  /** `301!`: applies even when a file exists at the path. */
  readonly force: boolean;
  readonly conditions: readonly RedirectCondition[];
}

/** A line of `_redirects` or `_headers` that `serve` ignores, and why. */
export interface IgnoredLine {
  readonly line: number;
  readonly reason: string;
}

/** What a request offers a rule's query parameters and conditions. */
export interface RedirectRequest {
  /** The query string with its `?` (`''` when none). */
  readonly search: string;
  readonly acceptLanguage?: string | undefined;
  readonly cookie?: string | undefined;
  /** `serve --country`. */
  readonly country?: string | undefined;
}

export interface ServeHeaders {
  readonly pattern: string;
  /** Lowercase names. */
  readonly headers: ReadonlyArray<readonly [string, string]>;
}

const HEADER_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
/** Headers the server owns; `_headers` cannot set them. */
const PROTECTED_HEADERS = new Set(['content-length', 'transfer-encoding', 'connection', 'keep-alive', 'etag', 'location', 'accept-ranges', 'content-range', 'last-modified', 'www-authenticate', 'content-encoding']);
export const SERVE_STATUSES: readonly number[] = [200, 301, 302, 303, 307, 308, 404, 410, 451];
/** Statuses that serve the target path's file with that status instead of redirecting. */
export const isRewriteStatus = (s: number): boolean => s === 200 || s === 404 || s === 410 || s === 451;
const PLACEHOLDER = /^[A-Za-z_][A-Za-z0-9_]*$/;
const STATUS_FIELD = /^[0-9]{3}!?$/;

const asciiLower = (s: string): string => s.replace(/[A-Z]/g, (c) => c.toLowerCase());

/** `decodeURIComponent` after `+` becomes a space (form encoding), or undefined when malformed. */
function decodeQueryPart(s: string): string | undefined {
  try {
    return decodeURIComponent(s.replace(/\+/g, ' '));
  } catch {
    return undefined;
  }
}

/** The decoded `name=value` pairs of a query string (without its `?`); malformed pairs are skipped. */
export function parseQuery(q: string): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  for (const part of q.split('&')) {
    if (part === '') continue;
    const i = part.indexOf('=');
    const name = decodeQueryPart(i === -1 ? part : part.slice(0, i));
    const value = decodeQueryPart(i === -1 ? '' : part.slice(i + 1));
    if (name !== undefined && value !== undefined) out.push([name, value]);
  }
  return out;
}

const isTargetField = (f: string): boolean => f.startsWith('/') || /^https?:\/\//i.test(f);

/**
 * Parse `_redirects` (Netlify / Cloudflare Pages): `from [name=value…] to [status[!]] [Key=values…]`
 * per line, `#` comments. Supported: literal segments, `:placeholders` (one segment each) and a
 * trailing `/*` (`:splat`); query parameters, literal or `:placeholder`; statuses 301, 302, 303,
 * 307, 308 (redirect), 200 (rewrite), 404, 410 and 451 (the target's file with that status);
 * conditions `Language=`, `Country=` and `Cookie=`. Every other line is in `ignored`, with why.
 */
export function parseRedirects(text: string): { rules: ServeRedirect[]; ignored: IgnoredLine[] } {
  const rules: ServeRedirect[] = [];
  const ignored: IgnoredLine[] = [];
  text.split('\n').forEach((raw, index) => {
    const t = raw.trim();
    if (t === '' || t.startsWith('#')) return;
    const r = parseRedirectLine(t, index + 1);
    if (typeof r === 'string') ignored.push({ line: index + 1, reason: r });
    else rules.push(r);
  });
  return { rules, ignored };
}

/** One non-comment line: its rule, or why it is ignored. */
function parseRedirectLine(t: string, line: number): ServeRedirect | string {
  const fields = t.split(/[ \t]+/);
  const from = fields[0]!;
  let k = 1;
  const query: Array<[string, string]> = [];
  for (; k < fields.length && !isTargetField(fields[k]!); k++) {
    const f = fields[k]!;
    const i = f.indexOf('=');
    if (i <= 0) break;
    const name = decodeQueryPart(f.slice(0, i));
    const value = decodeQueryPart(f.slice(i + 1));
    if (name === undefined || value === undefined || (value.startsWith(':') && !PLACEHOLDER.test(value.slice(1)))) {
      return 'bad query parameter (expected name=value or name=:placeholder)';
    }
    query.push([name, value]);
  }
  if (k >= fields.length) return "expected 'from [name=value...] to [status] [conditions]'";
  const to = fields[k++]!;
  let st = '301';
  if (k < fields.length && STATUS_FIELD.test(fields[k]!)) st = fields[k++]!;
  const conditions: RedirectCondition[] = [];
  for (; k < fields.length; k++) {
    const f = fields[k]!;
    const i = f.indexOf('=');
    if (i <= 0) return 'unexpected field (expected a status, then Key=value conditions)';
    const key = asciiLower(f.slice(0, i));
    const values = f
      .slice(i + 1)
      .split(',')
      .filter((x) => x !== '');
    if (key === 'role') return 'Role= conditions need Netlify Identity and are not supported';
    if (key !== 'language' && key !== 'country' && key !== 'cookie') return 'unsupported condition (supported: Language=, Country=, Cookie=)';
    if (values.length === 0) return 'a condition needs at least one value';
    conditions.push({ key, values: key === 'cookie' ? values : values.map(asciiLower) });
  }
  const force = st.endsWith('!');
  const status = Number(force ? st.slice(0, -1) : st);
  if (!SERVE_STATUSES.includes(status)) return `unsupported status (supported: ${SERVE_STATUSES.join(', ')})`;
  if (!/^[\x21-\x7e]+$/.test(to) || !(to.startsWith('/') || /^https?:\/\/[^/]/i.test(to))) {
    return 'the target must be an absolute path or an http(s) URL, in printable ASCII';
  }
  if (isRewriteStatus(status) && status !== 200 && !to.startsWith('/')) return `status ${status} needs a path target`;
  if (!from.startsWith('/')) return "the source must be a path starting with '/'";
  const splat = from === '/*' || from.endsWith('/*');
  const prefix = splat ? from.slice(0, -2) : from;
  const segments: RedirectSegment[] = [];
  const names = new Set<string>();
  for (const raw of prefix.split('/')) {
    if (raw === '') continue;
    if (raw.includes('*')) return "'*' is only supported as the last segment of the source";
    if (raw.startsWith(':')) {
      const name = raw.slice(1);
      if (!PLACEHOLDER.test(name)) return 'bad placeholder (expected :name of letters, digits and _)';
      if (name === 'splat') return ":splat is the value of a trailing '*', not a placeholder";
      if (names.has(name)) return 'a placeholder is used twice in the source';
      names.add(name);
      segments.push({ param: name });
      continue;
    }
    const seg = safeSegments('/' + raw);
    if (!seg) return 'the source is not a valid path';
    segments.push({ lit: seg[0]! });
  }
  const shown = '/' + segments.map((s) => ('param' in s ? `:${s.param}` : s.lit)).join('/');
  return { line, from: splat && shown === '/' ? '' : shown, segments, splat, query, to, status, force, conditions };
}

/** {@link parseRedirects}' rules. */
export function parseRedirectRules(text: string): ServeRedirect[] {
  return parseRedirects(text).rules;
}

/** A rule that depends on nothing but the path: no placeholders, query parameters or conditions. */
export function isPlainRule(r: ServeRedirect): boolean {
  return r.query.length === 0 && r.conditions.length === 0 && r.segments.every((s) => 'lit' in s);
}

/**
 * Parse `_headers`: a path pattern line (`*` matches anything), then indented `Name: value` lines.
 * Every other line is in `ignored`, with why.
 */
export function parseHeaders(text: string): { rules: ServeHeaders[]; ignored: IgnoredLine[] } {
  const out: Array<{ pattern: string; headers: Array<[string, string]> }> = [];
  const ignored: IgnoredLine[] = [];
  let cur: { pattern: string; headers: Array<[string, string]> } | undefined;
  text.split('\n').forEach((raw, index) => {
    const skip = (reason: string): void => void ignored.push({ line: index + 1, reason });
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    const t = line.trim();
    if (t === '' || t.startsWith('#')) return;
    if (line[0] === ' ' || line[0] === '\t') {
      if (!cur) return skip('a header line without a path line above it');
      if (t.startsWith('!')) return skip("'! Name' detach lines are not supported");
      const i = t.indexOf(':');
      if (i <= 0) return skip("expected 'Name: value'");
      const name = asciiLower(t.slice(0, i).trim());
      const value = t.slice(i + 1).trim();
      if (!HEADER_TOKEN.test(name)) return skip('invalid header name');
      if (!/^[\t\x20-\x7e]*$/.test(value)) return skip('the value must be printable ASCII');
      if (PROTECTED_HEADERS.has(name)) return skip('the server sets this header itself');
      cur.headers.push([name, value]);
      return;
    }
    cur = t.startsWith('/') ? { pattern: t, headers: [] } : undefined;
    if (cur) out.push(cur);
    else skip("a path pattern must start with '/' (indent header lines)");
  });
  return { rules: out, ignored };
}

/** {@link parseHeaders}' rules. */
export function parseHeaderRules(text: string): ServeHeaders[] {
  return parseHeaders(text).rules;
}

/**
 * The warnings for a tree's `/_redirects` and `/_headers` lines `serve` ignores, one per line:
 * `_redirects:3: ignored: <reason>`. `content` reads files a zip served in place holds only as digests.
 */
export function siteFileWarnings(tree: Tree, content?: { read(path: string): Uint8Array | undefined }): string[] {
  const out: string[] = [];
  for (const [file, parse] of [
    ['_redirects', parseRedirects],
    ['_headers', parseHeaders],
  ] as const) {
    const f = tree.get(`/${file}`);
    if (!f) continue;
    for (const i of parse(new TextDecoder().decode(content?.read(`/${file}`) ?? f.bytes)).ignored) out.push(`${file}:${i.line}: ignored: ${i.reason}`);
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

/**
 * The values a rule's source captures from `path` (decoded, no trailing slash): each placeholder,
 * and `splat` (the remainder, `''` for the prefix itself) for a splat rule; undefined when it does
 * not match.
 */
export function matchRedirect(r: ServeRedirect, path: string): Map<string, string> | undefined {
  const segs = path.split('/').filter((s) => s !== '');
  const n = r.segments.length;
  if (r.splat ? segs.length < n : segs.length !== n) return undefined;
  const caps = new Map<string, string>();
  for (let i = 0; i < n; i++) {
    const s = r.segments[i]!;
    if ('param' in s) caps.set(s.param, segs[i]!);
    else if (s.lit !== segs[i]) return undefined;
  }
  if (r.splat) caps.set('splat', segs.slice(n).join('/'));
  return caps;
}

/** Whether `Accept-Language` names a language a `Language=` value matches (`en` matches `en-GB`). */
function languageMatches(values: readonly string[], header: string): boolean {
  for (const part of header.split(',')) {
    const [tag, ...params] = part.split(';');
    const q = params.map((p) => p.trim()).find((p) => p.startsWith('q=') || p.startsWith('Q='));
    if (q !== undefined && /^0(\.0{0,3})?$/.test(q.slice(2))) continue;
    const t = asciiLower(tag!.trim());
    if (t !== '' && values.some((v) => t === v || t.startsWith(`${v}-`))) return true;
  }
  return false;
}

/** Whether a `Cookie` header has a cookie with one of these names. */
function cookieMatches(names: readonly string[], header: string): boolean {
  return header.split(';').some((c) => {
    const i = c.indexOf('=');
    return i > 0 && names.includes(c.slice(0, i).trim());
  });
}

/** {@link matchRedirect}, then the rule's query parameters (adding their captures) and conditions. */
export function matchRule(r: ServeRedirect, path: string, req: RedirectRequest): Map<string, string> | undefined {
  const caps = matchRedirect(r, path);
  if (!caps) return undefined;
  if (r.query.length > 0) {
    const pairs = parseQuery(req.search.slice(1));
    for (const [name, value] of r.query) {
      const hit = pairs.find(([n]) => n === name);
      if (!hit) return undefined;
      if (value.startsWith(':')) caps.set(value.slice(1), hit[1]);
      else if (hit[1] !== value) return undefined;
    }
  }
  for (const c of r.conditions) {
    const ok =
      c.key === 'language'
        ? languageMatches(c.values, req.acceptLanguage ?? '')
        : c.key === 'country'
          ? req.country !== undefined && c.values.includes(asciiLower(req.country))
          : cookieMatches(c.values, req.cookie ?? '');
    if (!ok) return undefined;
  }
  return caps;
}

/**
 * A rule's target with `:name` (letters, digits and `_`, the longest run) replaced by the captured
 * value, percent-encoded like encodeURIComponent; `:splat` keeps its slashes. Other names stay.
 */
export function substituteTarget(to: string, caps: ReadonlyMap<string, string>): string {
  return to.replace(/:([A-Za-z_][A-Za-z0-9_]*)/g, (whole, name: string) => {
    const v = caps.get(name);
    if (v === undefined) return whole;
    return name === 'splat' ? v.split('/').map(encodeURIComponent).join('/') : encodeURIComponent(v);
  });
}
