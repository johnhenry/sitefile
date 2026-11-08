// Site files: sitemap.xml, an Atom feed, _redirects and _headers (SITEMAP, FEED,
// REDIRECT, HEADER; docs/LANGUAGE.md "Site files"). Ported as rust/src/site.rs:
// both must produce the same bytes and messages.
import { SitefileError, type SourceSpan } from './errors.js';
import type { RedirectStatus } from './language/ast.js';
import type { Tree } from './tree.js';

/** ASCII whitespace, other control characters, and DEL. */
const SPACE_OR_CONTROL = /[\u0000- \u007f]/;
/** Control characters other than tab. */
const CONTROL = /[\u0000-\u0008\u000a-\u001f\u007f]/;
/** An RFC 9110 token (header names). */
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/**
 * Check a SITEMAP/FEED `BASE`: an absolute http(s) URL with a host and no credentials,
 * query, fragment, whitespace or control characters. Returns it without trailing slashes.
 */
export function checkBaseUrl(raw: string, what: string, span: SourceSpan): string {
  const m = /^https?:\/\/([^/]*)/i.exec(raw);
  if (!m || m[1] === '' || m[1]!.includes('@') || /[\u0000- \u007f?#]/.test(raw)) {
    throw new SitefileError(
      'E_VALIDATION',
      `${what} '${raw}' must be an absolute http(s) URL such as https://example.com (no credentials, query or fragment)`,
      span,
    );
  }
  return raw.replace(/\/+$/, '');
}

/** `<base><path>` with each segment percent-encoded; `…/index.html` becomes its directory URL `…/`. */
export function pageUrl(base: string, path: string): string {
  const p = path.endsWith('/index.html') ? path.slice(0, -'index.html'.length) : path;
  return base + p.split('/').map(encodeURIComponent).join('/');
}

/** Escape text for XML content and attribute values; characters XML 1.0 forbids become U+FFFD. */
export function xmlEscape(s: string): string {
  return s.replace(/[&<>"'\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, (c) =>
    c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : c === '"' ? '&quot;' : c === "'" ? '&apos;' : '�',
  );
}

function utf8Compare(a: string, b: string): number {
  return Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
}

/** sitemaps.org XML for every `.html` file in the tree except `/404.html`, sorted by path. */
export function sitemapXml(tree: Tree, base: string): string {
  const paths = tree
    .entries()
    .map(([p]) => p)
    .filter((p) => p.endsWith('.html') && p !== '/404.html')
    .sort(utf8Compare);
  let out = '<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n';
  for (const p of paths) out += `  <url><loc>${xmlEscape(pageUrl(base, p))}</loc></url>\n`;
  return out + '</urlset>\n';
}

// ---------------------------------------------------------------- dates

export interface FeedDate {
  /** RFC 3339 form written to the feed. */
  readonly text: string;
  /** Seconds since the epoch (UTC), for picking the newest entry. */
  readonly seconds: number;
  /** Fractional seconds as 9 digits. */
  readonly fraction: string;
}

function daysFromCivil(y: number, m: number, d: number): number {
  const yy = m <= 2 ? y - 1 : y;
  const era = Math.floor(yy / 400);
  const yoe = yy - era * 400;
  const doy = Math.floor((153 * (m + (m > 2 ? -3 : 9)) + 2) / 5) + d - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}

function daysInMonth(y: number, m: number): number {
  if (m === 2) return (y % 4 === 0 && y % 100 !== 0) || y % 400 === 0 ? 29 : 28;
  return [4, 6, 9, 11].includes(m) ? 30 : 31;
}

/** `YYYY-MM-DD` (midnight UTC) or an RFC 3339 date-time; undefined when it is neither. */
export function parseFeedDate(s: string): FeedDate | undefined {
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:[Tt ](\d{2}):(\d{2})(?::(\d{2})(\.\d+)?)?([Zz]|[+-]\d{2}:\d{2}))?$/.exec(s);
  if (!m) return undefined;
  const [y, mo, d] = [Number(m[1]), Number(m[2]), Number(m[3])];
  if (mo < 1 || mo > 12 || d < 1 || d > daysInMonth(y, mo)) return undefined;
  const date = `${m[1]}-${m[2]}-${m[3]}`;
  if (m[4] === undefined) return { text: `${date}T00:00:00Z`, seconds: daysFromCivil(y, mo, d) * 86400, fraction: '000000000' };
  const [h, mi, sec] = [Number(m[4]), Number(m[5]), Number(m[6] ?? '0')];
  if (h > 23 || mi > 59 || sec > 59) return undefined;
  const zone = m[8]!.toUpperCase();
  let offset = 0;
  if (zone !== 'Z') {
    const [oh, om] = [Number(zone.slice(1, 3)), Number(zone.slice(4, 6))];
    if (oh > 23 || om > 59) return undefined;
    offset = (zone[0] === '-' ? -1 : 1) * (oh * 3600 + om * 60);
  }
  const frac = m[7] ?? '';
  const text = `${date}T${m[4]}:${m[5]}:${m[6] ?? '00'}${frac}${zone}`;
  const seconds = daysFromCivil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec - offset;
  return { text, seconds, fraction: (frac.slice(1) + '000000000').slice(0, 9) };
}

function newer(a: FeedDate, b: FeedDate): boolean {
  return a.seconds > b.seconds || (a.seconds === b.seconds && a.fraction > b.fraction);
}

// ---------------------------------------------------------------- feed

export interface FeedEntry {
  readonly title: string;
  readonly url: string;
  readonly date: FeedDate;
  readonly author?: string;
  readonly summary?: string;
  readonly content?: string;
}

/** Atom 1.0. The feed's id is its own URL; each entry's id is its link. */
export function atomXml(feed: { title: string; siteUrl: string; selfUrl: string; entries: readonly FeedEntry[] }): string {
  let updated: FeedDate | undefined;
  for (const e of feed.entries) if (!updated || newer(e.date, updated)) updated = e.date;
  const x = xmlEscape;
  let out = '<?xml version="1.0" encoding="utf-8"?>\n<feed xmlns="http://www.w3.org/2005/Atom">\n';
  out += `  <title>${x(feed.title)}</title>\n`;
  out += `  <link href="${x(feed.siteUrl)}"/>\n`;
  out += `  <link rel="self" href="${x(feed.selfUrl)}"/>\n`;
  out += `  <id>${x(feed.selfUrl)}</id>\n`;
  out += `  <updated>${updated?.text ?? '1970-01-01T00:00:00Z'}</updated>\n`;
  for (const e of feed.entries) {
    out += '  <entry>\n';
    out += `    <title>${x(e.title)}</title>\n`;
    out += `    <link href="${x(e.url)}"/>\n`;
    out += `    <id>${x(e.url)}</id>\n`;
    out += `    <updated>${e.date.text}</updated>\n`;
    if (e.author !== undefined) out += `    <author><name>${x(e.author)}</name></author>\n`;
    if (e.summary !== undefined) out += `    <summary>${x(e.summary)}</summary>\n`;
    if (e.content !== undefined) out += `    <content type="html">${x(e.content)}</content>\n`;
    out += '  </entry>\n';
  }
  return out + '</feed>\n';
}

// ---------------------------------------------------------------- _redirects and _headers

export interface RedirectRule {
  readonly from: string;
  readonly to: string;
  readonly status: RedirectStatus;
}

export interface HeaderRule {
  readonly pattern: string;
  readonly name: string;
  readonly value: string;
}

/** Check a REDIRECT source (`from`) or HEADER pattern: an absolute path with no spaces or control characters. */
export function checkSitePath(raw: string, what: string, span: SourceSpan): string {
  if (SPACE_OR_CONTROL.test(raw)) throw new SitefileError('E_VALIDATION', `${what} must not contain spaces or control characters`, span);
  if (!raw.startsWith('/')) throw new SitefileError('E_VALIDATION', `${what} '${raw}' must be an absolute path starting with '/'`, span);
  return raw;
}

/** Check a REDIRECT target: an absolute path or an http(s) URL. */
export function checkRedirectTarget(raw: string, span: SourceSpan): string {
  if (SPACE_OR_CONTROL.test(raw)) throw new SitefileError('E_VALIDATION', 'REDIRECT target must not contain spaces or control characters', span);
  if (!raw.startsWith('/') && !/^https?:\/\/[^/]/i.test(raw)) {
    throw new SitefileError('E_VALIDATION', `REDIRECT target '${raw}' must be an absolute path or an http(s) URL`, span);
  }
  return raw;
}

export function checkHeaderName(raw: string, span: SourceSpan): string {
  if (SPACE_OR_CONTROL.test(raw)) throw new SitefileError('E_VALIDATION', 'HEADER name must not contain spaces or control characters', span);
  if (!TOKEN.test(raw)) {
    throw new SitefileError('E_VALIDATION', `HEADER name '${raw}' is not a valid header name (letters, digits and !#$%&'*+-.^_\`|~)`, span);
  }
  return raw;
}

/** The header value: trimmed of spaces and tabs; non-empty, no control characters other than tab. */
export function checkHeaderValue(raw: string, name: string, span: SourceSpan): string {
  if (CONTROL.test(raw)) throw new SitefileError('E_VALIDATION', `HEADER ${name}: the value must not contain control characters (CR, LF, …)`, span);
  const v = raw.replace(/^[ \t]+|[ \t]+$/g, '');
  if (v === '') throw new SitefileError('E_VALIDATION', `HEADER ${name}: the value is empty`, span);
  return v;
}

/** `/_redirects` in the Netlify / Cloudflare Pages format: `from to status`, one per line, in source order. */
export function redirectsFile(rules: readonly RedirectRule[]): string {
  return rules.map((r) => `${r.from} ${r.to} ${r.status}\n`).join('');
}

/** `/_headers`: one block per path pattern (in order of first use), its headers indented in source order. */
export function headersFile(rules: readonly HeaderRule[]): string {
  const blocks = new Map<string, string[]>();
  for (const r of rules) {
    const b = blocks.get(r.pattern) ?? [];
    b.push(`  ${r.name}: ${r.value}\n`);
    blocks.set(r.pattern, b);
  }
  let out = '';
  for (const [pattern, lines] of blocks) out += `${pattern}\n${lines.join('')}`;
  return out;
}

// ---------------------------------------------------------------- FINGERPRINT cache headers

/** The `Cache-Control` of a fingerprinted file: its name changes with its content, so it never goes stale. */
export const IMMUTABLE = 'public, max-age=31536000, immutable';
/** The `Cache-Control` of a page: revalidate on every request, so it always names the current assets. */
export const NO_CACHE = 'no-cache';

/**
 * The `/_headers` rules EXPORT adds for FINGERPRINT (docs/LANGUAGE.md, "Fingerprinting"). `files` are the
 * URL paths of every file in the exported directory, `hashed` those of the fingerprinted ones. A
 * fingerprinted file gets `Cache-Control: public, max-age=31536000, immutable`, by a `/dir/*` rule for the
 * topmost directory (below the root) that holds only fingerprinted files, or else by its own path; pages
 * get `Cache-Control: no-cache` through `/`, `/*/` and `/*.html` (each only when a page it matches
 * exists). Paths with spaces or control characters are left out (a `_headers` path cannot hold them).
 * Rules are in path order (UTF-8), immutable ones first; none when nothing is fingerprinted.
 */
export function cacheHeaderRules(files: readonly string[], hashed: ReadonlySet<string>): HeaderRule[] {
  const ok = (p: string): boolean => !SPACE_OR_CONTROL.test(p);
  const sorted = [...files].sort(utf8Compare);
  const counts = new Map<string, { total: number; hashed: number }>();
  for (const f of sorted) {
    const segs = f.split('/');
    for (let k = 2; k < segs.length; k++) {
      const d = segs.slice(0, k).join('/');
      const c = counts.get(d) ?? { total: 0, hashed: 0 };
      c.total++;
      if (hashed.has(f)) c.hashed++;
      counts.set(d, c);
    }
  }
  const out: HeaderRule[] = [];
  const seen = new Set<string>();
  const add = (pattern: string, value: string): void => {
    if (seen.has(pattern) || !ok(pattern)) return;
    seen.add(pattern);
    out.push({ pattern, name: 'Cache-Control', value });
  };
  for (const f of sorted) {
    if (!hashed.has(f)) continue;
    const segs = f.split('/');
    let pattern = f;
    for (let k = 2; k < segs.length; k++) {
      const d = segs.slice(0, k).join('/');
      const c = counts.get(d)!;
      if (c.total === c.hashed) {
        pattern = d + '/*';
        break;
      }
    }
    add(pattern, IMMUTABLE);
  }
  if (out.length === 0) return out;
  const pages = sorted.filter((f) => /\.html$/i.test(f));
  if (pages.includes('/index.html')) add('/', NO_CACHE);
  if (pages.some((f) => f !== '/index.html' && /\/index\.html$/i.test(f))) add('/*/', NO_CACHE);
  if (pages.length > 0) add('/*.html', NO_CACHE);
  return out;
}
// ---------------------------------------------------------------- robots.txt and security.txt

/** `/robots.txt` (RFC 9309) for every user agent: the rules in order (an empty `Disallow:` when none), then a `Sitemap:` line per URL. */
export function robotsTxt(rules: ReadonlyArray<{ readonly allow: boolean; readonly path: string }>, sitemaps: readonly string[]): string {
  let out = 'User-agent: *\n';
  if (rules.length === 0) out += 'Disallow:\n';
  for (const r of rules) out += `${r.allow ? 'Allow' : 'Disallow'}: ${r.path}\n`;
  if (sitemaps.length > 0) out += '\n' + sitemaps.map((u) => `Sitemap: ${u}\n`).join('');
  return out;
}

/** A security.txt URI field: one of `schemes` (lowercase, with their `:` or `://`), no spaces or control characters. */
export function checkFieldUri(raw: string, field: string, schemes: readonly string[], span: SourceSpan): string {
  const lower = raw.toLowerCase();
  if (SPACE_OR_CONTROL.test(raw) || !schemes.some((s) => lower.startsWith(s) && raw.length > s.length)) {
    throw new SitefileError('E_VALIDATION', `SECURITY_TXT ${field} '${raw}' must be a ${schemes.map((s) => s.replace(/\/\/$/, '//…')).join(', ')} URI with no spaces`, span);
  }
  return raw;
}

export interface SecurityTxt {
  readonly contact: readonly string[];
  /** RFC 3339, as {@link parseFeedDate} writes it. */
  readonly expires: string;
  readonly encryption: readonly string[];
  readonly policy: readonly string[];
}

/** RFC 9116 `security.txt`: Contact lines, Expires, then Encryption and Policy lines, in source order. */
export function securityTxt(s: SecurityTxt): string {
  return [
    ...s.contact.map((v) => `Contact: ${v}\n`),
    `Expires: ${s.expires}\n`,
    ...s.encryption.map((v) => `Encryption: ${v}\n`),
    ...s.policy.map((v) => `Policy: ${v}\n`),
  ].join('');
}
