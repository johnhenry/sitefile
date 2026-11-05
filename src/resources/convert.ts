import { SitefileError, type SourceSpan } from '../errors.js';
import { sha256 } from '../digest.js';
import { extractArchive } from './archive.js';
import { markdownDocument } from './documents.js';
import { bundleValue } from './bundle-data.js';
import { readWebBundle } from './wbn-reader.js';
import { parseYaml } from '../yaml.js';
import { parseCsvRecords } from '../csv.js';
import { DEFAULT_ARCHIVE_LIMITS, type ArchiveLimits, type Provenance, type Resource } from './types.js';

export interface BytesInfo {
  readonly uri: string;
  readonly resolvedUrl?: string;
  readonly basename: string;
  readonly mode?: number;
}

function decodeUtf8(bytes: Uint8Array, uri: string, span?: SourceSpan): string {
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch (e) {
    throw new SitefileError('E_ENCODING', `${uri} is not valid UTF-8`, span, { cause: e });
  }
}

/** Turn fetched or read bytes into a resource of the requested kind. */
export async function bytesToResource(
  bytes: Uint8Array,
  kind: 'file' | 'text' | 'json' | 'tree' | 'markdown' | 'yaml' | 'csv' | 'wbn',
  info: BytesInfo,
  span?: SourceSpan,
  archiveLimits: ArchiveLimits = DEFAULT_ARCHIVE_LIMITS,
): Promise<Resource> {
  const digest = sha256(bytes);
  const provenance: Provenance =
    info.resolvedUrl === undefined ? { uri: info.uri, digest } : { uri: info.uri, resolvedUrl: info.resolvedUrl, digest };
  if (kind === 'tree') {
    // The provenance digest is that of the archive bytes, which is what pins and the lockfile cover.
    return { kind: 'tree', tree: await extractArchive(bytes, info.uri, archiveLimits, span), provenance };
  }
  if (kind === 'markdown') {
    return { kind: 'value', format: 'markdown', value: markdownDocument(bytes, '/' + info.basename, info.uri, span), provenance };
  }
  if (kind === 'wbn') {
    return { kind: 'value', format: 'wbn', value: bundleValue(readWebBundle(bytes, info.uri, archiveLimits, span)), provenance };
  }
  if (kind === 'file') {
    const mode = (info.mode ?? 0o644) & 0o111 ? 0o755 : 0o644;
    return { kind: 'file', bytes, mode, basename: info.basename, provenance };
  }
  const text = decodeUtf8(bytes, info.uri, span);
  if (kind === 'text') return { kind: 'value', format: 'text', value: text, provenance };
  if (kind === 'yaml') return { kind: 'value', format: 'yaml', value: parseYaml(text, info.uri), provenance };
  if (kind === 'csv') return { kind: 'value', format: 'csv', value: parseCsvRecords(text, info.uri), provenance };
  let value: unknown;
  try {
    // TextDecoder already strips a leading BOM.
    value = JSON.parse(text);
  } catch (e) {
    throw new SitefileError('E_TYPE', `${info.uri} is not valid JSON: ${(e as Error).message}`, span, { cause: e });
  }
  return { kind: 'value', format: 'json', value, provenance };
}

/** Throw E_INTEGRITY unless sha256(bytes) equals `expected` (`sha256:<hex>`). */
export function verifyIntegrity(bytes: Uint8Array, expected: string, uri: string, span?: SourceSpan): void {
  const actual = sha256(bytes);
  if (actual !== expected.toLowerCase()) {
    throw new SitefileError(
      'E_INTEGRITY',
      `integrity mismatch for ${uri}: expected ${expected}, got ${actual}`,
      span,
    );
  }
}
