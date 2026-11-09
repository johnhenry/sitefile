import { SitefileError } from './errors.js';
import { encodePath, resolveReference, scanCss, scanHtml, type Reference } from './links.js';
import type { Tree } from './tree.js';
import { mimeTypeFor, validateBaseUrl } from './wbn.js';

/**
 * Single-file page exports (issue #210): `EXPORT … AS html` inlines every asset a page references as a
 * `data:` URI; `AS mhtml` writes the page and those assets as one multipart/related (MHTML) file.
 *
 * The page is `/index.html` of the exported tree, or the file the EXPORT path names. Its references are
 * found with the FINGERPRINT scanner (src/links.ts): every URL in an attribute (`src`, `srcset`, `href`
 * of <link>, `poster`, …) and in its CSS (`<style>`, `style=""`) is an asset, except `href` of <a> and
 * <area> (navigation, kept as written), and references to other HTML files (pages, not assets). A stylesheet's own `url()` and `@import` references are inlined
 * into it first, recursively. External (http, https, `//`) and non-file URLs (`data:`, `mailto:`, `#x`)
 * are kept as written; a reference to a file that is not in the tree, or to a directory, fails with
 * E_EXPORT. Scripts are inlined byte for byte (what they load at run time is not followed).
 */

const NAVIGATION = new Set(['a href', 'area href']);
const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const enc = new TextEncoder();

function text(tree: Tree, path: string): string {
  const n = tree.get(path);
  if (!n) throw new SitefileError('E_EXPORT', `single-file export: '${path}' does not exist`);
  try {
    return UTF8.decode(n.bytes);
  } catch {
    throw new SitefileError('E_EXPORT', `single-file export: '${path}' is not valid UTF-8`);
  }
}

/** `data:` media type: the wbn MIME table's type, without spaces. */
const mediaType = (path: string): string => mimeTypeFor(path).replace(/ /g, '');

const isCss = (path: string): boolean => path.toLowerCase().endsWith('.css');

interface Asset {
  readonly path: string;
  /** The fragment of the URL as written (`#icon`), kept after a data URI. */
  readonly fragment: string;
}

/** The file a reference names when it is an asset to inline (undefined: keep as written). */
function assetOf(r: Reference, from: string): Asset | undefined {
  if (r.element !== undefined && NAVIGATION.has(r.element)) return undefined;
  const t = resolveReference(r.url, from, '/');
  if (t.kind !== 'internal') return undefined;
  if (t.dir) throw new SitefileError('E_EXPORT', `single-file export: cannot inline '${r.url}' in ${from}:${r.line}: it names a directory`);
  // Pages are documents, not assets (a canonical or alternate <link>, an <iframe>): kept as written.
  if (/\.html?$/i.test(t.path)) return undefined;
  const hash = r.url.indexOf('#');
  return { path: t.path, fragment: hash === -1 ? '' : r.url.slice(hash) };
}

function missing(r: Reference, from: string, path: string): SitefileError {
  return new SitefileError('E_EXPORT', `single-file export: cannot inline '${r.url}' in ${from}:${r.line}: no file ${path} in the exported tree`);
}

/** The exported page: the EXPORT path's file, else `/index.html`. */
function pageOf(tree: Tree, page: string | undefined): string {
  const p = page ?? '/index.html';
  if (!tree.get(p)) throw new SitefileError('E_EXPORT', `single-file export: the exported tree has no ${p} (export a directory with an index.html, or name the page: EXPORT /page.html AS html)`);
  return p;
}

/** `EXPORT … AS html`: the page with every asset inlined as a data: URI. */
export function writeSingleFile(tree: Tree, page?: string): Uint8Array {
  const p = pageOf(tree, page);
  const uris = new Map<string, string>();
  const dataUri = (path: string, stack: readonly string[]): string => {
    const known = uris.get(path);
    if (known !== undefined) return known;
    const node = tree.get(path)!;
    const bytes = isCss(path) ? enc.encode(inlineCss(path, [...stack, path])) : node.bytes;
    const uri = `data:${mediaType(path)};base64,${Buffer.from(bytes).toString('base64')}`;
    uris.set(path, uri);
    return uri;
  };
  const rewrite = (src: string, refs: readonly Reference[], from: string, stack: readonly string[]): string => {
    let out = '';
    let at = 0;
    for (const r of refs) {
      const a = assetOf(r, from);
      if (!a) continue;
      if (!tree.get(a.path)) throw missing(r, from, a.path);
      if (stack.includes(a.path)) throw new SitefileError('E_EXPORT', `single-file export: ${from} imports ${a.path}, which imports it back (a cycle)`);
      out += src.slice(at, r.start) + dataUri(a.path, stack) + a.fragment;
      at = r.end;
    }
    return out + src.slice(at);
  };
  const inlineCss = (path: string, stack: readonly string[]): string => {
    const css = text(tree, path);
    return rewrite(css, scanCss(css), path, stack);
  };
  const html = text(tree, p);
  return enc.encode(rewrite(html, scanHtml(html), p, [p]));
}

/** The default MHTML base URL (no EXPORT BASE or --base-url). */
export const MHTML_DEFAULT_BASE = 'https://site.invalid/';

/** The page and every asset it references (through CSS too), each once, in the order first referenced. */
function assetsOf(tree: Tree, page: string): string[] {
  const out = [page];
  const visit = (from: string): void => {
    const src = text(tree, from);
    const refs = from === page || !isCss(from) ? scanHtml(src) : scanCss(src);
    for (const r of refs) {
      const a = assetOf(r, from);
      if (!a) continue;
      if (!tree.get(a.path)) throw missing(r, from, a.path);
      if (out.includes(a.path)) continue;
      out.push(a.path);
      if (isCss(a.path)) visit(a.path);
    }
  };
  visit(page);
  return out;
}

const BOUNDARY = '----MultipartBoundary--sitefile----';

function base64Lines(bytes: Uint8Array): string {
  const b = Buffer.from(bytes).toString('base64');
  let out = '';
  for (let i = 0; i < b.length; i += 76) out += b.slice(i, i + 76) + '\r\n';
  return out;
}

/**
 * `EXPORT … AS mhtml [BASE <url>]`: the page and its assets as MHTML (RFC 2557 multipart/related, as
 * Chrome saves "Webpage, Single File"): each part base64 with its Content-Type and an absolute
 * Content-Location under the base URL (default `https://site.invalid/`), the page first. References
 * are kept as written; the browser resolves them against the page's Content-Location.
 */
export function writeMhtml(tree: Tree, page?: string, baseUrl?: string): Uint8Array {
  const p = pageOf(tree, page);
  const base = validateBaseUrl(baseUrl ?? MHTML_DEFAULT_BASE);
  const url = (path: string): string => base + encodePath(path.slice(1));
  let out =
    `From: <Saved by sitefile>\r\nSnapshot-Content-Location: ${url(p)}\r\nMIME-Version: 1.0\r\n` +
    `Content-Type: multipart/related;\r\n\ttype="text/html";\r\n\tboundary="${BOUNDARY}"\r\n\r\n`;
  for (const path of assetsOf(tree, p)) {
    out +=
      `--${BOUNDARY}\r\nContent-Type: ${mimeTypeFor(path)}\r\nContent-Transfer-Encoding: base64\r\nContent-Location: ${url(path)}\r\n\r\n` +
      base64Lines(tree.get(path)!.bytes) +
      '\r\n';
  }
  return enc.encode(out + `--${BOUNDARY}--\r\n`);
}
