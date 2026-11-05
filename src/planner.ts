import { SitefileError, type SourceSpan } from './errors.js';
import type { EscapeMode, ImportKind, Operand, Program, ValueType } from './language/ast.js';
import { normalizeLocal, normalizeVirtual } from './paths.js';
import { coerce, evaluateOperand, type Scalar } from './values.js';

/** Where a COPY/RENDER reads from. */
export type SourceRef =
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

/** A fully evaluated build: no variables left, no IO done yet. */
export interface Plan {
  readonly file: string;
  readonly stage: { readonly base: 'scratch'; readonly alias?: string };
  readonly vars: ReadonlyMap<string, Scalar>;
  readonly imports: readonly ImportSpec[];
  readonly operations: readonly Operation[];
  readonly exportPath: string;
  readonly warnings: readonly string[];
}

const URL_RE = /^([A-Za-z][A-Za-z0-9+.-]*):\/\//;
const IMPORT_REF_RE = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/s;

export function plan(program: Program, buildArgs: Readonly<Record<string, string>> = {}): Plan {
  const vars = new Map<string, Scalar>();
  const imports = new Map<string, ImportSpec>();
  const operations: Operation[] = [];
  const warnings: string[] = [];
  const declaredArgs = new Set<string>();
  let stage: Plan['stage'] | undefined;
  let exportPath: string | undefined;

  const bind = (name: string, raw: string, type: ValueType, span: SourceSpan, what: string): void => {
    vars.set(name, coerce(raw, type, span, what));
  };

  for (const ins of program.instructions) {
    switch (ins.kind) {
      case 'FROM':
        if (ins.base !== 'scratch') throw new SitefileError('E_UNSUPPORTED', 'only FROM scratch is supported until M2', ins.span);
        stage = ins.alias === undefined ? { base: 'scratch' } : { base: 'scratch', alias: ins.alias };
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
        if (spec.location.kind === 'local' && spec.integrity) {
          warnings.push(`${program.file}:${ins.span.line}: INTEGRITY on a local import is checked but not recorded in the lockfile`);
        }
        imports.set(ins.name, spec);
        break;
      }
      case 'COPY': {
        const dest = normalizeVirtual(evaluateOperand(ins.dest, vars), ins.dest.span, 'COPY destination');
        operations.push({
          kind: 'COPY',
          source: sourceRef(ins.source, vars, imports),
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
        operations.push({
          kind: 'RENDER',
          template: sourceRef(ins.template, vars, imports),
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
  if (!stage || exportPath === undefined) {
    // validate() guarantees both; this keeps the types honest.
    throw new SitefileError('E_VALIDATION', 'a Sitefile needs FROM and EXPORT');
  }
  return { file: program.file, stage, vars, imports: [...imports.values()], operations, exportPath, warnings };
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

function sourceRef(op: Operand, vars: ReadonlyMap<string, Scalar>, imports: ReadonlyMap<string, ImportSpec>): SourceRef {
  const raw = evaluateOperand(op, vars);
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
