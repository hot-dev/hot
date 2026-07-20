//! Shared outbound destination validation for user-controlled network clients.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationPolicy {
    AllowPrivate,
    PublicOnly,
}

impl DestinationPolicy {
    pub fn from_vm(vm: &crate::lang::runtime::vm::VirtualMachine) -> Self {
        if vm.is_isolation_enabled()
            && !vm
                .get_conf()
                .get_bool_or_default("network.allow-private", false)
        {
            Self::PublicOnly
        } else {
            Self::AllowPrivate
        }
    }

    pub fn validate_ip(self, ip: IpAddr) -> Result<(), String> {
        if self == Self::AllowPrivate || is_public_destination(ip) {
            Ok(())
        } else {
            Err(format!(
                "outbound connection to non-public address {ip} is blocked"
            ))
        }
    }

    pub async fn resolve_host(self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("failed to resolve {host}: {e}"))?
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        if addrs.is_empty() {
            return Err(format!("{host} resolved to no addresses"));
        }
        for addr in &addrs {
            self.validate_ip(addr.ip())?;
        }
        Ok(addrs)
    }

    pub async fn resolve_url(&self, url: &Url) -> Result<Vec<SocketAddr>, String> {
        let host = url
            .host_str()
            .ok_or_else(|| "outbound URL must include a host".to_string())?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "outbound URL has no known port".to_string())?;
        self.resolve_host(host, port).await
    }
}

fn ipv4_in(ip: Ipv4Addr, network: [u8; 4], prefix: u32) -> bool {
    let value = u32::from(ip);
    let network = u32::from(Ipv4Addr::from(network));
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    value & mask == network & mask
}

fn ipv6_in(ip: Ipv6Addr, network: [u16; 8], prefix: u32) -> bool {
    let value = u128::from(ip);
    let network = u128::from(Ipv6Addr::from(network));
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    value & mask == network & mask
}

pub fn is_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_private()
                && !ip.is_link_local()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !ipv4_in(ip, [0, 0, 0, 0], 8)
                && !ipv4_in(ip, [100, 64, 0, 0], 10) // carrier-grade NAT
                && !ipv4_in(ip, [192, 0, 0, 0], 24)
                && !ipv4_in(ip, [192, 0, 2, 0], 24)
                && !ipv4_in(ip, [198, 18, 0, 0], 15)
                && !ipv4_in(ip, [198, 51, 100, 0], 24)
                && !ipv4_in(ip, [203, 0, 113, 0], 24)
                && !ipv4_in(ip, [240, 0, 0, 0], 4)
        }
        IpAddr::V6(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && !ipv6_in(ip, [0xfc00, 0, 0, 0, 0, 0, 0, 0], 7)
                && !ipv6_in(ip, [0xfe80, 0, 0, 0, 0, 0, 0, 0], 10)
                && !ipv6_in(ip, [0x2001, 0x0db8, 0, 0, 0, 0, 0, 0], 32)
                // 6to4 and Teredo tunnel prefixes embed IPv4 addresses and can
                // route to arbitrary v4 destinations through relays.
                && !ipv6_in(ip, [0x2002, 0, 0, 0, 0, 0, 0, 0], 16)
                && !ipv6_in(ip, [0x2001, 0, 0, 0, 0, 0, 0, 0], 32)
                && embedded_ipv4(ip).is_none_or(|v4| is_public_destination(v4.into()))
        }
    }
}

/// IPv4 address carried inside an IPv6 address, for prefixes that translate
/// directly to a v4 destination: IPv4-mapped (`::ffff:0:0/96`), the deprecated
/// IPv4-compatible `::/96`, and the NAT64 well-known prefix (`64:ff9b::/96`).
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    if ipv6_in(ip, [0, 0, 0, 0, 0, 0, 0, 0], 96)
        || ipv6_in(ip, [0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96)
    {
        let segments = ip.segments();
        let value = ((segments[6] as u32) << 16) | segments[7] as u32;
        // `::` and `::1` are handled by the unspecified/loopback checks.
        if value > 1 {
            return Some(Ipv4Addr::from(value));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_only_blocks_internal_and_special_ranges() {
        for ip in [
            "127.0.0.1",
            "0.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::10.0.0.1",              // IPv4-compatible
            "64:ff9b::a00:1",          // NAT64 embedding 10.0.0.1
            "2002:a00:1::1",           // 6to4
            "2001:0:53aa:64c:0:0:0:1", // Teredo
        ] {
            assert!(!is_public_destination(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public_destination("8.8.8.8".parse().unwrap()));
        assert!(is_public_destination(
            "2606:4700:4700::1111".parse().unwrap()
        ));
        // NAT64 translating to a public v4 stays reachable; 2001:db8's siblings
        // outside the Teredo /32 stay reachable.
        assert!(is_public_destination("64:ff9b::808:808".parse().unwrap()));
        assert!(is_public_destination(
            "2001:4860:4860::8888".parse().unwrap()
        ));
    }
}
