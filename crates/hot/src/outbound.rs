//! Shared outbound HTTP client setup and destination validation for
//! user-controlled network clients.

use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use url::Url;

/// A reqwest client builder that trusts the bundled Mozilla root certificates.
/// reqwest 0.13 otherwise verifies against the OS trust store, which minimal
/// containers may lack; this keeps reqwest 0.12's `rustls-tls` behavior.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_certs_only(bundled_root_certificates().iter().cloned())
}

/// Like `reqwest::Client::new()`, with [`client_builder`]'s roots. Panics if the
/// TLS backend cannot be initialized, as `Client::new()` does.
pub fn client() -> reqwest::Client {
    client_builder()
        .build()
        .expect("failed to build HTTP client with bundled root certificates")
}

fn bundled_root_certificates() -> &'static [reqwest::Certificate] {
    static ROOTS: LazyLock<Vec<reqwest::Certificate>> = LazyLock::new(|| {
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok())
            .collect()
    });
    &ROOTS
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestinationPolicy {
    /// No restriction: trusted callers and development without isolation.
    AllowPrivate,
    /// Public destinations only.
    PublicOnly,
    /// Public destinations, plus the operator's private allowlist, minus the
    /// fixed deny set and the installation's own endpoints.
    Restricted(Arc<OutboundRules>),
}

impl DestinationPolicy {
    pub fn from_vm(vm: &crate::lang::runtime::vm::VirtualMachine) -> Self {
        if vm.is_isolation_enabled() {
            Self::for_isolated(vm.get_conf(), LegacyAllowPrivate::Honor)
        } else {
            Self::AllowPrivate
        }
    }

    /// The policy for code under isolation. Invalid rules fail closed to
    /// public destinations; services reject them at startup with
    /// [`validate_user_code_conf`].
    fn for_isolated(conf: &crate::val::Val, legacy: LegacyAllowPrivate) -> Self {
        match OutboundRules::parse(conf, legacy) {
            Ok(Some(rules)) => Self::Restricted(Arc::new(rules)),
            Ok(None) => Self::PublicOnly,
            Err(error) => {
                tracing::error!("Ignoring invalid outbound network rules: {error}");
                Self::PublicOnly
            }
        }
    }

    pub fn validate_ip(&self, ip: IpAddr) -> Result<(), String> {
        self.check(None, ip)
    }

    fn check(&self, host: Option<&str>, ip: IpAddr) -> Result<(), String> {
        let allowed = match self {
            Self::AllowPrivate => true,
            Self::PublicOnly => is_public_destination(ip),
            Self::Restricted(rules) => rules.allows(host, ip),
        };
        if allowed {
            Ok(())
        } else {
            Err(format!(
                "outbound connection to non-public address {ip} is blocked"
            ))
        }
    }

    pub async fn resolve_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
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
            self.check(Some(host), addr.ip())?;
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

    /// Alert destinations are server-side configuration, not VM calls. Hosted
    /// runtimes always require public destinations; local/self-hosted operators
    /// must explicitly opt in before private destinations are reachable.
    pub fn for_alert_delivery(conf: &crate::val::Val) -> Self {
        if !conf.get_bool_or_default("engine.isolation", false)
            && conf.get_bool_or_default("network.allow-private", false)
        {
            Self::AllowPrivate
        } else {
            // Isolated alert delivery has never honored `allow-private`; only
            // the explicit allowlists open private destinations.
            Self::for_isolated(conf, LegacyAllowPrivate::Ignore)
        }
    }

    /// Resolve and validate immediately before delivery, then pin the validated
    /// addresses into a no-redirect client. This closes DNS rebinding and
    /// redirect-to-private-address gaps.
    pub async fn pinned_http_client(
        &self,
        url: &Url,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<reqwest::Client, String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("outbound URL scheme must be http or https".to_string());
        }
        let host = url
            .host_str()
            .ok_or_else(|| "outbound URL must include a host".to_string())?;
        let addrs = self.resolve_url(url).await?;

        client_builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addrs)
            .build()
            .map_err(|e| format!("failed to build pinned HTTP client: {e}"))
    }
}

/// Operator rules for user code that may reach private destinations:
/// `network.allow-cidrs`, `network.allow-hosts`, and `network.deny-cidrs`.
/// Each takes a list or a comma-separated string. The legacy
/// `network.allow-private = true` allows every private address. None of them
/// can reach the [fixed deny set](is_always_denied).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OutboundRules {
    allow_cidrs: Vec<IpNet>,
    /// Exact names, or `*.example.internal` for any subdomain.
    allow_hosts: Vec<String>,
    /// The installation's own endpoints: database, cache, ingress, and agent.
    deny_cidrs: Vec<IpNet>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LegacyAllowPrivate {
    Honor,
    Ignore,
}

impl OutboundRules {
    /// `None` when nothing is configured, which means public destinations only.
    pub fn from_conf(conf: &crate::val::Val) -> Result<Option<Self>, String> {
        Self::parse(conf, LegacyAllowPrivate::Honor)
    }

    fn parse(conf: &crate::val::Val, legacy: LegacyAllowPrivate) -> Result<Option<Self>, String> {
        let mut allow_cidrs = cidrs(conf, "network.allow-cidrs")?;
        if legacy == LegacyAllowPrivate::Honor
            && conf.get_bool_or_default("network.allow-private", false)
        {
            allow_cidrs.extend(["0.0.0.0/0", "::/0"].map(|net| net.parse::<IpNet>().unwrap()));
        }
        let allow_hosts = list(conf, "network.allow-hosts")
            .into_iter()
            .map(|host| {
                let host = host.to_ascii_lowercase();
                let name = host.strip_prefix("*.").unwrap_or(&host);
                if name.is_empty() || name.contains(['*', '/', ':']) {
                    Err(format!(
                        "network.allow-hosts entry {host:?} is not a hostname"
                    ))
                } else {
                    Ok(host)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let deny_cidrs = cidrs(conf, "network.deny-cidrs")?;
        let rules = Self {
            allow_cidrs,
            allow_hosts,
            deny_cidrs,
        };
        Ok((rules != Self::default()).then_some(rules))
    }

    fn allows(&self, host: Option<&str>, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if is_always_denied(ip) || self.deny_cidrs.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        is_public_destination(ip)
            || self.allow_cidrs.iter().any(|net| net.contains(&ip))
            || host.is_some_and(|host| self.allows_host(host))
    }

    fn allows_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.allow_hosts
            .iter()
            .any(|allowed| match allowed.strip_prefix("*.") {
                Some(domain) => host
                    .strip_suffix(domain)
                    .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1),
                None => *allowed == host,
            })
    }
}

fn list(conf: &crate::val::Val, key: &str) -> Vec<String> {
    let entries: Vec<String> = match conf.get(key) {
        Some(crate::val::Val::Vec(items)) => items
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

fn cidrs(conf: &crate::val::Val, key: &str) -> Result<Vec<IpNet>, String> {
    list(conf, key)
        .into_iter()
        .map(|entry| {
            entry
                .parse::<IpNet>()
                .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                .map(|net| net.trunc())
                .map_err(|_| format!("{key} entry {entry:?} is not an IP address or CIDR"))
        })
        .collect()
}

/// Addresses user code never reaches, whatever the operator allows: the
/// machine itself, link-local services including every cloud metadata
/// endpoint, and addresses that are never a single remote host.
pub fn is_always_denied(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_link_local() // 169.254.169.254, ECS 169.254.170.2
                || ip.is_broadcast()
                || ip.is_multicast()
                || ipv4_in(ip, [0, 0, 0, 0], 8)
                || ipv4_in(ip, [240, 0, 0, 0], 4)
        }
        IpAddr::V6(ip) => {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ipv6_in(ip, [0xfe80, 0, 0, 0, 0, 0, 0, 0], 10)
                // Metadata over IPv6: AWS IMDS and Google Compute Engine.
                || ip == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
                || ip == Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

/// Compare IPv4 destinations embedded in IPv6 addresses as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => embedded_ipv4(v6).map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Startup check for services that run user code. Self-hosted production
/// installations always isolate it, and outbound rules must parse.
pub fn validate_user_code_conf(conf: &crate::val::Val) -> Result<(), String> {
    check_user_code_conf(conf, &crate::env::get_env())
}

fn check_user_code_conf(conf: &crate::val::Val, env: &str) -> Result<(), String> {
    if crate::product::is_self_host(conf)
        && env == "production"
        && !conf.get_bool_or_default("engine.isolation", false)
    {
        return Err(
            "engine.isolation must be true for self-hosted production services that run user code"
                .into(),
        );
    }
    OutboundRules::from_conf(conf).map(|_| ())
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
    fn bundled_root_certificates_all_parse() {
        let roots = bundled_root_certificates();
        assert_eq!(roots.len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        assert!(roots.len() > 100, "only {} roots", roots.len());
    }

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

    fn rules(conf: crate::val::Val) -> DestinationPolicy {
        DestinationPolicy::for_isolated(
            &crate::val!({ "network": conf }),
            LegacyAllowPrivate::Honor,
        )
    }

    fn reaches(policy: &DestinationPolicy, host: Option<&str>, ip: &str) -> bool {
        policy.check(host, ip.parse().unwrap()).is_ok()
    }

    #[test]
    fn allowlisted_private_destinations_never_include_the_deny_set() {
        let policy = rules(crate::val!({
            "allow-cidrs": ["10.20.0.0/16", "0.0.0.0/0", "::/0"],
            "deny-cidrs": "10.20.0.5, 2600:1f18::/64",
        }));
        assert!(reaches(&policy, None, "10.20.3.4"));
        assert!(reaches(&policy, None, "8.8.8.8"));
        for ip in [
            "10.20.0.5",       // the installation's database
            "2600:1f18::10",   // a denied public range
            "169.254.169.254", // AWS, GCP, and Azure metadata
            "169.254.170.2",   // ECS task metadata
            "fd00:ec2::254",   // AWS IMDS over IPv6
            "fd20:ce::254",    // Google metadata over IPv6
            "127.0.0.1",
            "::1",
            "::ffff:169.254.169.254", // IPv4-mapped metadata
            "::ffff:10.20.0.5",       // IPv4-mapped denied address
            "fe80::1",
            "0.0.0.0",
        ] {
            assert!(!reaches(&policy, None, ip), "{ip} must stay blocked");
        }
    }

    #[test]
    fn allowlists_open_only_the_listed_private_destinations() {
        let policy = rules(crate::val!({
            "allow-cidrs": ["192.168.5.10"],
            "allow-hosts": ["api.corp.example", "*.svc.internal"],
        }));
        assert!(reaches(&policy, None, "192.168.5.10"));
        assert!(!reaches(&policy, None, "192.168.5.11"));
        assert!(reaches(&policy, Some("API.corp.example."), "10.1.1.1"));
        assert!(reaches(&policy, Some("billing.svc.internal"), "10.1.1.2"));
        assert!(!reaches(&policy, Some("svc.internal"), "10.1.1.2"));
        assert!(!reaches(&policy, Some("evilsvc.internal"), "10.1.1.2"));
        assert!(!reaches(&policy, Some("other.corp.example"), "10.1.1.1"));
        // A name is never a way into the deny set.
        assert!(!reaches(
            &policy,
            Some("api.corp.example"),
            "169.254.169.254"
        ));
        assert!(!reaches(&policy, Some("api.corp.example"), "127.0.0.1"));
    }

    #[test]
    fn legacy_allow_private_keeps_metadata_blocked() {
        let policy = rules(crate::val!({"allow-private": true}));
        assert!(reaches(&policy, None, "10.0.0.1"));
        assert!(reaches(&policy, None, "fd12::1"));
        assert!(!reaches(&policy, None, "169.254.169.254"));
        assert!(!reaches(&policy, None, "127.0.0.1"));
    }

    #[test]
    fn rules_parse_strictly_and_default_to_public_only() {
        assert_eq!(rules(crate::val!({})), DestinationPolicy::PublicOnly);
        let invalid = crate::val!({"network": {"allow-cidrs": "10.0.0.0/33"}});
        assert!(OutboundRules::from_conf(&invalid).is_err());
        assert_eq!(
            DestinationPolicy::for_isolated(&invalid, LegacyAllowPrivate::Honor),
            DestinationPolicy::PublicOnly,
            "invalid rules fail closed"
        );
        let invalid = crate::val!({"network": {"allow-hosts": ["*"]}});
        assert!(OutboundRules::from_conf(&invalid).is_err());
    }

    #[test]
    fn self_hosted_production_requires_isolation() {
        let self_host = |isolation: bool| {
            crate::val!({
                "product": {"experience": "self-host"},
                "engine": {"isolation": isolation},
            })
        };
        assert!(check_user_code_conf(&self_host(false), "production").is_err());
        assert!(check_user_code_conf(&self_host(true), "production").is_ok());
        assert!(check_user_code_conf(&self_host(false), "development").is_ok());
        assert!(check_user_code_conf(&crate::val!({}), "production").is_ok());
        let bad_rules = self_host(true).merge(&crate::val!({"network": {"deny-cidrs": "nope"}}));
        assert!(check_user_code_conf(&bad_rules, "production").is_err());
    }

    #[test]
    fn alert_private_destinations_require_explicit_self_hosted_opt_in() {
        assert_eq!(
            DestinationPolicy::for_alert_delivery(&crate::val!({})),
            DestinationPolicy::PublicOnly
        );
        assert_eq!(
            DestinationPolicy::for_alert_delivery(&crate::val!({
                "network": {"allow-private": true},
            })),
            DestinationPolicy::AllowPrivate
        );
        assert_eq!(
            DestinationPolicy::for_alert_delivery(&crate::val!({
                "engine": {"isolation": true},
                "network": {"allow-private": true},
            })),
            DestinationPolicy::PublicOnly
        );
    }
}
