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
   * `scratch` keyword, or an operand: an earlier stage name or a context-relative
   * directory (the planner decides, see docs/LANGUAGE.md).
   */
  readonly base: 'scratch' | Operand;
  readonly alias?: string;
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
}

export type ExportFormat = 'directory' | 'zip' | 'tar' | 'tgz' | 'wbn' | 'oci';
export const EXPORT_FORMATS: readonly ExportFormat[] = ['directory', 'zip', 'tar', 'tgz', 'wbn', 'oci'];

/** Parse a format name (`tar.gz` is an alias for `tgz`); undefined if unknown. */
export function parseExportFormat(name: string): ExportFormat | undefined {
  const n = name.toLowerCase();
  if (n === 'tar.gz') return 'tgz';
  return (EXPORT_FORMATS as readonly string[]).includes(n) ? (n as ExportFormat) : undefined;
}

export type Instruction =
  | FromInstruction
  | ArgInstruction
  | LetInstruction
  | ImportInstruction
  | CopyInstruction
  | RenderInstruction
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
