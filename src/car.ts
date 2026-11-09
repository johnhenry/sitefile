import { createHash } from 'node:crypto';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';

/**
 * `EXPORT … AS car` (issue #208): the tree as a UnixFS DAG in a CARv1 file, the way
 * `ipfs add -r --hidden --cid-version=1` (kubo) builds it, so the root CID is the one kubo
 * prints for the same directory:
 *
 * - files are cut into 262144-byte chunks (kubo's default `size-262144` chunker); each chunk
 *   is a raw block (CIDv1, codec raw 0x55); a one-chunk file is that block;
 * - longer files are a balanced tree of dag-pb UnixFS File nodes of at most 174 links;
 * - directories are dag-pb UnixFS Directory nodes, links sorted by name (no HAMT sharding:
 *   a directory large enough for kubo to shard is refused);
 * - every CID is v1 with a SHA-256 multihash; the root is the tree's `/` directory.
 *
 * The CAR holds every block once, root first, then depth first in link order. No
 * timestamps or modes are recorded (kubo stores neither by default), so the output
 * depends only on paths and contents.
 */
export const CHUNK_SIZE = 262144;
export const MAX_LINKS = 174;
/** kubo shards a directory (HAMT) when the names and CIDs of its links exceed this many bytes. */
export const HAMT_THRESHOLD = 262144;

const RAW = 0x55;
const DAG_PB = 0x70;

function varint(n: number): number[] {
  const out: number[] = [];
  let v = n;
  while (v >= 0x80) {
    out.push((v % 0x80) | 0x80);
    v = Math.floor(v / 0x80);
  }
  out.push(v);
  return out;
}

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

/** CIDv1 bytes: version, codec, sha2-256 multihash. */
function cidOf(codec: number, block: Uint8Array): Uint8Array {
  const digest = createHash('sha256').update(block).digest();
  return new Uint8Array([1, codec, 0x12, 0x20, ...digest]);
}

const B32 = 'abcdefghijklmnopqrstuvwxyz234567';

/** A CID as text: multibase base32 (lower case, no padding) with the `b` prefix. */
export function cidString(cid: Uint8Array): string {
  let out = 'b';
  let bits = 0;
  let value = 0;
  for (const byte of cid) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      out += B32[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
    value &= (1 << bits) - 1;
  }
  if (bits > 0) out += B32[(value << (5 - bits)) & 31];
  return out;
}

// --- protobuf (only what dag-pb and UnixFS need) ------------------------------

const field = (num: number, wire: number): number[] => varint(num * 8 + wire);
const pbVarint = (num: number, v: number): number[] => [...field(num, 0), ...varint(v)];
const pbBytes = (num: number, b: Uint8Array): Uint8Array => concat([new Uint8Array([...field(num, 2), ...varint(b.byteLength)]), b]);

/** UnixFS `Data` message: Type, filesize, blocksizes (Data and the rest omitted). */
function unixfs(type: 1 | 2, filesize?: number, blocksizes: readonly number[] = []): Uint8Array {
  return new Uint8Array([...pbVarint(1, type), ...(filesize !== undefined ? pbVarint(3, filesize) : []), ...blocksizes.flatMap((s) => pbVarint(4, s))]);
}

interface Link {
  readonly cid: Uint8Array;
  readonly name: string;
  /** The block's size plus every block below it (dag-pb Tsize). */
  readonly tsize: number;
}

/** A dag-pb node: links first (field 2), then data (field 1), as the dag-pb spec's canonical form has it. */
function dagPb(links: readonly Link[], data: Uint8Array): Uint8Array {
  const enc = new TextEncoder();
  const parts = links.map((l) => pbBytes(2, concat([pbBytes(1, l.cid), pbBytes(2, enc.encode(l.name)), new Uint8Array(pbVarint(3, l.tsize))])));
  return concat([...parts, pbBytes(1, data)]);
}

interface Node {
  readonly cid: Uint8Array;
  readonly tsize: number;
  /** File bytes below this node (UnixFS blocksizes of the parent). */
  readonly fileSize: number;
}

class Dag {
  /** Blocks by CID (hex), in creation order, and their children for the CAR walk. */
  readonly blocks = new Map<string, { cid: Uint8Array; data: Uint8Array; children: readonly Uint8Array[] }>();

  put(codec: number, data: Uint8Array, children: readonly Uint8Array[]): Uint8Array {
    const cid = cidOf(codec, data);
    const key = Buffer.from(cid).toString('hex');
    if (!this.blocks.has(key)) this.blocks.set(key, { cid, data, children });
    return cid;
  }

  file(bytes: Uint8Array): Node {
    const leaves: Node[] = [];
    for (let o = 0; o < bytes.byteLength || (o === 0 && leaves.length === 0); o += CHUNK_SIZE) {
      const chunk = bytes.subarray(o, Math.min(o + CHUNK_SIZE, bytes.byteLength));
      leaves.push({ cid: this.put(RAW, chunk, []), tsize: chunk.byteLength, fileSize: chunk.byteLength });
      if (bytes.byteLength === 0) break;
    }
    if (leaves.length === 1) return leaves[0] as Node;
    let depth = 1;
    let span = MAX_LINKS;
    while (span < leaves.length) {
      depth++;
      span *= MAX_LINKS;
    }
    return this.fileNode(leaves, depth);
  }

  /** kubo's balanced layout: every subtree but the last is full. */
  private fileNode(leaves: readonly Node[], depth: number): Node {
    let children: Node[];
    if (depth === 1) children = [...leaves];
    else {
      const per = MAX_LINKS ** (depth - 1);
      children = [];
      for (let i = 0; i < leaves.length; i += per) children.push(this.fileNode(leaves.slice(i, i + per), depth - 1));
    }
    const fileSize = children.reduce((n, c) => n + c.fileSize, 0);
    const block = dagPb(
      children.map((c) => ({ cid: c.cid, name: '', tsize: c.tsize })),
      unixfs(2, fileSize, children.map((c) => c.fileSize)),
    );
    const cid = this.put(DAG_PB, block, children.map((c) => c.cid));
    return { cid, tsize: block.byteLength + children.reduce((n, c) => n + c.tsize, 0), fileSize };
  }

  dir(path: string, entries: ReadonlyArray<readonly [string, Node]>): Node {
    const enc = new TextEncoder();
    // dag-pb sorts links by name bytes.
    const sorted = [...entries].sort(([a], [b]) => Buffer.compare(enc.encode(a), enc.encode(b)));
    const estimate = sorted.reduce((n, [name, c]) => n + enc.encode(name).byteLength + c.cid.byteLength, 0);
    if (estimate > HAMT_THRESHOLD) {
      throw new SitefileError('E_EXPORT', `car export: directory '${path}' has too many entries for a plain UnixFS directory (IPFS would shard it; HAMT directories are not supported)`);
    }
    const block = dagPb(
      sorted.map(([name, c]) => ({ cid: c.cid, name, tsize: c.tsize })),
      unixfs(1),
    );
    const cid = this.put(DAG_PB, block, sorted.map(([, c]) => c.cid));
    return { cid, tsize: block.byteLength + sorted.reduce((n, [, c]) => n + c.tsize, 0), fileSize: 0 };
  }
}

interface DirEntry {
  readonly files: Map<string, Uint8Array>;
  readonly dirs: Map<string, DirEntry>;
}

function buildDag(tree: Tree): { dag: Dag; root: Uint8Array } {
  const top: DirEntry = { files: new Map(), dirs: new Map() };
  for (const [p, n] of tree.entries()) {
    const segs = p.slice(1).split('/');
    let d = top;
    for (const s of segs.slice(0, -1)) {
      let next = d.dirs.get(s);
      if (!next) d.dirs.set(s, (next = { files: new Map(), dirs: new Map() }));
      d = next;
    }
    d.files.set(segs[segs.length - 1] as string, n.bytes);
  }
  const dag = new Dag();
  const walk = (d: DirEntry, path: string): Node => {
    const entries: Array<readonly [string, Node]> = [];
    for (const [name, sub] of d.dirs) entries.push([name, walk(sub, path === '/' ? `/${name}` : `${path}/${name}`)]);
    for (const [name, bytes] of d.files) entries.push([name, dag.file(bytes)]);
    return dag.dir(path, entries);
  };
  return { dag, root: walk(top, '/').cid };
}

/** The root CID of `tree` as `EXPORT … AS car` writes it. */
export function carRoot(tree: Tree): string {
  return cidString(buildDag(tree).root);
}

/** CARv1: a dag-cbor header `{roots: [root], version: 1}`, then each block as varint(len) CID data. */
export function writeCar(tree: Tree): Uint8Array {
  return encodeCar(tree).bytes;
}

/** {@link writeCar} and the root CID. */
export function encodeCar(tree: Tree): { bytes: Uint8Array; root: string } {
  const { dag, root } = buildDag(tree);
  const enc = new TextEncoder();
  // dag-cbor: map(2) "roots" -> [tag 42 bytes(0x00 ‖ cid)], "version" -> 1 (keys by length, then bytes).
  const cidBytes = [0x00, ...root];
  const header = new Uint8Array([0xa2, 0x65, ...enc.encode('roots'), 0x81, 0xd8, 0x2a, 0x58, cidBytes.length, ...cidBytes, 0x67, ...enc.encode('version'), 0x01]);
  const parts: Uint8Array[] = [new Uint8Array(varint(header.byteLength)), header];
  const seen = new Set<string>();
  const stack: Uint8Array[] = [root];
  while (stack.length) {
    const cid = stack.pop() as Uint8Array;
    const key = Buffer.from(cid).toString('hex');
    if (seen.has(key)) continue;
    seen.add(key);
    const b = dag.blocks.get(key);
    if (!b) throw new Error('car: missing block');
    parts.push(new Uint8Array(varint(cid.byteLength + b.data.byteLength)), cid, b.data);
    for (let i = b.children.length - 1; i >= 0; i--) stack.push(b.children[i] as Uint8Array);
  }
  return { bytes: concat(parts), root: cidString(root) };
}

/** The CID of one file's UnixFS DAG (a raw block for a file of one chunk). */
export function fileCid(bytes: Uint8Array): string {
  return cidString(new Dag().file(bytes).cid);
}
