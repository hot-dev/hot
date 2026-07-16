//! Network helpers shared by the hot servers.

use std::io;
use std::net::{IpAddr, SocketAddr};

use tokio::net::{TcpListener, lookup_host};

/// Bind listeners for every address `host` resolves to.
///
/// A literal IP binds exactly that address. A hostname (e.g. "localhost")
/// binds one listener per resolved address: on many systems "localhost"
/// resolves to both 127.0.0.1 and ::1, and binding only the resolver's first
/// answer strands clients that dial a single address per family (the JDK,
/// .NET) on whichever family was skipped. Bind failures for individual
/// addresses are tolerated (e.g. hosts with IPv6 loopback disabled) as long
/// as at least one listener binds.
pub async fn bind_all(host: &str, port: u16) -> io::Result<Vec<TcpListener>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![TcpListener::bind((ip, port)).await?]);
    }

    let mut addrs: Vec<SocketAddr> = lookup_host((host, port)).await?.collect();
    addrs.sort();
    addrs.dedup();

    let mut listeners = Vec::new();
    let mut last_error = None;
    for addr in addrs {
        match TcpListener::bind(addr).await {
            Ok(listener) => listeners.push(listener),
            Err(error) => {
                tracing::warn!("hot.dev: could not bind {}: {}", addr, error);
                last_error = Some(error);
            }
        }
    }

    if listeners.is_empty() {
        return Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("'{host}' resolved to no addresses"),
            )
        }));
    }
    Ok(listeners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_ip_binds_exactly_one_listener() {
        let listeners = bind_all("127.0.0.1", 0).await.unwrap();
        assert_eq!(listeners.len(), 1);
        assert!(listeners[0].local_addr().unwrap().is_ipv4());
    }

    #[tokio::test]
    async fn hostname_binds_every_resolved_address() {
        let listeners = bind_all("localhost", 0).await.unwrap();
        assert!(!listeners.is_empty());
        // On dual-stack hosts, "localhost" must yield both loopbacks; on
        // v4-only or v6-only hosts, one listener is correct.
        let families: std::collections::HashSet<bool> = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap().is_ipv4())
            .collect();
        assert_eq!(families.len(), listeners.len().min(2));
    }
}
