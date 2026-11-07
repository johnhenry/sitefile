import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { SitefileError, type SourceSpan } from '../errors.js';
import { WasmPlugin } from './wasm.js';

/**
 * Plugins run one Node process each, under Node's permission model:
 *
 *   node --permission --allow-fs-read=<plugin folder> --input-type=module -e <runner>
 *
 * so a plugin can read files in its own folder and nothing else: no writes,
 * child processes, workers, native addons, WASI or network, and an empty
 * environment (no secrets). Requests and results travel over stdin/stdout.
 *
 * Node documents the permission model as a "seat belt" for trusted code, not a
 * defence against deliberately malicious code: plugins are code you choose to
 * run. Pin them with INTEGRITY.
 */

export interface PluginPolicy {
  /** Per-call time limit (default 60 s); the process is killed when it passes. */
  readonly timeoutMs: number;
  /** Largest result accepted from one call (default 64 MiB). */
  readonly maxOutputBytes: number;
}

export const DEFAULT_PLUGIN_POLICY: PluginPolicy = { timeoutMs: 60_000, maxOutputBytes: 64 * 1024 * 1024 };

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
// Plugin console output goes to stderr so stdout stays a clean channel.
for (const k of ['log', 'info', 'debug']) console[k] = (...a) => process.stderr.write(a.map(String).join(' ') + '\n');
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
      // 'export const cache = false' opts a (non-deterministic) plugin out of the result cache.
      const cache = api.cache ?? mod.cache;
      send({ type: 'ready', cache: cache !== false });
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
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  /** False when the module exports `cache = false`; known once it has loaded. */
  cacheable = true;
  private ready: Promise<void>;
  private exited: string | undefined;

  constructor(
    readonly name: string,
    file: string,
    private readonly policy: PluginPolicy,
    private readonly log: (line: string) => void,
    span: SourceSpan | undefined,
  ) {
    const dir = path.dirname(file);
    this.child = spawn(
      process.execPath,
      ['--permission', `--allow-fs-read=${dir}`, '--disable-warning=ExperimentalWarning', '--input-type=module', '-e', RUNNER],
      { cwd: dir, env: {}, stdio: ['pipe', 'pipe', 'pipe'] },
    );
    this.child.stdout.setEncoding('utf8');
    this.child.stderr.setEncoding('utf8');
    this.child.stdout.on('data', (chunk: string) => this.onStdout(chunk));
    this.child.stderr.on('data', (chunk: string) => {
      this.stderr = (this.stderr + chunk).slice(-8192);
      for (const line of chunk.split('\n')) if (line.trim()) this.log(line);
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
    this.child.on('exit', (code, signal) => this.fail(signal ? `stopped by ${signal}` : `exited with code ${code}`));
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

export class PluginHost {
  private readonly registered = new Map<string, { file: string; span: SourceSpan | undefined; wasm?: Uint8Array }>();
  private readonly procs = new Map<string, PluginRunner>();
  readonly stats: HostStats = { spawned: 0, calls: 0 };

  constructor(
    private readonly policy: PluginPolicy = DEFAULT_PLUGIN_POLICY,
    private readonly log: (line: string) => void = () => {},
  ) {}

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
      p = reg.wasm
        ? new WasmPlugin(name, reg.wasm, this.policy, TRANSFORM_JOBS)
        : new PluginProcess(name, reg.file, this.policy, (l) => this.log(`[plugin ${name}] ${l}`), reg.span);
      this.procs.set(name, p);
      this.stats.spawned++;
    }
    this.stats.calls++;
    return p.call(request, span);
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
