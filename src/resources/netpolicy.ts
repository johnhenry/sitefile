import { BlockList, isIP } from 'node:net';

// Separate lists: a BlockList checks IPv4 addresses against IPv4-mapped IPv6
// rules too, so `::ffff:0:0/96` in a shared list would block every IPv4.
const blocked4 = new BlockList();
const blocked6 = new BlockList();

const V4: Array<[string, number]> = [
  ['0.0.0.0', 8],
  ['10.0.0.0', 8],
  ['100.64.0.0', 10],
  ['127.0.0.0', 8],
  ['169.254.0.0', 16],
  ['172.16.0.0', 12],
  ['192.0.0.0', 24],
  ['192.0.2.0', 24],
  ['192.168.0.0', 16],
  ['198.18.0.0', 15],
  ['198.51.100.0', 24],
  ['203.0.113.0', 24],
  ['224.0.0.0', 4],
  ['240.0.0.0', 4], // includes 255.255.255.255
];

const V6: Array<[string, number]> = [
  ['::', 128],
  ['::1', 128],
  ['::ffff:0:0', 96], // all IPv4-mapped addresses
  ['64:ff9b:1::', 48],
  ['100::', 64],
  ['2001:db8::', 32],
  ['fc00::', 7],
  ['fe80::', 10],
  ['ff00::', 8],
];

for (const [net, prefix] of V4) blocked4.addSubnet(net, prefix, 'ipv4');
for (const [net, prefix] of V6) blocked6.addSubnet(net, prefix, 'ipv6');

/**
 * True when `address` is an IP literal that is publicly routable. Loopback,
 * private, link-local, CGNAT, documentation, multicast, reserved and every
 * IPv4-mapped IPv6 address are not public. Non-IP strings return false.
 */
export function isPublicAddress(address: string): boolean {
  let a = address;
  if (a.startsWith('[') && a.endsWith(']')) a = a.slice(1, -1);
  // Zone IDs (fe80::1%en0) are link-local by definition.
  if (a.includes('%')) return false;
  const family = isIP(a);
  if (family === 4) return !blocked4.check(a, 'ipv4');
  if (family === 6) {
    if (blocked6.check(a, 'ipv6')) return false;
    // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address in the low
    // 32 bits; judge it by that address (64:ff9b::7f00:1 is 127.0.0.1).
    const groups = expandIPv6(a);
    if (groups && groups[0] === 0x64 && groups[1] === 0xff9b && groups.slice(2, 6).every((g) => g === 0)) {
      const hi = groups[6]!;
      const lo = groups[7]!;
      return !blocked4.check(`${hi >> 8}.${hi & 0xff}.${lo >> 8}.${lo & 0xff}`, 'ipv4');
    }
    return true;
  }
  return false;
}

/** Eight 16-bit groups of a valid IPv6 literal (which may end in dotted IPv4). */
function expandIPv6(a: string): number[] | undefined {
  let s = a.toLowerCase();
  const v4 = /(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(s);
  if (v4) {
    const [b0, b1, b2, b3] = v4.slice(1).map(Number) as [number, number, number, number];
    s = s.slice(0, v4.index) + ((b0 << 8) | b1).toString(16) + ':' + ((b2 << 8) | b3).toString(16);
  }
  const [head, tail] = s.split('::') as [string, string | undefined];
  const h = head ? head.split(':') : [];
  const t = tail ? tail.split(':') : [];
  const zeros = tail === undefined ? [] : new Array<string>(8 - h.length - t.length).fill('0');
  const all = [...h, ...zeros, ...t].map((g) => parseInt(g, 16));
  return all.length === 8 ? all : undefined;
}
