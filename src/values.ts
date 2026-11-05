import { SitefileError, type SourceSpan } from './errors.js';
import type { Operand, ValueType } from './language/ast.js';

export type Scalar = string | number | boolean;

const NUMBER_RE = /^-?(?:\d+)(?:\.\d+)?(?:[eE][+-]?\d+)?$/;

/** Convert a raw string to a typed scalar, or fail with E_TYPE. */
export function coerce(raw: string, type: ValueType, span: SourceSpan | undefined, what: string): Scalar {
  switch (type) {
    case 'string':
      return raw;
    case 'number': {
      if (!NUMBER_RE.test(raw.trim())) throw new SitefileError('E_TYPE', `${what} expects a number, got '${raw}'`, span);
      return Number(raw);
    }
    case 'boolean': {
      if (raw === 'true') return true;
      if (raw === 'false') return false;
      throw new SitefileError('E_TYPE', `${what} expects true or false, got '${raw}'`, span);
    }
  }
}

/** Interpolate an instruction operand with ARG/LET bindings. */
export function evaluateOperand(op: Operand, env: ReadonlyMap<string, Scalar>): string {
  let out = '';
  for (const part of op.parts) {
    if (part.kind === 'text') {
      out += part.value;
      continue;
    }
    const v = env.get(part.name);
    if (v === undefined) throw new SitefileError('E_UNDEFINED', `undefined variable '${part.name}'`, part.span);
    out += String(v);
  }
  return out;
}
