// HTTP basic authentication for `sitefile serve --auth` / `--auth-file` (johnhenry/sitefile#177).
// Ported as rust/src/serve_auth.rs (same verifier strings, same rules).
//
// A credential is kept only as a verifier, never as the password:
//   pbkdf2-sha256$<iterations>$<salt base64>$<32-byte key base64>   (what --auth and SITEFILE_SERVE_AUTH become)
//   {SHA}<base64 SHA-1 of the password>                              (an htpasswd -s entry, --auth-file)
//   $2y$<cost>$<salt><hash> (also $2a$, $2b$)                         (an htpasswd -B bcrypt entry, --auth-file; #278)
import { createHash, pbkdf2Sync, randomBytes, timingSafeEqual } from 'node:crypto';
import { isBcryptPrefix, parseBcrypt, verifyBcrypt } from './bcrypt.js';
import { SitefileError } from './errors.js';

export interface AuthEntry {
  readonly user: string;
  readonly verifier: string;
}

/** PBKDF2 rounds of a verifier made here; checks are cached per server, so this is paid once per credential. */
export const AUTH_ITERATIONS = 20000;

/** `user:password` (`--auth`, `SITEFILE_SERVE_AUTH`): the user is non-empty and has no `:`; the password is the rest. */
export function parseAuthPair(value: string, what: string): { user: string; password: string } {
  const i = value.indexOf(':');
  if (i <= 0 || i === value.length - 1) throw new SitefileError('E_USAGE', `${what} expects user:password`);
  return { user: value.slice(0, i), password: value.slice(i + 1) };
}

/** A pbkdf2-sha256 verifier for `password` (a random salt unless one is given). */
export function makeVerifier(password: string, salt: Uint8Array = randomBytes(16), iterations = AUTH_ITERATIONS): string {
  const key = pbkdf2Sync(Buffer.from(password, 'utf8'), salt, iterations, 32, 'sha256');
  return `pbkdf2-sha256$${iterations}$${Buffer.from(salt).toString('base64')}$${key.toString('base64')}`;
}

/** Canonical base64 (standard alphabet, padded) decoded, else undefined. */
function b64(s: string): Buffer | undefined {
  const b = Buffer.from(s, 'base64');
  return b.toString('base64') === s ? b : undefined;
}

function equal(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && timingSafeEqual(a, b);
}

/** Whether `password` matches `verifier` (false for a malformed verifier). Constant time in the key comparison. */
export function verifyPassword(verifier: string, password: string): boolean {
  if (isBcryptPrefix(verifier)) return verifyBcrypt(verifier, password);
  const pw = Buffer.from(password, 'utf8');
  if (verifier.startsWith('{SHA}')) {
    const want = b64(verifier.slice(5));
    return want !== undefined && equal(createHash('sha1').update(pw).digest(), want);
  }
  const m = /^pbkdf2-sha256\$([0-9]{1,7})\$([A-Za-z0-9+/=]+)\$([A-Za-z0-9+/=]+)$/.exec(verifier);
  if (!m) return false;
  const iterations = Number(m[1]);
  const salt = b64(m[2]!);
  const want = b64(m[3]!);
  if (iterations < 1 || salt === undefined || want === undefined || want.length !== 32) return false;
  return equal(pbkdf2Sync(pw, salt, iterations, 32, 'sha256'), want);
}

/**
 * An htpasswd file: `user:hash` lines (blank lines and `#` comments skipped). `{SHA}` (htpasswd -s) and bcrypt
 * (`$2y$`, `$2a$`, `$2b$`, cost 4 to 31; htpasswd -B) entries are accepted; a malformed bcrypt hash, `$apr1$`,
 * crypt and plain-text entries are `E_USAGE` errors naming the user.
 */
export function parseHtpasswd(text: string, origin: string): AuthEntry[] {
  const out: AuthEntry[] = [];
  for (const [n, raw] of text.split('\n').entries()) {
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    if (line.trim() === '' || line.trimStart().startsWith('#')) continue;
    const i = line.indexOf(':');
    if (i <= 0) throw new SitefileError('E_USAGE', `${origin}:${n + 1}: expected user:hash`);
    const user = line.slice(0, i);
    const hash = line.slice(i + 1);
    if (isBcryptPrefix(hash)) {
      if (!parseBcrypt(hash)) throw new SitefileError('E_USAGE', `${origin}:${n + 1}: user '${user}': malformed bcrypt hash (expected $2y$, $2a$ or $2b$, a cost of 04 to 31 and 53 characters of salt and hash)`);
    } else if (!/^\{SHA\}[A-Za-z0-9+/]{27}=$/.test(hash)) {
      throw new SitefileError('E_USAGE', `${origin}:${n + 1}: user '${user}': unsupported password hash (only {SHA} and bcrypt entries, htpasswd -s or -B)`);
    }
    out.push({ user, verifier: hash });
  }
  return out;
}

/** The `user` and `password` of an `Authorization: Basic …` value, or undefined. */
export function basicCredentials(header: string | undefined): { user: string; password: string } | undefined {
  if (header === undefined) return undefined;
  const m = /^[Bb][Aa][Ss][Ii][Cc] +([A-Za-z0-9+/]+=*) *$/.exec(header.trim());
  if (!m) return undefined;
  const raw = b64(m[1]!);
  if (raw === undefined) return undefined;
  const text = raw.toString('utf8');
  const i = text.indexOf(':');
  return i < 0 ? undefined : { user: text.slice(0, i), password: text.slice(i + 1) };
}

/** Accepts `Authorization` values for a set of entries; good values are remembered (at most 64) so PBKDF2 runs once per credential. */
export class BasicAuth {
  private readonly good = new Set<string>();
  constructor(readonly entries: readonly AuthEntry[]) {}

  allows(header: string | undefined): boolean {
    if (header === undefined) return false;
    if (this.good.has(header)) return true;
    const c = basicCredentials(header);
    if (c === undefined) return false;
    let ok = false;
    for (const e of this.entries) if (e.user === c.user && verifyPassword(e.verifier, c.password)) ok = true;
    if (ok) {
      if (this.good.size >= 64) this.good.clear();
      this.good.add(header);
    }
    return ok;
  }
}
