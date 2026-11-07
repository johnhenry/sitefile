import { SitefileError, type SourceSpan } from './errors.js';
import type { EscapeMode } from './language/ast.js';
import type { Scalar } from './values.js';
import { FILE_HELPERS, renderFile, VALUE_HELPERS, type HelperAttr, type TemplateTree } from './media.js';

/** A string of a `${file …}` expression: text and `${name.key}` references. */
export type HelperText = ReadonlyArray<
  { readonly kind: 'text'; readonly value: string } | { readonly kind: 'ref'; readonly name: string; readonly path: readonly string[]; readonly span: SourceSpan }
>;

/**
 * Resolves a template variable to a scalar. Supplied by the executor, which
 * throws E_UNDEFINED / E_TYPE itself for unknown names or non-scalar values.
 */
export type TemplateLookup = (name: string, path: readonly string[], span: SourceSpan) => Scalar;

/**
 * Resolves a value for `${#each}` and `${#if}`. `loose` (used by `#if`) means a
 * missing name or key yields `undefined` instead of an error.
 */
export type TemplateValueLookup = (name: string, path: readonly string[], span: SourceSpan, loose: boolean) => unknown;

/** A parsed RENDER template: text, substitutions and blocks. */
export type TemplateNode =
  | { readonly kind: 'text'; readonly value: string }
  /** `raw` is set (true) only for `${x|raw}`, which is never escaped. */
  | { readonly kind: 'var'; readonly name: string; readonly path: readonly string[]; readonly span: SourceSpan; readonly raw?: true }
  | {
      readonly kind: 'each';
      readonly list: Ref;
      readonly as: string;
      readonly body: readonly TemplateNode[];
      readonly empty: readonly TemplateNode[];
      readonly span: SourceSpan;
    }
  /** `${file "<path>" [| helper attr…]}`: markup or a value for a file of the RENDER's tree (src/media.ts). */
  | {
      readonly kind: 'file';
      readonly path: HelperText;
      readonly helper?: string;
      readonly attrs: ReadonlyArray<{ readonly name: string; readonly value?: HelperText }>;
      readonly span: SourceSpan;
    }
  | {
      readonly kind: 'if';
      readonly cond: Ref;
      readonly negate: boolean;
      readonly then: readonly TemplateNode[];
      readonly otherwise: readonly TemplateNode[];
      readonly span: SourceSpan;
    };

interface Ref {
  readonly name: string;
  readonly path: readonly string[];
  readonly span: SourceSpan;
}

const REF = String.raw`[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z0-9_-]+)*`;
const VAR_RE = new RegExp(String.raw`^(${REF})(?:\|(raw))?$`);
const EACH_RE = new RegExp(String.raw`^#each\s+(${REF})\s+as\s+([A-Za-z_][A-Za-z0-9_]*)\s*$`);
const IF_RE = new RegExp(String.raw`^#if\s+(!?)\s*(${REF})\s*$`);
const BLOCK_RE = /^(#each|#if|else|\/each|\/if)(?![A-Za-z0-9_.])/;

type Token =
  | { readonly kind: 'text'; readonly value: string }
  | Extract<TemplateNode, { kind: 'var' }>
  | Extract<TemplateNode, { kind: 'file' }>
  | { readonly kind: 'open-each'; readonly list: Ref; readonly as: string; readonly span: SourceSpan }
  | { readonly kind: 'open-if'; readonly cond: Ref; readonly negate: boolean; readonly span: SourceSpan }
  | { readonly kind: 'else' | 'close-each' | 'close-if'; readonly span: SourceSpan };

/**
 * Parse a RENDER template.
 *
 * - `${name}` / `${name.seg.seg}` substitutes a value (segments may be array
 *   indexes); `${name|raw}` substitutes it without escaping.
 * - `${#each list as item}…${else}…${/each}` repeats for each element of a list
 *   (the `${else}` part renders when the list is empty). A missing key such as
 *   `post.tags` counts as an empty list; an unknown top-level name is an error.
 * - `${#if x}…${else}…${/if}` / `${#if !x}`: false, null, missing, '', 0 and
 *   empty lists are falsy.
 * - A block tag alone on its line removes that whole line from the output.
 * - `$${` writes a literal `${`; a `$` not followed by `{` is literal.
 * - Anything else inside `${…}`, or unbalanced blocks, fails with E_TEMPLATE.
 */
export function parseRenderTemplate(text: string, file: string, firstLine = 1, firstColumn = 1): TemplateNode[] {
  const tokens: Token[] = [];
  let buf = '';
  // firstLine > 1 when the template is a heredoc body inside the Sitefile; firstColumn > 1 when it
  // starts inside a Sitefile line (`LET name: html = …`).
  let line = firstLine;
  let lineStart = 0;
  let i = 0;
  const spanAt = (idx: number): SourceSpan => ({ file, line, column: idx - lineStart + 1 + (line === firstLine ? firstColumn - 1 : 0) });
  const flush = (): void => {
    if (buf !== '') tokens.push({ kind: 'text', value: buf });
    buf = '';
  };

  while (i < text.length) {
    const ch = text[i];
    if (ch === '\n') {
      buf += ch;
      i++;
      line++;
      lineStart = i;
      continue;
    }
    if (ch !== '$') {
      buf += ch;
      i++;
      continue;
    }
    if (text[i + 1] === '$' && text[i + 2] === '{') {
      buf += '${';
      i += 3;
      continue;
    }
    if (text[i + 1] !== '{') {
      buf += '$';
      i++;
      continue;
    }
    const span = spanAt(i);
    if (/^file[ \t]/.test(text.slice(i + 2, i + 7))) {
      // `${file …}`: strings in it may hold `}`, so it is read by its own scanner.
      const r = parseFileExpr(text, i + 2, spanAt, span);
      flush();
      tokens.push(r.node);
      i = r.end;
      continue;
    }
    const close = text.indexOf('}', i + 2);
    const nl = text.indexOf('\n', i + 2);
    if (close === -1 || (nl !== -1 && nl < close)) {
      throw new SitefileError('E_TEMPLATE', "unterminated '${' in template", span);
    }
    const expr = text.slice(i + 2, close);

    if (BLOCK_RE.test(expr)) {
      tokens.push(blockToken(expr, span));
      // A block tag alone on its line: drop the line's indentation and newline.
      const before = text.slice(lineStart, i);
      const lineEnd = nl === -1 ? text.length : nl;
      if (/^[ \t]*$/.test(before) && /^[ \t\r]*$/.test(text.slice(close + 1, lineEnd))) {
        if (buf.endsWith(before)) buf = buf.slice(0, buf.length - before.length);
        const last = tokens.pop()!;
        flush();
        tokens.push(last);
        i = lineEnd;
        if (i < text.length) {
          i++; // the newline
          line++;
          lineStart = i;
        }
        continue;
      }
      const last = tokens.pop()!;
      flush();
      tokens.push(last);
      i = close + 1;
      continue;
    }

    const m = VAR_RE.exec(expr);
    if (!m) {
      throw new SitefileError(
        'E_TEMPLATE',
        `invalid template expression '\${${expr}}' (expected \${name}, \${name.key}, \${name|raw} or a block like \${#each list as item}; write $\${ for a literal '\${')`,
        span,
      );
    }
    flush();
    const [name, ...path] = m[1]!.split('.');
    tokens.push({ kind: 'var', name: name!, path, span, ...(m[2] ? { raw: true as const } : {}) });
    i = close + 1;
  }
  flush();
  return buildTree(tokens, file, { file, line, column: text.length - lineStart + 1 });
}

const REF_AT = new RegExp(String.raw`^${REF}`);

/** The code point at `i`, for messages (whole, also when it is outside the BMP). */
function charAt(text: string, i: number): string {
  return String.fromCodePoint(text.codePointAt(i)!);
}

/**
 * `${file <value> [| <helper> [<name>[=<value>]]…]}` from `start` (after `${`). A value is a
 * "double-quoted" string (`\` escapes, `${name}` references), a 'single-quoted' one (literal), or a
 * bare word (escapes and references, up to whitespace, `|` or `}`). Errors point at the `${`.
 */
function parseFileExpr(text: string, start: number, spanAt: (i: number) => SourceSpan, span: SourceSpan): { node: Extract<TemplateNode, { kind: 'file' }>; end: number } {
  const fail = (detail: string): SitefileError => new SitefileError('E_TEMPLATE', detail, span);
  const unterminated = (): SitefileError => fail("unterminated '${' in template");
  let i = start + 4;
  const ws = (): void => {
    while (text[i] === ' ' || text[i] === '\t') i++;
  };
  const atEnd = (): boolean => i >= text.length || text[i] === '\n';
  const value = (): HelperText => {
    const parts: Array<HelperText[number]> = [];
    const add = (s: string): void => {
      const last = parts[parts.length - 1];
      if (last?.kind === 'text') parts[parts.length - 1] = { kind: 'text', value: last.value + s };
      else parts.push({ kind: 'text', value: s });
    };
    if (text[i] === "'") {
      const close = text.indexOf("'", i + 1);
      const nl = text.indexOf('\n', i + 1);
      if (close === -1 || (nl !== -1 && nl < close)) throw fail('unterminated string in ${file …}');
      add(text.slice(i + 1, close));
      i = close + 1;
      return parts.length ? parts : [{ kind: 'text', value: '' }];
    }
    const quoted = text[i] === '"';
    if (quoted) i++;
    for (;;) {
      if (atEnd()) {
        if (quoted) throw fail('unterminated string in ${file …}');
        break;
      }
      const ch = text[i]!;
      if (quoted && ch === '"') {
        i++;
        break;
      }
      if (!quoted && (ch === ' ' || ch === '\t' || ch === '|' || ch === '}' || ch === '"' || ch === "'")) break;
      if (ch === '\\') {
        if (i + 1 >= text.length || text[i + 1] === '\n') throw quoted ? fail('unterminated string in ${file …}') : unterminated();
        const c = charAt(text, i + 1);
        add(c);
        i += 1 + c.length;
        continue;
      }
      if (ch === '$' && text[i + 1] === '{') {
        const m = REF_AT.exec(text.slice(i + 2));
        if (!m || text[i + 2 + m[0].length] !== '}') throw fail('expected ${name} or ${name.key} inside a ${file …} string');
        const [name, ...path] = m[0].split('.');
        parts.push({ kind: 'ref', name: name!, path, span: spanAt(i) });
        i += 3 + m[0].length;
        continue;
      }
      add(ch);
      i++;
    }
    return parts.length ? parts : [{ kind: 'text', value: '' }];
  };
  ws();
  if (atEnd() || text[i] === '}' || text[i] === '|') {
    if (atEnd()) throw unterminated();
    throw fail('expected a path after ${file, as in ${file "/img/a.png"}');
  }
  const path = value();
  ws();
  let helper: string | undefined;
  const attrs: Array<{ name: string; value?: HelperText }> = [];
  if (text[i] === '|') {
    i++;
    ws();
    const m = /^[a-z][a-z0-9-]*/.exec(text.slice(i));
    if (!m) throw fail(`expected a helper name after '|' (${FILE_HELPERS.join(', ')})`);
    helper = m[0];
    i += helper.length;
    if (!FILE_HELPERS.includes(helper)) throw fail(`unknown file helper '${helper}' (expected ${FILE_HELPERS.join(', ')})`);
    for (;;) {
      const before = i;
      ws();
      if (atEnd() || text[i] === '}') break;
      const n = /^[A-Za-z_:][A-Za-z0-9_:.-]*/.exec(text.slice(i));
      if (i === before || !n) throw fail(`expected an attribute (name or name="value") in \${file … | ${helper}}, got '${charAt(text, i)}'`);
      const name = n[0];
      i += name.length;
      let v: HelperText | undefined;
      if (text[i] === '=') {
        i++;
        if (atEnd() || text[i] === ' ' || text[i] === '\t' || text[i] === '}') throw fail(`expected a value after '${name}='`);
        v = value();
      }
      if (attrs.some((a) => a.name.toLowerCase() === name.toLowerCase())) throw fail(`attribute '${name}' given twice`);
      attrs.push(v === undefined ? { name } : { name, value: v });
    }
    if (attrs.length > 0 && (VALUE_HELPERS as readonly string[]).includes(helper)) throw fail(`the ${helper} helper takes no attributes`);
  }
  ws();
  if (atEnd()) throw unterminated();
  if (text[i] !== '}') throw fail(`expected | or } after the path in \${file …}, got '${charAt(text, i)}'`);
  return { node: { kind: 'file', path, ...(helper !== undefined ? { helper } : {}), attrs, span }, end: i + 1 };
}

/** Every `${name}` reference of a `${file …}` expression, in order. */
export function fileRefs(n: Extract<TemplateNode, { kind: 'file' }>): Array<Extract<HelperText[number], { kind: 'ref' }>> {
  return [n.path, ...n.attrs.map((a) => a.value ?? [])].flatMap((t) => t.filter((p) => p.kind === 'ref'));
}

/** A name a template reads from outside itself (not a `${#each}` variable of its own), in template order. */
export interface FreeRef {
  readonly name: string;
  readonly path: readonly string[];
  readonly span: SourceSpan;
  /** `var`: `${name…}`; `each`: the list of `${#each}`; `if`: the condition of `${#if}`; `text`: inside a `${file …}` string. */
  readonly use: 'var' | 'each' | 'if' | 'text';
}

/** The names a template reads that its own `${#each}` blocks do not bind, in order. */
export function freeRefs(nodes: readonly TemplateNode[]): FreeRef[] {
  const out: FreeRef[] = [];
  const walkNodes = (list: readonly TemplateNode[], bound: readonly string[]): void => {
    for (const n of list) {
      switch (n.kind) {
        case 'text':
          break;
        case 'var':
          if (!bound.includes(n.name)) out.push({ name: n.name, path: n.path, span: n.span, use: 'var' });
          break;
        case 'file':
          for (const r of fileRefs(n)) if (!bound.includes(r.name)) out.push({ name: r.name, path: r.path, span: r.span, use: 'text' });
          break;
        case 'each':
          if (!bound.includes(n.list.name)) out.push({ ...n.list, use: 'each' });
          walkNodes(n.body, [...bound, n.as]);
          walkNodes(n.empty, bound);
          break;
        case 'if':
          if (!bound.includes(n.cond.name)) out.push({ ...n.cond, use: 'if' });
          walkNodes(n.then, bound);
          walkNodes(n.otherwise, bound);
          break;
      }
    }
  };
  walkNodes(nodes, []);
  return out;
}

/** What renderTemplate needs beyond values: html fragments (`LET name: html`, `IMPORT … AS html`). */
export interface TemplateContext {
  /** Whether `name` is an html fragment (checked after loop variables, before `lookup`). */
  readonly isFragment?: (name: string) => boolean;
  /** The parsed template of fragment `name` (may throw, e.g. E_TEMPLATE for an imported partial). */
  readonly fragment?: (name: string) => readonly TemplateNode[];
  /** The stage's tree at this RENDER, for `${file …}`. */
  readonly tree?: TemplateTree;
  /** RENDER --inline-svg. */
  readonly inlineSvg?: boolean;
}

/** E_TYPE for a fragment used where a value is needed. */
export function fragmentNotValue(name: string, span: SourceSpan): SitefileError {
  return new SitefileError('E_TYPE', `'${name}' is an html fragment, not a value; insert it with \${${name}}`, span);
}

/** An error raised inside fragment `name`, with the place it was used added to the message. */
function inFragment(e: unknown, name: string, span: SourceSpan): unknown {
  if (!(e instanceof SitefileError)) return e;
  return new SitefileError(e.code, `${e.detail} (in html fragment '${name}' used at ${span.file}:${span.line}:${span.column})`, e.span ?? span, { cause: e });
}

function ref(text: string, span: SourceSpan): Ref {
  const [name, ...path] = text.split('.');
  return { name: name!, path, span };
}

function blockToken(expr: string, span: SourceSpan): Token {
  const word = BLOCK_RE.exec(expr)![1]!;
  if (word === '#each') {
    const m = EACH_RE.exec(expr);
    if (!m) throw new SitefileError('E_TEMPLATE', `expected \${#each list as item}, got '\${${expr}}'`, span);
    return { kind: 'open-each', list: ref(m[1]!, span), as: m[2]!, span };
  }
  if (word === '#if') {
    const m = IF_RE.exec(expr);
    if (!m) throw new SitefileError('E_TEMPLATE', `expected \${#if value} or \${#if !value}, got '\${${expr}}'`, span);
    return { kind: 'open-if', cond: ref(m[2]!, span), negate: m[1] === '!', span };
  }
  if (expr.trim() !== word) throw new SitefileError('E_TEMPLATE', `unexpected text in '\${${expr}}'`, span);
  return { kind: word === 'else' ? 'else' : word === '/each' ? 'close-each' : 'close-if', span };
}

function buildTree(tokens: readonly Token[], file: string, end: SourceSpan): TemplateNode[] {
  interface Frame {
    readonly open: Extract<Token, { kind: 'open-each' | 'open-if' }>;
    readonly first: TemplateNode[];
    second: TemplateNode[] | undefined;
  }
  const root: TemplateNode[] = [];
  const stack: Frame[] = [];
  const target = (): TemplateNode[] => {
    const f = stack[stack.length - 1];
    return f ? (f.second ?? f.first) : root;
  };
  for (const t of tokens) {
    switch (t.kind) {
      case 'text':
      case 'var':
      case 'file':
        target().push(t);
        break;
      case 'open-each':
      case 'open-if':
        stack.push({ open: t, first: [], second: undefined });
        break;
      case 'else': {
        const f = stack[stack.length - 1];
        if (!f) throw new SitefileError('E_TEMPLATE', '${else} outside of ${#if} or ${#each}', t.span);
        if (f.second) throw new SitefileError('E_TEMPLATE', 'a block can have only one ${else}', t.span);
        f.second = [];
        break;
      }
      case 'close-each':
      case 'close-if': {
        const f = stack.pop();
        const want = t.kind === 'close-each' ? 'open-each' : 'open-if';
        if (!f || f.open.kind !== want) {
          throw new SitefileError('E_TEMPLATE', `\${${t.kind === 'close-each' ? '/each' : '/if'}} without a matching opening tag`, t.span);
        }
        const o = f.open;
        const node: TemplateNode =
          o.kind === 'open-each'
            ? { kind: 'each', list: o.list, as: o.as, body: f.first, empty: f.second ?? [], span: o.span }
            : { kind: 'if', cond: o.cond, negate: o.negate, then: f.first, otherwise: f.second ?? [], span: o.span };
        target().push(node);
        break;
      }
    }
  }
  const open = stack.pop();
  if (open) {
    const tag = open.open.kind === 'open-each' ? '${#each}' : '${#if}';
    throw new SitefileError('E_TEMPLATE', `${tag} opened here is never closed (template ends at ${file}:${end.line})`, open.open.span);
  }
  return mergeText(root);
}

function mergeText(nodes: TemplateNode[]): TemplateNode[] {
  const out: TemplateNode[] = [];
  for (const n of nodes) {
    const prev = out[out.length - 1];
    if (n.kind === 'text' && prev?.kind === 'text') out[out.length - 1] = { kind: 'text', value: prev.value + n.value };
    else if (n.kind !== 'text' || n.value !== '') out.push(n);
  }
  return out;
}

/**
 * Render a parsed template. `lookup` resolves scalars; `values` resolves lists
 * and conditions for blocks (needed only if the template has blocks). Loop
 * variables are resolved here and shadow every other name. Escaping applies to
 * substituted values only, never to template text or `|raw` values.
 */
export function renderTemplate(
  nodes: readonly TemplateNode[],
  lookup: TemplateLookup,
  escape: EscapeMode,
  values?: TemplateValueLookup,
  ctx: TemplateContext = {},
): string {
  const scope = new Map<string, unknown>();
  /** Fragments being rendered, outermost first (a fragment that reaches itself is an error). */
  const using: string[] = [];
  const isFragment = (name: string): boolean => !scope.has(name) && (ctx.isFragment?.(name) ?? false);
  const valueOf = (r: Ref, loose: boolean): unknown => {
    if (scope.has(r.name)) return walk(scope.get(r.name), r.path, r.name, r.span, loose);
    if (isFragment(r.name)) throw fragmentNotValue(r.name, r.span);
    if (!values) throw new SitefileError('E_TEMPLATE', 'this template uses blocks, which need list values', r.span);
    return values(r.name, r.path, r.span, loose);
  };
  // A fragment is rendered in place: same escape mode, same loop variables. Its markup is never
  // escaped; the values it substitutes are.
  const fragment = (name: string, path: readonly string[], span: SourceSpan): string => {
    if (path.length > 0) throw fragmentNotValue(name, span);
    if (using.includes(name)) {
      throw new SitefileError('E_TEMPLATE', `html fragment '${name}' uses itself (${[...using.slice(using.indexOf(name)), name].join(' → ')})`, span);
    }
    using.push(name);
    try {
      return run(ctx.fragment!(name));
    } catch (e) {
      throw inFragment(e, name, span);
    } finally {
      using.pop();
    }
  };
  const run = (list: readonly TemplateNode[]): string => {
    let out = '';
    for (const n of list) {
      switch (n.kind) {
        case 'text':
          out += n.value;
          break;
        case 'file': {
          // References inside its strings are values (a fragment is not one).
          const textOf = (t: HelperText): string =>
            t
              .map((p) => {
                if (p.kind === 'text') return p.value;
                if (scope.has(p.name)) return String(resolveJsonPath(scope.get(p.name), p.path, p.name, p.span));
                if (isFragment(p.name)) throw fragmentNotValue(p.name, p.span);
                return String(lookup(p.name, p.path, p.span));
              })
              .join('');
          if (!ctx.tree) throw new SitefileError('E_TEMPLATE', '${file …} reads the tree of a RENDER, which this template has not', n.span);
          const attrs: HelperAttr[] = n.attrs.map((a) => (a.value === undefined ? { name: a.name } : { name: a.name, value: textOf(a.value) }));
          const r = renderFile(textOf(n.path), n.helper, attrs, ctx.tree, { inlineSvg: ctx.inlineSvg ?? false }, n.span);
          out += 'markup' in r ? r.markup : escape === 'html' ? escapeHtml(r.value) : r.value;
          break;
        }
        case 'var': {
          if (isFragment(n.name)) {
            out += fragment(n.name, n.path, n.span);
            break;
          }
          const v = scope.has(n.name)
            ? resolveJsonPath(scope.get(n.name), n.path, n.name, n.span)
            : lookup(n.name, n.path, n.span);
          const s = String(v);
          out += n.raw || escape !== 'html' ? s : escapeHtml(s);
          break;
        }
        case 'each': {
          // A missing key (post.tags on a post without tags) is an empty list;
          // an unknown top-level name is still an error, so typos are caught.
          const found = valueOf(n.list, n.list.path.length > 0);
          const items = found === undefined || found === null ? [] : found;
          if (!Array.isArray(items)) {
            throw new SitefileError('E_TYPE', `${dotted(n.list)} is ${describe(found)}, not a list; \${#each} needs a list`, n.list.span);
          }
          if (items.length === 0) {
            out += run(n.empty);
            break;
          }
          const saved = scope.has(n.as) ? { v: scope.get(n.as) } : undefined;
          for (const item of items) {
            scope.set(n.as, item);
            out += run(n.body);
          }
          if (saved) scope.set(n.as, saved.v);
          else scope.delete(n.as);
          break;
        }
        case 'if': {
          const truthy = isTruthy(valueOf(n.cond, true));
          out += run(truthy !== n.negate ? n.then : n.otherwise);
          break;
        }
      }
    }
    return out;
  };
  return run(nodes);
}

function dotted(r: Ref): string {
  return `'${[r.name, ...r.path].join('.')}'`;
}

export function isTruthy(v: unknown): boolean {
  if (Array.isArray(v)) return v.length > 0;
  return v !== undefined && v !== null && v !== false && v !== '' && v !== 0;
}

/**
 * Walk `value` along `path`. Strict mode fails like resolveJsonPath on a
 * missing key; loose mode returns undefined. Any value type may be returned.
 */
export function walk(value: unknown, path: readonly string[], name: string, span: SourceSpan, loose: boolean): unknown {
  let cur: unknown = value;
  let walked = name;
  for (const seg of path) {
    const parent = walked;
    walked += `.${seg}`;
    if (Array.isArray(cur) && /^(0|[1-9][0-9]*)$/.test(seg) && Number(seg) < cur.length) {
      cur = cur[Number(seg)];
    } else if (cur !== null && typeof cur === 'object' && !Array.isArray(cur) && Object.prototype.hasOwnProperty.call(cur, seg)) {
      cur = (cur as Record<string, unknown>)[seg];
    } else {
      if (loose) return undefined;
      throw new SitefileError('E_UNDEFINED', `'${walked}' is undefined ('${parent}' is ${describe(cur)})`, span);
    }
  }
  return cur;
}

const HTML_ESCAPES: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
};

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => HTML_ESCAPES[c] ?? c);
}

/** Decode strict UTF-8 (E_ENCODING on invalid bytes), stripping a leading BOM. */
export function decodeUtf8(bytes: Uint8Array, what: string, span?: SourceSpan): string {
  try {
    // ignoreBOM: false (default) strips a leading BOM.
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch (e) {
    throw new SitefileError('E_ENCODING', `${what} is not valid UTF-8`, span, { cause: e });
  }
}

/**
 * Walk a parsed JSON value along `path` and return the scalar found there.
 * Missing keys/indexes fail with E_UNDEFINED; null, objects and arrays fail
 * with E_TYPE. Messages name the full dotted path.
 */
export function resolveJsonPath(value: unknown, path: readonly string[], name: string, span: SourceSpan): Scalar {
  let cur: unknown = value;
  let walked = name;
  for (const seg of path) {
    const parent = walked;
    walked += `.${seg}`;
    if (Array.isArray(cur)) {
      if (!/^(0|[1-9][0-9]*)$/.test(seg) || Number(seg) >= cur.length) {
        throw new SitefileError(
          'E_UNDEFINED',
          `'${walked}' is undefined: '${parent}' is an array of length ${cur.length}`,
          span,
        );
      }
      cur = cur[Number(seg)];
    } else if (cur !== null && typeof cur === 'object') {
      if (!Object.prototype.hasOwnProperty.call(cur, seg)) {
        throw new SitefileError('E_UNDEFINED', `'${walked}' is undefined: '${parent}' has no key '${seg}'`, span);
      }
      cur = (cur as Record<string, unknown>)[seg];
    } else {
      throw new SitefileError(
        'E_UNDEFINED',
        `'${walked}' is undefined: '${parent}' is ${describe(cur)}, not an object or array`,
        span,
      );
    }
  }
  if (typeof cur === 'string' || typeof cur === 'number' || typeof cur === 'boolean') return cur;
  throw new SitefileError(
    'E_TYPE',
    `'${walked}' is ${describe(cur)}; templates can only substitute strings, numbers and booleans`,
    span,
  );
}

function describe(v: unknown): string {
  if (v === null) return 'null';
  if (Array.isArray(v)) return 'an array';
  if (typeof v === 'object') return 'an object';
  if (v === undefined) return 'undefined';
  return `a ${typeof v}`;
}
