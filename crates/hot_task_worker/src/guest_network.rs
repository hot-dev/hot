//! Which destinations box guests may reach. The Linux CNI code turns this
//! policy into each task's iptables chain; the policy itself is plain data so
//! it is tested on every platform.

use hot::val::Val;
use ipnet::Ipv4Net;
use std::net::{IpAddr, Ipv4Addr};
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

/// Operator rules for box guests: the same `network.allow-cidrs`,
/// `network.deny-cidrs`, and `network.egress-proxy` that govern normal user
/// code (IPv4 entries only; guests have no IPv6), plus `box.guest-dns`
/// resolvers. Only the Kata backend applies them; Docker box tasks use plain
/// bridge networking, so a task worker on Docker refuses `network.egress-proxy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestNetworkPolicy {
    allow: Vec<Ipv4Net>,
    deny: Vec<Ipv4Net>,
    dns: Vec<Ipv4Addr>,
    /// The egress proxy's host and port. With a proxy, Kata guests reach
    /// nothing past their resolvers but the proxy.
    proxy: Option<(String, u16)>,
}

impl Default for GuestNetworkPolicy {
    /// Public resolvers: VPC resolvers are private or link-local, so using
    /// them would mean opening a path to the VPC or metadata network.
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            dns: vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
            proxy: None,
        }
    }
}

impl GuestNetworkPolicy {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        let proxy = hot::outbound::egress_proxy_setting(conf)?.and_then(|url| {
            let host = url.host_str()?.trim_matches(['[', ']']).to_string();
            Some((host, url.port_or_known_default()?))
        });
        let mut policy = Self {
            allow: ipv4_nets(conf, "network.allow-cidrs")?,
            deny: ipv4_nets(conf, "network.deny-cidrs")?,
            proxy,
            ..Self::default()
        };
        let dns = hot::outbound::conf_list(conf, "box.guest-dns");
        if !dns.is_empty() {
            policy.dns = dns
                .iter()
                .map(|entry| {
                    let resolver = entry.parse().map_err(|_| {
                        format!("box.guest-dns entry {entry:?} is not an IPv4 address")
                    })?;
                    if is_always_blocked(resolver) {
                        return Err(format!(
                            "box.guest-dns entry {entry:?} is in a range box guests can never \
                             reach, such as loopback, link-local, or the guest network"
                        ));
                    }
                    Ok(resolver)
                })
                .collect::<Result<_, _>>()?;
        }
        Ok(policy)
    }

    /// Resolvers written to the guests' resolv.conf.
    pub(crate) fn resolvers(&self) -> &[Ipv4Addr] {
        &self.dns
    }

    /// Why direct connections fail in box tasks under an egress proxy, for the
    /// startup log. It names the proxy's host and port, never its credentials.
    fn proxy_notice(&self) -> Option<String> {
        let (host, port) = self.proxy.as_ref()?;
        Some(format!(
            "network.egress-proxy is set: box guests on the Kata backend can connect only to \
             {host}:{port} and their resolvers. Guests do not use the proxy on their own and \
             never get its credentials; a box task sets HTTP_PROXY and HTTPS_PROXY in its env to \
             use it"
        ))
    }

    /// Guests' DNS does not pass through the egress proxy, so the default
    /// public resolvers are a way out the operator has no record of.
    fn public_resolver_warning(&self) -> Option<String> {
        if self.proxy.is_none() || self.dns != Self::default().dns {
            return None;
        }
        let resolvers: Vec<String> = self.dns.iter().map(ToString::to_string).collect();
        Some(format!(
            "box guests on the Kata backend can query public resolvers ({}) directly, outside \
             network.egress-proxy; set box.guest-dns to resolvers you can audit",
            resolvers.join(", ")
        ))
    }

    /// The egress proxy's addresses that guests may connect to, looked up now
    /// so each task's chain follows the proxy's DNS. Empty without a proxy.
    pub(crate) async fn proxy_addresses(&self) -> Result<Vec<Ipv4Addr>, String> {
        let Some((host, port)) = &self.proxy else {
            return Ok(Vec::new());
        };
        let mut addrs: Vec<Ipv4Addr> = tokio::net::lookup_host((host.as_str(), *port))
            .await
            .map_err(|e| format!("failed to resolve egress proxy {host}: {e}"))?
            .filter_map(|addr| match addr.ip() {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(ip) => ip.to_ipv4_mapped(),
            })
            .filter(|ip| !is_always_blocked(*ip))
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        if addrs.is_empty() {
            return Err(format!(
                "egress proxy {host} has no IPv4 address box guests can reach"
            ));
        }
        Ok(addrs)
    }

    /// The per-task chain, in order: the fixed deny set, resolvers on port
    /// 53, and then either the egress proxy's `proxy` addresses and nothing
    /// else, or the operator deny set, the allowlist, the remaining private
    /// ranges, and public egress.
    pub(crate) fn chain_rules(&self, chain: &str, proxy: &[Ipv4Addr]) -> Vec<Vec<String>> {
        let rule = |parts: &[&str]| -> Vec<String> {
            ["-A", chain]
                .iter()
                .chain(parts)
                .map(|s| s.to_string())
                .collect()
        };
        let mut rules = Vec::new();
        for destination in ALWAYS_BLOCKED {
            rules.push(rule(&["-d", destination, "-j", "REJECT"]));
        }
        for resolver in &self.dns {
            let resolver = resolver.to_string();
            for protocol in ["udp", "tcp"] {
                rules.push(rule(&[
                    "-d", &resolver, "-p", protocol, "--dport", "53", "-j", "ACCEPT",
                ]));
            }
        }
        if let Some((_, port)) = &self.proxy {
            // Guests reach the internet only through the operator's proxy.
            let port = port.to_string();
            for address in proxy {
                rules.push(rule(&[
                    "-d",
                    &address.to_string(),
                    "-p",
                    "tcp",
                    "--dport",
                    &port,
                    "-j",
                    "ACCEPT",
                ]));
            }
            rules.push(rule(&["-j", "REJECT"]));
            return rules;
        }
        for destination in &self.deny {
            rules.push(rule(&["-d", &destination.to_string(), "-j", "REJECT"]));
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
    if let Some(notice) = policy.proxy_notice() {
        tracing::info!("{notice}");
    }
    if let Some(warning) = policy.public_resolver_warning() {
        tracing::warn!("{warning}");
    }
    let _ = GUEST_POLICY.set(policy);
}

pub(crate) fn guest_policy() -> &'static GuestNetworkPolicy {
    GUEST_POLICY.get_or_init(GuestNetworkPolicy::default)
}

fn is_always_blocked(ip: Ipv4Addr) -> bool {
    ALWAYS_BLOCKED
        .iter()
        .any(|net| net.parse::<Ipv4Net>().is_ok_and(|net| net.contains(&ip)))
}

/// The IPv4 entries of an outbound CIDR setting, parsed as normal user code
/// reads them.
fn ipv4_nets(conf: &Val, key: &str) -> Result<Vec<Ipv4Net>, String> {
    Ok(hot::outbound::conf_cidrs(conf, key)?
        .into_iter()
        .filter_map(|net| match net {
            ipnet::IpNet::V4(net) => Some(net),
            ipnet::IpNet::V6(_) => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(policy: &GuestNetworkPolicy) -> Vec<String> {
        proxied_rules(policy, &[])
    }

    fn proxied_rules(policy: &GuestNetworkPolicy, proxy: &[Ipv4Addr]) -> Vec<String> {
        policy
            .chain_rules("HKT-test", proxy)
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
        // The fixed deny set comes first, so no later rule can open it.
        assert!(metadata < dns && dns < private);
        assert_eq!(rules.last().unwrap(), "-j ACCEPT");
    }

    #[test]
    fn resolvers_never_open_the_fixed_deny_set() {
        for resolver in [
            "169.254.169.254",
            "169.254.169.253",
            "127.0.0.53",
            "10.88.0.1",
        ] {
            let conf = hot::val!({"box": {"guest-dns": resolver}});
            let error = GuestNetworkPolicy::from_conf(&conf).unwrap_err();
            assert!(error.contains("never"), "{resolver}: {error}");
        }
    }

    #[test]
    fn ipv6_written_ipv4_entries_apply_to_guests() {
        let conf = hot::val!({"network": {"deny-cidrs": "::ffff:10.68.0.0/112"}});
        let rules = rules(&GuestNetworkPolicy::from_conf(&conf).unwrap());
        assert!(position(&rules, "-d 10.68.0.0/16 -j REJECT") > 0);
    }

    #[tokio::test]
    async fn an_egress_proxy_is_the_only_way_out() {
        let conf = hot::val!({
            "network": {
                "egress-proxy": "http://192.168.5.10:3128",
                "allow-cidrs": "10.20.0.0/16",
            },
            "box": {"guest-dns": "10.68.0.2"},
        });
        let policy = GuestNetworkPolicy::from_conf(&conf).unwrap();
        let proxy = policy.proxy_addresses().await.unwrap();
        assert_eq!(proxy, [Ipv4Addr::new(192, 168, 5, 10)]);
        let rules = proxied_rules(&policy, &proxy);
        let accept = position(&rules, "-d 192.168.5.10 -p tcp --dport 3128 -j ACCEPT");
        assert!(position(&rules, "-d 169.254.0.0/16 -j REJECT") < accept);
        assert!(position(&rules, "-d 10.68.0.2 -p udp --dport 53 -j ACCEPT") < accept);
        assert_eq!(rules.last().unwrap(), "-j REJECT");
        assert_eq!(
            rules.iter().filter(|r| r.ends_with("ACCEPT")).count(),
            3,
            "the resolver over UDP and TCP, and the proxy: {rules:#?}"
        );
        assert!(
            !rules.iter().any(|r| r.contains("10.20.0.0/16")),
            "{rules:#?}"
        );

        // A proxy guests could never reach leaves them no way out.
        for proxy in ["http://127.0.0.1:3128", "http://[::1]:3128"] {
            let conf = hot::val!({"network": {"egress-proxy": proxy}});
            let policy = GuestNetworkPolicy::from_conf(&conf).unwrap();
            assert!(policy.proxy_addresses().await.is_err(), "{proxy}");
        }
        assert!(
            GuestNetworkPolicy::default()
                .proxy_addresses()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn startup_log_explains_proxied_guests_without_credentials() {
        let conf = hot::val!({
            "network": {"egress-proxy": "http://user:hunter2@proxy.internal:3128"},
        });
        let policy = GuestNetworkPolicy::from_conf(&conf).unwrap();
        let notice = policy.proxy_notice().unwrap();
        assert!(notice.contains("proxy.internal:3128"), "{notice}");
        assert!(notice.contains("HTTP_PROXY"), "{notice}");
        // Only Kata installs the chain, so the restriction must not read as
        // holding for every backend.
        assert!(
            notice.contains("box guests on the Kata backend can connect only to"),
            "{notice}"
        );
        let warning = policy.public_resolver_warning().unwrap();
        assert!(warning.contains("on the Kata backend"), "{warning}");
        assert!(warning.contains("1.1.1.1, 8.8.8.8"), "{warning}");
        assert!(warning.contains("box.guest-dns"), "{warning}");
        assert!(!format!("{notice}{warning}").contains("hunter2"));

        // The operator's own resolvers can record what guests look up.
        let own_dns = conf.merge(&hot::val!({"box": {"guest-dns": "10.68.0.2"}}));
        let policy = GuestNetworkPolicy::from_conf(&own_dns).unwrap();
        assert!(policy.proxy_notice().is_some());
        assert!(policy.public_resolver_warning().is_none());

        // Without a proxy, guests connect directly and nothing needs explaining.
        let policy = GuestNetworkPolicy::default();
        assert!(policy.proxy_notice().is_none());
        assert!(policy.public_resolver_warning().is_none());
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
        // A private resolver is reachable on port 53 only, even inside the
        // operator's deny set.
        assert!(
            position(&rules, "-d 10.68.0.2 -p udp --dport 53 -j ACCEPT")
                < position(&rules, "-d 10.68.0.0/16 -j REJECT")
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
