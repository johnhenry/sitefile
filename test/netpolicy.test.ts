import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isPublicAddress } from '../src/resources/netpolicy.js';

test('blocks non-public IPv4 ranges', () => {
  for (const a of [
    '0.0.0.0', '0.1.2.3', '10.0.0.1', '10.255.255.255', '100.64.0.1', '100.127.255.254', '127.0.0.1', '127.9.9.9',
    '169.254.169.254', '172.16.0.1', '172.31.255.255', '192.0.0.8', '192.0.2.1', '192.168.1.1', '198.18.0.1',
    '198.19.255.255', '198.51.100.7', '203.0.113.9', '224.0.0.1', '239.255.255.255', '240.0.0.1', '255.255.255.255',
  ]) {
    assert.equal(isPublicAddress(a), false, a);
  }
});

test('allows public IPv4', () => {
  for (const a of ['1.1.1.1', '8.8.8.8', '93.184.216.34', '172.15.255.255', '172.32.0.1', '100.63.255.255', '100.128.0.1', '198.20.0.1', '223.255.255.255']) {
    assert.equal(isPublicAddress(a), true, a);
  }
});

test('blocks non-public IPv6 ranges, including all IPv4-mapped', () => {
  for (const a of [
    '::', '::1', '::ffff:127.0.0.1', '::ffff:8.8.8.8', '::ffff:a00:1', '64:ff9b:1::1', '100::1', '2001:db8::1',
    'fc00::1', 'fd12:3456::1', 'fe80::1', 'febf::1', 'ff02::1', '[::1]', 'fe80::1%en0',
  ]) {
    assert.equal(isPublicAddress(a), false, a);
  }
});

test('allows public IPv6', () => {
  for (const a of ['2606:4700:4700::1111', '2001:4860:4860::8888', '[2606:4700::1]', '64:ff9b::808:808']) {
    assert.equal(isPublicAddress(a), true, a);
  }
});

test('non-IP strings are not public', () => {
  for (const a of ['', 'localhost', 'example.com', '1.2.3', '999.1.1.1', 'http://1.1.1.1']) {
    assert.equal(isPublicAddress(a), false, a);
  }
});

test('NAT64 addresses are judged by their embedded IPv4 address', () => {
  for (const a of ['64:ff9b::7f00:1', '64:ff9b::127.0.0.1', '64:ff9b::a00:1', '64:ff9b::192.168.1.1']) {
    assert.equal(isPublicAddress(a), false, a);
  }
  for (const a of ['64:ff9b::808:808', '64:ff9b::1.1.1.1']) {
    assert.equal(isPublicAddress(a), true, a);
  }
});
