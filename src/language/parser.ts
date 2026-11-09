import { INTEGRITY_RE } from '../digest.js';
import type { SourceSpan } from '../errors.js';
import {
  IDENT_RE,
  IMPORTED_STAGE_RE,
  isInline,
  type FromBase,
  type InlineBody,
  literalText,
  type EscapeMode,
  type ExportFormat,
  parseExportFormat,
  type OciCompression,
  parseOciCompression,
  REDIRECT_STATUSES,
  type RedirectStatus,
  type ImportArg,
  type ImportKind,
  type ImportSecret,
  type Instruction,
  type Operand,
  type RenderEach,
  type StageRef,
  type ValueType,
} from './ast.js';
import { isIgnorePattern } from '../resources/ignore.js';
import { Cursor, logicalLines } from './scanner.js';

const VALUE_TYPES: readonly string[] = ['string', 'number', 'boolean'];
const IMPORT_KINDS: readonly string[] = ['file', 'text', 'json', 'tree', 'markdown', 'yaml', 'csv', 'wbn', 'html', 'sitefile'];
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
    case 'STAGE':
      return parseStage(c, span);
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
    case 'TOOL':
      return parseTool(c, span);
    case 'TRANSFORM':
      return parseTransform(c, span);
    case 'SITEMAP':
      return parseSitemap(c, span);
    case 'FEED':
      return parseFeed(c, span);
    case 'REDIRECT':
      return parseRedirect(c, span);
    case 'HEADER':
      return parseHeader(c, span);
    case 'FINGERPRINT':
      return parseFingerprint(c, span);
    case 'CANONICAL':
      return parseCanonical(c, span);
    case 'CHECKLINKS':
      return parseCheckLinks(c, span);
    case 'RUN':
      throw c.error('E_UNSUPPORTED', 'RUN is not supported: Sitefile never runs shell commands', span);
  }
  if (NOT_YET.has(kw)) throw c.error('E_UNSUPPORTED', `${kw} is not supported yet`, span);
  throw c.error('E_UNKNOWN_INSTRUCTION', `unknown instruction '${word}'`, span);
}

/**
 * A FROM base: `scratch`, `image <ref> [AT <dir>]`, or an operand (stage or directory). `image` is the
 * keyword only when something other than `AS` follows it, so `FROM image` and `FROM image AS x` still
 * name a stage or directory called `image`.
 */
function fromBase(c: Cursor): FromBase {
  if (c.keyword('scratch')) return 'scratch';
  const mark = c.mark();
  if (c.keyword('image') && !c.atEnd() && !c.peekKeyword('AS')) {
    const ref = c.operand('image reference');
    return c.keyword('AT') ? { kind: 'image', ref, at: c.operand('image directory') } : { kind: 'image', ref };
  }
  c.reset(mark);
  return c.operand('base');
}

function parseFrom(c: Cursor, span: SourceSpan): Instruction {
  const base = fromBase(c);
  const alias = c.keyword('AS') ? c.ident('stage name') : undefined;
  c.expectEnd();
  return alias === undefined ? { kind: 'FROM', span, base } : { kind: 'FROM', span, base, alias };
}

/** `STAGE name [FROM base]`: the same IR as `FROM base AS name` (an omitted base is `scratch`). */
function parseStage(c: Cursor, span: SourceSpan): Instruction {
  const alias = c.ident('stage name');
  let base: FromBase = 'scratch';
  if (c.keyword('FROM')) base = fromBase(c);
  c.expectEnd();
  return { kind: 'FROM', span, base, alias, keyword: 'STAGE' };
}

function parseBinding(c: Cursor, span: SourceSpan, kind: 'ARG' | 'LET'): Instruction {
  const name = c.ident(`${kind} name`, ':=');
  let type: ValueType = 'string';
  if (c.eat(':')) {
    const at = c.span;
    const t = c.ident('type', '=').toLowerCase();
    if (t === 'html') {
      if (kind === 'ARG') throw c.error('E_SYNTAX', `ARG ${name} cannot be html: an html value is markup written in the Sitefile (LET ${name}: html = …)`, at);
      return parseFragment(c, span, name);
    }
    if (!VALUE_TYPES.includes(t)) throw c.error('E_SYNTAX', `unknown type '${t}' (expected string, number or boolean${kind === 'LET' ? ', or html' : ''})`, at);
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

/**
 * `LET name: html = <markup>` (the rest of the line is the template) or `LET name: html <<HTML`
 * (also `= <<HTML`). A one-line fragment cannot use `\` continuations, so its error positions are exact.
 */
function parseFragment(c: Cursor, span: SourceSpan, name: string): Instruction {
  const eq = c.eat('=');
  const inline = c.inline();
  if (inline) {
    c.expectEnd();
    return { kind: 'LET', span, name, type: 'html', body: inline };
  }
  c.expectInlineUsed(`the value of LET ${name}: html`);
  const at = c.span;
  const rest = c.rest();
  if (!eq || rest.text === '') {
    throw c.error('E_SYNTAX', `LET ${name}: html requires a value (LET ${name}: html = <markup>, or a heredoc: LET ${name}: html <<HTML)`, eq ? at : rest.span);
  }
  if (rest.multiline) {
    throw c.error('E_SYNTAX', `LET ${name}: html on one line cannot continue onto the next line; write it as a heredoc (LET ${name}: html <<HTML)`, rest.span);
  }
  return { kind: 'LET', span, name, type: 'html', body: { kind: 'line', text: rest.text, span: rest.span } };
}

function parseImport(c: Cursor, span: SourceSpan): Instruction {
  const name = c.ident('import name');
  c.expectKeyword('FROM');
  const uri = c.operand('URI');
  let as: ImportKind | undefined;
  let integrity: string | undefined;
  let sort: { key: string; descending: boolean; span: SourceSpan } | undefined;
  let schema: { path: Operand; span: SourceSpan } | undefined;
  let drafts: { value: Operand; span: SourceSpan } | undefined;
  const secrets: ImportSecret[] = [];
  const args: ImportArg[] = [];
  while (!c.atEnd()) {
    const at = c.span;
    if (c.keyword('AS')) {
      if (as !== undefined) throw c.error('E_SYNTAX', 'AS given twice', at);
      const kindAt = c.span;
      const k = c.ident('import kind').toLowerCase();
      if (!IMPORT_KINDS.includes(k)) throw c.error('E_SYNTAX', `unknown import kind '${k}' (expected file, text, json, yaml, csv, markdown, wbn, html, tree or sitefile)`, kindAt);
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
    } else if (c.keyword('SCHEMA')) {
      if (schema !== undefined) throw c.error('E_SYNTAX', 'SCHEMA given twice', at);
      schema = { path: c.operand('schema path'), span: at };
    } else if (c.keyword('DRAFTS')) {
      if (drafts !== undefined) throw c.error('E_SYNTAX', 'DRAFTS given twice', at);
      drafts = { value: c.operand('true or false'), span: at };
    } else if (c.keyword('SECRET')) {
      const s = parseSecretClause(c, at);
      const prior = secrets.find((p) => p.header.toLowerCase() === s.header.toLowerCase());
      if (prior) throw c.error('E_SYNTAX', `header '${s.header}' is set by two SECRET clauses`, at);
      secrets.push(s);
    } else if (c.keyword('ARG')) {
      // `ARG NAME=value`: a build arg of the Sitefile imported AS sitefile.
      const argName = c.ident('ARG name', '=');
      if (!c.eat('=')) throw c.error('E_SYNTAX', `expected = after ARG ${argName} (ARG ${argName}=<value>)`);
      if (args.some((a) => a.name === argName)) throw c.error('E_SYNTAX', `ARG ${argName} given twice`, at);
      args.push({ name: argName, value: c.operand('ARG value'), span: at });
    } else {
      throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected AS, INTEGRITY, SORT BY, SCHEMA, DRAFTS, SECRET or ARG)`, at);
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
    ...(schema !== undefined && { schema }),
    ...(drafts !== undefined && { drafts }),
    ...(secrets.length > 0 && { secrets }),
    ...(args.length > 0 && { args }),
  };
}

const HEADER_TOKEN = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;
/** Headers the HTTP client sets itself (or that frame the request): a secret cannot replace them. */
const RESERVED_HEADERS = new Set([
  'accept', 'accept-encoding', 'connection', 'content-encoding', 'content-length', 'content-type', 'git-protocol',
  'host', 'keep-alive', 'te', 'trailer', 'transfer-encoding', 'upgrade', 'user-agent',
]);

/** `SECRET <id> AS HEADER <Name>` or `SECRET <id> AS BEARER` (after the SECRET keyword). */
function parseSecretClause(c: Cursor, span: SourceSpan): ImportSecret {
  const id = c.ident('secret id');
  c.expectKeyword('AS');
  if (c.keyword('BEARER')) return { id, header: 'Authorization', bearer: true, span };
  const at = c.span;
  if (!c.keyword('HEADER')) throw c.error('E_SYNTAX', 'expected HEADER <name> or BEARER', at);
  const nameAt = c.span;
  const header = c.word();
  if (header === '') throw c.error('E_SYNTAX', 'expected a header name', nameAt);
  if (!HEADER_TOKEN.test(header)) throw c.error('E_SYNTAX', `invalid header name '${header}'`, nameAt);
  if (RESERVED_HEADERS.has(header.toLowerCase())) throw c.error('E_SYNTAX', `header '${header}' is set by sitefile and cannot carry a secret`, nameAt);
  return { id, header, bearer: false, span };
}

interface Flag {
  readonly value: string;
  readonly span: SourceSpan;
}

/** A repeatable flag, in the order given (several repeatable flags share one list). */
interface NamedFlag extends Flag {
  readonly name: string;
}

/**
 * Read leading `--flags`; returns name -> flag (value '' for a bare flag). A `repeat` flag may be
 * given several times: each one is added to `repeated` (and the map holds the first).
 */
function parseFlags(c: Cursor, allowed: Record<string, 'bare' | 'value' | 'repeat'>, repeated: NamedFlag[] = []): Map<string, Flag> {
  const out = new Map<string, Flag>();
  for (let f = c.flag(); f; f = c.flag()) {
    const shape = allowed[f.name];
    if (shape === undefined) throw c.error('E_SYNTAX', `unknown flag --${f.name}`, f.span);
    if (out.has(f.name) && shape !== 'repeat') throw c.error('E_SYNTAX', `--${f.name} given twice`, f.span);
    if (shape === 'bare' && f.value !== undefined) throw c.error('E_SYNTAX', `--${f.name} takes no value`, f.span);
    if (shape !== 'bare' && !f.value) throw c.error('E_SYNTAX', `--${f.name} requires a value (--${f.name}=...)`, f.span);
    const flag = { value: f.value ?? '', span: f.span };
    if (shape === 'repeat') repeated.push({ ...flag, name: f.name });
    if (!out.has(f.name)) out.set(f.name, flag);
  }
  return out;
}

function stageRef(c: Cursor, flags: Map<string, Flag>): { from?: StageRef } {
  const f = flags.get('from');
  if (!f) return {};
  // A stage of this Sitefile, or `<import>+<stage>` for a stage of a Sitefile imported AS sitefile.
  if (!IDENT_RE.test(f.value) && !IMPORTED_STAGE_RE.test(f.value)) throw c.error('E_SYNTAX', `--from needs a stage name, got '${f.value}'`, f.span);
  return { from: { name: f.value, span: f.span } };
}

function noInlineFrom(c: Cursor, source: Operand | InlineBody, flags: Map<string, Flag>): void {
  const from = flags.get('from');
  if (from && isInline(source)) throw c.error('E_SYNTAX', '--from cannot be combined with an inline (<<) body', from.span);
}

/** Repeatable pattern flags (`--exclude`, `--include`): each value must be a `.sitefileignore` rule. */
function checkPatterns(c: Cursor, patterns: readonly NamedFlag[]): void {
  for (const e of patterns) {
    if (!isIgnorePattern(e.value)) throw c.error('E_SYNTAX', `--${e.name}=${e.value} is not a pattern (it is blank or a # comment in .sitefileignore syntax)`, e.span);
  }
}

function parseCopy(c: Cursor, span: SourceSpan): Instruction {
  const excludes: NamedFlag[] = [];
  const flags = parseFlags(c, { overwrite: 'bare', from: 'value', exclude: 'repeat', chmod: 'value', parents: 'bare' }, excludes);
  checkPatterns(c, excludes);
  const chmodFlag = flags.get('chmod');
  let chmod: number | undefined;
  if (chmodFlag) {
    chmod = Object.hasOwn(CHMOD_MODES, chmodFlag.value) ? CHMOD_MODES[chmodFlag.value] : undefined;
    if (chmod === undefined) {
      throw c.error('E_SYNTAX', `--chmod must be 644 or 755 (files in a Sitefile tree are 0644 or 0755), got '${chmodFlag.value}'`, chmodFlag.span);
    }
  }
  const source = c.inline() ?? c.operand('source');
  const dest = c.operand('destination');
  c.expectEnd();
  c.expectInlineUsed('the COPY source');
  noInlineFrom(c, source, flags);
  for (const name of ['exclude', 'parents']) {
    const f = flags.get(name);
    if (f && isInline(source)) throw c.error('E_SYNTAX', `--${name} cannot be combined with an inline (<<) body`, f.span);
  }
  return {
    kind: 'COPY',
    span,
    source,
    dest,
    overwrite: flags.has('overwrite'),
    ...stageRef(c, flags),
    ...(excludes.length > 0 && { exclude: excludes.map((e) => e.value) }),
    ...(chmod !== undefined && { chmod }),
    ...(flags.has('parents') && { parents: true as const }),
  };
}

/** `--chmod` values: Sitefile trees only hold 0644 and 0755 files. */
const CHMOD_MODES: Readonly<Record<string, number>> = { '644': 0o644, '0644': 0o644, '755': 0o755, '0755': 0o755 };

function parseRender(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { escape: 'value', overwrite: 'bare', from: 'value', 'inline-svg': 'bare' });
  const escapeFlag = flags.get('escape');
  const escape = (escapeFlag?.value ?? 'none').toLowerCase();
  if (escapeFlag && escape !== 'none' && escape !== 'html') {
    throw c.error('E_SYNTAX', `--escape must be none or html, got '${escapeFlag.value}'`, escapeFlag.span);
  }
  const template = c.inline() ?? c.operand('template');
  c.expectKeyword('TO');
  const dest = c.operand('destination');
  let each: RenderEach | undefined;
  const forAt = c.span;
  if (c.keyword('FOR')) {
    const as = c.ident('loop variable');
    c.expectKeyword('IN');
    const list = c.ident('list name');
    const byAt = c.span;
    const by = c.keyword('BY') ? { key: c.ident('group key'), span: byAt } : undefined;
    const pageAt = c.span;
    let page: RenderEach['page'];
    if (c.keyword('PAGE')) {
      const size = c.operand('page size');
      page = c.keyword('FIRST') ? { size, first: c.operand('first page destination'), span: pageAt } : { size, span: pageAt };
    }
    each = { as, list, span: forAt, ...(by && { by }), ...(page && { page }) };
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
    ...(flags.has('inline-svg') && { inlineSvg: true as const }),
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

function parseTool(c: Cursor, span: SourceSpan): Instruction {
  const name = c.ident('tool name');
  c.expectEnd();
  return { kind: 'TOOL', span, name };
}

function parseTransform(c: Cursor, span: SourceSpan): Instruction {
  const patterns: NamedFlag[] = [];
  const flags = parseFlags(c, { overwrite: 'bare', from: 'value', include: 'repeat', exclude: 'repeat' }, patterns);
  checkPatterns(c, patterns);
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
  let args: Operand[] | undefined;
  const parseArgs = (): void => {
    // ARGS takes the rest of the line: every word is one argv entry.
    args = [];
    while (!c.atEnd()) args.push(c.operand('argument'));
  };
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
        if (c.keyword('ARGS')) {
          parseArgs();
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
    } else if (c.keyword('ARGS')) {
      parseArgs();
    } else {
      throw c.error('E_SYNTAX', `unexpected '${c.word()}' (expected OPTIONS, FOR or ARGS)`, at);
    }
  }
  c.expectEnd();
  c.expectInlineUsed('the TRANSFORM source');
  if (source !== undefined) noInlineFrom(c, source, flags);
  if (source === undefined && flags.has('from')) throw c.error('E_SYNTAX', '--from needs a source path', flags.get('from')!.span);
  // --include/--exclude pick the files of a directory source, which runs the plugin once per file.
  const firstPattern = patterns[0];
  if (firstPattern) {
    if (source === undefined) throw c.error('E_SYNTAX', `--${firstPattern.name} needs a source directory`, firstPattern.span);
    if (isInline(source)) throw c.error('E_SYNTAX', `--${firstPattern.name} cannot be combined with an inline (<<) body`, firstPattern.span);
    if (each) throw c.error('E_SYNTAX', `--${firstPattern.name} filters a directory source, which cannot be combined with FOR`, firstPattern.span);
  }
  const include = patterns.filter((p) => p.name === 'include').map((p) => p.value);
  const exclude = patterns.filter((p) => p.name === 'exclude').map((p) => p.value);
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
    ...(args && { args }),
    ...(include.length > 0 && { include }),
    ...(exclude.length > 0 && { exclude }),
    overwrite: flags.has('overwrite'),
  };
}

function parseFor(c: Cursor, span: SourceSpan): { as: string; list: string; span: SourceSpan } {
  const as = c.ident('loop variable');
  c.expectKeyword('IN');
  return { as, list: c.ident('list name'), span };
}

/** `SITEMAP [--overwrite] BASE <url> [TO <dest>]` */
function parseSitemap(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare' });
  c.expectKeyword('BASE');
  const base = c.operand('base URL');
  const dest = c.keyword('TO') ? c.operand('destination') : undefined;
  c.expectEnd();
  return { kind: 'SITEMAP', span, base, ...(dest ? { dest } : {}), overwrite: flags.has('overwrite') };
}

/** `FEED [--overwrite] <import> BASE <url> LINK <path> [TO <dest>] [TITLE <text>]` (clauses in this order) */
function parseFeed(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare' });
  const listSpan = c.span;
  const list = c.ident('import name');
  c.expectKeyword('BASE');
  const base = c.operand('base URL');
  c.expectKeyword('LINK');
  const link = c.operand('link path');
  const dest = c.keyword('TO') ? c.operand('destination') : undefined;
  const title = c.keyword('TITLE') ? c.operand('feed title') : undefined;
  c.expectEnd();
  return {
    kind: 'FEED',
    span,
    list,
    listSpan,
    base,
    link,
    ...(dest ? { dest } : {}),
    ...(title ? { title } : {}),
    overwrite: flags.has('overwrite'),
  };
}

/** `REDIRECT [--overwrite] <from> <to> [STATUS 301|302|303|307|308]` */
function parseRedirect(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare' });
  const from = c.operand('redirect source path');
  const to = c.operand('redirect target');
  let status: RedirectStatus = 301;
  if (c.keyword('STATUS')) {
    const at = c.span;
    const w = c.word();
    const n = REDIRECT_STATUSES.find((s) => String(s) === w);
    if (n === undefined) {
      throw c.error('E_SYNTAX', w === '' ? 'expected a status after STATUS (301, 302, 303, 307 or 308)' : `unknown redirect status '${w}' (expected 301, 302, 303, 307 or 308)`, at);
    }
    status = n;
  }
  c.expectEnd();
  return { kind: 'REDIRECT', span, from, to, status, overwrite: flags.has('overwrite') };
}

/** `HEADER [--overwrite] <path-pattern> <Name>: <value …>` */
function parseHeader(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare' });
  const pattern = c.operand('path pattern');
  const raw = c.operand('header name');
  const last = raw.parts[raw.parts.length - 1];
  if (last?.kind !== 'text' || !last.value.endsWith(':')) {
    throw c.error('E_SYNTAX', "expected 'Name:' (a header name and a colon, then the value: HEADER /* X-Frame-Options: DENY)", raw.span);
  }
  const head = last.value.slice(0, -1);
  const parts = [...raw.parts.slice(0, -1), ...(head === '' ? [] : [{ kind: 'text' as const, value: head }])];
  if (parts.length === 0) throw c.error('E_SYNTAX', 'expected a header name before the colon', raw.span);
  const value: Operand[] = [];
  while (!c.atEnd()) value.push(c.operand('header value'));
  if (value.length === 0) throw c.error('E_SYNTAX', 'expected header value');
  return { kind: 'HEADER', span, pattern, name: { parts, span: raw.span }, value, overwrite: flags.has('overwrite') };
}

/** `FINGERPRINT [--overwrite] [--no-cache-headers] <pattern>… [MANIFEST <dest>] [INTEGRITY]` */
function parseFingerprint(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { overwrite: 'bare', 'no-cache-headers': 'bare' });
  const patterns: Operand[] = [];
  while (!c.atEnd() && !c.peekKeyword('MANIFEST') && !c.peekKeyword('INTEGRITY')) patterns.push(c.operand('path pattern'));
  if (patterns.length === 0) throw c.error('E_SYNTAX', 'expected a path pattern (FINGERPRINT /assets/ or FINGERPRINT /**/*.css)');
  const manifest = c.keyword('MANIFEST') ? c.operand('manifest destination') : undefined;
  const integrity = c.keyword('INTEGRITY');
  c.expectEnd();
  return {
    kind: 'FINGERPRINT',
    span,
    patterns,
    ...(manifest ? { manifest } : {}),
    ...(integrity && { integrity: true as const }),
    ...(flags.has('no-cache-headers') && { noCacheHeaders: true as const }),
    overwrite: flags.has('overwrite'),
  };
}

/** `CANONICAL BASE <url> [PRETTY]` */
function parseCanonical(c: Cursor, span: SourceSpan): Instruction {
  c.expectKeyword('BASE');
  const base = c.operand('base URL');
  const pretty = c.keyword('PRETTY');
  c.expectEnd();
  return { kind: 'CANONICAL', span, base, pretty };
}

/** `CHECKLINKS [--warn] [--external]` */
function parseCheckLinks(c: Cursor, span: SourceSpan): Instruction {
  const flags = parseFlags(c, { warn: 'bare', external: 'bare' });
  c.expectEnd();
  return { kind: 'CHECKLINKS', span, warn: flags.has('warn'), external: flags.has('external') };
}

function parseExport(c: Cursor, span: SourceSpan): Instruction {
  const path = c.operand('path');
  let format: ExportFormat = 'directory';
  let base: Operand | undefined;
  let at: Operand | undefined;
  if (c.keyword('AS')) {
    const at = c.span;
    const name = c.word();
    if (name === '') throw c.error('E_SYNTAX', 'expected export format after AS', at);
    const f = parseExportFormat(name);
    if (f === undefined) {
      throw c.error('E_UNSUPPORTED', `export format '${name}' is not supported (expected directory, zip, tar, tgz, wbn, oci, car, html or mhtml)`, at);
    }
    format = f;
  }
  if (c.keyword('BASE')) {
    base = c.operand('base URL');
  }
  if (c.keyword('AT')) {
    at = c.operand('image root path');
  }
  let compression: { value: OciCompression; span: SourceSpan } | undefined;
  if (c.keyword('COMPRESSION')) {
    const cspan = c.span;
    const name = c.word();
    const value = parseOciCompression(name);
    if (value === undefined) {
      throw c.error('E_SYNTAX', name === '' ? 'expected gzip or none after COMPRESSION' : `unknown compression '${name}' (expected gzip or none)`, cspan);
    }
    compression = { value, span: cspan };
  }
  c.expectEnd();
  return { kind: 'EXPORT', span, path, format, ...(base ? { base } : {}), ...(at ? { at } : {}), ...(compression ? { compression } : {}) };
}
