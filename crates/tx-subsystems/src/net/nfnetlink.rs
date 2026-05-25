//! Minimal NETLINK_NETFILTER / nfnetlink surface.
//!
//! This is a small adapter over the staging netfilter rule model. It gives
//! nftables-aware userspace a real protocol endpoint, read-only dumps, and a
//! tiny mutation subset before full nf_tables expression compatibility exists.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use tx_substrate::zone::Cap;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::net::admin::require_net_admin;
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::netfilter::{
    add_netfilter_rule_in_namespace_for_test_or_bootstrap, netfilter_rules_snapshot_for_namespace,
    remove_netfilter_rule_in_namespace_for_test_or_bootstrap,
    remove_netfilter_rules_for_chain_in_namespace_for_test_or_bootstrap,
    remove_netfilter_rules_for_table_in_namespace_for_test_or_bootstrap,
    NetfilterConntrackProtocol, NetfilterHook, NetfilterIpv4Cidr, NetfilterRule, NetfilterTable,
    NetfilterTarget,
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
pub const NFNL_SUBSYS_NFT_COMPAT: u16 = 11;
pub const NFNL_MSG_BATCH_BEGIN: u16 = NLMSG_MIN_TYPE;
pub const NFNL_MSG_BATCH_END: u16 = NLMSG_MIN_TYPE + 1;

pub const NFT_MSG_NEWTABLE: u16 = 0;
pub const NFT_MSG_GETTABLE: u16 = 1;
pub const NFT_MSG_DELTABLE: u16 = 2;
pub const NFT_MSG_NEWCHAIN: u16 = 3;
pub const NFT_MSG_GETCHAIN: u16 = 4;
pub const NFT_MSG_DELCHAIN: u16 = 5;
pub const NFT_MSG_NEWRULE: u16 = 6;
pub const NFT_MSG_GETRULE: u16 = 7;
pub const NFT_MSG_DELRULE: u16 = 8;
pub const NFT_MSG_GETSET: u16 = 10;
pub const NFT_MSG_GETSETELEM: u16 = 13;
pub const NFT_MSG_NEWGEN: u16 = 15;
pub const NFT_MSG_GETGEN: u16 = 16;
pub const NFT_MSG_GETOBJ: u16 = 19;
pub const NFT_MSG_GETOBJ_RESET: u16 = 21;
pub const NFT_MSG_GETFLOWTABLE: u16 = 23;
pub const NFNL_MSG_COMPAT_GET: u16 = 0;

const NLMSG_HDR_LEN: usize = 16;
const NFGENMSG_LEN: usize = 4;
const NLA_HDR_LEN: usize = 4;
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLA_F_NESTED: u16 = 0x8000;

const NFTA_LIST_ELEM: u16 = 1;

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

const NFTA_DATA_VALUE: u16 = 1;
const NFTA_DATA_VERDICT: u16 = 2;
const NFTA_VERDICT_CODE: u16 = 1;

const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;

const NFTA_IMMEDIATE_DREG: u16 = 1;
const NFTA_IMMEDIATE_DATA: u16 = 2;

const NFTA_BITWISE_SREG: u16 = 1;
const NFTA_BITWISE_DREG: u16 = 2;
const NFTA_BITWISE_LEN: u16 = 3;
const NFTA_BITWISE_MASK: u16 = 4;
const NFTA_BITWISE_XOR: u16 = 5;

const NFTA_CMP_SREG: u16 = 1;
const NFTA_CMP_OP: u16 = 2;
const NFTA_CMP_DATA: u16 = 3;

const NFTA_PAYLOAD_DREG: u16 = 1;
const NFTA_PAYLOAD_BASE: u16 = 2;
const NFTA_PAYLOAD_OFFSET: u16 = 3;
const NFTA_PAYLOAD_LEN: u16 = 4;

const NFTA_META_DREG: u16 = 1;
const NFTA_META_KEY: u16 = 2;

const NFTA_NAT_TYPE: u16 = 1;
const NFTA_NAT_FAMILY: u16 = 2;
const NFTA_NAT_REG_ADDR_MIN: u16 = 3;
const NFTA_NAT_REG_PROTO_MIN: u16 = 5;

const NFTA_TARGET_NAME: u16 = 1;
const NFTA_TARGET_REV: u16 = 2;
const NFTA_TARGET_INFO: u16 = 3;

const NFT_REG_VERDICT: u32 = 0;
const NFT_CMP_EQ: u32 = 0;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;
const NFT_META_IIFNAME: u32 = 6;
const NFT_META_OIFNAME: u32 = 7;
const NFT_META_L4PROTO: u32 = 16;
const NFT_NAT_DNAT: u32 = 1;

const NFT_CHAIN_BASE: u32 = 1;
const NFT_REG32_00: u32 = 8;
const NF_DROP: u32 = 0;
const NF_ACCEPT: u32 = 1;
const IFNAMSIZ: usize = 16;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetlinkNetfilterState;

static NFT_NAMESPACES: SpinMutex<Vec<NftNamespaceState>> = SpinMutex::new(Vec::new());

#[cfg(any(test, feature = "test-support"))]
pub fn reset_nfnetlink_for_test() {
    NFT_NAMESPACES.lock().clear();
}

fn nft_namespace_key(netns: &NetNamespacePayload) -> usize {
    core::ptr::addr_of!(*netns) as usize
}

fn nft_state_snapshot(netns: &NetNamespacePayload) -> (Vec<NftTableObject>, Vec<NftChainObject>) {
    let key = nft_namespace_key(netns);
    NFT_NAMESPACES
        .lock()
        .iter()
        .find(|state| state.namespace_key == key)
        .map(|state| (state.tables.clone(), state.chains.clone()))
        .unwrap_or_default()
}

fn with_nft_state_mut<T>(
    netns: &NetNamespacePayload,
    mutate: impl FnOnce(&mut NftNamespaceState) -> T,
) -> T {
    let key = nft_namespace_key(netns);
    let mut namespaces = NFT_NAMESPACES.lock();
    let index = if let Some(index) = namespaces
        .iter()
        .position(|state| state.namespace_key == key)
    {
        index
    } else {
        namespaces.push(NftNamespaceState::new(key));
        namespaces.len() - 1
    };
    mutate(&mut namespaces[index])
}

pub struct RawNetlinkNetfilterSocket {
    rx: SpinMutex<VecDeque<Vec<u8>>>,
}

impl Default for RawNetlinkNetfilterSocket {
    fn default() -> Self {
        Self::new()
    }
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
    cred: Cred,
) -> Result<usize, Errno> {
    if socket.kind != SocketKind::NetlinkNetfilter {
        return Err(Errno::EOPNOTSUPP);
    }
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let raw = payload
        .raw_netlink_netfilter_socket()
        .ok_or(Errno::EOPNOTSUPP)?;

    let responses =
        nfnetlink_handle_request_in_namespace_with_cred(&payload.net_namespace(), bytes, cred);
    let mut packet = Vec::new();
    for response in responses {
        packet.extend_from_slice(&response);
    }
    if !packet.is_empty() {
        raw.queue_response(packet);
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
    let reported = if flags.contains(SendRecvFlags::MSG_TRUNC) {
        response.len()
    } else {
        copied
    };
    payload.refresh_io_from_raw();
    if !peek && raw.is_empty() {
        socket.readiness.clear_recv(RecvWireSet::HAS_DATA);
    }
    Ok(reported)
}

pub fn nfnetlink_handle_request(request: &[u8]) -> Vec<Vec<u8>> {
    nfnetlink_handle_request_with_cred(request, Cred::root())
}

pub fn nfnetlink_handle_request_with_cred(request: &[u8], cred: Cred) -> Vec<Vec<u8>> {
    let netns = initial_net_namespace_payload();
    nfnetlink_handle_request_in_namespace_with_cred(&netns, request, cred)
}

pub fn nfnetlink_handle_request_in_namespace(
    netns: &NetNamespacePayload,
    request: &[u8],
) -> Vec<Vec<u8>> {
    nfnetlink_handle_request_in_namespace_with_cred(netns, request, Cred::root())
}

pub fn nfnetlink_handle_request_in_namespace_with_cred(
    netns: &NetNamespacePayload,
    request: &[u8],
    cred: Cred,
) -> Vec<Vec<u8>> {
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
        handle_one_message(netns, cred, header, payload, &mut responses);
        offset += align4(msg_len);
    }
    responses
}

fn handle_one_message(
    netns: &NetNamespacePayload,
    cred: Cred,
    header: NlMsgHeader,
    payload: &[u8],
    responses: &mut Vec<Vec<u8>>,
) {
    if header.kind == NFNL_MSG_BATCH_BEGIN || header.kind == NFNL_MSG_BATCH_END {
        responses.push(build_ack_response(header));
        return;
    }

    if nfnl_subsys(header.kind) == NFNL_SUBSYS_NFT_COMPAT {
        if nfnl_msg_type(header.kind) == NFNL_MSG_COMPAT_GET {
            responses.push(build_ack_response(header));
        } else {
            responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP));
        }
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
            for msg in render_table_request(netns, header, family, payload) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETCHAIN => {
            for msg in render_chain_request(netns, header, family, payload) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETRULE => {
            for msg in render_rule_request(netns, header, family, payload) {
                responses.push(msg);
            }
        }
        NFT_MSG_GETSET | NFT_MSG_GETSETELEM | NFT_MSG_GETOBJ | NFT_MSG_GETOBJ_RESET
        | NFT_MSG_GETFLOWTABLE => {
            responses.push(build_done_message(header.seq, header.pid));
        }
        NFT_MSG_GETGEN => {
            responses.push(build_generation_message(header, family));
            responses.push(build_done_message(header.seq, header.pid));
        }
        NFT_MSG_NEWTABLE => responses.push(ack_or_error(
            header,
            handle_newtable(netns, cred, family, payload),
        )),
        NFT_MSG_DELTABLE => responses.push(ack_or_error(
            header,
            handle_deltable(netns, cred, family, payload),
        )),
        NFT_MSG_NEWCHAIN => responses.push(ack_or_error(
            header,
            handle_newchain(netns, cred, family, payload),
        )),
        NFT_MSG_DELCHAIN => responses.push(ack_or_error(
            header,
            handle_delchain(netns, cred, family, payload),
        )),
        NFT_MSG_NEWRULE => responses.push(ack_or_error(
            header,
            handle_newrule(netns, cred, family, payload),
        )),
        NFT_MSG_DELRULE => {
            responses.push(ack_or_error(header, handle_delrule(netns, cred, payload)))
        }
        _ => responses.push(build_error_response(Some(header), Errno::EOPNOTSUPP)),
    }
}

fn render_table_request(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    family: u8,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    if is_dump_request(header) {
        return render_table_dump(netns, header, family);
    }

    let Ok(attrs) = parse_nfmsg_attrs(payload) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let Some(name) = attr_string(&attrs, NFTA_TABLE_NAME) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let Some(table) = table_summary_by_name(netns, family, name) else {
        return alloc::vec![build_error_response(Some(header), Errno::ENOENT)];
    };

    alloc::vec![build_table_message(
        header.seq, header.pid, 0, family, table,
    )]
}

fn render_table_dump(netns: &NetNamespacePayload, header: NlMsgHeader, family: u8) -> Vec<Vec<u8>> {
    let tables = table_summaries(netns, family);
    let mut out = Vec::new();
    for table in tables {
        out.push(build_table_message(
            header.seq,
            header.pid,
            NLM_F_MULTI,
            family,
            table,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_chain_request(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    family: u8,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    if is_dump_request(header) {
        return render_chain_dump(netns, header, family);
    }

    let Ok(attrs) = parse_nfmsg_attrs(payload) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let Some(table) = attr_string(&attrs, NFTA_CHAIN_TABLE) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let Some(name) = attr_string(&attrs, NFTA_CHAIN_NAME) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let Some((idx, chain)) = chain_summaries(netns, family)
        .into_iter()
        .enumerate()
        .find(|(_, chain)| chain.table == table && chain.name == name)
    else {
        return alloc::vec![build_error_response(Some(header), Errno::ENOENT)];
    };

    alloc::vec![build_chain_message(
        header.seq,
        header.pid,
        0,
        family,
        chain,
        idx as u64 + 1,
    )]
}

fn render_chain_dump(netns: &NetNamespacePayload, header: NlMsgHeader, family: u8) -> Vec<Vec<u8>> {
    let chains = chain_summaries(netns, family);
    let mut out = Vec::new();
    for (idx, chain) in chains.into_iter().enumerate() {
        out.push(build_chain_message(
            header.seq,
            header.pid,
            NLM_F_MULTI,
            family,
            chain,
            idx as u64 + 1,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn render_rule_request(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    family: u8,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    let Ok(attrs) = parse_nfmsg_attrs(payload) else {
        return alloc::vec![build_error_response(Some(header), Errno::EINVAL)];
    };
    let table_filter = attr_string(&attrs, NFTA_RULE_TABLE);
    let chain_filter = attr_string(&attrs, NFTA_RULE_CHAIN);
    let handle = attr_u64(&attrs, NFTA_RULE_HANDLE);
    if is_dump_request(header) {
        return render_rule_dump(netns, header, family, table_filter, chain_filter, handle);
    }

    let Some((idx, rule)) = netfilter_rules_snapshot_for_namespace(netns)
        .into_iter()
        .enumerate()
        .find(|(idx, rule)| {
            rule_matches_filter(*rule, family, table_filter, chain_filter, handle, *idx)
        })
    else {
        return alloc::vec![build_error_response(Some(header), Errno::ENOENT)];
    };

    alloc::vec![build_rule_message(
        header.seq,
        header.pid,
        0,
        family,
        rule,
        idx as u64 + 1,
    )]
}

fn render_rule_dump(
    netns: &NetNamespacePayload,
    header: NlMsgHeader,
    family: u8,
    table_filter: Option<&str>,
    chain_filter: Option<&str>,
    handle: Option<u64>,
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (idx, rule) in netfilter_rules_snapshot_for_namespace(netns)
        .into_iter()
        .enumerate()
    {
        if !rule_matches_filter(rule, family, table_filter, chain_filter, handle, idx) {
            continue;
        }
        out.push(build_rule_message(
            header.seq,
            header.pid,
            NLM_F_MULTI,
            family,
            rule,
            idx as u64 + 1,
        ));
    }
    out.push(build_done_message(header.seq, header.pid));
    out
}

fn rule_matches_filter(
    rule: NetfilterRule,
    family: u8,
    table_filter: Option<&str>,
    chain_filter: Option<&str>,
    handle: Option<u64>,
    index: usize,
) -> bool {
    if let Some(handle) = handle {
        if handle != index as u64 + 1 {
            return false;
        }
    }
    let chain = chain_for_rule(rule, family);
    if let Some(table) = table_filter {
        if table != chain.table {
            return false;
        }
    }
    if let Some(name) = chain_filter {
        if name != chain.name {
            return false;
        }
    }
    true
}

fn is_dump_request(header: NlMsgHeader) -> bool {
    (header.flags & NLM_F_DUMP) != 0
}

fn table_summaries(netns: &NetNamespacePayload, family: u8) -> Vec<TableSummary> {
    let rules = netfilter_rules_snapshot_for_namespace(netns);
    let (nft_tables, _) = nft_state_snapshot(netns);
    let mut tables = Vec::<TableSummary>::new();
    push_compat_table_summaries(&mut tables, family);
    for table in nft_tables.iter() {
        if table.family == family
            && tables
                .iter()
                .all(|summary| summary.name != table.name || summary.family != family)
        {
            let chains = compat_chain_count(&table.name);
            tables.push(TableSummary {
                family,
                name: table.name.clone(),
                chains,
            });
        }
    }
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
    tables
}

fn push_compat_table_summaries(tables: &mut Vec<TableSummary>, family: u8) {
    if family != NFPROTO_IPV4 {
        return;
    }
    tables.push(TableSummary {
        family,
        name: String::from("filter"),
        chains: 3,
    });
    tables.push(TableSummary {
        family,
        name: String::from("nat"),
        chains: 4,
    });
}

fn compat_chain_count(table: &str) -> u32 {
    match table {
        "filter" | "FILTER" => 3,
        "nat" | "NAT" => 4,
        _ => 0,
    }
}

fn table_summary_by_name(
    netns: &NetNamespacePayload,
    family: u8,
    name: &str,
) -> Option<TableSummary> {
    table_summaries(netns, family)
        .into_iter()
        .find(|table| table.name == name)
}

fn chain_summaries(netns: &NetNamespacePayload, family: u8) -> Vec<ChainSummary> {
    let (_, nft_chains) = nft_state_snapshot(netns);
    let mut chains = Vec::<ChainSummary>::new();
    push_compat_chain_summaries(&mut chains, family);
    for chain in nft_chains.iter() {
        if chain.family == family
            && chains.iter().all(|summary| {
                summary.table != chain.table
                    || summary.name != chain.name
                    || summary.family != family
            })
        {
            chains.push(ChainSummary {
                family,
                table: chain.table.clone(),
                name: chain.name.clone(),
                hooknum: hooknum(chain.hook),
                priority: chain.priority,
                chain_type: chain.chain_type.clone(),
            });
        }
    }
    for rule in netfilter_rules_snapshot_for_namespace(netns) {
        let chain = chain_for_rule(rule, family);
        if chains.iter().all(|seen| {
            seen.table != chain.table || seen.name != chain.name || seen.family != chain.family
        }) {
            chains.push(chain);
        }
    }
    chains
}

fn push_compat_chain_summaries(chains: &mut Vec<ChainSummary>, family: u8) {
    if family != NFPROTO_IPV4 {
        return;
    }
    for (table, name, hook, priority, chain_type) in [
        ("filter", "INPUT", NetfilterHook::Input, 0, "filter"),
        ("filter", "FORWARD", NetfilterHook::Forward, 0, "filter"),
        ("filter", "OUTPUT", NetfilterHook::Output, 0, "filter"),
        ("nat", "PREROUTING", NetfilterHook::Prerouting, -100, "nat"),
        ("nat", "INPUT", NetfilterHook::Input, 100, "nat"),
        ("nat", "OUTPUT", NetfilterHook::Output, -100, "nat"),
        ("nat", "POSTROUTING", NetfilterHook::Postrouting, 100, "nat"),
    ] {
        chains.push(ChainSummary {
            family,
            table: String::from(table),
            name: String::from(name),
            hooknum: hooknum(hook),
            priority,
            chain_type: String::from(chain_type),
        });
    }
}

fn handle_newtable(
    netns: &NetNamespacePayload,
    cred: Cred,
    family: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let name = attr_string(&attrs, NFTA_TABLE_NAME).ok_or(Errno::EINVAL)?;
    with_nft_state_mut(netns, |state| {
        if state
            .tables
            .iter()
            .any(|table| table.family == family && table.name == name)
        {
            return;
        }
        state.tables.push(NftTableObject {
            family,
            name: name.to_string(),
        });
    });
    Ok(())
}

fn handle_deltable(
    netns: &NetNamespacePayload,
    cred: Cred,
    family: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let name = attr_string(&attrs, NFTA_TABLE_NAME).ok_or(Errno::EINVAL)?;
    if let Ok(table) = netfilter_table_from_name(name) {
        remove_netfilter_rules_for_table_in_namespace_for_test_or_bootstrap(netns, table);
    }
    with_nft_state_mut(netns, |state| {
        state
            .tables
            .retain(|table| !(table.family == family && table.name == name));
        state
            .chains
            .retain(|chain| !(chain.family == family && chain.table == name));
    });
    Ok(())
}

fn handle_newchain(
    netns: &NetNamespacePayload,
    cred: Cred,
    family: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let table = attr_string(&attrs, NFTA_CHAIN_TABLE).ok_or(Errno::EINVAL)?;
    let name = attr_string(&attrs, NFTA_CHAIN_NAME).ok_or(Errno::EINVAL)?;
    let (hook, priority) = attr_payload(&attrs, NFTA_CHAIN_HOOK)
        .map(parse_chain_hook)
        .transpose()?
        .unwrap_or((hook_from_name(name)?, default_chain_priority(table, name)));
    let chain_type = attr_string(&attrs, NFTA_CHAIN_TYPE).unwrap_or_else(|| table_type_name(table));

    handle_newtable(netns, cred, family, &table_message_payload(family, table))?;

    with_nft_state_mut(netns, |state| {
        if let Some(existing) = state
            .chains
            .iter_mut()
            .find(|chain| chain.family == family && chain.table == table && chain.name == name)
        {
            existing.hook = hook;
            existing.priority = priority;
            existing.chain_type = chain_type.to_string();
            return;
        }
        state.chains.push(NftChainObject {
            family,
            table: table.to_string(),
            name: name.to_string(),
            hook,
            priority,
            chain_type: chain_type.to_string(),
        });
    });
    Ok(())
}

fn handle_delchain(
    netns: &NetNamespacePayload,
    cred: Cred,
    family: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let table = attr_string(&attrs, NFTA_CHAIN_TABLE).ok_or(Errno::EINVAL)?;
    let name = attr_string(&attrs, NFTA_CHAIN_NAME).ok_or(Errno::EINVAL)?;
    if let (Ok(table_kind), Ok(hook)) = (
        netfilter_table_from_name(table),
        hook_from_chain(netns, table, name),
    ) {
        remove_netfilter_rules_for_chain_in_namespace_for_test_or_bootstrap(
            netns, table_kind, hook,
        );
    }
    with_nft_state_mut(netns, |state| {
        state.chains.retain(|chain| {
            !(chain.family == family && chain.table == table && chain.name == name)
        });
    });
    Ok(())
}

fn handle_newrule(
    netns: &NetNamespacePayload,
    cred: Cred,
    family: u8,
    payload: &[u8],
) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let table_name = attr_string(&attrs, NFTA_RULE_TABLE).ok_or(Errno::EINVAL)?;
    let chain_name = attr_string(&attrs, NFTA_RULE_CHAIN).ok_or(Errno::EINVAL)?;
    let table = netfilter_table_from_name(table_name)?;
    let hook = hook_from_chain(netns, table_name, chain_name)?;
    let mut parsed = NftRuleParse::default();
    if let Some(exprs) = attr_payload(&attrs, NFTA_RULE_EXPRESSIONS) {
        parsed = parse_rule_expressions(exprs)?;
    }
    if parsed.target.is_none() {
        if let Some(userdata) = attr_payload(&attrs, NFTA_RULE_USERDATA) {
            parsed = parse_rule_userdata(userdata)?;
        }
    }
    let target = parsed.target.ok_or(Errno::EOPNOTSUPP)?;
    add_netfilter_rule_in_namespace_for_test_or_bootstrap(
        netns,
        NetfilterRule {
            table,
            hook,
            protocol: parsed.protocol,
            src: parsed.src,
            dst: parsed.dst,
            dst_port: parsed.dst_port,
            in_iface: parsed.in_iface,
            out_iface: parsed.out_iface,
            target,
            compat_target: parsed.compat_target,
            to_addr: parsed.to_addr,
            to_port: parsed.to_port,
        },
    )?;

    handle_newchain(
        netns,
        cred,
        family,
        &chain_message_payload(family, table_name, chain_name, hook),
    )
}

fn handle_delrule(netns: &NetNamespacePayload, cred: Cred, payload: &[u8]) -> Result<(), Errno> {
    let _auth = require_net_admin(cred)?;
    let attrs = parse_nfmsg_attrs(payload)?;
    let handle = attr_u64(&attrs, NFTA_RULE_HANDLE).ok_or(Errno::EINVAL)?;
    let index = handle.checked_sub(1).ok_or(Errno::EINVAL)? as usize;
    remove_netfilter_rule_in_namespace_for_test_or_bootstrap(netns, index)
}

fn build_table_message(seq: u32, pid: u32, flags: u16, family: u8, table: TableSummary) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_TABLE_NAME, &table.name);
    push_attr_u32(&mut payload, NFTA_TABLE_FLAGS, 0);
    push_attr_u32(&mut payload, NFTA_TABLE_USE, table.chains);
    build_nlmsg(nft_msg(NFT_MSG_NEWTABLE), flags, seq, pid, &payload)
}

fn build_chain_message(
    seq: u32,
    pid: u32,
    flags: u16,
    family: u8,
    chain: ChainSummary,
    handle: u64,
) -> Vec<u8> {
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_CHAIN_TABLE, &chain.table);
    push_attr_u64(&mut payload, NFTA_CHAIN_HANDLE, handle);
    push_attr_string(&mut payload, NFTA_CHAIN_NAME, &chain.name);
    push_nested_attr(&mut payload, NFTA_CHAIN_HOOK, |nested| {
        push_attr_u32(nested, NFTA_HOOK_HOOKNUM, chain.hooknum);
        push_attr_u32(nested, NFTA_HOOK_PRIORITY, chain.priority as u32);
    });
    push_attr_u32(&mut payload, NFTA_CHAIN_POLICY, NF_ACCEPT);
    push_attr_string(&mut payload, NFTA_CHAIN_TYPE, &chain.chain_type);
    push_attr_u32(&mut payload, NFTA_CHAIN_FLAGS, NFT_CHAIN_BASE);
    build_nlmsg(nft_msg(NFT_MSG_NEWCHAIN), flags, seq, pid, &payload)
}

fn build_rule_message(
    seq: u32,
    pid: u32,
    flags: u16,
    family: u8,
    rule: NetfilterRule,
    handle: u64,
) -> Vec<u8> {
    let chain = chain_for_rule(rule, family);
    let mut payload = nfgenmsg(family);
    push_attr_string(&mut payload, NFTA_RULE_TABLE, &chain.table);
    push_attr_string(&mut payload, NFTA_RULE_CHAIN, &chain.name);
    push_attr_u64(&mut payload, NFTA_RULE_HANDLE, handle);
    push_nested_attr(&mut payload, NFTA_RULE_EXPRESSIONS, |exprs| {
        push_rule_expressions(exprs, rule);
    });
    let summary = rule_summary(rule);
    push_attr(&mut payload, NFTA_RULE_USERDATA, summary.as_bytes());
    build_nlmsg(nft_msg(NFT_MSG_NEWRULE), flags, seq, pid, &payload)
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
            NetfilterTable::Filter => String::from("filter"),
            NetfilterTable::Nat => String::from("nat"),
        },
        chains: 1,
    }
}

fn chain_for_rule(rule: NetfilterRule, family: u8) -> ChainSummary {
    let table = match rule.table {
        NetfilterTable::Filter => String::from("filter"),
        NetfilterTable::Nat => String::from("nat"),
    };
    ChainSummary {
        family,
        table,
        name: hook_name_for_rule(rule).to_string(),
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
            NetfilterTable::Filter => String::from("filter"),
            NetfilterTable::Nat => String::from("nat"),
        },
    }
}

fn hook_name_for_rule(rule: NetfilterRule) -> &'static str {
    if rule.compat_target.is_some() {
        return hook_name_upper(rule.hook);
    }
    hook_name(rule.hook)
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

fn hook_name_upper(hook: NetfilterHook) -> &'static str {
    match hook {
        NetfilterHook::Prerouting => "PREROUTING",
        NetfilterHook::Input => "INPUT",
        NetfilterHook::Forward => "FORWARD",
        NetfilterHook::Output => "OUTPUT",
        NetfilterHook::Postrouting => "POSTROUTING",
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

fn parse_rule_expressions(bytes: &[u8]) -> Result<NftRuleParse, Errno> {
    let mut parsed = NftRuleParse::default();
    let mut regs = Vec::<RegisterBinding>::new();
    for expr in parse_attrs(bytes)? {
        let expr_attrs = parse_attrs(expr.payload)?;
        let name = attr_string(&expr_attrs, NFTA_EXPR_NAME).ok_or(Errno::EINVAL)?;
        let data = attr_payload(&expr_attrs, NFTA_EXPR_DATA).unwrap_or(&[]);
        match name {
            "meta" => parse_meta_expr(data, &mut regs)?,
            "payload" => parse_payload_expr(data, &mut regs)?,
            "bitwise" => parse_bitwise_expr(data, &mut regs)?,
            "cmp" => parse_cmp_expr(data, &regs, &mut parsed)?,
            "immediate" => parse_immediate_expr(data, &mut regs, &mut parsed)?,
            "masq" => parsed.target = Some(NetfilterTarget::Masquerade),
            "target" => parse_target_expr(data, &mut parsed)?,
            "nat" => parse_nat_expr(data, &regs, &mut parsed)?,
            "counter" => {}
            _ => return Err(Errno::EOPNOTSUPP),
        }
    }
    Ok(parsed)
}

fn push_rule_expressions(out: &mut Vec<u8>, rule: NetfilterRule) {
    let mut next = 1u16;
    if let Some(in_iface) = rule.in_iface {
        push_ifname_match(out, &mut next, NFT_META_IIFNAME, in_iface);
    }
    if let Some(out_iface) = rule.out_iface {
        push_ifname_match(out, &mut next, NFT_META_OIFNAME, out_iface);
    }
    if let Some(src) = rule.src {
        push_ipv4_cidr_match(out, &mut next, NFT_PAYLOAD_NETWORK_HEADER, 12, src);
    }
    if let Some(dst) = rule.dst {
        push_ipv4_cidr_match(out, &mut next, NFT_PAYLOAD_NETWORK_HEADER, 16, dst);
    }
    match rule.target {
        NetfilterTarget::Masquerade => {
            if let Some(name) = rule.compat_target {
                push_compat_target_expr(out, &mut next, name);
            } else {
                push_empty_expr(out, &mut next, "masq");
            }
        }
        NetfilterTarget::Accept => push_immediate_verdict_expr(out, &mut next, NF_ACCEPT),
        NetfilterTarget::Drop => push_immediate_verdict_expr(out, &mut next, NF_DROP),
        NetfilterTarget::Dnat => push_summary_only_expr(out, &mut next),
    }
}

fn push_ifname_match(out: &mut Vec<u8>, next: &mut u16, key: u32, name: &str) {
    let reg = NFT_REG32_00;
    let mut value = [0u8; IFNAMSIZ];
    let bytes = name.as_bytes();
    let len = bytes.len().min(IFNAMSIZ.saturating_sub(1));
    value[..len].copy_from_slice(&bytes[..len]);
    push_meta_expr(out, next, reg, key);
    push_cmp_expr(out, next, reg, &value);
}

fn push_meta_expr(out: &mut Vec<u8>, next: &mut u16, dreg: u32, key: u32) {
    push_expr(out, next, "meta", |data| {
        push_attr_u32(data, NFTA_META_DREG, dreg);
        push_attr_u32(data, NFTA_META_KEY, key);
    });
}

fn push_ipv4_cidr_match(
    out: &mut Vec<u8>,
    next: &mut u16,
    base: u32,
    offset: u32,
    cidr: NetfilterIpv4Cidr,
) {
    if cidr.prefix_len == 0 {
        return;
    }
    let reg = NFT_REG32_00;
    let octets = cidr.addr.octets();
    if cidr.prefix_len.is_multiple_of(8) {
        let len = (cidr.prefix_len / 8).clamp(1, 4);
        push_payload_expr(out, next, reg, base, offset, len as u32);
        push_cmp_expr(out, next, reg, &octets[..len as usize]);
        return;
    }
    push_payload_expr(out, next, reg, base, offset, 4);
    push_bitwise_mask_expr(out, next, reg, reg + 1, ipv4_prefix_mask(cidr.prefix_len));
    push_cmp_expr(out, next, reg + 1, &octets);
}

fn push_payload_expr(
    out: &mut Vec<u8>,
    next: &mut u16,
    dreg: u32,
    base: u32,
    offset: u32,
    len: u32,
) {
    push_expr(out, next, "payload", |data| {
        push_attr_u32(data, NFTA_PAYLOAD_DREG, dreg);
        push_attr_u32(data, NFTA_PAYLOAD_BASE, base);
        push_attr_u32(data, NFTA_PAYLOAD_OFFSET, offset);
        push_attr_u32(data, NFTA_PAYLOAD_LEN, len);
    });
}

fn push_bitwise_mask_expr(out: &mut Vec<u8>, next: &mut u16, sreg: u32, dreg: u32, mask: [u8; 4]) {
    push_expr(out, next, "bitwise", |data| {
        push_attr_u32(data, NFTA_BITWISE_SREG, sreg);
        push_attr_u32(data, NFTA_BITWISE_DREG, dreg);
        push_attr_u32(data, NFTA_BITWISE_LEN, 4);
        push_nested_attr(data, NFTA_BITWISE_MASK, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, &mask);
        });
        push_nested_attr(data, NFTA_BITWISE_XOR, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, &[0, 0, 0, 0]);
        });
    });
}

fn push_cmp_expr(out: &mut Vec<u8>, next: &mut u16, sreg: u32, value: &[u8]) {
    push_expr(out, next, "cmp", |data| {
        push_attr_u32(data, NFTA_CMP_SREG, sreg);
        push_attr_u32(data, NFTA_CMP_OP, NFT_CMP_EQ);
        push_nested_attr(data, NFTA_CMP_DATA, |nested| {
            push_attr(nested, NFTA_DATA_VALUE, value);
        });
    });
}

fn push_immediate_verdict_expr(out: &mut Vec<u8>, next: &mut u16, verdict: u32) {
    push_expr(out, next, "immediate", |data| {
        push_attr_u32(data, NFTA_IMMEDIATE_DREG, NFT_REG_VERDICT);
        push_nested_attr(data, NFTA_IMMEDIATE_DATA, |nested| {
            push_nested_attr(nested, NFTA_DATA_VERDICT, |verdict_attrs| {
                push_attr_u32(verdict_attrs, NFTA_VERDICT_CODE, verdict);
            });
        });
    });
}

fn push_empty_expr(out: &mut Vec<u8>, next: &mut u16, name: &str) {
    push_expr(out, next, name, |_| {});
}

fn push_compat_target_expr(out: &mut Vec<u8>, next: &mut u16, name: &str) {
    push_expr(out, next, "target", |data| {
        push_attr_string(data, NFTA_TARGET_NAME, name);
        push_attr_u32(data, NFTA_TARGET_REV, 0);
        push_attr(data, NFTA_TARGET_INFO, &[]);
    });
}

fn push_summary_only_expr(out: &mut Vec<u8>, next: &mut u16) {
    push_empty_expr(out, next, "counter");
}

fn push_expr(out: &mut Vec<u8>, next: &mut u16, name: &str, build_data: impl FnOnce(&mut Vec<u8>)) {
    let mut expr = Vec::new();
    push_attr_string(&mut expr, NFTA_EXPR_NAME, name);
    let mut data = Vec::new();
    build_data(&mut data);
    if !data.is_empty() {
        push_attr(&mut expr, NFTA_EXPR_DATA | NLA_F_NESTED, &data);
    }
    push_attr(out, NFTA_LIST_ELEM | NLA_F_NESTED, &expr);
    *next = (*next).saturating_add(1);
}

fn parse_meta_expr(data: &[u8], regs: &mut Vec<RegisterBinding>) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let dreg = attr_u32(&attrs, NFTA_META_DREG).ok_or(Errno::EINVAL)?;
    let key = attr_u32(&attrs, NFTA_META_KEY).ok_or(Errno::EINVAL)?;
    bind_register(regs, dreg, RegisterBindingKind::Meta(key));
    Ok(())
}

fn parse_payload_expr(data: &[u8], regs: &mut Vec<RegisterBinding>) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let dreg = attr_u32(&attrs, NFTA_PAYLOAD_DREG).ok_or(Errno::EINVAL)?;
    let base = attr_u32(&attrs, NFTA_PAYLOAD_BASE).ok_or(Errno::EINVAL)?;
    let offset = attr_u32(&attrs, NFTA_PAYLOAD_OFFSET).ok_or(Errno::EINVAL)?;
    let len = attr_u32(&attrs, NFTA_PAYLOAD_LEN).ok_or(Errno::EINVAL)?;
    bind_register(
        regs,
        dreg,
        RegisterBindingKind::Payload {
            base,
            offset,
            len,
            prefix_len: None,
        },
    );
    Ok(())
}

fn parse_bitwise_expr(data: &[u8], regs: &mut Vec<RegisterBinding>) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let sreg = attr_u32(&attrs, NFTA_BITWISE_SREG).ok_or(Errno::EINVAL)?;
    let dreg = attr_u32(&attrs, NFTA_BITWISE_DREG).ok_or(Errno::EINVAL)?;
    let len = attr_u32(&attrs, NFTA_BITWISE_LEN).ok_or(Errno::EINVAL)?;
    let mask = attr_nested_data_value(&attrs, NFTA_BITWISE_MASK).ok_or(Errno::EINVAL)?;
    let xor = attr_nested_data_value(&attrs, NFTA_BITWISE_XOR).unwrap_or(&[]);
    if !xor.iter().all(|byte| *byte == 0) {
        return Err(Errno::EOPNOTSUPP);
    }
    let Some(RegisterBindingKind::Payload {
        base,
        offset,
        len: source_len,
        ..
    }) = register_binding(regs, sreg)
    else {
        return Err(Errno::EOPNOTSUPP);
    };
    if len != source_len || mask.len() != len as usize {
        return Err(Errno::EINVAL);
    }
    bind_register(
        regs,
        dreg,
        RegisterBindingKind::Payload {
            base,
            offset,
            len,
            prefix_len: ipv4_mask_prefix_len(mask),
        },
    );
    Ok(())
}

fn parse_cmp_expr(
    data: &[u8],
    regs: &[RegisterBinding],
    parsed: &mut NftRuleParse,
) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let sreg = attr_u32(&attrs, NFTA_CMP_SREG).ok_or(Errno::EINVAL)?;
    let op = attr_u32(&attrs, NFTA_CMP_OP).ok_or(Errno::EINVAL)?;
    if op != NFT_CMP_EQ {
        return Err(Errno::EOPNOTSUPP);
    }
    let value = attr_nested_data_value(&attrs, NFTA_CMP_DATA).ok_or(Errno::EINVAL)?;
    match register_binding(regs, sreg).ok_or(Errno::EINVAL)? {
        RegisterBindingKind::Meta(NFT_META_IIFNAME) => {
            parsed.in_iface = Some(leak_ascii_nul_string(value)?);
        }
        RegisterBindingKind::Meta(NFT_META_OIFNAME) => {
            parsed.out_iface = Some(leak_ascii_nul_string(value)?);
        }
        RegisterBindingKind::Meta(NFT_META_L4PROTO) => {
            parsed.protocol = Some(protocol_from_data(value)?);
        }
        RegisterBindingKind::Payload {
            base,
            offset,
            len,
            prefix_len: _,
        } if base == NFT_PAYLOAD_NETWORK_HEADER && offset == 9 && len == 1 => {
            parsed.protocol = Some(protocol_from_data(value)?);
        }
        RegisterBindingKind::Payload {
            base,
            offset,
            len,
            prefix_len,
        } if base == NFT_PAYLOAD_NETWORK_HEADER && offset == 12 && len <= 4 => {
            parsed.src = Some(ipv4_cidr_from_payload_data(value, len, prefix_len)?);
        }
        RegisterBindingKind::Payload {
            base,
            offset,
            len,
            prefix_len,
        } if base == NFT_PAYLOAD_NETWORK_HEADER && offset == 16 && len <= 4 => {
            parsed.dst = Some(ipv4_cidr_from_payload_data(value, len, prefix_len)?);
        }
        RegisterBindingKind::Payload {
            base, offset, len, ..
        } if base == NFT_PAYLOAD_TRANSPORT_HEADER && offset == 2 && len == 2 => {
            parsed.dst_port = Some(u16_from_be_data(value)?);
        }
        _ => return Err(Errno::EOPNOTSUPP),
    }
    Ok(())
}

fn parse_immediate_expr(
    data: &[u8],
    regs: &mut Vec<RegisterBinding>,
    parsed: &mut NftRuleParse,
) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let dreg = attr_u32(&attrs, NFTA_IMMEDIATE_DREG).ok_or(Errno::EINVAL)?;
    let data_attrs = parse_attrs(attr_payload(&attrs, NFTA_IMMEDIATE_DATA).ok_or(Errno::EINVAL)?)?;
    if dreg == NFT_REG_VERDICT {
        let verdict = attr_payload(&data_attrs, NFTA_DATA_VERDICT).ok_or(Errno::EINVAL)?;
        let verdict_attrs = parse_attrs(verdict)?;
        let code = attr_u32(&verdict_attrs, NFTA_VERDICT_CODE).ok_or(Errno::EINVAL)?;
        parsed.target = match code {
            NF_ACCEPT => Some(NetfilterTarget::Accept),
            NF_DROP => Some(NetfilterTarget::Drop),
            _ => return Err(Errno::EOPNOTSUPP),
        };
        return Ok(());
    }
    let value = attr_payload(&data_attrs, NFTA_DATA_VALUE)
        .ok_or(Errno::EINVAL)?
        .to_vec();
    bind_register(regs, dreg, RegisterBindingKind::Value(value));
    Ok(())
}

fn parse_nat_expr(
    data: &[u8],
    regs: &[RegisterBinding],
    parsed: &mut NftRuleParse,
) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let nat_type = attr_u32(&attrs, NFTA_NAT_TYPE).ok_or(Errno::EINVAL)?;
    let family = attr_u32(&attrs, NFTA_NAT_FAMILY).unwrap_or(NFPROTO_IPV4 as u32);
    if nat_type != NFT_NAT_DNAT || family as u8 != NFPROTO_IPV4 {
        return Err(Errno::EOPNOTSUPP);
    }
    let addr_reg = attr_u32(&attrs, NFTA_NAT_REG_ADDR_MIN).ok_or(Errno::EINVAL)?;
    let port_reg = attr_u32(&attrs, NFTA_NAT_REG_PROTO_MIN);
    parsed.target = Some(NetfilterTarget::Dnat);
    parsed.to_addr = Some(ipv4_from_data(
        register_value(regs, addr_reg).ok_or(Errno::EINVAL)?,
    )?);
    if let Some(port_reg) = port_reg {
        parsed.to_port = Some(u16_from_be_data(
            register_value(regs, port_reg).ok_or(Errno::EINVAL)?,
        )?);
    }
    Ok(())
}

fn parse_target_expr(data: &[u8], parsed: &mut NftRuleParse) -> Result<(), Errno> {
    let attrs = parse_attrs(data)?;
    let name = attr_string(&attrs, NFTA_TARGET_NAME).ok_or(Errno::EINVAL)?;
    match name {
        "MASQUERADE" => {
            parsed.target = Some(NetfilterTarget::Masquerade);
            parsed.compat_target = Some("MASQUERADE");
            Ok(())
        }
        _ => Err(Errno::EOPNOTSUPP),
    }
}

fn parse_rule_userdata(bytes: &[u8]) -> Result<NftRuleParse, Errno> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| Errno::EINVAL)?
        .trim_matches(char::from(0))
        .trim();
    let mut parsed = NftRuleParse::default();
    for field in text.split_ascii_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "target" => parsed.target = Some(target_from_name(value)?),
            "proto" if value != "*" => parsed.protocol = Some(protocol_from_name(value)?),
            "src" if value != "*" => parsed.src = Some(cidr_from_text(value)?),
            "dst" if value != "*" => parsed.dst = Some(cidr_from_text(value)?),
            "dport" if value != "*" => parsed.dst_port = Some(parse_u16_text(value)?),
            "in" if value != "*" => parsed.in_iface = Some(leak_str(value)),
            "out" if value != "*" => parsed.out_iface = Some(leak_str(value)),
            "to" if value != "*" => {
                if let Some((addr, port)) = value.split_once(':') {
                    parsed.to_addr = Some(ipv4_from_text(addr)?);
                    parsed.to_port = Some(parse_u16_text(port)?);
                } else {
                    parsed.to_addr = Some(ipv4_from_text(value)?);
                }
            }
            _ => {}
        }
    }
    Ok(parsed)
}

mod wire;
use wire::*;

#[derive(Clone, Copy)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct NftNamespaceState {
    namespace_key: usize,
    tables: Vec<NftTableObject>,
    chains: Vec<NftChainObject>,
}

impl NftNamespaceState {
    fn new(namespace_key: usize) -> Self {
        Self {
            namespace_key,
            tables: Vec::new(),
            chains: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NftTableObject {
    family: u8,
    name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NftChainObject {
    family: u8,
    table: String,
    name: String,
    hook: NetfilterHook,
    priority: i32,
    chain_type: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct NftRuleParse {
    protocol: Option<NetfilterConntrackProtocol>,
    src: Option<NetfilterIpv4Cidr>,
    dst: Option<NetfilterIpv4Cidr>,
    dst_port: Option<u16>,
    in_iface: Option<&'static str>,
    out_iface: Option<&'static str>,
    target: Option<NetfilterTarget>,
    compat_target: Option<&'static str>,
    to_addr: Option<Ipv4Address>,
    to_port: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisterBinding {
    reg: u32,
    kind: RegisterBindingKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RegisterBindingKind {
    Meta(u32),
    Payload {
        base: u32,
        offset: u32,
        len: u32,
        prefix_len: Option<u8>,
    },
    Value(Vec<u8>),
}

#[derive(Clone)]
struct TableSummary {
    family: u8,
    name: String,
    chains: u32,
}

#[derive(Clone)]
struct ChainSummary {
    family: u8,
    table: String,
    name: String,
    hooknum: u32,
    priority: i32,
    chain_type: String,
}
