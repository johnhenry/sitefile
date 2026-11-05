import type { BundleEntry } from './wbn-reader.js';

/**
 * A Web Bundle as template data (`IMPORT b FROM ./x.wbn AS wbn`):
 *
 *   { version, primaryUrl, entries: [ { url, path, status, contentType,
 *     headers, size, content? } ] }
 *
 * `path` is the URL's pathname; `content` is set only for textual responses
 * (text/*, JSON, XML, JavaScript, SVG) whose body is valid UTF-8. Binary
 * bodies are not exposed as data: use `AS tree` and COPY to place them.
 */
export function bundleValue(bundle: {
  version: string;
  primaryUrl: string | undefined;
  entries: readonly BundleEntry[];
}): Record<string, unknown> {
  return {
    version: bundle.version,
    primaryUrl: bundle.primaryUrl ?? null,
    entries: bundle.entries.map((e) => {
      const contentType = e.headers['content-type'] ?? '';
      const content = isTextual(contentType) ? utf8(e.body) : undefined;
      return {
        url: e.url,
        path: pathOf(e.url),
        status: e.status,
        contentType,
        headers: { ...e.headers },
        size: e.body.byteLength,
        ...(content !== undefined ? { content } : {}),
      };
    }),
  };
}

const TEXTUAL = /^(text\/|application\/(json|xml|javascript|ecmascript|manifest\+json|[a-z0-9.+-]*\+(json|xml))|image\/svg\+xml)/i;

function isTextual(contentType: string): boolean {
  return TEXTUAL.test(contentType.trim());
}

function utf8(bytes: Uint8Array): string | undefined {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    return undefined;
  }
}

function pathOf(url: string): string {
  try {
    return new URL(url).pathname;
  } catch {
    return url;
  }
}
