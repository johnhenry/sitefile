// CANONICAL (docs/LANGUAGE.md, "Site files"): a `<link rel="canonical">` in every page that has none.
// Ported as rust/src/canonical.rs: both must write the same pages and messages.
import type { SourceSpan } from './errors.js';
import { canonicalSlot, referenceKind } from './links.js';
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
 * below `root` (except `/404.html` there) that is valid UTF-8 and has no canonical link yet. Pages without
 * a `</head>` are left as they are, and named in one warning.
 */
export function canonical(tree: Tree, op: CanonicalOp, origin: string, warn: (m: string) => void): Tree {
  const files: Array<[string, FileNode]> = [];
  const headless: string[] = [];
  for (const [p, node] of tree.entries()) {
    if (referenceKind(p) !== 'html') continue;
    const url = op.root === '/' ? p : p.startsWith(op.root + '/') ? p.slice(op.root.length) : undefined;
    if (url === undefined || url === '/404.html') continue;
    let text: string;
    try {
      text = UTF8.decode(node.bytes);
    } catch {
      continue;
    }
    const at = canonicalSlot(text);
    if (at === 'has') continue;
    if (at === undefined) {
      headless.push(p);
      continue;
    }
    const link = `<link rel="canonical" href="${xmlEscape(canonicalUrl(op.base, url, op.pretty))}">`;
    files.push([p, makeFile(new TextEncoder().encode(text.slice(0, at) + link + text.slice(at)), origin, node.mode)]);
  }
  if (headless.length > 0) {
    warn(`${origin}: ${headless.length === 1 ? '1 page has' : `${headless.length} pages have`} no </head>, so no canonical link was added: ${headless.join(', ')}`);
  }
  return files.length ? tree.withFiles(files, 'overwrite', op.span) : tree;
}
