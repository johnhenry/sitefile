import { SitefileError } from './errors.js';

/**
 * Image reference grammar (shaped like Docker's so a registry can be added later):
 *
 *   [registry-host[:port]/]name[:tag][@sha256:<64 hex>]
 *
 * name   = component ('/' component)*, component = [a-z0-9]+([._-][a-z0-9]+)*
 * tag    = [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}   (default `latest`)
 * The first path component is a registry host when it contains '.' or ':' or is
 * `localhost` and something follows it. Only local references are usable today.
 */
export interface ImageRef {
  readonly registry: string | undefined;
  readonly name: string;
  /** The explicit tag, if any. */
  readonly tag: string | undefined;
  /** `sha256:<hex>` when the reference pins an image ID. */
  readonly digest: string | undefined;
}

const COMPONENT = /^[a-z0-9]+(?:[._-][a-z0-9]+)*$/;
const TAG = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/;
const HOST = /^(?:[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?|\[[0-9A-Fa-f:]+\])(?::[0-9]{1,5})?$/;
const DIGEST = /^sha256:[0-9a-f]{64}$/;
export const DEFAULT_TAG = 'latest';

function bad(ref: string, why: string): SitefileError {
  return new SitefileError('E_USAGE', `invalid image reference '${ref}': ${why}`);
}

export function parseRef(input: string): ImageRef {
  if (input === '') throw bad(input, 'empty reference');
  let rest = input;
  let digest: string | undefined;
  const at = rest.indexOf('@');
  if (at !== -1) {
    digest = rest.slice(at + 1);
    rest = rest.slice(0, at);
    if (!DIGEST.test(digest)) throw bad(input, "the digest must be 'sha256:' followed by 64 lowercase hex digits");
  }
  let tag: string | undefined;
  const lastSlash = rest.lastIndexOf('/');
  const colon = rest.indexOf(':', lastSlash + 1);
  if (colon !== -1) {
    tag = rest.slice(colon + 1);
    rest = rest.slice(0, colon);
    if (!TAG.test(tag)) throw bad(input, `invalid tag '${tag}' (letters, digits, '_', '.', '-'; at most 128 characters)`);
  }
  const parts = rest.split('/');
  let registry: string | undefined;
  const first = parts[0] ?? '';
  if (parts.length > 1 && (first.includes('.') || first.includes(':') || first === 'localhost')) {
    if (!HOST.test(first)) throw bad(input, `invalid registry host '${first}'`);
    registry = first;
    parts.shift();
  }
  const name = parts.join('/');
  if (name === '' || name.length > 255) throw bad(input, 'the name must be 1-255 characters');
  for (const c of parts) {
    if (!COMPONENT.test(c)) {
      throw bad(input, `invalid name component '${c}' (lowercase letters and digits separated by '.', '_' or '-')`);
    }
  }
  return { registry, name, tag, digest };
}

/** A reference that can name a local image: no registry host. */
export function parseLocalRef(input: string): ImageRef {
  const ref = parseRef(input);
  if (ref.registry !== undefined) {
    throw new SitefileError('E_UNSUPPORTED', `'${input}': registries are not supported yet (registry host '${ref.registry}'); use a local name`);
  }
  return ref;
}

/** The refs.json key of a local reference: `name:tag`. */
export function refKey(ref: ImageRef): string {
  return `${ref.name}:${ref.tag ?? DEFAULT_TAG}`;
}

/** Hex of at least 12 digits, optionally `sha256:`-prefixed: an image ID (prefix). */
export function looksLikeImageId(s: string): boolean {
  return /^(?:sha256:)?[0-9a-f]{12,64}$/.test(s);
}
