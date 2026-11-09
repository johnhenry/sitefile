import { Worker } from 'node:worker_threads';
import { SitefileError, type SourceSpan } from '../errors.js';
import type { PluginPolicy, PluginRequest, PluginResult } from './host.js';

/**
 * WebAssembly renderer plugins (ABI v1).
 *
 * A plugin is a `wasm32-unknown-unknown` module with **no imports**, so it has
 * no way to reach files, the network, the clock or randomness: it can only
 * compute. It exports:
 *
 *   memory                                  its linear memory
 *   sitefile_alloc(len: i32) -> i32         reserve `len` bytes, return a pointer
 *   sitefile_transform(ptr: i32, len: i32) -> i64
 *                                           read the request at ptr..ptr+len and
 *                                           return (resultPtr << 32) | resultLen
 *   sitefile_api_version() -> i32           optional; must return 1
 *
 * Request and result are UTF-8 JSON. The request matches the JS plugin
 * request (docs/PLUGINS.md) with `input.bytes` base64-encoded. The result is
 * one of `{ "bytes": <base64> }`, `{ "text": "…" }`,
 * `{ "files": { "a.txt": <base64>, … } }` or `{ "error": "message" }`.
 *
 * Each call gets a fresh instance (no state carries over between calls), in a
 * worker thread that is terminated when a call exceeds the time limit.
 */

const REQUIRED_EXPORTS: ReadonlyArray<[string, WebAssembly.ImportExportKind]> = [
  ['memory', 'memory'],
  ['sitefile_alloc', 'function'],
  ['sitefile_transform', 'function'],
];

/** Compile and check a module against ABI v1; throws E_PLUGIN with a precise reason. */
export function checkWasmPlugin(name: string, bytes: Uint8Array, span?: SourceSpan): void {
  let mod: WebAssembly.Module;
  try {
    mod = new WebAssembly.Module(bytes);
  } catch (e) {
    throw new SitefileError('E_PLUGIN', `PLUGIN ${name}: not a valid WebAssembly module (${(e as Error).message})`, span);
  }
  const imports = WebAssembly.Module.imports(mod);
  if (imports.length > 0) {
    const list = imports.map((i) => `${i.module}.${i.name} (${i.kind})`).join(', ');
    throw new SitefileError(
      'E_PLUGIN',
      `PLUGIN ${name}: WebAssembly plugins can't import anything, so they can't reach files, the network or the clock; this module imports ${list}`,
      span,
    );
  }
  const exports = new Map(WebAssembly.Module.exports(mod).map((e) => [e.name, e.kind]));
  for (const [ename, kind] of REQUIRED_EXPORTS) {
    if (exports.get(ename) !== kind) {
      throw new SitefileError('E_PLUGIN', `PLUGIN ${name}: a WebAssembly plugin must export ${kind} '${ename}' (see docs/PLUGINS.md)`, span);
    }
  }
}

/** Runs inside the worker; plain JS (eval'd), so it can't use TypeScript. */
const WORKER = String.raw`
const { parentPort, workerData } = require('node:worker_threads');
const mod = new WebAssembly.Module(workerData.bytes);
parentPort.on('message', ({ id, request }) => {
  try {
    const { exports: ex } = new WebAssembly.Instance(mod, {});
    if (typeof ex.sitefile_api_version === 'function' && ex.sitefile_api_version() !== 1) {
      throw new Error('sitefile_api_version() must return 1');
    }
    const ptr = ex.sitefile_alloc(request.length) >>> 0;
    if (ptr + request.length > ex.memory.buffer.byteLength) throw new Error('sitefile_alloc returned a pointer outside memory');
    new Uint8Array(ex.memory.buffer, ptr, request.length).set(request);
    const packed = BigInt.asUintN(64, BigInt(ex.sitefile_transform(ptr, request.length)));
    const outPtr = Number(packed >> 32n);
    const outLen = Number(packed & 0xffffffffn);
    const size = ex.memory.buffer.byteLength;
    if (size > workerData.maxMemory) throw new Error('used ' + size + ' bytes of memory, more than the ' + workerData.maxMemory + ' allowed');
    if (outPtr + outLen > size) throw new Error('sitefile_transform returned a result outside memory');
    const out = new Uint8Array(ex.memory.buffer, outPtr, outLen).slice();
    parentPort.postMessage({ id, out }, [out.buffer]);
  } catch (e) {
    parentPort.postMessage({ id, error: e && e.message ? e.message : String(e) });
  }
});
`;

interface Pending {
  resolve: (out: Uint8Array) => void;
  reject: (e: Error) => void;
}

/**
 * A wasm plugin: up to `lanes` workers, so a directory TRANSFORM can run several calls at once.
 * A call waits for a free worker, and its time limit starts when that worker takes it.
 */
export class WasmPlugin {
  /** No imports means no clock or randomness: results depend only on the request. */
  readonly cacheable = true;
  private readonly idle: WasmLane[] = [];
  private readonly all: WasmLane[] = [];
  private readonly waiting: Array<(lane: WasmLane) => void> = [];

  constructor(
    readonly name: string,
    private readonly bytes: Uint8Array,
    private readonly policy: PluginPolicy,
    private readonly lanes = 1,
  ) {}

  private async lane(): Promise<WasmLane> {
    const free = this.idle.pop();
    if (free) return free;
    if (this.all.length < Math.max(1, this.lanes)) {
      const lane = new WasmLane(this.name, this.bytes, this.policy);
      this.all.push(lane);
      return lane;
    }
    return new Promise((resolve) => this.waiting.push(resolve));
  }

  async call(request: PluginRequest, span?: SourceSpan): Promise<PluginResult> {
    const lane = await this.lane();
    try {
      return await lane.call(request, span);
    } finally {
      const next = this.waiting.shift();
      if (next) next(lane);
      else this.idle.push(lane);
    }
  }

  close(): void {
    for (const lane of this.all) lane.close();
    this.all.length = 0;
    this.idle.length = 0;
  }
}

/** One worker thread running a wasm plugin's calls one at a time. */
class WasmLane {
  private worker: Worker | undefined;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private failed: string | undefined;

  constructor(
    readonly name: string,
    private readonly bytes: Uint8Array,
    private readonly policy: PluginPolicy,
  ) {}

  private start(): Worker {
    if (this.worker) return this.worker;
    const w = new Worker(WORKER, {
      eval: true,
      workerData: { bytes: this.bytes, maxMemory: this.policy.maxMemoryMiB * 1024 * 1024 },
      // The worker's own JS heap; wasm memory is checked after each call.
      resourceLimits: { maxOldGenerationSizeMb: 256 },
      env: {},
      stdout: true,
      stderr: true,
    });
    w.on('message', (m: { id: number; out?: Uint8Array; error?: string }) => {
      const p = this.pending.get(m.id);
      if (!p) return;
      this.pending.delete(m.id);
      if (m.error !== undefined) p.reject(new Error(m.error));
      else p.resolve(m.out!);
    });
    w.on('error', (e: unknown) => this.fail(e instanceof Error ? e.message : String(e)));
    w.on('exit', (code) => this.fail(`stopped (exit code ${code})`));
    this.worker = w;
    return w;
  }

  private fail(reason: string): void {
    for (const p of this.pending.values()) p.reject(new Error(reason));
    this.pending.clear();
    this.worker = undefined;
    this.failed = reason;
  }

  async call(request: PluginRequest, span?: SourceSpan): Promise<PluginResult> {
    const wire = {
      ...request,
      ...(request.input ? { input: { ...request.input, bytes: Buffer.from(request.input.bytes).toString('base64') } } : {}),
    };
    const json = new TextEncoder().encode(JSON.stringify(wire));
    const worker = this.start();
    const id = this.nextId++;
    const out = await new Promise<Uint8Array>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        void worker.terminate();
        this.worker = undefined;
        reject(new Error(`timed out after ${this.policy.timeoutMs} ms`));
      }, this.policy.timeoutMs);
      this.pending.set(id, {
        resolve: (o) => {
          clearTimeout(timer);
          resolve(o);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      worker.postMessage({ id, request: json }, [json.buffer]);
    }).catch((e: Error) => {
      throw new SitefileError('E_PLUGIN', `PLUGIN ${this.name}: ${e.message}`, span);
    });
    this.failed = undefined;
    return this.parse(out, span);
  }

  private parse(out: Uint8Array, span?: SourceSpan): PluginResult {
    if (out.byteLength > this.policy.maxOutputBytes) {
      throw new SitefileError('E_PLUGIN', `PLUGIN ${this.name}: result is larger than ${this.policy.maxOutputBytes} bytes`, span);
    }
    let msg: unknown;
    try {
      msg = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(out));
    } catch (e) {
      throw new SitefileError('E_PLUGIN', `PLUGIN ${this.name}: the result is not UTF-8 JSON (${(e as Error).message})`, span);
    }
    const bad = (why: string): SitefileError => new SitefileError('E_PLUGIN', `PLUGIN ${this.name}: ${why}`, span);
    if (msg === null || typeof msg !== 'object' || Array.isArray(msg)) throw bad('the result must be a JSON object');
    const r = msg as Record<string, unknown>;
    if (typeof r['error'] === 'string') throw bad(r['error']);
    const b64 = (v: unknown, what: string): Uint8Array => {
      if (typeof v !== 'string' || !/^[A-Za-z0-9+/]*={0,2}$/.test(v)) throw bad(`${what} must be a base64 string`);
      return new Uint8Array(Buffer.from(v, 'base64'));
    };
    if ('bytes' in r) return { kind: 'file', bytes: b64(r['bytes'], 'bytes') };
    if (typeof r['text'] === 'string') return { kind: 'file', bytes: new TextEncoder().encode(r['text']) };
    if (r['files'] !== null && typeof r['files'] === 'object' && !Array.isArray(r['files'])) {
      const files = Object.entries(r['files'] as Record<string, unknown>).map(([k, v]) => [k, b64(v, `files["${k}"]`)] as const);
      return { kind: 'files', files };
    }
    throw bad('the result must have "bytes", "text", "files" or "error"');
  }

  close(): void {
    const w = this.worker;
    this.worker = undefined;
    if (w) void w.terminate();
  }
}
