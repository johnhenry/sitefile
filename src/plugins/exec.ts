import { spawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access, lstat, mkdtemp, readFile, realpath, rm, stat, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { SitefileError, type SourceSpan } from '../errors.js';
import { TOOL_PLACEHOLDER_RE } from '../language/validate.js';
import type { PluginResult } from './host.js';

/**
 * The exec adapter: runs an external program that the user allow-listed with
 * `--allow-exec`. There is no shell: the program is started with an argv array
 * (`shell: false`), in a fresh temporary directory, with an almost empty
 * environment and a closed stdin. Node's permission model cannot restrict a
 * native binary, so the tool runs with the user's privileges: the allow-list is
 * the control. See docs/PLUGINS.md.
 */

export interface ToolPolicy {
  /** Per-call time limit (default 60 s). */
  readonly timeoutMs: number;
  /** Largest output file accepted (default 64 MiB). */
  readonly maxOutputBytes: number;
}

export const DEFAULT_TOOL_POLICY: ToolPolicy = { timeoutMs: 60_000, maxOutputBytes: 64 * 1024 * 1024 };

/** An allow-listed program, resolved to an absolute real path. */
export interface ResolvedTool {
  readonly name: string;
  readonly path: string;
  readonly size: number;
  readonly mtimeMs: number;
}

/** The environment given to every tool: nothing but a fixed PATH (tools are started by absolute path). */
export const TOOL_ENV: Readonly<Record<string, string>> = { PATH: '/usr/bin:/bin' };

const FLAG_HINT = (name: string): string => `--allow-exec ${name}`;

/**
 * Resolve `--allow-exec name[=path]`. A bare name is looked up on PATH; an explicit
 * path must be absolute. The result is the real path (symlinks followed), so what
 * is printed, keyed and run is one file.
 */
export async function resolveTool(name: string, explicit: string | undefined, searchPath: string | undefined, span?: SourceSpan): Promise<ResolvedTool> {
  const candidates: string[] = [];
  if (explicit !== undefined) {
    if (!path.isAbsolute(explicit)) {
      throw new SitefileError('E_USAGE', `--allow-exec ${name}=${explicit}: the path must be absolute`, span);
    }
    candidates.push(explicit);
  } else {
    for (const dir of (searchPath ?? '').split(path.delimiter)) if (dir && path.isAbsolute(dir)) candidates.push(path.join(dir, name));
  }
  for (const c of candidates) {
    try {
      const real = await realpath(c);
      const st = await stat(real);
      if (!st.isFile()) continue;
      await access(real, constants.X_OK);
      return { name, path: real, size: st.size, mtimeMs: st.mtimeMs };
    } catch {
      // try the next PATH entry
    }
  }
  throw new SitefileError(
    'E_NOT_FOUND',
    explicit !== undefined ? `TOOL ${name}: ${explicit} is not an executable file` : `TOOL ${name}: no executable '${name}' found on PATH (or pass ${FLAG_HINT(name)}=/absolute/path)`,
    span,
  );
}

/** What the runner needs from one TRANSFORM call. */
export interface ToolCall {
  /** argv template with ${…} already interpolated; `{input}`, `{output}` and `{option.key}` are replaced here. */
  readonly args: readonly string[];
  readonly dest: string;
  readonly options: Readonly<Record<string, string>>;
  readonly input?: { readonly name: string; readonly bytes: Uint8Array };
}

const MAX_LOG_LINES = 20;
const MAX_LINE = 300;

/** A file extension safe to put in a temp file name (tools such as ffmpeg pick formats from it). */
function safeExt(name: string): string {
  const ext = path.extname(name);
  return /^\.[A-Za-z0-9]{1,12}$/.test(ext) ? ext : '';
}

/**
 * The argv after placeholder substitution. `{input}` and `{output}` become
 * *relative* names inside the call's temp dir (the tool's cwd), so filter syntaxes
 * with `:` or `,` need no escaping. Values are inserted verbatim into one argv entry;
 * they are never re-scanned, and never split.
 */
export function substituteArgs(args: readonly string[], inputFile: string | undefined, outputFile: string, options: Readonly<Record<string, string>>): string[] {
  return args.map((a) =>
    a.replace(TOOL_PLACEHOLDER_RE, (_m, name: string) => {
      if (name === 'input') return inputFile ?? '';
      if (name === 'output') return outputFile;
      return options[name.slice('option.'.length)] ?? '';
    }),
  );
}

export async function runTool(tool: ResolvedTool, call: ToolCall, policy: ToolPolicy, log: (line: string) => void, span?: SourceSpan): Promise<PluginResult> {
  const label = `TOOL ${tool.name}`;
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-tool-'));
  try {
    let inputFile: string | undefined;
    if (call.input) {
      inputFile = `input${safeExt(call.input.name)}`;
      await writeFile(path.join(dir, inputFile), call.input.bytes, { mode: 0o600 });
    }
    const outputFile = `output${safeExt(call.dest)}`;
    const argv = substituteArgs(call.args, inputFile, outputFile, call.options);

    const { code, signal, stderr, stdout, timedOut, oversized, spawnError } = await new Promise<{
      code: number | null;
      signal: NodeJS.Signals | null;
      stderr: string;
      stdout: string;
      timedOut: boolean;
      oversized: boolean;
      spawnError: string | undefined;
    }>((resolve) => {
      const child = spawn(tool.path, argv, { cwd: dir, env: { ...TOOL_ENV }, shell: false, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
      let stdout = '';
      let stderr = '';
      let timedOut = false;
      let oversized = false;
      let spawnError: string | undefined;
      // Keep only the tail of each stream; a chatty tool must not exhaust memory.
      const keep = (cur: string, chunk: string): string => (cur + chunk).slice(-16 * 1024);
      child.stdout.setEncoding('utf8').on('data', (c: string) => (stdout = keep(stdout, c)));
      child.stderr.setEncoding('utf8').on('data', (c: string) => (stderr = keep(stderr, c)));
      const timer = setTimeout(() => {
        timedOut = true;
        child.kill('SIGKILL');
      }, policy.timeoutMs);
      // Stop a tool that is filling the disk with an oversized output.
      const watch = setInterval(() => {
        stat(path.join(dir, outputFile)).then(
          (st) => {
            if (st.size > policy.maxOutputBytes && !oversized) {
              oversized = true;
              child.kill('SIGKILL');
            }
          },
          () => {},
        );
      }, 100);
      child.on('error', (e) => {
        spawnError = e.message;
      });
      child.on('close', (code, signal) => {
        clearTimeout(timer);
        clearInterval(watch);
        resolve({ code, signal, stderr, stdout, timedOut, oversized, spawnError });
      });
    });

    const tail = (s: string): string[] => s.split('\n').map((l) => l.trimEnd()).filter(Boolean).slice(-MAX_LOG_LINES).map((l) => (l.length > MAX_LINE ? `${l.slice(0, MAX_LINE)}…` : l));
    const fail = (why: string): never => {
      const lines = tail(stderr || stdout).slice(-8);
      throw new SitefileError('E_PLUGIN', `${label}: ${why}${lines.length ? `\n${lines.join('\n')}` : ''}`, span);
    };
    if (spawnError) fail(`could not start ${tool.path}: ${spawnError}`);
    if (timedOut) fail(`timed out after ${policy.timeoutMs} ms`);
    if (oversized) fail(`output is larger than ${policy.maxOutputBytes} bytes`);
    if (signal) fail(`stopped by ${signal}`);
    if (code !== 0) fail(`exited with code ${code}`);

    for (const line of tail(stdout)) log(`[tool ${tool.name}] ${line}`);
    for (const line of tail(stderr)) log(`[tool ${tool.name}] ${line}`);

    const out = path.join(dir, outputFile);
    const st = await lstat(out).catch(() => undefined);
    if (!st) fail(`did not write {output} (${outputFile})`);
    if (!st!.isFile()) fail(`{output} is not a regular file`);
    if (st!.size > policy.maxOutputBytes) fail(`output is larger than ${policy.maxOutputBytes} bytes`);
    return { kind: 'file', bytes: new Uint8Array(await readFile(out)) };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}
