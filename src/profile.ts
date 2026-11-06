import { performance } from 'node:perf_hooks';

/**
 * `--profile <file>`: a timing trace of one build in the Chrome trace event format
 * (open it in chrome://tracing, https://ui.perfetto.dev or speedscope).
 *
 * Every event is a complete event (`ph: "X"`, microseconds) on process 1. The
 * build runs on thread 1; import N (0-based, in Sitefile order) on thread N+2,
 * because imports resolve concurrently. Events appear in the file in the order
 * they started in the Sitefile (imports in declaration order), so two builds of
 * the same Sitefile list the same events in the same order with the same names,
 * categories and args; only `ts` and `dur` differ. The Rust implementation
 * (rust/src/profile.rs) writes the same events.
 */
export type ProfileArgs = Record<string, string | number | boolean>;

interface Slot {
  readonly name: string;
  readonly cat: string;
  readonly tid: number;
  args: ProfileArgs;
  ts?: number;
  dur?: number;
}

export interface ProfileSpan {
  /** Start the clock (for reserved spans; `begin` starts it at once). */
  start(): void;
  /** Stop the clock, adding args. */
  end(args?: ProfileArgs): void;
}

const NOOP: ProfileSpan = { start() {}, end() {} };

export class Profiler {
  private readonly t0 = performance.now();
  private readonly slots: Slot[] = [];
  private readonly threads = new Map<number, string>([[1, 'build']]);

  constructor(readonly command: string) {}

  private now(): number {
    return Math.round((performance.now() - this.t0) * 1000);
  }

  /** Reserve an event's place in the file now; its clock starts with `start()`. */
  reserve(name: string, cat: string, args: ProfileArgs = {}, tid = 1): ProfileSpan {
    const slot: Slot = { name, cat, tid, args: { ...args } };
    this.slots.push(slot);
    return {
      start: () => {
        slot.ts = this.now();
      },
      end: (more) => {
        if (slot.ts === undefined || slot.dur !== undefined) return;
        slot.dur = this.now() - slot.ts;
        if (more) Object.assign(slot.args, more);
      },
    };
  }

  /** Start an event now. */
  begin(name: string, cat: string, args: ProfileArgs = {}, tid = 1): ProfileSpan {
    const s = this.reserve(name, cat, args, tid);
    s.start();
    return s;
  }

  nameThread(tid: number, name: string): void {
    this.threads.set(tid, name);
  }

  /**
   * The trace as JSON text, one event per line. Events still open (the build failed
   * inside them) end now with `incomplete: true`; reserved events that never started
   * are left out.
   */
  toJSON(): string {
    const now = this.now();
    const lines: string[] = [JSON.stringify({ name: 'process_name', ph: 'M', pid: 1, tid: 1, args: { name: `sitefile ${this.command}` } })];
    for (const [tid, name] of [...this.threads].sort((a, b) => a[0] - b[0])) {
      lines.push(JSON.stringify({ name: 'thread_name', ph: 'M', pid: 1, tid, args: { name } }));
    }
    for (const s of this.slots) {
      if (s.ts === undefined) continue;
      const args = s.dur === undefined ? { ...s.args, incomplete: true } : s.args;
      lines.push(JSON.stringify({ name: s.name, cat: s.cat, ph: 'X', ts: s.ts, dur: s.dur ?? now - s.ts, pid: 1, tid: s.tid, args }));
    }
    return `{"traceEvents":[\n${lines.join(',\n')}\n],\n"displayTimeUnit":"ms"}\n`;
  }
}

/** The span helpers the executor uses: a no-op without a profiler. */
export function begin(p: Profiler | undefined, name: string, cat: string, args?: ProfileArgs): ProfileSpan {
  return p ? p.begin(name, cat, args) : NOOP;
}

/** Reserve an event's place now (its clock starts with `start()`): a no-op without a profiler. */
export function reserve(p: Profiler | undefined, name: string, cat: string, args?: ProfileArgs): ProfileSpan {
  return p ? p.reserve(name, cat, args) : NOOP;
}
