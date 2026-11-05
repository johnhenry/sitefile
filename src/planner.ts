import { SitefileError, type SourceSpan } from './errors.js';
import { isInline, stageNames, type EscapeMode, type ExportFormat, type InlineBody, type ImportKind, type Operand, type Program, type StageRef, type ValueType } from './language/ast.js';
import { normalizeLocal, normalizeVirtual } from './paths.js';
import { coerce, evaluateOperand, type Scalar } from './values.js';

/** Where a COPY/RENDER reads from. */
export type SourceRef =
  /** Content written in the Sitefile itself (heredoc); `bodyLine` is its first Sitefile line. */
  | { readonly kind: 'inline'; readonly text: string; readonly delimiter: string; readonly bodyLine: number; readonly span: SourceSpan }
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
  readonly sort?: { readonly key: string; readonly descending: boolean };
  readonly span: SourceSpan;
}

/** A declared renderer plugin: a local `.mjs` file in its own folder, or a `.wasm` module. */
export interface PluginSpec {
  readonly name: string;
  /** Context-relative path of the module. */
  readonly path: string;
  readonly integrity?: string;
  readonly span: SourceSpan;
}

/** A declared external program; it runs only when the CLI allows it (`--allow-exec`). */
export interface ToolSpec {
  readonly name: string;
  readonly span: SourceSpan;
}

export type Operation =
  | {
      /** Run a plugin; `dest` and option values are interpolated per FOR element at execution. */
      readonly kind: 'TRANSFORM';
      readonly source?: SourceRef;
      readonly plugin: string;
      readonly dest: Operand;
      readonly options: ReadonlyArray<{ readonly key: string; readonly value: Operand }>;
      readonly each?: { readonly as: string; readonly list: string };
      /** argv template when `plugin` names a TOOL. */
      readonly args?: readonly Operand[];
      readonly overwrite: boolean;
      readonly span: SourceSpan;
    }
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
      /** Normalized destination; '' when `each` is set (computed per element). */
      readonly dest: string;
      /** `FOR as IN list`: render once per element; `dest` is interpolated per element. */
      readonly each?: { readonly as: string; readonly list: string; readonly dest: Operand };
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
  readonly plugins: readonly PluginSpec[];
  readonly tools: readonly ToolSpec[];
  readonly exportPath: string;
  readonly exportFormat: ExportFormat;
  /** Evaluated `BASE` clause of EXPORT, if any (raw string; validated when the export is written). */
  readonly exportBase: string | undefined;
  /** Evaluated `AT` clause of EXPORT ... AS oci: the root directory inside the image layer. */
  readonly exportRoot: string | undefined;
  readonly warnings: readonly string[];
}

const URL_RE = /^([A-Za-z][A-Za-z0-9+.-]*):\/\//;
const IMPORT_REF_RE = /^([A-Za-z_][A-Za-z0-9_]*):(.*)$/s;

export function plan(program: Program, buildArgs: Readonly<Record<string, string>> = {}): Plan {
  const vars = new Map<string, Scalar>();
  const imports = new Map<string, ImportSpec>();
  const stages: Stage[] = [];
  const plugins: PluginSpec[] = [];
  const tools: ToolSpec[] = [];
  const warnings: string[] = [];
  const declaredArgs = new Set<string>();
  const allStageNames = stageNames(program.instructions);
  // validate() guarantees a FROM precedes every operation.
  const operations = (): Operation[] => stages[stages.length - 1]!.operations;
  let exportPath: string | undefined;
  let exportFormat: ExportFormat = 'directory';
  let exportBase: string | undefined;
  let exportRoot: string | undefined;

  const bind = (name: string, raw: string, type: ValueType, span: SourceSpan, what: string): void => {
    vars.set(name, coerce(raw, type, span, what));
  };

  for (const ins of program.instructions) {
    switch (ins.kind) {
      case 'FROM':
        stages.push({
          ...(ins.alias !== undefined && { name: ins.alias }),
          base: ins.base === 'scratch' ? { kind: 'scratch' } : stageBase(ins.base, vars, stages, allStageNames, ins.keyword === 'STAGE' ? `STAGE ${ins.alias} FROM` : 'FROM'),
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
          ...(ins.sort ? { sort: { key: ins.sort.key, descending: ins.sort.descending } } : {}),
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
        if (isInline(ins.source) && dest.trailingSlash) {
          throw new SitefileError('E_PATH', `COPY of an inline body needs a file destination, not a directory`, ins.dest.span);
        }
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
        if (ins.each) {
          operations().push({
            kind: 'RENDER',
            template: sourceRef(ins.template, vars, imports, ins.from),
            dest: '',
            each: { as: ins.each.as, list: ins.each.list, dest: ins.dest },
            escape: ins.escape,
            overwrite: ins.overwrite,
            span: ins.span,
          });
          break;
        }
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
      case 'PLUGIN': {
        const raw = evaluateOperand(ins.uri, vars);
        if (URL_RE.test(raw)) {
          throw new SitefileError('E_PROTOCOL', `PLUGIN ${ins.name}: plugins must be local files in the build context, not URLs`, ins.uri.span);
        }
        const rel = normalizeLocal(raw, ins.uri.span).path;
        if (!/\.(mjs|wasm)$/.test(rel)) {
          throw new SitefileError('E_VALIDATION', `PLUGIN ${ins.name}: '${raw}' must be an ES module (.mjs) or a WebAssembly module (.wasm)`, ins.uri.span);
        }
        plugins.push({ name: ins.name, path: rel, ...(ins.integrity ? { integrity: ins.integrity } : {}), span: ins.span });
        break;
      }
      case 'TOOL':
        tools.push({ name: ins.name, span: ins.span });
        break;
      case 'TRANSFORM':
        operations().push({
          kind: 'TRANSFORM',
          ...(ins.source ? { source: sourceRef(ins.source, vars, imports, ins.from) } : {}),
          plugin: ins.plugin,
          dest: ins.dest,
          options: ins.options.map((o) => ({ key: o.key, value: o.value })),
          ...(ins.each ? { each: { as: ins.each.as, list: ins.each.list } } : {}),
          ...(ins.args ? { args: ins.args } : {}),
          overwrite: ins.overwrite,
          span: ins.span,
        });
        break;
      case 'EXPORT':
        exportPath = normalizeVirtual(evaluateOperand(ins.path, vars), ins.path.span, 'EXPORT path').path;
        exportFormat = ins.format;
        exportBase = ins.base ? evaluateOperand(ins.base, vars) : undefined;
        if (ins.at && ins.format !== 'oci') throw new SitefileError('E_VALIDATION', 'EXPORT ... AT applies only to AS oci', ins.at.span);
        exportRoot = ins.at ? normalizeVirtual(evaluateOperand(ins.at, vars), ins.at.span, 'EXPORT AT path').path : undefined;
        break;
    }
  }

  // An unnamed stage can be neither a FROM base nor a COPY --from source; only the last one is useful (EXPORT reads it).
  for (const st of stages.slice(0, -1)) {
    if (st.name === undefined) {
      warnings.push(`${program.file}:${st.span.line}: unnamed stage is not the last one, so its output can never be used; name it with FROM … AS <name>`);
    }
  }
  for (const name of Object.keys(buildArgs)) {
    if (!declaredArgs.has(name)) warnings.push(`--build-arg ${name} does not match any ARG and was ignored`);
  }
  if (stages.length === 0 || exportPath === undefined) {
    // validate() guarantees both; this keeps the types honest.
    throw new SitefileError('E_VALIDATION', 'a Sitefile needs FROM and EXPORT');
  }
  return { file: program.file, stages, vars, imports: [...imports.values()], plugins, tools, exportPath, exportFormat, exportBase, exportRoot, warnings };
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
function stageBase(op: Operand, vars: ReadonlyMap<string, Scalar>, earlier: readonly Stage[], all: ReadonlySet<string>, lead = 'FROM'): StageBase {
  const raw = evaluateOperand(op, vars);
  if (earlier.some((s) => s.name === raw)) return { kind: 'stage', name: raw };
  if (all.has(raw)) {
    throw new SitefileError('E_UNDEFINED', `${lead} ${raw}: stage '${raw}' is not defined before this line (write ./${raw} for a local directory)`, op.span);
  }
  return { kind: 'local', path: normalizeLocal(raw, op.span).path };
}

function sourceRef(
  op: Operand | InlineBody,
  vars: ReadonlyMap<string, Scalar>,
  imports: ReadonlyMap<string, ImportSpec>,
  from?: StageRef,
): SourceRef {
  if (isInline(op)) return { kind: 'inline', text: op.text, delimiter: op.delimiter, bodyLine: op.bodyLine, span: op.span };
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
