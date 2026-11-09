import { writeZip } from './archive-writers.js';
import { SitefileError } from './errors.js';
import { encodePath } from './links.js';
import { makeFile, Tree, type FileNode } from './tree.js';
import { mimeTypeFor } from './wbn.js';

/**
 * `EXPORT … AS epub` (issue #209): the tree as an EPUB 3 publication.
 *
 * The zip holds `mimetype` first (stored, no extra field, as the OCF container requires), then
 * `META-INF/container.xml`, the package document `EPUB/package.opf` and the site's files under
 * `EPUB/` (all deflated as other zips). The spine is every `.xhtml`/`.html` page in path order,
 * `/index.xhtml` or `/index.html` first; pages must be well-formed XHTML for reading systems and
 * epubcheck. A site file `/nav.xhtml` is the navigation document; otherwise one is generated that lists
 * the spine pages by their `<title>`. Metadata: the title and language of the first page (`<title>`,
 * `<html lang>`; default `Untitled` and `en`), an identifier derived from the tree digest, and
 * `dcterms:modified` from `SOURCE_DATE_EPOCH` (default the Unix epoch), so the file is deterministic.
 * `_redirects` and `_headers` are left out.
 */

const enc = new TextEncoder();
const dec = new TextDecoder();
const SKIP = new Set(['/_redirects', '/_headers']);

const isPage = (p: string): boolean => /\.x?html?$/i.test(p);

function mediaType(p: string): string {
  if (isPage(p)) return 'application/xhtml+xml';
  return mimeTypeFor(p).split(';')[0]!.trim();
}

function escapeXml(s: string): string {
  return s.replace(/&(?![A-Za-z]+;|#[0-9]+;|#x[0-9A-Fa-f]+;)/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function titleOf(bytes: Uint8Array): string | undefined {
  const m = /<title(?:\s[^>]*)?>([^<]*)<\/title>/i.exec(dec.decode(bytes));
  const t = m?.[1]?.replace(/\s+/g, ' ').trim();
  return t ? t : undefined;
}

function langOf(bytes: Uint8Array): string | undefined {
  const m = /<html\b[^>]*?\s(?:xml:)?lang\s*=\s*"([A-Za-z0-9-]+)"/i.exec(dec.decode(bytes));
  return m?.[1];
}

function isoOf(epoch: number | undefined): string {
  return epoch === undefined ? '1970-01-01T00:00:00Z' : new Date(epoch * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

/** `urn:uuid:` (version 8) from a `sha256:<hex>` digest. */
function uuidOf(digest: string): string {
  const h = digest.slice('sha256:'.length);
  const variant = ((parseInt(h.slice(16, 17), 16) & 3) | 8).toString(16);
  return `urn:uuid:${h.slice(0, 8)}-${h.slice(8, 12)}-8${h.slice(13, 16)}-${variant}${h.slice(17, 20)}-${h.slice(20, 32)}`;
}

/** The files of the EPUB container (paths as zip entries, with a leading `/`). */
export function epubTree(tree: Tree, epoch?: number): Tree {
  const files = tree.entries().filter(([p]) => !SKIP.has(p));
  if (tree.get('/package.opf')) throw new SitefileError('E_CONFLICT', "epub export: '/package.opf' is written by the export; rename the site's file");
  const pages = files.filter(([p]) => isPage(p) && p !== '/nav.xhtml');
  const front = pages.findIndex(([p]) => p === '/index.xhtml' || p === '/index.html');
  if (front > 0) pages.unshift(...pages.splice(front, 1));
  if (pages.length === 0) throw new SitefileError('E_EXPORT', 'epub export: the tree has no .xhtml or .html page to read');
  const first = pages[0]![1].bytes;
  const title = escapeXml(titleOf(first) ?? 'Untitled');
  const lang = langOf(first) ?? 'en';
  const ids = new Map(files.map(([p], i) => [p, `f${i + 1}`]));
  const href = (p: string): string => encodePath(p.slice(1));
  const siteNav = tree.get('/nav.xhtml') !== undefined;

  const items = files.map(([p]) => `    <item id="${ids.get(p)}" href="${href(p)}" media-type="${mediaType(p)}"${p === '/nav.xhtml' ? ' properties="nav"' : ''}/>\n`);
  if (!siteNav) items.unshift('    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>\n');
  const opf =
    '<?xml version="1.0" encoding="UTF-8"?>\n' +
    `<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid" xml:lang="${lang}">\n` +
    '  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">\n' +
    `    <dc:identifier id="uid">${uuidOf(tree.digest())}</dc:identifier>\n` +
    `    <dc:title>${title}</dc:title>\n` +
    `    <dc:language>${lang}</dc:language>\n` +
    `    <meta property="dcterms:modified">${isoOf(epoch)}</meta>\n` +
    '  </metadata>\n' +
    '  <manifest>\n' +
    items.join('') +
    '  </manifest>\n' +
    '  <spine>\n' +
    pages.map(([p]) => `    <itemref idref="${ids.get(p)}"/>\n`).join('') +
    '  </spine>\n' +
    '</package>\n';
  const container =
    '<?xml version="1.0" encoding="UTF-8"?>\n' +
    '<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">\n' +
    '  <rootfiles>\n' +
    '    <rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/>\n' +
    '  </rootfiles>\n' +
    '</container>\n';
  const out: Array<[string, FileNode]> = [
    ['/mimetype', makeFile(enc.encode('application/epub+zip'), 'epub')],
    ['/META-INF/container.xml', makeFile(enc.encode(container), 'epub')],
    ['/EPUB/package.opf', makeFile(enc.encode(opf), 'epub')],
  ];
  if (!siteNav) {
    const nav =
      '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<!DOCTYPE html>\n' +
      `<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="${lang}" lang="${lang}">\n` +
      `<head><title>${title}</title></head>\n` +
      `<body>\n<nav epub:type="toc" id="toc">\n<h1>${title}</h1>\n<ol>\n` +
      pages.map(([p, n]) => `<li><a href="${href(p)}">${escapeXml(titleOf(n.bytes) ?? p.slice(1))}</a></li>\n`).join('') +
      '</ol>\n</nav>\n</body>\n</html>\n';
    out.push(['/EPUB/nav.xhtml', makeFile(enc.encode(nav), 'epub')]);
  }
  for (const [p, n] of files) out.push([`/EPUB${p}`, n]);
  return Tree.fromFiles(out);
}

/** `EXPORT … AS epub`: the EPUB file. */
export function writeEpub(tree: Tree, epoch?: number): Uint8Array {
  return writeZip(epubTree(tree, epoch), { epoch }, ['/mimetype']);
}
