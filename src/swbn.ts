import { createHash, createPrivateKey, createPublicKey, sign } from 'node:crypto';
import { cborArray, cborBytes, cborMap, cborText } from './cbor.js';
import { SitefileError } from './errors.js';
import type { Tree } from './tree.js';
import { writeWebBundleAt } from './wbn.js';

/**
 * `EXPORT … AS swbn KEY <secret-id>` (issue #207): a Signed Web Bundle for Isolated Web Apps.
 *
 * The key is a build secret (`--secret id=<id>,src=key.pem`): an unencrypted Ed25519 private key in
 * PEM, PKCS #8 (`openssl genpkey -algorithm ed25519`). It is never written anywhere. The Web Bundle ID
 * is base32 (RFC 4648, lower case, no padding) of the public key and the Ed25519 type suffix
 * `00 01 02`; the bundle is the tree's Web Bundle (b2) at `isolated-app://<id>/`. In front of it goes
 * the integrity block (v2, deterministic CBOR):
 *
 *   [ h'F09F968BF09F93A6', h'32620000', {"webBundleId": id}, [[{"ed25519PublicKey": pk}, signature]] ]
 *
 * The signature is Ed25519 over the length-prefixed (8-byte big-endian) concatenation of the SHA-512
 * of the bundle, the integrity block with an empty signature stack, and the signature attributes, as
 * the WICG integrity-signature explainer and `wbn-sign` define it. Ed25519 is deterministic, so the
 * same tree and key give the same bytes.
 */

const MAGIC = new Uint8Array([0xf0, 0x9f, 0x96, 0x8b, 0xf0, 0x9f, 0x93, 0xa6]);
const VERSION_B2 = new Uint8Array([0x32, 0x62, 0x00, 0x00]);
/** DER of a PKCS #8 Ed25519 private key, before its 32-byte seed. */
const PKCS8_PREFIX = Buffer.from('302e020100300506032b657004220420', 'hex');

/** The 32-byte Ed25519 seed of a PEM PKCS #8 private key, or undefined. */
export function parseEd25519Pem(pem: string): Uint8Array | undefined {
  const m = /-----BEGIN PRIVATE KEY-----([A-Za-z0-9+/=\s]+)-----END PRIVATE KEY-----/.exec(pem);
  if (!m) return undefined;
  const der = Buffer.from((m[1] as string).replace(/\s+/g, ''), 'base64');
  if (der.length !== 48 || !der.subarray(0, 16).equals(PKCS8_PREFIX)) return undefined;
  return new Uint8Array(der.subarray(16));
}

const B32 = 'abcdefghijklmnopqrstuvwxyz234567';

function base32(bytes: Uint8Array): string {
  let out = '';
  let bits = 0;
  let value = 0;
  for (const b of bytes) {
    value = (value << 8) | b;
    bits += 8;
    while (bits >= 5) {
      out += B32[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
    value &= (1 << bits) - 1;
  }
  if (bits > 0) out += B32[(value << (5 - bits)) & 31];
  return out;
}

function keyPair(seed: Uint8Array): { privateKey: ReturnType<typeof createPrivateKey>; publicKey: Uint8Array } {
  const privateKey = createPrivateKey({ key: Buffer.concat([PKCS8_PREFIX, seed]), format: 'der', type: 'pkcs8' });
  const jwk = createPublicKey(privateKey).export({ format: 'jwk' });
  return { privateKey, publicKey: new Uint8Array(Buffer.from(jwk.x as string, 'base64url')) };
}

/** The Web Bundle ID of an Ed25519 public key. */
export function webBundleId(publicKey: Uint8Array): string {
  return base32(new Uint8Array([...publicKey, 0x00, 0x01, 0x02]));
}

function be64(n: number): Uint8Array {
  const b = Buffer.alloc(8);
  b.writeBigUInt64BE(BigInt(n));
  return b;
}

/** `keyPem` is the secret's value; `keyId` names it in errors. */
export function writeSignedWebBundle(tree: Tree, keyPem: string, keyId: string): { bytes: Uint8Array; id: string } {
  const seed = parseEd25519Pem(keyPem);
  if (!seed) {
    throw new SitefileError(
      'E_SECRET',
      `EXPORT ... KEY ${keyId}: the secret is not an unencrypted Ed25519 private key in PEM (PKCS #8, as 'openssl genpkey -algorithm ed25519' writes)`,
    );
  }
  const { privateKey, publicKey } = keyPair(seed);
  const id = webBundleId(publicKey);
  const bundle = writeWebBundleAt(tree, `isolated-app://${id}/`);
  const attributes = cborMap([[cborText('webBundleId'), cborText(id)]]);
  const head = [cborBytes(MAGIC), cborBytes(VERSION_B2), attributes];
  const stripped = cborArray([...head, cborArray([])]);
  const sigAttributes = cborMap([[cborText('ed25519PublicKey'), cborBytes(publicKey)]]);
  const hash = createHash('sha512').update(bundle).digest();
  const toSign = Buffer.concat([be64(hash.length), hash, be64(stripped.length), stripped, be64(sigAttributes.length), sigAttributes]);
  const signature = sign(null, toSign, privateKey);
  const block = cborArray([...head, cborArray([cborArray([sigAttributes, cborBytes(signature)])])]);
  return { bytes: Buffer.concat([block, bundle]), id };
}
