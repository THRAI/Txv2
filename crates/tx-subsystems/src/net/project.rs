//! Read-only network projection helpers.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use smoltcp::time::Instant;

use crate::net::device::EthernetAddress;
use crate::net::netfilter::{
    netfilter_conntrack_snapshot_for_namespace, netfilter_rule_snapshots_for_namespace,
    NetfilterConntrackProtocol, NetfilterHook, NetfilterIpv4Cidr, NetfilterNatKind, NetfilterTable,
    NetfilterTarget,
};
use crate::net::protocol::{ArpSnapshotState, EtherIface, NetStatsSnapshot};
use crate::net::structure::{Ipv4Address, Ipv6Address};
use crate::net::{
    AddressFamily, IpAddress, IpEndpoint, NetNamespacePayload, NetNamespaceRouteInfo,
    SocketProtocol, TcpState,
};

pub fn proc_net_arp_snapshot_text(ifaces: &[&EtherIface], now: Instant) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "IP address       HW type     Flags       HW address            Device       State"
    );

    for iface in ifaces {
        for entry in iface.arp_snapshot(now) {
            let mac = entry
                .mac
                .map(format_mac)
                .unwrap_or_else(|| String::from("00:00:00:00:00:00"));
            let flags = match entry.state {
                ArpSnapshotState::Resolved => "0x2",
                ArpSnapshotState::Pending | ArpSnapshotState::Failed => "0x0",
            };
            let _ = writeln!(
                out,
                "{:<16} {:<11} {:<11} {:<21} {:<12} {}",
                format_ipv4(entry.ip),
                "0x1",
                flags,
                mac,
                entry.iface_name,
                arp_state_name(entry.state),
            );
        }
    }

    out
}

pub fn proc_net_arp_snapshot_zero_text(ifaces: &[&EtherIface]) -> String {
    proc_net_arp_snapshot_text(ifaces, Instant::ZERO)
}

pub fn proc_net_neigh_snapshot_text(ifaces: &[&EtherIface], now: Instant) -> String {
    let mut out = String::new();
    for iface in ifaces {
        for entry in iface.arp_snapshot(now) {
            let mac = entry
                .mac
                .map(format_mac)
                .unwrap_or_else(|| String::from("00:00:00:00:00:00"));
            let nud = match entry.state {
                ArpSnapshotState::Resolved => "REACHABLE",
                ArpSnapshotState::Pending => "INCOMPLETE",
                ArpSnapshotState::Failed => "FAILED",
            };
            let _ = writeln!(
                out,
                "{} dev {} lladdr {} {}",
                format_ipv4(entry.ip),
                entry.iface_name,
                mac,
                nud
            );
        }
        for entry in iface.ndisc_snapshot(now) {
            let mac = entry
                .mac
                .map(format_mac)
                .unwrap_or_else(|| String::from("00:00:00:00:00:00"));
            let nud = match entry.state {
                ArpSnapshotState::Resolved => "REACHABLE",
                ArpSnapshotState::Pending => "INCOMPLETE",
                ArpSnapshotState::Failed => "FAILED",
            };
            let _ = writeln!(
                out,
                "{} dev {} lladdr {} {}",
                format_ipv6(entry.ip),
                entry.iface_name,
                mac,
                nud
            );
        }
    }
    out
}

pub fn proc_net_neigh_snapshot_text_for_namespace(netns: &NetNamespacePayload) -> String {
    proc_net_neigh_snapshot_text(&netns.ether_ifaces_snapshot(), Instant::ZERO)
}

pub fn proc_net_dev_snapshot_text(ifaces: &[&EtherIface]) -> String {
    let mut out = String::new();
    push_proc_net_dev_header(&mut out);

    for iface in ifaces {
        let stats = iface.net_stats_snapshot();
        push_proc_net_dev_line(&mut out, stats);
    }

    out
}

pub fn proc_net_dev_snapshot_text_for_namespace(netns: &NetNamespacePayload) -> String {
    let mut out = String::new();
    push_proc_net_dev_header(&mut out);

    let ifaces = netns.ether_ifaces_snapshot();
    for link in netns.link_snapshot() {
        if let Some(stats) = ifaces
            .iter()
            .find(|iface| iface.name == link.name)
            .map(|iface| iface.net_stats_snapshot())
        {
            push_proc_net_dev_line(&mut out, stats);
        } else {
            push_proc_net_dev_line(
                &mut out,
                NetStatsSnapshot {
                    iface_name: link.name,
                    rx_packets: 0,
                    tx_packets: 0,
                    rx_bytes: 0,
                    tx_bytes: 0,
                    rx_errors: 0,
                    tx_errors: 0,
                    rx_dropped: 0,
                },
            );
        }
    }

    out
}

pub fn proc_net_route_snapshot_text(netns: &NetNamespacePayload) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT"
    );

    for route in netns.route_snapshot() {
        let iface = route.oif_name.unwrap_or("*");
        let flags = proc_route_flags(route);
        let gateway = route.gateway.unwrap_or(Ipv4Address::UNSPECIFIED);
        let _ = writeln!(
            out,
            "{iface}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
            proc_route_hex(route.dst),
            proc_route_hex(gateway),
            flags,
            proc_route_hex(prefix_len_to_netmask(route.prefix_len)),
        );
    }

    out
}

/// `/proc/net/ipv6_route` — the kernel IPv6 FIB in its native format, one line
/// per route, no header, space-separated lowercase hex without `0x`:
/// `<dst 32hex> <dstplen 2hex> <src 32hex> <srcplen 2hex> <nexthop 32hex>
///  <metric 8hex> <refcnt 8hex> <use 8hex> <flags 8hex> <devname>`.
/// `ip -6 route` parses this; the address fields are the 16 bytes rendered as
/// 32 hex chars. Only RTF_UP (0x1) and RTF_GATEWAY (0x2) flags are modeled.
pub fn proc_net_ipv6_route_snapshot_text(netns: &NetNamespacePayload) -> String {
    let mut out = String::new();
    for route in netns.route6_snapshot() {
        let devname = route.oif_name.unwrap_or("*");
        let mut flags: u32 = 0x0000_0001;
        let nexthop = if let Some(gateway) = route.gateway {
            flags |= 0x0000_0002;
            gateway
        } else {
            Ipv6Address::UNSPECIFIED
        };
        let _ = writeln!(
            out,
            "{} {:02x} {} {:02x} {} {:08x} {:08x} {:08x} {:08x} {}",
            proc_ipv6_route_hex(route.dst),
            route.prefix_len,
            proc_ipv6_route_hex(Ipv6Address::UNSPECIFIED),
            0u8,
            proc_ipv6_route_hex(nexthop),
            0u32,
            0u32,
            0u32,
            flags,
            devname,
        );
    }
    out
}

/// Render an IPv6 address as 32 lowercase hex chars (the 16 octets concatenated,
/// no separators), matching the `/proc/net/ipv6_route` address column format.
fn proc_ipv6_route_hex(addr: Ipv6Address) -> String {
    let mut out = String::new();
    for byte in addr.octets() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn proc_net_netfilter_rules_text() -> String {
    let netns = crate::net::namespace::initial_net_namespace_payload();
    proc_net_netfilter_rules_text_for_namespace(&netns)
}

pub fn proc_net_netfilter_rules_text_for_namespace(netns: &NetNamespacePayload) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "idx\tpackets\tbytes\ttable\thook\tproto\ttarget\tsrc\tdst\tdport\tin\tout\tto"
    );

    for (idx, snapshot) in netfilter_rule_snapshots_for_namespace(netns)
        .into_iter()
        .enumerate()
    {
        let rule = snapshot.rule;
        let src = rule
            .src
            .map(format_cidr)
            .unwrap_or_else(|| String::from("*"));
        let dst = rule
            .dst
            .map(format_cidr)
            .unwrap_or_else(|| String::from("*"));
        let proto = rule.protocol.map(conntrack_protocol_name).unwrap_or("*");
        let dst_port = rule
            .dst_port
            .map(format_u16)
            .unwrap_or_else(|| String::from("*"));
        let in_iface = rule.in_iface.unwrap_or("*");
        let out_iface = rule.out_iface.unwrap_or("*");
        let to = match (rule.to_addr, rule.to_port) {
            (Some(addr), Some(port)) => {
                let mut out = format_ipv4(addr);
                let _ = write!(out, ":{port}");
                out
            }
            (Some(addr), None) => format_ipv4(addr),
            _ => String::from("*"),
        };
        let _ = writeln!(
            out,
            "{idx}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            snapshot.counters.packets,
            snapshot.counters.bytes,
            table_name(rule.table),
            hook_name(rule.hook),
            proto,
            target_name(rule.target),
            src,
            dst,
            dst_port,
            in_iface,
            out_iface,
            to,
        );
    }

    out
}

pub fn proc_net_nf_conntrack_text() -> String {
    let netns = crate::net::namespace::initial_net_namespace_payload();
    proc_net_nf_conntrack_text_for_namespace(&netns)
}

pub fn proc_net_nf_conntrack_text_for_namespace(netns: &NetNamespacePayload) -> String {
    let mut out = String::new();

    for entry in netfilter_conntrack_snapshot_for_namespace(netns) {
        let proto = conntrack_protocol_name(entry.protocol);
        let kind = nat_kind_name(entry.kind);
        let _ = writeln!(
            out,
            "{proto} {kind} original={} sport={} dst={} dport={} translated={} tport={}",
            format_ipv4(entry.original_src),
            entry.original_src_port,
            format_ipv4(entry.external_dst),
            entry.external_dst_port,
            format_ipv4(entry.masquerade_src),
            entry.masquerade_src_port,
        );
    }

    out
}

#[derive(Clone)]
struct TcpProcRow {
    local: IpEndpoint,
    remote: IpEndpoint,
    state: u8,
    uid: u32,
    inode: u64,
    pid: u32,
    fd: u32,
    comm: String,
}

pub fn proc_net_tcp_socket_table_text(
    family: AddressFamily,
    netns: Option<&NetNamespacePayload>,
) -> String {
    let mut out = proc_socket_table_header();
    for (idx, row) in tcp_proc_rows(family, netns, false).into_iter().enumerate() {
        let _ = writeln!(
            out,
            "{:4}: {} {} {:02X} 00000000:00000000 00:00000000 00000000 {:5}        0 {} 1 0000000000000000 100 0 0 10 0",
            idx,
            proc_endpoint(row.local),
            proc_endpoint(row.remote),
            row.state,
            row.uid,
            row.inode
        );
    }
    out
}

pub fn proc_net_tcp_listener_process_table_text(
    family: AddressFamily,
    netns: Option<&NetNamespacePayload>,
) -> String {
    let mut out =
        String::from("State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n");
    for row in tcp_proc_rows(family, netns, true) {
        let _ = writeln!(
            out,
            "LISTEN 0 0 {} *:* users:((\"{}\",pid={},fd={}))",
            ss_endpoint(row.local),
            row.comm,
            row.pid,
            row.fd
        );
    }
    out
}

fn proc_socket_table_header() -> String {
    String::from(
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
    )
}

fn tcp_proc_rows(
    family: AddressFamily,
    netns: Option<&NetNamespacePayload>,
    listeners_only: bool,
) -> Vec<TcpProcRow> {
    let mut rows = Vec::new();
    for (pid, alive) in crate::process::all_pids() {
        if !alive {
            continue;
        }
        let Some(process) = crate::process::process_by_pid(pid) else {
            continue;
        };
        if let Some(netns) = netns {
            let Some(process_netns) = process.net_namespace() else {
                continue;
            };
            if !core::ptr::eq(&*process_netns, netns) {
                continue;
            }
        }
        let comm = process_comm_string(&process);
        for (fd, file) in process.open_fds() {
            let Some(socket) = file.socket_identity().cloned() else {
                continue;
            };
            let Some(payload) = socket.acquire_operational() else {
                continue;
            };
            let Some((local, remote, state)) = tcp_proc_state(payload.protocol_snapshot()) else {
                continue;
            };
            if socket.family != family && local.family != family {
                continue;
            }
            if listeners_only && state != 0x0A {
                continue;
            }
            rows.push(TcpProcRow {
                local,
                remote,
                state,
                uid: 0,
                inode: file.rnode().fs_object_id().as_u64(),
                pid: pid.0,
                fd,
                comm: comm.clone(),
            });
        }
    }
    rows
}

fn tcp_proc_state(protocol: SocketProtocol) -> Option<(IpEndpoint, IpEndpoint, u8)> {
    match protocol {
        SocketProtocol::Tcp(TcpState::Listening { local, .. }) => {
            Some((local, unspecified_peer(local.family), 0x0A))
        }
        SocketProtocol::Tcp(TcpState::Connected { local, remote }) => Some((local, remote, 0x01)),
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => Some((local, remote, 0x02)),
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            Some((local, unspecified_peer(local.family), 0x07))
        }
        _ => None,
    }
}

fn unspecified_peer(family: AddressFamily) -> IpEndpoint {
    IpEndpoint::unspecified_for_family(family, 0)
}

fn process_comm_string(
    process: &tx_substrate::zone::Cap<crate::process::ProcessIdentity>,
) -> String {
    if let Some(cmdline) = process.ident_cmdline() {
        if let Some(argv0) = cmdline.split(|b| *b == 0).find(|s| !s.is_empty()) {
            let name = argv0.rsplit(|b| *b == b'/').next().unwrap_or(argv0);
            if !name.is_empty() {
                return String::from_utf8_lossy(name).into_owned();
            }
        }
    }

    let comm = process.ident_comm().unwrap_or([0; 16]);
    let len = comm.iter().position(|b| *b == 0).unwrap_or(comm.len());
    if len == 0 {
        String::from("?")
    } else {
        String::from_utf8_lossy(&comm[..len]).into_owned()
    }
}

fn proc_endpoint(endpoint: IpEndpoint) -> String {
    match endpoint.ip_addr() {
        IpAddress::V4(addr) => {
            let [a, b, c, d] = addr.octets();
            format!("{d:02X}{c:02X}{b:02X}{a:02X}:{:04X}", endpoint.port)
        }
        IpAddress::V6(addr) => {
            let octets = addr.octets();
            let mut out = String::new();
            for chunk in octets.chunks_exact(4).rev() {
                let _ = write!(
                    out,
                    "{:02X}{:02X}{:02X}{:02X}",
                    chunk[3], chunk[2], chunk[1], chunk[0]
                );
            }
            let _ = write!(out, ":{:04X}", endpoint.port);
            out
        }
    }
}

fn ss_endpoint(endpoint: IpEndpoint) -> String {
    match endpoint.ip_addr() {
        IpAddress::V4(addr) => {
            let [a, b, c, d] = addr.octets();
            format!("{a}.{b}.{c}.{d}:{}", endpoint.port)
        }
        IpAddress::V6(addr) => {
            let octets = addr.octets();
            let mut groups = [0u16; 8];
            for (idx, chunk) in octets.chunks_exact(2).enumerate() {
                groups[idx] = u16::from_be_bytes([chunk[0], chunk[1]]);
            }
            format!(
                "[{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}]:{}",
                groups[0],
                groups[1],
                groups[2],
                groups[3],
                groups[4],
                groups[5],
                groups[6],
                groups[7],
                endpoint.port
            )
        }
    }
}

fn push_proc_net_dev_header(out: &mut String) {
    let _ = writeln!(
        out,
        "Inter-|   Receive                                                |  Transmit"
    );
    let _ = writeln!(
        out,
        " face |bytes    packets errs drop |bytes    packets errs"
    );
}

fn push_proc_net_dev_line(out: &mut String, stats: NetStatsSnapshot) {
    let _ = writeln!(
        out,
        "{iface_name:>6}: {rx_bytes:<8} {rx_packets:<7} {rx_errors:<4} {rx_dropped:<4} |{tx_bytes:<8} {tx_packets:<7} {tx_errors:<4}",
        iface_name = stats.iface_name,
        rx_bytes = stats.rx_bytes,
        rx_packets = stats.rx_packets,
        rx_errors = stats.rx_errors,
        rx_dropped = stats.rx_dropped,
        tx_bytes = stats.tx_bytes,
        tx_packets = stats.tx_packets,
        tx_errors = stats.tx_errors,
    );
}

fn arp_state_name(state: ArpSnapshotState) -> &'static str {
    match state {
        ArpSnapshotState::Resolved => "resolved",
        ArpSnapshotState::Pending => "pending",
        ArpSnapshotState::Failed => "failed",
    }
}

fn format_ipv4(addr: Ipv4Address) -> String {
    let [a, b, c, d] = addr.octets();
    let mut out = String::new();
    let _ = write!(out, "{a}.{b}.{c}.{d}");
    out
}

fn format_ipv6(addr: Ipv6Address) -> String {
    let octets = addr.octets();
    let mut hextets = [0u16; 8];
    for idx in 0..8 {
        hextets[idx] = u16::from_be_bytes([octets[idx * 2], octets[idx * 2 + 1]]);
    }

    let mut best_start = None;
    let mut best_len = 0usize;
    let mut idx = 0usize;
    while idx < hextets.len() {
        if hextets[idx] != 0 {
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < hextets.len() && hextets[idx] == 0 {
            idx += 1;
        }
        let len = idx - start;
        if len >= 2 && len > best_len {
            best_start = Some(start);
            best_len = len;
        }
    }

    let mut out = String::new();
    let mut idx = 0usize;
    while idx < hextets.len() {
        if best_start == Some(idx) {
            out.push_str("::");
            idx += best_len;
            if idx >= hextets.len() {
                break;
            }
            continue;
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        let _ = write!(out, "{:x}", hextets[idx]);
        idx += 1;
    }
    if out.is_empty() {
        out.push_str("::");
    }
    out
}

fn format_cidr(cidr: NetfilterIpv4Cidr) -> String {
    let mut out = format_ipv4(cidr.addr);
    let _ = write!(out, "/{}", cidr.prefix_len);
    out
}

fn table_name(table: NetfilterTable) -> &'static str {
    match table {
        NetfilterTable::Filter => "filter",
        NetfilterTable::Nat => "nat",
    }
}

fn hook_name(hook: NetfilterHook) -> &'static str {
    match hook {
        NetfilterHook::Prerouting => "PREROUTING",
        NetfilterHook::Input => "INPUT",
        NetfilterHook::Forward => "FORWARD",
        NetfilterHook::Output => "OUTPUT",
        NetfilterHook::Postrouting => "POSTROUTING",
    }
}

fn target_name(target: NetfilterTarget) -> &'static str {
    match target {
        NetfilterTarget::Accept => "ACCEPT",
        NetfilterTarget::Drop => "DROP",
        NetfilterTarget::Masquerade => "MASQUERADE",
        NetfilterTarget::Dnat => "DNAT",
    }
}

fn nat_kind_name(kind: NetfilterNatKind) -> &'static str {
    match kind {
        NetfilterNatKind::Masquerade => "masquerade",
        NetfilterNatKind::Dnat => "dnat",
    }
}

fn conntrack_protocol_name(protocol: NetfilterConntrackProtocol) -> &'static str {
    match protocol {
        NetfilterConntrackProtocol::Icmp => "icmp",
        NetfilterConntrackProtocol::Tcp => "tcp",
        NetfilterConntrackProtocol::Udp => "udp",
    }
}

fn format_u16(value: u16) -> String {
    let mut out = String::new();
    let _ = write!(out, "{value}");
    out
}

fn format_mac(addr: EthernetAddress) -> String {
    let [a, b, c, d, e, f] = addr.octets();
    let mut out = String::new();
    let _ = write!(out, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}");
    out
}

fn proc_route_flags(route: NetNamespaceRouteInfo) -> u16 {
    let mut flags = 0x0001;
    if route.gateway.is_some() {
        flags |= 0x0002;
    }
    flags
}

fn proc_route_hex(addr: Ipv4Address) -> u32 {
    let [a, b, c, d] = addr.octets();
    u32::from_le_bytes([a, b, c, d])
}

fn prefix_len_to_netmask(prefix_len: u8) -> Ipv4Address {
    let prefix_len = prefix_len.min(32);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    };
    Ipv4Address::new(mask.to_be_bytes())
}
