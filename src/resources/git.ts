import { createHash } from 'node:crypto';
import { inflateSync } from 'node:zlib';
import { SitefileError, type SourceSpan } from '../errors.js';
import { nfc } from '../paths.js';
import { fetchRemote, redactUrl } from './https.js';
import type { ArchiveLimits, FetchPolicy } from './types.js';

/**
 * Git sources over smart HTTP (issue #91): `IMPORT name FROM git+https://host/repo.git#ref:path` and
 * `sitefile build https://host/repo.git#ref:dir`.
 *
 * This is a small client for Git's protocol v2 (gitprotocol-v2, gitprotocol-http), written against
 * the build's HTTPS client so it inherits its network policy unchanged (https only unless
 * --allow-http, no credentials in URLs, the SSRF address check at connect time, redirects re-checked
 * hop by hop, the download cap and the timeout). No `git` binary runs, so there are no hooks, no
 * config, no credential helpers and no filters:
 *
 *   1. GET  <repo>/info/refs?service=git-upload-pack  (Git-Protocol: version=2): capabilities
 *   2. POST <repo>/git-upload-pack  command=ls-refs    (only when the ref is not a commit)
 *   3. POST <repo>/git-upload-pack  command=fetch      want <commit>, deepen 1, done
 *
 * The packfile is parsed here (zlib objects, OFS_DELTA and REF_DELTA), every object's SHA-1 is
 * recomputed, and the pack checksum is verified, so the tree that is imported is the one the commit
 * names. Only SHA-1 repositories are supported. Submodules (gitlinks) are refused; symbolic links are
 * resolved inside the imported tree with the same rules as local symlinks (see `walkGitTree`).
 */

export const GIT_COMMIT_RE = /^[0-9a-f]{40}$/;

/** A parsed `git+https://…#ref:path` location. */
export interface GitLocation {
  /** The URI as written (after interpolation): the lockfile and vendor key. */
  readonly url: string;
  /** The repository URL (`https://host/repo.git`), without `git+` and the fragment. */
  readonly repo: string;
  /** The ref as written: a branch, a tag, `refs/…`, a 40-hex commit, or '' for the remote HEAD. */
  readonly ref: string;
  /** The path inside the repository ('' for its root), `/`-separated, no `.` or `..`. */
  readonly path: string;
  /** The commit, when `ref` is a full 40-hex commit id (a pin). */
  readonly commit?: string;
}

const USER_AGENT = 'sitefile/0.1';

/** True for URIs with a `git+` scheme (`git+https:`, `git+http:`, `git+ssh:`, …). */
export function isGitUri(uri: string): boolean {
  return /^git\+[A-Za-z][A-Za-z0-9+.-]*:/i.test(uri);
}

function badRef(ref: string, span?: SourceSpan): SitefileError {
  return new SitefileError('E_VALIDATION', `invalid Git ref '${ref}' (see git check-ref-format)`, span);
}

/** Git's ref name rules (git check-ref-format), applied to the ref in a Git URL. */
export function checkRefName(ref: string, span?: SourceSpan): void {
  if (ref === '') return;
  // eslint-disable-next-line no-control-regex
  if (/[\x00-\x20\x7f~^:?*[\\]/.test(ref) || ref.includes('..') || ref.includes('@{') || ref === '@') throw badRef(ref, span);
  if (ref.startsWith('/') || ref.endsWith('/') || ref.endsWith('.') || ref.startsWith('-') || ref.includes('//')) throw badRef(ref, span);
  for (const part of ref.split('/')) if (part.startsWith('.') || part.endsWith('.lock')) throw badRef(ref, span);
}

/**
 * Parse `git+https://host/repo[.git][#ref[:path]]` (`git+http:` too; plain http still needs
 * --allow-http when fetched). `scheme` errors are E_PROTOCOL, a bad ref E_VALIDATION, a path with
 * `..` E_PATH_ESCAPE.
 */
export function parseGitUri(uri: string, span?: SourceSpan): GitLocation {
  // Schemes are case-insensitive: GIT+HTTPS:// is git+https://.
  const m = /^git\+([A-Za-z][A-Za-z0-9+.-]*):/i.exec(uri);
  const scheme = m ? m[1]!.toLowerCase() : '';
  if (scheme !== 'https' && scheme !== 'http') {
    throw new SitefileError('E_PROTOCOL', `unsupported Git transport 'git+${scheme}:' (supported: git+https)`, span);
  }
  const hash = uri.indexOf('#');
  const repo = (hash === -1 ? uri : uri.slice(0, hash)).slice('git+'.length);
  const fragment = hash === -1 ? '' : uri.slice(hash + 1);
  let u: URL;
  try {
    u = new URL(repo);
  } catch (e) {
    throw new SitefileError('E_PROTOCOL', `invalid Git URL '${redactUrl(repo)}'`, span, { cause: e });
  }
  if (u.username !== '' || u.password !== '') {
    throw new SitefileError('E_PROTOCOL', `URL ${redactUrl(repo)} contains credentials; credentials in URLs are not allowed`, span);
  }
  if (u.search !== '') throw new SitefileError('E_PROTOCOL', `Git URL ${redactUrl(repo)} must not have a query string`, span);
  const colon = fragment.indexOf(':');
  const ref = colon === -1 ? fragment : fragment.slice(0, colon);
  const rawPath = colon === -1 ? '' : fragment.slice(colon + 1);
  checkRefName(ref, span);
  const path = gitPath(rawPath, span);
  const base = { url: uri, repo, ref, path };
  return GIT_COMMIT_RE.test(ref) ? { ...base, commit: ref } : base;
}

/** The path inside a repository: segments without `.`, empty segments or backslashes; `..` is E_PATH_ESCAPE. */
function gitPath(raw: string, span?: SourceSpan): string {
  const segs: string[] = [];
  for (const seg of raw.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') throw new SitefileError('E_PATH_ESCAPE', `path '${raw}' in a Git URL must not contain '..'`, span);
    if (seg.includes('\\') || seg.includes('\0')) throw new SitefileError('E_PATH', `invalid path '${raw}' in a Git URL`, span);
    segs.push(nfc(seg));
  }
  return segs.join('/');
}

/** The URL of a location with the ref replaced by the commit it resolved to (provenance). */
export function pinnedGitUrl(loc: GitLocation, commit: string): string {
  return `git+${loc.repo}#${commit}${loc.path === '' ? '' : ':' + loc.path}`;
}

// ---------------------------------------------------------------- pkt-line

const enc = new TextEncoder();

function pkt(line: string): Uint8Array {
  const body = enc.encode(line);
  const head = enc.encode((body.byteLength + 4).toString(16).padStart(4, '0'));
  const out = new Uint8Array(head.byteLength + body.byteLength);
  out.set(head);
  out.set(body, 4);
  return out;
}

const FLUSH = enc.encode('0000');
const DELIM = enc.encode('0001');

function concat(parts: readonly Uint8Array[]): Uint8Array {
  let n = 0;
  for (const p of parts) n += p.byteLength;
  const out = new Uint8Array(n);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.byteLength;
  }
  return out;
}

/** One pkt-line: data, or a special packet (flush 0000, delim 0001, response-end 0002). */
type Packet = { readonly kind: 'data'; readonly data: Uint8Array } | { readonly kind: 'flush' | 'delim' | 'end' };

function gitError(repo: string, why: string, span?: SourceSpan): SitefileError {
  return new SitefileError('E_GIT', `${redactUrl(repo)}: ${why}`, span);
}

function* packets(bytes: Uint8Array, repo: string, span?: SourceSpan): Generator<Packet> {
  let i = 0;
  const dec = new TextDecoder();
  while (i < bytes.byteLength) {
    if (i + 4 > bytes.byteLength) throw gitError(repo, 'truncated pkt-line', span);
    const head = dec.decode(bytes.subarray(i, i + 4));
    if (!/^[0-9a-f]{4}$/.test(head)) throw gitError(repo, 'malformed pkt-line (not a Git smart HTTP response?)', span);
    const n = parseInt(head, 16);
    if (n === 0) yield { kind: 'flush' };
    else if (n === 1) yield { kind: 'delim' };
    else if (n === 2) yield { kind: 'end' };
    else if (n < 4) throw gitError(repo, 'malformed pkt-line', span);
    else {
      if (i + n > bytes.byteLength) throw gitError(repo, 'truncated pkt-line', span);
      yield { kind: 'data', data: bytes.subarray(i + 4, i + n) };
    }
    i += n < 4 ? 4 : n;
  }
}

/** Printable text of a server message (control characters dropped, at most 200 characters). */
function serverText(data: Uint8Array): string {
  // eslint-disable-next-line no-control-regex
  return new TextDecoder().decode(data).replace(/[\x00-\x1f\x7f]+/g, ' ').trim().slice(0, 200);
}

function lineOf(data: Uint8Array): string {
  const s = new TextDecoder().decode(data);
  return s.endsWith('\n') ? s.slice(0, -1) : s;
}

// ---------------------------------------------------------------- protocol

/** What a fetch needs: the build's network policy and extra headers for the repository's origin. */
export interface GitTransport {
  readonly policy: FetchPolicy;
  /** Sent only to the repository's origin (IMPORT … SECRET). */
  readonly originHeaders?: Readonly<Record<string, string>>;
  readonly span?: SourceSpan | undefined;
}

interface Remote {
  /** The repository URL after the capability request's redirects (no trailing slash). */
  readonly base: string;
  readonly shallow: boolean;
}

const ADVERTISEMENT = 'application/x-git-upload-pack-advertisement';
const RESULT = 'application/x-git-upload-pack-result';

/** Step 1: the v2 capability advertisement. Redirects are followed here only (as Git does). */
async function connect(repo: string, t: GitTransport): Promise<Remote> {
  const trimmed = repo.replace(/\/+$/, '');
  const url = `${trimmed}/info/refs?service=git-upload-pack`;
  const r = await fetchRemote(url, t.policy, t.span, {
    headers: { 'git-protocol': 'version=2', accept: ADVERTISEMENT, 'user-agent': USER_AGENT },
    ...(t.originHeaders ? { originHeaders: t.originHeaders } : {}),
  });
  if (r.mediaType !== ADVERTISEMENT) {
    throw gitError(repo, `not a Git smart HTTP server (content-type ${r.mediaType ?? 'missing'}; Git protocol v2 is required)`, t.span);
  }
  const suffix = '/info/refs?service=git-upload-pack';
  if (!r.finalUrl.endsWith(suffix)) throw gitError(repo, `redirected to ${redactUrl(r.finalUrl)}, which is not a Git repository URL`, t.span);
  const base = r.finalUrl.slice(0, -suffix.length);
  let v2 = false;
  let fetch: string | undefined;
  let lsRefs = false;
  let format = 'sha1';
  for (const p of packets(r.bytes, repo, t.span)) {
    if (p.kind !== 'data') continue;
    const line = lineOf(p.data);
    if (line === 'version 2') v2 = true;
    else if (line === 'ls-refs' || line.startsWith('ls-refs=')) lsRefs = true;
    else if (line === 'fetch' || line.startsWith('fetch=')) fetch = line.slice('fetch='.length);
    else if (line.startsWith('object-format=')) format = line.slice('object-format='.length);
  }
  if (!v2 || fetch === undefined || !lsRefs) throw gitError(repo, 'the server does not speak Git protocol v2 (ls-refs and fetch are required)', t.span);
  if (format !== 'sha1') throw gitError(repo, `object format '${format}' is not supported (only sha1 repositories)`, t.span);
  return { base, shallow: fetch.split(' ').includes('shallow') };
}

async function command(remote: Remote, repo: string, body: Uint8Array, t: GitTransport): Promise<Uint8Array> {
  const r = await fetchRemote(`${remote.base}/git-upload-pack`, t.policy, t.span, {
    method: 'POST',
    body,
    headers: { 'git-protocol': 'version=2', 'content-type': 'application/x-git-upload-pack-request', accept: RESULT, 'user-agent': USER_AGENT },
    ...(t.originHeaders ? { originHeaders: t.originHeaders } : {}),
  });
  if (r.mediaType !== RESULT) throw gitError(repo, `unexpected content-type ${r.mediaType ?? 'missing'} from git-upload-pack`, t.span);
  return r.bytes;
}

/**
 * The commit `ref` names on the server: '' is the remote HEAD; `refs/…` is exact; otherwise
 * `refs/tags/<ref>` then `refs/heads/<ref>` (Git's own order). Annotated tags are peeled.
 */
export async function resolveRef(repo: string, ref: string, t: GitTransport): Promise<string> {
  const remote = await connect(repo, t);
  return lsRef(remote, repo, ref, t);
}

async function lsRef(remote: Remote, repo: string, ref: string, t: GitTransport): Promise<string> {
  const candidates = ref === '' ? ['HEAD'] : ref.startsWith('refs/') ? [ref] : [`refs/tags/${ref}`, `refs/heads/${ref}`];
  const parts = [pkt('command=ls-refs\n'), pkt(`agent=${USER_AGENT}\n`), DELIM, pkt('peel\n'), pkt('symrefs\n')];
  for (const c of candidates) parts.push(pkt(`ref-prefix ${c}\n`));
  parts.push(FLUSH);
  const bytes = await command(remote, repo, concat(parts), t);
  const found = new Map<string, string>();
  for (const p of packets(bytes, repo, t.span)) {
    if (p.kind === 'flush') break;
    if (p.kind !== 'data') continue;
    const line = lineOf(p.data);
    if (line.startsWith('ERR ')) throw gitError(repo, `remote error: ${serverText(p.data.subarray(4))}`, t.span);
    const [oid, name, ...attrs] = line.split(' ');
    if (oid === undefined || name === undefined || !GIT_COMMIT_RE.test(oid)) throw gitError(repo, 'malformed ls-refs response', t.span);
    const peeled = attrs.find((a) => a.startsWith('peeled:'))?.slice('peeled:'.length);
    found.set(name, peeled !== undefined && GIT_COMMIT_RE.test(peeled) ? peeled : oid);
  }
  for (const c of candidates) {
    const oid = found.get(c);
    if (oid !== undefined) return oid;
  }
  throw new SitefileError('E_NOT_FOUND', ref === '' ? `${redactUrl(repo)} has no HEAD` : `Git ref '${ref}' not found in ${redactUrl(repo)}`, t.span);
}

/** A fetched commit: its objects, by hex id. */
export interface GitObjects {
  readonly commit: string;
  readonly objects: ReadonlyMap<string, GitObject>;
  /** The packfile as received (for size accounting). */
  readonly packBytes: number;
}

export interface GitObject {
  readonly type: 'commit' | 'tree' | 'blob' | 'tag';
  readonly data: Uint8Array;
}

/**
 * Fetch `commit` (when given) or the commit `ref` names now: one shallow fetch (`deepen 1`) of that
 * commit alone. The pack is checked as described in the module comment.
 */
export async function fetchGitCommit(repo: string, ref: string, commit: string | undefined, t: GitTransport, limits: ArchiveLimits): Promise<GitObjects> {
  const remote = await connect(repo, t);
  const want = commit ?? (await lsRef(remote, repo, ref, t));
  const parts = [pkt('command=fetch\n'), pkt(`agent=${USER_AGENT}\n`), DELIM, pkt(`want ${want}\n`)];
  if (remote.shallow) parts.push(pkt('deepen 1\n'));
  parts.push(pkt('no-progress\n'), pkt('ofs-delta\n'), pkt('done\n'), FLUSH);
  const bytes = await command(remote, repo, concat(parts), t);
  const pack = packfileOf(bytes, repo, t.span);
  const objects = parsePack(pack, repo, limits, t.span);
  const c = objects.get(want);
  if (!c) throw gitError(repo, `the server did not send commit ${want}`, t.span);
  if (c.type !== 'commit') throw gitError(repo, `${want} is a ${c.type}, not a commit`, t.span);
  return { commit: want, objects, packBytes: pack.byteLength };
}

/** The packfile section of a fetch response, with side-band-64k framing removed. */
function packfileOf(bytes: Uint8Array, repo: string, span?: SourceSpan): Uint8Array {
  const chunks: Uint8Array[] = [];
  let section = '';
  let header = true;
  for (const p of packets(bytes, repo, span)) {
    if (p.kind === 'flush' || p.kind === 'end') {
      if (section === 'packfile') break;
      header = true;
      continue;
    }
    if (p.kind !== 'data') {
      header = true;
      continue;
    }
    if (header) {
      const line = lineOf(p.data);
      if (line.startsWith('ERR ')) throw gitError(repo, `remote error: ${serverText(p.data.subarray(4))}`, span);
      section = line;
      header = false;
      if (section !== 'packfile' && section !== 'shallow-info' && section !== 'acknowledgments' && section !== 'wanted-refs') {
        throw gitError(repo, `unexpected fetch response section '${serverText(p.data)}'`, span);
      }
      continue;
    }
    if (section !== 'packfile') continue;
    const band = p.data[0];
    if (band === 1) chunks.push(p.data.subarray(1));
    else if (band === 3) throw gitError(repo, `remote error: ${serverText(p.data.subarray(1))}`, span);
    else if (band !== 2) throw gitError(repo, 'malformed side-band packet', span);
  }
  if (section !== 'packfile') throw gitError(repo, 'the fetch response has no packfile', span);
  return concat(chunks);
}

// ---------------------------------------------------------------- packfile

const TYPES = ['', 'commit', 'tree', 'blob', 'tag'] as const;

function sha1Hex(...parts: Uint8Array[]): string {
  const h = createHash('sha1');
  for (const p of parts) h.update(p);
  return h.digest('hex');
}

const hexOf = (b: Uint8Array): string => Buffer.from(b.buffer, b.byteOffset, b.byteLength).toString('hex');

/** The SHA-1 object id of `data` as a loose object of `type`. */
export function gitObjectId(type: GitObject['type'], data: Uint8Array): string {
  return sha1Hex(enc.encode(`${type} ${data.byteLength}\0`), data);
}

interface RawEntry {
  readonly offset: number;
  /** 1-4 for whole objects; 6 (OFS_DELTA) or 7 (REF_DELTA). */
  readonly type: number;
  readonly data: Uint8Array;
  readonly baseOffset?: number;
  readonly baseId?: string;
}

/**
 * Parse a version 2 or 3 packfile into objects by id. Every object is inflated at most to its
 * declared size, deltas are applied (bases must be in the pack: thin packs are not requested), and
 * the total inflated size is bounded by `limits.maxTotalBytes`.
 */
export function parsePack(pack: Uint8Array, repo: string, limits: ArchiveLimits, span?: SourceSpan): Map<string, GitObject> {
  const bad = (why: string): SitefileError => gitError(repo, `malformed packfile: ${why}`, span);
  if (pack.byteLength < 32 || new TextDecoder().decode(pack.subarray(0, 4)) !== 'PACK') throw bad('missing PACK header');
  const view = new DataView(pack.buffer, pack.byteOffset, pack.byteLength);
  const version = view.getUint32(4);
  if (version !== 2 && version !== 3) throw bad(`unsupported version ${version}`);
  const count = view.getUint32(8);
  const end = pack.byteLength - 20;
  if (sha1Hex(pack.subarray(0, end)) !== hexOf(pack.subarray(end))) throw bad('checksum mismatch');
  // Each object takes at least a few bytes, so the count is bounded by the pack size too.
  if (count > end) throw bad('object count exceeds the pack size');
  let total = 0;
  const charge = (n: number): void => {
    total += n;
    if (total > limits.maxTotalBytes) throw new SitefileError('E_ARCHIVE_LIMIT', `${redactUrl(repo)}: Git objects expand beyond ${limits.maxTotalBytes} bytes`, span);
  };
  const entries: RawEntry[] = [];
  let pos = 12;
  for (let i = 0; i < count; i++) {
    const offset = pos;
    if (pos >= end) throw bad('truncated');
    let c = pack[pos++]!;
    const type = (c >> 4) & 7;
    let size = c & 15;
    let shift = 4;
    while (c & 0x80) {
      if (pos >= end || shift > 49) throw bad('bad object header');
      c = pack[pos++]!;
      size += (c & 0x7f) * 2 ** shift;
      shift += 7;
    }
    let baseOffset: number | undefined;
    let baseId: string | undefined;
    if (type === 6) {
      if (pos >= end) throw bad('truncated');
      c = pack[pos++]!;
      let n = c & 0x7f;
      while (c & 0x80) {
        if (pos >= end || n > 2 ** 45) throw bad('bad delta offset');
        c = pack[pos++]!;
        n = (n + 1) * 128 + (c & 0x7f);
      }
      baseOffset = offset - n;
      if (baseOffset < 12 || n === 0) throw bad('delta base offset out of range');
    } else if (type === 7) {
      if (pos + 20 > end) throw bad('truncated');
      baseId = hexOf(pack.subarray(pos, pos + 20));
      pos += 20;
    } else if (type < 1 || type > 4) {
      throw bad(`unknown object type ${type}`);
    }
    charge(size);
    let res: { buffer: Buffer; engine: { bytesWritten: number } };
    try {
      res = inflateSync(pack.subarray(pos, end), { info: true, maxOutputLength: Math.max(1, size) }) as unknown as typeof res;
    } catch (e) {
      throw bad(`object at offset ${offset} does not inflate (${(e as Error).message})`);
    }
    if (res.buffer.byteLength !== size) throw bad(`object at offset ${offset} has the wrong size`);
    pos += res.engine.bytesWritten;
    const data = new Uint8Array(res.buffer.buffer, res.buffer.byteOffset, res.buffer.byteLength);
    entries.push({ offset, type, data, ...(baseOffset !== undefined ? { baseOffset } : {}), ...(baseId !== undefined ? { baseId } : {}) });
  }
  if (pos !== end) throw bad('trailing data after the last object');

  const byOffset = new Map<number, RawEntry>(entries.map((e) => [e.offset, e]));
  const resolved = new Map<number, GitObject>();
  const objects = new Map<string, GitObject>();
  const offsetOf = new Map<string, number>();
  for (const e of entries) {
    if (e.type <= 4) {
      const o: GitObject = { type: TYPES[e.type] as GitObject['type'], data: e.data };
      resolved.set(e.offset, o);
      const id = gitObjectId(o.type, o.data);
      objects.set(id, o);
      offsetOf.set(id, e.offset);
    }
  }
  // Deltas: resolve each chain from its first resolved base (iteratively; REF_DELTA bases may come later).
  let progress = true;
  while (progress) {
    progress = false;
    for (const e of entries) {
      if (resolved.has(e.offset)) continue;
      const chain: RawEntry[] = [e];
      let base: GitObject | undefined;
      for (;;) {
        const top = chain[chain.length - 1]!;
        const bOff = top.baseOffset ?? (top.baseId !== undefined ? offsetOf.get(top.baseId) : undefined);
        if (bOff === undefined) break; // a REF_DELTA base not seen yet
        base = resolved.get(bOff);
        if (base) break;
        const next = byOffset.get(bOff);
        if (!next) throw bad('delta base is not an object in the pack');
        if (chain.length > count) throw bad('delta cycle');
        chain.push(next);
      }
      if (!base) continue;
      for (let i = chain.length - 1; i >= 0; i--) {
        const d = chain[i]!;
        const data = applyDelta(base.data, d.data, bad);
        charge(data.byteLength);
        base = { type: base.type, data };
        resolved.set(d.offset, base);
        const id = gitObjectId(base.type, base.data);
        objects.set(id, base);
        offsetOf.set(id, d.offset);
      }
      progress = true;
    }
  }
  if (resolved.size !== entries.length) throw bad('delta base missing from the pack');
  return objects;
}

function applyDelta(base: Uint8Array, delta: Uint8Array, bad: (why: string) => SitefileError): Uint8Array {
  let pos = 0;
  const varint = (): number => {
    let n = 0;
    let shift = 0;
    let c: number;
    do {
      if (pos >= delta.byteLength || shift > 49) throw bad('truncated delta');
      c = delta[pos++]!;
      n += (c & 0x7f) * 2 ** shift;
      shift += 7;
    } while (c & 0x80);
    return n;
  };
  if (varint() !== base.byteLength) throw bad('delta base size mismatch');
  const size = varint();
  const out = new Uint8Array(size);
  let o = 0;
  while (pos < delta.byteLength) {
    const op = delta[pos++]!;
    if (op & 0x80) {
      let off = 0;
      let len = 0;
      for (let i = 0; i < 4; i++) if (op & (1 << i)) off += (delta[pos++] ?? 0) * 2 ** (8 * i);
      for (let i = 0; i < 3; i++) if (op & (0x10 << i)) len += (delta[pos++] ?? 0) * 2 ** (8 * i);
      if (len === 0) len = 0x10000;
      if (pos > delta.byteLength || off + len > base.byteLength || o + len > size) throw bad('delta copy out of range');
      out.set(base.subarray(off, off + len), o);
      o += len;
    } else if (op !== 0) {
      if (pos + op > delta.byteLength || o + op > size) throw bad('delta insert out of range');
      out.set(delta.subarray(pos, pos + op), o);
      pos += op;
      o += op;
    } else {
      throw bad('reserved delta opcode 0');
    }
  }
  if (o !== size) throw bad('delta result size mismatch');
  return out;
}

// ---------------------------------------------------------------- trees

interface TreeEntry {
  /** As stored (bytes decoded as UTF-8). */
  readonly name: string;
  readonly kind: 'tree' | 'blob' | 'link' | 'gitlink';
  readonly exec: boolean;
  readonly id: string;
}

/** Selected content of a commit: a tree's files, or one file. */
export type GitSelection =
  | { readonly kind: 'tree'; readonly files: ReadonlyArray<readonly [string, { readonly bytes: Uint8Array; readonly mode: number }]> }
  | { readonly kind: 'file'; readonly bytes: Uint8Array; readonly mode: number; readonly basename: string };

class Repo {
  private readonly cache = new Map<string, TreeEntry[]>();
  constructor(
    private readonly objs: GitObjects,
    private readonly repo: string,
    private readonly span: SourceSpan | undefined,
  ) {}

  object(id: string, type: GitObject['type']): Uint8Array {
    const o = this.objs.objects.get(id);
    if (!o) throw gitError(this.repo, `object ${id} is missing from the pack`, this.span);
    if (o.type !== type) throw gitError(this.repo, `object ${id} is a ${o.type}, expected a ${type}`, this.span);
    return o.data;
  }

  rootTree(): string {
    const text = new TextDecoder().decode(this.object(this.objs.commit, 'commit'));
    const m = /^tree ([0-9a-f]{40})\n/.exec(text);
    if (!m) throw gitError(this.repo, `commit ${this.objs.commit} has no tree`, this.span);
    return m[1]!;
  }

  entries(id: string, where: string): TreeEntry[] {
    const hit = this.cache.get(id);
    if (hit) return hit;
    const data = this.object(id, 'tree');
    const out: TreeEntry[] = [];
    const dec = new TextDecoder('utf-8', { fatal: true });
    let i = 0;
    while (i < data.byteLength) {
      const sp = data.indexOf(0x20, i);
      const nul = sp === -1 ? -1 : data.indexOf(0, sp);
      if (sp === -1 || nul === -1 || nul + 21 > data.byteLength) throw gitError(this.repo, `malformed tree ${id}`, this.span);
      const mode = new TextDecoder().decode(data.subarray(i, sp));
      let name: string;
      try {
        name = dec.decode(data.subarray(sp + 1, nul));
      } catch {
        throw new SitefileError('E_ENCODING', `${redactUrl(this.repo)}: a file name in '${where || '/'}' is not valid UTF-8`, this.span);
      }
      const shown = where === '' ? name : `${where}/${name}`;
      if (name === '' || name === '.' || name === '..' || name.includes('/') || name.includes('\\') || name.toLowerCase() === '.git') {
        throw new SitefileError('E_ARCHIVE_UNSAFE', `${redactUrl(this.repo)}: unsafe tree entry '${shown}'`, this.span);
      }
      const kind = mode === '40000' ? 'tree' : mode === '100644' || mode === '100755' || mode === '100664' ? 'blob' : mode === '120000' ? 'link' : mode === '160000' ? 'gitlink' : undefined;
      if (!kind) throw gitError(this.repo, `tree entry '${shown}' has unknown mode ${mode}`, this.span);
      out.push({ name, kind, exec: mode === '100755', id: hexOf(data.subarray(nul + 1, nul + 21)) });
      i = nul + 21;
    }
    this.cache.set(id, out);
    return out;
  }

  /** The entry named `seg` (compared in NFC) in tree `id`. Two names with one NFC form are E_PATH_COLLISION. */
  child(id: string, where: string, seg: string): TreeEntry | undefined {
    const want = nfc(seg);
    const matches = this.entries(id, where).filter((e) => nfc(e.name) === want);
    if (matches.length > 1) throw collision(where === '' ? want : `${where}/${want}`, this.repo, this.span);
    return matches[0];
  }

  linkTarget(id: string): string {
    return new TextDecoder().decode(this.object(id, 'blob'));
  }
}

function collision(path: string, repo: string, span?: SourceSpan): SitefileError {
  return new SitefileError('E_PATH_COLLISION', `${redactUrl(repo)}: two names in the repository have the same Unicode NFC form '${path}'; rename one`, span);
}

interface Resolved {
  readonly entry: { readonly kind: 'tree' | 'blob'; readonly id: string; readonly exec: boolean };
  /** Real (symlink-free) path segments from the repository root. */
  readonly real: readonly string[];
}

const MAX_LINKS = 40;

/**
 * Resolve `segs` from the repository root, following symbolic links (relative targets only; at
 * most 40). The result must lie under `boundary` (E_PATH_ESCAPE). `link` names the symlink being
 * resolved for messages (dangling links are E_NOT_FOUND, loops E_UNSUPPORTED_FILE).
 */
function resolvePath(r: Repo, root: string, segs: readonly string[], boundary: readonly string[], shown: string, link: boolean, repo: string, span?: SourceSpan): Resolved {
  const escape = (): SitefileError =>
    new SitefileError('E_PATH_ESCAPE', link ? `symlink '${shown}' points outside the imported tree` : `'${shown}' resolves outside the repository`, span);
  const missing = (): SitefileError =>
    new SitefileError('E_NOT_FOUND', link ? `symlink '${shown}' is dangling` : `'${shown}' does not exist in ${redactUrl(repo)}`, span);
  const pending = [...segs];
  const stack: Array<{ name: string; id: string }> = [];
  let cur: { kind: 'tree' | 'blob'; id: string; exec: boolean } = { kind: 'tree', id: root, exec: false };
  let hops = 0;
  while (pending.length > 0) {
    const seg = pending.shift()!;
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      if (cur.kind !== 'tree') throw missing();
      if (stack.length === 0) throw escape();
      stack.pop();
      cur = { kind: 'tree', id: stack.length ? stack[stack.length - 1]!.id : root, exec: false };
      continue;
    }
    if (cur.kind !== 'tree') throw missing();
    const where = stack.map((s) => s.name).join('/');
    const e = r.child(cur.id, where, seg);
    if (!e) throw missing();
    if (e.kind === 'gitlink') throw submodule(where === '' ? e.name : `${where}/${e.name}`, span);
    if (e.kind === 'link') {
      if (++hops > MAX_LINKS) throw new SitefileError('E_UNSUPPORTED_FILE', `'${shown}' is a symlink loop`, span);
      const target = r.linkTarget(e.id);
      if (target.startsWith('/') || target === '') throw escape();
      pending.unshift(...target.split('/'));
      continue;
    }
    stack.push({ name: nfc(e.name), id: e.id });
    cur = { kind: e.kind, id: e.id, exec: e.exec };
  }
  const real = stack.map((s) => s.name);
  if (boundary.some((b, i) => real[i] !== b) || real.length < boundary.length) throw escape();
  return { entry: cur, real };
}

function submodule(path: string, span?: SourceSpan): SitefileError {
  return new SitefileError('E_UNSUPPORTED_FILE', `'${path}' is a Git submodule; submodules are not supported (import that repository separately)`, span);
}

/**
 * The content of `loc.path` in a fetched commit: a tree's files (symlinks resolved, like a local
 * directory walk: a link must stay inside the imported tree, directory links back to an ancestor
 * are skipped) or one file. Entry names are checked like archive entries; the file count and total
 * size are bounded by `limits`.
 */
export function selectGitPath(objs: GitObjects, loc: Pick<GitLocation, 'repo' | 'path'>, limits: ArchiveLimits, span?: SourceSpan): GitSelection {
  const r = new Repo(objs, loc.repo, span);
  const root = r.rootTree();
  const segs = loc.path === '' ? [] : loc.path.split('/');
  const top = resolvePath(r, root, segs, [], loc.path, false, loc.repo, span);
  if (top.entry.kind === 'blob') {
    const bytes = r.object(top.entry.id, 'blob');
    return { kind: 'file', bytes, mode: top.entry.exec ? 0o755 : 0o644, basename: segs[segs.length - 1] ?? '' };
  }
  const files: Array<[string, { bytes: Uint8Array; mode: number }]> = [];
  let total = 0;
  const walk = (id: string, real: readonly string[], logical: string, ancestors: Set<string>): void => {
    const where = real.join('/');
    const seen = new Set<string>();
    const entries = [...r.entries(id, where)].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    for (const e of entries) {
      const name = nfc(e.name);
      const shownLogical = logical === '' ? name : `${logical}/${name}`;
      if (seen.has(name)) throw collision(shownLogical, loc.repo, span);
      seen.add(name);
      let kind: 'tree' | 'blob';
      let entryId: string;
      let exec: boolean;
      let childReal: readonly string[];
      if (e.kind === 'gitlink') throw submodule(where === '' ? name : `${where}/${name}`, span);
      if (e.kind === 'link') {
        const shown = where === '' ? name : `${where}/${name}`;
        const res = resolvePath(r, root, [...real, name], top.real, shown, true, loc.repo, span);
        kind = res.entry.kind;
        entryId = res.entry.id;
        exec = res.entry.exec;
        childReal = res.real;
      } else {
        kind = e.kind;
        entryId = e.id;
        exec = e.exec;
        childReal = [...real, name];
      }
      if (kind === 'blob') {
        const bytes = r.object(entryId, 'blob');
        if (files.length + 1 > limits.maxEntries) throw new SitefileError('E_ARCHIVE_LIMIT', `${redactUrl(loc.repo)}: more than ${limits.maxEntries} files`, span);
        total += bytes.byteLength;
        if (total > limits.maxTotalBytes) throw new SitefileError('E_ARCHIVE_LIMIT', `${redactUrl(loc.repo)}: files exceed ${limits.maxTotalBytes} bytes`, span);
        files.push(['/' + shownLogical, { bytes, mode: exec ? 0o755 : 0o644 }]);
      } else {
        const key = childReal.join('/');
        if (ancestors.has(key)) continue; // directory symlink cycle
        ancestors.add(key);
        try {
          walk(entryId, childReal, shownLogical, ancestors);
        } finally {
          ancestors.delete(key);
        }
      }
    }
  };
  walk(top.entry.id, top.real, '', new Set([top.real.join('/')]));
  return { kind: 'tree', files };
}
