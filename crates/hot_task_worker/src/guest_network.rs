//! Which destinations box guests may reach. The Linux CNI code turns this
//! policy into each task's iptables chain; the policy itself is plain data so
//! it is tested on every platform.

use hot::val::Val;
use ipnet::Ipv4Net;
use std::net::Ipv4Addr;
use std::sync::OnceLock;

/// The Kata guest bridge network.
pub(crate) const SUBNET: &str = "10.88.0.0/16";

/// Destinations that are never public internet. This includes cloud metadata,
/// host/VPC address space, carrier NAT, multicast, and other Kata guests.
pub(crate) const BLOCKED_DESTINATIONS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Guest destinations no operator rule can open: the host, link-local
/// services including every cloud metadata endpoint, other guests, and
/// addresses that are never one remote host.
const ALWAYS_BLOCKED: &[&str] = &[
    "0.0.0.0/8",
    "127.0.0.0/8",
    "169.254.0.0/16",
    SUBNET,
    "224.0.0.0/4",
    "240.0.0.0/4",
];

/// Operator rules for box guests: the same `network.allow-cidrs` and
/// `network.deny-cidrs` that govern normal user code (IPv4 entries only;
/// guests have no IPv6), plus `box.guest-dns` resolvers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestNetworkPolicy {
    allow: Vec<Ipv4Net>,
    deny: Vec<Ipv4Net>,
    dns: Vec<Ipv4Addr>,
}

impl Default for GuestNetworkPolicy {
    /// Public resolvers: VPC resolvers are private or link-local, so using
    /// them would mean opening a path to the VPC or metadata network.
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            dns: vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
        }
    }
}

impl GuestNetworkPolicy {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        let mut policy = Self {
            allow: ipv4_nets(conf, "network.allow-cidrs")?,
            deny: ipv4_nets(conf, "network.deny-cidrs")?,
            ..Self::default()
        };
        let dns = list(conf, "box.guest-dns");
        if !dns.is_empty() {
            policy.dns = dns
                .iter()
                .map(|entry| {
                    entry.parse().map_err(|_| {
                        format!("box.guest-dns entry {entry:?} is not an IPv4 address")
                    })
                })
                .collect::<Result<_, _>>()?;
        }
        Ok(policy)
    }

    /// Resolvers written to the guests' resolv.conf.
    pub(crate) fn resolvers(&self) -> &[Ipv4Addr] {
        &self.dns
    }

    /// The per-task chain, in order: resolvers on port 53, the fixed and
    /// operator deny sets, the allowlist, the remaining private ranges, and
    /// then public egress.
    pub(crate) fn chain_rules(&self, chain: &str) -> Vec<Vec<String>> {
        let rule = |parts: &[&str]| -> Vec<String> {
            ["-A", chain]
                .iter()
                .chain(parts)
                .map(|s| s.to_string())
                .collect()
        };
        let mut rules = Vec::new();
        for resolver in &self.dns {
            let resolver = resolver.to_string();
            for protocol in ["udp", "tcp"] {
                rules.push(rule(&[
                    "-d", &resolver, "-p", protocol, "--dport", "53", "-j", "ACCEPT",
                ]));
            }
        }
        let denied = ALWAYS_BLOCKED
            .iter()
            .map(|net| net.to_string())
            .chain(self.deny.iter().map(Ipv4Net::to_string));
        for destination in denied {
            rules.push(rule(&["-d", &destination, "-j", "REJECT"]));
        }
        for destination in &self.allow {
            rules.push(rule(&["-d", &destination.to_string(), "-j", "ACCEPT"]));
        }
        for destination in BLOCKED_DESTINATIONS {
            rules.push(rule(&["-d", destination, "-j", "REJECT"]));
        }
        rules.push(rule(&["-j", "ACCEPT"]));
        rules
    }
}

static GUEST_POLICY: OnceLock<GuestNetworkPolicy> = OnceLock::new();

/// Set the guest policy once at task-worker startup; later calls keep the first.
pub fn configure(policy: GuestNetworkPolicy) {
    let _ = GUEST_POLICY.set(policy);
}

pub(crate) fn guest_policy() -> &'static GuestNetworkPolicy {
    GUEST_POLICY.get_or_init(GuestNetworkPolicy::default)
}

fn list(conf: &Val, key: &str) -> Vec<String> {
    let entries: Vec<String> = match conf.get(key) {
        Some(Val::Vec(items)) => items
            .iter()
            .map(|item| item.to_string().trim_matches('"').to_string())
            .collect(),
        Some(_) => conf.get_str(key).split(',').map(str::to_string).collect(),
        None => Vec::new(),
    };
    entries
        .into_iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn ipv4_nets(conf: &Val, key: &str) -> Result<Vec<Ipv4Net>, String> {
    let mut nets = Vec::new();
    for entry in list(conf, key) {
        let net = entry
            .parse::<ipnet::IpNet>()
            .or_else(|_| entry.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
            .map_err(|_| format!("{key} entry {entry:?} is not an IP address or CIDR"))?;
        if let ipnet::IpNet::V4(net) = net {
            nets.push(net.trunc());
        }
    }
    Ok(nets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(policy: &GuestNetworkPolicy) -> Vec<String> {
        policy
            .chain_rules("HKT-test")
            .into_iter()
            .map(|rule| rule[2..].join(" "))
            .collect()
    }

    fn position(rules: &[String], rule: &str) -> usize {
        rules
            .iter()
            .position(|r| r == rule)
            .unwrap_or_else(|| panic!("missing {rule:?} in {rules:#?}"))
    }

    #[test]
    fn default_guest_chain_keeps_public_resolvers_and_blocks_private_ranges() {
        let rules = rules(&GuestNetworkPolicy::default());
        let dns = position(&rules, "-d 1.1.1.1 -p udp --dport 53 -j ACCEPT");
        let metadata = position(&rules, "-d 169.254.0.0/16 -j REJECT");
        let private = position(&rules, "-d 10.0.0.0/8 -j REJECT");
        assert!(dns < metadata && metadata < private);
        assert_eq!(rules.last().unwrap(), "-j ACCEPT");
    }

    #[test]
    fn allowlists_open_private_ranges_but_never_the_fixed_deny_set() {
        let conf = hot::val!({
            "network": {
                "allow-cidrs": "10.0.0.0/8, 192.168.5.10, fd00::/8",
                "deny-cidrs": ["10.68.0.0/16"],
            },
            "box": {"guest-dns": "10.68.0.2"},
        });
        let policy = GuestNetworkPolicy::from_conf(&conf).unwrap();
        let rules = rules(&policy);
        let allow = position(&rules, "-d 10.0.0.0/8 -j ACCEPT");
        assert!(position(&rules, "-d 192.168.5.10/32 -j ACCEPT") > 0);
        // Deny rules come first, so the allowlist cannot reopen them.
        for denied in [
            "10.68.0.0/16",
            "10.88.0.0/16",
            "169.254.0.0/16",
            "127.0.0.0/8",
        ] {
            assert!(
                position(&rules, &format!("-d {denied} -j REJECT")) < allow,
                "{denied}"
            );
        }
        assert!(position(&rules, "-d 10.0.0.0/8 -j REJECT") > allow);
        // A private resolver is reachable on port 53 only.
        assert_eq!(
            position(&rules, "-d 10.68.0.2 -p udp --dport 53 -j ACCEPT"),
            0
        );
        assert!(
            !rules.iter().any(|r| r.contains("fd00")),
            "guests are IPv4-only"
        );
        assert!(
            GuestNetworkPolicy::from_conf(&hot::val!({"box": {"guest-dns": "dns.example"}}))
                .is_err()
        );
    }

    #[test]
    fn blocked_ranges_cover_guest_and_metadata_networks() {
        assert!(BLOCKED_DESTINATIONS.contains(&"10.0.0.0/8"));
        assert!(BLOCKED_DESTINATIONS.contains(&"169.254.0.0/16"));
        assert!(BLOCKED_DESTINATIONS.contains(&"100.64.0.0/10"));
        assert!(BLOCKED_DESTINATIONS.contains(&"224.0.0.0/4"));
    }
}
