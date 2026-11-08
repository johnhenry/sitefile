import { SitefileError, type SourceSpan } from '../errors.js';
import { FEED_ITEM, IDENT_RE, importedStage, isFragment, isImageBase, isInline, literalText, stageNames, type FragmentBody, type FragmentInstruction, type InlineBody, type Instruction, type Operand, type Program, type StageRef } from './ast.js';
import { parseSitefile } from './parser.js';
import { fragmentNotValue, freeRefs, parseRenderTemplate, type TemplateNode } from '../render.js';

/** A fragment's template: a heredoc body or partial file without its final newline (it is inserted inline). */
export function fragmentText(text: string): string {
  return text.replace(/\r?\n$/, '');
}

/** Parse the template of an html LET (errors point into the Sitefile). */
export function parseFragmentBody(body: FragmentBody, file: string): TemplateNode[] {
  return body.kind === 'line' ? parseRenderTemplate(body.text, file, body.span.line, body.span.column) : parseRenderTemplate(fragmentText(body.text), file, body.bodyLine);
}

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
      return isFragment(ins) ? [] : [ins.value];
    case 'IMPORT':
      return [ins.uri, ...(ins.schema ? [ins.schema.path] : []), ...(ins.drafts ? [ins.drafts.value] : []), ...(ins.args ?? []).map((a) => a.value)];
    case 'COPY':
      return isInline(ins.source) ? [ins.dest] : [ins.source, ins.dest];
    case 'RENDER': {
      const page = ins.each?.page;
      const extra = page ? [page.size, ...(page.first ? [page.first] : [])] : [];
      return [...(isInline(ins.template) ? [] : [ins.template]), ins.dest, ...extra];
    }
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
    case 'ROBOTS':
      return [...ins.rules.map((r) => r.path), ...(ins.dest ? [ins.dest] : [])];
    case 'SECURITY_TXT':
      return [...ins.contact, ins.expires, ...ins.encryption, ...ins.policy, ...(ins.dest ? [ins.dest] : [])];
    case 'CHECKLINKS':
      return [];
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
  /** html LETs and `IMPORT … AS html`: usable only inside RENDER templates. */
  const fragments = new Set<string>();
  /** IMPORTs `AS sitefile`: `<name>+<stage>` names a stage of the imported Sitefile. */
  const sitefiles = new Set<string>();
  /** One namespace for ARG/LET, IMPORT, stage and PLUGIN names. */
  const taken = (name: string): boolean => vars.has(name) || imports.has(name) || declaredStages.has(name) || plugins.has(name) || tools.has(name) || fragments.has(name);
  // Where each ARG, LET and IMPORT name is first declared (a fragment may only use names declared before it).
  const declaredAt = new Map<string, number>();
  for (const ins of instructions) {
    if ((ins.kind === 'ARG' || ins.kind === 'LET' || ins.kind === 'IMPORT') && !declaredAt.has(ins.name)) declaredAt.set(ins.name, ins.span.line);
  }
  /** Not a value a template can read. */
  const notValue = (name: string): boolean => declaredStages.has(name) || plugins.has(name) || tools.has(name);
  let sawFrom = false;
  let sawExport = false;
  const out: Instruction[] = [];

  for (const ins of instructions) {
    // The first instruction other than ARG, with no FROM yet, starts an implicit unnamed `FROM scratch`
    // stage. It has no source line of its own, so it takes that instruction's span.
    // An IMPORT … AS sitefile may come first too, so a file can start FROM one of its stages.
    if (!sawFrom && ins.kind !== 'FROM' && ins.kind !== 'ARG' && !(ins.kind === 'IMPORT' && ins.as === 'sitefile')) {
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
        (ins.kind === 'RENDER' && (op === ins.dest || op === ins.each?.page?.first)) ||
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
        if (fragments.has(part.name)) {
          throw new SitefileError('E_TYPE', `'${part.name}' is an html fragment; html values can only be used in RENDER templates`, part.span);
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
        const imported = text === undefined ? undefined : importedStage(text);
        // `<import>+<stage>` of an IMPORT … AS sitefile: whether the stage exists is checked when that Sitefile is read.
        if (!(imported && sitefiles.has(imported.import)) && text !== undefined && IDENT_RE.test(text) && allStages.has(text) && !finishedStages.has(text)) {
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
        if (isFragment(ins)) {
          checkFragment(ins, file, { vars, imports, fragments, notValue, declaredAt });
          fragments.add(ins.name);
        } else {
          vars.add(ins.name);
        }
        break;
      case 'IMPORT':
        if (taken(ins.name)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        }
        if (ins.sort && !SORTABLE.has(ins.as ?? '')) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: SORT BY applies to lists: AS markdown, json, yaml or csv`, ins.sort.span);
        }
        if (ins.drafts && !SORTABLE.has(ins.as ?? '')) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: DRAFTS applies to lists: AS markdown, json, yaml or csv`, ins.drafts.span);
        }
        if (ins.schema && !SORTABLE.has(ins.as ?? '')) {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: SCHEMA applies to documents and lists: AS markdown, json, yaml or csv`, ins.schema.span);
        }
        // `AS html`: a file-based fragment, usable only in templates (still an import for `name:` sources).
        if (ins.as === 'html') fragments.add(ins.name);
        if (ins.args && ins.as !== 'sitefile') {
          throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: ARG sets a build arg of an imported Sitefile, so it needs AS sitefile`, ins.args[0]!.span);
        }
        if (ins.as === 'sitefile') {
          if (ins.integrity) {
            throw new SitefileError('E_VALIDATION', `IMPORT ${ins.name}: INTEGRITY does not apply to AS sitefile (pin a Git import by its commit, #<40 hex>[:path])`, ins.span);
          }
          sitefiles.add(ins.name);
        }
        imports.add(ins.name);
        break;
      case 'COPY':
        checkSource(ins.source, imports, ins.from, finishedStages, allStages, sitefiles);
        break;
      case 'RENDER':
        checkSource(ins.template, imports, ins.from, finishedStages, allStages, sitefiles);
        if (ins.each) {
          const { as, list, span } = ins.each;
          if (fragments.has(list)) throw notAList(as, list, span);
          if (!imports.has(list)) {
            throw new SitefileError('E_UNDEFINED', `FOR ${as} IN ${list}: '${list}' is not an IMPORT declared on an earlier line`, span);
          }
          notSitefile(list, `FOR ${as} IN ${list}`, span, sitefiles);
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
        if (ins.source) checkSource(ins.source, imports, ins.from, finishedStages, allStages, sitefiles);
        if (ins.each) {
          const { as, list, span } = ins.each;
          if (fragments.has(list)) throw notAList(as, list, span);
          if (!imports.has(list)) {
            throw new SitefileError('E_UNDEFINED', `FOR ${as} IN ${list}: '${list}' is not an IMPORT declared on an earlier line`, span);
          }
          notSitefile(list, `FOR ${as} IN ${list}`, span, sitefiles);
          if (taken(as)) throw new SitefileError('E_DUPLICATE', `FOR ${as}: '${as}' is already defined; pick another loop variable name`, span);
        }
        break;
      case 'FEED':
        if (fragments.has(ins.list)) {
          throw new SitefileError('E_TYPE', `FEED ${ins.list}: '${ins.list}' is an html fragment, not a list`, ins.listSpan);
        }
        if (!imports.has(ins.list)) {
          throw new SitefileError('E_UNDEFINED', `FEED ${ins.list}: '${ins.list}' is not an IMPORT declared on an earlier line`, ins.listSpan);
        }
        notSitefile(ins.list, `FEED ${ins.list}`, ins.listSpan, sitefiles);
        break;
      case 'SITEMAP':
      case 'REDIRECT':
      case 'HEADER':
      case 'FINGERPRINT':
      case 'ROBOTS':
      case 'SECURITY_TXT':
      case 'CHECKLINKS':
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

function notAList(as: string, list: string, span: Instruction['span']): SitefileError {
  return new SitefileError('E_TYPE', `FOR ${as} IN ${list}: '${list}' is an html fragment, not a list`, span);
}

interface FragmentScope {
  readonly vars: ReadonlySet<string>;
  readonly imports: ReadonlySet<string>;
  readonly fragments: ReadonlySet<string>;
  readonly notValue: (name: string) => boolean;
  readonly declaredAt: ReadonlyMap<string, number>;
}

/**
 * An html LET is checked where it is declared: its template syntax, and every name it reads that
 * is declared before it. A name declared later is an error (define before use); a name declared
 * nowhere is left to the place it is used (a `RENDER … FOR` loop variable, say).
 */
function checkFragment(ins: FragmentInstruction, file: string, s: FragmentScope): void {
  for (const r of freeRefs(parseFragmentBody(ins.body, file))) {
    if (r.name === ins.name) throw new SitefileError('E_TEMPLATE', `html fragment '${ins.name}' uses itself`, r.span);
    if (s.fragments.has(r.name)) {
      if (r.use !== 'var' || r.path.length > 0) throw fragmentNotValue(r.name, r.span);
    } else if (s.vars.has(r.name)) {
      if ((r.use === 'var' || r.use === 'text') && r.path.length > 0) throw new SitefileError('E_TYPE', `'${r.name}' is an ARG/LET value; dotted access only works on JSON imports`, r.span);
    } else if (s.imports.has(r.name)) {
      // Its kind (and so whether a template can read it) is known once it is resolved.
    } else if (s.notValue(r.name)) {
      throw new SitefileError('E_UNDEFINED', `'${r.name}' is not an ARG, LET or IMPORT`, r.span);
    } else {
      const line = s.declaredAt.get(r.name);
      if (line !== undefined) {
        throw new SitefileError('E_UNDEFINED', `'${r.name}' is declared on line ${line}, after LET ${ins.name}: html; a fragment can only use names declared before it`, r.span);
      }
    }
  }
}

/** A FOR or FEED list must be a value import, not a Sitefile imported AS sitefile. */
function notSitefile(name: string, what: string, span: SourceSpan, sitefiles: ReadonlySet<string>): void {
  if (sitefiles.has(name)) throw new SitefileError('E_TYPE', `${what}: '${name}' is a Sitefile import, not a list`, span);
}

function checkSource(
  op: Operand | InlineBody,
  imports: ReadonlySet<string>,
  from: StageRef | undefined,
  finished: ReadonlySet<string>,
  all: ReadonlySet<string>,
  sitefiles: ReadonlySet<string>,
): void {
  if (isInline(op)) return; // content is in the Sitefile itself
  const imported = from ? importedStage(from.name) : undefined;
  if (from && imported) {
    // `--from=<import>+<stage>`: a stage of an imported Sitefile (whether it exists is checked when that Sitefile is read).
    if (!sitefiles.has(imported.import)) {
      throw new SitefileError('E_UNDEFINED', `--from=${from.name}: '${imported.import}' is not an IMPORT … AS sitefile declared on an earlier line`, from.span);
    }
    return;
  }
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
  if (m && sitefiles.has(m[1]!)) {
    throw new SitefileError('E_TYPE', `'${m[1]}' is a Sitefile import; read one of its stages with --from=${m[1]}+<stage>`, op.span);
  }
}

/** Parse and validate a Sitefile. */
export function parseProgram(source: string, file = 'Sitefile'): Program {
  return validate(parseSitefile(source, file), file);
}
