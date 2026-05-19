//! Minimal NETLINK_ROUTE / rtnetlink surface for namespace-owned links.
//!
//! This is intentionally an adapter over the existing net namespace and
//! net-device objects. It does not own a side socket table, routing table,
//! or Docker-specific state.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::str;
use core::sync::atomic::{AtomicU32, Ordering};

use smoltcp::time::Instant;
use tx_substrate::zone::{Cap, PayloadCap};

use crate::cred::Cred;
use crate::device::DevT;
use crate::execution::Errno;
use crate::net::admin::{require_net_admin, NetAdminAuthority};
use crate::net::device::{
    create_bridge_for_test_or_bootstrap, create_veth_pair_for_test_or_bootstrap, BridgeConfig,
    EthernetAddress, NetDeviceKind, VethEndpointConfig, VethPairConfig, VETH_DEFAULT_MTU,
};
use crate::net::namespace::{NetNamespaceLinkInfo, NetNamespacePayload};
use crate::net::protocol::ArpSnapshotState;
use crate::net::structure::{Ipv4Address, RecvWireSet, SendRecvFlags, SocketIdentity, SocketKind};
use crate::sync::SpinMutex;

pub const AF_NETLINK: i32 = 16;
pub const NETLINK_ROUTE: i32 = 0;

pub const NLM_F_REQUEST: u16 = 0x0001;
pub const NLM_F_MULTI: u16 = 0x0002;
pub const NLM_F_ACK: u16 = 0x0004;
pub const NLM_F_ROOT: u16 = 0x0100;
pub const NLM_F_MATCH: u16 = 0x0200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_SETLINK: u16 = 19;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_NEWROUTE: u16 = 24;
pub const RTM_GETROUTE: u16 = 26;
pub const RTM_NEWNEIGH: u16 = 28;
pub const RTM_GETNEIGH: u16 = 30;

const NLMSG_HDR_LEN: usize = 16;
const IFINFO_MSG_LEN: usize = 16;
const IFADDR_MSG_LEN: usize = 8;
const RTMSG_LEN: usize = 12;
const NLA_HDR_LEN: usize = 4;
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLA_F_NESTED: u16 = 0x8000;

const AF_INET: u8 = 2;
const AF_UNSPEC: u8 = 0;
const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;

const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_PREFSRC: u16 = 7;

const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;

const RT_TABLE_MAIN: u8 = 254;
const RTPROT_KERNEL: u8 = 2;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;

const NUD_INCOMPLETE: u16 = 0x01;
const NUD_REACHABLE: u16 = 0x02;
const NUD_FAILED: u16 = 0x20;

const RTNL_DYNAMIC_NET_MAJOR: u32 = 94;
static NEXT_RTNL_DYNAMIC_MINOR: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetlinkRouteState;

pub struct RawNetlinkRouteSocket {
    rx: SpinMutex<VecDeque<Vec<u8>>>,
}

impl RawNetlinkRouteSocket {
    pub fn new() -> Self {
        Self {
            rx: SpinMutex::new(VecDeque::new()),
        }
    }

    pub fn queue_response(&self, bytes: Vec<u8>) {
        self.rx.lock().push_back(bytes);
    }

    pub fn pop_response(&self, peek: bool) -> Option<Vec<u8>> {
        let mut rx = self.rx.lock();
        if peek {
            rx.front().cloned()
        } else {
            rx.pop_front()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rx.lock().is_empty()
    }

    pub fn recv_len(&self, len: usize) -> Option<(usize, bool)> {
        let rx = self.rx.lock();
        let front = rx.front()?;
        Some((core::cmp::min(len, front.len()), false))
    }

    pub fn recv_available(&self) -> usize {
        self.rx.lock().front().map_or(0, Vec::len)
    }

    pub const fn send_available(&self) -> usize {
        usize::MAX / 2
    }
}

pub fn netlink_route_send(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    cred: Cred,
) -> Result<usize, Errno> {
    let mut no_netns_fd = |_fd: i32| None;
    let mut no_netns_pid = |_pid: u32| None;
    netlink_route_send_with_netns_resolvers(
        socket,
        bytes,
        cred,
        &mut no_netns_fd,
        &mut no_netns_pid,
    )
}

pub fn netlink_route_send_with_netns_resolver<F>(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    cred: Cred,
    resolve_netns_fd: &mut F,
) -> Result<usize, Errno>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let mut no_netns_pid = |_pid: u32| None;
    netlink_route_send_with_netns_resolvers(
        socket,
        bytes,
        cred,
        resolve_netns_fd,
        &mut no_netns_pid,
    )
}

pub fn netlink_route_send_with_netns_resolvers<F, G>(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    cred: Cred,
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut G,
) -> Result<usize, Errno>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
    G: FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    if socket.kind != SocketKind::NetlinkRoute {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_route_socket()
        .ok_or(Errno::EOPNOTSUPP)?;

    for response in rtnetlink_handle_request_with_netns_resolvers(
        &payload.net_namespace(),
        cred,
        bytes,
        resolve_netns_fd,
        resolve_netns_pid,
    ) {
        raw.queue_response(response);
    }
    payload.refresh_io_from_raw();
    if !raw.is_empty() {
        socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    Ok(bytes.len())
}

pub fn netlink_route_recv(
    socket: &Cap<SocketIdentity>,
    out: &mut [u8],
    flags: SendRecvFlags,
) -> Result<usize, Errno> {
    if socket.kind != SocketKind::NetlinkRoute {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_route_socket()
        .ok_or(Errno::EOPNOTSUPP)?;
    let peek = flags.contains(SendRecvFlags::MSG_PEEK);
    let response = raw.pop_response(peek).ok_or(Errno::EAGAIN)?;
    let copied = core::cmp::min(out.len(), response.len());
    out[..copied].copy_from_slice(&response[..copied]);
    payload.refresh_io_from_raw();
    if !peek && raw.is_empty() {
        socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    }
    Ok(copied)
}

pub fn rtnetlink_handle_request(
    netns: &NetNamespacePayload,
    cred: Cred,
    request: &[u8],
) -> Vec<Vec<u8>> {
    let mut no_netns_fd = |_fd: i32| None;
    let mut no_netns_pid = |_pid: u32| None;
    rtnetlink_handle_request_with_netns_resolvers(
        netns,
        cred,
        request,
        &mut no_netns_fd,
        &mut no_netns_pid,
    )
}

pub fn rtnetlink_handle_request_with_netns_resolver<F>(
    netns: &NetNamespacePayload,
    cred: Cred,
    request: &[u8],
    resolve_netns_fd: &mut F,
) -> Vec<Vec<u8>>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let mut no_netns_pid = |_pid: u32| None;
    rtnetlink_handle_request_with_netns_resolvers(
        netns,
        cred,
        request,
        resolve_netns_fd,
        &mut no_netns_pid,
    )
}

pub fn rtnetlink_handle_request_with_netns_resolvers<F, G>(
    netns: &NetNamespacePayload,
    cred: Cred,
    request: &[u8],
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut G,
) -> Vec<Vec<u8>>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
    G: FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let mut responses = Vec::new();
    let mut offset = 0usize;
    while offset < request.len() {
        if request.len() - offset < NLMSG_HDR_LEN {
            responses.push(build_error_response(None, Errno::EINVAL));
            break;
        }

        let Some(header) = parse_nlmsg_header(&request[offset..]) else {
            responses.push(build_error_response(None, Errno::EINVAL));
            break;
        };
        let msg_len = header.len as usize;
        if msg_len < NLMSG_HDR_LEN || offset.saturating_add(msg_len) > request.len() {
            responses.push(build_error_response(Some(header), Errno::EINVAL));
            break;
        }

        let payload = &request[offset + NLMSG_HDR_LEN..offset + msg_len];
        handle_one_message(
            netns,
            cred,
            header,
            payload,
            resolve_netns_fd,
            resolve_netns_pid,
            &mut responses,
        );
        offset += align4(msg_len);
    }
    responses
}

fn handle_one_message<F>(
    netns: &NetNamespacePayload,
    cred: Cred,
    header: NlMsgHeader,
    payload: &[u8],
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut impl FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
    responses: &mut Vec<Vec<u8>>,
) where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    match header.kind {
        RTM_GETLINK => {
            for msg in render_getlink_dump(netns, header) {
                responses.push(msg);
            }
        }
        RTM_GETADDR => {
            for msg in render_getaddr_dump(netns, header) {
                responses.push(msg);
            }
        }
        RTM_GETROUTE => {
            for msg in render_getroute_dump(netns, header) {
                responses.push(msg);
            }
        }
        RTM_GETNEIGH => {
            for msg in render_getneigh_dump(netns, header) {
                responses.push(msg);
            }
        }
        RTM_NEWLINK => {
            let result = handle_newlink(netns, cred, payload);
            responses.push(ack_or_error(header, result));
        }
        RTM_SETLINK => {
            let result = handle_setlink(netns, cred, payload, resolve_netns_fd, resolve_netns_pid);
            responses.push(ack_or_error(header, result));
        }
        RTM_NEWADDR => {
            let result = handle_newaddr(netns, cred, payload);
            responses.push(ack_or_error(header, result));
        }
        _ => responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP)),
    }
}

fn render_getlink_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let snapshot = netns.network_snapshot();
    let mut out = Vec::new();
    for link in &snapshot.links {
        out.push(build_link_message(
            header.seq,
            header.pid,
            RTM_NEWLINK,
            NLM_F_MULTI,
            link,
            &snapshot.links,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_getaddr_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let snapshot = netns.network_snapshot();
    let mut out = Vec::new();
    for link in &snapshot.links {
        if link.ipv4_addr.is_some() {
            out.push(build_addr_message(
                header.seq,
                header.pid,
                NLM_F_MULTI,
                link,
            ));
        }
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_getroute_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let snapshot = netns.network_snapshot();
    let mut out = Vec::new();
    for link in &snapshot.links {
        if link.ipv4_addr.is_some() && !link.is_loopback {
            out.push(build_route_message(
                header.seq,
                header.pid,
                NLM_F_MULTI,
                link,
            ));
        }
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_getneigh_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let links = netns.link_snapshot();
    let mut out = Vec::new();
    for iface in netns.ether_ifaces_snapshot() {
        let Some(link) = links.iter().find(|link| link.name == iface.name) else {
            continue;
        };
        for entry in iface.arp_snapshot(Instant::ZERO) {
            out.push(build_neigh_message(
                header.seq,
                header.pid,
                NLM_F_MULTI,
                link.ifindex,
                entry.ip,
                entry.mac,
                entry.state,
            ));
        }
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn handle_newlink(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let attrs = parse_ifinfomsg(payload)?.attrs;
    let name = attr_string(&attrs, IFLA_IFNAME).ok_or(Errno::EINVAL)?;
    validate_ifname(name)?;
    let link_info = parse_linkinfo(&attrs)?;

    match link_info.kind {
        Some("bridge") => create_bridge_link(netns, auth, name),
        Some("veth") => {
            let peer_name = match link_info.peer_name {
                Some(peer_name) => peer_name,
                None => default_veth_peer_name(netns, name)?,
            };
            validate_ifname(peer_name)?;
            create_veth_links(netns, auth, name, peer_name)
        }
        _ => Err(Errno::EOPNOTSUPP),
    }
}

fn handle_setlink<F>(
    netns: &NetNamespacePayload,
    cred: Cred,
    payload: &[u8],
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut impl FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
) -> Result<(), Errno>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let auth = require_net_admin(cred)?;
    let info = parse_ifinfomsg(payload)?;

    let target = link_target_from_info_or_attrs(netns, &info, &info.attrs)?;

    if info.change & IFF_UP != 0 {
        netns.set_device_up_by_ifindex(auth, target.ifindex, info.flags & IFF_UP != 0)?;
    }

    if let Some(master_attr) = attr_by_kind(&info.attrs, IFLA_MASTER) {
        if master_attr.payload.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let master_ifindex = read_u32(master_attr.payload, 0);
        if master_ifindex == 0 {
            return Err(Errno::ENOSYS);
        }
        let port = netns
            .find_device_by_ifindex(target.ifindex)
            .ok_or(Errno::ENODEV)?;
        let master = netns
            .find_device_by_ifindex(master_ifindex)
            .ok_or(Errno::ENODEV)?;
        master.ops.bridge_add_port(auth, port)?;
    }

    if let Some(netns_fd_attr) = attr_by_kind(&info.attrs, IFLA_NET_NS_FD) {
        if netns_fd_attr.payload.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let fd = read_i32(netns_fd_attr.payload, 0);
        if fd < 0 {
            return Err(Errno::EBADF);
        }
        let target_netns = resolve_netns_fd(fd).ok_or(Errno::EBADF)?;
        netns.move_device_to_namespace_by_ifindex(auth, target.ifindex, &target_netns)?;
    }

    if let Some(netns_pid_attr) = attr_by_kind(&info.attrs, IFLA_NET_NS_PID) {
        if netns_pid_attr.payload.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let pid = read_i32(netns_pid_attr.payload, 0);
        if pid <= 0 {
            return Err(Errno::ESRCH);
        }
        let target_netns = resolve_netns_pid(pid as u32).ok_or(Errno::ESRCH)?;
        netns.move_device_to_namespace_by_ifindex(auth, target.ifindex, &target_netns)?;
    }

    Ok(())
}

fn handle_newaddr(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let info = parse_ifaddrmsg(payload)?;
    if info.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let attrs = parse_attrs(&payload[IFADDR_MSG_LEN..])?;
    let addr_attr = attr_by_kind(&attrs, IFA_LOCAL)
        .or_else(|| attr_by_kind(&attrs, IFA_ADDRESS))
        .ok_or(Errno::EINVAL)?;
    if addr_attr.payload.len() < 4 {
        return Err(Errno::EINVAL);
    }
    let addr = Ipv4Address::new([
        addr_attr.payload[0],
        addr_attr.payload[1],
        addr_attr.payload[2],
        addr_attr.payload[3],
    ]);
    let ifindex = if info.index != 0 {
        info.index
    } else {
        let label = attr_string(&attrs, IFA_LABEL).ok_or(Errno::ENODEV)?;
        netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.name == label)
            .map(|link| link.ifindex)
            .ok_or(Errno::ENODEV)?
    };
    netns.set_device_ipv4_addr_by_ifindex(auth, ifindex, Some(addr), Some(info.prefix_len))
}

fn create_bridge_link(
    netns: &NetNamespacePayload,
    auth: NetAdminAuthority,
    name: &str,
) -> Result<(), Errno> {
    if netns.find_device_by_name(name).is_some() {
        return Err(Errno::EEXIST);
    }
    let leaked_name = leak_ifname(name);
    let instance = create_bridge_for_test_or_bootstrap(BridgeConfig {
        name: leaked_name,
        devt: allocate_dynamic_devt(),
        mac: allocate_dynamic_mac(0x71),
        mtu: VETH_DEFAULT_MTU,
    });
    netns.attach_device(auth, instance.registration, None)
}

fn create_veth_links(
    netns: &NetNamespacePayload,
    auth: NetAdminAuthority,
    left_name: &str,
    right_name: &str,
) -> Result<(), Errno> {
    if netns.find_device_by_name(left_name).is_some()
        || netns.find_device_by_name(right_name).is_some()
    {
        return Err(Errno::EEXIST);
    }

    let left_name = leak_ifname(left_name);
    let right_name = leak_ifname(right_name);
    let left_minor = allocate_dynamic_minor();
    let right_minor = allocate_dynamic_minor();
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: left_name,
            devt: DevT::new(RTNL_DYNAMIC_NET_MAJOR, left_minor),
            mac: dynamic_mac_from_minor(0x72, left_minor),
        },
        right: VethEndpointConfig {
            name: right_name,
            devt: DevT::new(RTNL_DYNAMIC_NET_MAJOR, right_minor),
            mac: dynamic_mac_from_minor(0x72, right_minor),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    netns.attach_device(auth, pair.left, None)?;
    netns.attach_device(auth, pair.right, None)
}

fn default_veth_peer_name(
    netns: &NetNamespacePayload,
    left_name: &str,
) -> Result<&'static str, Errno> {
    if left_name != "eth0" && netns.find_device_by_name("eth0").is_none() {
        return Ok("eth0");
    }

    for idx in 1..256 {
        let candidate = format!("eth{}", idx);
        if candidate != left_name && netns.find_device_by_name(&candidate).is_none() {
            return Ok(leak_ifname(&candidate));
        }
    }
    Err(Errno::ENOMEM)
}

fn build_link_message(
    seq: u32,
    pid: u32,
    kind: u16,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    all_links: &[NetNamespaceLinkInfo],
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(AF_UNSPEC);
    payload.push(0);
    payload.extend_from_slice(&arphrd_for_link(link).to_le_bytes());
    payload.extend_from_slice(&(link.ifindex as i32).to_le_bytes());
    payload.extend_from_slice(&link_flags(link).to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());

    push_attr_string(&mut payload, IFLA_IFNAME, link.name);
    push_attr_u32(&mut payload, IFLA_MTU, u32::from(link.mtu));
    if let Some(mac) = link.mac {
        push_attr(&mut payload, IFLA_ADDRESS, &mac.octets());
    }
    if let Some(master_name) = link.master {
        if let Some(master) = all_links
            .iter()
            .find(|candidate| candidate.name == master_name)
        {
            push_attr_u32(&mut payload, IFLA_MASTER, master.ifindex);
        }
    }
    push_nested_attr(&mut payload, IFLA_LINKINFO, |nested| {
        push_attr_string(nested, IFLA_INFO_KIND, link_kind_name(link.kind));
    });

    build_nlmsg(kind, flags, seq, pid, &payload)
}

fn build_addr_message(seq: u32, pid: u32, flags: u16, link: &NetNamespaceLinkInfo) -> Vec<u8> {
    let Some(addr) = link.ipv4_addr else {
        return build_done_message(seq, pid);
    };
    let mut payload = Vec::new();
    payload.push(AF_INET);
    payload.push(link.ipv4_prefix_len.unwrap_or(32));
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(&link.ifindex.to_le_bytes());
    push_attr(&mut payload, IFA_ADDRESS, &addr.octets());
    push_attr(&mut payload, IFA_LOCAL, &addr.octets());
    push_attr_string(&mut payload, IFA_LABEL, link.name);
    build_nlmsg(RTM_NEWADDR, flags, seq, pid, &payload)
}

fn build_route_message(seq: u32, pid: u32, flags: u16, link: &NetNamespaceLinkInfo) -> Vec<u8> {
    let Some(addr) = link.ipv4_addr else {
        return build_done_message(seq, pid);
    };
    let prefix_len = link.ipv4_prefix_len.unwrap_or(32).min(32);
    let dst = ipv4_network(addr, prefix_len);
    let mut payload = Vec::with_capacity(RTMSG_LEN + 32);
    payload.push(AF_INET);
    payload.push(prefix_len);
    payload.push(0);
    payload.push(0);
    payload.push(RT_TABLE_MAIN);
    payload.push(RTPROT_KERNEL);
    payload.push(RT_SCOPE_LINK);
    payload.push(RTN_UNICAST);
    payload.extend_from_slice(&0u32.to_le_bytes());
    push_attr(&mut payload, RTA_DST, &dst.octets());
    push_attr_u32(&mut payload, RTA_OIF, link.ifindex);
    push_attr(&mut payload, RTA_PREFSRC, &addr.octets());
    build_nlmsg(RTM_NEWROUTE, flags, seq, pid, &payload)
}

fn build_neigh_message(
    seq: u32,
    pid: u32,
    flags: u16,
    ifindex: u32,
    ip: Ipv4Address,
    mac: Option<EthernetAddress>,
    state: ArpSnapshotState,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(AF_INET);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&(ifindex as i32).to_le_bytes());
    payload.extend_from_slice(&neigh_state(state).to_le_bytes());
    payload.push(0);
    payload.push(0);
    push_attr(&mut payload, NDA_DST, &ip.octets());
    if let Some(mac) = mac {
        push_attr(&mut payload, NDA_LLADDR, &mac.octets());
    }
    build_nlmsg(RTM_NEWNEIGH, flags, seq, pid, &payload)
}

fn link_flags(link: &NetNamespaceLinkInfo) -> u32 {
    let mut flags = IFF_RUNNING;
    if link.is_up {
        flags |= IFF_UP;
    }
    if link.is_loopback {
        flags |= IFF_LOOPBACK;
    } else {
        flags |= IFF_BROADCAST | IFF_MULTICAST;
    }
    flags
}

fn neigh_state(state: ArpSnapshotState) -> u16 {
    match state {
        ArpSnapshotState::Resolved => NUD_REACHABLE,
        ArpSnapshotState::Pending => NUD_INCOMPLETE,
        ArpSnapshotState::Failed => NUD_FAILED,
    }
}

fn ipv4_network(addr: Ipv4Address, prefix_len: u8) -> Ipv4Address {
    let prefix_len = prefix_len.min(32);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    };
    Ipv4Address::new((u32::from_be_bytes(addr.octets()) & mask).to_be_bytes())
}

fn arphrd_for_link(link: &NetNamespaceLinkInfo) -> u16 {
    if link.is_loopback {
        ARPHRD_LOOPBACK
    } else {
        ARPHRD_ETHER
    }
}

fn link_kind_name(kind: NetDeviceKind) -> &'static str {
    match kind {
        NetDeviceKind::Loopback => "loopback",
        NetDeviceKind::Ethernet => "ether",
        NetDeviceKind::Veth => "veth",
        NetDeviceKind::Bridge => "bridge",
    }
}

fn parse_linkinfo<'a>(attrs: &'a [NlAttr<'a>]) -> Result<LinkInfoAttrs<'a>, Errno> {
    let Some(linkinfo) = attr_by_kind(attrs, IFLA_LINKINFO) else {
        return Ok(LinkInfoAttrs {
            kind: None,
            peer_name: None,
        });
    };
    let nested = parse_attrs(linkinfo.payload)?;
    let kind = attr_string(&nested, IFLA_INFO_KIND);
    let mut peer_name = None;

    if let Some(info_data) = attr_by_kind(&nested, IFLA_INFO_DATA) {
        let data_attrs = parse_attrs(info_data.payload)?;
        if let Some(peer) = attr_by_kind(&data_attrs, VETH_INFO_PEER) {
            let peer_info = parse_ifinfomsg(peer.payload)?;
            peer_name = attr_string(&peer_info.attrs, IFLA_IFNAME);
        }
    }

    Ok(LinkInfoAttrs { kind, peer_name })
}

fn link_target_from_info_or_attrs<'a>(
    netns: &NetNamespacePayload,
    info: &IfInfoMsg<'a>,
    attrs: &[NlAttr<'a>],
) -> Result<NetNamespaceLinkInfo, Errno> {
    if info.index != 0 {
        return netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == info.index)
            .ok_or(Errno::ENODEV);
    }
    let name = attr_string(attrs, IFLA_IFNAME).ok_or(Errno::EINVAL)?;
    netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == name)
        .ok_or(Errno::ENODEV)
}

fn parse_ifinfomsg(payload: &[u8]) -> Result<IfInfoMsg<'_>, Errno> {
    if payload.len() < IFINFO_MSG_LEN {
        return Err(Errno::EINVAL);
    }
    Ok(IfInfoMsg {
        _family: payload[0],
        index: parse_ifindex(payload, 4)?,
        flags: read_u32(payload, 8),
        change: read_u32(payload, 12),
        attrs: parse_attrs(&payload[IFINFO_MSG_LEN..])?,
    })
}

fn parse_ifaddrmsg(payload: &[u8]) -> Result<IfAddrMsg, Errno> {
    if payload.len() < IFADDR_MSG_LEN {
        return Err(Errno::EINVAL);
    }
    Ok(IfAddrMsg {
        family: payload[0],
        prefix_len: payload[1],
        index: read_u32(payload, 4),
    })
}

fn parse_attrs(mut bytes: &[u8]) -> Result<Vec<NlAttr<'_>>, Errno> {
    let mut attrs = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < NLA_HDR_LEN {
            if bytes.iter().all(|byte| *byte == 0) {
                break;
            }
            return Err(Errno::EINVAL);
        }
        let len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        if len < NLA_HDR_LEN || len > bytes.len() {
            return Err(Errno::EINVAL);
        }
        let raw_kind = u16::from_le_bytes([bytes[2], bytes[3]]);
        attrs.push(NlAttr {
            kind: raw_kind & NLA_TYPE_MASK,
            payload: &bytes[NLA_HDR_LEN..len],
        });
        let aligned = align4(len);
        if aligned > bytes.len() {
            break;
        }
        bytes = &bytes[aligned..];
    }
    Ok(attrs)
}

fn attr_by_kind<'a>(attrs: &[NlAttr<'a>], kind: u16) -> Option<NlAttr<'a>> {
    attrs.iter().copied().find(|attr| attr.kind == kind)
}

fn attr_string<'a>(attrs: &[NlAttr<'a>], kind: u16) -> Option<&'a str> {
    let attr = attr_by_kind(attrs, kind)?;
    let raw = if attr.payload.last().copied() == Some(0) {
        &attr.payload[..attr.payload.len().saturating_sub(1)]
    } else {
        attr.payload
    };
    str::from_utf8(raw).ok()
}

fn validate_ifname(name: &str) -> Result<(), Errno> {
    if name.is_empty() || name.len() >= 16 || name.as_bytes().contains(&0) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn build_nlmsg(kind: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let msg_len = NLMSG_HDR_LEN + payload.len();
    let mut out = Vec::with_capacity(align4(msg_len));
    out.extend_from_slice(&(msg_len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(&mut out);
    out
}

fn build_done_message(seq: u32, pid: u32) -> Vec<u8> {
    build_nlmsg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes())
}

fn ack_or_error(header: NlMsgHeader, result: Result<(), Errno>) -> Vec<u8> {
    match result {
        Ok(()) => build_ack_response(header),
        Err(errno) => build_error_response(Some(header), errno),
    }
}

fn build_ack_response(header: NlMsgHeader) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0i32.to_le_bytes());
    push_original_header(&mut payload, header);
    build_nlmsg(NLMSG_ERROR, 0, header.seq, header.pid, &payload)
}

fn build_error_response(header: Option<NlMsgHeader>, errno: Errno) -> Vec<u8> {
    let mut payload = Vec::new();
    let error = -linux_errno_i32(errno);
    payload.extend_from_slice(&error.to_le_bytes());
    if let Some(header) = header {
        push_original_header(&mut payload, header);
        build_nlmsg(NLMSG_ERROR, 0, header.seq, header.pid, &payload)
    } else {
        build_nlmsg(NLMSG_ERROR, 0, 0, 0, &payload)
    }
}

fn push_original_header(out: &mut Vec<u8>, header: NlMsgHeader) {
    out.extend_from_slice(&header.len.to_le_bytes());
    out.extend_from_slice(&header.kind.to_le_bytes());
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.pid.to_le_bytes());
}

fn push_attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = NLA_HDR_LEN + payload.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(out);
}

fn push_nested_attr(out: &mut Vec<u8>, kind: u16, build: impl FnOnce(&mut Vec<u8>)) {
    let mut nested = Vec::new();
    build(&mut nested);
    push_attr(out, kind | NLA_F_NESTED, &nested);
}

fn push_attr_string(out: &mut Vec<u8>, kind: u16, value: &str) {
    let mut bytes = Vec::with_capacity(value.len() + 1);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    push_attr(out, kind, &bytes);
}

fn push_attr_u32(out: &mut Vec<u8>, kind: u16, value: u32) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn parse_nlmsg_header(bytes: &[u8]) -> Option<NlMsgHeader> {
    if bytes.len() < NLMSG_HDR_LEN {
        return None;
    }
    Some(NlMsgHeader {
        len: read_u32(bytes, 0),
        kind: read_u16(bytes, 4),
        flags: read_u16(bytes, 6),
        seq: read_u32(bytes, 8),
        pid: read_u32(bytes, 12),
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn parse_ifindex(bytes: &[u8], offset: usize) -> Result<u32, Errno> {
    let index = read_i32(bytes, offset);
    if index < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(index as u32)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn align4(len: usize) -> usize {
    (len + 3) & !3
}

fn pad_to_align4(out: &mut Vec<u8>) {
    while out.len() != align4(out.len()) {
        out.push(0);
    }
}

fn leak_ifname(name: &str) -> &'static str {
    Box::leak(String::from(name).into_boxed_str())
}

fn allocate_dynamic_devt() -> DevT {
    DevT::new(RTNL_DYNAMIC_NET_MAJOR, allocate_dynamic_minor())
}

fn allocate_dynamic_minor() -> u32 {
    NEXT_RTNL_DYNAMIC_MINOR.fetch_add(1, Ordering::AcqRel)
}

fn allocate_dynamic_mac(tag: u8) -> EthernetAddress {
    dynamic_mac_from_minor(tag, allocate_dynamic_minor())
}

fn dynamic_mac_from_minor(tag: u8, minor: u32) -> EthernetAddress {
    EthernetAddress::new([
        0x02,
        0,
        0,
        tag,
        ((minor >> 8) & 0xff) as u8,
        (minor & 0xff) as u8,
    ])
}

fn linux_errno_i32(errno: Errno) -> i32 {
    match errno {
        Errno::EACCES => 13,
        Errno::EADDRINUSE => 98,
        Errno::EAFNOSUPPORT => 97,
        Errno::EAGAIN => 11,
        Errno::EBADF => 9,
        Errno::EEXIST => 17,
        Errno::EFAULT => 14,
        Errno::EINVAL => 22,
        Errno::EIO => 5,
        Errno::ENODEV => 19,
        Errno::ENOMEM => 12,
        Errno::ENOENT => 2,
        Errno::ENOSYS => 38,
        Errno::EPERM => 1,
        Errno::EOPNOTSUPP => 95,
        _ => 5,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NlMsgHeader {
    len: u32,
    kind: u16,
    flags: u16,
    seq: u32,
    pid: u32,
}

#[derive(Clone, Copy)]
struct NlAttr<'a> {
    kind: u16,
    payload: &'a [u8],
}

struct IfInfoMsg<'a> {
    _family: u8,
    index: u32,
    flags: u32,
    change: u32,
    attrs: Vec<NlAttr<'a>>,
}

struct IfAddrMsg {
    family: u8,
    prefix_len: u8,
    index: u32,
}

struct LinkInfoAttrs<'a> {
    kind: Option<&'a str>,
    peer_name: Option<&'a str>,
}
