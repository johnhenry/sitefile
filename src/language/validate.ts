import { SitefileError } from '../errors.js';
import { FEED_ITEM, IDENT_RE, isImageBase, isInline, literalText, stageNames, type InlineBody, type Instruction, type Operand, type Program, type StageRef } from './ast.js';
import { parseSitefile } from './parser.js';

/** Import kinds that can produce a list (and so can be sorted). */
const SORTABLE = new Set(['markdown', 'json', 'yaml', 'csv']);

const IMPORT_REF = /^([A-Za-z_][A-Za-z0-9_]*):/;

export function operandsOf(ins: Instruction): Operand[] {
  switch (ins.kind) {
    case 'FROM':
      return ins.base === 'scratch' ? [] : isImageBase(ins.base) ? [ins.base.ref, ...(ins.base.at ? [ins.base.at] : [])] : [ins.base];
    case 'ARG':
      return ins.defaultValue ? [ins.defaultValue] : [];
    case 'LET':
      return [ins.value];
    case 'IMPORT':
      return ins.schema ? [ins.uri, ins.schema.path] : [ins.uri];
    case 'COPY':
      return isInline(ins.source) ? [ins.dest] : [ins.source, ins.dest];
    case 'RENDER':
      return isInline(ins.template) ? [ins.dest] : [ins.template, ins.dest];
    case 'PLUGIN':
      return [ins.uri];
    case 'TRANSFORM': {
      const ops: Operand[] = ins.source && !isInline(ins.source) ? [ins.source] : [];
      return [...ops, ins.dest, ...ins.options.map((o) => o.value), ...(ins.args ?? [])];
    }
    case 'TOOL':
      return [];
    case 'SITEMAP':
      return [ins.base, ...(ins.dest ? [ins.dest] : [])];
    case 'FEED':
      return [ins.base, ins.link, ...(ins.dest ? [ins.dest] : []), ...(ins.title ? [ins.title] : [])];
    case 'REDIRECT':
      return [ins.from, ins.to];
    case 'HEADER':
      return [ins.pattern, ins.name, ...ins.value];
    case 'FINGERPRINT':
      return [...ins.patterns, ...(ins.manifest ? [ins.manifest] : [])];
    case 'EXPORT':
      return [ins.path];
  }
}

/** The only placeholders the host replaces inside TOOL arguments; any other `{…}` stays literal (e.g. ffmpeg's `%{pts}`). */
export const TOOL_PLACEHOLDER_RE = /\{(input|output|option\.[A-Za-z_][A-Za-z0-9_-]*)\}/g;

function checkToolArgs(ins: Extract<Instruction, { kind: 'TRANSFORM' }>): void {
  if (!ins.args || ins.args.length === 0) {
    throw new SitefileError('E_VALIDATION', `TRANSFORM … WITH ${ins.plugin}: a TOOL needs ARGS (the argument list, e.g. ARGS -i {input} {output})`, ins.span);
  }
  let usesOutput = false;
  for (const arg of ins.args) {
    for (const part of arg.parts) {
      if (part.kind !== 'text') continue;
      for (const m of part.value.matchAll(TOOL_PLACEHOLDER_RE)) {
        const name = m[1]!;
        if (name === 'output') usesOutput = true;
        else if (name === 'input') {
          if (!ins.source) throw new SitefileError('E_VALIDATION', `{input} needs a TRANSFORM source file`, arg.span);
        } else if (!ins.options.some((o) => `option.${o.key}` === name)) {
          throw new SitefileError('E_UNDEFINED', `{${name}}: no OPTIONS ${name.slice(7)}=… on this TRANSFORM`, arg.span);
        }
      }
    }
  }
  if (!usesOutput) throw new SitefileError('E_VALIDATION', `TRANSFORM … WITH ${ins.plugin}: ARGS must contain {output}, the file the tool writes`, ins.span);
}

/** Check semantic rules (see docs/LANGUAGE.md, "v0.1 implementation decisions"). */
export function validate(instructions: readonly Instruction[], file: string): Program {
  if (instructions.length === 0) throw new SitefileError('E_VALIDATION', 'Sitefile has no instructions', { file, line: 1, column: 1 });

  const vars = new Set<string>();
  const imports = new Set<string>();
  // Stage names: every alias in the file, those already declared, and those whose stage is complete.
  const allStages = stageNames(instructions);
  const declaredStages = new Set<string>();
  const finishedStages = new Set<string>();
  let currentStage: string | undefined;
  const plugins = new Set<string>();
  const tools = new Set<string>();
  /** One namespace for ARG/LET, IMPORT, stage and PLUGIN names. */
  const taken = (name: string): boolean => vars.has(name) || imports.has(name) || declaredStages.has(name) || plugins.has(name) || tools.has(name);
  let sawFrom = false;
  let sawExport = false;
  const out: Instruction[] = [];

  for (const ins of instructions) {
    // The first instruction other than ARG, with no FROM yet, starts an implicit unnamed `FROM scratch`
    // stage. It has no source line of its own, so it takes that instruction's span.
    if (!sawFrom && ins.kind !== 'FROM' && ins.kind !== 'ARG') {
      out.push({ kind: 'FROM', span: ins.span, base: 'scratch' });
      sawFrom = true;
    }
    out.push(ins);
    if (sawExport) throw new SitefileError('E_VALIDATION', 'EXPORT must be the last instruction', ins.span);

    // RENDER … FOR item IN list: the destination may use ${item} / ${item.key}.
    // RENDER/TRANSFORM … FOR item IN list: the destination (and TRANSFORM options) may use ${item.key}.
    // FEED … LINK <path>: the link may use ${item.key} for the current element.
    const loopVar = ins.kind === 'RENDER' || ins.kind === 'TRANSFORM' ? ins.each?.as : ins.kind === 'FEED' ? FEED_ITEM : undefined;
    for (const op of operandsOf(ins)) {
      const isDest =
        (ins.kind === 'FEED' && op === ins.link) ||
        (ins.kind === 'RENDER' && op === ins.dest) ||
        (ins.kind === 'TRANSFORM' && (op === ins.dest || ins.options.some((o) => o.value === op) || (ins.args?.includes(op) ?? false)));
      for (const part of op.parts) {
        if (part.kind !== 'var') continue;
        if (isDest && part.name === loopVar) continue;
        if (part.path.length > 0) {
          throw new SitefileError(
            'E_SYNTAX',
            `dotted access \${${[part.name, ...part.path].join('.')}} is only allowed in RENDER templates and for a RENDER … FOR loop variable`,
            part.span,
          );
        }
        if (vars.has(part.name)) continue;
        const hint = imports.has(part.name) ? ` ('${part.name}' is an import; use it in a RENDER template)` : '';
        throw new SitefileError('E_UNDEFINED', `undefined variable \${${part.name}}: declare it with ARG or LET on an earlier line${hint}`, part.span);
      }
    }

    if (ins.kind === 'FROM') {
      if (currentStage !== undefined) finishedStages.add(currentStage);
      if (ins.base !== 'scratch' && !isImageBase(ins.base)) {
        // A bare word naming a stage must name an earlier one; './name' always means a directory.
        const text = literalText(ins.base);
        if (text !== undefined && IDENT_RE.test(text) && allStages.has(text) && !finishedStages.has(text)) {
          throw new SitefileError(
            'E_UNDEFINED',
            `${ins.keyword === 'STAGE' ? `STAGE ${ins.alias} FROM` : 'FROM'} ${text}: stage '${text}' is not defined before this line (write ./${text} for a local directory)`,
            ins.base.span,
          );
        }
      }
      if (ins.alias !== undefined) {
        if (taken(ins.alias)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.alias}' is already defined`, ins.span);
        }
        declaredStages.add(ins.alias);
      }
      currentStage = ins.alias;
      sawFrom = true;
      continue;
    }
    switch (ins.kind) {
      case 'ARG':
      case 'LET':
        if (taken(ins.name)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        }
        vars.add(ins.name);
        break;
      case 'IMPORT':
        if (taken(ins.name)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        }
        if (ins.sort && !SORTABLE.has(ins.as ?? '')) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: SORT BY applies to lists: AS markdown, json, yaml or csv`, ins.sort.span);
        }
        if (ins.schema && !SORTABLE.has(ins.as ?? '')) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: SCHEMA applies to documents and lists: AS markdown, json, yaml or csv`, ins.schema.span);
        }
        imports.add(ins.name);
        break;
      case 'COPY':
        checkSource(ins.source, imports, ins.from, finishedStages, allStages);
        break;
      case 'RENDER':
        checkSource(ins.template, imports, ins.from, finishedStages, allStages);
        if (ins.each) {
          const { as, list, span } = ins.each;
          if (!imports.has(list)) {
            throw new SitefileError('E_UNDEFINED', `FOR ${as} IN ${list}: '${list}' is not an IMPORT declared on an earlier line`, span);
          }
          if (taken(as) || allStages.has(as)) {
            throw new SitefileError('E_DUPLICATE', `FOR ${as}: '${as}' is already defined; pick another loop variable name`, span);
          }
        }
        break;
      case 'PLUGIN':
        if (taken(ins.name)) throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        plugins.add(ins.name);
        break;
      case 'TOOL':
        if (taken(ins.name)) throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        tools.add(ins.name);
        break;
      case 'TRANSFORM':
        if (tools.has(ins.plugin)) checkToolArgs(ins);
        else if (!plugins.has(ins.plugin)) {
          throw new SitefileError('E_UNDEFINED', `TRANSFORM … WITH ${ins.plugin}: no PLUGIN or TOOL ${ins.plugin} is declared on an earlier line`, ins.pluginSpan);
        } else if (ins.args) {
          throw new SitefileError('E_VALIDATION', `ARGS applies to a TOOL; ${ins.plugin} is a PLUGIN (pass values with OPTIONS)`, ins.span);
        }
        // With no source and no FOR the plugin gets only its OPTIONS (e.g. a badge or a generated file).
        if (ins.source) checkSource(ins.source, imports, ins.from, finishedStages, allStages);
        if (ins.each) {
          const { as, list, span } = ins.each;
          if (!imports.has(list)) {
            throw new SitefileError('E_UNDEFINED', `FOR ${as} IN ${list}: '${list}' is not an IMPORT declared on an earlier line`, span);
          }
          if (taken(as)) throw new SitefileError('E_DUPLICATE', `FOR ${as}: '${as}' is already defined; pick another loop variable name`, span);
        }
        break;
      case 'FEED':
        if (!imports.has(ins.list)) {
          throw new SitefileError('E_UNDEFINED', `FEED ${ins.list}: '${ins.list}' is not an IMPORT declared on an earlier line`, ins.listSpan);
        }
        break;
      case 'SITEMAP':
      case 'REDIRECT':
      case 'HEADER':
      case 'FINGERPRINT':
        break;
      case 'EXPORT':
        sawExport = true;
        break;
    }
  }

  const last = instructions[instructions.length - 1]!;
  if (!sawExport) throw new SitefileError('E_VALIDATION', 'missing EXPORT: the last instruction must be EXPORT', last.span);
  return { file, instructions: out };
}

function checkSource(
  op: Operand | InlineBody,
  imports: ReadonlySet<string>,
  from: StageRef | undefined,
  finished: ReadonlySet<string>,
  all: ReadonlySet<string>,
): void {
  if (isInline(op)) return; // content is in the Sitefile itself
  if (from) {
    // The source is a path inside the stage, not an import ref or local path.
    if (!finished.has(from.name)) {
      const why = all.has(from.name) ? 'is not complete before this line (a stage cannot read itself or a later stage)' : 'is not defined';
      throw new SitefileError('E_UNDEFINED', `--from=${from.name}: stage '${from.name}' ${why}`, from.span);
    }
    return;
  }
  const text = literalText(op);
  if (text === undefined) return;
  if (text.includes('://')) {
    throw new SitefileError('E_VALIDATION', 'URL sources are not allowed here: use IMPORT then COPY', op.span);
  }
  const m = IMPORT_REF.exec(text);
  if (m && !imports.has(m[1]!)) {
    throw new SitefileError('E_UNDEFINED', `unknown import '${m[1]}' (declare it with IMPORT on an earlier line, or write ./${text} for a local path)`, op.span);
  }
}

/** Parse and validate a Sitefile. */
export function parseProgram(source: string, file = 'Sitefile'): Program {
  return validate(parseSitefile(source, file), file);
}
