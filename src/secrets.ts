import { readFile } from 'node:fs/promises';
import { SitefileError, isSitefileError } from './errors.js';

/**
 * Build secrets (issue #85), modelled on Docker's `--secret id=…,env=…|src=…`: values that remote
 * IMPORTs send as HTTP request headers (`IMPORT … SECRET <id> AS HEADER <Name>` / `AS BEARER`).
 *
 * A secret value lives only in memory: it is sent to the IMPORT URL's own origin (never after a
 * redirect to another one) and never becomes part of the output, a cache or memo key, Sitefile.lock,
 * a vendor manifest, provenance, a --profile trace, a warning or an error message. As a last line of
 * defence, warnings and errors of a build that has secrets are passed through {@link redactSecrets}.
 */

const ID_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** Visible ASCII, space and tab: what every HTTP client sends unchanged in a header value. */
const HEADER_VALUE_RE = /^[\t\x20-\x7e]+$/;

export type Secrets = ReadonlyMap<string, string>;

function usage(message: string): SitefileError {
  return new SitefileError('E_USAGE', message);
}

/**
 * Parse `--secret` flags: `id=<id>,env=<VAR>` (an environment variable) or `id=<id>,src=<file>` (a
 * file, relative to the working directory; one trailing newline is dropped). Values are checked to
 * be usable as an HTTP header value. Messages never include a value.
 */
export async function loadSecrets(flags: readonly string[], env: NodeJS.ProcessEnv = process.env): Promise<Map<string, string>> {
  const out = new Map<string, string>();
  for (const flag of flags) {
    const fields = new Map<string, string>();
    for (const part of flag.split(',')) {
      const eq = part.indexOf('=');
      const key = eq < 0 ? part : part.slice(0, eq);
      if (eq < 0 || (key !== 'id' && key !== 'env' && key !== 'src')) {
        throw usage('--secret expects id=<id>,env=<VAR> or id=<id>,src=<file>'); // never echo the flag: it might be the secret itself
      }
      if (fields.has(key)) throw usage(`--secret: '${key}' given twice`);
      fields.set(key, part.slice(eq + 1));
    }
    const id = fields.get('id');
    if (id === undefined || !ID_RE.test(id)) throw usage(`--secret needs id=<id> (letters, digits and _, not starting with a digit)`);
    if (out.has(id)) throw usage(`--secret id=${id} given twice`);
    const name = fields.get('env');
    const src = fields.get('src');
    if ((name === undefined) === (src === undefined)) throw usage(`--secret id=${id}: give exactly one of env=<VAR> or src=<file>`);
    let value: string;
    if (name !== undefined) {
      const v = env[name];
      if (v === undefined) throw new SitefileError('E_SECRET', `--secret id=${id}: environment variable ${name} is not set`);
      value = v;
    } else {
      let bytes: Buffer;
      try {
        bytes = await readFile(src!);
      } catch (e) {
        throw new SitefileError('E_SECRET', `--secret id=${id}: cannot read '${src}'`, undefined, { cause: isSitefileError(e) ? e : undefined });
      }
      value = bytes.toString('latin1').replace(/\r?\n$/, '');
    }
    if (value === '') throw new SitefileError('E_SECRET', `--secret id=${id} is empty`);
    if (!HEADER_VALUE_RE.test(value)) {
      throw new SitefileError('E_SECRET', `--secret id=${id} cannot be sent in an HTTP header (only printable ASCII, spaces and tabs)`);
    }
    out.set(id, value);
  }
  return out;
}

/** Replace every occurrence of a secret value (longest first) with `***`. */
export function redactSecrets(text: string, secrets: Secrets | undefined): string {
  if (!secrets || secrets.size === 0) return text;
  let out = text;
  for (const v of [...secrets.values()].sort((a, b) => b.length - a.length)) if (v.length > 0) out = out.split(v).join('***');
  return out;
}

/** A SitefileError whose message has the secrets redacted (the same error when nothing changes). */
export function redactError(e: unknown, secrets: Secrets | undefined): unknown {
  if (!isSitefileError(e) || !secrets || secrets.size === 0) return e;
  const detail = redactSecrets(e.detail, secrets);
  return detail === e.detail ? e : new SitefileError(e.code, detail, e.span);
}

/** The request headers of an import's SECRET clauses; a secret that was not given is E_SECRET. */
export function secretHeaders(
  spec: { readonly name: string; readonly secrets?: readonly { readonly id: string; readonly header: string; readonly bearer: boolean }[]; readonly span: import('./errors.js').SourceSpan },
  secrets: Secrets | undefined,
): Record<string, string> | undefined {
  if (!spec.secrets?.length) return undefined;
  const headers: Record<string, string> = {};
  for (const s of spec.secrets) {
    const v = secrets?.get(s.id);
    if (v === undefined) {
      throw new SitefileError('E_SECRET', `IMPORT ${spec.name}: secret '${s.id}' was not given; pass --secret id=${s.id},env=<VAR> or --secret id=${s.id},src=<file>`, spec.span);
    }
    headers[s.header.toLowerCase()] = s.bearer ? `Bearer ${v}` : v;
  }
  return headers;
}
