import { createHash } from 'node:crypto';
import { writeZip } from './archive-writers.js';
import { gzip } from './deflate.js';
import { SitefileError } from './errors.js';
import { makeFile, Tree, type FileNode } from './tree.js';
import { isoDate as iso, warcRecords, type WarcOptions } from './warc.js';

/**
 * `EXPORT / AS wacz BASE <url>` (issue #206): a WACZ 1.1.1 package (Webrecorder), the format
 * ReplayWeb.page opens. A zip of:
 *
 * - `archive/data.warc.gz`: the site's WARC (as `AS warc COMPRESSION gzip` writes it), one gzip
 *   member per record;
 * - `indexes/index.cdx`: a CDXJ line per response record, `<surt> <timestamp> {json}` sorted, with the
 *   record's offset and length in `data.warc.gz`;
 * - `pages/pages.jsonl`: the HTML pages (an index page by its directory URL), with their `<title>`;
 * - `datapackage.json` (every file with its SHA-256 and size) and `datapackage-digest.json`.
 *
 * Dates come from `SOURCE_DATE_EPOCH` (default the Unix epoch), so the package is deterministic.
 */

const enc = new TextEncoder();
const dec = new TextDecoder();

/** `YYYYMMDDhhmmss`. */
function timestamp14(epoch: number | undefined): string {
  return iso(epoch).replace(/[-:TZ]/g, '');
}

/** The SURT form of an http(s) URL: `org,example)/path` (host labels reversed, a leading `www.` dropped). */
export function surt(url: string): string {
  const m = /^https?:\/\/([^/?#]*)(.*)$/i.exec(url);
  if (!m) return url;
  const labels = (m[1] as string).toLowerCase().split('.');
  if (labels[0] === 'www' && labels.length > 1) labels.shift();
  return `${labels.reverse().join(',')})${m[2] === '' ? '/' : m[2]}`;
}

function titleOf(bytes: Uint8Array): string | undefined {
  const m = /<title(?:\s[^>]*)?>([^<]*)<\/title>/i.exec(dec.decode(bytes));
  const t = m?.[1]?.replace(/\s+/g, ' ').trim();
  return t ? t : undefined;
}

const sha256 = (b: Uint8Array): string => 'sha256:' + createHash('sha256').update(b).digest('hex');

/** `EXPORT … AS wacz`: the WACZ file. */
export function writeWacz(tree: Tree, options: WarcOptions): Uint8Array {
  if (options.baseUrl === undefined || options.baseUrl === '') {
    throw new SitefileError('E_VALIDATION', "wacz export needs a base URL: use 'EXPORT <path> AS wacz BASE https://example.com/' or --base-url");
  }
  const records = warcRecords(tree, options);
  const ts = timestamp14(options.epoch);
  const members: Uint8Array[] = [];
  const cdx: string[] = [];
  const pages: string[] = [];
  let offset = 0;
  for (const r of records) {
    const member = gzip(r.bytes);
    members.push(member);
    const res = r.response;
    if (res) {
      const entry = { url: res.uri, mime: res.type.split(';')[0]!.trim(), status: '200', digest: res.digest, length: member.byteLength, offset, filename: 'data.warc.gz' };
      cdx.push(`${surt(res.uri)} ${ts} ${JSON.stringify(entry)}`);
      // An index page once, by its directory URL; other HTML pages by their own.
      const index = res.path === '/index.html' || res.path.endsWith('/index.html');
      if (res.type.startsWith('text/html') && (!index || res.uri.endsWith('/'))) {
        const title = titleOf(tree.get(res.path)!.bytes);
        pages.push(JSON.stringify({ id: sha256(enc.encode(res.uri)).slice(7, 39), url: res.uri, ts: iso(options.epoch), ...(title !== undefined ? { title } : {}) }));
      }
    }
    offset += member.byteLength;
  }
  cdx.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  const warc = new Uint8Array(offset);
  let at = 0;
  for (const m of members) {
    warc.set(m, at);
    at += m.byteLength;
  }
  const files: Array<[string, Uint8Array]> = [
    ['archive/data.warc.gz', warc],
    ['indexes/index.cdx', enc.encode(cdx.map((l) => l + '\n').join(''))],
    ['pages/pages.jsonl', enc.encode([JSON.stringify({ format: 'json-pages-1.0', id: 'pages', title: 'All Pages' }), ...pages].map((l) => l + '\n').join(''))],
  ];
  const datapackage = enc.encode(
    JSON.stringify(
      {
        profile: 'data-package',
        resources: files.map(([path, bytes]) => ({ name: path.slice(path.lastIndexOf('/') + 1), path, hash: sha256(bytes), bytes: bytes.byteLength })),
        wacz_version: '1.1.1',
        software: 'sitefile',
        created: iso(options.epoch),
      },
      null,
      2,
    ) + '\n',
  );
  const digest = enc.encode(JSON.stringify({ path: 'datapackage.json', hash: sha256(datapackage) }, null, 2) + '\n');
  const out: Array<[string, FileNode]> = [
    ...files.map(([p, b]) => [`/${p}`, makeFile(b, 'wacz')] as [string, FileNode]),
    ['/datapackage.json', makeFile(datapackage, 'wacz')],
    ['/datapackage-digest.json', makeFile(digest, 'wacz')],
  ];
  return writeZip(Tree.fromFiles(out), { epoch: options.epoch });
}
