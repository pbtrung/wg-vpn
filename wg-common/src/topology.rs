//! Input topology config schema, validation, and the peer-selection
//! algorithm (wg-server.md §5–7).

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use ipnet::Ipv4Net;
use serde::Deserialize;
use thiserror::Error;

use crate::hostname;
use crate::limits;
use crate::strict_json;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct R2Config {
    pub endpoint: String,
    pub read_write_access_key_id: String,
    pub read_write_secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    pub region: String,
    pub bucket: String,
}

fn default_listen_port() -> u16 {
    limits::DEFAULT_LISTEN_PORT
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct WgConfig {
    pub tunnel_address: String,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub dns: Option<String>,
    #[serde(default)]
    pub mtu: Option<u16>,
    #[serde(default)]
    pub extra_allowed_ips: Vec<String>,
    #[serde(default)]
    pub persistent_keepalive: Option<u16>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub hostname: String,
    pub wg_config: WgConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TopologyConfig {
    pub r2_config: R2Config,
    pub nodes: Vec<NodeConfig>,
    pub master: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("duplicate JSON key: {0}")]
    DuplicateJsonKey(String),
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("invalid hostname {hostname:?}: {source}")]
    InvalidHostname {
        hostname: String,
        source: hostname::HostnameError,
    },
    #[error("duplicate hostname: {0}")]
    DuplicateHostname(String),
    #[error("nodes must be non-empty")]
    EmptyNodes(),
    #[error("too many nodes: {0} exceeds the limit of {max}", max = limits::MAX_NODES)]
    TooManyNodes(usize),
    #[error("master {0:?} is not a known hostname")]
    UnknownMaster(String),
    #[error("duplicate entry in master list: {0}")]
    DuplicateMaster(String),
    #[error("node {0:?}: tunnel_address must be an IPv4 unicast /32, got {1:?}")]
    InvalidTunnelAddress(String, String),
    #[error("duplicate tunnel_address {addr:?} used by both {a:?} and {b:?}")]
    DuplicateTunnelAddress { addr: String, a: String, b: String },
    #[error("node {0:?}: listen_port must be 1-65535")]
    InvalidListenPort(String),
    #[error("node {0:?}: invalid endpoint {1:?}: {2}")]
    InvalidEndpoint(String, String, &'static str),
    #[error("node {0:?}: invalid dns {1:?}: must be a single IP address")]
    InvalidDns(String, String),
    #[error("node {0:?}: mtu {1} out of range ({min}-65535)", min = limits::MIN_MTU)]
    InvalidMtu(String, u16),
    #[error(
        "node {0:?}: extra_allowed_ips entry {1:?} is not a canonical IPv4 network (host bits set)"
    )]
    NonCanonicalRoute(String, String),
    #[error("node {0:?}: extra_allowed_ips entry {1:?} is invalid: {2}")]
    InvalidRoute(String, String, &'static str),
    #[error("node {0:?}: extra_allowed_ips must not include a default route (0.0.0.0/0)")]
    DefaultRouteRejected(String),
    #[error("routes {a_route:?} (from {a_host:?}) and {b_route:?} (from {b_host:?}) overlap")]
    OverlappingRoutes {
        a_host: String,
        a_route: String,
        b_host: String,
        b_route: String,
    },
    #[error("route {route:?} (from {route_host:?}) covers {node:?}'s tunnel address {addr:?}")]
    RouteCoversTunnelAddress {
        route_host: String,
        route: String,
        node: String,
        addr: String,
    },
    #[error("edge between {0:?} and {1:?} has no endpoint configured on either side")]
    EdgeMissingEndpoint(String, String),
    #[error("invalid r2_config.endpoint {0:?}: {1}")]
    InvalidR2Endpoint(String, &'static str),
    #[error("invalid r2_config credentials: {0}")]
    InvalidR2Credentials(&'static str),
}

/// Validate an `r2_config.endpoint` URL, shared by `wg-server`'s
/// `R2Config` and `wg-client`'s `R2ReadConfig` (wg-server.md §9,
/// wg-client.md §4/§10: reject URL userinfo, query strings, and
/// fragments). HTTPS-only enforcement is deliberately not included here:
/// `docker-tests/` talks to local MinIO over plain HTTP by design (see
/// this repo's CLAUDE.md "Known simplifications" and wg-server.md's
/// "the SDK permits HTTP endpoints" caveat) — closing that gap needs a
/// TLS-enabled test harness, not just a stricter check here.
pub fn validate_r2_endpoint(s: &str) -> Result<(), &'static str> {
    let url = url::Url::parse(s).map_err(|_| "not a valid URL")?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("scheme must be http or https");
    }
    if url.host_str().is_none() {
        return Err("URL must have a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not contain userinfo");
    }
    if url.query().is_some() {
        return Err("URL must not contain a query string");
    }
    if url.fragment().is_some() {
        return Err("URL must not contain a fragment");
    }
    Ok(())
}

/// Extract the host from an `r2_config.endpoint` URL already accepted by
/// [`validate_r2_endpoint`], for `wg-client`'s route-conflict preflight
/// (wg-client.md §6): the storage endpoint's resolved address must not be
/// captured by a candidate peer route.
pub fn r2_endpoint_host(s: &str) -> Option<String> {
    url::Url::parse(s).ok()?.host_str().map(str::to_string)
}

/// Reject blank R2 credential fields, shared by `wg-server`'s `R2Config`
/// and `wg-client`'s `R2ReadConfig`. A present-but-empty `session_token`
/// is rejected too (wg-client.md §4: "when present it must be nonempty").
pub fn validate_r2_credentials(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
) -> Result<(), &'static str> {
    if access_key_id.is_empty() {
        return Err("access key id must not be empty");
    }
    if secret_access_key.is_empty() {
        return Err("secret access key must not be empty");
    }
    if session_token == Some("") {
        return Err("session token, if present, must not be empty");
    }
    Ok(())
}

/// Parse (with strict duplicate-key rejection) and deserialize a topology
/// config JSON document.
pub fn parse(text: &str) -> Result<TopologyConfig, ValidationError> {
    strict_json::check_no_duplicate_keys(text)
        .map_err(|e| ValidationError::DuplicateJsonKey(e.to_string()))?;
    serde_json::from_str(text).map_err(|e| ValidationError::InvalidJson(e.to_string()))
}

#[derive(Debug, Clone)]
pub struct ValidatedNode {
    pub hostname: String,
    pub tunnel_address: std::net::Ipv4Addr,
    pub listen_port: u16,
    pub endpoint: Option<String>,
    pub dns: Option<IpAddr>,
    pub mtu: Option<u16>,
    pub extra_allowed_ips: Vec<Ipv4Net>,
    /// `None` = unset (renderer applies the endpoint-dependent default);
    /// `Some(0)` = explicitly disabled; `Some(n)` = explicitly enabled.
    pub persistent_keepalive: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ValidatedTopology {
    /// Sorted by hostname for deterministic downstream iteration/rendering.
    pub nodes: Vec<ValidatedNode>,
    pub masters: BTreeSet<String>,
}

impl ValidatedTopology {
    pub fn node(&self, hostname: &str) -> Option<&ValidatedNode> {
        self.nodes.iter().find(|n| n.hostname == hostname)
    }

    /// The hub-and-spoke-with-meshed-hubs edge set (wg-server.md §7):
    /// masters fully mesh with each other and reach every spoke; spokes
    /// never connect directly to each other.
    pub fn edges(&self) -> BTreeSet<(String, String)> {
        compute_edges(
            &self
                .nodes
                .iter()
                .map(|n| n.hostname.clone())
                .collect::<Vec<_>>(),
            &self.masters,
        )
    }
}

pub(crate) fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

pub fn compute_edges(
    hostnames: &[String],
    masters: &BTreeSet<String>,
) -> BTreeSet<(String, String)> {
    let master_list: Vec<&String> = hostnames.iter().filter(|h| masters.contains(*h)).collect();
    let spoke_list: Vec<&String> = hostnames.iter().filter(|h| !masters.contains(*h)).collect();
    let mut edges = BTreeSet::new();
    for (i, a) in master_list.iter().enumerate() {
        for b in master_list.iter().skip(i + 1) {
            edges.insert(canonical_pair(a, b));
        }
    }
    for m in &master_list {
        for s in &spoke_list {
            edges.insert(canonical_pair(m, s));
        }
    }
    edges
}

fn parse_canonical_ipv4_net(s: &str) -> Result<Ipv4Net, &'static str> {
    let net: Ipv4Net = s.parse().map_err(|_| "not a valid IPv4 CIDR")?;
    if net.addr() != net.network() {
        return Err("host bits set (not the canonical network address)");
    }
    Ok(net)
}

fn net_range(net: &Ipv4Net) -> (u32, u32) {
    (u32::from(net.network()), u32::from(net.broadcast()))
}

fn ranges_overlap(a: (u32, u32), b: (u32, u32)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// Validate an `Endpoint =` value: `host:port`, `dns-name:port`, or
/// `[ipv6]:port`, port in 1-65535. DNS resolution/reachability is a
/// deployment-time concern (§7), not checked here.
pub(crate) fn validate_endpoint_syntax(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("endpoint must not be empty");
    }
    let (host, port_str) = if let Some(rest) = s.strip_prefix('[') {
        let (addr, after) = rest.split_once(']').ok_or("unterminated IPv6 bracket")?;
        addr.parse::<std::net::Ipv6Addr>()
            .map_err(|_| "invalid IPv6 address")?;
        let port_str = after
            .strip_prefix(':')
            .ok_or("missing port after IPv6 bracket")?;
        (addr, port_str)
    } else {
        s.rsplit_once(':').ok_or("missing :port")?
    };
    if host.is_empty() || host.len() > 253 {
        return Err("invalid host length");
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("host contains whitespace/control characters");
    }
    let port: u16 = port_str.parse().map_err(|_| "invalid port")?;
    if port == 0 {
        return Err("port must be 1-65535");
    }
    Ok(())
}

pub fn validate(
    config: &TopologyConfig,
) -> Result<(ValidatedTopology, Vec<String>), ValidationError> {
    let mut warnings = Vec::new();

    validate_r2_endpoint(&config.r2_config.endpoint)
        .map_err(|e| ValidationError::InvalidR2Endpoint(config.r2_config.endpoint.clone(), e))?;
    validate_r2_credentials(
        &config.r2_config.read_write_access_key_id,
        &config.r2_config.read_write_secret_access_key,
        config.r2_config.session_token.as_deref(),
    )
    .map_err(ValidationError::InvalidR2Credentials)?;

    if config.nodes.is_empty() {
        return Err(ValidationError::EmptyNodes());
    }
    if config.nodes.len() > limits::MAX_NODES {
        return Err(ValidationError::TooManyNodes(config.nodes.len()));
    }

    let mut seen_hostnames = BTreeSet::new();
    for n in &config.nodes {
        hostname::validate(&n.hostname).map_err(|source| ValidationError::InvalidHostname {
            hostname: n.hostname.clone(),
            source,
        })?;
        if !seen_hostnames.insert(n.hostname.clone()) {
            return Err(ValidationError::DuplicateHostname(n.hostname.clone()));
        }
    }

    let mut masters = BTreeSet::new();
    for m in &config.master {
        if !seen_hostnames.contains(m) {
            return Err(ValidationError::UnknownMaster(m.clone()));
        }
        if !masters.insert(m.clone()) {
            return Err(ValidationError::DuplicateMaster(m.clone()));
        }
    }
    if masters.is_empty() {
        warnings
            .push("master list is empty: no peer edges will be generated for any node".to_string());
    }

    // Per-node field validation, plus tunnel-address uniqueness.
    let mut validated_nodes = Vec::with_capacity(config.nodes.len());
    let mut addr_owners: BTreeMap<std::net::Ipv4Addr, String> = BTreeMap::new();
    for n in &config.nodes {
        let wg = &n.wg_config;

        let net = parse_canonical_ipv4_net(&wg.tunnel_address).map_err(|_| {
            ValidationError::InvalidTunnelAddress(n.hostname.clone(), wg.tunnel_address.clone())
        })?;
        if net.prefix_len() != 32 {
            return Err(ValidationError::InvalidTunnelAddress(
                n.hostname.clone(),
                wg.tunnel_address.clone(),
            ));
        }
        if let Some(prev_owner) = addr_owners.insert(net.addr(), n.hostname.clone()) {
            return Err(ValidationError::DuplicateTunnelAddress {
                addr: wg.tunnel_address.clone(),
                a: prev_owner,
                b: n.hostname.clone(),
            });
        }

        if wg.listen_port == 0 {
            return Err(ValidationError::InvalidListenPort(n.hostname.clone()));
        }

        if let Some(ep) = &wg.endpoint {
            validate_endpoint_syntax(ep)
                .map_err(|e| ValidationError::InvalidEndpoint(n.hostname.clone(), ep.clone(), e))?;
        }

        let dns = match &wg.dns {
            Some(d) => Some(
                d.parse::<IpAddr>()
                    .map_err(|_| ValidationError::InvalidDns(n.hostname.clone(), d.clone()))?,
            ),
            None => None,
        };

        if let Some(mtu) = wg.mtu
            && mtu < limits::MIN_MTU
        {
            return Err(ValidationError::InvalidMtu(n.hostname.clone(), mtu));
        }

        let mut extra_nets = Vec::with_capacity(wg.extra_allowed_ips.len());
        for r in &wg.extra_allowed_ips {
            let net = parse_canonical_ipv4_net(r)
                .map_err(|_| ValidationError::NonCanonicalRoute(n.hostname.clone(), r.clone()))?;
            if net.prefix_len() == 0 {
                return Err(ValidationError::DefaultRouteRejected(n.hostname.clone()));
            }
            extra_nets.push(net);
        }

        validated_nodes.push(ValidatedNode {
            hostname: n.hostname.clone(),
            tunnel_address: net.addr(),
            listen_port: wg.listen_port,
            endpoint: wg.endpoint.clone(),
            dns,
            mtu: wg.mtu,
            extra_allowed_ips: extra_nets,
            persistent_keepalive: wg.persistent_keepalive,
        });
    }

    // Global route checks: disjointness across the whole fleet, and no
    // route may cover any node's tunnel address.
    for i in 0..validated_nodes.len() {
        for r in &validated_nodes[i].extra_allowed_ips {
            let r_range = net_range(r);
            for node in &validated_nodes {
                let addr_u32 = u32::from(node.tunnel_address);
                if r_range.0 <= addr_u32 && addr_u32 <= r_range.1 {
                    return Err(ValidationError::RouteCoversTunnelAddress {
                        route_host: validated_nodes[i].hostname.clone(),
                        route: r.to_string(),
                        node: node.hostname.clone(),
                        addr: node.tunnel_address.to_string(),
                    });
                }
            }
        }
        for j in (i + 1)..validated_nodes.len() {
            for r_a in &validated_nodes[i].extra_allowed_ips {
                for r_b in &validated_nodes[j].extra_allowed_ips {
                    if ranges_overlap(net_range(r_a), net_range(r_b)) {
                        return Err(ValidationError::OverlappingRoutes {
                            a_host: validated_nodes[i].hostname.clone(),
                            a_route: r_a.to_string(),
                            b_host: validated_nodes[j].hostname.clone(),
                            b_route: r_b.to_string(),
                        });
                    }
                }
            }
        }
        // Also reject duplicate/overlapping routes within one node's own list.
        for a_idx in 0..validated_nodes[i].extra_allowed_ips.len() {
            for b_idx in (a_idx + 1)..validated_nodes[i].extra_allowed_ips.len() {
                let r_a = &validated_nodes[i].extra_allowed_ips[a_idx];
                let r_b = &validated_nodes[i].extra_allowed_ips[b_idx];
                if ranges_overlap(net_range(r_a), net_range(r_b)) {
                    return Err(ValidationError::OverlappingRoutes {
                        a_host: validated_nodes[i].hostname.clone(),
                        a_route: r_a.to_string(),
                        b_host: validated_nodes[i].hostname.clone(),
                        b_route: r_b.to_string(),
                    });
                }
            }
        }
    }

    let hostnames: Vec<String> = validated_nodes.iter().map(|n| n.hostname.clone()).collect();
    let edges = compute_edges(&hostnames, &masters);
    for (a, b) in &edges {
        let a_node = validated_nodes.iter().find(|n| &n.hostname == a).unwrap();
        let b_node = validated_nodes.iter().find(|n| &n.hostname == b).unwrap();
        if a_node.endpoint.is_none() && b_node.endpoint.is_none() {
            return Err(ValidationError::EdgeMissingEndpoint(a.clone(), b.clone()));
        }
    }

    validated_nodes.sort_by(|a, b| a.hostname.cmp(&b.hostname));

    Ok((
        ValidatedTopology {
            nodes: validated_nodes,
            masters,
        },
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r2() -> R2Config {
        R2Config {
            endpoint: "https://example.r2.cloudflarestorage.com".into(),
            read_write_access_key_id: "id".into(),
            read_write_secret_access_key: "secret".into(),
            session_token: None,
            region: "auto".into(),
            bucket: "wg-confs".into(),
        }
    }

    fn node(hostname: &str, addr: &str, endpoint: Option<&str>) -> NodeConfig {
        NodeConfig {
            hostname: hostname.into(),
            wg_config: WgConfig {
                tunnel_address: format!("{addr}/32"),
                listen_port: 51820,
                endpoint: endpoint.map(String::from),
                dns: None,
                mtu: None,
                extra_allowed_ips: Vec::new(),
                persistent_keepalive: None,
            },
        }
    }

    /// The 2-master/2-spoke fixture from wg-server.md §5/§7.
    fn example_config() -> TopologyConfig {
        TopologyConfig {
            r2_config: r2(),
            nodes: vec![
                node("master-us", "10.10.0.1", Some("203.0.113.10:51820")),
                node("master-eu", "10.10.0.2", Some("198.51.100.20:51820")),
                node("workstation-01", "10.10.0.100", None),
                node("mobile-01", "10.10.0.101", None),
            ],
            master: vec!["master-us".into(), "master-eu".into()],
        }
    }

    #[test]
    fn r2_endpoint_accepts_https() {
        assert!(validate_r2_endpoint("https://accountid.r2.cloudflarestorage.com").is_ok());
    }

    #[test]
    fn r2_endpoint_accepts_plain_http() {
        // Deliberately allowed: docker-tests talks to local MinIO over
        // plain HTTP by design (CLAUDE.md "Known simplifications").
        assert!(validate_r2_endpoint("http://minio:9000").is_ok());
    }

    #[test]
    fn r2_endpoint_rejects_non_http_scheme() {
        assert!(validate_r2_endpoint("ftp://example.com").is_err());
    }

    #[test]
    fn r2_endpoint_rejects_userinfo() {
        assert!(validate_r2_endpoint("https://user:pass@example.com").is_err());
    }

    #[test]
    fn r2_endpoint_rejects_query_string() {
        assert!(validate_r2_endpoint("https://example.com?token=abc").is_err());
    }

    #[test]
    fn r2_endpoint_rejects_fragment() {
        assert!(validate_r2_endpoint("https://example.com#frag").is_err());
    }

    #[test]
    fn r2_endpoint_rejects_unparseable_url() {
        assert!(validate_r2_endpoint("not a url").is_err());
    }

    #[test]
    fn r2_credentials_rejects_empty_access_key() {
        assert!(validate_r2_credentials("", "secret", None).is_err());
    }

    #[test]
    fn r2_credentials_rejects_empty_secret() {
        assert!(validate_r2_credentials("id", "", None).is_err());
    }

    #[test]
    fn r2_credentials_rejects_empty_session_token_when_present() {
        assert!(validate_r2_credentials("id", "secret", Some("")).is_err());
    }

    #[test]
    fn r2_credentials_accepts_valid_fields() {
        assert!(validate_r2_credentials("id", "secret", Some("token")).is_ok());
        assert!(validate_r2_credentials("id", "secret", None).is_ok());
    }

    #[test]
    fn validate_rejects_bad_r2_endpoint_in_full_config() {
        let mut config = example_config();
        config.r2_config.endpoint = "http://user:pass@minio:9000".into();
        assert!(matches!(
            validate(&config),
            Err(ValidationError::InvalidR2Endpoint(_, _))
        ));
    }

    #[test]
    fn validate_rejects_empty_r2_credentials_in_full_config() {
        let mut config = example_config();
        config.r2_config.read_write_access_key_id = String::new();
        assert!(matches!(
            validate(&config),
            Err(ValidationError::InvalidR2Credentials(_))
        ));
    }

    #[test]
    fn compute_edges_matches_hub_and_spoke_example() {
        let hostnames: Vec<String> = vec![
            "master-us".into(),
            "master-eu".into(),
            "workstation-01".into(),
            "mobile-01".into(),
        ];
        let masters: BTreeSet<String> = ["master-us", "master-eu"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let edges = compute_edges(&hostnames, &masters);
        assert_eq!(edges.len(), 5); // C(2,2)=1 master-master + 2*2=4 master-spoke
        assert!(edges.contains(&("master-eu".to_string(), "master-us".to_string())));
        assert!(edges.contains(&("master-us".to_string(), "workstation-01".to_string())));
        assert!(edges.contains(&("master-eu".to_string(), "mobile-01".to_string())));
        // no spoke-spoke edge
        assert!(!edges.contains(&("mobile-01".to_string(), "workstation-01".to_string())));
    }

    #[test]
    fn compute_edges_zero_masters_has_no_edges() {
        let hostnames: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let edges = compute_edges(&hostnames, &BTreeSet::new());
        assert!(edges.is_empty());
    }

    #[test]
    fn compute_edges_single_node_has_no_edges() {
        let hostnames: Vec<String> = vec!["a".into()];
        let masters: BTreeSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        assert!(compute_edges(&hostnames, &masters).is_empty());
    }

    #[test]
    fn compute_edges_all_masters_is_full_mesh() {
        let hostnames: Vec<String> = (0..5).map(|i| format!("m{i}")).collect();
        let masters: BTreeSet<String> = hostnames.iter().cloned().collect();
        let edges = compute_edges(&hostnames, &masters);
        assert_eq!(edges.len(), 5 * 4 / 2); // C(5,2)
    }

    #[test]
    fn valid_example_config_passes() {
        let (topo, warnings) = validate(&example_config()).expect("should validate");
        assert!(warnings.is_empty());
        assert_eq!(topo.nodes.len(), 4);
        assert_eq!(topo.edges().len(), 5);
    }

    #[test]
    fn empty_master_list_warns_but_is_ok() {
        let mut cfg = example_config();
        cfg.master.clear();
        let (topo, warnings) = validate(&cfg).expect("should still validate");
        assert!(!warnings.is_empty());
        assert!(topo.edges().is_empty());
    }

    #[test]
    fn rejects_empty_nodes() {
        let mut cfg = example_config();
        cfg.nodes.clear();
        cfg.master.clear();
        assert_eq!(validate(&cfg).err(), Some(ValidationError::EmptyNodes()));
    }

    #[test]
    fn rejects_duplicate_hostname() {
        let mut cfg = example_config();
        cfg.nodes.push(node("master-us", "10.10.0.200", None));
        assert_eq!(
            validate(&cfg).err(),
            Some(ValidationError::DuplicateHostname("master-us".into()))
        );
    }

    #[test]
    fn rejects_unknown_master() {
        let mut cfg = example_config();
        cfg.master.push("nonexistent".into());
        assert_eq!(
            validate(&cfg).err(),
            Some(ValidationError::UnknownMaster("nonexistent".into()))
        );
    }

    #[test]
    fn rejects_duplicate_master() {
        let mut cfg = example_config();
        cfg.master.push("master-us".into());
        assert_eq!(
            validate(&cfg).err(),
            Some(ValidationError::DuplicateMaster("master-us".into()))
        );
    }

    #[test]
    fn rejects_non_32_tunnel_address() {
        let mut cfg = example_config();
        cfg.nodes[0].wg_config.tunnel_address = "10.10.0.0/24".into();
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidTunnelAddress(_, _))
        ));
    }

    #[test]
    fn rejects_duplicate_tunnel_address() {
        let mut cfg = example_config();
        cfg.nodes[1].wg_config.tunnel_address = cfg.nodes[0].wg_config.tunnel_address.clone();
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::DuplicateTunnelAddress { .. })
        ));
    }

    #[test]
    fn rejects_default_route_extra_allowed_ip() {
        let mut cfg = example_config();
        cfg.nodes[0]
            .wg_config
            .extra_allowed_ips
            .push("0.0.0.0/0".into());
        assert_eq!(
            validate(&cfg).err(),
            Some(ValidationError::DefaultRouteRejected("master-us".into()))
        );
    }

    #[test]
    fn rejects_non_canonical_extra_route() {
        let mut cfg = example_config();
        cfg.nodes[0]
            .wg_config
            .extra_allowed_ips
            .push("10.20.0.5/24".into());
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::NonCanonicalRoute(_, _))
        ));
    }

    #[test]
    fn rejects_overlapping_routes_across_nodes() {
        let mut cfg = example_config();
        cfg.nodes[0]
            .wg_config
            .extra_allowed_ips
            .push("10.20.0.0/16".into());
        cfg.nodes[1]
            .wg_config
            .extra_allowed_ips
            .push("10.20.1.0/24".into());
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::OverlappingRoutes { .. })
        ));
    }

    #[test]
    fn rejects_route_covering_a_tunnel_address() {
        let mut cfg = example_config();
        // 10.10.0.0/24 covers every node's 10.10.0.x/32 tunnel address.
        cfg.nodes[0]
            .wg_config
            .extra_allowed_ips
            .push("10.10.0.0/24".into());
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::RouteCoversTunnelAddress { .. })
        ));
    }

    #[test]
    fn rejects_edge_with_no_endpoint_on_either_side() {
        // Two spokes both lacking an endpoint is fine (no edge between
        // them), but a master with no endpoint alongside a spoke with no
        // endpoint creates an edge that can never be dialed.
        let cfg = TopologyConfig {
            r2_config: r2(),
            nodes: vec![
                node("hub", "10.10.0.1", None),
                node("spoke", "10.10.0.2", None),
            ],
            master: vec!["hub".into()],
        };
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::EdgeMissingEndpoint(_, _))
        ));
    }

    #[test]
    fn accepts_edge_when_only_one_side_has_endpoint() {
        let cfg = TopologyConfig {
            r2_config: r2(),
            nodes: vec![
                node("hub", "10.10.0.1", Some("203.0.113.1:51820")),
                node("spoke", "10.10.0.2", None),
            ],
            master: vec!["hub".into()],
        };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_invalid_hostname() {
        let mut cfg = example_config();
        cfg.nodes[0].hostname = "Bad_Host".into();
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::InvalidHostname { .. })
        ));
    }

    #[test]
    fn rejects_too_many_nodes() {
        let mut cfg = example_config();
        cfg.nodes.clear();
        cfg.master.clear();
        for i in 0..=limits::MAX_NODES {
            let a = (i / (256 * 256)) as u8;
            let b = ((i / 256) % 256) as u8;
            let c = (i % 256) as u8;
            cfg.nodes
                .push(node(&format!("n{i}"), &format!("10.{a}.{b}.{c}"), None));
        }
        assert!(matches!(
            validate(&cfg),
            Err(ValidationError::TooManyNodes(_))
        ));
    }

    #[test]
    fn accepts_exactly_max_nodes() {
        let mut cfg = example_config();
        cfg.nodes.clear();
        cfg.master.clear();
        for i in 0..limits::MAX_NODES {
            let a = (i / (256 * 256)) as u8;
            let b = ((i / 256) % 256) as u8;
            let c = (i % 256) as u8;
            cfg.nodes
                .push(node(&format!("n{i}"), &format!("10.{a}.{b}.{c}"), None));
        }
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn parse_rejects_duplicate_json_keys() {
        let text = r#"{"r2_config":{},"r2_config":{},"nodes":[],"master":[]}"#;
        assert!(matches!(
            parse(text),
            Err(ValidationError::DuplicateJsonKey(_))
        ));
    }

    #[test]
    fn parse_rejects_unknown_top_level_field() {
        let text = r#"{"r2_config":{"endpoint":"https://x","read_write_access_key_id":"a","read_write_secret_access_key":"b","region":"auto","bucket":"c"},"nodes":[],"master":[],"unexpected":1}"#;
        assert!(matches!(parse(text), Err(ValidationError::InvalidJson(_))));
    }
}
