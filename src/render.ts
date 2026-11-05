import { SitefileError, type SourceSpan } from './errors.js';
import type { EscapeMode, TemplatePart } from './language/ast.js';
import type { Scalar } from './values.js';

/**
 * Resolves a template variable to a scalar. Supplied by the executor, which
 * throws E_UNDEFINED / E_TYPE itself for unknown names or non-scalar values.
 */
export type TemplateLookup = (name: string, path: readonly string[], span: SourceSpan) => Scalar;

const NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
const SEGMENT_RE = /^[A-Za-z0-9_-]+$/;

/**
 * Split a RENDER template into text and variable parts.
 *
 * - `${name}` / `${name.seg.seg}` is a variable (segments may be array indexes).
 * - `$${` writes a literal `${`; what follows is ordinary text.
 * - A `$` not followed by `{` is literal.
 * - Any other `${` (invalid or unterminated) fails with E_TEMPLATE at the `$`.
 */
export function parseRenderTemplate(text: string, file: string): TemplatePart[] {
  const parts: TemplatePart[] = [];
  let buf = '';
  let line = 1;
  let lineStart = 0;
  let i = 0;
  const spanAt = (idx: number): SourceSpan => ({ file, line, column: idx - lineStart + 1 });
  const flush = (): void => {
    if (buf !== '') parts.push({ kind: 'text', value: buf });
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
    // ch === '$'
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
    const [name, ...path] = expr.split('.');
    if (name === undefined || !NAME_RE.test(name) || !path.every((s) => SEGMENT_RE.test(s))) {
      throw new SitefileError(
        'E_TEMPLATE',
        `invalid template expression '\${${expr}}' (expected \${name} or \${name.key.key}; write $\${ for a literal '\${')`,
        span,
      );
    }
    flush();
    parts.push({ kind: 'var', name, path, span });
    i = close + 1;
  }
  flush();
  return parts;
}

/** Substitute template variables. Escaping applies to substituted values only. */
export function renderTemplate(parts: readonly TemplatePart[], lookup: TemplateLookup, escape: EscapeMode): string {
  let out = '';
  for (const part of parts) {
    if (part.kind === 'text') {
      out += part.value;
      continue;
    }
    const value = String(lookup(part.name, part.path, part.span));
    out += escape === 'html' ? escapeHtml(value) : value;
  }
  return out;
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
