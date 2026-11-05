import type { Tree } from '../tree.js';

export interface Provenance {
  /** The URI as written (after variable interpolation), or `./relative/path`. */
  readonly uri: string;
  /** Final URL after redirects, for remote resources. */
  readonly resolvedUrl?: string;
  /** `sha256:<hex>`: file bytes, tree digest, or the raw bytes a value was parsed from. */
  readonly digest: string;
}

export interface FileResource {
  readonly kind: 'file';
  readonly bytes: Uint8Array;
  readonly mode: number;
  /** Suggested basename when copied to a directory destination. */
  readonly basename: string;
  readonly provenance: Provenance;
}

export interface TreeResource {
  readonly kind: 'tree';
  readonly tree: Tree;
  readonly provenance: Provenance;
}

export interface ValueResource {
  readonly kind: 'value';
  readonly format: 'text' | 'json';
  readonly value: unknown;
  readonly provenance: Provenance;
}

export type Resource = FileResource | TreeResource | ValueResource;

/** Network policy for remote resources. Defaults are secure. */
export interface FetchPolicy {
  /** Permit plain `http:` (default false). */
  readonly allowHttp: boolean;
  /** Permit loopback, private, link-local and other non-public addresses (default false). */
  readonly allowPrivateNetwork: boolean;
  /** Maximum response body size in bytes (default 50 MiB). */
  readonly maxBytes: number;
  /** Total time budget per resource including redirects, ms (default 30 000). */
  readonly timeoutMs: number;
  /** Maximum redirects followed (default 5). */
  readonly maxRedirects: number;
  /**
   * Extra PEM-encoded CA certificate(s) to trust for https, in addition to
   * Node's built-in roots (never instead of them). For tests and enterprise
   * private CAs; certificate and hostname verification stay on.
   */
  readonly ca?: string | Buffer | undefined;
}

export const DEFAULT_FETCH_POLICY: FetchPolicy = {
  allowHttp: false,
  allowPrivateNetwork: false,
  maxBytes: 50 * 1024 * 1024,
  timeoutMs: 30_000,
  maxRedirects: 5,
};
