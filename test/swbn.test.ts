import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash, createPublicKey, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseEd25519Pem, webBundleId, writeSignedWebBundle } from '../src/swbn.js';
import { writeWebBundleAt } from '../src/wbn.js';
import { decodeCbor } from '../src/cbor.js';
import { Tree, makeFile } from '../src/tree.js';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
// The secret key of RFC 8032's first Ed25519 test vector: a published test value, never a real key.
const PEM = readFileSync(path.join(root, 'test', 'fixtures', 'swbn', 'rfc8032-test-key.pem'), 'utf8');
const PUBLIC = 'd75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a';

function site(): Tree {
  const enc = new TextEncoder();
  return Tree.fromFiles([
    ['/index.html', makeFile(enc.encode('<p>iwa</p>'), 't')],
    ['/.well-known/manifest.webmanifest', makeFile(enc.encode('{"name":"x"}'), 't')],
  ]);
}

test('the PKCS #8 PEM seed and the Web Bundle ID of the RFC 8032 test key', () => {
  assert.equal(Buffer.from(parseEd25519Pem(PEM)!).toString('hex'), '9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60');
  assert.equal(parseEd25519Pem('-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----'), undefined);
  assert.equal(webBundleId(Buffer.from(PUBLIC, 'hex')).length, 56);
});

test('swbn: integrity block v2 + the bundle at isolated-app://<id>/, signature verifies', () => {
  const { bytes, id } = writeSignedWebBundle(site(), PEM, 'iwa');
  assert.equal(id, webBundleId(Buffer.from(PUBLIC, 'hex')));
  const bundle = writeWebBundleAt(site(), `isolated-app://${id}/`);
  const block = bytes.subarray(0, bytes.length - bundle.length);
  assert.deepEqual(bytes.subarray(block.length), bundle);
  const { value, end } = decodeCbor(block);
  assert.equal(end, block.length);
  const [magic, version, attrs, stack] = value as [Uint8Array, Uint8Array, Map<string, string>, Array<[Map<string, Uint8Array>, Uint8Array]>];
  assert.equal(Buffer.from(magic).toString('hex'), 'f09f968bf09f93a6');
  assert.equal(Buffer.from(version).toString('hex'), '32620000');
  assert.equal(attrs.get('webBundleId'), id);
  const [sigAttrs, sig] = stack[0]!;
  assert.equal(Buffer.from(sigAttrs.get('ed25519PublicKey')!).toString('hex'), PUBLIC);
  // Data to be signed: length-prefixed SHA-512(bundle), the block without signatures, the attributes.
  const lp = (b: Uint8Array): Buffer => {
    const n = Buffer.alloc(8);
    n.writeBigUInt64BE(BigInt(b.length));
    return Buffer.concat([n, b]);
  };
  const stripped = Buffer.concat([Buffer.from([0x84]), block.subarray(1, block.indexOf(0x81, 30)), Buffer.from([0x80])]);
  const attrBytes = block.subarray(block.indexOf(0x81, 30) + 2, block.length - 66);
  const toSign = Buffer.concat([lp(createHash('sha512').update(bundle).digest()), lp(stripped), lp(attrBytes)]);
  const pub = createPublicKey({ key: { kty: 'OKP', crv: 'Ed25519', x: Buffer.from(PUBLIC, 'hex').toString('base64url') }, format: 'jwk' });
  assert.ok(verify(null, toSign, pub, sig));
  // Deterministic, and byte-identical to wbn-sign 0.3.1 for this tree and key (checked when written).
  assert.equal(createHash('sha256').update(bytes).digest('hex'), createHash('sha256').update(writeSignedWebBundle(site(), PEM, 'iwa').bytes).digest('hex'));
  assert.throws(() => writeSignedWebBundle(site(), 'nope', 'iwa'), /EXPORT \.\.\. KEY iwa: the secret is not an unencrypted Ed25519 private key/);
});

test('a PEM key is a valid --secret, but a value with line breaks is never sent as a header', async () => {
  const { loadSecrets, secretHeaders } = await import('../src/secrets.js');
  const keyFile = path.join(root, 'test', 'fixtures', 'swbn', 'rfc8032-test-key.pem');
  const secrets = await loadSecrets([`id=iwa,src=${keyFile}`]);
  assert.ok(secrets.get('iwa')?.includes('\n'));
  const span = { file: 'Sitefile', line: 1, column: 1 };
  assert.throws(() => secretHeaders({ name: 'api', secrets: [{ id: 'iwa', header: 'X-Key', bearer: false }], span }, secrets), /IMPORT api: secret 'iwa' cannot be sent in an HTTP header/);
  await assert.rejects(loadSecrets(['id=bad,env=BAD'], { BAD: 'a\u0001b' }), /must be printable ASCII/);
});
