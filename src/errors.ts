/** A position in a source file. Lines and columns are 1-based. */
export interface SourceSpan {
  readonly file: string;
  readonly line: number;
  readonly column: number;
}

/**
 * Stable error codes. Tests assert on these, so add new codes rather than
 * renaming existing ones.
 */
export type ErrorCode =
  // language
  | 'E_SYNTAX'
  | 'E_UNKNOWN_INSTRUCTION'
  | 'E_UNSUPPORTED'
  | 'E_VALIDATION'
  | 'E_UNDEFINED'
  | 'E_DUPLICATE'
  // values
  | 'E_MISSING_ARG'
  | 'E_TYPE'
  // paths and local files
  | 'E_PATH'
  | 'E_PATH_ESCAPE'
  | 'E_NOT_FOUND'
  | 'E_UNSUPPORTED_FILE'
  | 'E_HARDLINK'
  /** Two paths equal under Unicode simple case folding, or two context names equal after NFC. */
  | 'E_PATH_COLLISION'
  // composition
  | 'E_CONFLICT'
  // remote resources
  | 'E_PROTOCOL'
  | 'E_SSRF_BLOCKED'
  | 'E_FETCH'
  | 'E_FETCH_STATUS'
  | 'E_FETCH_TIMEOUT'
  | 'E_TOO_LARGE'
  | 'E_REDIRECT'
  | 'E_INTEGRITY'
  | 'E_LOCK_MISMATCH'
  | 'E_LOCK_MISSING'
  /** A remote input (IMPORT URL or BASE image) has no pin, under --strict-pins. */
  | 'E_UNPINNED'
  // archives
  | 'E_ARCHIVE'
  | 'E_ARCHIVE_UNSAFE'
  | 'E_ARCHIVE_LIMIT'
  // rendering
  | 'E_ENCODING'
  | 'E_TEMPLATE'
  | 'E_FRONTMATTER'
  | 'E_DATA'
  | 'E_PLUGIN'
  // export
  | 'E_EXPORT'
  | 'E_EXPORT_UNSAFE'
  // local image store and site servers
  | 'E_STORE'
  | 'E_SERVER'
  // registries
  | 'E_REGISTRY'
  | 'E_AUTH'
  // cli
  | 'E_USAGE';

export class SitefileError extends Error {
  readonly code: ErrorCode;
  readonly span: SourceSpan | undefined;
  /** The message without the location prefix. */
  readonly detail: string;

  constructor(code: ErrorCode, detail: string, span?: SourceSpan, options?: ErrorOptions) {
    super(span ? `${formatSpan(span)}: ${detail}` : detail, options);
    this.name = 'SitefileError';
    this.code = code;
    this.span = span;
    this.detail = detail;
  }
}

export function formatSpan(span: SourceSpan): string {
  return `${span.file}:${span.line}:${span.column}`;
}

export function isSitefileError(e: unknown, code?: ErrorCode): e is SitefileError {
  return e instanceof SitefileError && (code === undefined || e.code === code);
}
