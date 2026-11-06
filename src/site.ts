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
