import { test } from 'node:test';
import assert from 'node:assert/strict';
import { humanSize } from '../src/cli/common.js';
import { tailLines } from '../src/cli/servers.js';

test('humanSize keeps significant zeros and strips only decimal ones', () => {
  const cases: Array<[number, string]> = [
    [0, '0B'],
    [999, '999B'],
    [1000, '1kB'],
    [1023, '1.02kB'],
    [1500, '1.5kB'],
    [20_000, '20kB'],
    [99_995, '100kB'],
    [100_000, '100kB'],
    [1_500_000, '1.5MB'],
  ];
  for (const [n, want] of cases) assert.equal(humanSize(n), want, String(n));
});

test('tailLines returns the last n lines with or without a trailing newline', () => {
  assert.equal(tailLines('a\nb\nc\n', 2), 'b\nc\n');
  assert.equal(tailLines('a\nb\nc', 2), 'b\nc\n');
  assert.equal(tailLines('a\nb\nc', 10), 'a\nb\nc\n');
  assert.equal(tailLines('a\nb\nc\n', 0), '');
  assert.equal(tailLines('', 3), '');
});
