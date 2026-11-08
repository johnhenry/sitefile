import { parseArgs, type ParseArgsConfig } from 'node:util';
import { errorJson, SitefileError } from '../errors.js';

type Options = NonNullable<ParseArgsConfig['options']>;

export type ParsedValues = Record<string, string | boolean | Array<string | boolean> | undefined>;

export interface Command {
  readonly name: string;
  /** One line for the command list. */
  readonly summary: string;
  readonly usage: string;
  readonly description: string;
  /** Help lines for the options, already formatted. */
  readonly flags: readonly string[];
  readonly options: Options;
  /** Positional arguments allowed (default true). */
  readonly run: (values: ParsedValues, positionals: string[]) => Promise<number>;
}

export function commandHelp(c: Command): string {
  return `Usage: sitefile ${c.usage}\n\n${c.description}\n${c.flags.length ? `\nOptions:\n${c.flags.map((f) => `  ${f}`).join('\n')}\n` : ''}`;
}

export function usageError(message: string): number {
  process.stderr.write(`error: ${message}\nRun 'sitefile --help' for usage.\n`);
  return 2;
}

/** Parse `args` for `command` and run it, mapping SitefileError to the CLI exit codes (1, or 2 for E_USAGE). */
export async function runCommand(command: Command, args: readonly string[]): Promise<number> {
  let parsed;
  try {
    parsed = parseArgs({ args: [...args], allowPositionals: true, options: { ...command.options, help: { type: 'boolean', short: 'h' } } });
  } catch (e) {
    return usageError(`${command.name}: ${(e as Error).message}`);
  }
  if (parsed.values.help) {
    process.stdout.write(commandHelp(command));
    return 0;
  }
  try {
    return await command.run(parsed.values as ParsedValues, parsed.positionals);
  } catch (e) {
    if (e instanceof SitefileError) {
      if (e.code === 'E_USAGE') return usageError(e.detail);
      // With --json, the failure is also machine-readable on stdout (code, span, needs).
      if ((parsed.values as ParsedValues).json === true) process.stdout.write(errorJson(e));
      process.stderr.write(`error[${e.code}]: ${e.message}\n`);
      return 1;
    }
    throw e;
  }
}

export function usage(message: string): never {
  throw new SitefileError('E_USAGE', message);
}

export function requireArgs(positionals: string[], min: number, max: number, what: string): void {
  if (positionals.length < min) usage(`expected ${what}`);
  if (positionals.length > max) usage(`unexpected arguments: ${positionals.slice(max).join(' ')}`);
}

/** Left-aligned columns separated by two spaces, like `docker images`. */
export function table(rows: readonly (readonly string[])[]): string {
  if (rows.length === 0) return '';
  const widths: number[] = [];
  for (const r of rows) r.forEach((c, i) => (widths[i] = Math.max(widths[i] ?? 0, c.length)));
  return rows.map((r) => r.map((c, i) => (i === r.length - 1 ? c : c.padEnd(widths[i] ?? 0))).join('  ').trimEnd()).join('\n') + '\n';
}

/** Drop trailing zeros only after a decimal point ("1.50" -> "1.5", "100" stays). */
function stripDecimalZeros(s: string): string {
  return s.includes('.') ? s.replace(/\.?0+$/, '') : s;
}

/** 1.2kB, 34.5MB (decimal units, as Docker prints them). */
export function humanSize(bytes: number): string {
  if (bytes < 1000) return `${bytes}B`;
  const units = ['kB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let i = -1;
  do {
    v /= 1000;
    i++;
  } while (v >= 1000 && i < units.length - 1);
  return `${v >= 100 ? v.toFixed(0) : stripDecimalZeros(v.toPrecision(3))}${units[i]}`;
}

export function relativeTime(iso: string, now: number = Date.now()): string {
  const s = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
  if (Number.isNaN(s)) return 'unknown';
  if (s < 5) return 'Less than a second ago';
  const units: Array<[number, string]> = [
    [31536000, 'year'],
    [2592000, 'month'],
    [604800, 'week'],
    [86400, 'day'],
    [3600, 'hour'],
    [60, 'minute'],
    [1, 'second'],
  ];
  for (const [n, name] of units) {
    if (s >= n) {
      const k = Math.floor(s / n);
      return `${k} ${name}${k === 1 ? '' : 's'} ago`;
    }
  }
  return 'Less than a second ago';
}

export function str(v: ParsedValues[string]): string | undefined {
  return typeof v === 'string' ? v : undefined;
}

export function strs(v: ParsedValues[string]): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : typeof v === 'string' ? [v] : [];
}
