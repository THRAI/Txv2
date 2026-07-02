//! Minimal NETLINK_ROUTE / rtnetlink surface for namespace-owned links.
//!
//! This is intentionally an adapter over the existing net namespace and
//! net-device objects. It does not own a side socket table, routing table,
//! or Docker-specific state.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
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
    create_bridge_for_test_or_bootstrap, create_dummy_for_test_or_bootstrap,
    create_veth_pair_for_test_or_bootstrap, create_vlan_for_test_or_bootstrap, BridgeConfig,
    DummyConfig, EthernetAddress, NetDeviceKind, VethEndpointConfig, VethPairConfig, VlanConfig,
    DUMMY_DEFAULT_MTU, VETH_DEFAULT_MTU, VLAN_DEFAULT_MTU,
};
use crate::net::namespace::{
    NetNamespaceLinkInfo, NetNamespacePayload, NetNamespaceRouteConfig, NetNamespaceRouteInfo,
    NetNamespaceRouteSelector,
};
use crate::net::protocol::ArpSnapshotState;
use crate::net::structure::{
    Ipv4Address, Ipv6Address, RecvWireSet, SendRecvFlags, SocketIdentity, SocketKind,
};
use crate::sync::SpinMutex;

pub const AF_NETLINK: i32 = 16;
pub const NETLINK_ROUTE: i32 = 0;

pub const NLM_F_REQUEST: u16 = 0x0001;
pub const NLM_F_MULTI: u16 = 0x0002;
pub const NLM_F_ACK: u16 = 0x0004;
pub const NLM_F_ROOT: u16 = 0x0100;
pub const NLM_F_MATCH: u16 = 0x0200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;
const NLM_F_CREATE: u16 = 0x0400;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_DELLINK: u16 = 17;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_SETLINK: u16 = 19;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_DELADDR: u16 = 21;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_NEWROUTE: u16 = 24;
pub const RTM_DELROUTE: u16 = 25;
pub const RTM_GETROUTE: u16 = 26;
pub const RTM_NEWNEIGH: u16 = 28;
pub const RTM_DELNEIGH: u16 = 29;
pub const RTM_GETNEIGH: u16 = 30;

const NLMSG_HDR_LEN: usize = 16;
const IFINFO_MSG_LEN: usize = 16;
const IFADDR_MSG_LEN: usize = 8;
const RTMSG_LEN: usize = 12;
const NDMSG_LEN: usize = 12;
const NLA_HDR_LEN: usize = 4;
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLA_F_NESTED: u16 = 0x8000;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
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
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;

const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;

const RT_TABLE_MAIN: u8 = 254;
const RTPROT_STATIC: u8 = 4;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RTN_UNICAST: u8 = 1;

const NUD_INCOMPLETE: u16 = 0x01;
const NUD_REACHABLE: u16 = 0x02;
const NUD_FAILED: u16 = 0x20;

const RTNL_DYNAMIC_NET_MAJOR: u32 = 94;
static NEXT_RTNL_DYNAMIC_MINOR: AtomicU32 = AtomicU32::new(1);
const GETLINK_DUMP_TEMPLATE_CACHE_LIMIT: usize = 16;
static GETLINK_DUMP_TEMPLATE_CACHE: SpinMutex<Vec<GetlinkDumpTemplateCacheEntry>> =
    SpinMutex::new(Vec::new());
const GETADDR_DUMP_TEMPLATE_CACHE_LIMIT: usize = 16;
static GETADDR_DUMP_TEMPLATE_CACHE: SpinMutex<Vec<GetaddrDumpTemplateCacheEntry>> =
    SpinMutex::new(Vec::new());
const NETLINK_ROUTE_INLINE_RESPONSE_MAX: usize = 192;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetlinkRouteState;

struct GetlinkDumpTemplateCacheEntry {
    netns_key: usize,
    generation: u64,
    template: Vec<u8>,
}

struct GetaddrDumpTemplateCacheEntry {
    netns_key: usize,
    generation: u64,
    template: Vec<u8>,
}

#[derive(Clone)]
pub struct NetlinkRoutePacket {
    inner: NetlinkRoutePacketInner,
}

#[derive(Clone)]
enum NetlinkRoutePacketInner {
    Inline {
        len: usize,
        bytes: [u8; NETLINK_ROUTE_INLINE_RESPONSE_MAX],
    },
    Heap(Vec<u8>),
}

impl NetlinkRoutePacket {
    fn from_vec(bytes: Vec<u8>) -> Self {
        if bytes.len() <= NETLINK_ROUTE_INLINE_RESPONSE_MAX {
            let mut inline = [0u8; NETLINK_ROUTE_INLINE_RESPONSE_MAX];
            inline[..bytes.len()].copy_from_slice(&bytes);
            Self {
                inner: NetlinkRoutePacketInner::Inline {
                    len: bytes.len(),
                    bytes: inline,
                },
            }
        } else {
            Self {
                inner: NetlinkRoutePacketInner::Heap(bytes),
            }
        }
    }

    fn from_template_patched(template: &[u8], seq: u32, pid: u32) -> Self {
        if template.len() <= NETLINK_ROUTE_INLINE_RESPONSE_MAX {
            let mut inline = [0u8; NETLINK_ROUTE_INLINE_RESPONSE_MAX];
            inline[..template.len()].copy_from_slice(template);
            patch_dump_seq_pid(&mut inline[..template.len()], seq, pid);
            Self {
                inner: NetlinkRoutePacketInner::Inline {
                    len: template.len(),
                    bytes: inline,
                },
            }
        } else {
            let mut out = template.to_vec();
            patch_dump_seq_pid(&mut out, seq, pid);
            Self {
                inner: NetlinkRoutePacketInner::Heap(out),
            }
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        match &self.inner {
            NetlinkRoutePacketInner::Inline { len, bytes } => &bytes[..*len],
            NetlinkRoutePacketInner::Heap(bytes) => bytes,
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct RawNetlinkRouteSocket {
    rx: SpinMutex<VecDeque<NetlinkRoutePacket>>,
}

impl Default for RawNetlinkRouteSocket {
    fn default() -> Self {
        Self::new()
    }
}

impl RawNetlinkRouteSocket {
    pub fn new() -> Self {
        Self {
            rx: SpinMutex::new(VecDeque::new()),
        }
    }

    pub fn queue_response(&self, bytes: Vec<u8>) {
        self.rx
            .lock()
            .push_back(NetlinkRoutePacket::from_vec(bytes));
    }

    fn queue_response_from_template(&self, template: &[u8], seq: u32, pid: u32) {
        self.rx
            .lock()
            .push_back(NetlinkRoutePacket::from_template_patched(
                template, seq, pid,
            ));
    }

    pub fn pop_response(&self, peek: bool) -> Option<NetlinkRoutePacket> {
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
        self.rx.lock().front().map_or(0, NetlinkRoutePacket::len)
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

    if try_queue_fast_dump(raw, &payload.net_namespace(), bytes) {
        payload.refresh_io_from_raw();
        socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
        return Ok(bytes.len());
    }

    let responses = rtnetlink_handle_request_with_netns_resolvers(
        &payload.net_namespace(),
        cred,
        bytes,
        resolve_netns_fd,
        resolve_netns_pid,
    );
    if responses.len() == 1 {
        for response in responses {
            raw.queue_response(response);
        }
    } else if !responses.is_empty() {
        let len = responses.iter().map(Vec::len).sum();
        let mut combined = Vec::with_capacity(len);
        for response in responses {
            combined.extend_from_slice(&response);
        }
        raw.queue_response(combined);
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
    let response = netlink_route_recv_packet(socket, flags)?;
    let copied = core::cmp::min(out.len(), response.len());
    out[..copied].copy_from_slice(&response.as_slice()[..copied]);
    let reported = if flags.contains(SendRecvFlags::MSG_TRUNC) {
        response.len()
    } else {
        copied
    };
    Ok(reported)
}

pub fn netlink_route_recv_packet(
    socket: &Cap<SocketIdentity>,
    flags: SendRecvFlags,
) -> Result<NetlinkRoutePacket, Errno> {
    if socket.kind != SocketKind::NetlinkRoute {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_route_socket()
        .ok_or(Errno::EOPNOTSUPP)?;
    let peek = flags.contains(SendRecvFlags::MSG_PEEK);
    let response = raw.pop_response(peek).ok_or(Errno::EAGAIN)?;
    payload.refresh_io_from_raw();
    if !peek && raw.is_empty() {
        socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    }
    Ok(response)
}

pub fn netlink_route_recv_available(socket: &Cap<SocketIdentity>) -> Result<usize, Errno> {
    if socket.kind != SocketKind::NetlinkRoute {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_route_socket()
        .ok_or(Errno::EOPNOTSUPP)?;
    if raw.is_empty() {
        return Err(Errno::EAGAIN);
    }
    Ok(raw.recv_available())
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
            for msg in render_getlink(netns, header, payload) {
                responses.push(msg);
            }
        }
        RTM_GETADDR => {
            for msg in render_getaddr_dump(netns, header, payload) {
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
            let result = handle_newlink(
                netns,
                cred,
                header,
                payload,
                resolve_netns_fd,
                resolve_netns_pid,
            );
            push_ack_or_error(responses, header, result);
        }
        RTM_DELLINK => {
            let result = handle_dellink(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_SETLINK => {
            let result = handle_setlink(netns, cred, payload, resolve_netns_fd, resolve_netns_pid);
            push_ack_or_error(responses, header, result);
        }
        RTM_NEWADDR => {
            let result = handle_newaddr(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_DELADDR => {
            let result = handle_deladdr(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_NEWROUTE => {
            let result = handle_newroute(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_DELROUTE => {
            let result = handle_delroute(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_NEWNEIGH => {
            let result = handle_newneigh(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        RTM_DELNEIGH => {
            let result = handle_delneigh(netns, cred, payload);
            push_ack_or_error(responses, header, result);
        }
        _ => responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP)),
    }
}

fn render_getlink(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    if header.flags & NLM_F_DUMP == NLM_F_DUMP {
        return render_getlink_dump(netns, header);
    }

    let target = match parse_ifinfomsg(payload)
        .and_then(|info| link_target_from_info_or_attrs(netns, &info, &info.attrs))
    {
        Ok(target) => target,
        Err(errno) => return alloc::vec![build_error_response(Some(header), errno)],
    };
    let links = netns.link_snapshot();
    alloc::vec![build_link_message(
        header.seq,
        header.pid,
        RTM_NEWLINK,
        0,
        &target,
        &links,
    )]
}

fn render_getlink_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let links = netns.link_snapshot();
    let mut out = Vec::with_capacity(links.len() + 1);
    for link in &links {
        out.push(build_link_message(
            header.seq,
            header.pid,
            RTM_NEWLINK,
            NLM_F_MULTI,
            link,
            &links,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn try_queue_fast_dump(
    raw: &RawNetlinkRouteSocket,
    netns: &NetNamespacePayload,
    request: &[u8],
) -> bool {
    let Some(header) = parse_single_nlmsg_header(request) else {
        return false;
    };
    if header.flags & NLM_F_DUMP != NLM_F_DUMP {
        return false;
    }
    match header.kind {
        RTM_GETLINK => {
            queue_getlink_dump_cached_combined(raw, netns, header);
            true
        }
        RTM_GETADDR => {
            let msg_len = header.len as usize;
            if request.len() < msg_len || msg_len < NLMSG_HDR_LEN {
                return false;
            }
            let payload = &request[NLMSG_HDR_LEN..msg_len];
            if getaddr_dump_family(payload) != AF_UNSPEC {
                return false;
            }
            queue_getaddr_dump_cached_combined(raw, netns, header);
            true
        }
        _ => false,
    }
}

fn parse_single_nlmsg_header(request: &[u8]) -> Option<NlMsgHeader> {
    let header = parse_nlmsg_header(request)?;
    let msg_len = header.len as usize;
    if msg_len < NLMSG_HDR_LEN || msg_len > request.len() {
        return None;
    }
    if align4(msg_len) != request.len() {
        return None;
    }
    Some(header)
}

fn queue_getlink_dump_cached_combined(
    raw: &RawNetlinkRouteSocket,
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
) {
    let netns_key = netns as *const NetNamespacePayload as usize;
    let generation = netns.link_snapshot_generation();
    let mut cache = GETLINK_DUMP_TEMPLATE_CACHE.lock();
    if let Some(entry) = cache
        .iter()
        .find(|entry| entry.netns_key == netns_key && entry.generation == generation)
    {
        raw.queue_response_from_template(&entry.template, header.seq, header.pid);
        return;
    }

    let template = build_getlink_dump_template(netns);
    raw.queue_response_from_template(&template, header.seq, header.pid);

    if let Some(entry) = cache.iter_mut().find(|entry| entry.netns_key == netns_key) {
        entry.generation = generation;
        entry.template = template;
        return;
    }
    if cache.len() >= GETLINK_DUMP_TEMPLATE_CACHE_LIMIT {
        cache.remove(0);
    }
    cache.push(GetlinkDumpTemplateCacheEntry {
        netns_key,
        generation,
        template,
    });
}

fn build_getlink_dump_template(netns: &NetNamespacePayload) -> Vec<u8> {
    let links = netns.link_snapshot();
    let mut out = Vec::with_capacity(links.len() * 128 + align4(NLMSG_HDR_LEN + 4));
    for link in &links {
        append_link_message(&mut out, 0, 0, RTM_NEWLINK, NLM_F_MULTI, link, &links);
    }
    append_done_message(&mut out, 0, 0);
    out
}

fn patch_dump_seq_pid(out: &mut [u8], seq: u32, pid: u32) {
    let mut offset = 0usize;
    while out.len().saturating_sub(offset) >= NLMSG_HDR_LEN {
        let len = read_u32(out, offset) as usize;
        if len < NLMSG_HDR_LEN || offset.saturating_add(len) > out.len() {
            return;
        }
        out[offset + 8..offset + 12].copy_from_slice(&seq.to_le_bytes());
        out[offset + 12..offset + 16].copy_from_slice(&pid.to_le_bytes());
        offset += align4(len);
    }
}

fn getaddr_dump_family(payload: &[u8]) -> u8 {
    payload.first().copied().unwrap_or(AF_UNSPEC)
}

fn render_getaddr_dump(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    let family = getaddr_dump_family(payload);
    let links = netns.link_snapshot();
    let mut out = Vec::new();
    for link in &links {
        append_addr_messages_as_vec(&mut out, header.seq, header.pid, NLM_F_MULTI, link, family);
    }
    append_extra_addr_messages(
        &mut out,
        header.seq,
        header.pid,
        NLM_F_MULTI,
        netns,
        &links,
        family,
    );
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn queue_getaddr_dump_cached_combined(
    raw: &RawNetlinkRouteSocket,
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
) {
    let netns_key = netns as *const NetNamespacePayload as usize;
    let generation = netns.link_snapshot_generation();
    let mut cache = GETADDR_DUMP_TEMPLATE_CACHE.lock();
    if let Some(entry) = cache
        .iter()
        .find(|entry| entry.netns_key == netns_key && entry.generation == generation)
    {
        raw.queue_response_from_template(&entry.template, header.seq, header.pid);
        return;
    }

    let template = build_getaddr_dump_template(netns);
    raw.queue_response_from_template(&template, header.seq, header.pid);

    if let Some(entry) = cache.iter_mut().find(|entry| entry.netns_key == netns_key) {
        entry.generation = generation;
        entry.template = template;
        return;
    }
    if cache.len() >= GETADDR_DUMP_TEMPLATE_CACHE_LIMIT {
        cache.remove(0);
    }
    cache.push(GetaddrDumpTemplateCacheEntry {
        netns_key,
        generation,
        template,
    });
}

fn build_getaddr_dump_template(netns: &NetNamespacePayload) -> Vec<u8> {
    let links = netns.link_snapshot();
    let mut out = Vec::with_capacity(links.len() * 80 + align4(NLMSG_HDR_LEN + 4));
    for link in &links {
        append_addr_messages(&mut out, 0, 0, NLM_F_MULTI, link, AF_UNSPEC);
    }
    let mut extra_messages = Vec::new();
    append_extra_addr_messages(
        &mut extra_messages,
        0,
        0,
        NLM_F_MULTI,
        netns,
        &links,
        AF_UNSPEC,
    );
    for message in extra_messages {
        out.extend_from_slice(&message);
    }
    append_done_message(&mut out, 0, 0);
    out
}

fn render_getroute_dump(netns: &NetNamespacePayload, header: NlMsgHeader) -> Vec<Vec<u8>> {
    let links = netns.link_snapshot();
    let mut out = Vec::new();
    for route in netns.route_snapshot() {
        out.push(build_route_message(
            header.seq,
            header.pid,
            NLM_F_MULTI,
            route,
            &links,
        ));
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

fn handle_newlink<F>(
    netns: &NetNamespacePayload,
    cred: Cred,
    header: NlMsgHeader,
    payload: &[u8],
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut impl FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
) -> Result<(), Errno>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let info = parse_ifinfomsg(payload)?;
    let link_info = parse_linkinfo(&info.attrs)?;
    if header.flags & NLM_F_CREATE == 0 && link_info.kind.is_none() {
        return apply_setlink_info(netns, cred, info, resolve_netns_fd, resolve_netns_pid);
    }

    let auth = require_net_admin(cred)?;
    let name = attr_string(&info.attrs, IFLA_IFNAME).ok_or(Errno::EINVAL)?;
    validate_ifname(name)?;

    match link_info.kind {
        Some("bridge") => create_bridge_link(netns, auth, name),
        Some("dummy") => create_dummy_link(netns, auth, name),
        Some("vlan") => create_vlan_link(netns, auth, name),
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

fn handle_dellink(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let info = parse_ifinfomsg(payload)?;
    let target = link_target_from_info_or_attrs(netns, &info, &info.attrs)?;
    netns.delete_device_by_ifindex(auth, target.ifindex)
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
    let info = parse_ifinfomsg(payload)?;
    apply_setlink_info(netns, cred, info, resolve_netns_fd, resolve_netns_pid)
}

fn apply_setlink_info<F>(
    netns: &NetNamespacePayload,
    cred: Cred,
    info: IfInfoMsg<'_>,
    resolve_netns_fd: &mut F,
    resolve_netns_pid: &mut impl FnMut(u32) -> Option<PayloadCap<NetNamespacePayload>>,
) -> Result<(), Errno>
where
    F: FnMut(i32) -> Option<PayloadCap<NetNamespacePayload>>,
{
    let auth = require_net_admin(cred)?;

    let target = link_target_from_info_or_attrs(netns, &info, &info.attrs)?;

    if info.change & IFF_UP != 0 {
        netns.set_device_up_by_ifindex(auth, target.ifindex, info.flags & IFF_UP != 0)?;
    }

    if let Some(mtu_attr) = attr_by_kind(&info.attrs, IFLA_MTU) {
        if mtu_attr.payload.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let mtu = read_u32(mtu_attr.payload, 0);
        let mtu = u16::try_from(mtu).map_err(|_| Errno::EINVAL)?;
        netns.set_device_mtu_by_ifindex(auth, target.ifindex, mtu)?;
    }

    if let Some(master_attr) = attr_by_kind(&info.attrs, IFLA_MASTER) {
        if master_attr.payload.len() < 4 {
            return Err(Errno::EINVAL);
        }
        let master_ifindex = read_u32(master_attr.payload, 0);
        if master_ifindex == 0 {
            netns.detach_device_from_bridges_by_ifindex(auth, target.ifindex)?;
        } else {
            let port = netns
                .find_device_by_ifindex(target.ifindex)
                .ok_or(Errno::ENODEV)?;
            let master = netns
                .find_device_by_ifindex(master_ifindex)
                .ok_or(Errno::ENODEV)?;
            master.ops.bridge_add_port(auth, port)?;
            netns.invalidate_link_snapshot_cache();
        }
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
    let attrs = parse_attrs(&payload[IFADDR_MSG_LEN..])?;
    let addr_attr = attr_by_kind(&attrs, IFA_LOCAL)
        .or_else(|| attr_by_kind(&attrs, IFA_ADDRESS))
        .ok_or(Errno::EINVAL)?;
    let ifindex = resolve_ifaddr_ifindex(netns, &info, &attrs)?;
    match info.family {
        AF_INET => {
            let addr = ipv4_addr_from_payload(addr_attr.payload)?;
            // `ip addr add` is additive: the first address becomes the
            // primary, further ones become secondaries (net_stress.interface
            // if-addr-adddel/-addlarge add test addresses next to the primary
            // and the primary must keep carrying traffic).
            let label = attr_string(&attrs, IFA_LABEL);
            netns.add_device_ipv4_addr_by_ifindex(
                auth,
                ifindex,
                addr,
                info.prefix_len,
                label.as_deref(),
            )
        }
        AF_INET6 => {
            let addr = ipv6_addr_from_payload(addr_attr.payload)?;
            netns.add_device_ipv6_addr_by_ifindex(auth, ifindex, addr, info.prefix_len)
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn handle_deladdr(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let info = parse_ifaddrmsg(payload)?;
    let attrs = parse_attrs(&payload[IFADDR_MSG_LEN..])?;
    let addr_attr = attr_by_kind(&attrs, IFA_LOCAL).or_else(|| attr_by_kind(&attrs, IFA_ADDRESS));
    let ifindex = resolve_ifaddr_ifindex(netns, &info, &attrs)?;
    let current = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == ifindex)
        .ok_or(Errno::ENODEV)?;
    match info.family {
        AF_INET => {
            if let Some(addr_attr) = addr_attr {
                let requested = ipv4_addr_from_payload(addr_attr.payload)?;
                // Removes a matching secondary, or clears a matching primary;
                // an unknown address stays a tolerated no-op (matching the
                // pre-secondary behaviour relied on by setup scripts).
                let _ = netns.del_device_ipv4_addr_by_ifindex(auth, ifindex, requested)?;
                return Ok(());
            }
            if current.ipv4_addr.is_none() {
                return Ok(());
            }
            netns.set_device_ipv4_addr_by_ifindex(auth, ifindex, None, None)
        }
        AF_INET6 => {
            if let Some(addr_attr) = addr_attr {
                let requested = ipv6_addr_from_payload(addr_attr.payload)?;
                let _ = netns.del_device_ipv6_addr_by_ifindex(auth, ifindex, requested)?;
                return Ok(());
            }
            if current.ipv6_addr.is_none() {
                return Ok(());
            }
            netns.set_device_ipv6_addr_by_ifindex(auth, ifindex, None, None)
        }
        AF_UNSPEC => {
            if addr_attr.is_none() && current.ipv4_addr.is_none() && current.ipv6_addr.is_none() {
                return Ok(());
            }
            netns.set_device_ipv4_addr_by_ifindex(auth, ifindex, None, None)?;
            netns.set_device_ipv6_addr_by_ifindex(auth, ifindex, None, None)
        }
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

fn resolve_ifaddr_ifindex(
    netns: &NetNamespacePayload,
    info: &IfAddrMsg,
    attrs: &[NlAttr<'_>],
) -> Result<u32, Errno> {
    if info.index != 0 {
        return Ok(info.index);
    }
    let label = attr_string(attrs, IFA_LABEL).ok_or(Errno::ENODEV)?;
    netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == label)
        .map(|link| link.ifindex)
        .ok_or(Errno::ENODEV)
}

fn ipv4_addr_from_payload(payload: &[u8]) -> Result<Ipv4Address, Errno> {
    if payload.len() < 4 {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4Address::new([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

fn ipv6_addr_from_payload(payload: &[u8]) -> Result<Ipv6Address, Errno> {
    if payload.len() < 16 {
        return Err(Errno::EINVAL);
    }
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&payload[..16]);
    Ok(Ipv6Address::new(octets))
}

fn handle_newroute(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let route = parse_route_config(netns, payload)?;
    netns.add_ipv4_route(auth, route)
}

fn handle_delroute(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let selector = parse_route_selector(netns, payload)?;
    netns.delete_ipv4_route(auth, selector)
}

fn handle_newneigh(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let msg = parse_ndmsg(payload)?;
    if msg.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    if msg.ifindex == 0 {
        return Err(Errno::EINVAL);
    }
    let ip = ipv4_attr(&msg.attrs, NDA_DST)?.ok_or(Errno::EINVAL)?;
    let mac_attr = attr_by_kind(&msg.attrs, NDA_LLADDR).ok_or(Errno::EINVAL)?;
    if mac_attr.payload.len() < 6 {
        return Err(Errno::EINVAL);
    }
    let mac = EthernetAddress::new([
        mac_attr.payload[0],
        mac_attr.payload[1],
        mac_attr.payload[2],
        mac_attr.payload[3],
        mac_attr.payload[4],
        mac_attr.payload[5],
    ]);
    netns.install_static_neighbor_by_ifindex(auth, msg.ifindex, ip, mac)
}

fn handle_delneigh(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let auth = require_net_admin(cred)?;
    let msg = parse_ndmsg(payload)?;
    if msg.family != AF_INET && msg.family != AF_UNSPEC {
        return Err(Errno::EAFNOSUPPORT);
    }
    if msg.ifindex == 0 {
        return Err(Errno::EINVAL);
    }
    let ip = ipv4_attr(&msg.attrs, NDA_DST)?.ok_or(Errno::EINVAL)?;
    netns.delete_static_neighbor_by_ifindex(auth, msg.ifindex, ip)
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

fn create_dummy_link(
    netns: &NetNamespacePayload,
    auth: NetAdminAuthority,
    name: &str,
) -> Result<(), Errno> {
    if netns.find_device_by_name(name).is_some() {
        return Err(Errno::EEXIST);
    }
    let leaked_name = leak_ifname(name);
    let instance = create_dummy_for_test_or_bootstrap(DummyConfig {
        name: leaked_name,
        devt: allocate_dynamic_devt(),
        mac: allocate_dynamic_mac(0x73),
        mtu: DUMMY_DEFAULT_MTU,
    });
    netns.attach_device(auth, instance.registration, None)
}

pub fn create_vlan_link(
    netns: &NetNamespacePayload,
    auth: NetAdminAuthority,
    name: &str,
) -> Result<(), Errno> {
    if netns.find_device_by_name(name).is_some() {
        return Err(Errno::EEXIST);
    }
    // Metadata-only VLAN link: it can be created, brought up/down, listed, and
    // deleted, but carries no tagging data path (see device/vlan.rs). The
    // parent link (IFLA_LINK) and VLAN id (IFLA_VLAN_ID) are accepted but not
    // yet modelled, so do not claim a data path.
    let leaked_name = leak_ifname(name);
    let instance = create_vlan_for_test_or_bootstrap(VlanConfig {
        name: leaked_name,
        devt: allocate_dynamic_devt(),
        mac: allocate_dynamic_mac(0x74),
        mtu: VLAN_DEFAULT_MTU,
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
    let mut out = Vec::new();
    append_link_message(&mut out, seq, pid, kind, flags, link, all_links);
    out
}

fn append_link_message(
    out: &mut Vec<u8>,
    seq: u32,
    pid: u32,
    kind: u16,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    all_links: &[NetNamespaceLinkInfo],
) {
    let mut payload = Vec::with_capacity(IFINFO_MSG_LEN + 72 + link.name.len());
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

    append_nlmsg(out, kind, flags, seq, pid, &payload);
}

fn append_addr_messages_as_vec(
    out: &mut Vec<Vec<u8>>,
    seq: u32,
    pid: u32,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    family: u8,
) {
    if family == AF_UNSPEC || family == AF_INET {
        if let Some(addr) = link.ipv4_addr {
            let mut message = Vec::new();
            append_addr_message_with_ipv4_addr(&mut message, seq, pid, flags, link, addr);
            out.push(message);
        }
    }
    if family == AF_UNSPEC || family == AF_INET6 {
        if let Some(addr) = link.ipv6_addr {
            let mut message = Vec::new();
            append_addr_message_with_ipv6_addr(&mut message, seq, pid, flags, link, addr);
            out.push(message);
        }
    }
}

fn append_addr_messages(
    out: &mut Vec<u8>,
    seq: u32,
    pid: u32,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    family: u8,
) {
    if family == AF_UNSPEC || family == AF_INET {
        if let Some(addr) = link.ipv4_addr {
            append_addr_message_with_ipv4_addr(out, seq, pid, flags, link, addr);
        }
    }
    if family == AF_UNSPEC || family == AF_INET6 {
        if let Some(addr) = link.ipv6_addr {
            append_addr_message_with_ipv6_addr(out, seq, pid, flags, link, addr);
        }
    }
}

fn append_addr_message_with_ipv4_addr(
    out: &mut Vec<u8>,
    seq: u32,
    pid: u32,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    addr: Ipv4Address,
) {
    let mut payload = vec![AF_INET, link.ipv4_prefix_len.unwrap_or(32), 0, 0];
    payload.extend_from_slice(&link.ifindex.to_le_bytes());
    push_attr(&mut payload, IFA_ADDRESS, &addr.octets());
    push_attr(&mut payload, IFA_LOCAL, &addr.octets());
    push_attr_string(&mut payload, IFA_LABEL, link.name);
    append_nlmsg(out, RTM_NEWADDR, flags, seq, pid, &payload);
}

const IFA_F_SECONDARY: u8 = 0x01;

/// Emit the RTM_NEWADDR records for every secondary (extra) address in the
/// namespace, after the per-link primaries. `family` filters like the dump
/// request; labeled IPv4 extras carry their alias label, unlabeled ones the
/// owning link name.
fn append_extra_addr_messages(
    out: &mut Vec<Vec<u8>>,
    seq: u32,
    pid: u32,
    flags: u16,
    netns: &NetNamespacePayload,
    links: &[NetNamespaceLinkInfo],
    family: u8,
) {
    let link_name = |ifindex: u32| {
        links
            .iter()
            .find(|link| link.ifindex == ifindex)
            .map(|link| link.name)
            .unwrap_or("")
    };
    if family == AF_UNSPEC || family == AF_INET {
        for extra in netns.ipv4_extra_snapshot() {
            let mut payload = vec![AF_INET, extra.prefix_len, IFA_F_SECONDARY, 0];
            payload.extend_from_slice(&extra.ifindex.to_le_bytes());
            push_attr(&mut payload, IFA_ADDRESS, &extra.addr.octets());
            push_attr(&mut payload, IFA_LOCAL, &extra.addr.octets());
            let label = extra
                .label
                .as_deref()
                .unwrap_or_else(|| link_name(extra.ifindex));
            push_attr_string(&mut payload, IFA_LABEL, label);
            let mut message = Vec::new();
            append_nlmsg(&mut message, RTM_NEWADDR, flags, seq, pid, &payload);
            out.push(message);
        }
    }
    if family == AF_UNSPEC || family == AF_INET6 {
        for extra in netns.ipv6_extra_snapshot() {
            let mut payload = vec![AF_INET6, extra.prefix_len, IFA_F_SECONDARY, 0];
            payload.extend_from_slice(&extra.ifindex.to_le_bytes());
            push_attr(&mut payload, IFA_ADDRESS, &extra.addr.octets());
            push_attr(&mut payload, IFA_LOCAL, &extra.addr.octets());
            push_attr_string(&mut payload, IFA_LABEL, link_name(extra.ifindex));
            let mut message = Vec::new();
            append_nlmsg(&mut message, RTM_NEWADDR, flags, seq, pid, &payload);
            out.push(message);
        }
    }
}

fn append_addr_message_with_ipv6_addr(
    out: &mut Vec<u8>,
    seq: u32,
    pid: u32,
    flags: u16,
    link: &NetNamespaceLinkInfo,
    addr: Ipv6Address,
) {
    let mut payload = vec![AF_INET6, link.ipv6_prefix_len.unwrap_or(128), 0, 0];
    payload.extend_from_slice(&link.ifindex.to_le_bytes());
    push_attr(&mut payload, IFA_ADDRESS, &addr.octets());
    push_attr(&mut payload, IFA_LOCAL, &addr.octets());
    push_attr_string(&mut payload, IFA_LABEL, link.name);
    append_nlmsg(out, RTM_NEWADDR, flags, seq, pid, &payload);
}

fn build_route_message(
    seq: u32,
    pid: u32,
    flags: u16,
    route: NetNamespaceRouteInfo,
    links: &[NetNamespaceLinkInfo],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(RTMSG_LEN + 32);
    payload.push(AF_INET);
    payload.push(route.prefix_len);
    payload.push(0);
    payload.push(0);
    payload.push(route.table);
    payload.push(route.protocol);
    payload.push(route.scope);
    payload.push(route.route_type);
    payload.extend_from_slice(&0u32.to_le_bytes());
    if route.prefix_len != 0 {
        push_attr(&mut payload, RTA_DST, &route.dst.octets());
    }
    if let Some(gateway) = route.gateway {
        push_attr(&mut payload, RTA_GATEWAY, &gateway.octets());
    }
    if let Some(oif_name) = route.oif_name {
        if let Some(link) = links.iter().find(|link| link.name == oif_name) {
            push_attr_u32(&mut payload, RTA_OIF, link.ifindex);
        }
    }
    if let Some(preferred_src) = route.preferred_src {
        push_attr(&mut payload, RTA_PREFSRC, &preferred_src.octets());
    }
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
    payload.push(RTN_UNICAST);
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
        NetDeviceKind::Dummy => "dummy",
        NetDeviceKind::Veth => "veth",
        NetDeviceKind::Bridge => "bridge",
        NetDeviceKind::Vlan => "vlan",
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
        // IFLA_INFO_DATA layout is link-kind-specific; only veth's nested peer
        // is consumed here. Tolerate kinds whose INFO_DATA we don't model
        // (e.g. vlan's IFLA_VLAN_ID/PROTOCOL/FLAGS) rather than rejecting the
        // whole RTM_NEWLINK with EINVAL.
        if let Ok(data_attrs) = parse_attrs(info_data.payload) {
            if let Some(peer) = attr_by_kind(&data_attrs, VETH_INFO_PEER) {
                peer_name = parse_veth_peer_name(peer.payload)?;
            }
        }
    }

    Ok(LinkInfoAttrs { kind, peer_name })
}

fn parse_veth_peer_name(payload: &[u8]) -> Result<Option<&str>, Errno> {
    if payload.len() >= IFINFO_MSG_LEN {
        if let Ok(peer_info) = parse_ifinfomsg(payload) {
            if let Some(name) = attr_string(&peer_info.attrs, IFLA_IFNAME) {
                return Ok(Some(name));
            }
        }
    }

    let attrs = parse_attrs(payload)?;
    Ok(attr_string(&attrs, IFLA_IFNAME))
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

fn parse_rtmsg(payload: &[u8]) -> Result<RtMsg<'_>, Errno> {
    if payload.len() < RTMSG_LEN {
        return Err(Errno::EINVAL);
    }
    Ok(RtMsg {
        family: payload[0],
        dst_len: payload[1],
        table: payload[4],
        protocol: payload[5],
        scope: payload[6],
        route_type: payload[7],
        attrs: parse_attrs(&payload[RTMSG_LEN..])?,
    })
}

fn parse_ndmsg(payload: &[u8]) -> Result<NdMsg<'_>, Errno> {
    if payload.len() < NDMSG_LEN {
        return Err(Errno::EINVAL);
    }
    Ok(NdMsg {
        family: payload[0],
        ifindex: parse_ifindex(payload, 4)?,
        attrs: parse_attrs(&payload[NDMSG_LEN..])?,
    })
}

fn parse_route_config(
    netns: &NetNamespacePayload,
    payload: &[u8],
) -> Result<NetNamespaceRouteConfig, Errno> {
    let msg = parse_rtmsg(payload)?;
    if msg.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    if msg.dst_len > 32 {
        return Err(Errno::EINVAL);
    }
    let dst = ipv4_attr(&msg.attrs, RTA_DST)?.unwrap_or(Ipv4Address::UNSPECIFIED);
    let gateway = ipv4_attr(&msg.attrs, RTA_GATEWAY)?;
    let preferred_src = ipv4_attr(&msg.attrs, RTA_PREFSRC)?;
    let oif_name =
        route_oif_name(netns, &msg.attrs)?.or_else(|| infer_route_oif_name(netns, gateway));
    Ok(NetNamespaceRouteConfig {
        dst,
        prefix_len: msg.dst_len,
        gateway,
        oif_name,
        preferred_src,
        table: normalize_route_table(msg.table),
        protocol: normalize_route_protocol(msg.protocol),
        scope: normalize_route_scope(msg.scope, gateway),
        route_type: normalize_route_type(msg.route_type),
    })
}

fn parse_route_selector(
    netns: &NetNamespacePayload,
    payload: &[u8],
) -> Result<NetNamespaceRouteSelector, Errno> {
    let msg = parse_rtmsg(payload)?;
    if msg.family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    if msg.dst_len > 32 {
        return Err(Errno::EINVAL);
    }
    Ok(NetNamespaceRouteSelector {
        dst: ipv4_attr(&msg.attrs, RTA_DST)?.unwrap_or(Ipv4Address::UNSPECIFIED),
        prefix_len: msg.dst_len,
        gateway: ipv4_attr(&msg.attrs, RTA_GATEWAY)?,
        oif_name: route_oif_name(netns, &msg.attrs)?,
        table: normalize_route_table(msg.table),
    })
}

fn ipv4_attr(attrs: &[NlAttr<'_>], kind: u16) -> Result<Option<Ipv4Address>, Errno> {
    let Some(attr) = attr_by_kind(attrs, kind) else {
        return Ok(None);
    };
    if attr.payload.len() < 4 {
        return Err(Errno::EINVAL);
    }
    Ok(Some(Ipv4Address::new([
        attr.payload[0],
        attr.payload[1],
        attr.payload[2],
        attr.payload[3],
    ])))
}

fn route_oif_name(
    netns: &NetNamespacePayload,
    attrs: &[NlAttr<'_>],
) -> Result<Option<&'static str>, Errno> {
    let Some(oif_attr) = attr_by_kind(attrs, RTA_OIF) else {
        return Ok(None);
    };
    if oif_attr.payload.len() < 4 {
        return Err(Errno::EINVAL);
    }
    let ifindex = read_u32(oif_attr.payload, 0);
    if ifindex == 0 {
        return Ok(None);
    }
    let name = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == ifindex)
        .map(|link| link.name)
        .ok_or(Errno::ENODEV)?;
    Ok(Some(name))
}

fn infer_route_oif_name(
    netns: &NetNamespacePayload,
    gateway: Option<Ipv4Address>,
) -> Option<&'static str> {
    let gateway = gateway?;
    netns.link_snapshot().into_iter().find_map(|link| {
        let addr = link.ipv4_addr?;
        let prefix_len = link.ipv4_prefix_len.unwrap_or(32);
        ipv4_in_prefix(gateway, addr, prefix_len).then_some(link.name)
    })
}

fn ipv4_in_prefix(addr: Ipv4Address, base: Ipv4Address, prefix_len: u8) -> bool {
    let prefix_len = prefix_len.min(32);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    (ipv4_to_u32(addr) & mask) == (ipv4_to_u32(base) & mask)
}

fn ipv4_to_u32(addr: Ipv4Address) -> u32 {
    u32::from_be_bytes(addr.octets())
}

fn normalize_route_table(table: u8) -> u8 {
    if table == 0 {
        RT_TABLE_MAIN
    } else {
        table
    }
}

fn normalize_route_protocol(protocol: u8) -> u8 {
    if protocol == 0 {
        RTPROT_STATIC
    } else {
        protocol
    }
}

fn normalize_route_scope(scope: u8, gateway: Option<Ipv4Address>) -> u8 {
    if scope != 0 {
        scope
    } else if gateway.is_some() {
        RT_SCOPE_UNIVERSE
    } else {
        RT_SCOPE_LINK
    }
}

fn normalize_route_type(route_type: u8) -> u8 {
    if route_type == 0 {
        RTN_UNICAST
    } else {
        route_type
    }
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
    append_nlmsg(&mut out, kind, flags, seq, pid, payload);
    out
}

fn append_nlmsg(out: &mut Vec<u8>, kind: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) {
    let msg_len = NLMSG_HDR_LEN + payload.len();
    out.extend_from_slice(&(msg_len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&pid.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(out);
}

fn build_done_message(seq: u32, pid: u32) -> Vec<u8> {
    build_nlmsg(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes())
}

fn append_done_message(out: &mut Vec<u8>, seq: u32, pid: u32) {
    append_nlmsg(out, NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes());
}

fn push_ack_or_error(responses: &mut Vec<Vec<u8>>, header: NlMsgHeader, result: Result<(), Errno>) {
    match result {
        Ok(()) if header.flags & NLM_F_ACK != 0 => responses.push(build_ack_response(header)),
        Ok(()) => {}
        Err(errno) => responses.push(build_error_response(Some(header), errno)),
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
    let len = NLA_HDR_LEN + value.len() + 1;
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    pad_to_align4(out);
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

struct RtMsg<'a> {
    family: u8,
    dst_len: u8,
    table: u8,
    protocol: u8,
    scope: u8,
    route_type: u8,
    attrs: Vec<NlAttr<'a>>,
}

struct NdMsg<'a> {
    family: u8,
    ifindex: u32,
    attrs: Vec<NlAttr<'a>>,
}

struct LinkInfoAttrs<'a> {
    kind: Option<&'a str>,
    peer_name: Option<&'a str>,
}
