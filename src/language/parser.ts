import { INTEGRITY_RE } from '../digest.js';
import type { SourceSpan } from '../errors.js';
import {
  IDENT_RE,
  isInline,
  type InlineBody,
  literalText,
  type EscapeMode,
  type ExportFormat,
  parseExportFormat,
  type ImportKind,
  type Instruction,
  type Operand,
  type StageRef,
  type ValueType,
} from './ast.js';
import { Cursor, logicalLines } from './scanner.js';

const VALUE_TYPES: readonly string[] = ['string', 'number', 'boolean'];
const IMPORT_KINDS: readonly string[] = ['file', 'text', 'json', 'tree', 'markdown', 'yaml', 'csv', 'wbn'];
const NOT_YET = new Set(['REMOVE', 'GENERATE', 'ASSERT', 'META', 'EXEC']);

/** Parse Sitefile source into instructions (syntax only; see validate.ts for semantics). */
export function parseSitefile(source: string, file = 'Sitefile'): Instruction[] {
  return logicalLines(source, file).map((line) => parseInstruction(new Cursor(line, file)));
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
    case 'PLUGIN':
      return parsePlugin(c, span);
    case 'TRANSFORM':
      return parseTransform(c, span);
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
  let sort: { key: string; descending: boolean; span: SourceSpan } | undefined;
  while (!c.atEnd()) {
    const at = c.span;
    if (c.keyword('AS')) {
      if (as !== undefined) throw c.error('E_SYNTAX', 'AS given twice', at);
      const kindAt = c.span;
      const k = c.ident('import kind').toLowerCase();
      if (!IMPORT_KINDS.includes(k)) throw c.error('E_SYNTAX', `unknown import kind '${k}' (expected file, text, json, yaml, csv, markdown, wbn or tree)`, kindAt);
      as = k as ImportKind;
    } else if (c.keyword('INTEGRITY')) {
      if (integrity !== undefined) throw c.error('E_SYNTAX', 'INTEGRITY given twice', at);
      const op = c.operand('integrity digest');
      const text = literalText(op);
      if (text === undefined || !INTEGRITY_RE.test(text)) {
        throw c.error('E_SYNTAX', 'INTEGRITY must be sha256:<64 lowercase hex digits>', op.span);
      }
      integrity = text;
    } else if (c.keyword('SORT')) {
      if (sort !== undefined) throw c.error('E_SYNTAX', 'SORT given twice', at);
      c.expectKeyword('BY');
      const key = c.ident('sort key');
      const descending = c.keyword('DESC') ? true : (c.keyword('ASC'), false);
      sort = { key, descending, span: at };
    } else {
      throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected AS, INTEGRITY or SORT BY)`, at);
    }
  }
  return {
    kind: 'IMPORT',
    span,
    name,
    uri,
    ...(as !== undefined && { as }),
    ...(integrity !== undefined && { integrity }),
    ...(sort !== undefined && { sort }),
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

function noInlineFrom(c: Cursor, source: Operand | InlineBody, flags: Map<string, Flag>): void {
  const from = flags.get('from');
  if (from && isInline(source)) throw c.error('E_SYNTAX', '--from cannot be combined with an inline (<<) body', from.span);
}

function parseCopy(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare', from: 'value' });
  const source = c.inline() ?? c.operand('source');
  const dest = c.operand('destination');
  c.expectEnd();
  c.expectInlineUsed('the COPY source');
  noInlineFrom(c, source, flags);
  return { kind: 'COPY', span, source, dest, overwrite: flags.has('overwrite'), ...stageRef(c, flags) };
}

function parseRender(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { escape: 'value', overwrite: 'bare', from: 'value' });
  const escapeFlag = flags.get('escape');
  const escape = (escapeFlag?.value ?? 'none').toLowerCase();
  if (escapeFlag && escape !== 'none' && escape !== 'html') {
    throw c.error('E_SYNTAX', `--escape must be none or html, got '${escapeFlag.value}'`, escapeFlag.span);
  }
  const template = c.inline() ?? c.operand('template');
  c.expectKeyword('TO');
  const dest = c.operand('destination');
  let each: { as: string; list: string; span: SourceSpan } | undefined;
  const forAt = c.span;
  if (c.keyword('FOR')) {
    const as = c.ident('loop variable');
    c.expectKeyword('IN');
    each = { as, list: c.ident('list name'), span: forAt };
  }
  c.expectEnd();
  c.expectInlineUsed('the RENDER template');
  noInlineFrom(c, template, flags);
  return {
    kind: 'RENDER',
    span,
    template,
    dest,
    escape: escape as EscapeMode,
    overwrite: flags.has('overwrite'),
    ...stageRef(c, flags),
    ...(each && { each }),
  };
}

function parsePlugin(c: Cursor, span: SourceSpan): Instruction {
  const name = c.ident('plugin name');
  c.expectKeyword('FROM');
  const uri = c.operand('plugin path');
  let integrity: string | undefined;
  const at = c.span;
  if (c.keyword('INTEGRITY')) {
    const op = c.operand('integrity digest');
    const text = literalText(op);
    if (text === undefined || !INTEGRITY_RE.test(text)) {
      throw c.error('E_SYNTAX', 'INTEGRITY must be sha256:<64 lowercase hex digits>', op.span);
    }
    integrity = text;
  } else if (!c.atEnd()) {
    throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected INTEGRITY)`, at);
  }
  c.expectEnd();
  return { kind: 'PLUGIN', span, name, uri, ...(integrity !== undefined && { integrity }) };
}

function parseTransform(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare', from: 'value' });
  // The source is optional (only with FOR): `TRANSFORM WITH og TO … FOR post IN posts`.
  const source = c.keyword('WITH') ? undefined : (c.inline() ?? c.operand('source'));
  if (source !== undefined) c.expectKeyword('WITH');
  const pluginSpan = c.span;
  const plugin = c.ident('plugin name');
  c.expectKeyword('TO');
  const dest = c.operand('destination');
  const options: Array<{ key: string; value: Operand; span: SourceSpan }> = [];
  let each: { as: string; list: string; span: SourceSpan } | undefined;
  let sawOptions = false;
  while (!c.atEnd()) {
    const at = c.span;
    if (c.keyword('OPTIONS')) {
      if (sawOptions) throw c.error('E_SYNTAX', 'OPTIONS given twice', at);
      sawOptions = true;
      // key=value pairs until FOR or the end of the line.
      while (!c.atEnd()) {
        const keyAt = c.span;
        if (c.keyword('FOR')) {
          each = parseFor(c, keyAt);
          break;
        }
        const key = c.ident('option name', '=');
        if (!c.eat('=')) throw c.error('E_SYNTAX', `expected ${key}=value`, keyAt);
        if (options.some((o) => o.key === key)) throw c.error('E_SYNTAX', `option '${key}' given twice`, keyAt);
        options.push({ key, value: c.operand(`value of ${key}`), span: keyAt });
      }
    } else if (c.keyword('FOR')) {
      if (each) throw c.error('E_SYNTAX', 'FOR given twice', at);
      each = parseFor(c, at);
    } else {
      throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected OPTIONS or FOR)`, at);
    }
  }
  c.expectEnd();
  c.expectInlineUsed('the TRANSFORM source');
  if (source !== undefined) noInlineFrom(c, source, flags);
  if (source === undefined && flags.has('from')) throw c.error('E_SYNTAX', '--from needs a source path', flags.get('from')!.span);
  return {
    kind: 'TRANSFORM',
    span,
    ...(source !== undefined && { source }),
    ...stageRef(c, flags),
    plugin,
    pluginSpan,
    dest,
    options,
    ...(each && { each }),
    overwrite: flags.has('overwrite'),
  };
}

function parseFor(c: Cursor, span: SourceSpan): { as: string; list: string; span: SourceSpan } {
  const as = c.ident('loop variable');
  c.expectKeyword('IN');
  return { as, list: c.ident('list name'), span };
}

function parseExport(c: Cursor, span: SourceSpan): Instruction {
  const path = c.operand('path');
  let format: ExportFormat = 'directory';
  let base: Operand | undefined;
  if (c.keyword('AS')) {
    const at = c.span;
    const name = c.word();
    if (name === '') throw c.error('E_SYNTAX', 'expected export format after AS', at);
    const f = parseExportFormat(name);
    if (f === undefined) {
      throw c.error('E_UNSUPPORTED', `export format '${name}' is not supported (expected directory, zip, tar, tgz or wbn)`, at);
    }
    format = f;
  }
  if (c.keyword('BASE')) {
    base = c.operand('base URL');
  }
  c.expectEnd();
  return { kind: 'EXPORT', span, path, format, ...(base ? { base } : {}) };
}
