// What the TypeScript reference does with one archive, as plain JSON.
// Shared by generate.mjs (which writes the fixtures) and the TypeScript test
// test/archive-fuzz-fixture.test.ts (which checks they are up to date).
//
//   { input, limits, result, bundle? }
//   input:   the archive bytes, base64
//   limits:  [maxEntries, maxTotalBytes, maxRatio]
//   result:  { ok: [[path, mode, digest], …] }  extractArchive's tree, sorted by UTF-8 bytes
//          | { error: [code, detail] }
//   bundle:  only for inputs readWebBundle accepts:
//            { version, primary, entries: [[url, status, [[name, value], …], digest], …] }
// A non-SitefileError exception is recorded as { crash: message } and a tree
// over the limits as { violation: message }: both are bugs.
import path from 'node:path';
import { pathToFileURL } from 'node:url';

export const ORIGIN = 'fuzz.bin';

export async function loadLib(root) {
  const imp = (p) => import(pathToFileURL(path.join(root, 'lib/src', p)).href);
  const [{ extractArchive }, { readWebBundle, isWebBundle }, { SitefileError }, { sha256 }] = await Promise.all([
    imp('resources/archive.js'),
    imp('resources/wbn-reader.js'),
    imp('errors.js'),
    imp('digest.js'),
  ]);
  return { extractArchive, readWebBundle, isWebBundle, SitefileError, sha256 };
}

const wf = (s) => (typeof s === 'string' ? s.toWellFormed() : s);
const utf8Order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));

function failure(lib, e) {
  if (e instanceof lib.SitefileError) return { error: [e.code, wf(e.detail)] };
  return { crash: wf(String(e?.stack ?? e)) };
}

export function limitsOf([maxEntries, maxTotalBytes, maxRatio]) {
  return { maxEntries, maxTotalBytes, maxRatio };
}

export async function record(lib, input, limits) {
  const bytes = new Uint8Array(Buffer.from(input, 'base64'));
  const out = { input, limits };
  const l = limitsOf(limits);
  try {
    const tree = await lib.extractArchive(bytes, ORIGIN, l);
    const files = tree.entries().map(([p, n]) => [wf(p), n.mode, n.digest]);
    files.sort(([a], [b]) => utf8Order(a, b));
    const total = tree.entries().reduce((s, [, n]) => s + n.bytes.byteLength, 0);
    if (files.length > l.maxEntries) out.result = { violation: `${files.length} files > ${l.maxEntries}` };
    else if (total > l.maxTotalBytes) out.result = { violation: `${total} bytes > ${l.maxTotalBytes}` };
    else out.result = { ok: files };
  } catch (e) {
    out.result = failure(lib, e);
  }
  if (lib.isWebBundle(bytes)) {
    try {
      const b = lib.readWebBundle(bytes, ORIGIN, l);
      out.bundle = {
        version: b.version,
        primary: wf(b.primaryUrl ?? null),
        entries: b.entries.map((e) => [wf(e.url), e.status, Object.entries(e.headers).map(([k, v]) => [k, v]), lib.sha256(e.body)]),
      };
    } catch {
      // extractArchive already recorded the error.
    }
  }
  return out;
}
