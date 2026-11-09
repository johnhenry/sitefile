import path from 'node:path';
import { SitefileError } from './errors.js';
import { parseHeaderRules, parseRedirectRules } from './serve-rules.js';
import { parseToml } from './toml.js';

/**
 * `sitefile import-config` (issue #204): translate a host's config file into `_redirects` and `_headers`
 * that `sitefile serve` (and REDIRECT/HEADER) understand today. Ported as rust/src/host_config.rs.
 *
 * Every rule that is written is one `serve` would apply; anything else (placeholders, conditions, proxying,
 * regular expressions, settings with no `_redirects` form) is listed as unsupported with the reason, never
 * written half-translated. See docs/CLI.md, "Host config files", for the mapping table.
 */
export const HOST_FORMATS = ['netlify', 'vercel', 'firebase', 'azure', 'wrangler', 'htaccess'] as const;
export type HostFormat = (typeof HOST_FORMATS)[number];

export interface HeaderBlock {
  readonly pattern: string;
  readonly headers: ReadonlyArray<readonly [string, string]>;
}

export interface Unsupported {
  /** Where in the file: `redirects[2]`, `hosting.cleanUrls`, `line 7`. */
  readonly entry: string;
  readonly reason: string;
}

export interface ImportedConfig {
  readonly format: HostFormat;
  /** `_redirects` lines (`from to status[!]`). */
  readonly redirects: readonly string[];
  readonly headers: readonly HeaderBlock[];
  readonly unsupported: readonly Unsupported[];
}

/** The format a file name implies, if any. */
export function formatOf(file: string): HostFormat | undefined {
  const base = path.basename(file).toLowerCase();
  if (base === 'netlify.toml') return 'netlify';
  if (base === 'vercel.json') return 'vercel';
  if (base === 'firebase.json') return 'firebase';
  if (base === 'staticwebapp.config.json') return 'azure';
  if (base === 'wrangler.toml' || base === 'wrangler.json') return 'wrangler';
  if (base === '.htaccess') return 'htaccess';
  return undefined;
}

const PLACEHOLDERS = ':placeholders are not supported yet (johnhenry/sitefile#193)';
const MIDDLE_STAR = '* is supported only as a trailing /*';
const PROXY = 'proxying to another host is not supported (johnhenry/sitefile#172)';
const CONDITIONS = 'conditions are not supported';

type Json = unknown;
const isMapping = (v: Json): v is Record<string, Json> => v !== null && typeof v === 'object' && !Array.isArray(v);
const isStr = (v: Json): v is string => typeof v === 'string';
const isInt = (v: Json): v is number => typeof v === 'number' && Number.isSafeInteger(v);

class Out {
  readonly redirects: string[] = [];
  /** Catch-all rules (404 pages, SPA fallbacks): written after every other rule, which they would shadow. */
  readonly fallbacks: string[] = [];
  readonly headers: HeaderBlock[] = [];
  readonly unsupported: Unsupported[] = [];

  skip(entry: string, reason: string): void {
    this.unsupported.push({ entry, reason });
  }

  /** Add `from to status[!]` when `serve` would apply it as written; otherwise list why not. */
  redirect(entry: string, from: string, to: string, status: number, force = false, fallback = false): boolean {
    const line = `${from} ${to} ${status}${force ? '!' : ''}`;
    if (parseRedirectRules(line).length === 1 && !hasPlaceholder(to)) {
      (fallback ? this.fallbacks : this.redirects).push(line);
      return true;
    }
    this.skip(entry, redirectReason(from, to, status));
    return false;
  }

  /** Add a `_headers` block when every header is one `serve` would set; otherwise list why not. */
  header(entry: string, pattern: string, headers: ReadonlyArray<readonly [string, string]>): void {
    if (headers.length === 0) return;
    const text = `${pattern}\n${headers.map(([n, v]) => `  ${n}: ${v}`).join('\n')}\n`;
    const parsed = parseHeaderRules(text);
    if (parsed.length !== 1 || parsed[0]?.pattern !== pattern || parsed[0].headers.length !== headers.length || /[ \t\n\r\f\v]/.test(pattern)) {
      this.skip(entry, headerReason(pattern, headers));
      return;
    }
    const last = this.headers[this.headers.length - 1];
    if (last && last.pattern === pattern) this.headers[this.headers.length - 1] = { pattern, headers: [...last.headers, ...headers] };
    else this.headers.push({ pattern, headers });
  }
}

/** A `:name` placeholder other than `:splat` in a target. */
function hasPlaceholder(to: string): boolean {
  return /(^|\/):(?!splat(\/|$|[?#]))[A-Za-z_]/.test(to.replace(/^https?:\/\/[^/]*/i, ''));
}

function redirectReason(from: string, to: string, status: number): string {
  if (status === 410) return 'status 410 is not supported yet (johnhenry/sitefile#193)';
  if (![200, 301, 302, 303, 307, 308, 404].includes(status)) return `status ${status} is not supported`;
  if (from.split('/').some((s) => s.startsWith(':')) || hasPlaceholder(to)) return PLACEHOLDERS;
  if (from.slice(0, from.endsWith('/*') ? -2 : undefined).includes('*')) return MIDDLE_STAR;
  if (!from.startsWith('/')) return `'${from}' is not a path`;
  if ((status === 200 || status === 404) && /^https?:/i.test(to)) return PROXY;
  return `sitefile serve cannot express '${from} ${to} ${status}'`;
}

function headerReason(pattern: string, headers: ReadonlyArray<readonly [string, string]>): string {
  if (!pattern.startsWith('/') || /[ \t\n\r\f\v]/.test(pattern)) return `'${pattern}' is not a path pattern`;
  for (const [n, v] of headers) {
    if (!/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(n)) return `'${n}' is not a header name`;
    if (['content-length', 'transfer-encoding', 'connection', 'keep-alive', 'etag', 'location'].includes(n.toLowerCase())) return `${n} cannot be set by _headers`;
    if (!/^[\t\x20-\x7e]*$/.test(v)) return `the value of ${n} has characters _headers cannot carry`;
  }
  return 'sitefile serve cannot express these headers';
}

/** The keys of `o` in JavaScript's order: integer-like keys ascending first, then the rest as written (Rust sorts them the same way). */
function keysOf(o: Record<string, Json>): string[] {
  return Object.keys(o);
}

// ------------------------------------------------------------------ netlify.toml

function netlify(doc: Record<string, Json>, out: Out): void {
  const redirects = doc['redirects'];
  if (redirects !== undefined) {
    if (!Array.isArray(redirects)) out.skip('redirects', 'expected [[redirects]] tables');
    else
      redirects.forEach((r, i) => {
        const entry = `redirects[${i}]`;
        if (!isMapping(r) || !isStr(r['from']) || !isStr(r['to'])) return out.skip(entry, 'expected from and to');
        for (const k of ['query', 'conditions', 'headers', 'signed']) if (r[k] !== undefined) return out.skip(entry, k === 'query' ? 'query matching is not supported' : CONDITIONS);
        const status = r['status'] ?? 301;
        if (!isInt(status)) return out.skip(entry, 'status must be a number');
        if (r['force'] !== undefined && typeof r['force'] !== 'boolean') return out.skip(entry, 'force must be true or false');
        out.redirect(entry, r['from'], r['to'], status, r['force'] === true);
      });
  }
  const headers = doc['headers'];
  if (headers !== undefined) {
    if (!Array.isArray(headers)) out.skip('headers', 'expected [[headers]] tables');
    else
      headers.forEach((h, i) => {
        const entry = `headers[${i}]`;
        if (!isMapping(h) || !isStr(h['for']) || !isMapping(h['values'])) return out.skip(entry, 'expected for and values');
        const values = h['values'];
        const list: Array<[string, string]> = [];
        for (const k of keysOf(values)) {
          const v = values[k];
          if (isStr(v)) list.push([k, v]);
          else if (Array.isArray(v) && v.every(isStr)) list.push([k, v.join(', ')]);
          else return out.skip(entry, `the value of ${k} must be a string`);
        }
        out.header(entry, h['for'], list);
      });
  }
  for (const k of ['edge_functions', 'functions', 'plugins']) if (doc[k] !== undefined) out.skip(k, 'not part of serving files');
}

// ------------------------------------------------------------------ path-to-regexp style sources (Vercel)

/**
 * A Vercel `source` as a `_redirects` path: exact, or a last segment `:name*` / `(.*)` (anything below),
 * which becomes `/*`. `params` are the names a destination may use for the splat.
 */
function vercelSource(source: string): { from: string; params: string[] } | undefined {
  if (/^\/[^:*()?+[\]{}\\]*$/.test(source)) return { from: source, params: [] };
  const m = /^(\/(?:[^:*()?+[\]{}\\/]+\/)*)(?::([A-Za-z_][A-Za-z0-9_]*)\*|\(\.\*\))$/.exec(source);
  if (!m) return undefined;
  return { from: `${m[1]}*`, params: m[2] !== undefined ? [`:${m[2]}*`, `:${m[2]}`] : ['$1'] };
}

function vercelTarget(dest: string, params: readonly string[]): string {
  let out = dest;
  for (const p of params) out = out.split(p).join(':splat');
  return out;
}

function vercelSourceReason(source: string): string {
  return /[()]/.test(source) ? 'regular expressions are not supported' : source.includes(':') ? PLACEHOLDERS : MIDDLE_STAR;
}

function vercel(doc: Record<string, Json>, out: Out): void {
  const list = (key: string, fn: (r: Record<string, Json>, entry: string) => void): void => {
    const v = doc[key];
    if (v === undefined) return;
    if (!Array.isArray(v)) return out.skip(key, 'expected a list');
    v.forEach((r, i) => {
      const entry = `${key}[${i}]`;
      if (!isMapping(r)) return out.skip(entry, 'expected an object');
      if (r['has'] !== undefined || r['missing'] !== undefined) return out.skip(entry, CONDITIONS);
      fn(r, entry);
    });
  };
  list('redirects', (r, entry) => {
    if (!isStr(r['source']) || !isStr(r['destination'])) return out.skip(entry, 'expected source and destination');
    const src = vercelSource(r['source']);
    if (!src) return out.skip(entry, vercelSourceReason(r['source']));
    const status = r['statusCode'] ?? (r['permanent'] === false ? 307 : 308);
    if (!isInt(status)) return out.skip(entry, 'statusCode must be a number');
    out.redirect(entry, src.from, vercelTarget(r['destination'], src.params), status);
  });
  list('rewrites', (r, entry) => {
    if (!isStr(r['source']) || !isStr(r['destination'])) return out.skip(entry, 'expected source and destination');
    if (/^https?:/i.test(r['destination'])) return out.skip(entry, PROXY);
    const src = vercelSource(r['source']);
    if (!src) return out.skip(entry, vercelSourceReason(r['source']));
    out.redirect(entry, src.from, vercelTarget(r['destination'], src.params), 200);
  });
  list('headers', (r, entry) => {
    if (!isStr(r['source']) || !Array.isArray(r['headers'])) return out.skip(entry, 'expected source and headers');
    const src = vercelSource(r['source']);
    if (!src) return out.skip(entry, vercelSourceReason(r['source']));
    const hs: Array<[string, string]> = [];
    for (const h of r['headers']) {
      if (!isMapping(h) || !isStr(h['key']) || !isStr(h['value'])) return out.skip(entry, 'expected headers of key and value');
      hs.push([h['key'], h['value']]);
    }
    out.header(entry, src.from, hs);
  });
  if (doc['cleanUrls'] !== undefined) out.skip('cleanUrls', 'serve always answers /about with about.html; canonical redirects are johnhenry/sitefile#192');
  if (doc['trailingSlash'] !== undefined) out.skip('trailingSlash', 'trailing-slash modes are johnhenry/sitefile#192');
}

// ------------------------------------------------------------------ firebase.json

/** A Firebase glob as a `_redirects` path: exact, or a trailing `/**` (anything below), which becomes `/*`. */
function firebaseSource(glob: string): string | undefined {
  if (/^\/[^*?{}[\]!:]*$/.test(glob)) return glob;
  const m = /^((?:\/[^*?{}[\]!:/]+)*)\/\*\*$/.exec(glob);
  return m ? `${m[1]}/*` : glob === '**' ? '/*' : undefined;
}

function firebaseReason(r: Record<string, Json>): string {
  if (r['regex'] !== undefined) return 'regular expressions are not supported';
  const s = String(r['source'] ?? r['glob'] ?? '');
  return s.includes(':') ? PLACEHOLDERS : 'only exact paths and a trailing /** are supported';
}

function firebase(doc: Record<string, Json>, out: Out): void {
  let hosting = doc['hosting'];
  let at = 'hosting';
  if (Array.isArray(hosting)) {
    if (hosting.length !== 1) return out.skip('hosting', 'several sites: import one site at a time');
    hosting = hosting[0];
    at = 'hosting[0]';
  }
  if (!isMapping(hosting)) {
    if (hosting !== undefined) out.skip('hosting', 'expected an object');
    return;
  }
  const h = hosting;
  const list = (key: string, fn: (r: Record<string, Json>, entry: string) => void): void => {
    const v = h[key];
    if (v === undefined) return;
    if (!Array.isArray(v)) return out.skip(`${at}.${key}`, 'expected a list');
    v.forEach((r, i) => {
      const entry = `${at}.${key}[${i}]`;
      if (!isMapping(r)) return out.skip(entry, 'expected an object');
      fn(r, entry);
    });
  };
  const sourceOf = (r: Record<string, Json>): string | undefined => {
    const s = r['source'] ?? r['glob'];
    return r['regex'] === undefined && isStr(s) ? firebaseSource(s) : undefined;
  };
  list('redirects', (r, entry) => {
    const from = sourceOf(r);
    if (from === undefined) return out.skip(entry, firebaseReason(r));
    if (!isStr(r['destination'])) return out.skip(entry, 'expected a destination');
    const status = r['type'] ?? 301;
    if (!isInt(status)) return out.skip(entry, 'type must be 301 or 302');
    out.redirect(entry, from, r['destination'], status);
  });
  list('rewrites', (r, entry) => {
    for (const k of ['function', 'run', 'dynamicLinks']) if (r[k] !== undefined) return out.skip(entry, `${k} rewrites are not supported`);
    const from = sourceOf(r);
    if (from === undefined) return out.skip(entry, firebaseReason(r));
    if (!isStr(r['destination'])) return out.skip(entry, 'expected a destination');
    if (/^https?:/i.test(r['destination'])) return out.skip(entry, PROXY);
    out.redirect(entry, from, r['destination'], 200);
  });
  list('headers', (r, entry) => {
    const from = sourceOf(r);
    if (from === undefined) return out.skip(entry, firebaseReason(r));
    if (!Array.isArray(r['headers'])) return out.skip(entry, 'expected headers');
    const hs: Array<[string, string]> = [];
    for (const x of r['headers']) {
      if (!isMapping(x) || !isStr(x['key']) || !isStr(x['value'])) return out.skip(entry, 'expected headers of key and value');
      hs.push([x['key'], x['value']]);
    }
    out.header(entry, from, hs);
  });
  if (h['cleanUrls'] !== undefined) out.skip(`${at}.cleanUrls`, 'serve always answers /about with about.html; canonical redirects are johnhenry/sitefile#192');
  if (h['trailingSlash'] !== undefined) out.skip(`${at}.trailingSlash`, 'trailing-slash modes are johnhenry/sitefile#192');
  if (h['i18n'] !== undefined) out.skip(`${at}.i18n`, 'language and country rewrites are not supported');
}

// ------------------------------------------------------------------ staticwebapp.config.json (Azure)

function azureRoute(route: string): string | undefined {
  return /^\/[^*?{}[\]:]*(\/\*)?$/.test(route) || route === '/*' ? route : undefined;
}

function azure(doc: Record<string, Json>, out: Out): void {
  const routes = doc['routes'];
  if (routes !== undefined) {
    if (!Array.isArray(routes)) out.skip('routes', 'expected a list');
    else
      routes.forEach((r, i) => {
        const entry = `routes[${i}]`;
        if (!isMapping(r) || !isStr(r['route'])) return out.skip(entry, 'expected a route');
        if (r['allowedRoles'] !== undefined || r['methods'] !== undefined) return out.skip(entry, 'allowedRoles and methods are not supported');
        const from = azureRoute(r['route']);
        if (from === undefined) return out.skip(entry, 'only exact routes and a trailing /* are supported');
        if (r['headers'] !== undefined) {
          const hv = r['headers'];
          if (!isMapping(hv) || !Object.values(hv).every(isStr)) out.skip(entry, 'headers must map names to strings');
          else out.header(entry, from, keysOf(hv).map((k) => [k, hv[k] as string]));
        }
        if (isStr(r['redirect'])) {
          const status = r['statusCode'] ?? 302;
          if (!isInt(status)) return out.skip(entry, 'statusCode must be a number');
          out.redirect(entry, from, r['redirect'], status);
        } else if (isStr(r['rewrite'])) {
          if (r['statusCode'] !== undefined && r['statusCode'] !== 200) return out.skip(entry, 'a rewrite with a statusCode is not supported');
          out.redirect(entry, from, r['rewrite'], 200);
        } else if (r['statusCode'] !== undefined) out.skip(entry, 'a statusCode without a redirect or rewrite is not supported');
      });
  }
  const nav = doc['navigationFallback'];
  if (nav !== undefined) {
    if (!isMapping(nav) || !isStr(nav['rewrite'])) out.skip('navigationFallback', 'expected a rewrite');
    else {
      out.redirect('navigationFallback', '/*', nav['rewrite'], 200, false, true);
      if (nav['exclude'] !== undefined) out.skip('navigationFallback.exclude', 'not translated: the fallback answers only paths with no file, so excluded paths that are missing get it too');
    }
  }
  const overrides = doc['responseOverrides'];
  if (overrides !== undefined) {
    if (!isMapping(overrides)) out.skip('responseOverrides', 'expected an object');
    else
      for (const code of keysOf(overrides)) {
        const entry = `responseOverrides.${code}`;
        const o = overrides[code];
        if (code !== '404') {
          out.skip(entry, `only 404 can be overridden`);
          continue;
        }
        if (!isMapping(o) || !isStr(o['rewrite']) || o['redirect'] !== undefined) {
          out.skip(entry, 'only a rewrite is supported');
          continue;
        }
        out.redirect(entry, '/*', o['rewrite'], 404, false, true);
      }
  }
  const g = doc['globalHeaders'];
  if (g !== undefined) {
    if (!isMapping(g) || !Object.values(g).every(isStr)) out.skip('globalHeaders', 'globalHeaders must map names to strings');
    else out.header('globalHeaders', '/*', keysOf(g).map((k) => [k, g[k] as string]));
  }
  if (doc['mimeTypes'] !== undefined) out.skip('mimeTypes', 'custom content types are johnhenry/sitefile#220');
}

// ------------------------------------------------------------------ wrangler [assets]

function wrangler(doc: Record<string, Json>, out: Out): void {
  const a = doc['assets'];
  if (a === undefined) return;
  if (!isMapping(a)) return out.skip('assets', 'expected a table');
  const nf = a['not_found_handling'];
  if (nf === 'single-page-application') out.redirect('assets.not_found_handling', '/*', '/index.html', 200, false, true);
  else if (nf !== undefined && nf !== '404-page' && nf !== 'none') out.skip('assets.not_found_handling', isStr(nf) ? `unknown value '${nf}'` : 'expected a string');
  const hh = a['html_handling'];
  if (hh !== undefined && hh !== 'auto-trailing-slash') out.skip('assets.html_handling', 'trailing-slash modes are johnhenry/sitefile#192');
  if (a['run_worker_first'] !== undefined) out.skip('assets.run_worker_first', 'Workers are not run');
}

// ------------------------------------------------------------------ .htaccess

/** Split a directive line into words, honouring "double quotes" (a backslash escapes the next character). */
function words(line: string): string[] | undefined {
  const out: string[] = [];
  let i = 0;
  for (;;) {
    while (line[i] === ' ' || line[i] === '\t') i++;
    if (i >= line.length) return out;
    if (line[i] === '"') {
      let w = '';
      i++;
      for (;;) {
        if (i >= line.length) return undefined;
        if (line[i] === '\\' && i + 1 < line.length) {
          w += line[i + 1];
          i += 2;
        } else if (line[i] === '"') {
          i++;
          break;
        } else w += line[i++];
      }
      out.push(w);
    } else {
      const start = i;
      while (i < line.length && line[i] !== ' ' && line[i] !== '\t') i++;
      out.push(line.slice(start, i));
    }
  }
}

const APACHE_STATUS: Record<string, number> = Object.assign(Object.create(null) as Record<string, number>, { permanent: 301, temp: 302, seeother: 303 });

function htaccess(text: string, out: Out): void {
  const containers: string[] = [];
  text.split('\n').forEach((raw, n) => {
    const entry = `line ${n + 1}`;
    const t = (raw.endsWith('\r') ? raw.slice(0, -1) : raw).trim();
    if (t === '' || t.startsWith('#')) return;
    const close = /^<\/([A-Za-z]+)[ \t]*>$/.exec(t);
    if (close) {
      containers.pop();
      return;
    }
    const open = /^<([A-Za-z]+)([ \t][^>]*)?>$/.exec(t);
    if (open) {
      const name = open[1] as string;
      containers.push(name.toLowerCase());
      if (name.toLowerCase() !== 'ifmodule') out.skip(entry, `<${name}> sections are not supported; their directives are skipped`);
      return;
    }
    if (containers.some((c) => c !== 'ifmodule')) return;
    const w = words(t);
    if (!w || w.length === 0) return out.skip(entry, 'unbalanced quotes');
    const d = (w[0] as string).toLowerCase();
    if (d === 'redirect' || d === 'redirectpermanent' || d === 'redirecttemp') {
      let args = w.slice(1);
      let status = d === 'redirectpermanent' ? 301 : 302;
      const first = (args[0] ?? '').toLowerCase();
      if (d === 'redirect' && (first in APACHE_STATUS || first === 'gone' || /^[0-9]{3}$/.test(first))) {
        if (first === 'gone' || first === '410') return out.skip(entry, 'status 410 is not supported yet (johnhenry/sitefile#193)');
        status = APACHE_STATUS[first] ?? (/^3[0-9]{2}$/.test(first) ? Number(first) : 0);
        if (status === 0) return out.skip(entry, `status ${first} is not supported`);
        args = args.slice(1);
      }
      if (args.length !== 2) return out.skip(entry, 'expected a path and a URL');
      const [from, to] = args as [string, string];
      // Apache matches the path and everything below it.
      if (out.redirect(entry, from, to, status) && from !== '/') out.redirect(entry, `${from.replace(/\/$/, '')}/*`, `${to.replace(/\/$/, '')}/:splat`, status);
      return;
    }
    if (d === 'redirectmatch') {
      let args = w.slice(1);
      let status = 302;
      const first = (args[0] ?? '').toLowerCase();
      if (first in APACHE_STATUS || first === 'gone' || /^[0-9]{3}$/.test(first)) {
        if (first === 'gone' || first === '410') return out.skip(entry, 'status 410 is not supported yet (johnhenry/sitefile#193)');
        status = APACHE_STATUS[first] ?? (/^3[0-9]{2}$/.test(first) ? Number(first) : 0);
        if (status === 0) return out.skip(entry, `status ${first} is not supported`);
        args = args.slice(1);
      }
      if (args.length !== 2) return out.skip(entry, 'expected a pattern and a URL');
      const [re, to] = args as [string, string];
      const exact = /^\^(\/[A-Za-z0-9._~\-/]*)\$$/.exec(re);
      if (exact) return void out.redirect(entry, exact[1] as string, to, status);
      const below = /^\^(\/(?:[A-Za-z0-9._~-]+\/)*)\(\.\*\)\$?$/.exec(re);
      if (below && !/\$(?!1)/.test(to)) {
        return void out.redirect(entry, `${below[1]}*`, to.split('$1').join(':splat'), status);
      }
      return out.skip(entry, 'only ^/path$ and ^/prefix/(.*)$ patterns are supported');
    }
    if (d === 'errordocument') {
      if (w.length === 3 && w[1] === '404' && (w[2] as string).startsWith('/')) return void out.redirect(entry, '/*', w[2] as string, 404, false, true);
      return out.skip(entry, w[1] === '404' ? 'only a local page is supported' : `ErrorDocument ${w[1]} is not supported`);
    }
    if (d === 'directoryindex') {
      if (w.length === 2 && w[1] === 'index.html') return;
      return out.skip(entry, 'serve always uses index.html');
    }
    if (d === 'header') {
      let args = w.slice(1);
      if ((args[0] ?? '').toLowerCase() === 'always') args = args.slice(1);
      if (args.length !== 3 || (args[0] as string).toLowerCase() !== 'set') return out.skip(entry, 'only Header set <name> <value> is supported');
      return void out.header(entry, '/*', [[args[1] as string, args[2] as string]]);
    }
    if (d === 'addtype') return out.skip(entry, 'custom content types are johnhenry/sitefile#220');
    out.skip(entry, `${w[0]} is not translated`);
  });
}

// ------------------------------------------------------------------ entry point

/** Translate the text of a host config file. `file` names it in messages. */
export function importHostConfig(text: string, file: string, format: HostFormat): ImportedConfig {
  const out = new Out();
  if (format === 'htaccess') htaccess(text, out);
  else {
    let doc: Json;
    if (format === 'netlify' || (format === 'wrangler' && !file.toLowerCase().endsWith('.json'))) doc = parseToml(text, file);
    else {
      try {
        doc = JSON.parse(text.replace(/^﻿/, '')) as Json;
      } catch (e) {
        throw new SitefileError('E_DATA', `${file} is not valid JSON: ${(e as Error).message}`, undefined, { cause: e });
      }
    }
    if (!isMapping(doc)) throw new SitefileError('E_DATA', `${file}: expected an object at the top level`);
    ({ netlify, vercel, firebase, azure, wrangler })[format](doc, out);
  }
  return { format, redirects: [...out.redirects, ...out.fallbacks], headers: out.headers, unsupported: out.unsupported };
}

/** `_redirects` text (empty when there are no rules). */
export function redirectsText(c: ImportedConfig): string {
  return c.redirects.map((l) => `${l}\n`).join('');
}

/** `_headers` text (empty when there are no blocks). */
export function headersText(c: ImportedConfig): string {
  return c.headers.map((b) => `${b.pattern}\n${b.headers.map(([n, v]) => `  ${n}: ${v}\n`).join('')}`).join('');
}
