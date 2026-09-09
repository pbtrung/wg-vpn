//! IPv4 rtnetlink helpers for link state, addresses, and routes -- the
//! part of "configure the interface the way innernet does" that the
//! `wireguard-control` crate itself doesn't cover (it only configures
//! the WireGuard-specific generic-netlink family: keys, listen port,
//! peers). Adapted from innernet's `shared/src/netlink.rs`, simplified
//! to IPv4-only since this project's tunnel addresses and routes are
//! always IPv4 (docs/wg-server.md §6).

use std::io;
use std::net::Ipv4Addr;

use ipnet::Ipv4Net;
use netlink_packet_core::{
    NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_REPLACE, NLM_F_REQUEST, NetlinkPayload,
};
use netlink_packet_route::{
    AddressFamily, RouteNetlinkMessage,
    address::{self, AddressHeader, AddressMessage},
    link::{self, LinkFlags, LinkHeader, LinkMessage},
    route::{self, RouteHeader, RouteMessage},
};
use netlink_request::netlink_request_rtnl;
use wireguard_control::InterfaceName;

fn if_index(name: &InterfaceName) -> Result<u32, io::Error> {
    match unsafe { libc::if_nametoindex(name.as_ptr()) } {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {name} not found"),
        )),
        index => Ok(index),
    }
}

/// The interface index for `name`, if it currently exists. Used by the
/// route-conflict preflight to recognize the managed interface's own
/// (about-to-be-replaced) routes among the dumped routing table.
pub fn interface_index(name: &str) -> Option<u32> {
    let name: InterfaceName = name.parse().ok()?;
    if_index(&name).ok()
}

/// The interface name for a route's outbound interface index, best-effort
/// (only used to make a preflight rejection message readable).
pub fn if_name_from_index(index: u32) -> Option<String> {
    let mut buf = [0i8; libc::IF_NAMESIZE];
    let ptr = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if ptr.is_null() {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    cstr.to_str().ok().map(str::to_string)
}

/// List every IPv4 unicast route currently installed in the main routing
/// table, as (destination, outbound interface index) pairs -- used by the
/// route-conflict preflight (wg-client.md §6: "check candidate routes
/// against the local routing table").
pub fn list_ipv4_routes() -> Result<Vec<(Ipv4Net, u32)>, io::Error> {
    let responses = netlink_request_rtnl(
        RouteNetlinkMessage::GetRoute(RouteMessage::default()),
        Some(NLM_F_REQUEST | NLM_F_DUMP),
    )?;
    let mut routes = Vec::new();
    for response in responses {
        let NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewRoute(msg)) = response.payload
        else {
            continue;
        };
        if msg.header.address_family != AddressFamily::Inet
            || msg.header.table != RouteHeader::RT_TABLE_MAIN
            || msg.header.kind != route::RouteType::Unicast
        {
            continue;
        }
        let mut dest = Ipv4Addr::UNSPECIFIED;
        let mut oif = None;
        for attr in &msg.attributes {
            match attr {
                route::RouteAttribute::Destination(route::RouteAddress::Inet(addr)) => {
                    dest = *addr;
                }
                route::RouteAttribute::Oif(index) => oif = Some(*index),
                _ => {}
            }
        }
        if let (Some(oif), Ok(net)) = (
            oif,
            Ipv4Net::new(dest, msg.header.destination_prefix_length),
        ) {
            routes.push((net, oif));
        }
    }
    Ok(routes)
}

/// Set the interface administratively up with the given MTU.
pub fn set_up(name: &InterfaceName, mtu: u32) -> Result<(), io::Error> {
    let index = if_index(name)?;
    let mut message = LinkMessage::default();
    message.header = LinkHeader {
        index,
        flags: LinkFlags::Up,
        ..Default::default()
    };
    message.attributes = vec![link::LinkAttribute::Mtu(mtu)];
    netlink_request_rtnl(RouteNetlinkMessage::SetLink(message), None)?;
    tracing::debug!("set interface {name} up with mtu {mtu}");
    Ok(())
}

/// Assign an IPv4 address (with prefix length) to the interface.
pub fn set_addr(name: &InterfaceName, addr: Ipv4Addr, prefix_len: u8) -> Result<(), io::Error> {
    let index = if_index(name)?;
    let ip = std::net::IpAddr::V4(addr);
    let mut message = AddressMessage::default();
    message.header = AddressHeader {
        index,
        family: AddressFamily::Inet,
        prefix_len,
        scope: address::AddressScope::Universe,
        ..Default::default()
    };
    message.attributes = vec![
        address::AddressAttribute::Local(ip),
        address::AddressAttribute::Address(ip),
    ];
    netlink_request_rtnl(
        RouteNetlinkMessage::NewAddress(message),
        Some(NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE | NLM_F_CREATE),
    )?;
    tracing::debug!("set address {addr}/{prefix_len} on interface {name}");
    Ok(())
}

/// Add a route for `net` via the interface. Returns `Ok(false)` (not an
/// error) if the route already existed -- reapplying an unchanged
/// configuration must not fail (wg-client.md §9's no-flap requirement).
pub fn add_route(name: &InterfaceName, net: Ipv4Net) -> Result<bool, io::Error> {
    let index = if_index(name)?;
    let mut message = RouteMessage::default();
    message.header = RouteHeader {
        table: RouteHeader::RT_TABLE_MAIN,
        protocol: route::RouteProtocol::Boot,
        scope: route::RouteScope::Link,
        kind: route::RouteType::Unicast,
        destination_prefix_length: net.prefix_len(),
        address_family: AddressFamily::Inet,
        ..Default::default()
    };
    message.attributes = vec![
        route::RouteAttribute::Destination(route::RouteAddress::Inet(net.network())),
        route::RouteAttribute::Oif(index),
    ];
    match netlink_request_rtnl(RouteNetlinkMessage::NewRoute(message), None) {
        Ok(_) => {
            tracing::debug!("added route {net} to interface {name}");
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            tracing::debug!("route {net} already existed on interface {name}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}
