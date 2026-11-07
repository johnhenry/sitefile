// What a `RENDER … FOR x IN list [BY key] [PAGE n [FIRST <dest>]]` loop renders (docs/LANGUAGE.md, "Tag pages,
// pagination and drafts"):
//
// - BY key: one element per distinct value of `key` (a list field gives one per element), `{ key, count, items }`,
//   keys in SORT BY order (numeric strings numerically, otherwise UTF-16 order); items keep the list's order.
// - PAGE n: the list (or each group) cut into pages of n items: `{ [key,] count, items, number, total }`; the
//   executor adds `url`, `prev` and `next` once it knows each page's destination. An empty list (without BY) is
//   one page with no items, so an index always exists.
import { SitefileError, type SourceSpan } from './errors.js';
import { compareValues } from './resources/documents.js';

type Doc = Record<string, unknown>;
const isObject = (v: unknown): v is Doc => v !== null && typeof v === 'object' && !Array.isArray(v);

/** Group documents by a field; `what` prefixes messages (`FOR tag IN posts BY tags`). */
export function groupBy(items: readonly unknown[], key: string, what: string, span: SourceSpan): Doc[] {
  const groups = new Map<string, unknown[]>();
  items.forEach((item, i) => {
    if (!isObject(item)) throw new SitefileError('E_TYPE', `${what}: element ${i + 1} is not a document (an object with a '${key}' field)`, span);
    const v = item[key];
    const name = typeof item['path'] === 'string' ? item['path'] : `#${i + 1}`;
    const seen = new Set<string>();
    for (const x of Array.isArray(v) ? v : [v]) {
      if (x === undefined || x === null || x === '') continue;
      if (typeof x !== 'string' && typeof x !== 'number' && typeof x !== 'boolean') {
        throw new SitefileError('E_TYPE', `${what}: '${key}' of document '${name}' is not text or a list of text`, span);
      }
      const k = String(x);
      if (seen.has(k)) continue;
      seen.add(k);
      const g = groups.get(k);
      if (g) g.push(item);
      else groups.set(k, [item]);
    }
  });
  const keys = [...groups.keys()].sort((a, b) => compareValues(a, b));
  return keys.map((k) => ({ key: k, count: groups.get(k)!.length, items: groups.get(k)! }));
}

/** Cut each group (or the whole list, as one group without a key) into pages of `size` items. */
export function paginate(groups: ReadonlyArray<{ readonly key?: string; readonly items: readonly unknown[] }>, size: number): Doc[][] {
  return groups.map((g) => {
    const total = Math.max(1, Math.ceil(g.items.length / size));
    return Array.from({ length: total }, (_, i) => ({
      ...(g.key !== undefined ? { key: g.key } : {}),
      count: g.items.length,
      items: g.items.slice(i * size, (i + 1) * size),
      number: i + 1,
      total,
    }));
  });
}
