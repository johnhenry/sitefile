import { SitefileError } from './errors.js';

/**
 * Image reference grammar (shaped like Docker's so a registry can be added later):
 *
 *   [registry-host[:port]/]name[:tag][@sha256:<64 hex>]
 *
 * name   = component ('/' component)*, component = [a-z0-9]+([._-][a-z0-9]+)*
 * tag    = [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}   (default `latest`)
 * The first path component is a registry host when it contains '.' or ':' or is
 * `localhost` and something follows it.
 *
 * A reference with a registry host names an image on that registry; locally it is just a tag
 * (`sitefile tag blog:1 localhost:5000/blog:1`), so the store keys it as `registry/name:tag`.
 * Docker Hub aliases (`docker.io`, `index.docker.io`, `registry-1.docker.io`) are one registry,
 * and a Hub name without a namespace lives in `library/`. A reference with NO registry host is
 * local everywhere except `pull` and `BASE`, which default it to Docker Hub (`nginx` is
 * `docker.io/library/nginx`); see {@link withDefaultRegistry}.
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

const DOCKER_HUB_ALIASES = new Set(['docker.io', 'index.docker.io', 'registry-1.docker.io']);
export const DOCKER_HUB = 'docker.io';

/** Canonical form of a reference with a registry host (Docker Hub aliases and `library/` applied). */
export function canonicalRef(ref: ImageRef): ImageRef {
  if (ref.registry === undefined || !DOCKER_HUB_ALIASES.has(ref.registry.toLowerCase())) return ref;
  return { ...ref, registry: DOCKER_HUB, name: ref.name.includes('/') ? ref.name : `library/${ref.name}` };
}

/** `pull` / `BASE` semantics: a reference without a registry host means Docker Hub. */
export function withDefaultRegistry(ref: ImageRef): ImageRef {
  return canonicalRef(ref.registry === undefined ? { ...ref, registry: DOCKER_HUB } : ref);
}

/** The host to connect to for a (canonical) registry name. */
export function registryApiHost(registry: string): string {
  return registry === DOCKER_HUB ? 'registry-1.docker.io' : registry;
}

/** Parse a reference that may be used as a local name or tag (a registry host is allowed). */
export function parseLocalRef(input: string): ImageRef {
  return canonicalRef(parseRef(input));
}

/** The refs.json key of a local reference: `[registry/]name:tag`. */
export function refKey(ref: ImageRef): string {
  const c = canonicalRef(ref);
  return `${c.registry === undefined ? '' : `${c.registry}/`}${c.name}:${c.tag ?? DEFAULT_TAG}`;
}

/** Hex of at least 12 digits, optionally `sha256:`-prefixed: an image ID (prefix). */
export function looksLikeImageId(s: string): boolean {
  return /^(?:sha256:)?[0-9a-f]{12,64}$/.test(s);
}
