import { SitefileError } from '../errors.js';
import { literalText, type Instruction, type Operand, type Program } from './ast.js';
import { parseSitefile } from './parser.js';

const IMPORT_REF = /^([A-Za-z_][A-Za-z0-9_]*):/;

function operandsOf(ins: Instruction): Operand[] {
  switch (ins.kind) {
    case 'FROM':
      return ins.base === 'scratch' ? [] : [ins.base];
    case 'ARG':
      return ins.defaultValue ? [ins.defaultValue] : [];
    case 'LET':
      return [ins.value];
    case 'IMPORT':
      return [ins.uri];
    case 'COPY':
      return [ins.source, ins.dest];
    case 'RENDER':
      return [ins.template, ins.dest];
    case 'EXPORT':
      return [ins.path];
  }
}

/** Check semantic rules (see docs/LANGUAGE.md, "v0.1 implementation decisions"). */
export function validate(instructions: readonly Instruction[], file: string): Program {
  const first = instructions[0];
  if (!first) throw new SitefileError('E_VALIDATION', 'Sitefile has no instructions', { file, line: 1, column: 1 });

  const vars = new Set<string>();
  const imports = new Set<string>();
  let sawFrom = false;
  let sawExport = false;

  for (const ins of instructions) {
    if (sawExport) throw new SitefileError('E_VALIDATION', 'EXPORT must be the last instruction', ins.span);

    for (const op of operandsOf(ins)) {
      for (const part of op.parts) {
        if (part.kind !== 'var' || vars.has(part.name)) continue;
        const hint = imports.has(part.name) ? ` ('${part.name}' is an import; use it in a RENDER template)` : '';
        throw new SitefileError('E_UNDEFINED', `undefined variable \${${part.name}}: declare it with ARG or LET on an earlier line${hint}`, part.span);
      }
    }

    if (ins.kind === 'FROM') {
      if (sawFrom) throw new SitefileError('E_UNSUPPORTED', 'multiple FROM instructions (multi-stage builds) arrive in M2', ins.span);
      if (ins.base !== 'scratch') throw new SitefileError('E_UNSUPPORTED', 'only FROM scratch is supported; other bases arrive in M2', ins.span);
      sawFrom = true;
      continue;
    }
    if (!sawFrom && ins.kind !== 'ARG') {
      throw new SitefileError('E_VALIDATION', `${ins.kind} before FROM: only ARG may come before FROM`, ins.span);
    }

    switch (ins.kind) {
      case 'ARG':
      case 'LET':
        if (vars.has(ins.name) || imports.has(ins.name)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        }
        vars.add(ins.name);
        break;
      case 'IMPORT':
        if (vars.has(ins.name) || imports.has(ins.name)) {
          throw new SitefileError('E_DUPLICATE', `'${ins.name}' is already defined`, ins.span);
        }
        imports.add(ins.name);
        break;
      case 'COPY':
        checkSource(ins.source, imports);
        break;
      case 'RENDER':
        checkSource(ins.template, imports);
        break;
      case 'EXPORT':
        sawExport = true;
        break;
    }
  }

  const last = instructions[instructions.length - 1]!;
  if (!sawFrom) throw new SitefileError('E_VALIDATION', 'missing FROM (start with FROM scratch)', first.span);
  if (!sawExport) throw new SitefileError('E_VALIDATION', 'missing EXPORT: the last instruction must be EXPORT', last.span);
  return { file, instructions };
}

function checkSource(op: Operand, imports: ReadonlySet<string>): void {
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
