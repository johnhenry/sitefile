// CHECKLINKS (docs/LANGUAGE.md, "Link checking"): find the references in a stage's HTML and CSS files that
// point at nothing. Ported as rust/src/checklinks.rs: both report the same links, in the same order, with the
// same messages.
import { isSitefileError, SitefileError, type SourceSpan } from './errors.js';
import { resolveReference, scanFile } from './links.js';
import { fetchRemote } from './resources/https.js';
import type { FetchPolicy } from './resources/types.js';
import type { Tree } from './tree.js';

export interface CheckLinksOp {
  /** Report broken links as warnings instead of failing. */
  readonly warn: boolean;
  /** Also request http(s) URLs (under the network policy). */
  readonly external: boolean;
  /** The EXPORT path: absolute references resolve below it. */
  readonly root: string;
  /** REDIRECT sources (`/old`, `/blog/*`): a link to one of them is not broken. */
  readonly redirects: readonly string[];
  readonly span: SourceSpan;
}

export interface BrokenLink {
  /** The referring file (a tree path) and the line of the reference. */
  readonly file: string;
  readonly line: number;
  /** The URL as the file has it (character references and escapes decoded). */
  readonly url: string;
  readonly reason: string;
}

/** The most broken links an error lists (CHECKLINKS --warn prints every one). */
export const MAX_LISTED = 20;

/**
 * Whether a tree path is served the way `sitefile serve` and static hosts serve it: the file itself, `<path>.html`
 * (clean URLs) or `<path>/index.html`; for a directory URL (`…/`), its `index.html`.
 */
export function isServed(tree: Tree, path: string, dir: boolean): boolean {
  const index = path === '/' ? '/index.html' : `${path}/index.html`;
  if (dir) return tree.isFile(index);
  return tree.isFile(path) || (path !== '/' && tree.isFile(`${path}.html`)) || (tree.isDirectory(path) && tree.isFile(index));
}

/** The URL path of a tree path below the root, or undefined outside it. */
function urlPath(root: string, path: string): string | undefined {
  if (root === '/') return path;
  if (path === root) return '/';
  return path.startsWith(root + '/') ? path.slice(root.length) : undefined;
}

/** Whether a REDIRECT source covers a URL path: equal, or a `/*` prefix of it. */
export function redirected(sources: readonly string[], url: string): boolean {
  return sources.some((from) => from === url || (from.endsWith('/*') && url.startsWith(from.slice(0, -1))));
}

/** Why an external URL is broken (undefined when it answered), from the error fetching it. */
function externalReason(e: SitefileError, url: string, policy: FetchPolicy): string | undefined {
  switch (e.code) {
    case 'E_TOO_LARGE':
      // The limit is 0 bytes: it answered 200 with a body.
      return undefined;
    case 'E_FETCH_STATUS': {
      const status = Number(/HTTP (\d+)$/.exec(e.detail)?.[1] ?? '0');
      return status >= 400 || status === 0 ? `HTTP ${status}` : undefined;
    }
    case 'E_FETCH_TIMEOUT':
      return `no answer within ${policy.timeoutMs} ms`;
    case 'E_PROTOCOL':
      return /^http:/i.test(url) && !policy.allowHttp ? 'plain http is checked only with --allow-http' : 'not allowed by the network policy';
    case 'E_SSRF_BLOCKED':
      return 'a non-public address (checked only with --allow-private-network)';
    case 'E_HOST_BLOCKED':
      return 'refused by the allow-hosts/deny-hosts rules';
    case 'E_REDIRECT':
      return 'too many or invalid redirects';
    default:
      return 'could not connect';
  }
}

/**
 * Every broken link in the HTML and CSS files of `tree`, by file (in path order) and then in the order of the
 * file. An internal link is broken when its target is not served (see {@link isServed}) and no REDIRECT covers
 * it; with `external`, an http(s) link is broken when requesting it fails or answers 400 or more (each URL is
 * requested once, in order of first use).
 */
export async function brokenLinks(tree: Tree, op: CheckLinksOp, policy: FetchPolicy): Promise<BrokenLink[]> {
  const out: BrokenLink[] = [];
  const external = new Map<string, string | undefined>();
  const entries = tree.entries().sort(([a], [b]) => Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8')));
  for (const [file, node] of entries) {
    const scan = scanFile(file, node.bytes);
    if (!scan) continue;
    for (const r of scan.refs) {
      const t = resolveReference(r.url, file, op.root);
      if (t.kind === 'none') continue;
      let reason: string | undefined;
      if (t.kind === 'external') {
        if (!op.external) continue;
        const url = t.url.startsWith('//') ? 'https:' + t.url : t.url;
        if (!external.has(url)) {
          try {
            await fetchRemote(url, { ...policy, maxBytes: 0 }, op.span);
            external.set(url, undefined);
          } catch (e) {
            if (!isSitefileError(e)) throw e;
            external.set(url, externalReason(e, url, policy));
          }
        }
        reason = external.get(url);
      } else if (!isServed(tree, t.path, t.dir)) {
        const url = urlPath(op.root, t.path);
        const display = t.dir && t.path !== '/' ? t.path + '/' : t.path;
        if (url === undefined || !redirected(op.redirects, t.dir && url !== '/' ? url + '/' : url)) reason = `not found: ${display}`;
      }
      if (reason !== undefined) out.push({ file, line: r.line, url: r.url, reason });
    }
  }
  return out;
}

/** One broken link as messages show it: `/index.html:3: css/a.css (not found: /css/a.css)`. */
export function brokenLinkText(b: BrokenLink): string {
  return `${b.file}:${b.line}: ${b.url} (${b.reason})`;
}

/** Report the broken links: a warning each with `--warn`, otherwise one E_BROKEN_LINK error listing them. */
export function reportBrokenLinks(broken: readonly BrokenLink[], op: CheckLinksOp, origin: string, warn: (m: string) => void): void {
  if (broken.length === 0) return;
  if (op.warn) {
    for (const b of broken) warn(`${origin}: broken link ${brokenLinkText(b)}`);
    return;
  }
  const listed = broken.slice(0, MAX_LISTED).map((b) => `\n  ${brokenLinkText(b)}`);
  const more = broken.length > MAX_LISTED ? `\n  … and ${broken.length - MAX_LISTED} more (CHECKLINKS --warn lists them all)` : '';
  throw new SitefileError('E_BROKEN_LINK', `CHECKLINKS: ${broken.length} broken link${broken.length === 1 ? '' : 's'}:${listed.join('')}${more}`, op.span);
}
