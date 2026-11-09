// CSP (docs/LANGUAGE.md, "Content Security Policy"): a policy per exported page with the hashes of its
// inline scripts and styles. Ported as rust/src/csp.rs: both must write the same headers and pages.
import { createHash } from 'node:crypto';
import { inlineBlocks, referenceKind } from './links.js';
import { cspMeta, cspPolicy, pagePatterns, type HeaderRule } from './site.js';
import { makeFile, type FileNode, type Tree } from './tree.js';

/** `sha256-<base64>` of a text's UTF-8 bytes, as CSP hash sources name it. */
export function cspHash(text: string): string {
  return 'sha256-' + createHash('sha256').update(text, 'utf8').digest('base64');
}

const SPACE_OR_CONTROL = /[\u0000- \u007f]/;
const UTF8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

export interface CspResult {
  /** META off: one `Content-Security-Policy` rule per page path (see {@link pagePatterns}), in page order. */
  readonly rules: HeaderRule[];
  /** META on: the pages with the `<meta>` inserted after `<head>`. */
  readonly pages: Array<[string, FileNode]>;
  /** META on: pages with no `<head>` start tag, left as they are. */
  readonly headless: string[];
}

/**
 * The policies for the `.html`/`.htm` pages (valid UTF-8) of `tree` below `exportPath`, in path order (UTF-8).
 * `tree` is the whole tree; paths in the results are tree paths, patterns URL paths below `exportPath`.
 */
export function cspPages(tree: Tree, exportPath: string, base: string, meta: boolean, origin: string): CspResult {
  const rules: HeaderRule[] = [];
  const pages: Array<[string, FileNode]> = [];
  const headless: string[] = [];
  const entries = tree.entries().sort(([a], [b]) => Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8')));
  for (const [p, node] of entries) {
    if (referenceKind(p) !== 'html') continue;
    const url = exportPath === '/' ? p : p.startsWith(exportPath + '/') ? p.slice(exportPath.length) : undefined;
    if (url === undefined) continue;
    let text: string;
    try {
      text = UTF8.decode(node.bytes);
    } catch {
      continue;
    }
    const blocks = inlineBlocks(text);
    const policy = cspPolicy(base, blocks.scripts.map(cspHash), blocks.styles.map(cspHash));
    if (!meta) {
      for (const pattern of pagePatterns(url)) if (!SPACE_OR_CONTROL.test(pattern)) rules.push({ pattern, name: 'Content-Security-Policy', value: policy });
    } else if (blocks.headEnd === undefined) {
      headless.push(p);
    } else {
      const out = text.slice(0, blocks.headEnd) + cspMeta(policy) + text.slice(blocks.headEnd);
      pages.push([p, makeFile(new TextEncoder().encode(out), origin, node.mode)]);
    }
  }
  return { rules, pages, headless };
}
