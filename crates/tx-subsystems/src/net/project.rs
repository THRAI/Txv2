//! Read-only network projection helpers.

use alloc::string::String;
use core::fmt::Write;

use smoltcp::time::Instant;

use crate::net::device::EthernetAddress;
use crate::net::protocol::{ArpSnapshotState, EtherIface};
use crate::net::structure::Ipv4Address;

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

fn format_mac(addr: EthernetAddress) -> String {
    let [a, b, c, d, e, f] = addr.octets();
    let mut out = String::new();
    let _ = write!(out, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}");
    out
}
