//! Which IP addresses count as public (port of `src/resources/netpolicy.ts`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const V4: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4), // includes 255.255.255.255
];

const V6: &[([u16; 8], u32)] = &[
    ([0, 0, 0, 0, 0, 0, 0, 0], 128),         // ::
    ([0, 0, 0, 0, 0, 0, 0, 1], 128),         // ::1
    ([0, 0, 0, 0, 0, 0xffff, 0, 0], 96),     // all IPv4-mapped addresses
    ([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48),  // 64:ff9b:1::/48 (local-use NAT64)
    ([0x100, 0, 0, 0, 0, 0, 0, 0], 64),      // discard-only
    ([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32), // documentation
    ([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7),      // unique local
    ([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10),     // link-local
    ([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),      // multicast
];

fn v4_blocked(a: Ipv4Addr) -> bool {
    let x = u32::from(a);
    V4.iter().any(|(net, prefix)| {
        let mask = if *prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        x & mask == u32::from(Ipv4Addr::from(*net)) & mask
    })
}

fn v6_blocked(a: Ipv6Addr) -> bool {
    let x = u128::from(a);
    V6.iter().any(|(net, prefix)| {
        let mask = if *prefix == 0 {
            0
        } else {
            u128::MAX << (128 - prefix)
        };
        let n = u128::from(Ipv6Addr::from(*net));
        x & mask == n & mask
    })
}

/// True when `ip` is publicly routable (see [`is_public_address`]).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => !v4_blocked(a),
        IpAddr::V6(a) => {
            if v6_blocked(a) {
                return false;
            }
            // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address in the low
            // 32 bits; judge it by that address (64:ff9b::7f00:1 is 127.0.0.1).
            let g = a.segments();
            if g[0] == 0x64 && g[1] == 0xff9b && g[2..6].iter().all(|&s| s == 0) {
                let v4 =
                    Ipv4Addr::new((g[6] >> 8) as u8, g[6] as u8, (g[7] >> 8) as u8, g[7] as u8);
                return !v4_blocked(v4);
            }
            true
        }
    }
}

/// True when `address` is an IP literal that is publicly routable. Loopback,
/// private, link-local, CGNAT, documentation, multicast, reserved and every
/// IPv4-mapped IPv6 address are not public. Non-IP strings return false.
pub fn is_public_address(address: &str) -> bool {
    let a = address
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(address);
    // Zone IDs (fe80::1%en0) are link-local by definition.
    if a.contains('%') {
        return false;
    }
    if let Ok(v4) = a.parse::<Ipv4Addr>() {
        return is_public_ip(IpAddr::V4(v4));
    }
    if let Ok(v6) = a.parse::<Ipv6Addr>() {
        return is_public_ip(IpAddr::V6(v6));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_public_address as p;

    #[test]
    fn blocks_non_public_ipv4_ranges() {
        for a in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "127.9.9.9",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!p(a), "{a}");
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for a in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.20.0.1",
            "223.255.255.255",
        ] {
            assert!(p(a), "{a}");
        }
    }

    #[test]
    fn blocks_non_public_ipv6_including_all_v4_mapped() {
        for a in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
            "::ffff:a00:1",
            "64:ff9b:1::1",
            "100::1",
            "2001:db8::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "ff02::1",
            "[::1]",
            "fe80::1%en0",
        ] {
            assert!(!p(a), "{a}");
        }
    }

    #[test]
    fn allows_public_ipv6() {
        for a in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "[2606:4700::1]",
            "64:ff9b::808:808",
        ] {
            assert!(p(a), "{a}");
        }
    }

    #[test]
    fn non_ip_strings_are_not_public() {
        for a in [
            "",
            "localhost",
            "example.com",
            "1.2.3",
            "999.1.1.1",
            "http://1.1.1.1",
        ] {
            assert!(!p(a), "{a}");
        }
    }

    #[test]
    fn nat64_judged_by_embedded_ipv4() {
        for a in [
            "64:ff9b::7f00:1",
            "64:ff9b::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::192.168.1.1",
        ] {
            assert!(!p(a), "{a}");
        }
        for a in ["64:ff9b::808:808", "64:ff9b::1.1.1.1"] {
            assert!(p(a), "{a}");
        }
    }
}
