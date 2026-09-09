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
use netlink_packet_core::{NLM_F_ACK, NLM_F_CREATE, NLM_F_REPLACE, NLM_F_REQUEST};
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
