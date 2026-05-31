//! sysfs — minimal projected filesystem for `/sys`.
//!
//! The current slice publishes the network class view used by Alpine/OpenRC
//! probes: `/sys/class/net/<ifname>/*`. It owns no network state; every read
//! derives from `NetNamespacePayload` link snapshots and iface runtime stats.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

pub mod adapter;

use adapter::step_engine::{Cap, NoProgress, StepOutcome};
use tx_substrate::zone::PayloadCap;
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::net::{EthernetAddress, NetNamespaceLinkInfo, NetNamespacePayload};
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::vfs::{
    Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, ProjectionKey,
    ProjectionSchemaId, RNode, RNodeBacking, S_IFDIR, S_IFREG,
};

pub const SYSFS_ROOT_ID: FsObjectId = FsObjectId::new(0x7379_7300);
const SYSFS_CLASS_ID: FsObjectId = FsObjectId::new(0x7379_7301);
const SYSFS_CLASS_NET_ID: FsObjectId = FsObjectId::new(0x7379_7302);

const SYSFS_NETDEV_ID_TAG: u64 = 0x7379_6E00_0000_0000;
const SYSFS_NETDEV_ID_MASK: u64 = 0xFFFF_FF00_0000_0000;
const SYSFS_NETDEV_NS_SHIFT: u64 = 32;
const SYSFS_NETDEV_IFINDEX_SHIFT: u64 = 16;
const SYSFS_NETDEV_KIND_MASK: u64 = 0xFFFF;

pub const SYSFS_DIR_MODE: u16 = S_IFDIR | 0o755;
pub const SYSFS_FILE_MODE: u16 = S_IFREG | 0o444;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NetdevNodeKind {
    DeviceDir = 0,
    Ifindex = 1,
    Address = 2,
    Broadcast = 3,
    Mtu = 4,
    Flags = 5,
    Operstate = 6,
    Carrier = 7,
    StatisticsDir = 8,
    RxPackets = 9,
    TxPackets = 10,
    RxBytes = 11,
    TxBytes = 12,
}

#[derive(Clone, Copy)]
struct NetdevEntry {
    name: &'static [u8],
    kind: InodeKind,
    node: NetdevNodeKind,
}

const NETDEV_FILES: &[NetdevEntry] = &[
    NetdevEntry {
        name: b"ifindex",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Ifindex,
    },
    NetdevEntry {
        name: b"address",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Address,
    },
    NetdevEntry {
        name: b"broadcast",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Broadcast,
    },
    NetdevEntry {
        name: b"mtu",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Mtu,
    },
    NetdevEntry {
        name: b"flags",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Flags,
    },
    NetdevEntry {
        name: b"operstate",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Operstate,
    },
    NetdevEntry {
        name: b"carrier",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::Carrier,
    },
    NetdevEntry {
        name: b"statistics",
        kind: InodeKind::Directory,
        node: NetdevNodeKind::StatisticsDir,
    },
];

const NETDEV_STAT_FILES: &[NetdevEntry] = &[
    NetdevEntry {
        name: b"rx_packets",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::RxPackets,
    },
    NetdevEntry {
        name: b"tx_packets",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::TxPackets,
    },
    NetdevEntry {
        name: b"rx_bytes",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::RxBytes,
    },
    NetdevEntry {
        name: b"tx_bytes",
        kind: InodeKind::Regular,
        node: NetdevNodeKind::TxBytes,
    },
];

#[derive(Clone, Copy, Debug, Default)]
pub struct Sysfs;

impl Sysfs {
    pub const fn new() -> Self {
        Self
    }

    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self::new())
    }

    pub fn fs_page_backing_arc() -> Arc<dyn FsPageBacking> {
        Arc::new(Self::new())
    }
}

impl FsOps for Sysfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if parent == SYSFS_ROOT_ID {
            return match name {
                b"class" => StepOutcome::done(SYSFS_CLASS_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == SYSFS_CLASS_ID {
            return match name {
                b"net" => StepOutcome::done(SYSFS_CLASS_NET_ID),
                _ => StepOutcome::err(Errno::ENOENT),
            };
        }
        if parent == SYSFS_CLASS_NET_ID {
            let Some((ns_slot, link)) = link_by_name_any(name) else {
                return StepOutcome::err(Errno::ENOENT);
            };
            return StepOutcome::done(netdev_node_id(
                ns_slot,
                link.ifindex,
                NetdevNodeKind::DeviceDir,
            ));
        }
        if let Some((ns_slot, ifindex, NetdevNodeKind::DeviceDir)) = parse_netdev_node_id(parent) {
            if link_by_node(ns_slot, ifindex).is_none() {
                return StepOutcome::err(Errno::ENOENT);
            }
            if let Some(entry) = NETDEV_FILES.iter().find(|entry| entry.name == name) {
                return StepOutcome::done(netdev_node_id(ns_slot, ifindex, entry.node));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        if let Some((ns_slot, ifindex, NetdevNodeKind::StatisticsDir)) =
            parse_netdev_node_id(parent)
        {
            if link_by_node(ns_slot, ifindex).is_none() {
                return StepOutcome::err(Errno::ENOENT);
            }
            if let Some(entry) = NETDEV_STAT_FILES.iter().find(|entry| entry.name == name) {
                return StepOutcome::done(netdev_node_id(ns_slot, ifindex, entry.node));
            }
            return StepOutcome::err(Errno::ENOENT);
        }
        StepOutcome::err(Errno::ENOENT)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        if matches!(
            fs_object_id,
            SYSFS_ROOT_ID | SYSFS_CLASS_ID | SYSFS_CLASS_NET_ID
        ) {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, SYSFS_DIR_MODE));
        }
        if let Some((ns_slot, ifindex, node)) = parse_netdev_node_id(fs_object_id) {
            if link_by_node(ns_slot, ifindex).is_none() {
                return StepOutcome::err(Errno::ENOENT);
            }
            let kind = if matches!(
                node,
                NetdevNodeKind::DeviceDir | NetdevNodeKind::StatisticsDir
            ) {
                InodeKind::Directory
            } else {
                InodeKind::Regular
            };
            let mode = if kind == InodeKind::Directory {
                SYSFS_DIR_MODE
            } else {
                SYSFS_FILE_MODE
            };
            return StepOutcome::done(InodeMeta::new(kind, mode));
        }
        StepOutcome::err(Errno::ENOENT)
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        let idx = cursor.as_u64() as usize;
        if fs_object_id == SYSFS_ROOT_ID {
            return emit_static_entry(idx, &[(b"class", SYSFS_CLASS_ID, InodeKind::Directory)]);
        }
        if fs_object_id == SYSFS_CLASS_ID {
            return emit_static_entry(idx, &[(b"net", SYSFS_CLASS_NET_ID, InodeKind::Directory)]);
        }
        if fs_object_id == SYSFS_CLASS_NET_ID {
            let links = all_netdev_links();
            let Some((ns_slot, link)) = links.get(idx).copied() else {
                return StepOutcome::done(None);
            };
            return match DirEntry::new(
                netdev_node_id(ns_slot, link.ifindex, NetdevNodeKind::DeviceDir),
                InodeKind::Directory,
                link.name.as_bytes(),
            ) {
                Ok(entry) => {
                    StepOutcome::done(Some((entry, DirCursor::from_u64((idx + 1) as u64))))
                }
                Err(errno) => StepOutcome::err(errno),
            };
        }
        if let Some((ns_slot, ifindex, NetdevNodeKind::DeviceDir)) =
            parse_netdev_node_id(fs_object_id)
        {
            if link_by_node(ns_slot, ifindex).is_none() {
                return StepOutcome::err(Errno::ENOENT);
            }
            return emit_netdev_entry(idx, ns_slot, ifindex, NETDEV_FILES);
        }
        if let Some((ns_slot, ifindex, NetdevNodeKind::StatisticsDir)) =
            parse_netdev_node_id(fs_object_id)
        {
            if link_by_node(ns_slot, ifindex).is_none() {
                return StepOutcome::err(Errno::ENOENT);
            }
            return emit_netdev_entry(idx, ns_slot, ifindex, NETDEV_STAT_FILES);
        }
        StepOutcome::err(Errno::ENOTDIR)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::done(())
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        let backing = match meta.kind() {
            InodeKind::Directory => RNodeBacking::Directory,
            InodeKind::Regular => RNodeBacking::Projected {
                schema: ProjectionSchemaId::Sysfs,
                key: ProjectionKey::from_fs_object_id(fs_object_id),
            },
            _ => return StepOutcome::err(Errno::ENOSYS),
        };
        match RNode::new_cap_in_mount(fs_object_id, meta, backing, mount) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::ENOMEM),
        }
    }

    fn step_read_projected(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        buf: &mut [u8],
        guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        self.step_read_projected_with_netns(fs_object_id, offset, buf, None, guard)
    }

    fn step_read_projected_with_netns(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        buf: &mut [u8],
        caller_netns: Option<&NetNamespacePayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        let default_netns;
        let netns = match caller_netns {
            Some(netns) => netns,
            None => {
                default_netns = initial_netns();
                &default_netns
            }
        };
        let content = match render_projected(fs_object_id, netns) {
            Ok(content) => content,
            Err(errno) => return StepOutcome::err(errno),
        };
        let bytes = content.as_bytes();
        let off = offset as usize;
        if off >= bytes.len() {
            return StepOutcome::done(0);
        }
        let available = &bytes[off..];
        let len = available.len().min(buf.len());
        buf[..len].copy_from_slice(&available[..len]);
        StepOutcome::done(len as u64)
    }

    fn step_write_projected(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<u64, NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn step_chmod(
        &self,
        _fs_object_id: FsObjectId,
        _new_mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }

    fn step_chown(
        &self,
        _fs_object_id: FsObjectId,
        _new_uid: Option<u32>,
        _new_gid: Option<u32>,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS)
    }
}

impl FsPageBacking for Sysfs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS)
    }
}

fn emit_static_entry(
    idx: usize,
    entries: &[(&'static [u8], FsObjectId, InodeKind)],
) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
    let Some((name, object_id, kind)) = entries.get(idx).copied() else {
        return StepOutcome::done(None);
    };
    match DirEntry::new(object_id, kind, name) {
        Ok(entry) => StepOutcome::done(Some((entry, DirCursor::from_u64((idx + 1) as u64)))),
        Err(errno) => StepOutcome::err(errno),
    }
}

fn emit_netdev_entry(
    idx: usize,
    ns_slot: u16,
    ifindex: u32,
    entries: &[NetdevEntry],
) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
    let Some(entry) = entries.get(idx).copied() else {
        return StepOutcome::done(None);
    };
    match DirEntry::new(
        netdev_node_id(ns_slot, ifindex, entry.node),
        entry.kind,
        entry.name,
    ) {
        Ok(dir_entry) => {
            StepOutcome::done(Some((dir_entry, DirCursor::from_u64((idx + 1) as u64))))
        }
        Err(errno) => StepOutcome::err(errno),
    }
}

fn render_projected(
    fs_object_id: FsObjectId,
    _netns: &NetNamespacePayload,
) -> Result<String, Errno> {
    let Some((ns_slot, ifindex, node)) = parse_netdev_node_id(fs_object_id) else {
        return Err(Errno::ENOENT);
    };
    let Some((netns, link)) = link_by_node(ns_slot, ifindex) else {
        return Err(Errno::ENOENT);
    };
    if matches!(
        node,
        NetdevNodeKind::DeviceDir | NetdevNodeKind::StatisticsDir
    ) {
        return Err(Errno::EISDIR);
    }
    Ok(match node {
        NetdevNodeKind::Ifindex => format!("{}\n", link.ifindex),
        NetdevNodeKind::Address => {
            let mac = link
                .mac
                .map(format_mac)
                .unwrap_or_else(|| String::from("00:00:00:00:00:00"));
            format!("{mac}\n")
        }
        NetdevNodeKind::Broadcast => {
            if link.is_loopback {
                String::from("00:00:00:00:00:00\n")
            } else {
                String::from("ff:ff:ff:ff:ff:ff\n")
            }
        }
        NetdevNodeKind::Mtu => format!("{}\n", link.mtu),
        NetdevNodeKind::Flags => format!("0x{:x}\n", netdev_flags(link)),
        NetdevNodeKind::Operstate => format!("{}\n", operstate(link)),
        NetdevNodeKind::Carrier => {
            if link.is_loopback || link.is_up {
                String::from("1\n")
            } else {
                String::from("0\n")
            }
        }
        NetdevNodeKind::RxPackets => {
            format!("{}\n", iface_stat(&netns, link.name, StatKind::RxPackets))
        }
        NetdevNodeKind::TxPackets => {
            format!("{}\n", iface_stat(&netns, link.name, StatKind::TxPackets))
        }
        NetdevNodeKind::RxBytes => {
            format!("{}\n", iface_stat(&netns, link.name, StatKind::RxBytes))
        }
        NetdevNodeKind::TxBytes => {
            format!("{}\n", iface_stat(&netns, link.name, StatKind::TxBytes))
        }
        NetdevNodeKind::DeviceDir | NetdevNodeKind::StatisticsDir => unreachable!(),
    })
}

#[derive(Clone, Copy)]
enum StatKind {
    RxPackets,
    TxPackets,
    RxBytes,
    TxBytes,
}

fn iface_stat(netns: &NetNamespacePayload, name: &'static str, kind: StatKind) -> u64 {
    let ifaces = netns.ether_ifaces_snapshot();
    let Some(stats) = ifaces
        .into_iter()
        .find(|iface| iface.name == name)
        .map(|iface| iface.net_stats_snapshot())
    else {
        return 0;
    };
    match kind {
        StatKind::RxPackets => stats.rx_packets,
        StatKind::TxPackets => stats.tx_packets,
        StatKind::RxBytes => stats.rx_bytes,
        StatKind::TxBytes => stats.tx_bytes,
    }
}

fn netdev_flags(link: NetNamespaceLinkInfo) -> u32 {
    const IFF_UP: u32 = 0x1;
    const IFF_BROADCAST: u32 = 0x2;
    const IFF_LOOPBACK: u32 = 0x8;
    const IFF_RUNNING: u32 = 0x40;
    const IFF_MULTICAST: u32 = 0x1000;

    if link.is_loopback {
        return IFF_UP | IFF_LOOPBACK | IFF_RUNNING;
    }

    let mut flags = IFF_BROADCAST | IFF_MULTICAST;
    if link.is_up {
        flags |= IFF_UP | IFF_RUNNING;
    }
    flags
}

fn operstate(link: NetNamespaceLinkInfo) -> &'static str {
    if link.is_loopback {
        "unknown"
    } else if link.is_up {
        "up"
    } else {
        "down"
    }
}

fn format_mac(addr: EthernetAddress) -> String {
    let [a, b, c, d, e, f] = addr.octets();
    let mut out = String::new();
    let _ = write!(out, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}");
    out
}

fn initial_netns() -> tx_substrate::zone::PayloadCap<NetNamespacePayload> {
    tx_subsystems::net::initial_net_namespace_payload()
}

fn netns_snapshot() -> Vec<PayloadCap<NetNamespacePayload>> {
    let _ = initial_netns();
    tx_subsystems::net::net_namespace_payloads_snapshot()
}

fn all_netdev_links() -> Vec<(u16, NetNamespaceLinkInfo)> {
    let mut links = Vec::new();
    for (idx, netns) in netns_snapshot().into_iter().enumerate() {
        let Ok(ns_slot) = u8::try_from(idx) else {
            continue;
        };
        let ns_slot = u16::from(ns_slot);
        links.extend(
            netns
                .link_snapshot()
                .into_iter()
                .map(|link| (ns_slot, link)),
        );
    }
    links
}

fn link_by_name_any(name: &[u8]) -> Option<(u16, NetNamespaceLinkInfo)> {
    all_netdev_links()
        .into_iter()
        .find(|(_, link)| link.name.as_bytes() == name)
}

fn link_by_node(
    ns_slot: u16,
    ifindex: u32,
) -> Option<(PayloadCap<NetNamespacePayload>, NetNamespaceLinkInfo)> {
    let netns = netns_snapshot().into_iter().nth(usize::from(ns_slot))?;
    let link = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == ifindex)?;
    Some((netns, link))
}

fn netdev_node_id(ns_slot: u16, ifindex: u32, kind: NetdevNodeKind) -> FsObjectId {
    FsObjectId::new(
        SYSFS_NETDEV_ID_TAG
            | ((ns_slot as u64) << SYSFS_NETDEV_NS_SHIFT)
            | ((ifindex as u64) << SYSFS_NETDEV_IFINDEX_SHIFT)
            | (kind as u64),
    )
}

fn parse_netdev_node_id(id: FsObjectId) -> Option<(u16, u32, NetdevNodeKind)> {
    let raw = id.as_u64();
    if raw & SYSFS_NETDEV_ID_MASK != SYSFS_NETDEV_ID_TAG {
        return None;
    }
    let ns_slot = ((raw >> SYSFS_NETDEV_NS_SHIFT) & 0xFF) as u16;
    let ifindex = ((raw >> SYSFS_NETDEV_IFINDEX_SHIFT) & 0xFFFF) as u32;
    if ifindex == 0 {
        return None;
    }
    let kind = match raw & SYSFS_NETDEV_KIND_MASK {
        0 => NetdevNodeKind::DeviceDir,
        1 => NetdevNodeKind::Ifindex,
        2 => NetdevNodeKind::Address,
        3 => NetdevNodeKind::Broadcast,
        4 => NetdevNodeKind::Mtu,
        5 => NetdevNodeKind::Flags,
        6 => NetdevNodeKind::Operstate,
        7 => NetdevNodeKind::Carrier,
        8 => NetdevNodeKind::StatisticsDir,
        9 => NetdevNodeKind::RxPackets,
        10 => NetdevNodeKind::TxPackets,
        11 => NetdevNodeKind::RxBytes,
        12 => NetdevNodeKind::TxBytes,
        _ => return None,
    };
    Some((ns_slot, ifindex, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_subsystems::device::DevT;
    use tx_subsystems::net::{
        create_veth_pair_for_test_or_bootstrap, EthernetAddress, NetAdminAuthority,
        VethEndpointConfig, VethPairConfig, VETH_DEFAULT_MTU,
    };

    fn init_sysfs_test() {
        tx_test_support::init_host();
        tx_subsystems::zones::register_all().expect("tx-subsystems zones");
        tx_subsystems::net::device::reset_net_registry_for_test();
        tx_subsystems::net::reset_initial_net_namespace_for_test();
    }

    #[test]
    fn sysfs_class_net_projects_loopback_attributes() {
        let _lock = crate::test_support::FS_TEST_LOCK
            .lock()
            .expect("fs test lock");
        init_sysfs_test();
        let guard = tx_substrate::epoch::guard();
        let sysfs = Sysfs::new();

        let class = match <Sysfs as FsOps>::lookup(&sysfs, SYSFS_ROOT_ID, b"class", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup /sys/class failed: {other:?}"),
        };
        let net = match <Sysfs as FsOps>::lookup(&sysfs, class, b"net", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup /sys/class/net failed: {other:?}"),
        };
        let lo = match <Sysfs as FsOps>::lookup(&sysfs, net, b"lo", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup /sys/class/net/lo failed: {other:?}"),
        };
        let mtu = match <Sysfs as FsOps>::lookup(&sysfs, lo, b"mtu", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup lo/mtu failed: {other:?}"),
        };

        let mut out = [0u8; 32];
        let read = match sysfs.step_read_projected(mtu, 0, &mut out, &guard) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("read lo/mtu failed: {other:?}"),
        };
        assert_eq!(&out[..read], b"65535\n");
    }

    #[test]
    fn sysfs_class_net_projects_attached_veth_address() {
        let _lock = crate::test_support::FS_TEST_LOCK
            .lock()
            .expect("fs test lock");
        init_sysfs_test();
        let guard = tx_substrate::epoch::guard();
        let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "sys-veth0",
                devt: DevT::new(101, 1),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 3, 1]),
            },
            right: VethEndpointConfig {
                name: "sys-peer0",
                devt: DevT::new(101, 2),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 3, 2]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let netns = tx_subsystems::net::initial_net_namespace_payload();
        netns
            .attach_device(NetAdminAuthority::for_test_or_bootstrap(), pair.left, None)
            .expect("attach sys-veth0");

        let sysfs = Sysfs::new();
        let veth = match <Sysfs as FsOps>::lookup(&sysfs, SYSFS_CLASS_NET_ID, b"sys-veth0", &guard)
        {
            StepOutcome::Done(id) => id,
            other => panic!("lookup sys-veth0 failed: {other:?}"),
        };
        let address = match <Sysfs as FsOps>::lookup(&sysfs, veth, b"address", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup sys-veth0/address failed: {other:?}"),
        };

        let mut out = [0u8; 64];
        let read = match sysfs.step_read_projected(address, 0, &mut out, &guard) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("read sys-veth0/address failed: {other:?}"),
        };
        assert_eq!(&out[..read], b"02:00:00:72:03:01\n");
    }

    #[test]
    fn sysfs_class_net_projects_isolated_namespace_veth_address() {
        let _lock = crate::test_support::FS_TEST_LOCK
            .lock()
            .expect("fs test lock");
        init_sysfs_test();
        let guard = tx_substrate::epoch::guard();
        let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
            left: VethEndpointConfig {
                name: "sys-host0",
                devt: DevT::new(101, 3),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 3, 3]),
            },
            right: VethEndpointConfig {
                name: "sys-remote0",
                devt: DevT::new(101, 4),
                mac: EthernetAddress::new([0x02, 0, 0, 0x72, 3, 4]),
            },
            mtu: VETH_DEFAULT_MTU,
        });
        let initial = tx_subsystems::net::initial_net_namespace_payload();
        initial
            .attach_device(NetAdminAuthority::for_test_or_bootstrap(), pair.left, None)
            .expect("attach sys-host0");
        let remote = tx_subsystems::net::create_isolated_net_namespace_for_test("sysfs-remote")
            .expect("remote netns")
            .payload_cap()
            .expect("remote payload");
        remote
            .attach_device(NetAdminAuthority::for_test_or_bootstrap(), pair.right, None)
            .expect("attach sys-remote0");

        let sysfs = Sysfs::new();
        let veth =
            match <Sysfs as FsOps>::lookup(&sysfs, SYSFS_CLASS_NET_ID, b"sys-remote0", &guard) {
                StepOutcome::Done(id) => id,
                other => panic!("lookup sys-remote0 failed: {other:?}"),
            };
        let address = match <Sysfs as FsOps>::lookup(&sysfs, veth, b"address", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup sys-remote0/address failed: {other:?}"),
        };

        let mut out = [0u8; 64];
        let read = match sysfs.step_read_projected(address, 0, &mut out, &guard) {
            StepOutcome::Done(read) => read as usize,
            other => panic!("read sys-remote0/address failed: {other:?}"),
        };
        assert_eq!(&out[..read], b"02:00:00:72:03:04\n");
    }
}
