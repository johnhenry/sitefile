import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { carRoot, cidString, encodeCar, fileCid } from '../src/car.js';
import { encodeTree, formatFromExtension, defaultOutName } from '../src/export-formats.js';
import { parseExportFormat } from '../src/language/ast.js';
import { Tree, makeFile } from '../src/tree.js';

// Expected values from kubo 0.43.1: `ipfs add -r --hidden --cid-version=1` of the same files on disk,
// and `ipfs dag export <root>` for the CAR's SHA-256.
function fixture(): Tree {
  const big = new Uint8Array(300000);
  for (let i = 0; i < big.length; i++) big[i] = i % 251;
  const enc = new TextEncoder();
  return Tree.fromFiles([
    ['/a.txt', makeFile(enc.encode('hello'), 't')],
    ['/empty', makeFile(new Uint8Array(0), 't')],
    ['/.hidden', makeFile(enc.encode('.h'), 't')],
    ['/sub/big.bin', makeFile(big, 't')],
    ['/sub/deeper/huge.txt', makeFile(new Uint8Array(50_000_000).fill(120), 't', 0o755)],
    ['/sub/ü name.txt', makeFile(enc.encode('z'), 't')],
  ]);
}

test('file CIDs match kubo: raw leaves, two chunks, a two-level balanced tree, an empty file', () => {
  assert.equal(fileCid(new TextEncoder().encode('hello')), 'bafkreibm6jg3ux5qumhcn2b3flc3tyu6dmlb4xa7u5bf44yegnrjhc4yeq');
  assert.equal(fileCid(new Uint8Array(0)), 'bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku');
  const tree = fixture();
  assert.equal(fileCid(tree.get('/sub/big.bin')!.bytes), 'bafybeih7gz5kvvg2zafb7vue6izhy6c4tglvwpi3rgunwdag2fidn2y6eq');
  assert.equal(fileCid(tree.get('/sub/deeper/huge.txt')!.bytes), 'bafybeib7wixxgtraw4anto5fi2eadaxjihpukuqbuuonv6w4njmny5lk2i');
});

test('the root CID and the CAR bytes match kubo (ipfs add -r --hidden --cid-version=1, ipfs dag export)', () => {
  const tree = fixture();
  const { bytes, root } = encodeCar(tree);
  assert.equal(root, 'bafybeiavtrkscgzjs4rysgzmsgbgoaq646dyz75fiee5jkoe67dvgminy4');
  assert.equal(carRoot(tree), root);
  assert.equal(createHash('sha256').update(bytes).digest('hex'), '71cc6a2157a41e5599ead9624df691744e72b8c6da0691100495c715a134920c');
  assert.deepEqual(encodeTree(tree, 'car'), bytes);
  // Modes are not recorded.
  const plain = Tree.fromFiles(tree.entries().map(([p, n]) => [p, makeFile(n.bytes, 't')]));
  assert.equal(carRoot(plain), root);
});

test('car format names, extension and default output', () => {
  assert.equal(parseExportFormat('CAR'), 'car');
  assert.equal(formatFromExtension('site.CAR'), 'car');
  assert.equal(defaultOutName('car'), 'dist.car');
  assert.equal(cidString(new Uint8Array([1, 0x55, 0x12, 0x20, ...new Uint8Array(32)])).slice(0, 8), 'bafkreia');
});

test('a directory too large for a plain UnixFS directory is refused', () => {
  const files: Array<[string, ReturnType<typeof makeFile>]> = [];
  for (let i = 0; i < 4000; i++) files.push([`/d/${'n'.repeat(40)}${i}`, makeFile(new TextEncoder().encode(String(i)), 't')]);
  assert.throws(() => carRoot(Tree.fromFiles(files)), /E_EXPORT|HAMT/);
});
