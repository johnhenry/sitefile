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
  | 'E_HOST_BLOCKED'
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
 * The permission a build needs, so callers need not parse the message. Carried by:
 *   - the `E_UNSUPPORTED` error of a JavaScript PLUGIN (`--allow-plugin <name>`) or a TOOL
 *     (`--allow-exec <name>`) that was not allowed;
 *   - the `E_HOST_BLOCKED` error of a host that matches no allow-hosts rule (`--allow-hosts <host>`;
 *     a deny-hosts match carries none, since no flag overrides it);
 *   - in strict network mode, the `E_HOST_BLOCKED` error of an `IMPORT … SECRET` whose secret may not be
 *     sent to the import's host (`--allow-secret-host <id>=<host>`).
 */
export interface Needs {
  readonly kind: 'plugin' | 'tool' | 'host' | 'secret-host';
  /** The declared PLUGIN or TOOL name, the host, or `<secret id>=<host>`. */
  readonly name: string;
  /** The flag that grants it (its value is `name`). */
  readonly flag: '--allow-plugin' | '--allow-exec' | '--allow-hosts' | '--allow-secret-host';
}

export function needsPlugin(name: string): Needs {
  return { kind: 'plugin', name, flag: '--allow-plugin' };
}

export function needsTool(name: string): Needs {
  return { kind: 'tool', name, flag: '--allow-exec' };
}

export function needsHost(host: string): Needs {
  return { kind: 'host', name: host, flag: '--allow-hosts' };
}

export function needsSecretHost(id: string, host: string): Needs {
  return { kind: 'secret-host', name: `${id}=${host}`, flag: '--allow-secret-host' };
}

/**
 * The `--json` form of a failed command (printed on stdout, next to the `error[CODE]:` line on stderr):
 * `{ "error": { code, message, span, needs } }`, with `span` and `needs` null when absent.
 */
export function errorJson(e: SitefileError): string {
  const error = {
    code: e.code,
    message: e.message,
    span: e.span ? { file: e.span.file, line: e.span.line, column: e.span.column } : null,
    needs: e.needs ? { kind: e.needs.kind, name: e.needs.name, flag: e.needs.flag } : null,
  };
  return JSON.stringify({ error }, null, 2) + '\n';
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
  /** For a permission error (`E_UNSUPPORTED` at a PLUGIN or TOOL line, `E_HOST_BLOCKED`): what must be allowed. Not part of the message. */
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
