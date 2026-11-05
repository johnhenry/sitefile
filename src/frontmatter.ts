import { SitefileError } from './errors.js';
import { parseYamlDocument } from './yaml.js';

/**
 * Frontmatter parser. The block between the `---` lines is parsed with the
 * YAML subset documented in `yaml.ts` (nested maps and lists, flow collections,
 * block scalars, quoted strings, ...), but it must be a mapping at the top level.
 *
 * Frontmatter exists only when the text starts (after an optional BOM) with a
 * `---` line. It ends at the next line that is exactly `---` or `...`.
 *
 * Every error is reported as E_FRONTMATTER with the Sitefile-file line and column.
 */
export type FrontmatterValue = string | number | boolean | null | FrontmatterValue[] | { [key: string]: FrontmatterValue };

export interface ParsedFrontmatter {
  data: { [key: string]: FrontmatterValue };
  body: string;
  /** 1-based line of the file on which `body` starts. */
  bodyLine: number;
}

export function parseFrontmatter(text: string, file: string): ParsedFrontmatter {
  const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;

  // Split into lines, remembering where each starts so the body can be sliced.
  const lines: string[] = [];
  const starts: number[] = [];
  for (let pos = 0; pos <= src.length; ) {
    let nl = src.indexOf('\n', pos);
    if (nl === -1) nl = src.length;
    starts.push(pos);
    lines.push(src.slice(pos, nl).replace(/\r$/, ''));
    pos = nl + 1;
  }

  if (lines[0] !== '---') return { data: {}, body: text, bodyLine: 1 };

  let close = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i] === '---' || lines[i] === '...') {
      close = i;
      break;
    }
  }
  if (close === -1) {
    throw new SitefileError('E_FRONTMATTER', "unterminated frontmatter: missing closing '---'", { file, line: 1, column: 1 });
  }

  const data = parseBlock(lines.slice(1, close).join('\n'), file);
  const bodyStart = close + 1 < starts.length ? (starts[close + 1] ?? src.length) : src.length;
  return { data, body: src.slice(bodyStart), bodyLine: close + 2 };
}

function parseBlock(block: string, file: string): { [key: string]: FrontmatterValue } {
  try {
    // The block starts on file line 2 (line 1 is the opening '---').
    const value = parseYamlDocument(block, file, 2, true);
    return (value ?? {}) as { [key: string]: FrontmatterValue };
  } catch (e) {
    if (e instanceof SitefileError && e.code === 'E_DATA') {
      throw new SitefileError('E_FRONTMATTER', e.detail, e.span, { cause: e });
    }
    throw e;
  }
}
