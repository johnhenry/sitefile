import { SitefileError } from './errors.js';
import { makeFile, Tree } from './tree.js';
import { dczEncode } from './zstd.js';

/**
 * Compression Dictionary Transport (issue #215; experimental, `build --experimental-dictionary`).
 *
 * With the previous build of a site, every text-like asset (`.js`, `.mjs`, `.css`, `.json`, `.wasm`,
 * `.html`) whose content changed but whose path stayed the same gets a `<path>.dcz` sidecar: the new
 * file as a `dcz` delta (Zstandard, src/zstd.ts) against the previous version. Every such asset with a
 * plain path also gets `Use-As-Dictionary: match="<path>"` in `/_headers`, so a browser that has it keeps
 * it as the dictionary for the next version. `serve` answers a request with `Available-Dictionary` and
 * `Accept-Encoding: dcz` from the sidecar when its dictionary hash matches ({@link deltaFor}).
 *
 * A delta is written only when it is smaller than the file, and only when the previous and new
 * versions together fit in an 8 MiB window (what browsers accept for `dcz`).
 */

const KINDS = ['.js', '.mjs', '.css', '.json', '.wasm', '.html'];
const MAX_WINDOW = 8 * 1024 * 1024;
const PLAIN_PATH = /^[A-Za-z0-9/._~-]+$/;

export function isDictionaryKind(path: string): boolean {
  const lower = path.toLowerCase();
  return KINDS.some((k) => lower.endsWith(k));
}

export interface DictionaryResult {
  readonly tree: Tree;
  /** Paths that got a `.dcz` delta. */
  readonly deltas: readonly string[];
}

/** The exported tree with `.dcz` deltas against `previous` and the `Use-As-Dictionary` headers. */
export function addDictionaryDeltas(tree: Tree, previous: Tree): DictionaryResult {
  const added: Array<[string, ReturnType<typeof makeFile>]> = [];
  const deltas: string[] = [];
  const header: string[] = [];
  for (const [p, n] of tree.entries()) {
    if (!isDictionaryKind(p)) continue;
    if (PLAIN_PATH.test(p)) header.push(`${p}\n  Use-As-Dictionary: match="${p}"\n`);
    const old = previous.get(p);
    if (!old || old.digest === n.digest || old.bytes.byteLength + n.bytes.byteLength > MAX_WINDOW) continue;
    const sidecar = `${p}.dcz`;
    if (tree.get(sidecar)) throw new SitefileError('E_CONFLICT', `--experimental-dictionary: '${sidecar}' already exists in the output`);
    const delta = dczEncode(n.bytes, old.bytes);
    if (delta.byteLength >= n.bytes.byteLength) continue;
    added.push([sidecar, makeFile(delta, 'dictionary delta')]);
    deltas.push(p);
  }
  if (header.length > 0) {
    const existing = tree.get('/_headers');
    const before = existing ? new TextDecoder().decode(existing.bytes) : '';
    const text = `${before}${before === '' || before.endsWith('\n') ? '' : '\n'}# Compression Dictionary Transport (sitefile build --experimental-dictionary)\n${header.join('')}`;
    added.push(['/_headers', makeFile(new TextEncoder().encode(text), 'dictionary headers')]);
  }
  return { tree: tree.withFiles(added, 'overwrite'), deltas };
}

/** The bytes of an RFC 8941 byte sequence (`:base64:`), or undefined. */
function sfBinary(v: string | undefined): Uint8Array | undefined {
  const m = v === undefined ? null : /^\s*:([A-Za-z0-9+/]*={0,2}):\s*$/.exec(v);
  return m ? new Uint8Array(Buffer.from(m[1] as string, 'base64')) : undefined;
}

/** True when `Accept-Encoding` lists `dcz` with a non-zero quality. */
export function acceptsDcz(acceptEncoding: string | undefined): boolean {
  if (acceptEncoding === undefined) return false;
  return acceptEncoding.split(',').some((part) => {
    const [token, ...params] = part.split(';').map((s) => s.trim());
    if (token?.toLowerCase() !== 'dcz') return false;
    const q = params.find((p) => /^q=/i.test(p));
    return q === undefined || Number(q.slice(2)) > 0;
  });
}

/**
 * The `dcz` response for `path`: the bytes of `<path>.dcz` when the request accepts `dcz` and its
 * `Available-Dictionary` is the hash the delta was made against; otherwise undefined.
 */
export function deltaFor(tree: Tree, path: string, availableDictionary: string | undefined, acceptEncoding: string | undefined): Uint8Array | undefined {
  if (!acceptsDcz(acceptEncoding)) return undefined;
  const hash = sfBinary(availableDictionary);
  if (!hash || hash.byteLength !== 32) return undefined;
  const sidecar = tree.get(`${path}.dcz`);
  if (!sidecar || sidecar.bytes.byteLength < 40) return undefined;
  for (let i = 0; i < 32; i++) if (sidecar.bytes[8 + i] !== hash[i]) return undefined;
  return sidecar.bytes;
}
