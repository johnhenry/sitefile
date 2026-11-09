import { SitefileError, type SourceSpan } from './errors.js';
import type { ExportHost } from './language/ast.js';
import { globMatch } from './serve-rules.js';
import { makeFile, Tree } from './tree.js';

/**
 * `EXPORT … FOR <host>` (issue #205): write what each host reads for the REDIRECT and HEADER rules (and the
 * FINGERPRINT cache headers), the reverse of `sitefile import-config`. Ported as rust/src/export_for.rs.
 *
 * A rule the host cannot express as `sitefile serve` (and Netlify) apply it is an E_EXPORT error naming the
 * rule, or, with `--lossy`, a warning and the rule is left out. FINGERPRINT's own cache headers are never an
 * error: when the host cannot carry them they are left out with a warning. docs/LANGUAGE.md, "EXPORT … FOR".
 */
export interface HostRedirect {
  readonly from: string;
  readonly to: string;
  readonly status: number;
  readonly span: SourceSpan;
}

export interface HostHeader {
  readonly pattern: string;
  readonly name: string;
  readonly value: string;
  readonly span: SourceSpan;
  /** Written by FINGERPRINT, not a HEADER line. */
  readonly auto: boolean;
}

export interface HostOptions {
  readonly label: string;
  readonly lossy: boolean;
  readonly warn: (m: string) => void;
  /** EXPORT's BASE (GitHub Pages: the custom domain for CNAME). */
  readonly base: string | undefined;
  readonly span: SourceSpan;
}

export interface HostResult {
  /** The rules that stay (Cloudflare: written as `_redirects` and `_headers`). */
  readonly redirects: readonly HostRedirect[];
  readonly headers: readonly HostHeader[];
  /** The exported files, moved and with the host's files added (unused for Cloudflare). */
  readonly output: Tree;
}

const PLACEHOLDER_SEG = /^:[A-Za-z_][A-Za-z0-9_]*$/;

type FromShape =
  | { readonly kind: 'exact'; readonly path: string }
  | { readonly kind: 'splat'; readonly prefix: string }
  | { readonly kind: 'placeholders'; readonly segments: readonly string[]; readonly splat: boolean }
  | { readonly kind: 'other' };

/** What a REDIRECT source is: an exact path, `<prefix>/*`, `:placeholder` segments (and a trailing `/*`), or other. */
function shapeOf(from: string): FromShape {
  const splat = from === '/*' || from.endsWith('/*');
  const head = splat ? from.slice(0, -2) : from;
  if (head.includes('*')) return { kind: 'other' };
  const segments = head.split('/').slice(1);
  const named = segments.some((s) => s.startsWith(':'));
  if (named) {
    if (!segments.every((s) => !s.startsWith(':') || PLACEHOLDER_SEG.test(s))) return { kind: 'other' };
    return { kind: 'placeholders', segments, splat };
  }
  if (splat) return { kind: 'splat', prefix: head };
  return { kind: 'exact', path: from };
}

/** The path a file in `tree` answers for `from` (Netlify serves a file before a rule), if any. */
function shadowingFile(tree: Tree, from: string): string | undefined {
  const s = shapeOf(from);
  const files = tree.entries().map(([p]) => p);
  if (s.kind === 'exact') {
    const p = s.path.length > 1 && s.path.endsWith('/') ? s.path.slice(0, -1) : s.path;
    for (const c of p === '/' ? ['/index.html'] : [p, `${p}/index.html`, `${p}.html`]) if (tree.isFile(c)) return c;
    return undefined;
  }
  const prefix =
    s.kind === 'splat' ? s.prefix : s.kind === 'placeholders' ? '/' + s.segments.slice(0, s.segments.findIndex((x) => x.startsWith(':'))).join('/') : '';
  const dir = prefix === '/' || prefix === '' ? '/' : `${prefix}/`;
  return files.find((f) => f.startsWith(dir));
}

class Ctx {
  constructor(
    readonly host: ExportHost,
    readonly o: HostOptions,
  ) {}

  redirectText(r: HostRedirect): string {
    return `REDIRECT ${r.from} ${r.to} ${r.status}`;
  }

  /** Refuse `r` (E_EXPORT), or with --lossy warn and leave it out. */
  refuse(r: HostRedirect, reason: string): void {
    const what = this.redirectText(r);
    if (this.o.lossy) {
      this.o.warn(`${this.o.label}:${r.span.line} ${what}: ${reason}; left out (--lossy)`);
      return;
    }
    throw new SitefileError('E_EXPORT', `EXPORT … FOR ${this.host}: ${what}: ${reason} (build with --lossy to leave it out)`, r.span);
  }

  refuseHeader(h: HostHeader, reason: string): void {
    if (h.auto) {
      this.o.warn(`${this.o.label}:${h.span.line} FINGERPRINT: the ${h.name} header for ${h.pattern} is left out: ${reason}`);
      return;
    }
    const what = `HEADER ${h.pattern} ${h.name}`;
    if (this.o.lossy) {
      this.o.warn(`${this.o.label}:${h.span.line} ${what}: ${reason}; left out (--lossy)`);
      return;
    }
    throw new SitefileError('E_EXPORT', `EXPORT … FOR ${this.host}: ${what}: ${reason} (build with --lossy to leave it out)`, h.span);
  }

  /** False (and refused) when a file in the export answers the rule's path first on Netlify. */
  unshadowed(tree: Tree, r: HostRedirect, hostName: string): boolean {
    const f = shadowingFile(tree, r.from);
    if (f === undefined) return true;
    this.refuse(r, `${f} is a file in the export: Netlify and sitefile serve answer with the file, ${hostName} applies the redirect first`);
    return false;
  }
}

/** Header rules grouped by pattern, in order of first use. */
function blocks(headers: readonly HostHeader[]): Array<[string, HostHeader[]]> {
  const m = new Map<string, HostHeader[]>();
  for (const h of headers) m.set(h.pattern, [...(m.get(h.pattern) ?? []), h]);
  return [...m];
}

const json = (v: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(v, null, 2) + '\n');

/** `output` with every file moved below `dir`. */
function moved(output: Tree, dir: string): Tree {
  return Tree.fromFiles(output.entries().map(([p, n]) => [dir + p, n] as const));
}

function add(output: Tree, files: ReadonlyArray<readonly [string, Uint8Array]>, o: HostOptions, origin: string): Tree {
  return output.withFiles(
    files.map(([p, b]) => [p, makeFile(b, `${o.label}:${o.span.line} ${origin}`)] as const),
    'error',
    o.span,
  );
}

// ------------------------------------------------------------------ Cloudflare Pages

const CF_STATIC_MAX = 2000;
const CF_DYNAMIC_MAX = 100;
const CF_HEADER_RULES_MAX = 100;

function cloudflare(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx): HostResult {
  const redirects: HostRedirect[] = [];
  let statics = 0;
  let dynamics = 0;
  for (const r of rs) {
    if (!c.unshadowed(output, r, 'Cloudflare Pages')) continue;
    const dynamic = shapeOf(r.from).kind !== 'exact';
    if (dynamic ? dynamics >= CF_DYNAMIC_MAX : statics >= CF_STATIC_MAX) {
      c.refuse(r, dynamic ? `Cloudflare Pages takes at most ${CF_DYNAMIC_MAX} dynamic redirects` : `Cloudflare Pages takes at most ${CF_STATIC_MAX} static redirects`);
      continue;
    }
    if (dynamic) dynamics++;
    else statics++;
    redirects.push(r);
  }
  const headers: HostHeader[] = [];
  const patterns: string[] = [];
  for (const h of hs) {
    if (!patterns.includes(h.pattern)) {
      if (patterns.length >= CF_HEADER_RULES_MAX) {
        c.refuseHeader(h, `Cloudflare Pages takes at most ${CF_HEADER_RULES_MAX} header rules`);
        continue;
      }
      patterns.push(h.pattern);
    }
    headers.push(h);
  }
  return { redirects, headers, output };
}

// ------------------------------------------------------------------ GitHub Pages

function githubPages(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx, o: HostOptions): HostResult {
  for (const r of rs) c.refuse(r, 'GitHub Pages has no redirects');
  for (const h of hs) c.refuseHeader(h, 'GitHub Pages cannot set response headers');
  const files: Array<[string, Uint8Array]> = [['/.nojekyll', new Uint8Array()]];
  if (o.base !== undefined) {
    if (!/^https?:\/\//i.test(o.base)) throw new SitefileError('E_VALIDATION', `EXPORT … FOR github-pages: BASE must be an http(s) URL, got '${o.base}'`, o.span);
    let u: URL;
    try {
      u = new URL(o.base);
    } catch {
      throw new SitefileError('E_VALIDATION', `EXPORT … FOR github-pages: BASE '${o.base}' is not a URL`, o.span);
    }
    const host = u.hostname.toLowerCase();
    if (!host.endsWith('.github.io')) {
      if (u.pathname !== '/' || u.search !== '' || u.hash !== '') {
        throw new SitefileError('E_VALIDATION', `EXPORT … FOR github-pages: a custom domain serves the site at its root, so BASE '${o.base}' cannot have a path`, o.span);
      }
      files.push(['/CNAME', new TextEncoder().encode(`${host}\n`)]);
    }
  }
  return { redirects: [], headers: [], output: add(output, files, o, 'EXPORT FOR github-pages') };
}

// ------------------------------------------------------------------ Vercel (Build Output API v3)

const reEscape = (s: string): string => s.replace(/[.+?^$()[\]{}|\\]/g, '\\$&');

/** A `_headers` pattern as a Vercel `src`: `*` is any run of characters, `:name` one segment. */
function headerRegex(pattern: string): string {
  return `^${pattern
    .split('/')
    .map((seg) => (PLACEHOLDER_SEG.test(seg) ? '[^/]+' : seg.split('*').map(reEscape).join('.*')))
    .join('/')}$`;
}

/** The names a target uses (`:splat`, `:name` at the start of a path segment), with `$name` in their place. */
function targetWith(to: string, sigil: (name: string) => string): { to: string; names: string[] } {
  const m = /^(https?:\/\/[^/]*)(.*)$/i.exec(to);
  const [origin, path] = m ? [m[1] as string, m[2] as string] : ['', to];
  const names: string[] = [];
  const out = path.replace(/(^|\/):([A-Za-z_][A-Za-z0-9_]*)/g, (_all, pre: string, name: string) => {
    names.push(name);
    return pre + sigil(name);
  });
  return { to: origin + out, names };
}

function vercel(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx, o: HostOptions): HostResult {
  const routes: unknown[] = [];
  for (const [pattern, list] of blocks(hs)) {
    const headers: Record<string, string> = {};
    for (const h of list) headers[h.name] = headers[h.name] === undefined ? h.value : `${headers[h.name]}, ${h.value}`;
    routes.push({ src: headerRegex(pattern), headers, continue: true });
  }
  routes.push({ handle: 'filesystem' });
  for (const r of rs) {
    const s = shapeOf(r.from);
    let src: string;
    const captures: string[] = [];
    if (s.kind === 'exact') {
      const p = s.path.length > 1 && s.path.endsWith('/') ? s.path.slice(0, -1) : s.path;
      src = p === '/' ? '^/$' : `^${reEscape(p)}/?$`;
    } else if (s.kind === 'splat') {
      src = `^${reEscape(s.prefix)}/(?<splat>.*)$`;
      captures.push('splat');
    } else if (s.kind === 'placeholders') {
      const segs = s.segments.map((seg) => {
        if (!PLACEHOLDER_SEG.test(seg)) return reEscape(seg);
        captures.push(seg.slice(1));
        return `(?<${seg.slice(1)}>[^/]+)`;
      });
      src = `^/${segs.join('/')}${s.splat ? '/(?<splat>.*)' : ''}$`;
      if (s.splat) captures.push('splat');
    } else {
      c.refuse(r, '* is supported only as a trailing /*');
      continue;
    }
    const t = targetWith(r.to, (n) => `$${n}`);
    const unknown = t.names.find((n) => !captures.includes(n));
    if (unknown !== undefined) {
      c.refuse(r, `:${unknown} in the target is not in the source`);
      continue;
    }
    routes.push({ src, headers: { Location: t.to }, status: r.status });
  }
  if (output.isFile('/404.html')) routes.push({ handle: 'error' }, { status: 404, src: '^/.*$', dest: '/404.html' });
  const out = add(moved(output, '/.vercel/output/static'), [['/.vercel/output/config.json', json({ version: 3, routes })]], o, 'EXPORT FOR vercel');
  return { redirects: [], headers: [], output: out };
}

// ------------------------------------------------------------------ Firebase Hosting

function firebase(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx, o: HostOptions): HostResult {
  const redirects: unknown[] = [];
  for (const r of rs) {
    if (r.status !== 301 && r.status !== 302) {
      c.refuse(r, 'Firebase Hosting redirects are 301 or 302');
      continue;
    }
    const s = shapeOf(r.from);
    let source: string;
    if (s.kind === 'exact') source = s.path;
    else if (s.kind === 'splat') source = `${s.prefix}/:splat*`;
    else if (s.kind === 'placeholders') source = `/${s.segments.join('/')}${s.splat ? '/:splat*' : ''}`;
    else {
      c.refuse(r, '* is supported only as a trailing /*');
      continue;
    }
    if (!c.unshadowed(output, r, 'Firebase Hosting')) continue;
    redirects.push({ source, destination: r.to, type: r.status });
  }
  const headers: unknown[] = [];
  for (const [pattern, list] of blocks(hs)) {
    if (pattern.split('/').some((seg) => seg.startsWith(':'))) {
      for (const h of list) c.refuseHeader(h, 'Firebase Hosting header sources are globs, without :placeholders');
      continue;
    }
    headers.push({ source: pattern.replace(/\*+/g, '**'), headers: list.map((h) => ({ key: h.name, value: h.value })) });
  }
  const hosting: Record<string, unknown> = { public: 'public' };
  if (redirects.length) hosting['redirects'] = redirects;
  if (headers.length) hosting['headers'] = headers;
  const out = add(moved(output, '/public'), [['/firebase.json', json({ hosting })]], o, 'EXPORT FOR firebase');
  return { redirects: [], headers: [], output: out };
}

// ------------------------------------------------------------------ Azure Static Web Apps

/** A route as (prefix, below): an exact path, or everything below `prefix/` for `prefix/*`. */
function routeOf(p: string): { prefix: string; below: boolean } | undefined {
  const s = shapeOf(p);
  if (s.kind === 'exact') return { prefix: s.path, below: false };
  if (s.kind === 'splat') return { prefix: s.prefix, below: true };
  return undefined;
}

function overlap(a: { prefix: string; below: boolean }, b: { prefix: string; below: boolean }): boolean {
  const under = (x: string, dir: string) => (dir === '' ? true : x.startsWith(`${dir}/`));
  if (!a.below && !b.below) return a.prefix === b.prefix;
  if (a.below && b.below) return a.prefix === b.prefix || under(a.prefix, b.prefix) || under(b.prefix, a.prefix);
  const [dir, path] = a.below ? [a.prefix, b.prefix] : [b.prefix, a.prefix];
  return under(path, dir);
}

function azure(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx, o: HostOptions): HostResult {
  const routes: Array<Record<string, unknown>> = [];
  const taken: Array<{ prefix: string; below: boolean; what: string }> = [];
  for (const r of rs) {
    if (r.status !== 301 && r.status !== 302) {
      c.refuse(r, 'Azure Static Web Apps redirects are 301 or 302');
      continue;
    }
    const route = routeOf(r.from);
    if (!route) {
      c.refuse(r, shapeOf(r.from).kind === 'placeholders' ? 'Azure Static Web Apps routes have no :placeholders' : '* is supported only as a trailing /*');
      continue;
    }
    if (targetWith(r.to, (n) => n).names.length > 0) {
      c.refuse(r, 'Azure Static Web Apps redirects cannot use :splat or :placeholders in the target');
      continue;
    }
    if (!c.unshadowed(output, r, 'Azure Static Web Apps')) continue;
    routes.push({ route: r.from, redirect: r.to, statusCode: r.status });
    taken.push({ ...route, what: `REDIRECT ${r.from}` });
  }
  const globalHeaders: Record<string, string> = {};
  const routeHeaders: Array<{ pattern: string; route: { prefix: string; below: boolean }; headers: Record<string, string> }> = [];
  const globalBlock = blocks(hs).find(([p]) => p === '/*');
  for (const h of globalBlock?.[1] ?? []) globalHeaders[h.name] = globalHeaders[h.name] === undefined ? h.value : `${globalHeaders[h.name]}, ${h.value}`;
  for (const [pattern, list] of blocks(hs)) {
    if (pattern === '/*') continue;
    const route = routeOf(pattern);
    if (!route) {
      for (const h of list) c.refuseHeader(h, 'Azure Static Web Apps routes are exact paths or end in /*');
      continue;
    }
    const clash = taken.find((t) => overlap(t, route));
    if (clash) {
      for (const h of list) c.refuseHeader(h, `on Azure Static Web Apps only the first matching route applies, and ${clash.what} matches the same paths`);
      continue;
    }
    const headers: Record<string, string> = {};
    for (const h of list) {
      if (Object.keys(globalHeaders).some((g) => g.toLowerCase() === h.name.toLowerCase())) {
        c.refuseHeader(h, `on Azure Static Web Apps a route's ${h.name} replaces the one for /* instead of adding to it`);
        continue;
      }
      headers[h.name] = headers[h.name] === undefined ? h.value : `${headers[h.name]}, ${h.value}`;
    }
    if (Object.keys(headers).length === 0) continue;
    routeHeaders.push({ pattern, route, headers });
    taken.push({ ...route, what: `HEADER ${pattern}` });
  }
  for (const rh of routeHeaders) routes.push({ route: rh.pattern, headers: rh.headers });
  const config: Record<string, unknown> = {};
  if (routes.length) config['routes'] = routes;
  if (Object.keys(globalHeaders).length) config['globalHeaders'] = globalHeaders;
  if (output.isFile('/404.html')) config['responseOverrides'] = { '404': { rewrite: '/404.html' } };
  const out = add(output, [['/staticwebapp.config.json', json(config)]], o, 'EXPORT FOR azure');
  return { redirects: [], headers: [], output: out };
}

// ------------------------------------------------------------------ S3 website hosting

/** `_headers` names S3 stores as object metadata, and the PutObject field each becomes. */
const S3_FIELDS: Record<string, string> = {
  'cache-control': 'CacheControl',
  'content-type': 'ContentType',
  'content-disposition': 'ContentDisposition',
  'content-encoding': 'ContentEncoding',
  'content-language': 'ContentLanguage',
  expires: 'Expires',
};

const utf16Compare = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

function s3(output: Tree, rs: readonly HostRedirect[], hs: readonly HostHeader[], c: Ctx, o: HostOptions): HostResult {
  const objects = new Map<string, Record<string, string>>();
  const object = (key: string): Record<string, string> => {
    let v = objects.get(key);
    if (!v) objects.set(key, (v = { Key: key }));
    return v;
  };
  const rules: unknown[] = [];
  for (const r of rs) {
    const s = shapeOf(r.from);
    if (s.kind === 'exact') {
      if (r.status !== 301) {
        c.refuse(r, 'S3 object redirects are always 301');
        continue;
      }
      if (s.path === '/') {
        c.refuse(r, 'S3 cannot redirect the root object');
        continue;
      }
      const key = s.path.endsWith('/') ? `${s.path.slice(1)}index.html` : s.path.slice(1);
      if (output.isFile(`/${key}`)) {
        c.refuse(r, `/${key} is a file in the export, and an S3 object is either a file or a redirect`);
        continue;
      }
      object(key)['WebsiteRedirectLocation'] = r.to;
      continue;
    }
    if (s.kind !== 'splat' || s.prefix === '') {
      c.refuse(r, s.kind === 'placeholders' ? 'S3 routing rules have no :placeholders' : s.kind === 'splat' ? 'S3 cannot redirect every key' : '* is supported only as a trailing /*');
      continue;
    }
    const t = targetWith(r.to, (n) => `:${n}`);
    const splatAtEnd = t.names.length === 1 && t.names[0] === 'splat' && t.to.endsWith(':splat');
    if (t.names.length > 0 && !splatAtEnd) {
      c.refuse(r, 'S3 routing rules can only keep the rest of the path at the end of the target (…/:splat)');
      continue;
    }
    if (!c.unshadowed(output, r, 'S3')) continue;
    const m = /^(https?):\/\/([^/]+)(.*)$/i.exec(r.to);
    const path = (m ? (m[3] as string) : r.to).replace(/:splat$/, '').replace(/^\//, '');
    const redirect: Record<string, string> = {};
    if (m) {
      redirect['Protocol'] = (m[1] as string).toLowerCase();
      redirect['HostName'] = m[2] as string;
    }
    if (splatAtEnd) redirect['ReplaceKeyPrefixWith'] = path;
    else if (path !== '') redirect['ReplaceKeyWith'] = path;
    redirect['HttpRedirectCode'] = String(r.status);
    rules.push({ Condition: { KeyPrefixEquals: `${s.prefix.slice(1)}/` }, Redirect: redirect });
  }
  const valid: HostHeader[] = [];
  for (const h of hs) {
    if (S3_FIELDS[h.name.toLowerCase()] === undefined) c.refuseHeader(h, 'S3 serves only Cache-Control, Content-Type, Content-Disposition, Content-Encoding, Content-Language and Expires from object metadata');
    else if (h.pattern.split('/').some((seg) => seg.startsWith(':'))) c.refuseHeader(h, 'S3 object metadata is set per file, so the pattern cannot have :placeholders');
    else valid.push(h);
  }
  for (const [f] of output.entries()) {
    const urls = f.endsWith('/index.html') ? [f, f.slice(0, -'index.html'.length)] : [f];
    for (const h of valid) {
      if (!urls.some((u) => globMatch(h.pattern, u))) continue;
      const rec = object(f.slice(1));
      const field = S3_FIELDS[h.name.toLowerCase()] as string;
      rec[field] = rec[field] === undefined ? h.value : `${rec[field]}, ${h.value}`;
    }
  }
  const website: Record<string, unknown> = { IndexDocument: { Suffix: 'index.html' } };
  if (output.isFile('/404.html')) website['ErrorDocument'] = { Key: '404.html' };
  if (rules.length) website['RoutingRules'] = rules;
  const list = [...objects.values()].sort((a, b) => utf16Compare(a['Key'] as string, b['Key'] as string));
  const out = add(
    output,
    [
      ['/_s3/website.json', json(website)],
      ['/_s3/objects.json', json(list)],
    ],
    o,
    'EXPORT FOR s3',
  );
  return { redirects: [], headers: [], output: out };
}

// ------------------------------------------------------------------ entry point

/** Apply `EXPORT … FOR <host>` (not `netlify`, which is `_redirects` and `_headers` as they are). */
export function exportForHost(host: Exclude<ExportHost, 'netlify'>, output: Tree, redirects: readonly HostRedirect[], headers: readonly HostHeader[], o: HostOptions): HostResult {
  const c = new Ctx(host, o);
  switch (host) {
    case 'cloudflare':
      return cloudflare(output, redirects, headers, c);
    case 'github-pages':
      return githubPages(output, redirects, headers, c, o);
    case 'vercel':
      return vercel(output, redirects, headers, c, o);
    case 'firebase':
      return firebase(output, redirects, headers, c, o);
    case 'azure':
      return azure(output, redirects, headers, c, o);
    case 's3':
      return s3(output, redirects, headers, c, o);
  }
}
