//! Route-conflict preflight (wg-client.md §6 "Before teardown ... check
//! candidate routes against the local routing table"): before tearing
//! down the working interface, reject a candidate configuration whose
//! peer routes would overlap another interface's existing non-default
//! route, or would capture the storage endpoint, this tunnel's own
//! configured DNS resolver, or another peer's transport endpoint -- any
//! of which would cut this node off from what it needs to recover. Keep
//! the current configuration untouched on any preflight error.
//!
//! Bounded to its own 20s budget, separate from the discovery/download
//! and local-operation budgets (wg-client.md §6 "Retry policy").

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use ipnet::Ipv4Net;
use wg_common::render::ParsedConfig;

use crate::netlink;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);

pub async fn check(
    cfg: &ParsedConfig,
    storage_endpoint_host: &str,
    managed_iface: &str,
) -> Result<(), String> {
    tokio::time::timeout(
        PREFLIGHT_TIMEOUT,
        check_inner(cfg, storage_endpoint_host, managed_iface),
    )
    .await
    .map_err(|_| "preflight timed out".to_string())?
}

async fn check_inner(
    cfg: &ParsedConfig,
    storage_endpoint_host: &str,
    managed_iface: &str,
) -> Result<(), String> {
    let mut protected: Vec<(Ipv4Addr, String)> = Vec::new();

    for ip in resolve_ipv4(storage_endpoint_host).await? {
        protected.push((ip, "the storage endpoint".to_string()));
    }
    if let Some(IpAddr::V4(dns)) = cfg.interface.dns {
        protected.push((dns, "this tunnel's configured DNS resolver".to_string()));
    }
    for peer in &cfg.peers {
        let Some(endpoint) = &peer.endpoint else {
            continue;
        };
        let Some(host) = host_part(endpoint) else {
            continue; // an IPv6 literal endpoint can't be captured by an IPv4 route
        };
        for ip in resolve_ipv4(&host).await? {
            protected.push((
                ip,
                format!(
                    "peer {}'s transport endpoint",
                    &peer.public_key[..8.min(peer.public_key.len())]
                ),
            ));
        }
    }

    let candidate_routes: Vec<Ipv4Net> = cfg
        .peers
        .iter()
        .flat_map(|p| p.allowed_ips.iter().copied())
        .collect();

    for route in &candidate_routes {
        for (addr, what) in &protected {
            if route.contains(addr) {
                return Err(format!(
                    "candidate route {route} would capture {what} ({addr})"
                ));
            }
        }
    }

    let existing_routes =
        netlink::list_ipv4_routes().map_err(|e| format!("listing routing table: {e}"))?;
    let managed_index = netlink::interface_index(managed_iface);

    for (existing_net, oif) in existing_routes {
        if existing_net.prefix_len() == 0 {
            continue; // an ordinary underlay default route is not itself a conflict
        }
        if managed_index == Some(oif) {
            continue; // this interface's own current routes, about to be replaced
        }
        for route in &candidate_routes {
            if nets_overlap(route, &existing_net) {
                let other = netlink::if_name_from_index(oif).unwrap_or_else(|| oif.to_string());
                return Err(format!(
                    "candidate route {route} overlaps route {existing_net} \
                     already installed on interface {other}"
                ));
            }
        }
    }

    Ok(())
}

fn nets_overlap(a: &Ipv4Net, b: &Ipv4Net) -> bool {
    let (a_lo, a_hi) = (u32::from(a.network()), u32::from(a.broadcast()));
    let (b_lo, b_hi) = (u32::from(b.network()), u32::from(b.broadcast()));
    a_lo <= b_hi && b_lo <= a_hi
}

/// Extract the host portion of a `host:port` endpoint string (already
/// syntax-validated by `wg_common`'s parser). Returns `None` for an IPv6
/// literal host (`[...]:port`): candidate routes here are IPv4-only, so
/// such an endpoint can't be captured by one anyway.
fn host_part(endpoint: &str) -> Option<String> {
    if endpoint.starts_with('[') {
        return None;
    }
    endpoint.rsplit_once(':').map(|(host, _)| host.to_string())
}

async fn resolve_ipv4(host: &str) -> Result<Vec<Ipv4Addr>, String> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| format!("resolving {host:?}: {e}"))?;
    Ok(addrs
        .filter_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nets_overlap_detects_containment() {
        let a: Ipv4Net = "10.0.0.0/24".parse().unwrap();
        let b: Ipv4Net = "10.0.0.128/25".parse().unwrap();
        assert!(nets_overlap(&a, &b));
    }

    #[test]
    fn nets_overlap_false_for_disjoint() {
        let a: Ipv4Net = "10.0.0.0/24".parse().unwrap();
        let b: Ipv4Net = "10.0.1.0/24".parse().unwrap();
        assert!(!nets_overlap(&a, &b));
    }

    #[test]
    fn host_part_extracts_hostname() {
        assert_eq!(
            host_part("example.com:51820"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn host_part_extracts_ipv4() {
        assert_eq!(
            host_part("203.0.113.10:51820"),
            Some("203.0.113.10".to_string())
        );
    }

    #[test]
    fn host_part_skips_ipv6_literal() {
        assert_eq!(host_part("[2001:db8::1]:51820"), None);
    }
}
