// Frontmatter and data schemas: `IMPORT posts FROM ./posts AS markdown SCHEMA ./post.schema.yaml`.
//
// A schema is a small, deterministic subset of JSON Schema, written as a YAML or JSON mapping:
//
//   required: [title, date]          # keys every document must have (null counts as missing)
//   properties:                      # key -> rule; keys not listed are not checked
//     title: string                  # a type name ...
//     tags: { type: list, items: string }
//     status: { enum: [draft, published] }
//
// Types: string, number, boolean, date (YYYY-MM-DD or an RFC 3339 date-time, as FEED reads it) and list
// (`items` gives the type of its elements). `enum` lists the allowed strings, numbers or booleans (for a list,
// the allowed elements). In a CSV import every cell is a string, so `number` also accepts a numeric string and
// `boolean` the strings `true` and `false`, enum entries match their text, and an empty cell counts as missing. docs/LANGUAGE.md has the rules.
import { SitefileError, type SourceSpan } from './errors.js';
import { parseFeedDate } from './site.js';

export type SchemaType = 'string' | 'number' | 'boolean' | 'date' | 'list';
type ItemType = Exclude<SchemaType, 'list'>;
type EnumValue = string | number | boolean;

const TYPES: readonly SchemaType[] = ['string', 'number', 'boolean', 'date', 'list'];
const ITEM_TYPES: readonly ItemType[] = ['string', 'number', 'boolean', 'date'];
const NUMERIC = /^-?\d+(\.\d+)?([eE][+-]?\d+)?$/;

/** What a value of each type looks like, for messages. */
const EXPECTED: Readonly<Record<SchemaType, string>> = {
  string: 'a string',
  number: 'a number',
  boolean: 'true or false',
  date: 'a date (YYYY-MM-DD or an RFC 3339 date-time)',
  list: 'a list',
};

export interface SchemaRule {
  readonly key: string;
  readonly type?: SchemaType;
  readonly items?: ItemType;
  readonly enum?: readonly EnumValue[];
}

/** A compiled schema; `label` names its file in messages. */
export interface Schema {
  readonly label: string;
  readonly required: readonly string[];
  readonly properties: readonly SchemaRule[];
}

const isObject = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Compile a parsed schema file. Errors are E_SCHEMA at `span` (the SCHEMA clause), prefixed with `what`. */
export function compileSchema(value: unknown, label: string, what: string, span: SourceSpan): Schema {
  const fail = (m: string): never => {
    throw new SitefileError('E_SCHEMA', `${what}: schema '${label}' ${m}`, span);
  };
  if (!isObject(value)) fail('must be a mapping (with required and/or properties)');
  const root = value as Record<string, unknown>;
  for (const k of Object.keys(root)) {
    if (k !== 'required' && k !== 'properties') fail(`has an unknown key '${k}' (expected required or properties)`);
  }
  const required: string[] = [];
  if (root['required'] !== undefined) {
    const r = root['required'];
    if (!Array.isArray(r) || !r.every((k) => typeof k === 'string' && k !== '')) fail("needs 'required' to be a list of key names");
    for (const k of r as string[]) if (!required.includes(k)) required.push(k);
  }
  const properties: SchemaRule[] = [];
  if (root['properties'] !== undefined) {
    const p = root['properties'];
    if (!isObject(p)) fail("needs 'properties' to be a mapping of key names to rules");
    for (const [key, rule] of Object.entries(p as Record<string, unknown>)) {
      const at = `property '${key}':`;
      const typeName = (t: unknown, allowed: readonly string[], kind: string): string => {
        if (typeof t !== 'string' || !allowed.includes(t)) {
          const shown = typeof t === 'string' ? ` '${t}'` : '';
          fail(`${at} unknown ${kind}${shown} (expected ${allowed.slice(0, -1).join(', ')} or ${allowed[allowed.length - 1]})`);
        }
        return t as string;
      };
      if (typeof rule === 'string') {
        properties.push({ key, type: typeName(rule, TYPES, 'type') as SchemaType });
        continue;
      }
      if (!isObject(rule)) fail(`${at} expected a type name or a mapping with type, items or enum`);
      const r = rule as Record<string, unknown>;
      for (const k of Object.keys(r)) {
        if (k !== 'type' && k !== 'items' && k !== 'enum') fail(`${at} unknown key '${k}' (expected type, items or enum)`);
      }
      const type = r['type'] !== undefined ? (typeName(r['type'], TYPES, 'type') as SchemaType) : undefined;
      let items: ItemType | undefined;
      if (r['items'] !== undefined) {
        if (type !== 'list') fail(`${at} items needs type list`);
        items = typeName(r['items'], ITEM_TYPES, 'items type') as ItemType;
      }
      let values: EnumValue[] | undefined;
      if (r['enum'] !== undefined) {
        const e = r['enum'];
        if (!Array.isArray(e) || e.length === 0 || !e.every((x) => typeof x === 'string' || typeof x === 'number' || typeof x === 'boolean')) {
          fail(`${at} enum must be a non-empty list of strings, numbers or booleans`);
        }
        values = e as EnumValue[];
      }
      properties.push({ key, ...(type ? { type } : {}), ...(items ? { items } : {}), ...(values ? { enum: values } : {}) });
    }
  }
  return { label, required, properties };
}

/** How a value is shown in a mismatch message. */
function describe(v: unknown): string {
  if (v === null) return 'null';
  if (typeof v === 'string') return `string '${v}'`;
  if (typeof v === 'number') return `number ${String(v)}`;
  if (typeof v === 'boolean') return `boolean ${String(v)}`;
  if (Array.isArray(v)) return 'a list';
  return 'an object';
}

function hasType(v: unknown, t: SchemaType, csv: boolean): boolean {
  switch (t) {
    case 'string':
      return typeof v === 'string';
    case 'number':
      return typeof v === 'number' || (csv && typeof v === 'string' && NUMERIC.test(v.trim()));
    case 'boolean':
      return typeof v === 'boolean' || (csv && (v === 'true' || v === 'false'));
    case 'date':
      return typeof v === 'string' && parseFeedDate(v) !== undefined;
    case 'list':
      return Array.isArray(v);
  }
}

function inEnum(v: unknown, values: readonly EnumValue[], csv: boolean): boolean {
  return values.some((e) => e === v || (csv && typeof v === 'string' && v === String(e)));
}

/**
 * Check an import's value against a schema: each element of a list, or a single document. Every element must be
 * an object (E_TYPE otherwise). The first mismatch fails with E_SCHEMA at `span`, naming the document (its `path`,
 * or `#n` for the n-th element), the key, what was expected and what was found.
 */
export function checkSchema(value: unknown, schema: Schema, what: string, csv: boolean, span: SourceSpan): void {
  const docs = Array.isArray(value) ? value : [value];
  docs.forEach((doc, i) => {
    if (!isObject(doc)) {
      throw new SitefileError('E_TYPE', `${what}: SCHEMA checks documents (objects), but element ${i + 1} is ${describe(doc)}`, span);
    }
    const name = typeof doc['path'] === 'string' ? doc['path'] : `#${i + 1}`;
    const fail = (m: string): never => {
      throw new SitefileError('E_SCHEMA', `${what}: document '${name}' ${m}`, span);
    };
    // A CSV cell is always there: an empty one counts as missing.
    const missing = (v: unknown): boolean => v === undefined || v === null || (csv && v === '');
    for (const key of schema.required) {
      if (missing(Object.hasOwn(doc, key) ? doc[key] : undefined)) fail(`has no '${key}' (required by schema '${schema.label}')`);
    }
    for (const rule of schema.properties) {
      const v = Object.hasOwn(doc, rule.key) ? doc[rule.key] : undefined;
      if (missing(v)) continue;
      const enumText = (values: readonly EnumValue[]): string => values.map((e) => String(e)).join(', ');
      if (rule.type !== undefined && !hasType(v, rule.type, csv)) fail(`has '${rule.key}' ${describe(v)}; it should be ${EXPECTED[rule.type]}`);
      if (rule.type === 'list') {
        (v as unknown[]).forEach((item, j) => {
          if (rule.items !== undefined && !hasType(item, rule.items, csv)) {
            fail(`has '${rule.key}' item ${j + 1} ${describe(item)}; it should be ${EXPECTED[rule.items]}`);
          }
          if (rule.enum !== undefined && !inEnum(item, rule.enum, csv)) fail(`has '${rule.key}' item ${j + 1} ${describe(item)}; it should be one of ${enumText(rule.enum)}`);
        });
      } else if (rule.enum !== undefined && !inEnum(v, rule.enum, csv)) {
        fail(`has '${rule.key}' ${describe(v)}; it should be one of ${enumText(rule.enum)}`);
      }
    }
  });
}
