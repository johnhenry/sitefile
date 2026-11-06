import { watch as fsWatch, promises as fs, type FSWatcher } from 'node:fs';
import path from 'node:path';
import { TEMP_PREFIX_PATTERN } from './exporter.js';

/** Quiet period after the last change before a rebuild starts. */
export const DEBOUNCE_MS = 100;

/**
 * Runs `build` once changes settle (no new change for `debounceMs`). Changes that
 * arrive while a build runs are collected and coalesced into one follow-up build.
 * `build` receives the paths reported since the previous build started.
 */
export class RebuildScheduler {
  private timer: NodeJS.Timeout | undefined;
  private running: Promise<void> | undefined;
  private changes = new Set<string>();
  private closed = false;

  constructor(
    private readonly build: (changed: readonly string[]) => Promise<void>,
    private readonly debounceMs = DEBOUNCE_MS,
  ) {}

  /** Record a change; (re)starts the debounce unless a build is running. */
  notify(changed: string): void {
    if (this.closed) return;
    this.changes.add(changed);
    if (this.running) return; // picked up when the running build finishes
    clearTimeout(this.timer);
    this.timer = setTimeout(() => void this.run(), this.debounceMs);
  }

  /** Build now (the initial build), then any follow-up. Resolves when idle. */
  run(): Promise<void> {
    if (this.running) return this.running;
    clearTimeout(this.timer);
    this.timer = undefined;
    const batch = [...this.changes];
    this.changes.clear();
    this.running = this.build(batch)
      .catch((e: unknown) => void process.stderr.write(`internal error: ${(e as Error)?.stack ?? String(e)}\n`))
      .finally(() => {
        this.running = undefined;
        if (this.changes.size && !this.closed) {
          clearTimeout(this.timer);
          this.timer = setTimeout(() => void this.run(), this.debounceMs);
        }
      });
    return this.running;
  }

  /** True while a build runs or one is scheduled. */
  get busy(): boolean {
    return this.running !== undefined || this.timer !== undefined;
  }

  close(): void {
    this.closed = true;
    clearTimeout(this.timer);
    this.timer = undefined;
  }
}

/** `p` is `dir` or inside it. */
export function isWithin(p: string, dir: string): boolean {
  const rel = path.relative(dir, p);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

/**
 * Paths a watcher never reacts to: anything under `.git`, the exporter's temp and
 * backup siblings (`.<name>.sitefile-tmp-*`, `.<name>.sitefile-old-*`), and any of
 * `ignored` (the output, the store, Sitefile.lock), checked at call time so the
 * set may grow between builds.
 */
export function isIgnoredChange(abs: string, roots: readonly string[], ignored: Iterable<string>): boolean {
  for (const q of ignored) if (isWithin(abs, q)) return true;
  const root = roots.find((r) => isWithin(abs, r));
  if (root === undefined) return false;
  return path
    .relative(root, abs)
    .split(path.sep)
    .some((seg) => seg === '.git' || TEMP_PREFIX_PATTERN.test(seg));
}

/** realpath of `p`, else of its parent joined with its name, else `p` resolved. */
export async function resolveReal(p: string): Promise<string> {
  const abs = path.resolve(p);
  try {
    return await fs.realpath(abs);
  } catch {
    try {
      return path.join(await fs.realpath(path.dirname(abs)), path.basename(abs));
    } catch {
      return abs;
    }
  }
}

export interface WatchSpec {
  /** Directories watched recursively (the build context). Absolute, real paths. */
  readonly dirs: readonly string[];
  /** Single files (the Sitefile, --build-arg-file files) outside `dirs`; their directory is watched for their name. */
  readonly files: readonly string[];
  /** Called with the absolute path of each change (the directory itself when the OS gives no name). */
  readonly onChange: (abs: string) => void;
}

/** Start fs.watch watchers for `spec`; returns a function that closes them. */
export function watchPaths(spec: WatchSpec): () => void {
  const watchers: FSWatcher[] = [];
  const add = (dir: string, recursive: boolean, names?: ReadonlySet<string>): void => {
    const w = fsWatch(dir, { recursive, persistent: true }, (_event, filename) => {
      const name = filename == null ? undefined : filename.toString();
      if (names && (name === undefined || !names.has(path.basename(name)))) return;
      spec.onChange(name === undefined ? dir : path.join(dir, name));
    });
    // A watched directory that disappears (or a platform error) must not crash the build loop.
    w.on('error', () => undefined);
    watchers.push(w);
  };
  for (const d of spec.dirs) add(d, true);
  const byDir = new Map<string, Set<string>>();
  for (const f of spec.files) {
    if (spec.dirs.some((d) => isWithin(f, d))) continue;
    const dir = path.dirname(f);
    const set = byDir.get(dir) ?? new Set<string>();
    set.add(path.basename(f));
    byDir.set(dir, set);
  }
  for (const [dir, names] of byDir) {
    try {
      add(dir, false, names);
    } catch {
      // the directory of a missing file: the build reports the missing file
    }
  }
  return () => {
    for (const w of watchers) w.close();
  };
}
