import { SitefileError, type SourceSpan } from './errors.js';
import type { EscapeMode, ImportKind, Operand, Program, StageRef, ValueType } from './language/ast.js';
import { normalizeLocal, normalizeVirtual } from './paths.js';
import { coerce, evaluateOperand, type Scalar } from './values.js';

/** Where a COPY/RENDER reads from. */
export type SourceRef =
  /** `path` is a normalized virtual path inside an earlier stage's final tree (`--from=name`). */
  | { readonly kind: 'stage'; readonly name: string; readonly path: string; readonly span: SourceSpan }
  | { readonly kind: 'local'; readonly path: string; readonly span: SourceSpan }
  /** `path` is a normalized virtual path inside the import, `/` for the whole import. */
  | { readonly kind: 'import'; readonly name: string; readonly path: string; readonly span: SourceSpan };

export interface ImportSpec {
  readonly name: string;
  /** Interpolated URI as written. */
  readonly uri: string;
  readonly location: { readonly kind: 'local'; readonly path: string } | { readonly kind: 'remote'; readonly url: string };
  readonly as?: ImportKind;
  readonly integrity?: string;
  readonly span: SourceSpan;
}

export type Operation =
  | {
      readonly kind: 'COPY';
      readonly source: SourceRef;
      readonly dest: string;
      readonly destIsDir: boolean;
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
  | {
      readonly kind: 'RENDER';
      readonly template: SourceRef;
      readonly dest: string;
      readonly escape: EscapeMode;
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    };

/** What a stage's tree starts as. */
export type StageBase =
  | { readonly kind: 'scratch' }
  /** `path` is context-relative (`''` for the context root). */
  | { readonly kind: 'local'; readonly path: string }
  /** A copy of an earlier stage's final tree. */
  | { readonly kind: 'stage'; readonly name: string };

/** One FROM and the operations that follow it. */
export interface Stage {
  readonly name?: string;
  readonly base: StageBase;
  readonly operations: Operation[];
  readonly span: SourceSpan;
}

/** A fully evaluated build: no variables left, no IO done yet. */
export interface Plan {
  readonly file: string;
  /** In source order; EXPORT reads the last one. */
  readonly stages: readonly Stage[];
  readonly vars: ReadonlyMap<string, Scalar>;
  readonly imports: readonly ImportSpec[];
  readonly exportPath: string;
  readonly warnings: readonly string[];
}

const URL_RE = /^([A-Za-z][A-Za-z0-9+.-]*):\/\//;
const IMPORT_REF_RE = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/s;

export function plan(program: Program, buildArgs: Readonly<Record<string, string>> = {}): Plan {
  const vars = new Map<string, Scalar>();
  const imports = new Map<string, ImportSpec>();
  const stages: Stage[] = [];
  const warnings: string[] = [];
  const declaredArgs = new Set<string>();
  const allStageNames = new Set<string>();
  for (const ins of program.instructions) if (ins.kind === 'FROM' && ins.alias !== undefined) allStageNames.add(ins.alias);
  // validate() guarantees a FROM precedes every operation.
  const operations = (): Operation[] => stages[stages.length - 1]!.operations;
  let exportPath: string | undefined;

  const bind = (name: string, raw: string, type: ValueType, span: SourceSpan, what: string): void => {
    vars.set(name, coerce(raw, type, span, what));
  };

  for (const ins of program.instructions) {
    switch (ins.kind) {
      case 'FROM':
        stages.push({
          ...(ins.alias !== undefined && { name: ins.alias }),
          base: ins.base === 'scratch' ? { kind: 'scratch' } : stageBase(ins.base, vars, stages, allStageNames),
          operations: [],
          span: ins.span,
        });
        break;
      case 'ARG': {
        declaredArgs.add(ins.name);
        const given = buildArgs[ins.name];
        if (given !== undefined) bind(ins.name, given, ins.type, ins.span, `--build-arg ${ins.name}`);
        else if (ins.defaultValue) bind(ins.name, evaluateOperand(ins.defaultValue, vars), ins.type, ins.defaultValue.span, `ARG ${ins.name}`);
        else throw new SitefileError('E_MISSING_ARG', `ARG ${ins.name} has no default; pass --build-arg ${ins.name}=<value>`, ins.span);
        break;
      }
      case 'LET':
        bind(ins.name, evaluateOperand(ins.value, vars), ins.type, ins.value.span, `LET ${ins.name}`);
        break;
      case 'IMPORT': {
        const uri = evaluateOperand(ins.uri, vars);
        const spec: ImportSpec = {
          name: ins.name,
          uri,
          location: classifyUri(uri, ins.uri.span),
          ...(ins.as ? { as: ins.as } : {}),
          ...(ins.integrity ? { integrity: ins.integrity } : {}),
          span: ins.span,
        };
        if (spec.location.kind === 'remote' && spec.as === 'tree') {
          throw new SitefileError('E_UNSUPPORTED', 'remote tree imports arrive in M2', ins.span);
        }
        if (spec.location.kind === 'local' && spec.integrity) {
          warnings.push(`${program.file}:${ins.span.line}: INTEGRITY on a local import is checked but not recorded in the lockfile`);
        }
        imports.set(ins.name, spec);
        break;
      }
      case 'COPY': {
        const dest = normalizeVirtual(evaluateOperand(ins.dest, vars), ins.dest.span, 'COPY destination');
        operations().push({
          kind: 'COPY',
          source: sourceRef(ins.source, vars, imports, ins.from),
          dest: dest.path,
          destIsDir: dest.trailingSlash,
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      }
      case 'RENDER': {
        const raw = evaluateOperand(ins.dest, vars);
        const dest = normalizeVirtual(raw, ins.dest.span, 'RENDER destination');
        if (dest.trailingSlash) {
          throw new SitefileError('E_PATH', `RENDER destination '${raw}' must be a file path, not a directory`, ins.dest.span);
        }
        operations().push({
          kind: 'RENDER',
          template: sourceRef(ins.template, vars, imports, ins.from),
          dest: dest.path,
          escape: ins.escape,
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      }
      case 'EXPORT':
        exportPath = normalizeVirtual(evaluateOperand(ins.path, vars), ins.path.span, 'EXPORT path').path;
        break;
    }
  }

  for (const name of Object.keys(buildArgs)) {
    if (!declaredArgs.has(name)) warnings.push(`--build-arg ${name} does not match any ARG and was ignored`);
  }
  if (stages.length === 0 || exportPath === undefined) {
    // validate() guarantees both; this keeps the types honest.
    throw new SitefileError('E_VALIDATION', 'a Sitefile needs FROM and EXPORT');
  }
  return { file: program.file, stages, vars, imports: [...imports.values()], exportPath, warnings };
}

function classifyUri(uri: string, span: SourceSpan): ImportSpec['location'] {
  const m = URL_RE.exec(uri);
  if (m) {
    const scheme = m[1]!.toLowerCase();
    if (scheme === 'https' || scheme === 'http') return { kind: 'remote', url: uri };
    if (scheme === 'file') {
      throw new SitefileError('E_PROTOCOL', `file: URLs are not allowed; use a path relative to the build context`, span);
    }
    throw new SitefileError('E_PROTOCOL', `unsupported protocol '${scheme}:' (supported: https, relative paths)`, span);
  }
  return { kind: 'local', path: normalizeLocal(uri, span).path };
}

/** A bare identifier naming an earlier stage is a stage; anything else is a context-relative directory. */
function stageBase(op: Operand, vars: ReadonlyMap<string, Scalar>, earlier: readonly Stage[], all: ReadonlySet<string>): StageBase {
  const raw = evaluateOperand(op, vars);
  if (earlier.some((s) => s.name === raw)) return { kind: 'stage', name: raw };
  if (all.has(raw)) {
    throw new SitefileError('E_UNDEFINED', `FROM ${raw}: stage '${raw}' is not defined before this line (write ./${raw} for a local directory)`, op.span);
  }
  return { kind: 'local', path: normalizeLocal(raw, op.span).path };
}

function sourceRef(
  op: Operand,
  vars: ReadonlyMap<string, Scalar>,
  imports: ReadonlyMap<string, ImportSpec>,
  from?: StageRef,
): SourceRef {
  const raw = evaluateOperand(op, vars);
  if (from) return { kind: 'stage', name: from.name, path: normalizeVirtual(raw, op.span, `path inside stage '${from.name}'`).path, span: op.span };
  if (URL_RE.test(raw)) {
    throw new SitefileError('E_VALIDATION', `cannot read '${raw}' directly; IMPORT it under a name, then COPY name:`, op.span);
  }
  const m = IMPORT_REF_RE.exec(raw);
  if (m) {
    const name = m[1]!;
    const inner = m[2]!;
    if (!imports.has(name)) {
      throw new SitefileError('E_UNDEFINED', `'${name}:' does not name an IMPORT; write './${raw}' for a local file`, op.span);
    }
    const path = inner === '' ? '/' : normalizeVirtual(inner, op.span, `path inside import '${name}'`).path;
    return { kind: 'import', name, path, span: op.span };
  }
  return { kind: 'local', path: normalizeLocal(raw, op.span).path, span: op.span };
}
