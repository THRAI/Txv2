//! Read-only network projection helpers.

use alloc::string::String;
use core::fmt::Write;

use smoltcp::time::Instant;

use crate::net::device::EthernetAddress;
use crate::net::netfilter::{
    netfilter_conntrack_snapshot, netfilter_rules_snapshot, NetfilterConntrackProtocol,
    NetfilterHook, NetfilterIpv4Cidr, NetfilterTable, NetfilterTarget,
};
use crate::net::protocol::{ArpSnapshotState, EtherIface};
use crate::net::structure::Ipv4Address;
use crate::net::{NetNamespacePayload, NetNamespaceRouteInfo};

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

pub fn proc_net_dev_snapshot_text(ifaces: &[&EtherIface]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Inter-|   Receive                                                |  Transmit"
    );
    let _ = writeln!(
        out,
        " face |bytes    packets errs drop |bytes    packets errs"
    );

    for iface in ifaces {
        let stats = iface.net_stats_snapshot();
        let _ = writeln!(
            out,
            "{:>6}: {:<8} {:<7} {:<4} {:<4} |{:<8} {:<7} {:<4}",
            stats.iface_name,
            stats.rx_bytes,
            stats.rx_packets,
            stats.rx_errors,
            stats.rx_dropped,
            stats.tx_bytes,
            stats.tx_packets,
            stats.tx_errors,
        );
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

pub fn proc_net_netfilter_rules_text() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "idx\ttable\thook\ttarget\tsrc\tout");

    for (idx, rule) in netfilter_rules_snapshot().into_iter().enumerate() {
        let src = rule
            .src
            .map(format_cidr)
            .unwrap_or_else(|| String::from("*"));
        let out_iface = rule.out_iface.unwrap_or("*");
        let _ = writeln!(
            out,
            "{idx}\t{}\t{}\t{}\t{}\t{}",
            table_name(rule.table),
            hook_name(rule.hook),
            target_name(rule.target),
            src,
            out_iface,
        );
    }

    out
}

pub fn proc_net_nf_conntrack_text() -> String {
    let mut out = String::new();

    for entry in netfilter_conntrack_snapshot() {
        let proto = conntrack_protocol_name(entry.protocol);
        let _ = writeln!(
            out,
            "{proto} original={} sport={} dst={} dport={} masquerade={} mport={}",
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
    }
}

fn conntrack_protocol_name(protocol: NetfilterConntrackProtocol) -> &'static str {
    match protocol {
        NetfilterConntrackProtocol::Icmp => "icmp",
        NetfilterConntrackProtocol::Tcp => "tcp",
        NetfilterConntrackProtocol::Udp => "udp",
    }
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
