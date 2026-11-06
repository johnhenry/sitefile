import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { sha256 } from './digest.js';
import { SitefileError } from './errors.js';
import type { BuildResult, Pins } from './executor.js';
import { canonicalJson, MEDIA } from './oci.js';
import type { Tree } from './tree.js';

/**
 * Build attestations for OCI site images (`build --provenance` / `--sbom`).
 *
 * Each attestation is an in-toto Statement v1 (one JSON blob) wrapped in an OCI 1.1 artifact
 * manifest whose `subject` is the image manifest:
 *
 *   manifest  artifactType application/vnd.in-toto+json, config = the empty descriptor ({}),
 *             one layer application/vnd.in-toto+json annotated in-toto.io/predicate-type,
 *             subject = the image manifest descriptor
 *
 * - provenance: predicate https://slsa.dev/provenance/v1 (the Sitefile digest, build args except
 *   secret-looking names, every import with its digest, the BASE image digest, the builder version)
 * - sbom: predicate https://cyclonedx.org/bom, a CycloneDX 1.6 document listing every file in the
 *   site layer (path in the image, SHA-256, mode, origin) and the BASE image
 *
 * The image manifest is never touched: an attestation points at it, so the image ID is the same with
 * or without attestations. Everything is canonical JSON with no timestamps unless SOURCE_DATE_EPOCH
 * is set, so the same build gives byte-identical attestations (in both implementations).
 */

/** Sitefile version recorded as the builder version (also `sitefile --version`). */
export const SITEFILE_VERSION = '0.1.0';

export const ATTEST = {
  /** artifactType of the attestation manifest and media type of its layer. */
  inToto: 'application/vnd.in-toto+json',
  emptyConfig: 'application/vnd.oci.empty.v1+json',
  predicateAnnotation: 'in-toto.io/predicate-type',
  statement: 'https://in-toto.io/Statement/v1',
  slsa: 'https://slsa.dev/provenance/v1',
  cyclonedx: 'https://cyclonedx.org/bom',
  buildType: 'https://github.com/johnhenry/sitefile/blob/main/docs/CLI.md#attestations',
  builderId: 'https://github.com/johnhenry/sitefile',
} as const;

/** The OCI empty descriptor's blob (`{}`). */
export const EMPTY_JSON = new TextEncoder().encode('{}');
export const EMPTY_DIGEST = sha256(EMPTY_JSON);

export interface AttestRequest {
  readonly provenance: boolean;
  readonly sbom: boolean;
}

/** One attestation (or any referrer): its index descriptor and the blobs it needs. */
export interface Referrer {
  /** Image-index descriptor: mediaType, digest, size, artifactType, annotations. */
  readonly descriptor: ReferrerDescriptor;
  /** digest -> bytes: the manifest, its config and its layers. */
  readonly blobs: ReadonlyArray<readonly [string, Uint8Array]>;
}

export interface ReferrerDescriptor {
  readonly mediaType: string;
  readonly digest: string;
  readonly size: number;
  readonly artifactType?: string | undefined;
  readonly annotations?: Readonly<Record<string, string>> | undefined;
}

/** What the provenance and SBOM say about a build (deterministic: no absolute paths, no times). */
export interface BuildFacts {
  /** The Sitefile's path relative to the context (its basename when it lives elsewhere). */
  readonly sitefile: string;
  readonly sitefileDigest: string;
  /** `--build-arg` / `--build-arg-file` values, except those whose names look secret. */
  readonly args: Readonly<Record<string, string>>;
  /** Names of the build args left out because they look secret (API_TOKEN, DB_PASSWORD, ...). */
  readonly redactedArgs: readonly string[];
  readonly platform: string;
  /** Imports in declaration order with the digest the build used (Sitefile.lock's for remote ones). */
  readonly imports: ReadonlyArray<{ readonly name: string; readonly uri: string; readonly resolved?: string | undefined; readonly digest: string }>;
  readonly base?: { readonly name: string; readonly digest: string } | undefined;
  /** `FROM image` stage bases, in stage order (FROM line, image name, image digest). */
  readonly images?: ReadonlyArray<{ readonly line: number; readonly name: string; readonly digest: string }> | undefined;
  /** `--strict-pins`: how each remote input was pinned (shown in the provenance). */
  readonly pins?: Pins | undefined;
  /** File origins start with the Sitefile label (relative to the cwd); they are rewritten to `sitefile`. */
  readonly label: string;
}

/** The image an attestation is about. */
export interface AttestSubject {
  readonly digest: string;
  readonly size: number;
  /** The site's files (rooted at `/`) and where they sit in the image. */
  readonly files: Tree;
  readonly root: string;
  /** org.opencontainers.image.title, if the image has one. */
  readonly title?: string | undefined;
}

const SECRET_WORDS = new Set(['apikey', 'auth', 'credential', 'credentials', 'key', 'passwd', 'password', 'pword', 'secret', 'token']);

/** Whether a build-arg name looks like it holds a secret (any `_`-separated word is key, token, password, ...). */
export function isSecretArgName(name: string): boolean {
  return name
    .toLowerCase()
    .split('_')
    .some((w) => SECRET_WORDS.has(w));
}

/** The Sitefile path the build used, as `build` resolves it. */
export function sitefilePathOf(contextDir: string, sitefile: string | undefined): string {
  return path.resolve(sitefile ?? path.join(contextDir, 'Sitefile'));
}

/** Collect the facts for a finished build. Reads the Sitefile again to digest it. */
export async function buildFacts(
  result: Pick<BuildResult, 'resources' | 'base' | 'pins' | 'images'>,
  options: { readonly contextDir: string; readonly sitefile?: string | undefined; readonly buildArgs?: Readonly<Record<string, string>> | undefined; readonly platform?: string | undefined },
): Promise<BuildFacts> {
  const file = sitefilePathOf(options.contextDir, options.sitefile);
  let bytes: Uint8Array;
  try {
    bytes = new Uint8Array(await readFile(file));
  } catch (e) {
    throw new SitefileError('E_NOT_FOUND', `cannot read Sitefile '${file}'`, undefined, { cause: e });
  }
  const rel = path.relative(path.resolve(options.contextDir), file);
  const inside = rel !== '' && !rel.startsWith('..') && !path.isAbsolute(rel);
  const sitefile = inside ? rel.split(path.sep).join('/') : path.basename(file);
  const args: Record<string, string> = {};
  const redactedArgs: string[] = [];
  for (const [k, v] of Object.entries(options.buildArgs ?? {})) {
    if (isSecretArgName(k)) redactedArgs.push(k);
    else args[k] = v;
  }
  redactedArgs.sort();
  const imports = [...result.resources].map(([name, r]) => ({ name, uri: r.provenance.uri, resolved: r.provenance.resolvedUrl, digest: r.provenance.digest }));
  return {
    sitefile,
    sitefileDigest: sha256(bytes),
    args,
    redactedArgs,
    platform: options.platform ?? 'linux/amd64',
    imports,
    base: result.base ? { name: result.base.name, digest: result.base.manifestDigest } : undefined,
    images: result.images?.map((i) => ({ line: i.line, name: i.name, digest: i.digest })),
    pins: result.pins,
    label: path.relative(process.cwd(), file) || path.basename(file),
  };
}

const hex = (digest: string): string => digest.replace(/^sha256:/, '');

function isoOf(epoch: number): string {
  return new Date(epoch * 1000).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

/** SLSA provenance v1 predicate. */
export function provenancePredicate(facts: BuildFacts, epoch: number | undefined): Record<string, unknown> {
  const externalParameters: Record<string, unknown> = { sitefile: facts.sitefile, args: { ...facts.args }, platform: facts.platform };
  if (facts.redactedArgs.length) externalParameters.redactedArgs = [...facts.redactedArgs];
  // --strict-pins: the build refused unpinned inputs; each remote dependency says what pinned it.
  if (facts.pins) externalParameters.strictPins = true;
  const deps: Array<Record<string, unknown>> = [{ name: 'sitefile', uri: facts.sitefile, digest: { sha256: hex(facts.sitefileDigest) } }];
  for (const i of facts.imports) {
    const d: Record<string, unknown> = { name: `import:${i.name}`, uri: i.uri, digest: { sha256: hex(i.digest) } };
    if (i.resolved !== undefined && i.resolved !== i.uri) d.downloadLocation = i.resolved;
    const pin = facts.pins?.imports[i.name];
    if (pin !== undefined) d.annotations = { 'dev.sitefile.pin': pin };
    deps.push(d);
  }
  for (const i of facts.images ?? []) {
    const pin = facts.pins?.images?.[i.line];
    deps.push({ name: `from:${i.line}`, uri: i.name, digest: { sha256: hex(i.digest) }, ...(pin ? { annotations: { 'dev.sitefile.pin': pin } } : {}) });
  }
  if (facts.base) {
    deps.push({ name: 'base', uri: facts.base.name, digest: { sha256: hex(facts.base.digest) }, ...(facts.pins?.base ? { annotations: { 'dev.sitefile.pin': facts.pins.base } } : {}) });
  }
  const runDetails: Record<string, unknown> = { builder: { id: ATTEST.builderId, version: { sitefile: SITEFILE_VERSION } } };
  if (epoch !== undefined) runDetails.metadata = { startedOn: isoOf(epoch), finishedOn: isoOf(epoch) };
  return { buildDefinition: { buildType: ATTEST.buildType, externalParameters, resolvedDependencies: deps }, runDetails };
}

/** CycloneDX 1.6 SBOM: the image, its BASE and every file of the site layer. */
export function sbomPredicate(subject: AttestSubject, facts: BuildFacts, epoch: number | undefined): Record<string, unknown> {
  const metadata: Record<string, unknown> = {
    tools: { components: [{ type: 'application', name: 'sitefile', version: SITEFILE_VERSION }] },
    component: { type: 'container', 'bom-ref': 'image', name: subject.title ?? 'site', version: subject.digest },
  };
  if (epoch !== undefined) metadata.timestamp = isoOf(epoch);
  const components: Array<Record<string, unknown>> = [];
  if (facts.base) components.push({ type: 'container', 'bom-ref': 'base', name: facts.base.name, version: facts.base.digest });
  const prefix = `${facts.label}:`;
  for (const [p, n] of subject.files.entries()) {
    const name = subject.root === '/' ? p : subject.root + p;
    const origin = n.origin.startsWith(prefix) ? `${facts.sitefile}:${n.origin.slice(prefix.length)}` : n.origin;
    components.push({
      type: 'file',
      'bom-ref': `file:${name}`,
      name,
      hashes: [{ alg: 'SHA-256', content: hex(n.digest) }],
      properties: [
        { name: 'sitefile:mode', value: n.mode.toString(8) },
        { name: 'sitefile:origin', value: origin },
      ],
    });
  }
  return { bomFormat: 'CycloneDX', specVersion: '1.6', version: 1, metadata, components };
}

const enc = new TextEncoder();

/** Wrap a predicate in an in-toto statement about `subject` and an OCI artifact manifest. */
export function attestationReferrer(subject: Pick<AttestSubject, 'digest' | 'size'>, predicateType: string, predicate: unknown): Referrer {
  const statement = enc.encode(
    canonicalJson({ _type: ATTEST.statement, subject: [{ name: '_', digest: { sha256: hex(subject.digest) } }], predicateType, predicate }),
  );
  const layerDigest = sha256(statement);
  const annotations = { [ATTEST.predicateAnnotation]: predicateType };
  const manifest = enc.encode(
    canonicalJson({
      schemaVersion: 2,
      mediaType: MEDIA.manifest,
      artifactType: ATTEST.inToto,
      config: { mediaType: ATTEST.emptyConfig, digest: EMPTY_DIGEST, size: EMPTY_JSON.byteLength },
      layers: [{ mediaType: ATTEST.inToto, digest: layerDigest, size: statement.byteLength, annotations }],
      subject: { mediaType: MEDIA.manifest, digest: subject.digest, size: subject.size },
      annotations,
    }),
  );
  const digest = sha256(manifest);
  return {
    descriptor: { mediaType: MEDIA.manifest, digest, size: manifest.byteLength, artifactType: ATTEST.inToto, annotations },
    blobs: [
      [digest, manifest],
      [EMPTY_DIGEST, EMPTY_JSON],
      [layerDigest, statement],
    ],
  };
}

/** The requested attestations for an image, provenance first. */
export function makeAttestations(subject: AttestSubject, facts: BuildFacts, request: AttestRequest, epoch: number | undefined): Referrer[] {
  const out: Referrer[] = [];
  if (request.provenance) out.push(attestationReferrer(subject, ATTEST.slsa, provenancePredicate(facts, epoch)));
  if (request.sbom) out.push(attestationReferrer(subject, ATTEST.cyclonedx, sbomPredicate(subject, facts, epoch)));
  return out;
}

/** An OCI image index listing referrers (the store's referrers file and the registry fallback tag). */
export function encodeReferrersIndex(descriptors: readonly ReferrerDescriptor[]): Uint8Array {
  return enc.encode(canonicalJson({ schemaVersion: 2, mediaType: MEDIA.index, manifests: descriptors }));
}

/** Parse a referrers index; entries that are not well-formed descriptors are dropped. */
export function decodeReferrersIndex(bytes: Uint8Array): ReferrerDescriptor[] {
  const j = JSON.parse(Buffer.from(bytes).toString('utf8')) as { schemaVersion?: unknown; manifests?: unknown };
  if (j.schemaVersion !== 2 || !Array.isArray(j.manifests)) throw new Error('not an OCI image index');
  const out: ReferrerDescriptor[] = [];
  for (const m of j.manifests as Array<Record<string, unknown>>) {
    if (typeof m?.digest !== 'string' || !/^sha256:[0-9a-f]{64}$/.test(m.digest) || typeof m.size !== 'number' || !Number.isSafeInteger(m.size) || m.size < 0) continue;
    let annotations: Record<string, string> | undefined;
    if (typeof m.annotations === 'object' && m.annotations !== null && !Array.isArray(m.annotations)) {
      annotations = {};
      for (const [k, v] of Object.entries(m.annotations as Record<string, unknown>)) if (typeof v === 'string') annotations[k] = v;
    }
    out.push({
      mediaType: typeof m.mediaType === 'string' ? m.mediaType : MEDIA.manifest,
      digest: m.digest,
      size: m.size,
      artifactType: typeof m.artifactType === 'string' ? m.artifactType : undefined,
      annotations,
    });
  }
  return out;
}

/** The referrers fallback tag for a subject digest (`sha256-<hex>`, distribution spec v1.1). */
export function referrersTag(subject: string): string {
  return subject.replace(':', '-');
}
