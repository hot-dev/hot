//! Portable client-IP resolution with opt-in trusted-proxy validation.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName},
    middleware::Next,
    response::Response,
};
use hot::val::Val;
use ipnet::IpNet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Once};

static LEGACY_FORWARDED_WARNING: Once = Once::new();

/// Resolved client identity shared by access logging and `hot.request.ip`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIp(pub String);

#[derive(Clone, Debug)]
pub struct ClientIpPolicy {
    trusted_proxy: bool,
    trusted_proxies: Vec<IpNet>,
    forwarded_header: HeaderName,
}

impl ClientIpPolicy {
    pub fn from_conf(conf: &Val) -> Result<Self, String> {
        let trusted_proxy = conf.get_bool_or_default("network.client-ip.trusted-proxy", false);
        let forwarded_header = if trusted_proxy {
            conf.get_str_or_default("network.client-ip.header", "x-forwarded-for")
                .parse::<HeaderName>()
                .map_err(|e| format!("network.client-ip.header is invalid: {e}"))?
        } else {
            HeaderName::from_static("x-forwarded-for")
        };

        let trusted_proxies = if trusted_proxy {
            parse_trusted_proxies(conf.get("network.client-ip.trusted-proxies"))?
        } else {
            Vec::new()
        };

        if trusted_proxy && trusted_proxies.is_empty() {
            return Err(
                "network.client-ip.trusted-proxy requires at least one trusted-proxies CIDR"
                    .to_string(),
            );
        }

        Ok(Self {
            trusted_proxy,
            trusted_proxies,
            forwarded_header,
        })
    }

    pub fn resolve(&self, headers: &HeaderMap, peer: SocketAddr) -> Option<ClientIp> {
        if !self.trusted_proxy {
            return legacy_forwarded_value(headers).map(ClientIp);
        }

        let peer_ip = peer.ip();
        if !self.is_trusted(peer_ip) {
            return Some(ClientIp(peer_ip.to_string()));
        }

        let Some(value) = headers
            .get(&self.forwarded_header)
            .and_then(|value| value.to_str().ok())
        else {
            return Some(ClientIp(peer_ip.to_string()));
        };

        let mut chain = Vec::new();
        for value in value.split(',') {
            let Ok(ip) = value.trim().parse::<IpAddr>() else {
                return Some(ClientIp(peer_ip.to_string()));
            };
            chain.push(ip);
        }
        if chain.is_empty() {
            return Some(ClientIp(peer_ip.to_string()));
        }

        chain.push(peer_ip);
        let client = chain
            .iter()
            .rev()
            .copied()
            .find(|ip| !self.is_trusted(*ip))
            .unwrap_or(chain[0]);
        Some(ClientIp(client.to_string()))
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted_proxies
            .iter()
            .any(|network| network.contains(&ip))
    }
}

fn parse_trusted_proxies(value: Option<Val>) -> Result<Vec<IpNet>, String> {
    let values = match value {
        Some(Val::Vec(values)) => values
            .iter()
            .map(|value| {
                let Val::Str(value) = value else {
                    return Err(
                        "network.client-ip.trusted-proxies entries must be strings".to_string()
                    );
                };
                Ok(value.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(Val::Str(value)) => value.split(',').map(str::to_string).collect(),
        Some(Val::Null) | None => Vec::new(),
        Some(_) => {
            return Err(
                "network.client-ip.trusted-proxies must be a vector or comma-delimited string"
                    .to_string(),
            );
        }
    };

    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value.parse::<IpNet>().map_err(|e| {
                format!("invalid network.client-ip.trusted-proxies entry '{value}': {e}")
            })
        })
        .collect()
}

fn legacy_forwarded_value(headers: &HeaderMap) -> Option<String> {
    ["x-forwarded-for", "x-real-ip"]
        .into_iter()
        .find_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.split(',').next().unwrap_or("").trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

pub async fn client_ip_middleware(
    State(policy): State<Arc<ClientIpPolicy>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    if !policy.trusted_proxy
        && (request.headers().contains_key("x-forwarded-for")
            || request.headers().contains_key("x-real-ip"))
    {
        LEGACY_FORWARDED_WARNING.call_once(|| {
            tracing::warn!(
                "Forwarded client IP headers are using compatibility mode. Configure network.client-ip.trusted-proxy and trusted-proxies to validate proxy identity."
            );
        });
    }
    if let Some(client_ip) = policy.resolve(request.headers(), peer) {
        request.extensions_mut().insert(client_ip);
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 443)
    }

    #[test]
    fn disabled_policy_preserves_legacy_forwarded_behavior() {
        let policy = ClientIpPolicy::from_conf(&hot::val!({})).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.7, 10.0.0.8".parse().unwrap());

        assert_eq!(
            policy.resolve(&headers, peer("127.0.0.1")),
            Some(ClientIp("198.51.100.7".to_string()))
        );
        assert_eq!(policy.resolve(&HeaderMap::new(), peer("127.0.0.1")), None);
    }

    #[test]
    fn enabled_policy_ignores_headers_from_untrusted_peers() {
        let policy = ClientIpPolicy::from_conf(&hot::val!({
            "network": {
                "client-ip": {
                    "trusted-proxy": true,
                    "trusted-proxies": ["127.0.0.1/32"],
                },
            },
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.7".parse().unwrap());

        assert_eq!(
            policy.resolve(&headers, peer("203.0.113.9")),
            Some(ClientIp("203.0.113.9".to_string()))
        );
    }

    #[test]
    fn enabled_policy_walks_the_chain_from_the_trusted_edge() {
        let policy = ClientIpPolicy::from_conf(&hot::val!({
            "network": {
                "client-ip": {
                    "trusted-proxy": true,
                    "trusted-proxies": [
                        "127.0.0.1/32",
                        "10.20.1.0/24",
                    ],
                },
            },
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "192.0.2.123, 198.51.100.7, 10.20.1.8".parse().unwrap(),
        );

        assert_eq!(
            policy.resolve(&headers, peer("127.0.0.1")),
            Some(ClientIp("198.51.100.7".to_string()))
        );
    }

    #[test]
    fn enabled_policy_requires_trusted_proxy_cidrs() {
        let error = ClientIpPolicy::from_conf(&hot::val!({
            "network": {
                "client-ip": {
                    "trusted-proxy": true,
                },
            },
        }))
        .unwrap_err();

        assert!(error.contains("requires at least one"));
    }

    #[test]
    fn enabled_policy_accepts_environment_style_cidr_string() {
        let policy = ClientIpPolicy::from_conf(&hot::val!({
            "network": {
                "client-ip": {
                    "trusted-proxy": true,
                    "trusted-proxies": "127.0.0.1/32, 10.0.0.0/8",
                },
            },
        }))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.7, 10.0.0.8".parse().unwrap());

        assert_eq!(
            policy.resolve(&headers, peer("127.0.0.1")),
            Some(ClientIp("198.51.100.7".to_string()))
        );
    }
}
