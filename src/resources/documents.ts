import { posix } from 'node:path';
import { SitefileError, type SourceSpan } from '../errors.js';
import { parseFrontmatter } from '../frontmatter.js';
import { renderMarkdown } from '../markdown.js';
import { decodeUtf8 } from '../render.js';
import type { Tree } from '../tree.js';

/**
 * A Markdown document as templates see it: its frontmatter fields plus
 *
 * - `slug`: the file name without `.md` and without a leading `YYYY-MM-DD-`
 *   (frontmatter `slug` wins)
 * - `date`: that leading date, if the frontmatter has no `date`
 * - `path`: the file's path inside the import (`/2026-01-02-hello.md`)
 * - `content`: the body rendered to HTML (insert it with `${post.content|raw}`)
 * - `excerpt`: plain text of the first paragraph (up to ~200 characters)
 *
 * `path`, `content` and `excerpt` are always computed; frontmatter keys with
 * those names are ignored.
 */
export type MarkdownDocument = Record<string, unknown>;

const DATED = /^(\d{4}-\d{2}-\d{2})-(.+)$/;
const SAFE_SLUG = /^[A-Za-z0-9][A-Za-z0-9._~-]*$/;

export function markdownDocument(bytes: Uint8Array, path: string, label: string, span?: SourceSpan): MarkdownDocument {
  const text = decodeUtf8(bytes, `'${label}'`, span);
  const { data, body } = parseFrontmatter(text, label);
  const { html, excerpt } = renderMarkdown(body);
  const stem = posix.basename(path).replace(/\.(md|markdown)$/i, '');
  const dated = DATED.exec(stem);
  const doc: MarkdownDocument = {
    slug: dated ? dated[2]! : stem,
    ...(dated ? { date: dated[1]! } : {}),
    ...data,
    path,
    content: html,
    excerpt,
  };
  const slug = doc['slug'];
  if (typeof slug !== 'string' || !SAFE_SLUG.test(slug)) {
    throw new SitefileError(
      'E_VALIDATION',
      `'${label}': slug ${JSON.stringify(slug)} must be letters, digits, '.', '_', '~' or '-' (it becomes part of a URL)`,
      span,
    );
  }
  return doc;
}

/** Every `.md` / `.markdown` file in a tree, as documents, ordered by path. */
export function markdownCollection(tree: Tree, labelPrefix: string, span?: SourceSpan): MarkdownDocument[] {
  const docs: MarkdownDocument[] = [];
  for (const [p, node] of tree.entries()) {
    if (!/\.(md|markdown)$/i.test(p)) continue;
    docs.push(markdownDocument(node.bytes, p, `${labelPrefix}${p}`, span));
  }
  return docs;
}

/**
 * Sort documents by a field. Numbers compare numerically, everything else as
 * strings (ISO dates sort correctly). Documents missing the field go last in
 * either direction; ties keep path order.
 */
export function sortDocuments(docs: readonly MarkdownDocument[], key: string, descending: boolean): MarkdownDocument[] {
  const cmp = (a: unknown, b: unknown): number => {
    if (typeof a === 'number' && typeof b === 'number') return a - b;
    const x = String(a);
    const y = String(b);
    return x < y ? -1 : x > y ? 1 : 0;
  };
  return [...docs].sort((a, b) => {
    const va = a[key];
    const vb = b[key];
    const ma = va === undefined || va === null;
    const mb = vb === undefined || vb === null;
    if (ma || mb) return ma === mb ? byPath(a, b) : ma ? 1 : -1;
    const c = cmp(va, vb);
    return c === 0 ? byPath(a, b) : descending ? -c : c;
  });
}

function byPath(a: MarkdownDocument, b: MarkdownDocument): number {
  const x = String(a['path']);
  const y = String(b['path']);
  return x < y ? -1 : x > y ? 1 : 0;
}
