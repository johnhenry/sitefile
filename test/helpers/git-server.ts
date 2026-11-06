import { createHash } from 'node:crypto';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { deflateSync } from 'node:zlib';

/**
 * A tiny Git smart-HTTP (protocol v2) server for tests and the conformance network cases: it builds
 * repositories in memory (blobs, trees, commits, annotated tags) and answers
 *
 *   GET  <repo>/info/refs?service=git-upload-pack   capability advertisement
 *   POST <repo>/git-upload-pack                     ls-refs and fetch (want, deepen, done)
 *
 * The packfile it sends holds every object reachable from the wanted commit's tree (and the commit),
 * and can deltify some objects (OFS_DELTA and REF_DELTA) so clients' delta code is exercised.
 * It speaks only what Sitefile's client sends; it is not a general Git server.
 */

export type RepoFile = string | Uint8Array | { readonly data: string | Uint8Array; readonly mode: '100644' | '100755' | '120000' } | { readonly gitlink: string };

export interface GitObject {
  readonly type: 'commit' | 'tree' | 'blob' | 'tag';
  readonly data: Uint8Array;
}

const enc = new TextEncoder();
const bytesOf = (d: string | Uint8Array): Uint8Array => (typeof d === 'string' ? enc.encode(d) : d);

export function objectId(type: GitObject['type'], data: Uint8Array): string {
  return createHash('sha1').update(`${type} ${data.byteLength}\0`).update(data).digest('hex');
}

/** An object store with helpers to write trees from file maps and commits. */
export class MemRepo {
  readonly objects = new Map<string, GitObject>();
  readonly refs = new Map<string, string>();
  /** `refs/tags/x` -> the commit an annotated tag points at (ls-refs `peeled:`). */
  readonly peeled = new Map<string, string>();
  head = 'refs/heads/main';
  /** Deltify objects in sent packs. */
  deltas = true;

  put(type: GitObject['type'], data: Uint8Array): string {
    const id = objectId(type, data);
    this.objects.set(id, { type, data });
    return id;
  }

  /** Write a tree for `files` (paths `a/b.txt`) and return its id. */
  tree(files: Readonly<Record<string, RepoFile>>): string {
    interface Dir {
      readonly dirs: Map<string, Dir>;
      readonly files: Map<string, { mode: string; id: string }>;
    }
    const root: Dir = { dirs: new Map(), files: new Map() };
    for (const [p, f] of Object.entries(files)) {
      const segs = p.split('/');
      let d = root;
      for (const s of segs.slice(0, -1)) {
        let next = d.dirs.get(s);
        if (!next) d.dirs.set(s, (next = { dirs: new Map(), files: new Map() }));
        d = next;
      }
      const name = segs[segs.length - 1]!;
      if (typeof f === 'object' && 'gitlink' in f) d.files.set(name, { mode: '160000', id: f.gitlink });
      else if (typeof f === 'string' || f instanceof Uint8Array) d.files.set(name, { mode: '100644', id: this.put('blob', bytesOf(f)) });
      else d.files.set(name, { mode: f.mode, id: this.put('blob', bytesOf(f.data)) });
    }
    const write = (d: Dir): string => {
      const entries: Array<{ name: string; mode: string; id: string }> = [];
      for (const [name, sub] of d.dirs) entries.push({ name, mode: '40000', id: write(sub) });
      for (const [name, f] of d.files) entries.push({ name, ...f });
      // Git sorts tree entries by name, directories compared as if they ended in '/'.
      const key = (e: { name: string; mode: string }): string => (e.mode === '40000' ? e.name + '/' : e.name);
      entries.sort((a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0));
      const parts: Uint8Array[] = [];
      for (const e of entries) parts.push(enc.encode(`${e.mode} ${e.name}\0`), Buffer.from(e.id, 'hex'));
      return this.put('tree', Buffer.concat(parts));
    };
    return write(root);
  }

  /** Commit `files` on `ref` (default refs/heads/main) and return the commit id. */
  commit(files: Readonly<Record<string, RepoFile>>, ref = 'refs/heads/main', message = 'commit'): string {
    const tree = this.tree(files);
    const parent = this.refs.get(ref);
    const text = `tree ${tree}\n${parent ? `parent ${parent}\n` : ''}author T <t@example.com> 0 +0000\ncommitter T <t@example.com> 0 +0000\n\n${message}\n`;
    const id = this.put('commit', enc.encode(text));
    this.refs.set(ref, id);
    return id;
  }

  /** An annotated tag object for `commit`. */
  tag(name: string, commit: string): string {
    const text = `object ${commit}\ntype commit\ntag ${name}\ntagger T <t@example.com> 0 +0000\n\n${name}\n`;
    const id = this.put('tag', enc.encode(text));
    this.refs.set(`refs/tags/${name}`, id);
    this.peeled.set(`refs/tags/${name}`, commit);
    return id;
  }

  /** Ids of the commit and everything its tree reaches (not its parents: a shallow fetch). */
  reachable(commit: string): string[] {
    const out: string[] = [commit];
    const c = this.objects.get(commit);
    if (!c) return out;
    const tree = /^tree ([0-9a-f]{40})/.exec(new TextDecoder().decode(c.data))![1]!;
    const walk = (id: string): void => {
      out.push(id);
      const t = this.objects.get(id)!;
      let i = 0;
      while (i < t.data.byteLength) {
        const sp = t.data.indexOf(0x20, i);
        const nul = t.data.indexOf(0, sp);
        const mode = new TextDecoder().decode(t.data.subarray(i, sp));
        const child = Buffer.from(t.data.subarray(nul + 1, nul + 21)).toString('hex');
        if (mode === '40000') walk(child);
        else if (mode !== '160000' && !out.includes(child)) out.push(child);
        i = nul + 21;
      }
    };
    walk(tree);
    return [...new Set(out)];
  }
}

const TYPE_CODE = { commit: 1, tree: 2, blob: 3, tag: 4 } as const;

function header(type: number, size: number): Uint8Array {
  const out: number[] = [];
  let c = (type << 4) | (size & 15);
  let s = Math.floor(size / 16);
  while (s > 0) {
    out.push(c | 0x80);
    c = s & 0x7f;
    s = Math.floor(s / 128);
  }
  out.push(c);
  return Uint8Array.from(out);
}

function varint(n: number): number[] {
  const out: number[] = [];
  do {
    let c = n & 0x7f;
    n = Math.floor(n / 128);
    if (n > 0) c |= 0x80;
    out.push(c);
  } while (n > 0);
  return out;
}

/** A delta that copies `base` in one or more pieces, then inserts the rest of `target` (target must start with a prefix of base). */
function makeDelta(base: Uint8Array, target: Uint8Array): Uint8Array | undefined {
  let common = 0;
  while (common < base.byteLength && common < target.byteLength && base[common] === target[common]) common++;
  if (common < 8) return undefined;
  const out = [...varint(base.byteLength), ...varint(target.byteLength)];
  // Copy in chunks of up to 0x10000 (size 0 means 0x10000), exercising multi-byte offsets.
  for (let off = 0; off < common; ) {
    const len = Math.min(common - off, 0x10000);
    let op = 0x80;
    const extra: number[] = [];
    for (let i = 0; i < 4; i++) {
      const b = Math.floor(off / 2 ** (8 * i)) & 0xff;
      if (b) {
        op |= 1 << i;
        extra.push(b);
      }
    }
    if (len !== 0x10000) {
      for (let i = 0; i < 3; i++) {
        const b = Math.floor(len / 2 ** (8 * i)) & 0xff;
        if (b) {
          op |= 0x10 << i;
          extra.push(b);
        }
      }
    }
    out.push(op, ...extra);
    off += len;
  }
  for (let i = common; i < target.byteLength; i += 127) {
    const chunk = target.subarray(i, Math.min(i + 127, target.byteLength));
    out.push(chunk.byteLength, ...chunk);
  }
  return Uint8Array.from(out);
}

/**
 * A version 2 packfile of `ids`. With `deltas`, a blob that shares a long prefix with an earlier
 * blob of the pack is sent as a delta against it: alternately OFS_DELTA and REF_DELTA, and one
 * chained delta (a delta whose base is itself a delta) when the pack allows it.
 */
export function writePack(repo: MemRepo, ids: readonly string[], deltas = repo.deltas): Uint8Array {
  const parts: Uint8Array[] = [];
  const head = Buffer.alloc(12);
  head.write('PACK', 0, 'latin1');
  head.writeUInt32BE(2, 4);
  head.writeUInt32BE(ids.length, 8);
  parts.push(head);
  let offset = 12;
  const offsets = new Map<string, number>();
  let n = 0;
  const sent: string[] = [];
  for (const id of ids) {
    const o = repo.objects.get(id)!;
    let entry: Uint8Array | undefined;
    if (deltas && o.type === 'blob') {
      for (const baseId of [...sent].reverse()) {
        const base = repo.objects.get(baseId)!;
        if (base.type !== 'blob') continue;
        const d = makeDelta(base.data, o.data);
        if (!d) continue;
        const z = deflateSync(d);
        if (n++ % 2 === 0) {
          const rel = offset - offsets.get(baseId)!;
          const enc7: number[] = [rel & 0x7f];
          let r = Math.floor(rel / 128);
          while (r > 0) {
            r -= 1;
            enc7.unshift(0x80 | (r & 0x7f));
            r = Math.floor(r / 128);
          }
          entry = Buffer.concat([header(6, d.byteLength), Uint8Array.from(enc7), z]);
        } else {
          entry = Buffer.concat([header(7, d.byteLength), Buffer.from(baseId, 'hex'), z]);
        }
        break;
      }
    }
    entry ??= Buffer.concat([header(TYPE_CODE[o.type], o.data.byteLength), deflateSync(o.data)]);
    offsets.set(id, offset);
    sent.push(id);
    parts.push(entry);
    offset += entry.byteLength;
  }
  const body = Buffer.concat(parts);
  return Buffer.concat([body, createHash('sha1').update(body).digest()]);
}

function pkt(data: string | Uint8Array): Uint8Array {
  const b = bytesOf(data);
  return Buffer.concat([enc.encode((b.byteLength + 4).toString(16).padStart(4, '0')), b]);
}

function readPkts(body: Uint8Array): string[] {
  const out: string[] = [];
  let i = 0;
  while (i + 4 <= body.byteLength) {
    const n = parseInt(new TextDecoder().decode(body.subarray(i, i + 4)), 16);
    if (n < 4) {
      out.push(n === 0 ? '<flush>' : '<delim>');
      i += 4;
      continue;
    }
    out.push(new TextDecoder().decode(body.subarray(i + 4, i + n)).replace(/\n$/, ''));
    i += n;
  }
  return out;
}

export interface GitServerOptions {
  /** Path prefix -> repository (`/repo.git`). */
  readonly repos: ReadonlyMap<string, MemRepo>;
  /** Every request seen (method, path, headers), for assertions. */
  readonly log?: Array<{ method: string; path: string; headers: IncomingMessage['headers'] }>;
}

/**
 * Handle a Git request; returns false when the path is not a repository URL. `/dumb.git/...` answers
 * like a dumb server (text/plain) and `/v1.git/...` without protocol v2, for error tests.
 */
export function handleGit(req: IncomingMessage, res: ServerResponse, url: URL, opts: GitServerOptions): boolean {
  const p = url.pathname;
  const match = [...opts.repos.keys()].find((k) => p === `${k}/info/refs` || p === `${k}/git-upload-pack`);
  if (match === undefined) return false;
  const repo = opts.repos.get(match)!;
  opts.log?.push({ method: req.method ?? '', path: p + url.search, headers: req.headers });
  if (p.endsWith('/info/refs')) {
    if (url.searchParams.get('service') !== 'git-upload-pack') {
      res.writeHead(403).end();
      return true;
    }
    if (match.endsWith('/dumb.git')) {
      res.writeHead(200, { 'content-type': 'text/plain' }).end('ref: refs/heads/main\n');
      return true;
    }
    const v2 = String(req.headers['git-protocol'] ?? '').includes('version=2') && !match.endsWith('/v1.git');
    const lines = v2
      ? [pkt('# service=git-upload-pack\n'), enc.encode('0000'), pkt('version 2\n'), pkt('agent=mock/1\n'), pkt('ls-refs=unborn\n'), pkt('fetch=shallow\n'), pkt('server-option\n'), pkt('object-format=sha1\n'), enc.encode('0000')]
      : [pkt('# service=git-upload-pack\n'), enc.encode('0000'), pkt(`${repo.refs.get(repo.head) ?? '0'.repeat(40)} HEAD\0side-band-64k\n`), enc.encode('0000')];
    res.writeHead(200, { 'content-type': 'application/x-git-upload-pack-advertisement', 'cache-control': 'no-cache' });
    res.end(Buffer.concat(lines));
    return true;
  }
  if (req.method !== 'POST') {
    res.writeHead(405).end();
    return true;
  }
  const chunks: Buffer[] = [];
  req.on('data', (c: Buffer) => chunks.push(c));
  req.on('end', () => {
    const lines = readPkts(Buffer.concat(chunks));
    const cmd = lines[0];
    const out: Uint8Array[] = [];
    if (cmd === 'command=ls-refs') {
      const prefixes = lines.filter((l) => l.startsWith('ref-prefix ')).map((l) => l.slice('ref-prefix '.length));
      const all: Array<[string, string]> = [...repo.refs].sort((a, b) => (a[0] < b[0] ? -1 : 1));
      const head = repo.refs.get(repo.head);
      if (head) all.unshift(['HEAD', head]);
      for (const [name, id] of all) {
        if (prefixes.length && !prefixes.some((x) => name.startsWith(x))) continue;
        const peeled = repo.peeled.get(name);
        const symref = name === 'HEAD' ? ` symref-target:${repo.head}` : '';
        out.push(pkt(`${id} ${name}${symref}${peeled ? ` peeled:${peeled}` : ''}\n`));
      }
      out.push(enc.encode('0000'));
    } else if (cmd === 'command=fetch') {
      const wants = lines.filter((l) => l.startsWith('want ')).map((l) => l.slice(5));
      const missing = wants.find((w) => repo.objects.get(w)?.type !== 'commit');
      if (missing) {
        out.push(pkt(`ERR upload-pack: not our ref ${missing}\n`));
      } else {
        const ids = [...new Set(wants.flatMap((w) => repo.reachable(w)))];
        const pack = writePack(repo, ids);
        if (lines.includes('deepen 1')) {
          out.push(pkt('shallow-info\n'));
          for (const w of wants) out.push(pkt(`shallow ${w}\n`));
          out.push(enc.encode('0001'));
        }
        out.push(pkt('packfile\n'));
        out.push(pkt(Buffer.concat([Uint8Array.of(2), enc.encode('Enumerating objects: done.\n')])));
        for (let i = 0; i < pack.byteLength; i += 8000) out.push(pkt(Buffer.concat([Uint8Array.of(1), pack.subarray(i, i + 8000)])));
        out.push(enc.encode('0000'));
      }
    } else {
      out.push(pkt('ERR unknown command\n'));
    }
    res.writeHead(200, { 'content-type': 'application/x-git-upload-pack-result', 'cache-control': 'no-cache' });
    res.end(Buffer.concat(out));
  });
  return true;
}
