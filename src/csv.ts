import { SitefileError } from './errors.js';

/**
 * RFC 4180 CSV parser (no dependencies).
 *
 * - Fields may be double-quoted; inside quotes `""` is a literal quote and
 *   delimiters / newlines are data. Records end at LF or CRLF; a trailing
 *   newline is optional; a leading UTF-8 BOM is stripped.
 * - Completely empty lines are skipped.
 * - A quote inside an unquoted field, text after a closing quote, or an
 *   unterminated quote is E_DATA with line and column.
 * - `header: true` (default): the first record names the columns (non-empty,
 *   unique) and every record must have the same number of fields. Values are
 *   always strings. With `header: false`, records are returned as string
 *   arrays and may have differing lengths.
 * - Rows are ordinary objects; a `__proto__` column is stored as plain data.
 */
export interface CsvOptions {
  header?: boolean;
  delimiter?: ',' | ';' | '\t';
}

interface Row {
  fields: string[];
  cols: number[];
  line: number;
}

export function parseCsv(text: string, file: string, options?: CsvOptions & { header?: true }): Array<Record<string, string>>;
export function parseCsv(text: string, file: string, options: CsvOptions & { header: false }): string[][];
export function parseCsv(text: string, file: string, options?: CsvOptions): Array<Record<string, string>> | string[][];
export function parseCsv(text: string, file: string, options: CsvOptions = {}): Array<Record<string, string>> | string[][] {
  const header = options.header ?? true;
  const delim = options.delimiter ?? ',';
  const rows = tokenize(text.charCodeAt(0) === 0xfeff ? text.slice(1) : text, file, delim);
  if (!header) return rows.map((r) => r.fields);
  const head = rows[0];
  if (!head) return [];

  const seen = new Set<string>();
  head.fields.forEach((name, i) => {
    const span = { file, line: head.line, column: head.cols[i] as number };
    if (name === '') throw new SitefileError('E_DATA', `empty CSV column name in header (column ${i + 1})`, span);
    if (seen.has(name)) throw new SitefileError('E_DATA', `duplicate CSV column name '${name}'`, span);
    seen.add(name);
  });

  return rows.slice(1).map((row) => {
    if (row.fields.length !== head.fields.length) {
      throw new SitefileError(
        'E_DATA',
        `expected ${head.fields.length} field${head.fields.length === 1 ? '' : 's'} but found ${row.fields.length}`,
        { file, line: row.line, column: 1 },
      );
    }
    // fromEntries defines own data properties, so `__proto__` is safe.
    return Object.fromEntries(head.fields.map((name, i) => [name, row.fields[i] as string]));
  });
}

export function parseCsvRecords(text: string, file: string): Array<Record<string, string>> {
  return parseCsv(text, file, { header: true });
}

function tokenize(text: string, file: string, delim: string): Row[] {
  const n = text.length;
  const rows: Row[] = [];
  let i = 0;
  let line = 1;
  let lineStart = 0;
  const fail = (detail: string, l: number, index: number, ls: number): never => {
    throw new SitefileError('E_DATA', detail, { file, line: l, column: index - ls + 1 });
  };

  while (i < n) {
    // Skip fully empty lines.
    if (text[i] === '\n') {
      i++;
      line++;
      lineStart = i;
      continue;
    }
    if (text[i] === '\r' && text[i + 1] === '\n') {
      i += 2;
      line++;
      lineStart = i;
      continue;
    }

    const row: Row = { fields: [], cols: [], line };
    for (;;) {
      row.cols.push(i - lineStart + 1);
      let field = '';
      if (text[i] === '"') {
        const qLine = line;
        const qIndex = i;
        const qStart = lineStart;
        i++;
        for (;;) {
          if (i >= n) return fail('unterminated quoted field', qLine, qIndex, qStart);
          const c = text[i] as string;
          if (c === '"') {
            if (text[i + 1] === '"') {
              field += '"';
              i += 2;
              continue;
            }
            i++;
            break;
          }
          if (c === '\n') {
            line++;
            lineStart = i + 1;
          }
          field += c;
          i++;
        }
        const after = text[i];
        const atEnd = i >= n || after === delim || after === '\n' || (after === '\r' && text[i + 1] === '\n');
        if (!atEnd) return fail('unexpected character after closing quote', line, i, lineStart);
      } else {
        const s = i;
        while (i < n) {
          const c = text[i] as string;
          if (c === delim || c === '\n' || (c === '\r' && text[i + 1] === '\n')) break;
          if (c === '"') return fail('quote inside an unquoted field', line, i, lineStart);
          i++;
        }
        field = text.slice(s, i);
      }
      row.fields.push(field);

      if (i >= n) break;
      if (text[i] === delim) {
        i++;
        continue;
      }
      // Record terminator.
      i += text[i] === '\r' ? 2 : 1;
      line++;
      lineStart = i;
      break;
    }
    rows.push(row);
  }
  return rows;
}
