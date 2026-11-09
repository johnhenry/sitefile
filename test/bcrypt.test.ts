// bcrypt for serve --auth-file (johnhenry/sitefile#278). rust/src/bcrypt_verify.rs has the same vectors.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PI_WORDS, bcryptHash, bcryptKey, parseBcrypt, verifyBcrypt } from '../src/bcrypt.js';
import { BasicAuth, parseHtpasswd, verifyPassword } from '../src/serve-auth.js';

/** OpenBSD's regression vectors ($2a$) and htpasswd -B output ($2y$). */
const VECTORS: [string, string][] = [
  ['$2a$06$DCq7YPn5Rq63x1Lad4cll.TV4S6ytwfsfvkgY8jIucDrjc8deX1s.', ''],
  ['$2a$06$m0CrhHm10qJ3lXRY.5zDGO3rS2KdeeWLuGmsfGlMfOxih58VYVfxe', 'a'],
  ['$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i', 'abc'],
  ['$2a$06$.rCVZVOThsIa97pEDOxvGuRRgzG64bvtJ0938xuqzv18d3ZpQhstC', 'abcdefghijklmnopqrstuvwxyz'],
  ['$2a$06$fPIsBO8qRqkjj273rfaOI.HtSV9jLDpTbZn782DC6/t7qT67P6FfO', '~!@#$%^&*()      ~!@#$%^&*()PNBFRD'],
  ['$2y$04$H52uuRGqEIy8.sR2oMtiEu08EXLUPljPpA9ItS9ur0y1xB84QRQI2', 'correct horse'],
  ['$2y$05$oazHu0lR350oOyo2jRbUTuzi6JA4Fsrc5wlinCvXNWT8q8B/KDyUW', 'correct horse'],
  ['$2y$04$NAqzALLvFANm2zUmRyTxDujofZJsxPeCfzjmO1uKWS.Yw9VLGqdqu', ''],
  ['$2y$04$fIY1G.Modf5ClXH1OSug7OWDvAiHLGvTDprrHq68qfgrlUv.Nynp.', 'ünïcødé ✓'],
];

test('the Blowfish tables are the hex digits of pi', () => {
  // Machin: pi = 16 atan(1/5) - 4 atan(1/239), in fixed point with guard bits.
  const words = 18 + 1024;
  const bits = BigInt(words * 32 + 64);
  const one = 1n << bits;
  const atanInv = (x: bigint): bigint => {
    let t = one / x;
    let s = t;
    const x2 = x * x;
    for (let n = 1n, sign = -1n; t !== 0n; n++, sign = -sign) {
      t /= x2;
      s += (sign * t) / (2n * n + 1n);
    }
    return s;
  };
  const frac = 16n * atanInv(5n) - 4n * atanInv(239n) - 3n * one;
  const hex = frac.toString(16).padStart(Number(bits) / 4, '0').slice(0, words * 8);
  assert.equal(Buffer.from(PI_WORDS, 'base64').toString('hex'), hex);
});

test('bcrypt vectors verify, round-trip, and $2a$/$2b$/$2y$ compute the same', () => {
  for (const [hash, pw] of VECTORS) {
    assert.ok(verifyBcrypt(hash, pw), hash);
    assert.ok(!verifyBcrypt(hash, pw + 'x'), hash);
    const p = parseBcrypt(hash)!;
    assert.equal(bcryptHash(pw, p.cost, p.salt, hash.slice(0, 4)), hash);
    for (const prefix of ['$2a$', '$2b$', '$2y$']) assert.ok(verifyBcrypt(prefix + hash.slice(4), pw));
  }
});

test('bcrypt keys stop at 72 bytes and at a NUL', () => {
  // htpasswd -bnBC 4 u <50 x then 50 y>
  const h = '$2y$04$ZftbXfh1CkyZ6xMWQVjV9.Wm8FDybgCYApVIKqfcgxfyHOex7MIOi';
  const x50 = 'x'.repeat(50);
  assert.ok(verifyBcrypt(h, x50 + 'y'.repeat(50)));
  assert.ok(verifyBcrypt(h, x50 + 'y'.repeat(22)));
  assert.ok(!verifyBcrypt(h, x50 + 'y'.repeat(21)));
  assert.ok(verifyBcrypt(VECTORS[2]![0], 'abc\0def'));
  assert.deepEqual([...bcryptKey('ab')], [0x61, 0x62, 0]);
});

test('malformed bcrypt hashes are refused', () => {
  const ok = VECTORS[0]![0];
  assert.ok(parseBcrypt(ok));
  assert.ok(parseBcrypt(ok.replace('$06$', '$31$')));
  const bads = [
    ok.slice(0, 59),
    ok + '.',
    ok.replace('$06$', '$03$'),
    ok.replace('$06$', '$32$'),
    ok.replace('$2a$', '$2x$'),
    ok.replace('DCq7', 'DC!7'),
    // a salt or hash whose last character has bits set that the bytes do not use
    `${ok.slice(0, 28)}/${ok.slice(29)}`,
    `${ok.slice(0, 59)}/`,
  ];
  for (const bad of bads) {
    assert.equal(parseBcrypt(bad), undefined, bad);
    assert.ok(!verifyBcrypt(bad, ''), bad);
  }
});

test('--auth-file accepts bcrypt entries (htpasswd -B) next to {SHA} ones', () => {
  const e = parseHtpasswd(`alice:{SHA}W6ph5Mm5Pz8GgiULbPgzG37mj9g=\ncarol:${VECTORS[5]![0]}\ndave:${VECTORS[2]![0].replace('$2a$', '$2b$')}\n`, 'f');
  assert.equal(e.length, 3);
  assert.ok(verifyPassword(e[1]!.verifier, 'correct horse'));
  assert.ok(!verifyPassword(e[1]!.verifier, 'correct horsE'));
  const a = new BasicAuth(e);
  const basic = (s: string): string => `Basic ${Buffer.from(s).toString('base64')}`;
  assert.ok(a.allows(basic('carol:correct horse')));
  assert.ok(a.allows(basic('dave:abc')));
  assert.ok(!a.allows(basic('dave:abd')));
  assert.ok(!a.allows(basic('carol:password')), 'one user, not another user\'s password');
  assert.throws(() => parseHtpasswd('bob:$2y$05$abc', 'f'), /f:1: user 'bob': malformed bcrypt hash/);
  assert.throws(() => parseHtpasswd(`bob:${VECTORS[0]![0].replace('$06$', '$03$')}`, 'f'), /malformed bcrypt hash/);
  assert.throws(() => parseHtpasswd('bob:$apr1$x$y', 'f'), /only \{SHA\} and bcrypt entries, htpasswd -s or -B/);
});
