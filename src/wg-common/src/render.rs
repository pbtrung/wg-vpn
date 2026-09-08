//! Rendering a `wg-quick` `.conf` from a validated topology + resolved
//! keys (wg-server.md §9), and the strict shared parser for the accepted
//! configuration subset (wg-client.md §6 "Accepted configuration
//! subset") used to validate both freshly rendered output and anything
//! downloaded off the wire before it's ever applied.

use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;

use ipnet::Ipv4Net;
use thiserror::Error;

use crate::keys;
use crate::limits;
use crate::topology::{self, ValidatedTopology};

/// Resolved key material for a whole generation: one keypair per node,
/// one preshared key per edge (wg-server.md §8).
#[derive(Debug, Default, Clone)]
pub struct ResolvedKeys {
    /// hostname -> (private_key_b64, public_key_b64)
    pub node_keys: BTreeMap<String, (String, String)>,
    /// canonical (a, b) pair, a < b -> preshared_key_b64
    pub edge_psks: BTreeMap<(String, String), String>,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("unknown hostname: {0}")]
    UnknownHostname(String),
    #[error("missing keypair for node {0}")]
    MissingNodeKey(String),
    #[error("missing preshared key for edge ({0}, {1})")]
    MissingEdgePsk(String, String),
    #[error("rendered file for {hostname} is {size} bytes, exceeding the {max} byte limit", max = limits::MAX_RENDERED_CONFIG_BYTES)]
    TooLarge { hostname: String, size: usize },
    #[error("rendered output for {hostname} failed self-validation: {source}")]
    SelfValidationFailed {
        hostname: String,
        #[source]
        source: ParseError,
    },
}

fn resolve_local_keepalive(node: &crate::topology::ValidatedNode) -> Option<u16> {
    match node.persistent_keepalive {
        Some(0) => None,
        Some(n) => Some(n),
        None => {
            if node.endpoint.is_none() {
                Some(limits::DEFAULT_PERSISTENT_KEEPALIVE_WHEN_NO_ENDPOINT)
            } else {
                None
            }
        }
    }
}

fn sorted_allowed_ips(node: &crate::topology::ValidatedNode) -> Vec<String> {
    let mut extra: Vec<&Ipv4Net> = node.extra_allowed_ips.iter().collect();
    extra.sort_by_key(|n| (u32::from(n.network()), n.prefix_len()));
    let mut out = vec![format!("{}/32", node.tunnel_address)];
    out.extend(extra.iter().map(|n| n.to_string()));
    out
}

/// Render the complete `.conf` for one node. Self-validates the output
/// through [`parse_wg_quick`] before returning it (wg-server.md §9:
/// "Validate every rendered file with the shared parser before upload").
pub fn render_node_config(
    topo: &ValidatedTopology,
    keys: &ResolvedKeys,
    hostname: &str,
) -> Result<String, RenderError> {
    let node = topo
        .node(hostname)
        .ok_or_else(|| RenderError::UnknownHostname(hostname.to_string()))?;
    let (priv_b64, _) = keys
        .node_keys
        .get(hostname)
        .ok_or_else(|| RenderError::MissingNodeKey(hostname.to_string()))?;

    let mut out = String::new();
    out.push_str("# Managed by wg-server -- do not edit manually.\n");
    out.push_str(&format!("# Hostname: {hostname}\n\n"));
    out.push_str("[Interface]\n");
    out.push_str(&format!("PrivateKey = {priv_b64}\n"));
    out.push_str(&format!("Address = {}/32\n", node.tunnel_address));
    out.push_str(&format!("ListenPort = {}\n", node.listen_port));
    if let Some(dns) = node.dns {
        out.push_str(&format!("DNS = {dns}\n"));
    }
    if let Some(mtu) = node.mtu {
        out.push_str(&format!("MTU = {mtu}\n"));
    }

    let edges = topo.edges();
    let mut peer_hosts: Vec<&String> = edges
        .iter()
        .filter_map(|(a, b)| {
            if a == hostname {
                Some(b)
            } else if b == hostname {
                Some(a)
            } else {
                None
            }
        })
        .collect();
    peer_hosts.sort();

    let local_keepalive = resolve_local_keepalive(node);

    for peer_host in peer_hosts {
        let peer = topo
            .node(peer_host)
            .ok_or_else(|| RenderError::UnknownHostname(peer_host.clone()))?;
        let (_, peer_pub) = keys
            .node_keys
            .get(peer_host)
            .ok_or_else(|| RenderError::MissingNodeKey(peer_host.clone()))?;
        let pair = topology::canonical_pair(hostname, peer_host);
        let psk = keys
            .edge_psks
            .get(&pair)
            .ok_or_else(|| RenderError::MissingEdgePsk(pair.0.clone(), pair.1.clone()))?;

        out.push('\n');
        out.push_str("[Peer]\n");
        out.push_str(&format!("# {peer_host}\n"));
        out.push_str(&format!("PublicKey = {peer_pub}\n"));
        out.push_str(&format!("PresharedKey = {psk}\n"));
        out.push_str(&format!(
            "AllowedIPs = {}\n",
            sorted_allowed_ips(peer).join(", ")
        ));
        if let Some(ep) = &peer.endpoint {
            out.push_str(&format!("Endpoint = {ep}\n"));
        }
        if let Some(k) = local_keepalive {
            out.push_str(&format!("PersistentKeepalive = {k}\n"));
        }
    }

    if out.len() > limits::MAX_RENDERED_CONFIG_BYTES {
        return Err(RenderError::TooLarge {
            hostname: hostname.to_string(),
            size: out.len(),
        });
    }

    parse_wg_quick(&out).map_err(|source| RenderError::SelfValidationFailed {
        hostname: hostname.to_string(),
        source,
    })?;

    Ok(out)
}

// ---------------------------------------------------------------------
// Shared strict parser: the accepted configuration subset.
// ---------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("configuration is {0} bytes, exceeding the {max} byte limit", max = limits::MAX_RENDERED_CONFIG_BYTES)]
    TooLarge(usize),
    #[error("configuration contains a NUL or other control character")]
    ControlCharacter,
    #[error("malformed line: {0}")]
    Malformed(&'static str),
    #[error("unknown section [{0}]")]
    UnknownSection(String),
    #[error("duplicate [{0}] section")]
    DuplicateSection(&'static str),
    #[error("directive {0:?} is not permitted (hooks/shell directives are rejected)")]
    ForbiddenDirective(String),
    #[error("unknown directive {0:?} in [{1}]")]
    UnknownDirective(String, &'static str),
    #[error("duplicate directive {0:?} in [{1}]")]
    DuplicateDirective(String, &'static str),
    #[error("missing [Interface] section")]
    MissingInterface,
    #[error("[Interface] is missing required field {0}")]
    MissingInterfaceField(&'static str),
    #[error("[Peer] is missing required field {0}")]
    MissingPeerField(&'static str),
    #[error("invalid key: {0}")]
    InvalidKey(#[from] keys::KeyError),
    #[error("invalid Address: {0}")]
    InvalidAddress(&'static str),
    #[error("invalid ListenPort: must be 1-65535")]
    InvalidListenPort,
    #[error("invalid DNS: must be a single IP address")]
    InvalidDns,
    #[error("invalid MTU: must be {min}-65535", min = limits::MIN_MTU)]
    InvalidMtu,
    #[error("AllowedIPs must be non-empty")]
    EmptyAllowedIps,
    #[error("invalid AllowedIPs entry {0:?}: {1}")]
    InvalidAllowedIps(String, &'static str),
    #[error("AllowedIPs must not include a default route (0.0.0.0/0)")]
    DefaultRouteRejected,
    #[error("invalid Endpoint: {0}")]
    InvalidEndpoint(&'static str),
    #[error("invalid PersistentKeepalive: must be 0-65535")]
    InvalidPersistentKeepalive,
    #[error("a peer's PublicKey matches this node's own public key")]
    PeerMatchesLocalKey,
    #[error("duplicate peer PublicKey")]
    DuplicatePeerPublicKey,
    #[error("AllowedIPs entries from different peers overlap")]
    OverlappingAllowedIps,
    #[error("a peer's AllowedIPs covers this node's own tunnel address")]
    RouteCoversLocalAddress,
}

#[derive(Debug, Clone)]
pub struct ParsedInterface {
    pub private_key: String,
    pub address: std::net::Ipv4Addr,
    pub listen_port: u16,
    pub dns: Option<IpAddr>,
    pub mtu: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ParsedPeer {
    pub public_key: String,
    pub preshared_key: String,
    pub allowed_ips: Vec<Ipv4Net>,
    pub endpoint: Option<String>,
    pub persistent_keepalive: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ParsedConfig {
    pub interface: ParsedInterface,
    pub peers: Vec<ParsedPeer>,
}

struct RawSection {
    name: &'static str,
    fields: Vec<(String, String)>,
}

fn net_range(net: &Ipv4Net) -> (u32, u32) {
    (u32::from(net.network()), u32::from(net.broadcast()))
}

fn ranges_overlap(a: (u32, u32), b: (u32, u32)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

pub fn parse_wg_quick(text: &str) -> Result<ParsedConfig, ParseError> {
    if text.len() > limits::MAX_RENDERED_CONFIG_BYTES {
        return Err(ParseError::TooLarge(text.len()));
    }
    if text.bytes().any(|b| b == 0) {
        return Err(ParseError::ControlCharacter);
    }

    let mut sections: Vec<RawSection> = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.contains('\r') {
            return Err(ParseError::ControlCharacter);
        }
        let line = raw_line.trim_matches(' ').trim_matches('\t');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let name = inner
                .strip_suffix(']')
                .ok_or(ParseError::Malformed("unterminated section header"))?;
            let name = match name {
                "Interface" => "Interface",
                "Peer" => "Peer",
                other => return Err(ParseError::UnknownSection(other.to_string())),
            };
            sections.push(RawSection {
                name,
                fields: Vec::new(),
            });
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or(ParseError::Malformed("expected 'Key = Value'"))?;
        let key = key.trim().to_string();
        let value = value.trim().to_string();
        if value.chars().any(|c| c.is_control()) {
            return Err(ParseError::ControlCharacter);
        }
        let section = sections
            .last_mut()
            .ok_or(ParseError::Malformed("directive outside any section"))?;
        section.fields.push((key, value));
    }

    let mut interface: Option<ParsedInterface> = None;
    let mut peers = Vec::new();
    for raw in sections {
        match raw.name {
            "Interface" => {
                if interface.is_some() {
                    return Err(ParseError::DuplicateSection("Interface"));
                }
                interface = Some(parse_interface_fields(&raw.fields)?);
            }
            "Peer" => {
                peers.push(parse_peer_fields(&raw.fields)?);
            }
            _ => unreachable!("only Interface/Peer sections are ever pushed"),
        }
    }
    let interface = interface.ok_or(ParseError::MissingInterface)?;

    let local_pub = keys::public_key_from_private_base64(&interface.private_key)?;
    let mut seen_pub_keys: HashSet<String> = HashSet::new();
    for p in &peers {
        if p.public_key == local_pub {
            return Err(ParseError::PeerMatchesLocalKey);
        }
        if !seen_pub_keys.insert(p.public_key.clone()) {
            return Err(ParseError::DuplicatePeerPublicKey);
        }
    }

    let local_addr_u32 = u32::from(interface.address);
    for (i, peer) in peers.iter().enumerate() {
        for net in &peer.allowed_ips {
            let (s, e) = net_range(net);
            if s <= local_addr_u32 && local_addr_u32 <= e {
                return Err(ParseError::RouteCoversLocalAddress);
            }
        }
        for other in &peers[i + 1..] {
            for a in &peer.allowed_ips {
                for b in &other.allowed_ips {
                    if ranges_overlap(net_range(a), net_range(b)) {
                        return Err(ParseError::OverlappingAllowedIps);
                    }
                }
            }
        }
    }

    Ok(ParsedConfig { interface, peers })
}

fn check_no_forbidden_or_unknown(
    key: &str,
    allowed: &[&str],
    forbidden: &[&str],
    section: &'static str,
) -> Result<(), ParseError> {
    if forbidden.contains(&key) {
        return Err(ParseError::ForbiddenDirective(key.to_string()));
    }
    if !allowed.contains(&key) {
        return Err(ParseError::UnknownDirective(key.to_string(), section));
    }
    Ok(())
}

fn parse_interface_fields(fields: &[(String, String)]) -> Result<ParsedInterface, ParseError> {
    const ALLOWED: &[&str] = &["PrivateKey", "Address", "ListenPort", "DNS", "MTU"];
    const FORBIDDEN: &[&str] = &[
        "PreUp",
        "PostUp",
        "PreDown",
        "PostDown",
        "SaveConfig",
        "Table",
    ];
    let mut seen = HashSet::new();
    let (mut private_key, mut address, mut listen_port, mut dns, mut mtu) =
        (None, None, None, None, None);
    for (k, v) in fields {
        check_no_forbidden_or_unknown(k, ALLOWED, FORBIDDEN, "Interface")?;
        if !seen.insert(k.clone()) {
            return Err(ParseError::DuplicateDirective(k.clone(), "Interface"));
        }
        match k.as_str() {
            "PrivateKey" => {
                keys::validate_key_bytes(v)?;
                private_key = Some(v.clone());
            }
            "Address" => {
                let net: Ipv4Net = v
                    .parse()
                    .map_err(|_| ParseError::InvalidAddress("not a valid IPv4 CIDR"))?;
                if net.prefix_len() != 32 {
                    return Err(ParseError::InvalidAddress("must be a /32"));
                }
                address = Some(net.addr());
            }
            "ListenPort" => {
                let port: u16 = v.parse().map_err(|_| ParseError::InvalidListenPort)?;
                if port == 0 {
                    return Err(ParseError::InvalidListenPort);
                }
                listen_port = Some(port);
            }
            "DNS" => {
                dns = Some(v.parse::<IpAddr>().map_err(|_| ParseError::InvalidDns)?);
            }
            "MTU" => {
                let m: u16 = v.parse().map_err(|_| ParseError::InvalidMtu)?;
                if m < limits::MIN_MTU {
                    return Err(ParseError::InvalidMtu);
                }
                mtu = Some(m);
            }
            _ => unreachable!("checked by check_no_forbidden_or_unknown"),
        }
    }
    Ok(ParsedInterface {
        private_key: private_key.ok_or(ParseError::MissingInterfaceField("PrivateKey"))?,
        address: address.ok_or(ParseError::MissingInterfaceField("Address"))?,
        listen_port: listen_port.ok_or(ParseError::MissingInterfaceField("ListenPort"))?,
        dns,
        mtu,
    })
}

fn parse_peer_fields(fields: &[(String, String)]) -> Result<ParsedPeer, ParseError> {
    const ALLOWED: &[&str] = &[
        "PublicKey",
        "PresharedKey",
        "AllowedIPs",
        "Endpoint",
        "PersistentKeepalive",
    ];
    let mut seen = HashSet::new();
    let (mut public_key, mut preshared_key, mut allowed_ips, mut endpoint, mut keepalive) =
        (None, None, None, None, None);
    for (k, v) in fields {
        check_no_forbidden_or_unknown(k, ALLOWED, &[], "Peer")?;
        if !seen.insert(k.clone()) {
            return Err(ParseError::DuplicateDirective(k.clone(), "Peer"));
        }
        match k.as_str() {
            "PublicKey" => {
                keys::validate_public_key(v)?;
                public_key = Some(v.clone());
            }
            "PresharedKey" => {
                keys::validate_preshared_key(v)?;
                preshared_key = Some(v.clone());
            }
            "AllowedIPs" => {
                if v.is_empty() {
                    return Err(ParseError::EmptyAllowedIps);
                }
                let mut nets = Vec::new();
                for entry in v.split(',').map(|s| s.trim()) {
                    let net: Ipv4Net = entry.parse().map_err(|_| {
                        ParseError::InvalidAllowedIps(entry.to_string(), "not a valid IPv4 CIDR")
                    })?;
                    if net.addr() != net.network() {
                        return Err(ParseError::InvalidAllowedIps(
                            entry.to_string(),
                            "host bits set",
                        ));
                    }
                    if net.prefix_len() == 0 {
                        return Err(ParseError::DefaultRouteRejected);
                    }
                    nets.push(net);
                }
                allowed_ips = Some(nets);
            }
            "Endpoint" => {
                topology::validate_endpoint_syntax(v).map_err(ParseError::InvalidEndpoint)?;
                endpoint = Some(v.clone());
            }
            "PersistentKeepalive" => {
                keepalive = Some(
                    v.parse::<u16>()
                        .map_err(|_| ParseError::InvalidPersistentKeepalive)?,
                );
            }
            _ => unreachable!("checked by check_no_forbidden_or_unknown"),
        }
    }
    let allowed_ips = allowed_ips.ok_or(ParseError::MissingPeerField("AllowedIPs"))?;
    if allowed_ips.is_empty() {
        return Err(ParseError::EmptyAllowedIps);
    }
    Ok(ParsedPeer {
        public_key: public_key.ok_or(ParseError::MissingPeerField("PublicKey"))?,
        preshared_key: preshared_key.ok_or(ParseError::MissingPeerField("PresharedKey"))?,
        allowed_ips,
        endpoint,
        persistent_keepalive: keepalive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{ValidatedNode, ValidatedTopology};
    use std::collections::BTreeSet;

    fn node(
        hostname: &str,
        addr: &str,
        endpoint: Option<&str>,
        keepalive: Option<u16>,
    ) -> ValidatedNode {
        ValidatedNode {
            hostname: hostname.to_string(),
            tunnel_address: addr.parse().unwrap(),
            listen_port: 51820,
            endpoint: endpoint.map(String::from),
            dns: None,
            mtu: None,
            extra_allowed_ips: Vec::new(),
            persistent_keepalive: keepalive,
        }
    }

    fn example_topology() -> ValidatedTopology {
        let nodes = vec![
            node("master-eu", "10.10.0.2", Some("198.51.100.20:51820"), None),
            node("master-us", "10.10.0.1", Some("203.0.113.10:51820"), None),
            node("workstation-01", "10.10.0.100", None, None),
        ];
        let masters: BTreeSet<String> = ["master-us", "master-eu"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        ValidatedTopology { nodes, masters }
    }

    fn example_keys(topo: &ValidatedTopology) -> ResolvedKeys {
        let mut rk = ResolvedKeys::default();
        for n in &topo.nodes {
            let kp = keys::Keypair::generate();
            rk.node_keys.insert(
                n.hostname.clone(),
                (kp.private_key_base64(), kp.public_key_base64()),
            );
        }
        for (a, b) in topo.edges() {
            rk.edge_psks.insert((a, b), keys::generate_preshared_key());
        }
        rk
    }

    #[test]
    fn renders_a_config_that_parses_cleanly() {
        let topo = example_topology();
        let rk = example_keys(&topo);
        let text = render_node_config(&topo, &rk, "master-us").unwrap();
        let parsed = parse_wg_quick(&text).unwrap();
        assert_eq!(parsed.peers.len(), 2); // master-eu + workstation-01
        assert_eq!(parsed.interface.listen_port, 51820);
    }

    #[test]
    fn rendering_is_byte_for_byte_deterministic() {
        let topo = example_topology();
        let rk = example_keys(&topo);
        let a = render_node_config(&topo, &rk, "master-us").unwrap();
        let b = render_node_config(&topo, &rk, "master-us").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn peer_order_is_independent_of_input_node_order() {
        let topo = example_topology();
        let rk = example_keys(&topo);
        let mut reordered = topo.clone();
        reordered.nodes.reverse();
        let a = render_node_config(&topo, &rk, "master-us").unwrap();
        let b = render_node_config(&reordered, &rk, "master-us").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn keepalive_defaults_to_25_when_local_node_has_no_endpoint() {
        let topo = example_topology();
        let rk = example_keys(&topo);
        let text = render_node_config(&topo, &rk, "workstation-01").unwrap();
        assert!(text.contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn keepalive_omitted_when_local_node_has_an_endpoint() {
        let topo = example_topology();
        let rk = example_keys(&topo);
        let text = render_node_config(&topo, &rk, "master-us").unwrap();
        assert!(!text.contains("PersistentKeepalive"));
    }

    #[test]
    fn explicit_zero_keepalive_disables_it_even_without_an_endpoint() {
        let mut topo = example_topology();
        for n in topo.nodes.iter_mut() {
            if n.hostname == "workstation-01" {
                n.persistent_keepalive = Some(0);
            }
        }
        let rk = example_keys(&topo);
        let text = render_node_config(&topo, &rk, "workstation-01").unwrap();
        assert!(!text.contains("PersistentKeepalive"));
    }

    #[test]
    fn explicit_keepalive_enables_it_even_with_an_endpoint() {
        let mut topo = example_topology();
        for n in topo.nodes.iter_mut() {
            if n.hostname == "master-us" {
                n.persistent_keepalive = Some(15);
            }
        }
        let rk = example_keys(&topo);
        let text = render_node_config(&topo, &rk, "master-us").unwrap();
        assert!(text.contains("PersistentKeepalive = 15"));
    }

    // --- parser ---

    fn minimal_valid_config() -> String {
        let kp = keys::Keypair::generate();
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n",
            kp.private_key_base64()
        )
    }

    #[test]
    fn parser_accepts_zero_peers() {
        assert!(parse_wg_quick(&minimal_valid_config()).is_ok());
    }

    #[test]
    fn parser_rejects_forbidden_directive() {
        let text = format!("{}PreUp = echo hi\n", minimal_valid_config());
        assert!(matches!(
            parse_wg_quick(&text),
            Err(ParseError::ForbiddenDirective(_))
        ));
    }

    #[test]
    fn parser_rejects_unknown_section() {
        let text = format!("{}\n[Bogus]\nFoo = bar\n", minimal_valid_config());
        assert!(matches!(
            parse_wg_quick(&text),
            Err(ParseError::UnknownSection(_))
        ));
    }

    #[test]
    fn parser_rejects_duplicate_interface_section() {
        let base = minimal_valid_config();
        let text = format!("{base}\n{base}");
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::DuplicateSection("Interface"))
        );
    }

    #[test]
    fn parser_rejects_duplicate_directive() {
        let text = format!("{}ListenPort = 51821\n", minimal_valid_config());
        assert!(matches!(
            parse_wg_quick(&text),
            Err(ParseError::DuplicateDirective(_, "Interface"))
        ));
    }

    #[test]
    fn parser_rejects_zero_private_key() {
        let zero = keys::encode_key(&[0u8; keys::KEY_LEN]);
        let text = format!(
            "[Interface]\nPrivateKey = {zero}\nAddress = 10.0.0.1/32\nListenPort = 51820\n"
        );
        assert!(matches!(
            parse_wg_quick(&text),
            Err(ParseError::InvalidKey(_))
        ));
    }

    fn valid_peer_block(pub_b64: &str, allowed_ips: &str) -> String {
        format!(
            "\n[Peer]\nPublicKey = {pub_b64}\nPresharedKey = {}\nAllowedIPs = {allowed_ips}\n",
            keys::generate_preshared_key()
        )
    }

    #[test]
    fn parser_rejects_self_peer() {
        let kp = keys::Keypair::generate();
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n{}",
            kp.private_key_base64(),
            valid_peer_block(&kp.public_key_base64(), "10.0.0.2/32")
        );
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::PeerMatchesLocalKey)
        );
    }

    #[test]
    fn parser_rejects_duplicate_peer_public_key() {
        let local = keys::Keypair::generate();
        let peer = keys::Keypair::generate();
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n{}{}",
            local.private_key_base64(),
            valid_peer_block(&peer.public_key_base64(), "10.0.0.2/32"),
            valid_peer_block(&peer.public_key_base64(), "10.0.0.3/32")
        );
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::DuplicatePeerPublicKey)
        );
    }

    #[test]
    fn parser_rejects_default_route_in_allowed_ips() {
        let local = keys::Keypair::generate();
        let peer = keys::Keypair::generate();
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n{}",
            local.private_key_base64(),
            valid_peer_block(&peer.public_key_base64(), "0.0.0.0/0")
        );
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::DefaultRouteRejected)
        );
    }

    #[test]
    fn parser_rejects_route_covering_local_address() {
        let local = keys::Keypair::generate();
        let peer = keys::Keypair::generate();
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n{}",
            local.private_key_base64(),
            valid_peer_block(&peer.public_key_base64(), "10.0.0.0/24")
        );
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::RouteCoversLocalAddress)
        );
    }

    #[test]
    fn parser_rejects_overlapping_peer_routes() {
        let local = keys::Keypair::generate();
        let peer_a = keys::Keypair::generate();
        let peer_b = keys::Keypair::generate();
        let text = format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n{}{}",
            local.private_key_base64(),
            valid_peer_block(&peer_a.public_key_base64(), "10.20.0.0/16"),
            valid_peer_block(&peer_b.public_key_base64(), "10.20.5.0/24")
        );
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::OverlappingAllowedIps)
        );
    }

    #[test]
    fn parser_rejects_nul_byte() {
        let text = format!("{}\0", minimal_valid_config());
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::ControlCharacter)
        );
    }

    #[test]
    fn parser_rejects_missing_interface() {
        assert_eq!(
            parse_wg_quick("# just a comment\n").err(),
            Some(ParseError::MissingInterface)
        );
    }

    #[test]
    fn parser_rejects_oversized_input() {
        let text = "#".to_string() + &"a".repeat(limits::MAX_RENDERED_CONFIG_BYTES + 1);
        assert_eq!(
            parse_wg_quick(&text).err(),
            Some(ParseError::TooLarge(text.len()))
        );
    }
}
