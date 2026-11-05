import { SitefileError, type SourceSpan } from './errors.js';
import type { EscapeMode } from './language/ast.js';
import type { Scalar } from './values.js';

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
export function parseRenderTemplate(text: string, file: string, firstLine = 1): TemplateNode[] {
  const tokens: Token[] = [];
  let buf = '';
  // firstLine > 1 when the template is a heredoc body inside the Sitefile.
  let line = firstLine;
  let lineStart = 0;
  let i = 0;
  const spanAt = (idx: number): SourceSpan => ({ file, line, column: idx - lineStart + 1 });
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
): string {
  const scope = new Map<string, unknown>();
  const valueOf = (r: Ref, loose: boolean): unknown => {
    if (scope.has(r.name)) return walk(scope.get(r.name), r.path, r.name, r.span, loose);
    if (!values) throw new SitefileError('E_TEMPLATE', 'this template uses blocks, which need list values', r.span);
    return values(r.name, r.path, r.span, loose);
  };
  const run = (list: readonly TemplateNode[]): string => {
    let out = '';
    for (const n of list) {
      switch (n.kind) {
        case 'text':
          out += n.value;
          break;
        case 'var': {
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
