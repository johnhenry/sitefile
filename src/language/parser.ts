import { INTEGRITY_RE } from '../digest.js';
import type { SourceSpan } from '../errors.js';
import {
  IDENT_RE,
  literalText,
  type EscapeMode,
  type ImportKind,
  type Instruction,
  type Operand,
  type StageRef,
  type ValueType,
} from './ast.js';
import { Cursor, logicalLines } from './scanner.js';

const VALUE_TYPES: readonly string[] = ['string', 'number', 'boolean'];
const IMPORT_KINDS: readonly string[] = ['file', 'text', 'json', 'tree'];
const NOT_YET = new Set(['REMOVE', 'TRANSFORM', 'GENERATE', 'ASSERT', 'META', 'EXEC']);

/** Parse Sitefile source into instructions (syntax only; see validate.ts for semantics). */
export function parseSitefile(source: string, file = 'Sitefile'): Instruction[] {
  return logicalLines(source).map((line) => parseInstruction(new Cursor(line, file)));
}

function parseInstruction(c: Cursor): Instruction {
  const span = c.span;
  const word = c.word();
  const kw = word.toUpperCase();
  switch (kw) {
    case 'FROM':
      return parseFrom(c, span);
    case 'ARG':
    case 'LET':
      return parseBinding(c, span, kw);
    case 'IMPORT':
      return parseImport(c, span);
    case 'COPY':
      return parseCopy(c, span);
    case 'RENDER':
      return parseRender(c, span);
    case 'EXPORT':
      return parseExport(c, span);
    case 'RUN':
      throw c.error('E_UNSUPPORTED', 'RUN is not supported: Sitefile never runs shell commands', span);
  }
  if (NOT_YET.has(kw)) throw c.error('E_UNSUPPORTED', `${kw} is not supported yet`, span);
  throw c.error('E_UNKNOWN_INSTRUCTION', `unknown instruction '${word}'`, span);
}

function parseFrom(c: Cursor, span: SourceSpan): Instruction {
  const base = c.keyword('scratch') ? 'scratch' : c.operand('base');
  const alias = c.keyword('AS') ? c.ident('stage name') : undefined;
  c.expectEnd();
  return alias === undefined ? { kind: 'FROM', span, base } : { kind: 'FROM', span, base, alias };
}

function parseBinding(c: Cursor, span: SourceSpan, kind: 'ARG' | 'LET'): Instruction {
  const name = c.ident(`${kind} name`, ':=');
  let type: ValueType = 'string';
  if (c.eat(':')) {
    const at = c.span;
    const t = c.ident('type', '=').toLowerCase();
    if (!VALUE_TYPES.includes(t)) throw c.error('E_SYNTAX', `unknown type '${t}' (expected string, number or boolean)`, at);
    type = t as ValueType;
  }
  const value = c.eat('=') ? c.operand('value') : undefined;
  c.expectEnd();
  if (kind === 'ARG') {
    return value === undefined ? { kind, span, name, type } : { kind, span, name, type, defaultValue: value };
  }
  if (value === undefined) throw c.error('E_SYNTAX', `LET ${name} requires a value (LET ${name} = ...)`, span);
  return { kind, span, name, type, value };
}

function parseImport(c: Cursor, span: SourceSpan): Instruction {
  const name = c.ident('import name');
  c.expectKeyword('FROM');
  const uri = c.operand('URI');
  let as: ImportKind | undefined;
  let integrity: string | undefined;
  while (!c.atEnd()) {
    const at = c.span;
    if (c.keyword('AS')) {
      if (as !== undefined) throw c.error('E_SYNTAX', 'AS given twice', at);
      const kindAt = c.span;
      const k = c.ident('import kind').toLowerCase();
      if (!IMPORT_KINDS.includes(k)) throw c.error('E_SYNTAX', `unknown import kind '${k}' (expected file, text, json or tree)`, kindAt);
      as = k as ImportKind;
    } else if (c.keyword('INTEGRITY')) {
      if (integrity !== undefined) throw c.error('E_SYNTAX', 'INTEGRITY given twice', at);
      const op = c.operand('integrity digest');
      const text = literalText(op);
      if (text === undefined || !INTEGRITY_RE.test(text)) {
        throw c.error('E_SYNTAX', 'INTEGRITY must be sha256:<64 lowercase hex digits>', op.span);
      }
      integrity = text;
    } else {
      throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected AS or INTEGRITY)`, at);
    }
  }
  return {
    kind: 'IMPORT',
    span,
    name,
    uri,
    ...(as !== undefined && { as }),
    ...(integrity !== undefined && { integrity }),
  };
}

interface Flag {
  readonly value: string;
  readonly span: SourceSpan;
}

/** Read leading `--flags`; returns name -> flag (value '' for a bare flag). */
function parseFlags(c: Cursor, allowed: Record<string, 'bare' | 'value'>): Map<string, Flag> {
  const out = new Map<string, Flag>();
  for (let f = c.flag(); f; f = c.flag()) {
    const shape = allowed[f.name];
    if (shape === undefined) throw c.error('E_SYNTAX', `unknown flag --${f.name}`, f.span);
    if (out.has(f.name)) throw c.error('E_SYNTAX', `--${f.name} given twice`, f.span);
    if (shape === 'bare' && f.value !== undefined) throw c.error('E_SYNTAX', `--${f.name} takes no value`, f.span);
    if (shape === 'value' && !f.value) throw c.error('E_SYNTAX', `--${f.name} requires a value (--${f.name}=...)`, f.span);
    out.set(f.name, { value: f.value ?? '', span: f.span });
  }
  return out;
}

function stageRef(c: Cursor, flags: Map<string, Flag>): { from?: StageRef } {
  const f = flags.get('from');
  if (!f) return {};
  if (!IDENT_RE.test(f.value)) throw c.error('E_SYNTAX', `--from needs a stage name, got '${f.value}'`, f.span);
  return { from: { name: f.value, span: f.span } };
}

function parseCopy(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare', from: 'value' });
  const source = c.operand('source');
  const dest = c.operand('destination');
  c.expectEnd();
  return { kind: 'COPY', span, source, dest, overwrite: flags.has('overwrite'), ...stageRef(c, flags) };
}

function parseRender(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { escape: 'value', overwrite: 'bare', from: 'value' });
  const escapeFlag = flags.get('escape');
  const escape = (escapeFlag?.value ?? 'none').toLowerCase();
  if (escapeFlag && escape !== 'none' && escape !== 'html') {
    throw c.error('E_SYNTAX', `--escape must be none or html, got '${escapeFlag.value}'`, escapeFlag.span);
  }
  const template: Operand = c.operand('template');
  c.expectKeyword('TO');
  const dest = c.operand('destination');
  c.expectEnd();
  return { kind: 'RENDER', span, template, dest, escape: escape as EscapeMode, overwrite: flags.has('overwrite'), ...stageRef(c, flags) };
}

function parseExport(c: Cursor, span: SourceSpan): Instruction {
  const path = c.operand('path');
  if (c.keyword('AS')) {
    const at = c.span;
    const format = c.word();
    if (format === '') throw c.error('E_SYNTAX', 'expected export format after AS', at);
    if (format.toLowerCase() !== 'directory') {
      throw c.error('E_UNSUPPORTED', `export format '${format}' is not supported yet (only directory)`, at);
    }
  }
  c.expectEnd();
  return { kind: 'EXPORT', span, path, format: 'directory' };
}
