// CANONICAL (docs/LANGUAGE.md, "Site files"): a `<link rel="canonical">` in every page that has none.
// Ported as rust/src/canonical.rs: both must write the same pages and messages.
import type { SourceSpan } from './errors.js';
import { alternates, type Languages } from './hreflang.js';
import { headLinks, referenceKind } from './links.js';
import { pageUrl, xmlEscape } from './site.js';
import { makeFile, type FileNode, type Tree } from './tree.js';

export interface CanonicalOp {
  readonly base: string;
  readonly pretty: boolean;
  readonly root: string;
  readonly span: SourceSpan;
}

const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** A page's canonical URL: `<base><url path>`, `…/index.html` as its directory, and with `pretty` no `.html`. */
export function canonicalUrl(base: string, url: string, pretty: boolean): string {
  const u = pretty && url.endsWith('.html') && !url.endsWith('/index.html') ? url.slice(0, -'.html'.length) : url;
  return pageUrl(base, u);
}

/**
 * The tree with `<link rel="canonical" href="…">` inserted before `</head>` of every `.html`/`.htm` page
 * below `root` (except `/404.html` there) that is valid UTF-8 and has no canonical link yet. With `langs`
 * (LANGUAGES), a page that has translations also gets a `<link rel="alternate" hreflang="…">` for each
 * (see {@link alternates}), unless it has such links already. Pages without a `</head>` are left as they
 * are, and named in one warning.
 */
export function canonical(tree: Tree, op: CanonicalOp, origin: string, warn: (m: string) => void, langs?: Languages): Tree {
  const pages: Array<[string, string, FileNode]> = [];
  for (const [p, node] of tree.entries()) {
    if (referenceKind(p) !== 'html') continue;
    const url = op.root === '/' ? p : p.startsWith(op.root + '/') ? p.slice(op.root.length) : undefined;
    if (url === undefined || url === '/404.html') continue;
    pages.push([p, url, node]);
  }
  const alts = langs ? alternates(pages.map(([p, url]) => [p, url] as const), langs, 'CANONICAL', op.span) : undefined;
  const files: Array<[string, FileNode]> = [];
  const headless: string[] = [];
  for (const [p, url, node] of pages) {
    let text: string;
    try {
      text = UTF8.decode(node.bytes);
    } catch {
      continue;
    }
    const head = headLinks(text);
    const alt = alts?.get(p);
    const addAlternates = alt !== undefined && !head.hreflang;
    if (head.canonical && !addAlternates) continue;
    if (head.at === undefined) {
      headless.push(p);
      continue;
    }
    let links = head.canonical ? '' : `<link rel="canonical" href="${xmlEscape(canonicalUrl(op.base, url, op.pretty))}">`;
    if (addAlternates) {
      for (const [lang, u] of alt) links += `<link rel="alternate" hreflang="${xmlEscape(lang)}" href="${xmlEscape(canonicalUrl(op.base, u, op.pretty))}">`;
    }
    files.push([p, makeFile(new TextEncoder().encode(text.slice(0, head.at) + links + text.slice(head.at)), origin, node.mode)]);
  }
  if (headless.length > 0) {
    warn(`${origin}: ${headless.length === 1 ? '1 page has' : `${headless.length} pages have`} no </head>, so no canonical link was added: ${headless.join(', ')}`);
  }
  return files.length ? tree.withFiles(files, 'overwrite', op.span) : tree;
}
