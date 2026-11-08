// Keyword lists shared by grammar.js, the query generator and the tests.
// test/keywords.test.mjs checks them against src/language/parser.ts,
// src/language/ast.ts, src/language/scanner.ts and rust/src/language/parser.rs.

export const KEYWORDS = {
  instructions: ['FROM', 'STAGE', 'ARG', 'LET', 'IMPORT', 'COPY', 'RENDER', 'TRANSFORM', 'PLUGIN', 'TOOL', 'SITEMAP', 'FEED', 'REDIRECT', 'HEADER', 'FINGERPRINT', 'PRECOMPRESS', 'CHECKLINKS', 'EXPORT'],
  /** Rejected by the parser (E_UNSUPPORTED); parsed as `unsupported_instruction`. */
  unsupported: ['RUN', 'REMOVE', 'GENERATE', 'ASSERT', 'META', 'EXEC'],
  clauses: ['FROM', 'AS', 'TO', 'INTEGRITY', 'SORT', 'BY', 'ASC', 'DESC', 'SCHEMA', 'DRAFTS', 'PAGE', 'FIRST', 'FOR', 'IN', 'WITH', 'OPTIONS', 'ARGS', 'BASE', 'AT', 'COMPRESSION', 'LINK', 'TITLE', 'STATUS', 'IMAGE', 'SECRET', 'HEADER', 'BEARER', 'MANIFEST', 'MIN', 'ARG'],
  scratch: 'scratch',
  valueTypes: ['string', 'number', 'boolean'],
  importKinds: ['file', 'text', 'json', 'tree', 'markdown', 'yaml', 'csv', 'wbn', 'html', 'sitefile'],
  /** The LET-only type of an html fragment (`LET name: html`), a keyword token here. */
  fragmentType: 'html',
  exportFormats: ['directory', 'zip', 'tar', 'tgz', 'tar.gz', 'wbn', 'oci'],
  compressions: ['gzip', 'none'],
  escapeModes: ['none', 'html'],
  /** `${file "…" | helper}` (src/media.ts FILE_HELPERS). */
  fileHelpers: ['url', 'width', 'height', 'type', 'size', 'integrity', 'datauri', 'srcset', 'img', 'video', 'audio', 'source', 'script', 'stylesheet', 'preload', 'modulepreload', 'pre', 'svg'],
  redirectStatuses: ['301', '302', '303', '307', '308'],
  flags: {
    COPY: ['overwrite', 'from', 'exclude', 'chmod', 'parents'],
    RENDER: ['escape', 'overwrite', 'from', 'inline-svg'],
    TRANSFORM: ['overwrite', 'from', 'include', 'exclude'],
    SITEMAP: ['overwrite'],
    FEED: ['overwrite'],
    REDIRECT: ['overwrite'],
    HEADER: ['overwrite'],
    FINGERPRINT: ['overwrite', 'no-cache-headers'],
    PRECOMPRESS: ['overwrite'],
    CHECKLINKS: ['warn', 'external'],
  },
  heredocInstructions: ['COPY', 'RENDER', 'TRANSFORM'],
  /** `LET name: html <<HTML` also takes a heredoc (scanner.ts isHtmlLet). */
  heredocLet: true,
};

/**
 * Heredoc delimiters that embed another language (see queries/injections.scm).
 * Matched exactly as written here, in upper or lower case.
 */
export const EMBEDS = [
  { names: ['HTML', 'HTM'], lang: 'html' },
  { names: ['CSS'], lang: 'css' },
  { names: ['JAVASCRIPT', 'JS', 'MJS'], lang: 'javascript' },
  { names: ['MARKDOWN', 'MD'], lang: 'markdown' },
  { names: ['JSON'], lang: 'json' },
  { names: ['YAML', 'YML'], lang: 'yaml' },
  { names: ['SVG', 'XML'], lang: 'xml' },
];

/** `FROM` -> /[Ff][Rr][Oo][Mm]/ source (case-insensitive without regex flags). */
export function caseInsensitive(word) {
  return [...word]
    .map((ch) => {
      const lo = ch.toLowerCase();
      const up = ch.toUpperCase();
      if (lo !== up) return `[${up}${lo}]`;
      return ch.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
    })
    .join('');
}
