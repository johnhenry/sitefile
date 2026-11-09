// bcrypt (EksBlowfish) password verification for `serve --auth-file` (johnhenry/sitefile#278).
// rust/src/serve_auth.rs checks the same entries (EksBlowfish from the `bcrypt` crate, parsing and the
// constant-time comparison as here). Written in-repo rather than as an npm dependency (sitefile has
// none): node:crypto has no Blowfish with bcrypt's expensive key schedule.
import { timingSafeEqual } from 'node:crypto';

/**
 * Blowfish's initial P-array (18 words) then S-boxes (4 x 256 words): the hexadecimal digits of pi's
 * fractional part, big-endian, base64 (test/bcrypt.test.ts recomputes them from pi).
 */
export const PI_WORDS =
  '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';

const ALPHABET = './ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';

/** bcrypt's base64 (its own alphabet, no padding) of `bytes`. */
function encode(bytes: Uint8Array): string {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const n = (bytes[i]! << 16) | ((bytes[i + 1] ?? 0) << 8) | (bytes[i + 2] ?? 0);
    const chars = Math.min(4, Math.ceil(((bytes.length - i) * 8) / 6));
    for (let k = 0; k < chars; k++) out += ALPHABET[(n >>> (18 - 6 * k)) & 63];
  }
  return out;
}

/** `n` bytes from bcrypt base64 `s`, or undefined unless `s` is exactly their canonical encoding. */
function decode(s: string, n: number): Uint8Array | undefined {
  const out = new Uint8Array(n);
  let acc = 0;
  let bits = 0;
  let j = 0;
  for (const c of s) {
    const v = ALPHABET.indexOf(c);
    if (v < 0) return undefined;
    acc = ((acc << 6) | v) & 0xffffff;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      if (j < n) out[j++] = (acc >>> bits) & 0xff;
    }
  }
  return j === n && encode(out) === s ? out : undefined;
}

/** A bcrypt hash: `$2a$`, `$2b$` or `$2y$`, a cost of 04 to 31, a 22-character salt and a 31-character hash. */
const BCRYPT_HASH = /^\$2[aby]\$(0[4-9]|[12][0-9]|3[01])\$[./A-Za-z0-9]{53}$/;

/** Whether `hash` looks like a bcrypt entry at all (`$2a$`, `$2b$`, `$2y$`). */
export const isBcryptPrefix = (hash: string): boolean => /^\$2[aby]\$/.test(hash);

/** The parts of a well-formed bcrypt hash (cost 4 to 31, canonical salt and hash encodings), else undefined. */
export function parseBcrypt(hash: string): { cost: number; salt: Uint8Array; digest: Uint8Array } | undefined {
  if (!BCRYPT_HASH.test(hash)) return undefined;
  const salt = decode(hash.slice(7, 29), 16);
  const digest = decode(hash.slice(29), 23);
  return salt && digest ? { cost: Number(hash.slice(4, 6)), salt, digest } : undefined;
}

/**
 * The key bcrypt hashes for `password`: its UTF-8 bytes up to the first NUL (C strings, as htpasswd
 * reads them), then a NUL, at most 72 bytes in all. `$2a$`, `$2b$` and `$2y$` are the same here, as in
 * OpenBSD's and Openwall's current code.
 */
export function bcryptKey(password: string): Uint8Array {
  const pw = Buffer.from(password, 'utf8');
  const nul = pw.indexOf(0);
  const bytes = nul < 0 ? pw : pw.subarray(0, nul);
  const key = new Uint8Array(Math.min(bytes.length + 1, 72));
  key.set(bytes.subarray(0, key.length));
  return key;
}

/** EksBlowfish: the 24 bytes bcrypt computes for `key` (the encoded hash keeps the first 23). */
export function bcryptRaw(cost: number, salt: Uint8Array, key: Uint8Array): Uint8Array {
  const init = Buffer.from(PI_WORDS, 'base64');
  const P = new Uint32Array(18);
  const S = new Uint32Array(1024);
  for (let i = 0; i < 18; i++) P[i] = init.readUInt32BE(4 * i);
  for (let i = 0; i < 1024; i++) S[i] = init.readUInt32BE(72 + 4 * i);
  let l = 0;
  let r = 0;
  const f = (x: number): number => (((S[x >>> 24]! + S[256 | ((x >>> 16) & 255)]!) ^ S[512 | ((x >>> 8) & 255)]!) + S[768 | (x & 255)]!) | 0;
  const encipher = (): void => {
    let xl = l ^ P[0]!;
    let xr = r;
    for (let i = 1; i <= 16; i += 2) {
      xr ^= f(xl) ^ P[i]!;
      xl ^= f(xr) ^ P[i + 1]!;
    }
    l = (xr ^ P[17]!) >>> 0;
    r = xl >>> 0;
  };
  // The next big-endian word of `data` at `at.i`, cycling through it.
  const word = (data: Uint8Array, at: { i: number }): number => {
    let w = 0;
    for (let k = 0; k < 4; k++) {
      w = ((w << 8) | data[at.i]!) >>> 0;
      at.i = (at.i + 1) % data.length;
    }
    return w;
  };
  const expand = (data: Uint8Array, saltStream: Uint8Array | undefined): void => {
    const at = { i: 0 };
    for (let i = 0; i < 18; i++) P[i] = P[i]! ^ word(data, at);
    const sat = { i: 0 };
    l = 0;
    r = 0;
    for (const t of [P, S]) {
      for (let i = 0; i < t.length; i += 2) {
        if (saltStream) {
          l = (l ^ word(saltStream, sat)) >>> 0;
          r = (r ^ word(saltStream, sat)) >>> 0;
        }
        encipher();
        t[i] = l;
        t[i + 1] = r;
      }
    }
  };
  expand(key, salt);
  for (let n = 2 ** cost; n > 0; n--) {
    expand(key, undefined);
    expand(salt, undefined);
  }
  const out = Buffer.from('OrpheanBeholderScryDoubt', 'latin1');
  for (let b = 0; b < 24; b += 8) {
    l = out.readUInt32BE(b);
    r = out.readUInt32BE(b + 4);
    for (let i = 0; i < 64; i++) encipher();
    out.writeUInt32BE(l, b);
    out.writeUInt32BE(r, b + 4);
  }
  return out;
}

/** The bcrypt hash string of `password` with this cost and salt (tests). */
export function bcryptHash(password: string, cost: number, salt: Uint8Array, prefix = '$2b$'): string {
  const raw = bcryptRaw(cost, salt, bcryptKey(password));
  return `${prefix}${String(cost).padStart(2, '0')}$${encode(salt)}${encode(raw.subarray(0, 23))}`;
}

/** Whether `password` matches bcrypt `hash` (false for a malformed one). The digests are compared in constant time. */
export function verifyBcrypt(hash: string, password: string): boolean {
  const p = parseBcrypt(hash);
  if (!p) return false;
  return timingSafeEqual(bcryptRaw(p.cost, p.salt, bcryptKey(password)).subarray(0, 23), p.digest);
}
