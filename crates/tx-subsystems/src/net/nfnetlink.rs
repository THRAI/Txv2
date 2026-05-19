//! Minimal NETLINK_NETFILTER / nfnetlink surface.
//!
//! This is a read-only adapter over the staging netfilter rule model. It gives
//! nftables-aware userspace a real protocol endpoint before full nf_tables
//! expression parsing and mutation support exists.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tx_substrate::zone::Cap;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::net::netfilter::{
    netfilter_rules_snapshot, NetfilterConntrackProtocol, NetfilterHook, NetfilterIpv4Cidr,
    NetfilterRule, NetfilterTable, NetfilterTarget,
};
use crate::net::structure::{Ipv4Address, RecvWireSet, SendRecvFlags, SocketIdentity, SocketKind};
use crate::sync::SpinMutex;

pub const NETLINK_NETFILTER: i32 = 12;

pub const NLM_F_REQUEST: u16 = 0x0001;
pub const NLM_F_MULTI: u16 = 0x0002;
pub const NLM_F_ACK: u16 = 0x0004;
pub const NLM_F_ROOT: u16 = 0x0100;
pub const NLM_F_MATCH: u16 = 0x0200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLMSG_MIN_TYPE: u16 = 0x10;

pub const NFNETLINK_V0: u8 = 0;
pub const NFPROTO_IPV4: u8 = 2;
pub const NFNL_SUBSYS_NFTABLES: u16 = 10;
pub const NFNL_MSG_BATCH_BEGIN: u16 = NLMSG_MIN_TYPE;
pub const NFNL_MSG_BATCH_END: u16 = NLMSG_MIN_TYPE + 1;

pub const NFT_MSG_NEWTABLE: u16 = 0;
pub const NFT_MSG_GETTABLE: u16 = 1;
pub const NFT_MSG_NEWCHAIN: u16 = 3;
pub const NFT_MSG_GETCHAIN: u16 = 4;
pub const NFT_MSG_NEWRULE: u16 = 6;
pub const NFT_MSG_GETRULE: u16 = 7;
pub const NFT_MSG_NEWGEN: u16 = 15;
pub const NFT_MSG_GETGEN: u16 = 16;

const NLMSG_HDR_LEN: usize = 16;
const NFGENMSG_LEN: usize = 4;
const NLA_HDR_LEN: usize = 4;
const NLA_F_NESTED: u16 = 0x8000;

const NFTA_TABLE_NAME: u16 = 1;
const NFTA_TABLE_FLAGS: u16 = 2;
const NFTA_TABLE_USE: u16 = 3;

const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_HANDLE: u16 = 2;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_POLICY: u16 = 5;
const NFTA_CHAIN_TYPE: u16 = 7;
const NFTA_CHAIN_FLAGS: u16 = 10;

const NFTA_HOOK_HOOKNUM: u16 = 1;
const NFTA_HOOK_PRIORITY: u16 = 2;

const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;
const NFTA_RULE_USERDATA: u16 = 7;

const NFTA_GEN_ID: u16 = 1;

const NFT_CHAIN_BASE: u32 = 1;
const NF_ACCEPT: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetlinkNetfilterState;

pub struct RawNetlinkNetfilterSocket {
    rx: SpinMutex<VecDeque<Vec<u8>>>,
}

impl RawNetlinkNetfilterSocket {
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

pub fn netlink_netfilter_send(
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
    _cred: Cred,
) -> Result<usize, Errno> {
    if socket.kind != SocketKind::NetlinkNetfilter {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_netfilter_socket()
        .ok_or(Errno::EOPNOTSUPP)?;

    for response in nfnetlink_handle_request(bytes) {
        raw.queue_response(response);
    }
    payload.refresh_io_from_raw();
    if !raw.is_empty() {
        socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
    Ok(bytes.len())
}

pub fn netlink_netfilter_recv(
    socket: &Cap<SocketIdentity>,
    out: &mut [u8],
    flags: SendRecvFlags,
) -> Result<usize, Errno> {
    if socket.kind != SocketKind::NetlinkNetfilter {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_netfilter_socket()
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

pub fn nfnetlink_handle_request(request: &[u8]) -> Vec<Vec<u8>> {
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
        handle_one_message(header, payload, &mut responses);
        offset += align4(msg_len);
    }
    responses
}

fn handle_one_message(header: NlMsgHeader, payload: &[u8], responses: &mut Vec<Vec<u8>>) {
    if header.kind == NFNL_MSG_BATCH_BEGIN || header.kind == NFNL_MSG_BATCH_END {
        responses.push(build_ack_response(header));
        return;
    }

    if nfnl_subsys(header.kind) != NFNL_SUBSYS_NFTABLES {
        responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP));
        return;
    }

    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(NFPROTO_IPV4);
    match nfnl_msg_type(header.kind) {
        NFT_MSG_GETTABLE => {
            for msg in render_table_dump(header, family) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETCHAIN => {
            for msg in render_chain_dump(header, family) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETRULE => {
            for msg in render_rule_dump(header, family) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETGEN => {
            responses.push(build_generation_message(header, family));
            responses.push(build_done_message(header.seq, header.pid));
        }
        _ => responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP)),
    }
}

fn render_table_dump(header: NlMsgHeader, family: u8) -> Vec<Vec<u8>> {
    let rules = netfilter_rules_snapshot();
    let mut tables = Vec::<TableSummary>::new();
    for rule in &rules {
        let summary = table_for_rule(*rule);
        if let Some(existing) = tables
            .iter_mut()
            .find(|table| table.name == summary.name && table.family == family)
        {
            existing.chains = existing.chains.max(summary.chains);
        } else {
            tables.push(TableSummary { family, ..summary });
        }
    }
    let mut out = Vec::new();
    for table in tables {
        out.push(build_table_message(header.seq, header.pid, family, table));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_chain_dump(header: NlMsgHeader, family: u8) -> Vec<Vec<u8>> {
    let mut chains = Vec::<ChainSummary>::new();
    for rule in netfilter_rules_snapshot() {
        let chain = chain_for_rule(rule, family);
        if chains.iter().all(|seen| {
            seen.table != chain.table || seen.name != chain.name || seen.family != chain.family
        }) {
            chains.push(chain);
        }
    }
    let mut out = Vec::new();
    for (idx, chain) in chains.into_iter().enumerate() {
        out.push(build_chain_message(
            header.seq,
            header.pid,
            family,
            chain,
            idx as u64 + 1,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_rule_dump(header: NlMsgHeader, family: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (idx, rule) in netfilter_rules_snapshot().into_iter().enumerate() {
        out.push(build_rule_message(
            header.seq,
            header.pid,
            family,
            rule,
            idx as u64 + 1,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn build_table_message(seq: u32, pid: u32, family: u8, table: TableSummary) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_TABLE_NAME, table.name);
    push_attr_u32(&mut payload, NFTA_TABLE_FLAGS, 0);
    push_attr_u32(&mut payload, NFTA_TABLE_USE, table.chains);
    build_nlmsg(nft_msg(NFT_MSG_NEWTABLE), NLM_F_MULTI, seq, pid, &payload)
}

fn build_chain_message(
    seq: u32,
    pid: u32,
    family: u8,
    chain: ChainSummary,
    handle: u64,
) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_CHAIN_TABLE, chain.table);
    push_attr_u64(&mut payload, NFTA_CHAIN_HANDLE, handle);
    push_attr_string(&mut payload, NFTA_CHAIN_NAME, chain.name);
    push_nested_attr(&mut payload, NFTA_CHAIN_HOOK, |nested| {
        push_attr_u32(nested, NFTA_HOOK_HOOKNUM, chain.hooknum);
        push_attr_u32(nested, NFTA_HOOK_PRIORITY, chain.priority as u32);
    });
    push_attr_u32(&mut payload, NFTA_CHAIN_POLICY, NF_ACCEPT);
    push_attr_string(&mut payload, NFTA_CHAIN_TYPE, chain.chain_type);
    push_attr_u32(&mut payload, NFTA_CHAIN_FLAGS, NFT_CHAIN_BASE);
    build_nlmsg(nft_msg(NFT_MSG_NEWCHAIN), NLM_F_MULTI, seq, pid, &payload)
}

fn build_rule_message(seq: u32, pid: u32, family: u8, rule: NetfilterRule, handle: u64) -> Vec<u8> {
    let chain = chain_for_rule(rule, family);
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_RULE_TABLE, chain.table);
    push_attr_string(&mut payload, NFTA_RULE_CHAIN, chain.name);
    push_attr_u64(&mut payload, NFTA_RULE_HANDLE, handle);
    push_nested_attr(&mut payload, NFTA_RULE_EXPRESSIONS, |_| {});
    let summary = rule_summary(rule);
    push_attr(&mut payload, NFTA_RULE_USERDATA, summary.as_bytes());
    build_nlmsg(nft_msg(NFT_MSG_NEWRULE), NLM_F_MULTI, seq, pid, &payload)
}

fn build_generation_message(header: NlMsgHeader, family: u8) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_u32(&mut payload, NFTA_GEN_ID, 1);
    build_nlmsg(
        nft_msg(NFT_MSG_NEWGEN),
        NLM_F_MULTI,
        header.seq,
        header.pid,
        &payload,
    )
}

fn table_for_rule(rule: NetfilterRule) -> TableSummary {
    TableSummary {
        family: NFPROTO_IPV4,
        name: match rule.table {
            NetfilterTable::Filter => "filter",
            NetfilterTable::Nat => "nat",
        },
        chains: 1,
    }
}

fn chain_for_rule(rule: NetfilterRule, family: u8) -> ChainSummary {
    let table = match rule.table {
        NetfilterTable::Filter => "filter",
        NetfilterTable::Nat => "nat",
    };
    ChainSummary {
        family,
        table,
        name: hook_name(rule.hook),
        hooknum: hooknum(rule.hook),
        priority: match rule.table {
            NetfilterTable::Filter => 0,
            NetfilterTable::Nat => match rule.hook {
                NetfilterHook::Prerouting => -100,
                NetfilterHook::Postrouting => 100,
                _ => 0,
            },
        },
        chain_type: match rule.table {
            NetfilterTable::Filter => "filter",
            NetfilterTable::Nat => "nat",
        },
    }
}

fn rule_summary(rule: NetfilterRule) -> String {
    format!(
        "tx:{}:{}:{} proto={} src={} dst={} dport={} in={} out={} to={}",
        table_name(rule.table),
        hook_name(rule.hook),
        target_name(rule.target),
        rule.protocol.map(protocol_name).unwrap_or("*"),
        rule.src
            .map(format_cidr)
            .unwrap_or_else(|| String::from("*")),
        rule.dst
            .map(format_cidr)
            .unwrap_or_else(|| String::from("*")),
        rule.dst_port
            .map(|port| format!("{port}"))
            .unwrap_or_else(|| String::from("*")),
        rule.in_iface.unwrap_or("*"),
        rule.out_iface.unwrap_or("*"),
        match (rule.to_addr, rule.to_port) {
            (Some(addr), Some(port)) => format!("{}:{port}", format_ipv4(addr)),
            (Some(addr), None) => format_ipv4(addr),
            _ => String::from("*"),
        }
    )
}

fn table_name(table: NetfilterTable) -> &'static str {
    match table {
        NetfilterTable::Filter => "filter",
        NetfilterTable::Nat => "nat",
    }
}

fn hook_name(hook: NetfilterHook) -> &'static str {
    match hook {
        NetfilterHook::Prerouting => "prerouting",
        NetfilterHook::Input => "input",
        NetfilterHook::Forward => "forward",
        NetfilterHook::Output => "output",
        NetfilterHook::Postrouting => "postrouting",
    }
}

fn hooknum(hook: NetfilterHook) -> u32 {
    match hook {
        NetfilterHook::Prerouting => 0,
        NetfilterHook::Input => 1,
        NetfilterHook::Forward => 2,
        NetfilterHook::Output => 3,
        NetfilterHook::Postrouting => 4,
    }
}

fn target_name(target: NetfilterTarget) -> &'static str {
    match target {
        NetfilterTarget::Accept => "accept",
        NetfilterTarget::Drop => "drop",
        NetfilterTarget::Masquerade => "masquerade",
        NetfilterTarget::Dnat => "dnat",
    }
}

fn format_cidr(cidr: NetfilterIpv4Cidr) -> String {
    format!("{}/{}", format_ipv4(cidr.addr), cidr.prefix_len)
}

fn format_ipv4(addr: Ipv4Address) -> String {
    let [a, b, c, d] = addr.octets();
    format!("{a}.{b}.{c}.{d}")
}

fn protocol_name(protocol: NetfilterConntrackProtocol) -> &'static str {
    match protocol {
        NetfilterConntrackProtocol::Icmp => "icmp",
        NetfilterConntrackProtocol::Tcp => "tcp",
        NetfilterConntrackProtocol::Udp => "udp",
    }
}

fn nfgenmsg(family: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(NFGENMSG_LEN);
    out.push(family);
    out.push(NFNETLINK_V0);
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn nft_msg(op: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | op
}

fn nfnl_subsys(kind: u16) -> u16 {
    (kind & 0xff00) >> 8
}

fn nfnl_msg_type(kind: u16) -> u16 {
    kind & 0x00ff
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
    let mut payload = Vec::from(value.as_bytes());
    payload.push(0);
    push_attr(out, kind, &payload);
}

fn push_attr_u32(out: &mut Vec<u8>, kind: u16, value: u32) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn push_attr_u64(out: &mut Vec<u8>, kind: u16, value: u64) {
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

fn linux_errno_i32(errno: Errno) -> i32 {
    match errno {
        Errno::EINVAL => 22,
        Errno::ENOTCONN => 107,
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
struct TableSummary {
    family: u8,
    name: &'static str,
    chains: u32,
}

#[derive(Clone, Copy)]
struct ChainSummary {
    family: u8,
    table: &'static str,
    name: &'static str,
    hooknum: u32,
    priority: i32,
    chain_type: &'static str,
}
