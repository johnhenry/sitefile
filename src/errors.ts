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
  /** A malformed or unsupported Git smart HTTP response or packfile (git+https IMPORT, Git URL contexts). */
  | 'E_GIT'
  /** A build secret (--secret, IMPORT … SECRET) is missing, unreadable or not usable in a header. */
  | 'E_SECRET'
  // archives
  | 'E_ARCHIVE'
  | 'E_ARCHIVE_UNSAFE'
  | 'E_ARCHIVE_LIMIT'
  // rendering
  | 'E_ENCODING'
  | 'E_TEMPLATE'
  | 'E_FRONTMATTER'
  | 'E_DATA'
  /** A document of an `IMPORT … SCHEMA` does not match the schema, or the schema itself is invalid. */
  | 'E_SCHEMA'
  | 'E_PLUGIN'
  /** CHECKLINKS found references to files that do not exist (or, with --external, URLs that do not answer). */
  | 'E_BROKEN_LINK'
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
  /** One or more `sitefile bake` targets failed. */
  | 'E_BAKE'
  | 'E_USAGE'
  /** The build was stopped by its embedder (the Rust library's `CancelToken`; see docs/RUST.md). */
  | 'E_CANCELLED';

/**
 * The permission a build needs: carried by the `E_UNSUPPORTED` error of a JavaScript PLUGIN
 * (`--allow-plugin <name>`) or a TOOL (`--allow-exec <name>`) that was not allowed, so callers
 * need not parse the message.
 */
export interface Needs {
  readonly kind: 'plugin' | 'tool';
  /** The declared PLUGIN or TOOL name. */
  readonly name: string;
  /** The flag that grants it. */
  readonly flag: '--allow-plugin' | '--allow-exec';
}

export function needsPlugin(name: string): Needs {
  return { kind: 'plugin', name, flag: '--allow-plugin' };
}

export function needsTool(name: string): Needs {
  return { kind: 'tool', name, flag: '--allow-exec' };
}

export interface SitefileErrorOptions extends ErrorOptions {
  /** What must be allowed (permission errors only). */
  readonly needs?: Needs;
}

export class SitefileError extends Error {
  readonly code: ErrorCode;
  readonly span: SourceSpan | undefined;
  /** The message without the location prefix. */
  readonly detail: string;
  /** For a permission error (`E_UNSUPPORTED` at a PLUGIN or TOOL line): what must be allowed. Not part of the message. */
  readonly needs: Needs | undefined;

  constructor(code: ErrorCode, detail: string, span?: SourceSpan, options?: SitefileErrorOptions) {
    super(span ? `${formatSpan(span)}: ${detail}` : detail, options);
    this.name = 'SitefileError';
    this.code = code;
    this.span = span;
    this.detail = detail;
    this.needs = options?.needs;
  }
}

export function formatSpan(span: SourceSpan): string {
  return `${span.file}:${span.line}:${span.column}`;
}

export function isSitefileError(e: unknown, code?: ErrorCode): e is SitefileError {
  return e instanceof SitefileError && (code === undefined || e.code === code);
}
