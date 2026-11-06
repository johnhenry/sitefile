import path from 'node:path';
import { makeClient } from './base-image.js';
import { sha256 } from './digest.js';
import { isSitefileError } from './errors.js';
import { loadProgram, type BuildOptions } from './executor.js';
import { plan } from './planner.js';
import { parseRef, withDefaultRegistry } from './ref.js';
import { fetchRemote } from './resources/https.js';
import { Lockfile } from './resources/lockfile.js';
import { DEFAULT_FETCH_POLICY, type FetchPolicy } from './resources/types.js';

/**
 * `sitefile outdated` (issue #104): report, for every remote input, whether what it points at today
 * differs from its pin. It never writes anything (no lockfile, no image store); `--update-lock`
 * stays the way to change pins.
 *
 * What is checkable, honestly:
 *   - an https IMPORT has no notion of "a newer version": the URL is fetched again (a full GET under
 *     the build's network policy) and the body's sha256 is compared with the pin (INTEGRITY, else the
 *     Sitefile.lock entry). Sitefile.lock records no ETag or Last-Modified, so there is nothing to make
 *     a conditional request against, and a changed validator would not prove changed content anyway.
 *     A version inside a URL (`.../lib@1.2.3/x.js`) is not interpreted.
 *   - a BASE tag (`nginx:1.27-alpine`, with or without `@sha256:`) is resolved again (one manifest
 *     GET) and compared with the digest pinned in the Sitefile or recorded in Sitefile.lock: "changed"
 *     means the tag has moved. A digest-only reference (`nginx@sha256:...`) has no tag to follow.
 *   - for a tag that looks like a version (`[v]N[.N[.N]][-suffix]`), the registry's tag list is read and
 *     the highest tag of the same shape (same `v`, number of components and suffix) that is greater is
 *     reported as `newerTag`. Registries that refuse to list tags just give no `newerTag`.
 */
export type OutdatedStatus = 'current' | 'changed' | 'unpinned' | 'digest-only' | 'error';

export interface OutdatedEntry {
  readonly kind: 'import' | 'base';
  /** The IMPORT name, or `BASE`. */
  readonly name: string;
  /** The URL, or the canonical image reference. */
  readonly source: string;
  /** The pinned digest, if any. */
  readonly pin: string | null;
  readonly pinnedBy: 'integrity' | 'lockfile' | 'digest' | null;
  /** What the source resolves to now (body sha256, or the tag's manifest/index digest). */
  readonly latest: string | null;
  readonly status: OutdatedStatus;
  /** BASE only: a greater version-like tag of the same shape. */
  readonly newerTag: string | null;
  readonly error: { readonly code: string; readonly message: string } | null;
}

export interface OutdatedReport {
  readonly inputs: readonly OutdatedEntry[];
  readonly summary: Readonly<Record<OutdatedStatus, number>>;
}

const VERSION_TAG = /^(v?)(\d+)(?:\.(\d+))?(?:\.(\d+))?(-[0-9A-Za-z][0-9A-Za-z._-]*)?$/;

interface VersionTag {
  readonly prefix: string;
  readonly parts: readonly string[];
  readonly suffix: string;
}

function versionTag(tag: string): VersionTag | undefined {
  const m = VERSION_TAG.exec(tag);
  if (!m) return undefined;
  const parts = [m[2], m[3], m[4]].filter((x): x is string => x !== undefined).map((x) => x.replace(/^0+(?=\d)/, ''));
  return { prefix: m[1] ?? '', parts, suffix: m[5] ?? '' };
}

/** Numeric comparison of two digit strings without leading zeros. */
function cmpNum(a: string, b: string): number {
  return a.length !== b.length ? a.length - b.length : a < b ? -1 : a > b ? 1 : 0;
}

function cmpVersion(a: VersionTag, b: VersionTag): number {
  for (let i = 0; i < a.parts.length; i++) {
    const c = cmpNum(a.parts[i] as string, b.parts[i] as string);
    if (c !== 0) return c;
  }
  return 0;
}

/** The greatest tag of `tags` with the same shape as `current` that is greater than it (ties: the first in byte order). */
export function newerVersionTag(current: string, tags: readonly string[]): string | undefined {
  const cur = versionTag(current);
  if (!cur) return undefined;
  let best: { tag: string; v: VersionTag } | undefined;
  for (const t of [...tags].sort()) {
    const v = versionTag(t);
    if (!v || v.prefix !== cur.prefix || v.suffix !== cur.suffix || v.parts.length !== cur.parts.length) continue;
    if (cmpVersion(v, cur) <= 0) continue;
    if (!best || cmpVersion(v, best.v) > 0) best = { tag: t, v };
  }
  return best?.tag;
}

function errorOf(e: unknown): { code: string; message: string } {
  if (isSitefileError(e)) return { code: e.code, message: e.detail };
  throw e;
}

/** An entry with its keys in the documented order (the JSON output is compared across implementations). */
function entry(e: OutdatedEntry): OutdatedEntry {
  return { kind: e.kind, name: e.name, source: e.source, pin: e.pin, pinnedBy: e.pinnedBy, latest: e.latest, status: e.status, newerTag: e.newerTag, error: e.error };
}

function statusOf(pin: string | null, latest: string): OutdatedStatus {
  return pin === null ? 'unpinned' : pin === latest ? 'current' : 'changed';
}

export async function outdated(options: BuildOptions): Promise<OutdatedReport> {
  const { program, sitefilePath } = await loadProgram(options);
  const p = plan(program, options.buildArgs ?? {});
  p.warnings.forEach((m) => options.onWarning?.(m));
  const policy: FetchPolicy = { ...DEFAULT_FETCH_POLICY, ...options.fetchPolicy };
  const lockPath = options.lockfile === false ? undefined : (options.lockfile ?? path.join(path.dirname(sitefilePath), 'Sitefile.lock'));
  const lock = lockPath ? await Lockfile.load(lockPath) : undefined;
  const inputs: OutdatedEntry[] = [];

  for (const spec of p.imports) {
    if (spec.location.kind !== 'remote') continue;
    const url = spec.location.url;
    const locked = lock?.get(url);
    const pin = spec.integrity ?? locked?.integrity ?? null;
    const pinnedBy: OutdatedEntry['pinnedBy'] = spec.integrity ? 'integrity' : locked ? 'lockfile' : null;
    const row = { kind: 'import' as const, name: spec.name, source: url, pin, pinnedBy, newerTag: null };
    try {
      const { bytes } = await fetchRemote(url, policy);
      const latest = sha256(bytes);
      inputs.push(entry({ ...row, latest, status: statusOf(pin, latest), error: null }));
    } catch (e) {
      inputs.push(entry({ ...row, latest: null, status: 'error', error: errorOf(e) }));
    }
  }

  const baseInput = options.baseImage ?? (p.exportFormat === 'oci' ? p.exportBase : undefined);
  if (baseInput !== undefined) {
    let source = baseInput;
    let pin: string | null = null;
    let pinnedBy: OutdatedEntry['pinnedBy'] = null;
    try {
      const ref = withDefaultRegistry(parseRef(baseInput));
      const registry = ref.registry as string;
      source = `${registry}/${ref.name}${ref.tag !== undefined ? `:${ref.tag}` : ''}${ref.digest !== undefined ? `@${ref.digest}` : ''}`;
      const locked = lock?.getImage(`${registry}/${ref.name}:${ref.tag ?? 'latest'}`);
      pin = ref.digest ?? locked?.digest ?? null;
      pinnedBy = ref.digest !== undefined ? 'digest' : pin !== null ? 'lockfile' : null;
      if (ref.tag === undefined && ref.digest !== undefined) {
        inputs.push(entry({ kind: 'base', name: 'BASE', source, pin, pinnedBy, latest: null, status: 'digest-only', newerTag: null, error: null }));
      } else {
        const tag = ref.tag ?? 'latest';
        const client = makeClient(registry, { ...options.registry, allowPrivateNetwork: options.registry?.allowPrivateNetwork ?? policy.allowPrivateNetwork });
        const latest = (await client.getManifest(ref.name, tag)).digest;
        let newerTag: string | null = null;
        if (versionTag(tag)) {
          try {
            newerTag = newerVersionTag(tag, await client.listTags(ref.name)) ?? null;
          } catch (e) {
            errorOf(e); // not listable: no newer tag
          }
        }
        inputs.push(entry({ kind: 'base', name: 'BASE', source, pin, pinnedBy, latest, status: statusOf(pin, latest), newerTag, error: null }));
      }
    } catch (e) {
      inputs.push(entry({ kind: 'base', name: 'BASE', source, pin, pinnedBy, latest: null, status: 'error', newerTag: null, error: errorOf(e) }));
    }
  }

  const summary: Record<OutdatedStatus, number> = { current: 0, changed: 0, unpinned: 0, 'digest-only': 0, error: 0 };
  for (const i of inputs) summary[i.status]++;
  return { inputs, summary };
}
