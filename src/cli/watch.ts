import { promises as fs } from 'node:fs';
import path from 'node:path';
import { SitefileError } from '../errors.js';
import { resolveStoreHome } from '../store.js';
import { DEFAULT_POLL_MS, isIgnoredChange, RebuildScheduler, resolveReal, watchPaths } from '../watch.js';

export interface BuildOutcome {
  /** CLI exit code of the build: 0 ok, 1 build error (already printed), 2 usage error (already printed). */
  readonly code: number;
  /** Paths the build writes (the export destination): changes there never trigger a rebuild. */
  readonly wrote?: readonly string[];
}

export interface WatchLoopOptions {
  /** The build context, as given (shown in messages). */
  readonly context: string;
  /** `-f`: the Sitefile path (default `<context>/Sitefile`). */
  readonly sitefile?: string;
  /** `--build-arg-file` files; they are re-read on every build. */
  readonly argFiles: readonly string[];
  readonly build: () => Promise<BuildOutcome>;
  /** After the first build, before the "Watching" line (serve starts listening here). */
  readonly onStarted?: () => Promise<void>;
  /** `--poll`: poll every this many milliseconds instead of using file system events. */
  readonly pollMs?: number;
  /** `serve --watch ./dir` without a Sitefile: the directory is re-read, not built (other messages). */
  readonly plain?: boolean;
  /** Resolves to stop the loop (default: SIGINT or SIGTERM). */
  readonly stop?: Promise<void>;
  /** Filled in with `rebuild()`: build again now, as if a file had changed (`serve --watch`, key `r`). */
  readonly control?: { rebuild?: () => void };
  /** Where the loop's own lines go (each ends in a newline; `err` for stderr). Default: stdout and stderr (`serve -d --watch` stamps them into its log). */
  readonly say?: (line: string, err: boolean) => void;
}

/**
 * `--poll` / `--poll-interval <ms>` (the interval implies polling): the poll interval in
 * milliseconds, or undefined for file system events. Throws E_USAGE for a bad interval.
 */
export function pollOption(poll: boolean | undefined, interval: string | undefined): number | undefined {
  if (interval === undefined) return poll ? DEFAULT_POLL_MS : undefined;
  const n = /^[0-9]{1,9}$/.test(interval) ? Number(interval) : NaN;
  if (!(n >= 10)) throw new SitefileError('E_USAGE', `--poll-interval expects a whole number of milliseconds (10 or more), got '${interval}'`);
  return n;
}

const MSG_CHANGE = 'Change detected, rebuilding\n';
const MSG_FAILED = 'Build failed; keeping the last good output\n';
const MSG_CHANGE_PLAIN = 'Change detected, re-reading\n';
const MSG_FAILED_PLAIN = 'Read failed; serving the last good copy\n';

function signalled(): Promise<void> {
  return new Promise((resolve) => {
    process.once('SIGINT', () => resolve());
    process.once('SIGTERM', () => resolve());
  });
}

/**
 * `build --watch` / `serve --watch`: build once, then rebuild whenever a file in the
 * context, the Sitefile or a --build-arg-file changes (debounced; changes during a build
 * coalesce into one follow-up). A failed rebuild is reported and the loop keeps going.
 * Returns the exit code: 0 on Ctrl-C, 2 when the first build is a usage error.
 */
export async function watchLoop(o: WatchLoopOptions): Promise<number> {
  const st = await fs.stat(o.context).catch(() => undefined);
  if (!st?.isDirectory()) throw new SitefileError('E_NOT_FOUND', `'${o.context}' is not a directory`);
  const root = await fs.realpath(o.context);
  const sitefile = await resolveReal(o.sitefile ?? path.join(o.context, 'Sitefile'));
  const argFiles = await Promise.all(o.argFiles.map(resolveReal));
  // The build itself writes the export, Sitefile.lock and (maybe) the store; none of that is input.
  const ignored = new Set<string>([await resolveReal(resolveStoreHome()), path.join(path.dirname(sitefile), 'Sitefile.lock')]);
  const relevant = (p: string): boolean => !isIgnoredChange(p, [root], ignored);

  const say = o.say ?? ((line: string, err: boolean) => void (err ? process.stderr : process.stdout).write(line));
  let finish!: (code: number) => void;
  const finished = new Promise<number>((r) => (finish = r));
  let first = true;
  const scheduler = new RebuildScheduler(async (changed) => {
    if (!first) {
      if (!changed.some(relevant)) return; // only our own writes
      say(o.plain ? MSG_CHANGE_PLAIN : MSG_CHANGE, false);
    }
    const r = await o.build();
    for (const w of r.wrote ?? []) {
      ignored.add(path.resolve(w));
      ignored.add(await resolveReal(w));
    }
    if (!first) {
      if (r.code !== 0) say(o.plain ? MSG_FAILED_PLAIN : MSG_FAILED, true);
      return;
    }
    first = false;
    if (r.code === 2) return finish(2);
    try {
      await o.onStarted?.();
    } catch (e) {
      if (!(e instanceof SitefileError)) throw e;
      say(`error[${e.code}]: ${e.message}\n`, true);
      return finish(1);
    }
    say(`Watching ${o.context} for changes (Ctrl-C to stop)\n`, false);
  });
  if (o.control) o.control.rebuild = () => scheduler.notify(sitefile);
  const close = watchPaths({
    dirs: [root],
    files: [sitefile, ...argFiles],
    onChange: (p) => {
      if (relevant(p)) scheduler.notify(p);
    },
    ...(o.pollMs !== undefined ? { pollMs: o.pollMs, ignore: (p: string) => !relevant(p) } : {}),
  });
  void (o.stop ?? signalled()).then(() => finish(0));
  try {
    await scheduler.run();
    return await finished;
  } finally {
    close();
    scheduler.close();
  }
}
