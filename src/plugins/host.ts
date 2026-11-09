import { execFile, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { realpath, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { SitefileError, type SourceSpan } from '../errors.js';
import { WasmPlugin } from './wasm.js';

/**
 * Plugins run one Node process each, under Node's permission model:
 *
 *   node --permission --allow-fs-read=<plugin folder> --max-old-space-size=<MiB> --input-type=module -e <runner>
 *
 * so a plugin can read files in its own folder and nothing else: no writes,
 * child processes, workers, native addons, WASI or network, and an empty
 * environment (no secrets). Requests and results travel over stdin/stdout.
 *
 * Node documents the permission model as a "seat belt" for trusted code, not a
 * defence against deliberately malicious code: plugins are code you choose to
 * run. Pin them with INTEGRITY.
 *
 * The sandbox is only as strong as the Node that runs it: by default the Node
 * running sitefile (`process.execPath`), or the one named by `--node` /
 * `SITEFILE_NODE` / `BuildOptions.node`. Its version is checked (at least
 * MIN_NODE) before the first plugin starts, and reported with `onNotice`.
 */

/**
 * The oldest Node that JavaScript plugins run on. Node 25 added network permissions
 * (before it, `--permission` did not block sockets); 26 is the release sitefile
 * requires and tests with (package.json `engines`).
 */
export const MIN_NODE = [26, 0, 0] as const;

/** The Node binary JavaScript plugins run on, as checked when the first one starts. */
export interface NodeRuntime {
  /** Absolute real path. */
  readonly path: string;
  /** `process.version` of that binary, e.g. `v26.11.1`. */
  readonly version: string;
}

/** `vX.Y.Z` → [X, Y, Z], or undefined. */
export function parseNodeVersion(v: string): [number, number, number] | undefined {
  const m = /^v(\d+)\.(\d+)\.(\d+)$/.exec(v.trim());
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : undefined;
}

const atLeast = (v: readonly number[], min: readonly number[]): boolean => {
  for (let i = 0; i < 3; i++) if (v[i]! !== min[i]!) return v[i]! > min[i]!;
  return true;
};

/**
 * Resolve and check the Node binary for JavaScript plugins (`node`: an absolute path; default
 * `process.execPath`). The error text is the part after `PLUGIN name: `.
 */
export async function checkNode(node: string | undefined, timeoutMs: number): Promise<NodeRuntime> {
  const given = node ?? process.execPath;
  const choose = 'choose another with --node or SITEFILE_NODE';
  if (!path.isAbsolute(given)) throw new Error(`the Node binary must be an absolute path, got '${given}'`);
  let real: string;
  try {
    real = await realpath(given);
    if (!(await stat(real)).isFile()) throw new Error('not a file');
  } catch {
    throw new Error(`could not start: no Node binary at ${given}`);
  }
  const out = await new Promise<string>((resolve, reject) =>
    execFile(real, ['--version'], { env: {}, timeout: timeoutMs, maxBuffer: 4096 }, (e, stdout) => (e ? reject(e) : resolve(String(stdout)))),
  ).catch(() => undefined);
  const parsed = out === undefined ? undefined : parseNodeVersion(out);
  if (!parsed) throw new Error(`could not start: ${real} --version did not print a Node version; ${choose}`);
  const version = out!.trim();
  if (!atLeast(parsed, MIN_NODE)) {
    throw new Error(`node ${version} (${real}) is too old for the plugin sandbox: JavaScript plugins need Node ${MIN_NODE.join('.')} or later; ${choose}`);
  }
  return { path: real, version };
}

export interface PluginPolicy {
  /** Per-call time limit (default 60 s); the process is killed when it passes. */
  readonly timeoutMs: number;
  /** Largest result accepted from one call (default 64 MiB). */
  readonly maxOutputBytes: number;
  /**
   * Memory limit in MiB (default 1024, `--plugin-memory`): a JavaScript plugin's heap
   * (`node --max-old-space-size`) and a wasm plugin's linear memory.
   */
  readonly maxMemoryMiB: number;
}

export const DEFAULT_PLUGIN_POLICY: PluginPolicy = { timeoutMs: 60_000, maxOutputBytes: 64 * 1024 * 1024, maxMemoryMiB: 1024 };

/** What V8 prints when a heap passes --max-old-space-size (followed by a native stack trace). */
const OUT_OF_MEMORY = 'JavaScript heap out of memory';
/** V8's out-of-memory report starts with one of these; it and everything after it is not logged. */
const isFatalReport = (line: string): boolean => line.startsWith('<--- Last few GCs --->') || line.startsWith('FATAL ERROR:');

/** A directory TRANSFORM runs at most this many wasm or tool calls at a time (a JS plugin, one). */
export const TRANSFORM_JOBS = Math.max(1, Math.min(4, os.availableParallelism()));

/** What a plugin receives (see docs/PLUGINS.md). */
export interface PluginRequest {
  readonly apiVersion: 1;
  readonly dest: string;
  readonly options: Readonly<Record<string, string>>;
  readonly input?: { readonly path: string; readonly name: string; readonly bytes: Uint8Array; readonly text?: string };
  readonly data?: unknown;
  readonly item?: unknown;
}

/** A plugin's result, normalized: one file, or several under the destination. */
export type PluginResult =
  | { readonly kind: 'file'; readonly bytes: Uint8Array }
  | { readonly kind: 'files'; readonly files: ReadonlyArray<readonly [string, Uint8Array]> };

/** Prefix that marks protocol lines on the child's stdout; anything else is plugin output. */
const MARK = '\u0000SITEFILE:';

/** Runs inside the sandboxed process (passed with -e, so it needs no file access). */
const RUNNER = String.raw`
const MARK = '\u0000SITEFILE:';
const send = (msg) => process.stdout.write(MARK + JSON.stringify(msg) + '\n');
// Plugin console output goes to stdout as unmarked lines, so it reaches the host before
// the result that follows it (stderr is another pipe, with no order against stdout).
for (const k of ['log', 'info', 'debug']) console[k] = (...a) => process.stdout.write(a.map(String).join(' ').split('\n').filter((l) => !l.startsWith(MARK)).join('\n') + '\n');
const toB64 = (v) => Buffer.from(typeof v === 'string' ? Buffer.from(v, 'utf8') : v).toString('base64');
const isBytes = (v) => typeof v === 'string' || v instanceof Uint8Array;
let transform;
let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf('\n')) !== -1) {
    const line = buf.slice(0, nl);
    buf = buf.slice(nl + 1);
    handle(JSON.parse(line)).catch((e) => send({ type: 'fatal', message: String(e && e.stack || e) }));
  }
});
async function handle(msg) {
  if (msg.type === 'load') {
    try {
      const mod = await import(msg.url);
      const api = mod.default && typeof mod.default.transform === 'function' ? mod.default : mod;
      const version = api.apiVersion ?? mod.apiVersion ?? 1;
      if (typeof api.transform !== 'function') throw new Error('the module must export transform(request), or a default object with transform');
      if (version !== 1) throw new Error('unsupported plugin apiVersion ' + JSON.stringify(version) + ' (this Sitefile supports 1)');
      transform = api.transform;
      // 'export const cache = false' opts a (non-deterministic) plugin out of the result cache;
      // 'cache = true' says it is deterministic. Without either it is cached, with a warning.
      const cache = api.cache ?? mod.cache;
      if (cache !== undefined && typeof cache !== 'boolean') throw new Error('cache must be true or false, got ' + JSON.stringify(cache));
      send({ type: 'ready', cache: cache !== false, declared: cache !== undefined });
    } catch (e) {
      send({ type: 'load-error', message: e && e.message ? e.message : String(e) });
    }
    return;
  }
  if (msg.type === 'transform') {
    const req = msg.request;
    if (req.input) {
      req.input.bytes = new Uint8Array(Buffer.from(req.input.bytes, 'base64'));
    }
    try {
      let out = await transform(req);
      if (isBytes(out)) out = { bytes: out };
      if (out && isBytes(out.bytes)) return send({ type: 'result', id: msg.id, bytes: toB64(out.bytes) });
      if (out && out.files && typeof out.files === 'object') {
        const files = {};
        for (const [k, v] of Object.entries(out.files)) {
          if (!isBytes(v)) throw new Error('files[' + JSON.stringify(k) + '] must be a string or Uint8Array');
          files[k] = toB64(v);
        }
        return send({ type: 'result', id: msg.id, files });
      }
      throw new Error('transform() must return a string, a Uint8Array, { bytes } or { files }');
    } catch (e) {
      send({ type: 'error', id: msg.id, message: e && e.message ? e.message : String(e) });
    }
  }
}
`;

interface Pending {
  resolve: (r: PluginResult) => void;
  reject: (e: Error) => void;
}

class PluginProcess {
  private readonly child: ChildProcessWithoutNullStreams;
  private buf = '';
  private stderr = '';
  /** V8 reported that the heap passed the memory limit. */
  private outOfMemory = false;
  /** Set once V8's fatal error report starts on stderr: the rest is a native stack trace, not logged. */
  private quiet = false;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  /** False when the module exports `cache = false`; known once it has loaded. */
  cacheable = true;
  /** Whether the module exports `cache` at all; known once it has loaded. */
  declaredCache = true;
  private ready: Promise<void>;
  private exited: string | undefined;

  constructor(
    readonly name: string,
    file: string,
    node: string,
    private readonly policy: PluginPolicy,
    private readonly log: (line: string) => void,
    span: SourceSpan | undefined,
  ) {
    const dir = path.dirname(file);
    this.child = spawn(
      node,
      ['--permission', `--allow-fs-read=${dir}`, `--max-old-space-size=${policy.maxMemoryMiB}`, '--disable-warning=ExperimentalWarning', '--input-type=module', '-e', RUNNER],
      { cwd: dir, env: {}, stdio: ['pipe', 'pipe', 'pipe'] },
    );
    this.child.stdout.setEncoding('utf8');
    this.child.stderr.setEncoding('utf8');
    this.child.stdout.on('data', (chunk: string) => this.onStdout(chunk));
    this.child.stderr.on('data', (chunk: string) => {
      this.stderr = (this.stderr + chunk).slice(-8192);
      if (this.stderr.includes(OUT_OF_MEMORY)) this.outOfMemory = true;
      for (const line of chunk.split('\n')) {
        if (isFatalReport(line)) this.quiet = true;
        if (line.trim() && !this.quiet) this.log(line);
      }
    });
    let onReady: () => void = () => {};
    let onLoadError: (e: Error) => void = () => {};
    this.ready = new Promise<void>((res, rej) => {
      onReady = res;
      onLoadError = rej;
    });
    // Awaited by call(); marking it handled keeps an unused failed plugin from crashing the process.
    this.ready.catch(() => {});
    this.onReadyMsg = onReady;
    this.onLoadError = (m) => onLoadError(new SitefileError('E_PLUGIN', `PLUGIN ${name}: ${m}`, span));
    this.child.on('error', (e) => this.fail(`could not start: ${e.message}`));
    // 'close' (not 'exit'): stderr has been read by then, so the reason can say why it died.
    this.child.on('close', (code, signal) => this.fail(signal ? `stopped by ${signal}` : `exited with code ${code}`));
    this.write({ type: 'load', url: new URL(`file://${file}`).href });
  }

  private onReadyMsg: () => void;
  private onLoadError: (message: string) => void;

  private write(msg: unknown): void {
    this.child.stdin.write(JSON.stringify(msg) + '\n');
  }

  private onStdout(chunk: string): void {
    this.buf += chunk;
    let nl;
    while ((nl = this.buf.indexOf('\n')) !== -1) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (!line.startsWith(MARK)) {
        if (line.trim()) this.log(line);
        continue;
      }
      if (line.length > this.policy.maxOutputBytes * 1.4 + 1024) {
        this.fail(`a result is larger than ${this.policy.maxOutputBytes} bytes`);
        return;
      }
      this.onMessage(JSON.parse(line.slice(MARK.length)) as Record<string, unknown>);
    }
    if (this.buf.length > this.policy.maxOutputBytes * 1.4 + 1024) this.fail(`a result is larger than ${this.policy.maxOutputBytes} bytes`);
  }

  private onMessage(msg: Record<string, unknown>): void {
    switch (msg['type']) {
      case 'ready':
        this.cacheable = msg['cache'] !== false;
        this.declaredCache = msg['declared'] === true;
        this.onReadyMsg();
        return;
      case 'load-error':
        this.onLoadError(String(msg['message']));
        return;
      case 'fatal':
        this.fail(String(msg['message']));
        return;
    }
    const p = this.pending.get(msg['id'] as number);
    if (!p) return;
    this.pending.delete(msg['id'] as number);
    if (msg['type'] === 'error') return p.reject(new Error(String(msg['message'])));
    try {
      if (typeof msg['bytes'] === 'string') return p.resolve({ kind: 'file', bytes: this.decode(msg['bytes']) });
      const files = Object.entries(msg['files'] as Record<string, string>).map(([k, v]) => [k, this.decode(v)] as const);
      p.resolve({ kind: 'files', files });
    } catch (e) {
      p.reject(e as Error);
    }
  }

  private decode(b64: string): Uint8Array {
    const bytes = new Uint8Array(Buffer.from(b64, 'base64'));
    if (bytes.byteLength > this.policy.maxOutputBytes) throw new Error(`result is larger than ${this.policy.maxOutputBytes} bytes`);
    return bytes;
  }

  private fail(reason: string): void {
    if (this.exited) return;
    if (this.outOfMemory) {
      // The stderr tail would be V8's native stack trace: machine-specific, and no help.
      reason = `ran out of memory (more than the ${this.policy.maxMemoryMiB} MiB limit)`;
      this.stderr = '';
    }
    this.exited = reason;
    const tail = this.stderr.trim() ? `\n${this.stderr.trim().split('\n').slice(-5).join('\n')}` : '';
    const err = new Error(`${reason}${tail}`);
    this.onLoadError(err.message);
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
    if (this.child.exitCode === null) this.child.kill('SIGKILL');
  }

  async call(request: PluginRequest, span?: SourceSpan): Promise<PluginResult> {
    await this.ready;
    if (this.exited) throw new SitefileError('E_PLUGIN', `PLUGIN ${this.name} ${this.exited}`, span);
    const id = this.nextId++;
    const wire = {
      ...request,
      ...(request.input ? { input: { ...request.input, bytes: Buffer.from(request.input.bytes).toString('base64') } } : {}),
    };
    return new Promise<PluginResult>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.fail(`timed out after ${this.policy.timeoutMs} ms`);
      }, this.policy.timeoutMs);
      this.pending.set(id, {
        resolve: (r) => {
          clearTimeout(timer);
          resolve(r);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(new SitefileError('E_PLUGIN', `PLUGIN ${this.name}: ${e.message}`, span));
        },
      });
      this.write({ type: 'transform', id, request: wire });
    });
  }

  close(): void {
    this.exited ??= 'closed';
    this.child.stdin.end();
    if (this.child.exitCode === null) this.child.kill();
  }
}

/** Counters a build reports (see BuildResult.transformStats). */
export interface HostStats {
  /** Plugin processes started. */
  spawned: number;
  /** transform() calls sent to a plugin process. */
  calls: number;
}

/** Starts a plugin's process on its first call (not at registration) and stops them all when the build ends. */
/** What a plugin runs as: a sandboxed Node process (.mjs) or a WebAssembly worker (.wasm). */
interface PluginRunner {
  readonly cacheable: boolean;
  call(request: PluginRequest, span?: SourceSpan): Promise<PluginResult>;
  close(): void;
}

/** What PluginHost warns when a JavaScript plugin whose results may be reused doesn't export `cache`. */
export function undeclaredCacheWarning(name: string): string {
  return `PLUGIN ${name} does not export cache, so its results are reused as if it were deterministic: export const cache = true to confirm, or cache = false if a result can depend on the clock, randomness or an earlier call`;
}

/** Where JavaScript plugins' Node comes from, and where to report it. */
export interface PluginHostOptions {
  /** Absolute path of the Node binary (default: `process.execPath`, the Node running sitefile). */
  readonly node?: string;
  /** Receives `plugins run on node <version> (<path>)` when the first JavaScript plugin starts. */
  readonly onNotice?: (message: string) => void;
  /**
   * The build caches or memoizes results (a cache directory or an incremental rebuild), so a
   * JavaScript plugin that doesn't export `cache` gets one warning.
   */
  readonly reusesResults?: boolean;
}

export class PluginHost {
  private readonly warnedCache = new Set<string>();
  private readonly registered = new Map<string, { file: string; span: SourceSpan | undefined; wasm?: Uint8Array }>();
  private readonly procs = new Map<string, PluginRunner>();
  readonly stats: HostStats = { spawned: 0, calls: 0 };
  private runtime: Promise<NodeRuntime> | undefined;

  constructor(
    private readonly policy: PluginPolicy = DEFAULT_PLUGIN_POLICY,
    private readonly log: (line: string) => void = () => {},
    private readonly options: PluginHostOptions = {},
  ) {}

  /** The checked Node binary (resolved once, on the first JavaScript plugin). */
  node(): Promise<NodeRuntime> {
    if (!this.runtime) {
      this.runtime = checkNode(this.options.node, this.policy.timeoutMs).then((rt) => {
        this.options.onNotice?.(`plugins run on node ${rt.version} (${rt.path})`);
        return rt;
      });
      this.runtime.catch(() => {});
    }
    return this.runtime;
  }

  /**
   * `file` is the plugin's absolute real path (already sandbox- and integrity-checked).
   * For a `.wasm` plugin, pass the checked module bytes. Runners start lazily.
   */
  register(name: string, file: string, span?: SourceSpan, wasm?: Uint8Array): void {
    if (!this.registered.has(name)) this.registered.set(name, { file, span, ...(wasm ? { wasm } : {}) });
  }

  /** Whether a registered plugin is a WebAssembly module. */
  isWasm(name: string): boolean {
    return this.registered.get(name)?.wasm !== undefined;
  }

  /** A registered wasm plugin's module bytes. */
  wasmBytes(name: string): Uint8Array | undefined {
    return this.registered.get(name)?.wasm;
  }

  async call(name: string, request: PluginRequest, span?: SourceSpan): Promise<PluginResult> {
    let p = this.procs.get(name);
    if (!p) {
      const reg = this.registered.get(name);
      if (!reg) throw new SitefileError('E_UNDEFINED', `unknown plugin '${name}'`, span);
      let node: NodeRuntime | undefined;
      if (!reg.wasm) {
        try {
          node = await this.node();
        } catch (e) {
          throw new SitefileError('E_PLUGIN', `PLUGIN ${name}: ${(e as Error).message}`, reg.span);
        }
        // Another call may have started it while this one waited for the check.
        p = this.procs.get(name);
      }
      if (!p) {
        p = reg.wasm
          ? new WasmPlugin(name, reg.wasm, this.policy, TRANSFORM_JOBS)
          : new PluginProcess(name, reg.file, node!.path, this.policy, (l) => this.log(`[plugin ${name}] ${l}`), reg.span);
        this.procs.set(name, p);
        this.stats.spawned++;
      }
    }
    this.stats.calls++;
    const result = await p.call(request, span);
    if (this.options.reusesResults && p instanceof PluginProcess && !p.declaredCache && !this.warnedCache.has(name)) {
      this.warnedCache.add(name);
      this.log(undeclaredCacheWarning(name));
    }
    return result;
  }

  /** The folder a registered plugin lives in. */
  folderOf(name: string): string | undefined {
    const f = this.registered.get(name)?.file;
    return f === undefined ? undefined : path.dirname(f);
  }

  /** Whether results of this plugin may be cached (false after it exported `cache = false`). */
  cacheable(name: string): boolean {
    return this.procs.get(name)?.cacheable ?? true;
  }

  close(): void {
    for (const p of this.procs.values()) p.close();
    this.procs.clear();
    this.registered.clear();
  }
}
