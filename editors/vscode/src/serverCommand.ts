// Pure logic shared by the extension and its tests: no `vscode` import, so it
// runs (and is unit-tested) in plain Node.

import { accessSync, constants, statSync } from 'node:fs';
import * as path from 'node:path';

/** The client id; vscode-languageclient also reads `sitefile.trace.server` from it. */
export const CLIENT_ID = 'sitefile';
export const CLIENT_NAME = 'Sitefile Language Server';
export const LANGUAGE_ID = 'sitefile';

/** Saved and unsaved Sitefiles. */
export const DOCUMENT_SELECTOR = [
  { scheme: 'file', language: LANGUAGE_ID },
  { scheme: 'untitled', language: LANGUAGE_ID },
];

/** Sent as `initialize.params.initializationOptions` (the server needs none). */
export const INITIALIZATION_OPTIONS = {};

export const DEFAULT_SERVER_PATH = 'sitefile';
export const DEFAULT_SERVER_ARGS = ['lsp'];

/** At most this many crashes within RESTART_WINDOW_MS before giving up. */
export const MAX_RESTARTS = 4;
export const RESTART_WINDOW_MS = 3 * 60 * 1000;

export interface ServerSettings {
  /** `sitefile.server.path`: a command on PATH, an absolute path, or a path relative to the workspace folder. */
  path?: unknown;
  /** `sitefile.server.args` */
  args?: unknown;
}

export interface ResolveEnv {
  /** PATH (or Path on Windows). */
  PATH?: string;
  /** Windows executable extensions, e.g. ".EXE;.CMD". */
  PATHEXT?: string;
  /** Home directory, for a leading `~`. */
  home?: string;
  /** The first workspace folder's filesystem path, for relative paths. */
  workspaceFolder?: string;
  platform?: NodeJS.Platform;
  /** Overridable for tests: true when `file` is an executable regular file. */
  isExecutable?: (file: string) => boolean;
}

export type ResolvedCommand =
  | { ok: true; command: string; args: string[] }
  | { ok: false; reason: 'not-found'; requested: string; message: string }
  | { ok: false; reason: 'invalid'; requested: string; message: string };

export function defaultIsExecutable(file: string): boolean {
  try {
    if (!statSync(file).isFile()) return false;
    if (process.platform === 'win32') return true;
    accessSync(file, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function normalizeArgs(args: unknown): string[] | undefined {
  if (args === undefined || args === null) return [...DEFAULT_SERVER_ARGS];
  if (!Array.isArray(args) || !args.every((a) => typeof a === 'string')) return undefined;
  return [...args];
}

/**
 * Turn the `sitefile.server.*` settings into a command to spawn, or a reason
 * it can't be started. A bare name (no path separator) is looked up on PATH;
 * anything else is a file path (`~` expands to the home directory, a relative
 * path is resolved against the workspace folder).
 */
export function resolveServerCommand(settings: ServerSettings, env: ResolveEnv = {}): ResolvedCommand {
  const platform = env.platform ?? process.platform;
  const p = platform === 'win32' ? path.win32 : path.posix;
  const isExecutable = env.isExecutable ?? defaultIsExecutable;

  const raw = typeof settings.path === 'string' ? settings.path.trim() : settings.path;
  if (raw !== undefined && raw !== null && typeof raw !== 'string') {
    return { ok: false, reason: 'invalid', requested: String(raw), message: 'sitefile.server.path must be a string.' };
  }
  const requested = raw ? raw : DEFAULT_SERVER_PATH;
  const args = normalizeArgs(settings.args);
  if (!args) {
    return { ok: false, reason: 'invalid', requested, message: 'sitefile.server.args must be an array of strings.' };
  }

  const exts =
    platform === 'win32'
      ? ['', ...(env.PATHEXT ?? '.COM;.EXE;.BAT;.CMD').split(';').filter(Boolean)]
      : [''];
  const firstExecutable = (base: string): string | undefined => {
    for (const ext of exts) {
      const candidate = base + ext;
      if (isExecutable(candidate)) return candidate;
    }
    return undefined;
  };

  const hasSeparator = requested.includes('/') || (platform === 'win32' && requested.includes('\\'));
  const startsWithTilde = requested === '~' || requested.startsWith('~/') || requested.startsWith('~\\');

  if (!hasSeparator && !startsWithTilde) {
    const dirs = (env.PATH ?? '').split(p.delimiter).filter(Boolean);
    for (const dir of dirs) {
      const found = firstExecutable(p.join(dir, requested));
      if (found) return { ok: true, command: found, args };
    }
    return {
      ok: false,
      reason: 'not-found',
      requested,
      message: `'${requested}' was not found on PATH.`,
    };
  }

  let file = requested;
  if (startsWithTilde) {
    if (!env.home) {
      return { ok: false, reason: 'not-found', requested, message: `Can't expand '~' in '${requested}': no home directory.` };
    }
    file = p.join(env.home, requested.slice(1));
  } else if (!p.isAbsolute(file)) {
    if (!env.workspaceFolder) {
      return {
        ok: false,
        reason: 'not-found',
        requested,
        message: `'${requested}' is a relative path, but no workspace folder is open to resolve it against.`,
      };
    }
    file = p.resolve(env.workspaceFolder, file);
  }
  const found = firstExecutable(file);
  if (found) return { ok: true, command: found, args };
  return { ok: false, reason: 'not-found', requested, message: `'${file}' is not an executable file.` };
}

/**
 * Crash bookkeeping: `record(now)` returns true while the server may be
 * restarted (fewer than `max` crashes in the last `windowMs`).
 */
export class RestartBudget {
  private crashes: number[] = [];
  constructor(
    private readonly max = MAX_RESTARTS,
    private readonly windowMs = RESTART_WINDOW_MS,
  ) {}
  record(now: number): boolean {
    this.crashes = this.crashes.filter((t) => now - t < this.windowMs);
    this.crashes.push(now);
    return this.crashes.length <= this.max;
  }
  reset(): void {
    this.crashes = [];
  }
}
