import type { SourceSpan } from '../errors.js';

/**
 * A string with `${...}` interpolations, split into parts.
 * In Sitefile operands `path` is always empty (only ARG/LET names are allowed);
 * in RENDER templates `path` holds dotted JSON access (`${data.site.title}`).
 */
export type TemplatePart =
  | { readonly kind: 'text'; readonly value: string }
  | { readonly kind: 'var'; readonly name: string; readonly path: readonly string[]; readonly span: SourceSpan };

/** One instruction argument: a quoted or bare string, possibly interpolated. */
export interface Operand {
  readonly parts: readonly TemplatePart[];
  readonly span: SourceSpan;
}

export type ValueType = 'string' | 'number' | 'boolean';
export type ImportKind = 'file' | 'text' | 'json' | 'tree' | 'markdown' | 'yaml' | 'csv' | 'wbn';
export type EscapeMode = 'none' | 'html';

export interface FromInstruction {
  readonly kind: 'FROM';
  readonly span: SourceSpan;
  /**
   * `scratch` keyword, an operand (an earlier stage name or a context-relative
   * directory; the planner decides, see docs/LANGUAGE.md), or `image <ref> [AT <dir>]`.
   */
  readonly base: FromBase;
  readonly alias?: string;
  /** Set when written as `STAGE <name> [FROM <base>]`; otherwise the instruction is `FROM … [AS name]`. Only used in error messages. */
  readonly keyword?: 'STAGE';
}

/** `FROM image <ref> [AT <dir>]`: start from the files of an OCI site image (a registry or the local store). */
export interface ImageBase {
  readonly kind: 'image';
  readonly ref: Operand;
  /** `AT <dir>`: the directory inside the image to start from (default: the image's `dev.sitefile.root` label). */
  readonly at?: Operand;
}

export type FromBase = 'scratch' | Operand | ImageBase;

/** Whether a FROM base is `image <ref>`. */
export function isImageBase(b: FromBase): b is ImageBase {
  return typeof b === 'object' && 'kind' in b && b.kind === 'image';
}

export interface ArgInstruction {
  readonly kind: 'ARG';
  readonly span: SourceSpan;
  readonly name: string;
  /** Defaults to 'string'. */
  readonly type: ValueType;
  readonly defaultValue?: Operand;
}

export interface LetInstruction {
  readonly kind: 'LET';
  readonly span: SourceSpan;
  readonly name: string;
  readonly type: ValueType;
  readonly value: Operand;
}

export interface ImportInstruction {
  readonly kind: 'IMPORT';
  readonly span: SourceSpan;
  readonly name: string;
  readonly uri: Operand;
  /** Omitted: inferred (local directory -> tree, everything else -> file). */
  readonly as?: ImportKind;
  /** `sha256:<64 lowercase hex>` */
  readonly integrity?: string;
  /** `SORT BY key [ASC|DESC]`, for `AS markdown` collections. */
  readonly sort?: { readonly key: string; readonly descending: boolean; readonly span: SourceSpan };
  /** `SECRET <id> AS HEADER <Name>` / `SECRET <id> AS BEARER` (build secrets, sent with remote requests). */
  readonly secrets?: readonly ImportSecret[];
}

/** A build secret sent as an HTTP request header of a remote IMPORT. */
export interface ImportSecret {
  /** The `--secret id=…` it names. */
  readonly id: string;
  /** Header name as written (`Authorization` for BEARER). */
  readonly header: string;
  /** `AS BEARER`: the header value is `Bearer <secret>`. */
  readonly bearer: boolean;
  readonly span: SourceSpan;
}

/**
 * Content written inline in the Sitefile with a heredoc (`COPY <<CSS /a.css`,
 * `RENDER <<HTML TO /index.html`): the lines after the instruction, up to a line
 * that is exactly the delimiter.
 */
export interface InlineBody {
  readonly kind: 'inline';
  /** The delimiter word, e.g. `HTML`. */
  readonly delimiter: string;
  /** The body, each line ending in `\n` (empty string for an empty body). */
  readonly text: string;
  /** Span of the `<<` marker. */
  readonly span: SourceSpan;
  /** 1-based Sitefile line of the body's first line. */
  readonly bodyLine: number;
}

export function isInline(x: Operand | InlineBody): x is InlineBody {
  return 'kind' in x && x.kind === 'inline';
}

/** `--from=<stage>`: read the source from an earlier stage's final tree. */
export interface StageRef {
  readonly name: string;
  readonly span: SourceSpan;
}

export interface CopyInstruction {
  readonly kind: 'COPY';
  readonly span: SourceSpan;
  readonly source: Operand | InlineBody;
  readonly dest: Operand;
  readonly from?: StageRef;
  /** `--overwrite`: replace files that differ instead of failing. */
  readonly overwrite: boolean;
}

export interface RenderInstruction {
  readonly kind: 'RENDER';
  readonly span: SourceSpan;
  readonly template: Operand | InlineBody;
  readonly dest: Operand;
  readonly from?: StageRef;
  /** `--escape=html|none`, default none. */
  readonly escape: EscapeMode;
  /** `FOR item IN list`: render once per element of an imported list. */
  readonly each?: { readonly as: string; readonly list: string; readonly span: SourceSpan };
  readonly overwrite: boolean;
}

export interface ExportInstruction {
  readonly kind: 'EXPORT';
  readonly span: SourceSpan;
  readonly path: Operand;
  readonly format: ExportFormat;
  /** `BASE <https-url>`: the URL the exported tree is mounted at (required for `wbn`). */
  readonly base?: Operand;
  /** `AT </srv/www>`: where the files live inside the layer (`oci` only). */
  readonly at?: Operand;
  /** `COMPRESSION gzip|none`: how the site layer is stored (`oci` only; default gzip). */
  readonly compression?: { readonly value: OciCompression; readonly span: SourceSpan };
}

/** OCI site layer encoding: `gzip` (tar+gzip, the default) or `none` (plain tar). */
export type OciCompression = 'gzip' | 'none';
export const OCI_COMPRESSIONS: readonly OciCompression[] = ['gzip', 'none'];

/** Parse a compression name (case-insensitive); undefined if unknown. */
export function parseOciCompression(name: string): OciCompression | undefined {
  const n = name.toLowerCase();
  return (OCI_COMPRESSIONS as readonly string[]).includes(n) ? (n as OciCompression) : undefined;
}

export type ExportFormat = 'directory' | 'zip' | 'tar' | 'tgz' | 'wbn' | 'oci';
export const EXPORT_FORMATS: readonly ExportFormat[] = ['directory', 'zip', 'tar', 'tgz', 'wbn', 'oci'];

/** Parse a format name (`tar.gz` is an alias for `tgz`); undefined if unknown. */
export function parseExportFormat(name: string): ExportFormat | undefined {
  const n = name.toLowerCase();
  if (n === 'tar.gz') return 'tgz';
  return (EXPORT_FORMATS as readonly string[]).includes(n) ? (n as ExportFormat) : undefined;
}

/** `PLUGIN name FROM ./plugins/x.mjs [INTEGRITY sha256:…]`: declare a renderer. */
export interface PluginInstruction {
  readonly kind: 'PLUGIN';
  readonly span: SourceSpan;
  readonly name: string;
  readonly uri: Operand;
  readonly integrity?: string;
}

/** `TOOL name`: declare an external program (run only when the CLI allows it with `--allow-exec name`). */
export interface ToolInstruction {
  readonly kind: 'TOOL';
  readonly span: SourceSpan;
  readonly name: string;
}

/** One `key=value` of a TRANSFORM's `OPTIONS` clause. */
export interface TransformOption {
  readonly key: string;
  readonly value: Operand;
  readonly span: SourceSpan;
}

/**
 * `TRANSFORM [--overwrite] [--from=stage] [<source>] WITH <plugin> TO <dest>
 *   [OPTIONS key=value …] [FOR item IN list] [ARGS <argument> …]`
 * ARGS (last on the line) is the argv template when `<plugin>` is a TOOL.
 */
export interface TransformInstruction {
  readonly kind: 'TRANSFORM';
  readonly span: SourceSpan;
  /** Omitted only with FOR: the plugin then gets just the element. */
  readonly source?: Operand | InlineBody;
  readonly from?: StageRef;
  readonly plugin: string;
  readonly pluginSpan: SourceSpan;
  readonly dest: Operand;
  readonly options: readonly TransformOption[];
  readonly each?: { readonly as: string; readonly list: string; readonly span: SourceSpan };
  /** argv template of a TOOL: `{input}`, `{output}` and `{option.key}` are replaced by the host. */
  readonly args?: readonly Operand[];
  readonly overwrite: boolean;
}

/** `SITEMAP [--overwrite] BASE <url> [TO <dest>]`: write a sitemaps.org file listing the stage's `.html` files. */
export interface SitemapInstruction {
  readonly kind: 'SITEMAP';
  readonly span: SourceSpan;
  readonly base: Operand;
  /** Default `/sitemap.xml`. */
  readonly dest?: Operand;
  readonly overwrite: boolean;
}

/**
 * `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]`: an Atom feed
 * from a list import. LINK is evaluated per element, with the element as `${item…}`.
 */
export interface FeedInstruction {
  readonly kind: 'FEED';
  readonly span: SourceSpan;
  readonly list: string;
  readonly listSpan: SourceSpan;
  readonly base: Operand;
  readonly link: Operand;
  /** Default `/feed.xml`. */
  readonly dest?: Operand;
  readonly title?: Operand;
  readonly overwrite: boolean;
}

/** The loop variable LINK of a FEED uses for the current element. */
export const FEED_ITEM = 'item';

/** `REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]`: a line of `/_redirects`. */
export interface RedirectInstruction {
  readonly kind: 'REDIRECT';
  readonly span: SourceSpan;
  readonly from: Operand;
  readonly to: Operand;
  readonly status: RedirectStatus;
  readonly overwrite: boolean;
}

export type RedirectStatus = 301 | 302 | 303 | 307 | 308;
export const REDIRECT_STATUSES: readonly RedirectStatus[] = [301, 302, 303, 307, 308];

/** `HEADER [--overwrite] <path-pattern> <Name>: <value …>`: a header in `/_headers`. */
export interface HeaderInstruction {
  readonly kind: 'HEADER';
  readonly span: SourceSpan;
  readonly pattern: Operand;
  /** The header name without its trailing `:`. */
  readonly name: Operand;
  /** The rest of the line; the words are joined with one space. */
  readonly value: readonly Operand[];
  readonly overwrite: boolean;
}

/** `FINGERPRINT [--overwrite] <pattern>… [MANIFEST <dest>]`: rename matching files to `name.<hash>.ext` and rewrite the HTML and CSS that reference them. */
export interface FingerprintInstruction {
  readonly kind: 'FINGERPRINT';
  readonly span: SourceSpan;
  /** Absolute path patterns (`*`, `?`, `**`; a trailing `/` means everything below). */
  readonly patterns: readonly Operand[];
  /** `MANIFEST <dest>`: where to write the JSON map of old to new paths. */
  readonly manifest?: Operand;
  readonly overwrite: boolean;
}

export type Instruction =
  | FromInstruction
  | ArgInstruction
  | LetInstruction
  | ImportInstruction
  | CopyInstruction
  | RenderInstruction
  | PluginInstruction
  | ToolInstruction
  | TransformInstruction
  | SitemapInstruction
  | FeedInstruction
  | RedirectInstruction
  | HeaderInstruction
  | FingerprintInstruction
  | ExportInstruction;

/** A parsed and semantically validated Sitefile. */
export interface Program {
  readonly file: string;
  readonly instructions: readonly Instruction[];
}

export function literalText(op: Operand | InlineBody): string | undefined {
  if (isInline(op)) return undefined;
  let out = '';
  for (const p of op.parts) {
    if (p.kind !== 'text') return undefined;
    out += p.value;
  }
  return out;
}

/** Names of ARG, LET, IMPORT and stages (`FROM … AS name`, `--from=name`). */
export const IDENT_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** Every stage alias declared anywhere in the file. */
export function stageNames(instructions: readonly Instruction[]): Set<string> {
  const names = new Set<string>();
  for (const ins of instructions) if (ins.kind === 'FROM' && ins.alias !== undefined) names.add(ins.alias);
  return names;
}
