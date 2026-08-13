use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetFrame, EthernetProtocol, Ipv4Packet};
use tx_substrate::zone::{
    self, register_zone_for, Cap, Dead, Entity, PayloadCap, PayloadPolicy, Zone, ZoneAllocated,
    ZoneError,
};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::admin::NetAdminAuthority;
use crate::net::device::{
    net_device_registry_len, net_device_snapshot, BridgeForwardOutcome, BridgeSnapshot,
    EthernetAddress, NetDeviceKind, NetDeviceRegistration,
};
use crate::net::execution::{
    step_flush_pending_arp, step_packet_ingress_fanout,
    step_process_device_tx_pending_in_namespace_at, step_process_network_events_in_namespace_at,
    ArpFlushOutcome, DeviceTxBudget, DeviceTxOutcome,
};
use crate::net::netfilter::{
    apply_postrouting_nat_ipv4_in_namespace, apply_prerouting_nat_ipv4_in_namespace,
    cleanup_netfilter_device_state_in_namespace_for_test_or_bootstrap, run_frame_hook_in_namespace,
    NetfilterFrameContext, NetfilterHook, NetfilterState, NetfilterVerdict,
};
use crate::net::packet::{PacketDispatch, PacketSource, PacketTxResult};
use crate::net::protocol::{
    loopback_iface, EtherIface, EtherPacketTxSink, IfaceCommon, LoopbackIface,
};
use crate::net::structure::{Ipv4Address, Ipv6Address, SocketTable};
use crate::process::nsproxy::UserNamespace;
use crate::sync::SpinMutex;
use crate::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking, StructPayload,
};
use crate::vfs::OpenFile;

static NET_NAMESPACE_IDENTITY_ZONE: Zone<NetNamespaceIdentity> = Zone::const_new();
static NET_NAMESPACE_PAYLOAD_ZONE: Zone<NetNamespacePayload> = Zone::const_new();
static INITIAL_NET_NAMESPACE: SpinMutex<Option<Cap<NetNamespaceIdentity>>> = SpinMutex::new(None);
// The staging socket indexes are larger than one Zone slab object.
// Keep the initial namespace's concrete table static and bind it
// through the namespace payload until dynamic per-netns table storage
// lands.
static INITIAL_SOCKET_TABLE: SocketTable = SocketTable::new();
static NET_NAMESPACE_RUNTIME_LIST: SpinMutex<Vec<PayloadCap<NetNamespacePayload>>> =
    SpinMutex::new(Vec::new());
const NETNS_FS_OBJECT_ID_BASE: u64 = 0xFFFD_0000_0000_0000;
static NEXT_NETNS_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(NETNS_FS_OBJECT_ID_BASE);
const PENDING_IPV4_FORWARD_LIMIT: usize = 32;
const PENDING_IPV4_FORWARD_RETRY_LIMIT: u8 = 8;

pub struct NetNamespaceIdentity {
    name: &'static str,
    payload: SpinMutex<Option<PayloadCap<NetNamespacePayload>>>,
}

pub struct NetNamespacePayload {
    owner_user_ns: SpinMutex<Option<Cap<UserNamespace>>>,
    socket_table: &'static SocketTable,
    /// True when `socket_table` is a per-namespace heap allocation (from
    /// `alloc_zeroed`) owned by this payload and freed in `Drop`; false when it
    /// is the shared `INITIAL_SOCKET_TABLE` static (never freed).
    socket_table_owned: bool,
    loopback_iface: &'static LoopbackIface,
    loopback_tcp_connected_cursor: AtomicU64,
    loopback_mtu: SpinMutex<u16>,
    loopback_ipv4_override: SpinMutex<Option<(Ipv4Address, u8)>>,
    loopback_ipv6_override: SpinMutex<Option<(Ipv6Address, u8)>>,
    host_devices_visible: bool,
    namespace_devices: SpinMutex<Vec<NetNamespaceDeviceLink>>,
    routes: SpinMutex<Vec<NetNamespaceRouteEntry>>,
    suppressed_connected_routes: SpinMutex<Vec<NetNamespaceConnectedRouteKey>>,
    routes6: SpinMutex<Vec<NetNamespaceRoute6Entry>>,
    suppressed_connected_routes6: SpinMutex<Vec<NetNamespaceConnectedRoute6Key>>,
    iface_runtime: SpinMutex<Vec<NetNamespaceIfaceRuntime>>,
    netfilter: SpinMutex<NetfilterState>,
    ipv4_forwarding: AtomicBool,
    ipv6_disabled: AtomicBool,
    ipv6_accept_dad: AtomicBool,
    pending_ipv4_forwards: SpinMutex<Vec<PendingIpv4Forward>>,
    ipv4_extra_addrs: SpinMutex<Vec<ExtraIpv4Addr>>,
    ipv6_extra_addrs: SpinMutex<Vec<ExtraIpv6Addr>>,
    link_snapshot_generation: AtomicU64,
    link_snapshot_cache: SpinMutex<Option<LinkSnapshotCache>>,
}

/// A secondary IPv4 address on a link (`ip addr add` beyond the primary, or a
/// labeled `ifconfig eth0:1` alias). The primary stays in
/// [`NetNamespaceDeviceLink::ipv4_addr`]; extras only extend local-address
/// ownership, the RTM_GETADDR dump, and labeled SIOC ioctl lookups — the
/// dataplane iface keeps running on the primary.
#[derive(Clone)]
struct ExtraIpv4Addr {
    devt: DevT,
    addr: Ipv4Address,
    prefix_len: u8,
    label: Option<alloc::string::String>,
}

/// A secondary IPv6 address on a link (`ip -6 addr add` beyond the primary).
#[derive(Clone)]
struct ExtraIpv6Addr {
    devt: DevT,
    addr: Ipv6Address,
    prefix_len: u8,
}

/// Public projection of one extra (secondary) IPv4 address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetNamespaceExtraIpv4Info {
    pub ifindex: u32,
    pub addr: Ipv4Address,
    pub prefix_len: u8,
    pub label: Option<alloc::string::String>,
}

/// Public projection of one extra (secondary) IPv6 address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetNamespaceExtraIpv6Info {
    pub ifindex: u32,
    pub addr: Ipv6Address,
    pub prefix_len: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceLinkInfo {
    pub ifindex: u32,
    pub name: &'static str,
    pub kind: NetDeviceKind,
    pub mtu: u16,
    pub mac: Option<EthernetAddress>,
    pub ipv4_addr: Option<Ipv4Address>,
    pub ipv4_prefix_len: Option<u8>,
    pub ipv6_addr: Option<Ipv6Address>,
    pub ipv6_prefix_len: Option<u8>,
    pub master: Option<&'static str>,
    pub is_loopback: bool,
    pub is_up: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetNamespaceBridgeInfo {
    pub name: &'static str,
    pub ports: Vec<&'static str>,
    pub learned_entries: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetNamespaceSnapshot {
    pub links: Vec<NetNamespaceLinkInfo>,
    pub bridges: Vec<NetNamespaceBridgeInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetNamespaceRouteKind {
    Connected,
    Static,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRouteInfo {
    pub kind: NetNamespaceRouteKind,
    pub dst: Ipv4Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv4Address>,
    pub oif_name: Option<&'static str>,
    pub preferred_src: Option<Ipv4Address>,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRouteConfig {
    pub dst: Ipv4Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv4Address>,
    pub oif_name: Option<&'static str>,
    pub preferred_src: Option<Ipv4Address>,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRouteSelector {
    pub dst: Ipv4Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv4Address>,
    pub oif_name: Option<&'static str>,
    pub table: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRouteDecision {
    pub oif_name: &'static str,
    pub next_hop: Ipv4Address,
    pub preferred_src: Option<Ipv4Address>,
    pub prefix_len: u8,
    pub kind: NetNamespaceRouteKind,
}

#[derive(Clone, Copy)]
struct NetNamespaceDeviceLink {
    registration: &'static NetDeviceRegistration,
    ipv4_addr: Option<Ipv4Address>,
    ipv4_prefix_len: Option<u8>,
    ipv6_addr: Option<Ipv6Address>,
    ipv6_prefix_len: Option<u8>,
    mtu: Option<u16>,
    is_up: bool,
}

#[derive(Clone, Copy)]
struct NetNamespaceIfaceRuntime {
    registration: &'static NetDeviceRegistration,
    ipv4_addr: Ipv4Address,
    ipv4_prefix_len: u8,
    gateway: Option<Ipv4Address>,
    ipv6_addr: Option<Ipv6Address>,
    ipv6_prefix_len: Option<u8>,
    ipv6_gateway: Option<Ipv6Address>,
    iface: &'static EtherIface,
}

#[derive(Clone, Copy)]
struct NetNamespaceRouteEntry {
    dst: Ipv4Address,
    prefix_len: u8,
    gateway: Option<Ipv4Address>,
    oif_name: Option<&'static str>,
    preferred_src: Option<Ipv4Address>,
    table: u8,
    protocol: u8,
    scope: u8,
    route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NetNamespaceConnectedRouteKey {
    dst: Ipv4Address,
    prefix_len: u8,
    oif_name: &'static str,
    table: u8,
}

// ---------------------------------------------------------------------------
// IPv6 routing / FIB (management plane) — a parallel, independent table that
// mirrors the IPv4 route types above field-for-field with `Ipv6Address`. The
// `NetNamespaceRouteKind` enum is family-agnostic and shared. None of this
// touches the IPv4 route path or the data path; it only stores v6 routes for
// rtnetlink (`ip -6 route`) and `/proc/net/ipv6_route`.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRoute6Info {
    pub kind: NetNamespaceRouteKind,
    pub dst: Ipv6Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv6Address>,
    pub oif_name: Option<&'static str>,
    pub preferred_src: Option<Ipv6Address>,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRoute6Config {
    pub dst: Ipv6Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv6Address>,
    pub oif_name: Option<&'static str>,
    pub preferred_src: Option<Ipv6Address>,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRoute6Selector {
    pub dst: Ipv6Address,
    pub prefix_len: u8,
    pub gateway: Option<Ipv6Address>,
    pub oif_name: Option<&'static str>,
    pub table: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetNamespaceRoute6Decision {
    pub oif_name: &'static str,
    pub next_hop: Ipv6Address,
    pub preferred_src: Option<Ipv6Address>,
    pub prefix_len: u8,
    pub kind: NetNamespaceRouteKind,
}

#[derive(Clone, Copy)]
struct NetNamespaceRoute6Entry {
    dst: Ipv6Address,
    prefix_len: u8,
    gateway: Option<Ipv6Address>,
    oif_name: Option<&'static str>,
    preferred_src: Option<Ipv6Address>,
    table: u8,
    protocol: u8,
    scope: u8,
    route_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NetNamespaceConnectedRoute6Key {
    dst: Ipv6Address,
    prefix_len: u8,
    oif_name: &'static str,
    table: u8,
}

struct PendingIpv4Forward {
    egress_name: &'static str,
    packet: Vec<u8>,
    attempts: u8,
}

#[derive(Clone)]
struct LinkSnapshotCache {
    generation: u64,
    host_device_count: usize,
    links: Vec<NetNamespaceLinkInfo>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetNamespaceForwardOutcome {
    pub forwarded: usize,
    pub pending_resolution: usize,
    pub dropped: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetNamespaceRuntimeOutcome {
    pub namespaces_seen: usize,
    pub ifaces_seen: usize,
    pub bridge_polls: usize,
    pub bridge_frames_seen: usize,
    pub bridge_forwarded: usize,
    pub bridge_local_delivered: usize,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub wakes_fired: usize,
    pub device_tx_attempted: usize,
    pub device_tx_packets: usize,
    pub device_tx_pending_resolution: usize,
    pub device_tx_failed: usize,
    pub arp_sent: usize,
    pub ipv4_forwarded: usize,
    pub ipv4_forward_pending_resolution: usize,
    pub ipv4_forward_dropped: usize,
}

// SAFETY: `NET_NAMESPACE_IDENTITY_ZONE` is the single process-wide
// zone for `NetNamespaceIdentity`; namespace identity handles resolve
// through it.
unsafe impl ZoneAllocated for NetNamespaceIdentity {
    fn zone() -> &'static Zone<Self> {
        &NET_NAMESPACE_IDENTITY_ZONE
    }
}

// SAFETY: `NET_NAMESPACE_PAYLOAD_ZONE` is the single process-wide
// zone for `NetNamespacePayload`; operational namespace state is
// retained through payload caps.
unsafe impl ZoneAllocated for NetNamespacePayload {
    type Policy = PayloadPolicy<Self>;

    fn zone() -> &'static Zone<Self> {
        &NET_NAMESPACE_PAYLOAD_ZONE
    }
}

impl Drop for NetNamespacePayload {
    fn drop(&mut self) {
        // Free the per-namespace `SocketTable` heap allocation when this payload
        // is reclaimed. The zone slot is only reclaimed (via EBR) after the last
        // strong payload cap is dropped — see `prune_dead_net_namespaces`, which
        // drops the runtime list's reference once no identity/socket holds the
        // namespace — so no live `socket_table()` reference exists here.
        if self.socket_table_owned {
            let ptr = self.socket_table as *const SocketTable as *mut SocketTable;
            // SAFETY: `socket_table_owned` => the table was heap-allocated by
            // `create_isolated_net_namespace_with_owner` via `alloc_zeroed` with
            // `Layout::new::<SocketTable>()` and is uniquely owned by this
            // payload. `SocketTable` bundles `Index` maps whose keys and values
            // (`Cap<SocketIdentity>`) live in `MaybeUninit`, and `Index` *does*
            // have a real `Drop` (see `index.rs`) that `assume_init_drop`s the
            // key/value of every committed entry — so `drop_in_place` is not a
            // no-op; it recursively runs those `Index::drop`s. Soundness rests on
            // the table being empty by the time the payload is reclaimed (see the
            // prose above: the namespace is pruned only once no identity/socket
            // holds it, so all entries have already been removed), which leaves
            // every `Index::drop` on its empty branch. `ptr` is uniquely owned, so
            // each drop runs at most once and the following `dealloc` frees the
            // zeroed backing without re-running `Drop` — no double-free.
            unsafe {
                core::ptr::drop_in_place(ptr);
                alloc::alloc::dealloc(ptr as *mut u8, core::alloc::Layout::new::<SocketTable>());
            }
        }
    }
}

impl NetNamespaceIdentity {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            payload: SpinMutex::new(None),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn install_payload(&self, payload: PayloadCap<NetNamespacePayload>) {
        *self.payload.lock() = Some(payload);
    }

    pub fn payload_cap(&self) -> Option<PayloadCap<NetNamespacePayload>> {
        self.payload.lock().clone()
    }
}

impl Entity for NetNamespaceIdentity {
    type OperationalEvidence = PayloadCap<NetNamespacePayload>;

    fn upgrade_operational(identity: &Cap<Self>) -> Result<Self::OperationalEvidence, Dead> {
        identity.payload_cap().ok_or(Dead)
    }
}

impl NetNamespacePayload {
    pub fn new_initial() -> Self {
        Self::new(
            None,
            initial_socket_table_for_namespace(),
            false,
            loopback_iface(),
            true,
        )
    }

    fn new(
        owner_user_ns: Option<Cap<UserNamespace>>,
        socket_table: &'static SocketTable,
        socket_table_owned: bool,
        loopback_iface: &'static LoopbackIface,
        host_devices_visible: bool,
    ) -> Self {
        Self {
            owner_user_ns: SpinMutex::new(owner_user_ns),
            socket_table,
            socket_table_owned,
            loopback_iface,
            loopback_tcp_connected_cursor: AtomicU64::new(0),
            loopback_mtu: SpinMutex::new(loopback_iface.mtu()),
            loopback_ipv4_override: SpinMutex::new(None),
            loopback_ipv6_override: SpinMutex::new(None),
            host_devices_visible,
            namespace_devices: SpinMutex::new(Vec::new()),
            routes: SpinMutex::new(Vec::new()),
            suppressed_connected_routes: SpinMutex::new(Vec::new()),
            routes6: SpinMutex::new(Vec::new()),
            suppressed_connected_routes6: SpinMutex::new(Vec::new()),
            iface_runtime: SpinMutex::new(Vec::new()),
            netfilter: SpinMutex::new(NetfilterState::new()),
            ipv4_forwarding: AtomicBool::new(false),
            ipv6_disabled: AtomicBool::new(false),
            ipv6_accept_dad: AtomicBool::new(false),
            pending_ipv4_forwards: SpinMutex::new(Vec::new()),
            ipv4_extra_addrs: SpinMutex::new(Vec::new()),
            ipv6_extra_addrs: SpinMutex::new(Vec::new()),
            link_snapshot_generation: AtomicU64::new(0),
            link_snapshot_cache: SpinMutex::new(None),
        }
    }

    pub fn socket_table(&self) -> &'static SocketTable {
        self.socket_table
    }

    pub fn owner_user_namespace(&self) -> Option<Cap<UserNamespace>> {
        self.owner_user_ns.lock().clone()
    }

    pub fn install_owner_user_namespace_once(&self, owner: Cap<UserNamespace>) {
        let mut slot = self.owner_user_ns.lock();
        if slot.is_none() {
            *slot = Some(owner);
        }
    }

    pub fn loopback_iface(&self) -> &'static LoopbackIface {
        self.loopback_iface
    }

    pub(crate) fn reserve_loopback_tcp_connected_window(&self, visits: usize) -> u64 {
        self.loopback_tcp_connected_cursor
            .fetch_add(visits as u64, Ordering::Relaxed)
    }

    pub(crate) fn netfilter_state(&self) -> &SpinMutex<NetfilterState> {
        &self.netfilter
    }

    pub(crate) fn invalidate_link_snapshot_cache(&self) {
        self.link_snapshot_generation.fetch_add(1, Ordering::AcqRel);
        *self.link_snapshot_cache.lock() = None;
    }

    pub(crate) fn link_snapshot_generation(&self) -> u64 {
        self.link_snapshot_generation.load(Ordering::Acquire)
    }

    pub fn attach_device(
        &self,
        _authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
        ipv4_addr: Option<Ipv4Address>,
    ) -> Result<(), Errno> {
        self.attach_device_inner(registration, ipv4_addr)
    }

    pub fn attach_device_for_test_or_bootstrap(
        &self,
        registration: &'static NetDeviceRegistration,
        ipv4_addr: Option<Ipv4Address>,
    ) -> Result<(), Errno> {
        self.attach_device_inner(registration, ipv4_addr)
    }

    pub fn move_device_to_namespace_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
        target: &NetNamespacePayload,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            return Err(Errno::EOPNOTSUPP);
        }
        if core::ptr::eq(self, target) {
            return Ok(());
        }

        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        if target.has_device_conflict(registration) {
            return Err(Errno::EEXIST);
        }

        let link = {
            let mut devices = self.namespace_devices.lock();
            let Some(idx) = devices
                .iter()
                .position(|link| link.registration.devt == registration.devt)
            else {
                return Err(Errno::EOPNOTSUPP);
            };
            devices.remove(idx)
        };

        self.remove_device_from_local_bridges(authority, registration);
        self.invalidate_link_snapshot_cache();
        target.attach_device_link_inner(link)
    }

    pub fn detach_device_from_bridges_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            return Err(Errno::EOPNOTSUPP);
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        self.remove_device_from_local_bridges(authority, registration);
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    pub fn delete_device_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            return Err(Errno::EOPNOTSUPP);
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let removed = {
            let mut devices = self.namespace_devices.lock();
            let Some(idx) = devices
                .iter()
                .position(|link| link.registration.devt == registration.devt)
            else {
                return Err(Errno::EOPNOTSUPP);
            };
            devices.remove(idx)
        };
        self.remove_device_from_local_bridges(authority, registration);
        self.invalidate_link_snapshot_cache();
        self.routes
            .lock()
            .retain(|route| route.oif_name != Some(registration.name));
        self.forget_connected_route_suppressions_for_oif(registration.name);
        self.iface_runtime
            .lock()
            .retain(|entry| entry.registration.devt != registration.devt);
        cleanup_netfilter_device_state_in_namespace_for_test_or_bootstrap(
            self,
            registration.name,
            removed.ipv4_addr,
        );
        Ok(())
    }

    fn attach_device_inner(
        &self,
        registration: &'static NetDeviceRegistration,
        ipv4_addr: Option<Ipv4Address>,
    ) -> Result<(), Errno> {
        if self.has_device_conflict(registration) {
            return Err(Errno::EEXIST);
        }

        self.attach_device_link_inner(NetNamespaceDeviceLink {
            registration,
            ipv4_addr,
            ipv4_prefix_len: ipv4_addr.map(|_| 32),
            ipv6_addr: None,
            ipv6_prefix_len: None,
            mtu: None,
            is_up: true,
        })
    }

    fn attach_device_link_inner(&self, link: NetNamespaceDeviceLink) -> Result<(), Errno> {
        if self.has_device_conflict(link.registration) {
            return Err(Errno::EEXIST);
        }
        self.namespace_devices.lock().push(link);
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    fn has_device_conflict(&self, registration: &'static NetDeviceRegistration) -> bool {
        if self.host_devices_visible
            && net_device_snapshot()
                .into_iter()
                .any(|reg| reg.name == registration.name || reg.devt == registration.devt)
        {
            return true;
        }

        self.namespace_devices.lock().iter().any(|link| {
            link.registration.name == registration.name
                || link.registration.devt == registration.devt
        })
    }

    pub fn link_snapshot(&self) -> Vec<NetNamespaceLinkInfo> {
        let generation = self.link_snapshot_generation.load(Ordering::Acquire);
        let host_device_count = if self.host_devices_visible {
            net_device_registry_len()
        } else {
            0
        };
        {
            let cache = self.link_snapshot_cache.lock();
            if let Some(cache) = cache.as_ref() {
                if cache.generation == generation && cache.host_device_count == host_device_count {
                    return cache.links.clone();
                }
            }
        }

        let mut links = Vec::new();
        let devices = self.device_snapshot();
        let bridges = bridge_snapshot_from_devices(&devices);
        let loopback_override = *self.loopback_ipv4_override.lock();
        let (loopback_addr, loopback_prefix_len) =
            loopback_override.unwrap_or((self.loopback_iface.local_ipv4(), 8));
        let loopback_ipv6_override = *self.loopback_ipv6_override.lock();
        let (loopback_ipv6_addr, loopback_ipv6_prefix_len) =
            loopback_ipv6_override.unwrap_or((Ipv6Address::LOOPBACK, 128));
        links.push(NetNamespaceLinkInfo {
            ifindex: 1,
            name: "lo",
            kind: NetDeviceKind::Loopback,
            mtu: *self.loopback_mtu.lock(),
            mac: None,
            ipv4_addr: Some(loopback_addr),
            ipv4_prefix_len: Some(loopback_prefix_len),
            ipv6_addr: Some(loopback_ipv6_addr),
            ipv6_prefix_len: Some(loopback_ipv6_prefix_len),
            master: None,
            is_loopback: true,
            is_up: true,
        });

        for (next_ifindex, reg) in (2..).zip(devices) {
            let ipv4_addr = self.ipv4_for_device(reg);
            let ipv4_prefix_len = self.ipv4_prefix_len_for_device(reg);
            let ipv6_addr = self.ipv6_for_device(reg);
            let ipv6_prefix_len = self.ipv6_prefix_len_for_device(reg);
            links.push(NetNamespaceLinkInfo {
                ifindex: next_ifindex,
                name: reg.name,
                kind: reg.ops.device_kind(),
                mtu: self.mtu_for_device(reg),
                mac: Some(reg.ops.mac_addr()),
                ipv4_addr,
                ipv4_prefix_len,
                ipv6_addr,
                ipv6_prefix_len,
                master: bridge_master_for(reg.name, &bridges),
                is_loopback: false,
                is_up: self.is_device_up(reg),
            });
        }

        if self.link_snapshot_generation.load(Ordering::Acquire) == generation {
            *self.link_snapshot_cache.lock() = Some(LinkSnapshotCache {
                generation,
                host_device_count,
                links: links.clone(),
            });
        }
        links
    }

    /// Whether `addr` is a local IPv4 address (primary or secondary) on an up
    /// link — the "is this mine" check the inline ICMP echo path uses.
    pub fn ipv4_addr_is_local_up(&self, addr: Ipv4Address) -> bool {
        if self
            .link_snapshot()
            .into_iter()
            .any(|link| link.is_up && link.ipv4_addr == Some(addr))
        {
            return true;
        }
        let matching: Vec<DevT> = self
            .ipv4_extra_addrs
            .lock()
            .iter()
            .filter(|extra| extra.addr == addr)
            .map(|extra| extra.devt)
            .collect();
        if matching.is_empty() {
            return false;
        }
        self.device_snapshot()
            .into_iter()
            .any(|reg| matching.contains(&reg.devt) && self.is_device_up(reg))
    }

    /// IPv6 counterpart of [`Self::ipv4_addr_is_local_up`].
    pub fn ipv6_addr_is_local_up(&self, addr: Ipv6Address) -> bool {
        if self
            .link_snapshot()
            .into_iter()
            .any(|link| link.is_up && link.ipv6_addr == Some(addr))
        {
            return true;
        }
        let matching: Vec<DevT> = self
            .ipv6_extra_addrs
            .lock()
            .iter()
            .filter(|extra| extra.addr == addr)
            .map(|extra| extra.devt)
            .collect();
        if matching.is_empty() {
            return false;
        }
        self.device_snapshot()
            .into_iter()
            .any(|reg| matching.contains(&reg.devt) && self.is_device_up(reg))
    }

    pub fn owns_ipv4_addr(&self, addr: Ipv4Address) -> bool {
        self.link_snapshot()
            .into_iter()
            .any(|link| link.ipv4_addr == Some(addr))
            || self
                .ipv4_extra_addrs
                .lock()
                .iter()
                .any(|extra| extra.addr == addr)
    }

    pub fn owns_ipv6_addr(&self, addr: Ipv6Address) -> bool {
        self.link_snapshot()
            .into_iter()
            .any(|link| link.ipv6_addr == Some(addr))
            || self
                .ipv6_extra_addrs
                .lock()
                .iter()
                .any(|extra| extra.addr == addr)
    }

    pub fn device_snapshot(&self) -> Vec<&'static NetDeviceRegistration> {
        let mut devices = Vec::new();
        let links = self.namespace_devices.lock();

        if self.host_devices_visible {
            for reg in net_device_snapshot() {
                devices.push(reg);
            }
        }

        for link in links.iter() {
            if devices
                .iter()
                .any(|reg| reg.name == link.registration.name || reg.devt == link.registration.devt)
            {
                continue;
            }
            devices.push(link.registration);
        }

        devices
    }

    pub fn bridge_snapshot(&self) -> Vec<NetNamespaceBridgeInfo> {
        bridge_snapshot_from_devices(&self.device_snapshot())
    }

    pub fn network_snapshot(&self) -> NetNamespaceSnapshot {
        NetNamespaceSnapshot {
            links: self.link_snapshot(),
            bridges: self.bridge_snapshot(),
        }
    }

    pub fn route_snapshot(&self) -> Vec<NetNamespaceRouteInfo> {
        let mut routes = Vec::new();
        let suppressed_connected_routes = self.suppressed_connected_routes.lock().clone();
        for link in self.link_snapshot() {
            if link.is_loopback {
                continue;
            }
            let Some(addr) = link.ipv4_addr else {
                continue;
            };
            let prefix_len = link.ipv4_prefix_len.unwrap_or(32).min(32);
            let key = NetNamespaceConnectedRouteKey {
                dst: ipv4_network(addr, prefix_len),
                prefix_len,
                oif_name: link.name,
                table: 254,
            };
            if suppressed_connected_routes
                .iter()
                .any(|suppressed| *suppressed == key)
            {
                continue;
            }
            routes.push(NetNamespaceRouteInfo {
                kind: NetNamespaceRouteKind::Connected,
                dst: key.dst,
                prefix_len,
                gateway: None,
                oif_name: Some(link.name),
                preferred_src: Some(addr),
                table: 254,
                protocol: 2,
                scope: 253,
                route_type: 1,
            });
        }

        // Secondary addresses contribute connected routes too (Linux: every
        // `ip addr add` installs one; only the first address per subnet/oif
        // keeps it). Without this, source selection for a peer reached via a
        // secondary's subnet finds no route — `ping 10.0.0.1` after
        // `ip addr add 10.0.0.2/24 dev eth0` on the boot NIC got EOPNOTSUPP
        // (LTP ping01 netns flow).
        let links = self.link_snapshot();
        for extra in self.ipv4_extra_snapshot() {
            let Some(link) = links.iter().find(|link| link.ifindex == extra.ifindex) else {
                continue;
            };
            if link.is_loopback {
                continue;
            }
            let prefix_len = extra.prefix_len.min(32);
            let key = NetNamespaceConnectedRouteKey {
                dst: ipv4_network(extra.addr, prefix_len),
                prefix_len,
                oif_name: link.name,
                table: 254,
            };
            if suppressed_connected_routes
                .iter()
                .any(|suppressed| *suppressed == key)
            {
                continue;
            }
            let duplicate = routes.iter().any(|route| {
                route.kind == NetNamespaceRouteKind::Connected
                    && route.dst == key.dst
                    && route.prefix_len == prefix_len
                    && route.oif_name == Some(link.name)
            });
            if duplicate {
                continue;
            }
            routes.push(NetNamespaceRouteInfo {
                kind: NetNamespaceRouteKind::Connected,
                dst: key.dst,
                prefix_len,
                gateway: None,
                oif_name: Some(link.name),
                preferred_src: Some(extra.addr),
                table: 254,
                protocol: 2,
                scope: 253,
                route_type: 1,
            });
        }

        routes.extend(
            self.routes
                .lock()
                .iter()
                .map(NetNamespaceRouteEntry::as_info),
        );
        routes.sort_by_key(|route| {
            (
                core::cmp::Reverse(route.prefix_len),
                route.dst,
                route.gateway.unwrap_or(Ipv4Address::UNSPECIFIED),
                route.oif_name.unwrap_or(""),
            )
        });
        routes
    }

    pub fn add_ipv4_route(
        &self,
        _authority: NetAdminAuthority,
        mut route: NetNamespaceRouteConfig,
    ) -> Result<(), Errno> {
        validate_route_config(route)?;
        if let Some(name) = route.oif_name {
            self.link_snapshot()
                .into_iter()
                .find(|link| link.name == name)
                .ok_or(Errno::ENODEV)?;
        } else if let Some(gateway) = route.gateway {
            // No explicit `dev`: resolve the egress interface from the
            // gateway's connected route (or loopback) so /proc/net/route and
            // `ip route show` render `... via <gw> dev <oif>` like Linux.
            route.oif_name = self.oif_for_gateway(gateway);
        }

        let entry = NetNamespaceRouteEntry::from_config(route);
        let mut routes = self.routes.lock();
        if routes.iter().any(|existing| existing.same_key(entry)) {
            return Err(Errno::EEXIST);
        }
        routes.push(entry);
        Ok(())
    }

    pub fn delete_ipv4_route(
        &self,
        _authority: NetAdminAuthority,
        selector: NetNamespaceRouteSelector,
    ) -> Result<(), Errno> {
        if selector.prefix_len > 32 {
            return Err(Errno::EINVAL);
        }
        let mut routes = self.routes.lock();
        let Some(idx) = routes
            .iter()
            .position(|route| route.matches_selector(selector))
        else {
            drop(routes);
            return self
                .suppress_connected_route_for_selector(selector)
                .ok_or(Errno::ENOENT)
                .map(|_| ());
        };
        routes.remove(idx);
        Ok(())
    }

    fn suppress_connected_route_for_selector(
        &self,
        selector: NetNamespaceRouteSelector,
    ) -> Option<NetNamespaceConnectedRouteKey> {
        let key = self.connected_route_key_matching_selector(selector)?;
        let mut suppressed = self.suppressed_connected_routes.lock();
        if !suppressed.iter().any(|existing| *existing == key) {
            suppressed.push(key);
        }
        Some(key)
    }

    fn connected_route_key_matching_selector(
        &self,
        selector: NetNamespaceRouteSelector,
    ) -> Option<NetNamespaceConnectedRouteKey> {
        if selector.gateway.is_some() {
            return None;
        }
        self.link_snapshot().into_iter().find_map(|link| {
            if link.is_loopback {
                return None;
            }
            let addr = link.ipv4_addr?;
            let prefix_len = link.ipv4_prefix_len.unwrap_or(32).min(32);
            let key = NetNamespaceConnectedRouteKey {
                dst: ipv4_network(addr, prefix_len),
                prefix_len,
                oif_name: link.name,
                table: 254,
            };
            key.matches_selector(selector).then_some(key)
        })
    }

    fn forget_connected_route_suppressions_for_oif(&self, oif_name: &'static str) {
        self.suppressed_connected_routes
            .lock()
            .retain(|key| key.oif_name != oif_name);
    }

    pub fn set_ipv4_forwarding_for_test_or_bootstrap(&self, enabled: bool) {
        self.ipv4_forwarding.store(enabled, Ordering::Release);
    }

    pub fn ipv4_forwarding_enabled(&self) -> bool {
        self.ipv4_forwarding.load(Ordering::Acquire)
    }

    pub fn set_ipv6_disabled_for_test_or_bootstrap(&self, disabled: bool) {
        self.ipv6_disabled.store(disabled, Ordering::Release);
    }

    pub fn ipv6_disabled(&self) -> bool {
        self.ipv6_disabled.load(Ordering::Acquire)
    }

    pub fn set_ipv6_accept_dad_for_test_or_bootstrap(&self, enabled: bool) {
        self.ipv6_accept_dad.store(enabled, Ordering::Release);
    }

    pub fn ipv6_accept_dad_enabled(&self) -> bool {
        self.ipv6_accept_dad.load(Ordering::Acquire)
    }

    pub fn best_ipv4_route(&self, dst: Ipv4Address) -> Option<NetNamespaceRouteDecision> {
        self.route_snapshot()
            .into_iter()
            .filter(|route| route_matches_ipv4(*route, dst))
            .filter_map(|route| self.route_decision_for_info(route, dst))
            .max_by_key(|decision| decision.prefix_len)
    }

    pub fn ether_ifaces_snapshot(&self) -> Vec<&'static EtherIface> {
        self.configured_ether_ifaces()
    }

    pub fn find_device_by_name(&self, name: &str) -> Option<&'static NetDeviceRegistration> {
        self.device_snapshot()
            .into_iter()
            .find(|registration| registration.name == name)
    }

    pub fn find_device_by_ifindex(&self, ifindex: u32) -> Option<&'static NetDeviceRegistration> {
        let name = self
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)?
            .name;
        self.find_device_by_name(name)
    }

    fn route_decision_for_info(
        &self,
        route: NetNamespaceRouteInfo,
        dst: Ipv4Address,
    ) -> Option<NetNamespaceRouteDecision> {
        let oif_name = route.oif_name.or_else(|| {
            route
                .gateway
                .and_then(|gateway| self.oif_for_gateway(gateway))
        })?;
        if !self.link_snapshot().into_iter().any(|link| {
            link.name == oif_name && link.is_up && link.ipv4_addr.is_some() && !link.is_loopback
        }) {
            return None;
        }
        Some(NetNamespaceRouteDecision {
            oif_name,
            next_hop: route.gateway.unwrap_or(dst),
            preferred_src: route.preferred_src,
            prefix_len: route.prefix_len,
            kind: route.kind,
        })
    }

    fn oif_for_gateway(&self, gateway: Ipv4Address) -> Option<&'static str> {
        // A loopback gateway (127.0.0.0/8) is reached over the loopback device;
        // the loopback link is intentionally absent from the connected-route
        // snapshot, so resolve it directly here.
        if gateway.octets()[0] == 127 {
            return self
                .link_snapshot()
                .into_iter()
                .find(|link| link.is_loopback)
                .map(|link| link.name);
        }
        self.route_snapshot()
            .into_iter()
            .filter(|route| route.kind == NetNamespaceRouteKind::Connected)
            .filter(|route| route_matches_ipv4(*route, gateway))
            .max_by_key(|route| route.prefix_len)
            .and_then(|route| route.oif_name)
    }

    // -----------------------------------------------------------------------
    // IPv6 routing / FIB methods — a 1:1 mirror of the IPv4 methods above,
    // operating on the independent `routes6` table. Management-plane only.
    // -----------------------------------------------------------------------

    pub fn route6_snapshot(&self) -> Vec<NetNamespaceRoute6Info> {
        let mut routes = Vec::new();
        let suppressed_connected_routes = self.suppressed_connected_routes6.lock().clone();
        for link in self.link_snapshot() {
            if link.is_loopback {
                continue;
            }
            let Some(addr) = link.ipv6_addr else {
                continue;
            };
            let prefix_len = link.ipv6_prefix_len.unwrap_or(64).min(128);
            let key = NetNamespaceConnectedRoute6Key {
                dst: ipv6_network(addr, prefix_len),
                prefix_len,
                oif_name: link.name,
                table: 254,
            };
            if suppressed_connected_routes
                .iter()
                .any(|suppressed| *suppressed == key)
            {
                continue;
            }
            routes.push(NetNamespaceRoute6Info {
                kind: NetNamespaceRouteKind::Connected,
                dst: key.dst,
                prefix_len,
                gateway: None,
                oif_name: Some(link.name),
                preferred_src: Some(addr),
                table: 254,
                protocol: 2,
                scope: 253,
                route_type: 1,
            });
        }

        routes.extend(
            self.routes6
                .lock()
                .iter()
                .map(NetNamespaceRoute6Entry::as_info),
        );
        routes.sort_by_key(|route| {
            (
                core::cmp::Reverse(route.prefix_len),
                route.dst,
                route.gateway.unwrap_or(Ipv6Address::UNSPECIFIED),
                route.oif_name.unwrap_or(""),
            )
        });
        routes
    }

    pub fn add_ipv6_route(
        &self,
        _authority: NetAdminAuthority,
        mut route: NetNamespaceRoute6Config,
    ) -> Result<(), Errno> {
        validate_route6_config(route)?;
        if let Some(name) = route.oif_name {
            self.link_snapshot()
                .into_iter()
                .find(|link| link.name == name)
                .ok_or(Errno::ENODEV)?;
        } else if let Some(gateway) = route.gateway {
            // No explicit `dev`: resolve the egress interface from the
            // gateway's connected route (or loopback) so /proc/net/ipv6_route
            // and `ip -6 route show` render `... via <gw> dev <oif>` like Linux.
            route.oif_name = self.oif6_for_gateway(gateway);
        }

        let entry = NetNamespaceRoute6Entry::from_config(route);
        let mut routes = self.routes6.lock();
        if routes.iter().any(|existing| existing.same_key(entry)) {
            return Err(Errno::EEXIST);
        }
        routes.push(entry);
        Ok(())
    }

    pub fn delete_ipv6_route(
        &self,
        _authority: NetAdminAuthority,
        selector: NetNamespaceRoute6Selector,
    ) -> Result<(), Errno> {
        if selector.prefix_len > 128 {
            return Err(Errno::EINVAL);
        }
        let mut routes = self.routes6.lock();
        let Some(idx) = routes
            .iter()
            .position(|route| route.matches_selector(selector))
        else {
            drop(routes);
            return self
                .suppress_connected_route6_for_selector(selector)
                .ok_or(Errno::ENOENT)
                .map(|_| ());
        };
        routes.remove(idx);
        Ok(())
    }

    fn suppress_connected_route6_for_selector(
        &self,
        selector: NetNamespaceRoute6Selector,
    ) -> Option<NetNamespaceConnectedRoute6Key> {
        let key = self.connected_route6_key_matching_selector(selector)?;
        let mut suppressed = self.suppressed_connected_routes6.lock();
        if !suppressed.iter().any(|existing| *existing == key) {
            suppressed.push(key);
        }
        Some(key)
    }

    fn connected_route6_key_matching_selector(
        &self,
        selector: NetNamespaceRoute6Selector,
    ) -> Option<NetNamespaceConnectedRoute6Key> {
        if selector.gateway.is_some() {
            return None;
        }
        self.link_snapshot().into_iter().find_map(|link| {
            if link.is_loopback {
                return None;
            }
            let addr = link.ipv6_addr?;
            let prefix_len = link.ipv6_prefix_len.unwrap_or(64).min(128);
            let key = NetNamespaceConnectedRoute6Key {
                dst: ipv6_network(addr, prefix_len),
                prefix_len,
                oif_name: link.name,
                table: 254,
            };
            key.matches_selector(selector).then_some(key)
        })
    }

    #[allow(dead_code)]
    fn forget_connected_route6_suppressions_for_oif(&self, oif_name: &'static str) {
        self.suppressed_connected_routes6
            .lock()
            .retain(|key| key.oif_name != oif_name);
    }

    pub fn best_ipv6_route(&self, dst: Ipv6Address) -> Option<NetNamespaceRoute6Decision> {
        self.route6_snapshot()
            .into_iter()
            .filter(|route| route6_matches_ipv6(*route, dst))
            .filter_map(|route| self.route6_decision_for_info(route, dst))
            .max_by_key(|decision| decision.prefix_len)
    }

    /// V5-3: FIB-driven IPv6 source-address selection — the single place that
    /// answers "which of my addresses do I send to `dst` from?".
    ///
    /// Before this, three call sites (connect autobind in tx-shims,
    /// `select_routed_local`, and the raw-ICMPv6 helper) each carried their own
    /// copy of "take the first up non-loopback link that has any v6 address",
    /// which ignores `preferred_src`, prefix length and the outgoing interface
    /// — wrong the moment a host has more than one v6 prefix.
    ///
    /// **Returning `None` when no route covers `dst` is load-bearing**, not an
    /// oversight: connect() relies on it to fail fast with EADDRNOTAVAIL
    /// instead of queueing a SYN that `decide_ipv6_route` will refuse forever.
    /// Callers that would rather send from *something* than not send at all
    /// (raw ICMPv6) layer their own fallback on top.
    pub fn preferred_ipv6_source(&self, dst: Ipv6Address) -> Option<Ipv6Address> {
        let route = self.best_ipv6_route(dst)?;
        route.preferred_src.or_else(|| {
            self.link_snapshot()
                .into_iter()
                .find(|link| {
                    link.name == route.oif_name
                        && link.is_up
                        && !link.is_loopback
                        && link.ipv6_addr.is_some()
                })
                .and_then(|link| link.ipv6_addr)
        })
    }

    fn route6_decision_for_info(
        &self,
        route: NetNamespaceRoute6Info,
        dst: Ipv6Address,
    ) -> Option<NetNamespaceRoute6Decision> {
        let oif_name = route.oif_name.or_else(|| {
            route
                .gateway
                .and_then(|gateway| self.oif6_for_gateway(gateway))
        })?;
        if !self.link_snapshot().into_iter().any(|link| {
            link.name == oif_name && link.is_up && link.ipv6_addr.is_some() && !link.is_loopback
        }) {
            return None;
        }
        Some(NetNamespaceRoute6Decision {
            oif_name,
            next_hop: route.gateway.unwrap_or(dst),
            preferred_src: route.preferred_src,
            prefix_len: route.prefix_len,
            kind: route.kind,
        })
    }

    fn oif6_for_gateway(&self, gateway: Ipv6Address) -> Option<&'static str> {
        // A loopback gateway (`::1`) is reached over the loopback device; the
        // loopback link is intentionally absent from the connected-route
        // snapshot, so resolve it directly here (mirrors the v4 127/8 case).
        if gateway == Ipv6Address::LOOPBACK {
            return self
                .link_snapshot()
                .into_iter()
                .find(|link| link.is_loopback)
                .map(|link| link.name);
        }
        self.route6_snapshot()
            .into_iter()
            .filter(|route| route.kind == NetNamespaceRouteKind::Connected)
            .filter(|route| route6_matches_ipv6(*route, gateway))
            .max_by_key(|route| route.prefix_len)
            .and_then(|route| route.oif_name)
    }

    fn gateway_for_device(&self, name: &'static str) -> Option<Ipv4Address> {
        let routes = self.routes.lock().clone();
        routes.iter().find_map(|route| {
            let gateway = route.gateway?;
            if route.prefix_len == 0
                && (route.oif_name == Some(name)
                    || if route.oif_name.is_none() {
                        self.oif_for_gateway(gateway) == Some(name)
                    } else {
                        false
                    })
            {
                Some(gateway)
            } else {
                None
            }
        })
    }

    /// IPv6 V3b: the ::/0 default route's gateway for `name` (mirror of
    /// [`gateway_for_device`], over the v6 `routes6` FIB).
    fn gateway6_for_device(&self, name: &'static str) -> Option<Ipv6Address> {
        let routes = self.routes6.lock().clone();
        routes.iter().find_map(|route| {
            let gateway = route.gateway?;
            if route.prefix_len == 0
                && (route.oif_name == Some(name)
                    || if route.oif_name.is_none() {
                        self.oif6_for_gateway(gateway) == Some(name)
                    } else {
                        false
                    })
            {
                Some(gateway)
            } else {
                None
            }
        })
    }

    pub fn set_device_up_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        is_up: bool,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            // The loopback device is always functional. Linux lets you toggle
            // its IFF_UP (returning success), so accept the request, but never
            // actually tear down loopback delivery — many subsystems rely on it
            // and no test needs `lo` genuinely down (e.g. CVE survival probes
            // just need SIOCSIFFLAGS to succeed).
            let _ = is_up;
            return Ok(());
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let mut devices = self.namespace_devices.lock();
        if let Some(link) = devices
            .iter_mut()
            .find(|link| link.registration.devt == registration.devt)
        {
            link.is_up = is_up;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        devices.push(NetNamespaceDeviceLink {
            registration,
            ipv4_addr: None,
            ipv4_prefix_len: None,
            ipv6_addr: None,
            ipv6_prefix_len: None,
            mtu: None,
            is_up,
        });
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    pub fn set_device_mtu_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        mtu: u16,
    ) -> Result<(), Errno> {
        if mtu < 68 {
            return Err(Errno::EINVAL);
        }
        if ifindex == 1 {
            *self.loopback_mtu.lock() = mtu;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let mut devices = self.namespace_devices.lock();
        if let Some(link) = devices
            .iter_mut()
            .find(|link| link.registration.devt == registration.devt)
        {
            link.mtu = Some(mtu);
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        devices.push(NetNamespaceDeviceLink {
            registration,
            ipv4_addr: None,
            ipv4_prefix_len: None,
            ipv6_addr: None,
            ipv6_prefix_len: None,
            mtu: Some(mtu),
            is_up: true,
        });
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    pub fn set_device_ipv4_addr_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ipv4_addr: Option<Ipv4Address>,
        ipv4_prefix_len: Option<u8>,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            let next = ipv4_addr.map(|addr| (addr, ipv4_prefix_len.unwrap_or(8)));
            *self.loopback_ipv4_override.lock() = next;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        self.forget_connected_route_suppressions_for_oif(registration.name);
        let mut devices = self.namespace_devices.lock();
        if let Some(link) = devices
            .iter_mut()
            .find(|link| link.registration.devt == registration.devt)
        {
            link.ipv4_addr = ipv4_addr;
            link.ipv4_prefix_len = ipv4_prefix_len;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        devices.push(NetNamespaceDeviceLink {
            registration,
            ipv4_addr,
            ipv4_prefix_len,
            ipv6_addr: None,
            ipv6_prefix_len: None,
            mtu: None,
            is_up: true,
        });
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    /// Add an IPv4 address to a link. The first address becomes the primary
    /// (same as [`Self::set_device_ipv4_addr_by_ifindex`]); further distinct
    /// addresses become secondaries, optionally labeled (`ifconfig eth0:1`).
    /// Re-adding the primary or an existing secondary updates its prefix/label.
    pub fn add_device_ipv4_addr_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
        addr: Ipv4Address,
        prefix_len: u8,
        label: Option<&str>,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            return self.set_device_ipv4_addr_by_ifindex(
                authority,
                ifindex,
                Some(addr),
                Some(prefix_len),
            );
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let primary = self.ipv4_for_device(registration);
        if primary.is_none() || primary == Some(addr) {
            return self.set_device_ipv4_addr_by_ifindex(
                authority,
                ifindex,
                Some(addr),
                Some(prefix_len),
            );
        }
        {
            let mut extras = self.ipv4_extra_addrs.lock();
            if let Some(extra) = extras
                .iter_mut()
                .find(|extra| extra.devt == registration.devt && extra.addr == addr)
            {
                extra.prefix_len = prefix_len;
                if label.is_some() {
                    extra.label = label.map(alloc::string::String::from);
                }
            } else {
                extras.push(ExtraIpv4Addr {
                    devt: registration.devt,
                    addr,
                    prefix_len,
                    label: label.map(alloc::string::String::from),
                });
            }
        }
        // Address sets are generation-tracked (the RTM_GETADDR dump template
        // cache keys on it), so secondaries must bump it too.
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    /// Delete one IPv4 address from a link: a matching secondary is removed;
    /// a matching primary is cleared (secondaries stay). Returns whether an
    /// address was actually removed.
    pub fn del_device_ipv4_addr_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
        addr: Ipv4Address,
    ) -> Result<bool, Errno> {
        if ifindex == 1 {
            // Loopback is override-modelled, not a registered device: deleting
            // the effective address reverts to the 127.0.0.1/8 default.
            let current = self
                .link_snapshot()
                .into_iter()
                .find(|link| link.ifindex == 1)
                .and_then(|link| link.ipv4_addr);
            if current == Some(addr) {
                self.set_device_ipv4_addr_by_ifindex(authority, ifindex, None, None)?;
                return Ok(true);
            }
            return Ok(false);
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        {
            let mut extras = self.ipv4_extra_addrs.lock();
            let before = extras.len();
            extras.retain(|extra| !(extra.devt == registration.devt && extra.addr == addr));
            if extras.len() != before {
                drop(extras);
                self.invalidate_link_snapshot_cache();
                return Ok(true);
            }
        }
        if self.ipv4_for_device(registration) == Some(addr) {
            self.set_device_ipv4_addr_by_ifindex(authority, ifindex, None, None)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Delete the labeled IPv4 alias (`ifconfig eth0:1 down`). Returns whether
    /// the label existed.
    pub fn del_device_ipv4_addr_by_label(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        label: &str,
    ) -> Result<bool, Errno> {
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let mut extras = self.ipv4_extra_addrs.lock();
        let before = extras.len();
        extras.retain(|extra| {
            !(extra.devt == registration.devt && extra.label.as_deref() == Some(label))
        });
        let removed = extras.len() != before;
        drop(extras);
        if removed {
            self.invalidate_link_snapshot_cache();
        }
        Ok(removed)
    }

    /// Look up a labeled IPv4 alias for the SIOCGIF* ioctls.
    pub fn ipv4_extra_by_label(&self, ifindex: u32, label: &str) -> Option<(Ipv4Address, u8)> {
        let registration = self.find_device_by_ifindex(ifindex)?;
        self.ipv4_extra_addrs
            .lock()
            .iter()
            .find(|extra| extra.devt == registration.devt && extra.label.as_deref() == Some(label))
            .map(|extra| (extra.addr, extra.prefix_len))
    }

    /// All secondary IPv4 addresses, with the same positional ifindex mapping
    /// as [`Self::link_snapshot`] (for the RTM_GETADDR dump).
    pub fn ipv4_extra_snapshot(&self) -> Vec<NetNamespaceExtraIpv4Info> {
        let mut out = Vec::new();
        let extras = self.ipv4_extra_addrs.lock();
        if extras.is_empty() {
            return out;
        }
        for (ifindex, reg) in (2..).zip(self.device_snapshot()) {
            for extra in extras.iter().filter(|extra| extra.devt == reg.devt) {
                out.push(NetNamespaceExtraIpv4Info {
                    ifindex,
                    addr: extra.addr,
                    prefix_len: extra.prefix_len,
                    label: extra.label.clone(),
                });
            }
        }
        out
    }

    /// Add an IPv6 address to a link. The NEWEST address takes the primary
    /// slot; any address it displaces is demoted to a secondary.
    ///
    /// **Why newest-wins and not first-wins.** Only the primary is routable:
    /// `route6_snapshot` synthesises connected routes from `link.ipv6_addr`
    /// alone, and `IfaceCommon` carries exactly one v6 address for
    /// `decide_ipv6_route`'s on-link test. A secondary is therefore *stored but
    /// unusable* — a silent black hole. That was tolerable while nothing
    /// occupied the primary slot at boot (the user's first `ip -6 addr add`
    /// became primary and worked), but the V5-2 boot seed now holds it, so
    /// first-wins would make **every** explicitly configured address unroutable.
    ///
    /// Newest-wins dominates the old behaviour in every case: one address
    /// behaves identically, and with several the most recently configured one
    /// works where previously only the first did. It is still a deviation from
    /// Linux, which routes all of them — carrying every on-link prefix into
    /// `IfaceCommon` (and emitting connected routes for secondaries) is the
    /// real fix and is recorded as P5 debt.
    pub fn add_device_ipv6_addr_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
        addr: Ipv6Address,
        prefix_len: u8,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            return self.set_device_ipv6_addr_by_ifindex(
                authority,
                ifindex,
                Some(addr),
                Some(prefix_len),
            );
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let primary = self.ipv6_for_device(registration);
        if primary.is_none() || primary == Some(addr) {
            return self.set_device_ipv6_addr_by_ifindex(
                authority,
                ifindex,
                Some(addr),
                Some(prefix_len),
            );
        }
        // Demote the address currently holding the primary slot, then install
        // the new one there (see the newest-wins rationale above). Read the
        // displaced prefix length BEFORE the overwrite, and do the fallible
        // primary update BEFORE touching `extras`, so a failure cannot leave the
        // link with the old primary demoted and nothing promoted.
        let displaced = primary.map(|addr| {
            (
                addr,
                self.ipv6_prefix_len_for_device(registration).unwrap_or(64),
            )
        });
        self.set_device_ipv6_addr_by_ifindex(authority, ifindex, Some(addr), Some(prefix_len))?;
        {
            let mut extras = self.ipv6_extra_addrs.lock();
            // Whatever we are promoting must not linger as a secondary too.
            extras.retain(|extra| !(extra.devt == registration.devt && extra.addr == addr));
            if let Some((old_addr, old_prefix_len)) = displaced {
                if let Some(extra) = extras
                    .iter_mut()
                    .find(|extra| extra.devt == registration.devt && extra.addr == old_addr)
                {
                    extra.prefix_len = old_prefix_len;
                } else {
                    extras.push(ExtraIpv6Addr {
                        devt: registration.devt,
                        addr: old_addr,
                        prefix_len: old_prefix_len,
                    });
                }
            }
        }
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    /// Delete one IPv6 address from a link (secondary removed, or primary
    /// cleared). Returns whether an address was actually removed.
    pub fn del_device_ipv6_addr_by_ifindex(
        &self,
        authority: NetAdminAuthority,
        ifindex: u32,
        addr: Ipv6Address,
    ) -> Result<bool, Errno> {
        if ifindex == 1 {
            let current = self
                .link_snapshot()
                .into_iter()
                .find(|link| link.ifindex == 1)
                .and_then(|link| link.ipv6_addr);
            if current == Some(addr) {
                self.set_device_ipv6_addr_by_ifindex(authority, ifindex, None, None)?;
                return Ok(true);
            }
            return Ok(false);
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        {
            let mut extras = self.ipv6_extra_addrs.lock();
            let before = extras.len();
            extras.retain(|extra| !(extra.devt == registration.devt && extra.addr == addr));
            if extras.len() != before {
                drop(extras);
                self.invalidate_link_snapshot_cache();
                return Ok(true);
            }
        }
        if self.ipv6_for_device(registration) == Some(addr) {
            // Symmetric counterpart of the newest-wins promotion in
            // `add_device_ipv6_addr_by_ifindex`: hand the routable primary slot
            // to the most recently demoted secondary instead of leaving the
            // link with no usable v6 address at all. Without this, deleting one
            // of two configured addresses kills v6 on the link entirely even
            // though another address is still configured.
            let promoted = {
                let mut extras = self.ipv6_extra_addrs.lock();
                extras
                    .iter()
                    .rposition(|extra| extra.devt == registration.devt)
                    .map(|index| {
                        let extra = extras.remove(index);
                        (extra.addr, extra.prefix_len)
                    })
            };
            match promoted {
                Some((next_addr, next_prefix_len)) => self.set_device_ipv6_addr_by_ifindex(
                    authority,
                    ifindex,
                    Some(next_addr),
                    Some(next_prefix_len),
                )?,
                None => self.set_device_ipv6_addr_by_ifindex(authority, ifindex, None, None)?,
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// All secondary IPv6 addresses (positional ifindex mapping, see
    /// [`Self::ipv4_extra_snapshot`]).
    pub fn ipv6_extra_snapshot(&self) -> Vec<NetNamespaceExtraIpv6Info> {
        let mut out = Vec::new();
        let extras = self.ipv6_extra_addrs.lock();
        if extras.is_empty() {
            return out;
        }
        for (ifindex, reg) in (2..).zip(self.device_snapshot()) {
            for extra in extras.iter().filter(|extra| extra.devt == reg.devt) {
                out.push(NetNamespaceExtraIpv6Info {
                    ifindex,
                    addr: extra.addr,
                    prefix_len: extra.prefix_len,
                });
            }
        }
        out
    }

    pub fn set_device_ipv6_addr_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ipv6_addr: Option<Ipv6Address>,
        ipv6_prefix_len: Option<u8>,
    ) -> Result<(), Errno> {
        if ifindex == 1 {
            let next = ipv6_addr.map(|addr| (addr, ipv6_prefix_len.unwrap_or(128)));
            *self.loopback_ipv6_override.lock() = next;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let mut devices = self.namespace_devices.lock();
        if let Some(link) = devices
            .iter_mut()
            .find(|link| link.registration.devt == registration.devt)
        {
            link.ipv6_addr = ipv6_addr;
            link.ipv6_prefix_len = ipv6_prefix_len;
            self.invalidate_link_snapshot_cache();
            return Ok(());
        }
        devices.push(NetNamespaceDeviceLink {
            registration,
            ipv4_addr: None,
            ipv4_prefix_len: None,
            ipv6_addr,
            ipv6_prefix_len,
            mtu: None,
            is_up: true,
        });
        self.invalidate_link_snapshot_cache();
        Ok(())
    }

    pub fn install_static_neighbor_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ip: Ipv4Address,
        mac: EthernetAddress,
    ) -> Result<(), Errno> {
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let link = self
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)
            .ok_or(Errno::ENODEV)?;
        if link.is_loopback || link.ipv4_addr.is_none() {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let iface = self
            .ensure_ether_iface_for_link(registration, link)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        iface.install_static_arp(ip, mac);
        Ok(())
    }

    pub fn delete_static_neighbor_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ip: Ipv4Address,
    ) -> Result<(), Errno> {
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let link = self
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)
            .ok_or(Errno::ENODEV)?;
        if link.is_loopback || link.ipv4_addr.is_none() {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let iface = self
            .ensure_ether_iface_for_link(registration, link)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        if iface.remove_static_arp(ip) {
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    }

    pub fn install_static_ndisc_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ip: Ipv6Address,
        mac: EthernetAddress,
    ) -> Result<(), Errno> {
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let link = self
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)
            .ok_or(Errno::ENODEV)?;
        if link.is_loopback || link.ipv6_addr.is_none() {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let iface = self
            .ensure_ether_iface_for_link(registration, link)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        iface.install_static_ndisc(ip, mac);
        Ok(())
    }

    pub fn delete_static_ndisc_by_ifindex(
        &self,
        _authority: NetAdminAuthority,
        ifindex: u32,
        ip: Ipv6Address,
    ) -> Result<(), Errno> {
        let registration = self.find_device_by_ifindex(ifindex).ok_or(Errno::ENODEV)?;
        let link = self
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)
            .ok_or(Errno::ENODEV)?;
        if link.is_loopback || link.ipv6_addr.is_none() {
            return Err(Errno::EADDRNOTAVAIL);
        }
        let iface = self
            .ensure_ether_iface_for_link(registration, link)
            .ok_or(Errno::EADDRNOTAVAIL)?;
        if iface.remove_static_ndisc(ip) {
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    }

    fn ipv4_for_device(&self, registration: &'static NetDeviceRegistration) -> Option<Ipv4Address> {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .and_then(|link| link.ipv4_addr)
    }

    fn ipv4_prefix_len_for_device(
        &self,
        registration: &'static NetDeviceRegistration,
    ) -> Option<u8> {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .and_then(|link| link.ipv4_prefix_len)
    }

    fn ipv6_for_device(&self, registration: &'static NetDeviceRegistration) -> Option<Ipv6Address> {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .and_then(|link| link.ipv6_addr)
    }

    fn ipv6_prefix_len_for_device(
        &self,
        registration: &'static NetDeviceRegistration,
    ) -> Option<u8> {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .and_then(|link| link.ipv6_prefix_len)
    }

    fn mtu_for_device(&self, registration: &'static NetDeviceRegistration) -> u16 {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .and_then(|link| link.mtu)
            .unwrap_or_else(|| registration.ops.mtu())
    }

    fn is_device_up(&self, registration: &'static NetDeviceRegistration) -> bool {
        self.namespace_devices
            .lock()
            .iter()
            .find(|link| link.registration.devt == registration.devt)
            .is_none_or(|link| link.is_up)
    }

    fn remove_device_from_local_bridges(
        &self,
        authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
    ) {
        for bridge in self.device_snapshot() {
            if bridge.devt == registration.devt {
                continue;
            }
            let _ = bridge.ops.bridge_remove_port(authority, registration);
        }
    }

    fn configured_ether_ifaces(&self) -> Vec<&'static EtherIface> {
        let mut ifaces = Vec::new();
        for link in self.link_snapshot() {
            if link.is_loopback || !link.is_up || link.ipv4_addr.is_none() {
                continue;
            }
            let Some(registration) = self.find_device_by_name(link.name) else {
                continue;
            };
            if let Some(iface) = self.ensure_ether_iface_for_link(registration, link) {
                ifaces.push(iface);
            }
        }
        ifaces
    }

    fn up_ether_links(&self) -> Vec<(NetNamespaceLinkInfo, &'static NetDeviceRegistration)> {
        self.link_snapshot()
            .into_iter()
            .filter(|link| !link.is_loopback && link.is_up)
            .filter_map(|link| {
                self.find_device_by_ifindex(link.ifindex)
                    .map(|registration| (link, registration))
            })
            .collect()
    }

    fn ensure_ether_iface_for_link(
        &self,
        registration: &'static NetDeviceRegistration,
        link: NetNamespaceLinkInfo,
    ) -> Option<&'static EtherIface> {
        let ipv4_addr = link.ipv4_addr?;
        let ipv4_prefix_len = link.ipv4_prefix_len.unwrap_or(32).min(32);
        let gateway = self.gateway_for_device(link.name);
        // IPv6 V1: carry the iface's on-link v6 config into IfaceCommon so
        // decide_ipv6_route can do the on-link prefix check; part of the
        // rebuild cache key so a v6-addr change re-leaks the iface.
        let ipv6_addr = link.ipv6_addr;
        // Clamp to the v6 max (mirror the v4 `.min(32)` above). The netlink
        // address parse (parse_ifaddrmsg) stores prefix_len as a raw byte and
        // never bounds it, so `ip -6 addr add .../200` would otherwise reach
        // decide_ipv6_route -> same_ipv6_prefix, which indexes a [u8;16] by
        // plen/8 and panics the kernel out of bounds.
        let ipv6_prefix_len = link.ipv6_prefix_len.map(|p| p.min(128));
        // IPv6 V3b: off-link v6 next-hop from the ::/0 default route (mirror of
        // the v4 `gateway`); part of the rebuild cache key so a v6-gateway change
        // re-leaks the iface.
        let ipv6_gateway = self.gateway6_for_device(link.name);
        let mut runtime = self.iface_runtime.lock();

        if let Some(entry) = runtime.iter().find(|entry| {
            entry.registration.devt == registration.devt
                && entry.ipv4_addr == ipv4_addr
                && entry.ipv4_prefix_len == ipv4_prefix_len
                && entry.gateway == gateway
                && entry.ipv6_addr == ipv6_addr
                && entry.ipv6_prefix_len == ipv6_prefix_len
                && entry.ipv6_gateway == ipv6_gateway
        }) {
            return Some(entry.iface);
        }

        let iface = Box::leak(Box::new(EtherIface::new(
            registration,
            IfaceCommon::with_gateway(
                ipv4_addr,
                prefix_len_to_netmask(ipv4_prefix_len),
                gateway,
                registration.ops.mtu(),
            )
            .with_ipv6(ipv6_addr, ipv6_prefix_len)
            .with_ipv6_gateway(ipv6_gateway),
            registration.ops.mac_addr(),
            registration.name,
        )));

        if let Some(entry) = runtime
            .iter_mut()
            .find(|entry| entry.registration.devt == registration.devt)
        {
            iface.copy_arp_cache_from(entry.iface);
            *entry = NetNamespaceIfaceRuntime {
                registration,
                ipv4_addr,
                ipv4_prefix_len,
                gateway,
                ipv6_addr,
                ipv6_prefix_len,
                ipv6_gateway,
                iface,
            };
        } else {
            runtime.push(NetNamespaceIfaceRuntime {
                registration,
                ipv4_addr,
                ipv4_prefix_len,
                gateway,
                ipv6_addr,
                ipv6_prefix_len,
                ipv6_gateway,
                iface,
            });
        }

        Some(iface)
    }
}

fn bridge_snapshot_from_devices(
    devices: &[&'static NetDeviceRegistration],
) -> Vec<NetNamespaceBridgeInfo> {
    devices
        .iter()
        .filter_map(|reg| bridge_info_from_device(reg.ops.bridge_snapshot()?))
        .collect()
}

fn bridge_info_from_device(snapshot: BridgeSnapshot) -> Option<NetNamespaceBridgeInfo> {
    Some(NetNamespaceBridgeInfo {
        name: snapshot.name,
        ports: snapshot.ports.into_iter().map(|port| port.name).collect(),
        learned_entries: snapshot.learned_entries,
    })
}

fn bridge_master_for(
    link_name: &'static str,
    bridges: &[NetNamespaceBridgeInfo],
) -> Option<&'static str> {
    bridges
        .iter()
        .find(|bridge| bridge.ports.contains(&link_name))
        .map(|bridge| bridge.name)
}

struct NamespaceEtherPacketSource<'a> {
    namespace: &'a NetNamespacePayload,
    registration: &'static NetDeviceRegistration,
    iface: Option<&'a EtherIface>,
    forwarded: Cell<usize>,
    pending_resolution: Cell<usize>,
    dropped: Cell<usize>,
}

impl<'a> NamespaceEtherPacketSource<'a> {
    fn new(
        namespace: &'a NetNamespacePayload,
        registration: &'static NetDeviceRegistration,
        iface: Option<&'a EtherIface>,
    ) -> Self {
        Self {
            namespace,
            registration,
            iface,
            forwarded: Cell::new(0),
            pending_resolution: Cell::new(0),
            dropped: Cell::new(0),
        }
    }

    fn record_forwarding(&self, outcome: NetNamespaceForwardOutcome) {
        self.forwarded
            .set(self.forwarded.get().saturating_add(outcome.forwarded));
        self.pending_resolution.set(
            self.pending_resolution
                .get()
                .saturating_add(outcome.pending_resolution),
        );
        self.dropped
            .set(self.dropped.get().saturating_add(outcome.dropped));
    }

    fn forwarding_outcome(&self) -> NetNamespaceForwardOutcome {
        NetNamespaceForwardOutcome {
            forwarded: self.forwarded.get(),
            pending_resolution: self.pending_resolution.get(),
            dropped: self.dropped.get(),
        }
    }
}

impl PacketSource for NamespaceEtherPacketSource<'_> {
    fn next_packet(&self) -> Option<PacketDispatch> {
        let frame = self.registration.ops.receive()?;
        let guard = tx_substrate::epoch::guard();
        step_packet_ingress_fanout(self.namespace, self.registration, &frame, &guard);
        Some(self.iface.map_or(PacketDispatch::Unsupported, |iface| {
            iface.process_frame_at(frame, Instant::ZERO, Option::<&Guard<'_>>::None)
        }))
    }

    fn next_packet_at(&self, now: Instant, guard: &Guard<'_>) -> Option<PacketDispatch> {
        let frame = self.registration.ops.receive()?;
        step_packet_ingress_fanout(self.namespace, self.registration, &frame, guard);
        if let Some(iface) = self.iface {
            if let Some(outcome) = self
                .namespace
                .try_forward_ingress_frame(iface, &frame, now, guard)
            {
                self.record_forwarding(outcome);
                return Some(PacketDispatch::Unsupported);
            }
            return Some(iface.process_frame_at(frame, now, Some(guard)));
        }
        Some(PacketDispatch::Unsupported)
    }
}

pub fn drive_all_net_namespace_runtimes_at(
    now: Instant,
    guard: &Guard<'_>,
) -> NetNamespaceRuntimeOutcome {
    let namespaces = NET_NAMESPACE_RUNTIME_LIST.lock().clone();
    let mut outcome = NetNamespaceRuntimeOutcome::default();
    for namespace in namespaces {
        outcome.merge(drive_net_namespace_runtime_at(namespace, now, guard));
    }
    outcome
}

pub fn drive_net_namespace_runtime_at(
    net_namespace: PayloadCap<NetNamespacePayload>,
    now: Instant,
    guard: &Guard<'_>,
) -> NetNamespaceRuntimeOutcome {
    let mut outcome = NetNamespaceRuntimeOutcome {
        namespaces_seen: 1,
        ..NetNamespaceRuntimeOutcome::default()
    };

    outcome.merge_bridge(net_namespace.poll_bridges(guard));

    for (link, registration) in net_namespace.up_ether_links() {
        outcome.ifaces_seen += 1;

        let iface = if link.ipv4_addr.is_some() {
            net_namespace.ensure_ether_iface_for_link(registration, link)
        } else {
            None
        };

        let source = NamespaceEtherPacketSource::new(&net_namespace, registration, iface);
        if let StepOutcome::Done(events) =
            step_process_network_events_in_namespace_at(&source, net_namespace.clone(), now, guard)
        {
            outcome.packets_seen += events.packets_seen;
            outcome.sockets_touched += events.sockets_touched;
            outcome.wakes_fired += events.wakes_fired;
        }
        outcome.merge_forwarding(source.forwarding_outcome());

        let Some(iface) = iface else {
            continue;
        };
        let sink = EtherPacketTxSink { iface };
        if let StepOutcome::Done(device_tx) = step_process_device_tx_pending_in_namespace_at(
            &sink,
            net_namespace.clone(),
            now,
            DeviceTxBudget::default(),
            guard,
        ) {
            outcome.merge_device_tx(device_tx);
        }

        if let StepOutcome::Done(arp_flush) = step_flush_pending_arp(iface, now, 8, guard) {
            outcome.merge_arp_flush(arp_flush);
        }
    }

    outcome.merge_forwarding(net_namespace.retry_pending_ipv4_forwards(now, guard));
    outcome.merge_bridge(net_namespace.poll_bridges(guard));
    outcome
}

impl NetNamespacePayload {
    fn try_forward_ingress_frame(
        &self,
        ingress: &EtherIface,
        frame: &crate::net::packet::RxFrame,
        now: Instant,
        guard: &Guard<'_>,
    ) -> Option<NetNamespaceForwardOutcome> {
        let ethernet = EthernetFrame::new_checked(frame.as_bytes()).ok()?;
        if !ingress.accepts_ethernet_destination_addr(EthernetAddress::new(ethernet.dst_addr().0)) {
            return None;
        }
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        if run_frame_hook_in_namespace(
            self,
            NetfilterFrameContext {
                hook: NetfilterHook::Prerouting,
                bridge: None,
                ingress: Some(ingress.name),
                egress: None,
            },
            ethernet.payload(),
        ) == NetfilterVerdict::Drop
        {
            return Some(NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            });
        }

        let prerouting = apply_prerouting_nat_ipv4_in_namespace(
            self,
            NetfilterFrameContext {
                hook: NetfilterHook::Prerouting,
                bridge: None,
                ingress: Some(ingress.name),
                egress: None,
            },
            ethernet.payload(),
        );
        let packet = prerouting.as_deref().unwrap_or_else(|| ethernet.payload());
        let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
        let dst = Ipv4Address::new(ipv4.dst_addr().octets());
        if prerouting.is_none() && self.is_local_ipv4_destination(dst) {
            return None;
        }
        if !self.ipv4_forwarding_enabled() {
            return Some(NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            });
        }
        Some(self.forward_ipv4_packet_from_iface(ingress.name, packet, dst, now, guard))
    }

    fn forward_ipv4_packet_from_iface(
        &self,
        ingress_name: &'static str,
        packet: &[u8],
        dst: Ipv4Address,
        now: Instant,
        guard: &Guard<'_>,
    ) -> NetNamespaceForwardOutcome {
        let Some(route) = self.best_ipv4_route(dst) else {
            return NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            };
        };
        if route.oif_name == ingress_name {
            return NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            };
        }
        let Some(egress) = self
            .configured_ether_ifaces()
            .into_iter()
            .find(|iface| iface.name == route.oif_name)
        else {
            return NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            };
        };

        if run_frame_hook_in_namespace(
            self,
            NetfilterFrameContext {
                hook: NetfilterHook::Forward,
                bridge: None,
                ingress: Some(ingress_name),
                egress: Some(egress.name),
            },
            packet,
        ) == NetfilterVerdict::Drop
        {
            return NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            };
        }

        let postrouting = apply_postrouting_nat_ipv4_in_namespace(
            self,
            NetfilterFrameContext {
                hook: NetfilterHook::Postrouting,
                bridge: None,
                ingress: Some(ingress_name),
                egress: Some(egress.name),
            },
            packet,
            egress.common.ipv4_addr(),
        );
        let packet = postrouting.as_deref().unwrap_or(packet);

        if run_frame_hook_in_namespace(
            self,
            NetfilterFrameContext {
                hook: NetfilterHook::Postrouting,
                bridge: None,
                ingress: Some(ingress_name),
                egress: Some(egress.name),
            },
            packet,
        ) == NetfilterVerdict::Drop
        {
            return NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            };
        }

        match egress.dispatch_ip_at(packet, now, guard) {
            PacketTxResult::Accepted { .. } => NetNamespaceForwardOutcome {
                forwarded: 1,
                ..NetNamespaceForwardOutcome::default()
            },
            PacketTxResult::PendingResolution { .. } => {
                let queued = self.enqueue_pending_ipv4_forward(egress.name, packet);
                NetNamespaceForwardOutcome {
                    pending_resolution: usize::from(queued),
                    dropped: usize::from(!queued),
                    ..NetNamespaceForwardOutcome::default()
                }
            }
            PacketTxResult::Busy | PacketTxResult::Failed { .. } => NetNamespaceForwardOutcome {
                dropped: 1,
                ..NetNamespaceForwardOutcome::default()
            },
        }
    }

    fn enqueue_pending_ipv4_forward(&self, egress_name: &'static str, packet: &[u8]) -> bool {
        let mut pending = self.pending_ipv4_forwards.lock();
        if pending.len() >= PENDING_IPV4_FORWARD_LIMIT {
            return false;
        }
        pending.push(PendingIpv4Forward {
            egress_name,
            packet: packet.to_vec(),
            attempts: 0,
        });
        true
    }

    fn retry_pending_ipv4_forwards(
        &self,
        now: Instant,
        guard: &Guard<'_>,
    ) -> NetNamespaceForwardOutcome {
        let mut current = {
            let mut pending = self.pending_ipv4_forwards.lock();
            if pending.is_empty() {
                return NetNamespaceForwardOutcome::default();
            }
            let mut current = Vec::new();
            core::mem::swap(&mut *pending, &mut current);
            current
        };

        let ifaces = self.configured_ether_ifaces();
        let mut keep = Vec::new();
        let mut outcome = NetNamespaceForwardOutcome::default();

        for mut forward in current.drain(..) {
            let Some(egress) = ifaces
                .iter()
                .copied()
                .find(|iface| iface.name == forward.egress_name)
            else {
                outcome.dropped += 1;
                continue;
            };

            match egress.dispatch_ip_at(&forward.packet, now, guard) {
                PacketTxResult::Accepted { .. } => {
                    outcome.forwarded += 1;
                }
                PacketTxResult::PendingResolution { .. } => {
                    forward.attempts = forward.attempts.saturating_add(1);
                    if forward.attempts >= PENDING_IPV4_FORWARD_RETRY_LIMIT {
                        outcome.dropped += 1;
                    } else {
                        outcome.pending_resolution += 1;
                        keep.push(forward);
                    }
                }
                PacketTxResult::Busy => {
                    forward.attempts = forward.attempts.saturating_add(1);
                    if forward.attempts >= PENDING_IPV4_FORWARD_RETRY_LIMIT {
                        outcome.dropped += 1;
                    } else {
                        keep.push(forward);
                    }
                }
                PacketTxResult::Failed { .. } => {
                    outcome.dropped += 1;
                }
            }
        }

        if !keep.is_empty() {
            let mut pending = self.pending_ipv4_forwards.lock();
            let available = PENDING_IPV4_FORWARD_LIMIT.saturating_sub(pending.len());
            let overflow = keep.len().saturating_sub(available);
            pending.extend(keep.into_iter().take(available));
            outcome.dropped += overflow;
        }

        outcome
    }

    fn is_local_ipv4_destination(&self, dst: Ipv4Address) -> bool {
        dst == Ipv4Address::BROADCAST || self.owns_ipv4_addr(dst)
    }

    fn poll_bridges(&self, guard: &Guard<'_>) -> BridgeForwardOutcome {
        let mut outcome = BridgeForwardOutcome::default();
        for registration in self.device_snapshot() {
            let Some(poll) = registration.ops.bridge_poll(guard) else {
                continue;
            };
            outcome.frames_seen += poll.frames_seen;
            outcome.learned += poll.learned;
            outcome.local_delivered += poll.local_delivered;
            outcome.forwarded += poll.forwarded;
            outcome.flooded += poll.flooded;
            outcome.dropped += poll.dropped;
            outcome.busy += poll.busy;
            outcome.tx_errors += poll.tx_errors;
        }
        outcome
    }
}

impl NetNamespaceRuntimeOutcome {
    pub fn made_progress(self) -> bool {
        self.bridge_frames_seen != 0
            || self.packets_seen != 0
            || self.device_tx_packets != 0
            || self.device_tx_pending_resolution != 0
            || self.arp_sent != 0
            || self.ipv4_forwarded != 0
            || self.ipv4_forward_pending_resolution != 0
            || self.sockets_touched != 0
            || self.wakes_fired != 0
    }

    pub fn merge(&mut self, other: Self) {
        self.namespaces_seen += other.namespaces_seen;
        self.ifaces_seen += other.ifaces_seen;
        self.bridge_polls += other.bridge_polls;
        self.bridge_frames_seen += other.bridge_frames_seen;
        self.bridge_forwarded += other.bridge_forwarded;
        self.bridge_local_delivered += other.bridge_local_delivered;
        self.packets_seen += other.packets_seen;
        self.sockets_touched += other.sockets_touched;
        self.wakes_fired += other.wakes_fired;
        self.device_tx_attempted += other.device_tx_attempted;
        self.device_tx_packets += other.device_tx_packets;
        self.device_tx_pending_resolution += other.device_tx_pending_resolution;
        self.device_tx_failed += other.device_tx_failed;
        self.arp_sent += other.arp_sent;
        self.ipv4_forwarded += other.ipv4_forwarded;
        self.ipv4_forward_pending_resolution += other.ipv4_forward_pending_resolution;
        self.ipv4_forward_dropped += other.ipv4_forward_dropped;
    }

    fn merge_bridge(&mut self, bridge: BridgeForwardOutcome) {
        self.bridge_polls += 1;
        self.bridge_frames_seen += bridge.frames_seen;
        self.bridge_forwarded += bridge.forwarded;
        self.bridge_local_delivered += bridge.local_delivered;
    }

    fn merge_device_tx(&mut self, device_tx: DeviceTxOutcome) {
        self.device_tx_attempted +=
            device_tx.tcp_attempted + device_tx.udp_attempted + device_tx.raw_icmp_attempted;
        self.device_tx_packets +=
            device_tx.tcp_packets + device_tx.udp_packets + device_tx.raw_icmp_packets;
        self.device_tx_pending_resolution += device_tx.tcp_resolution_pending
            + device_tx.udp_resolution_pending
            + device_tx.raw_icmp_resolution_pending;
        self.device_tx_failed +=
            device_tx.tcp_failed + device_tx.udp_failed + device_tx.raw_icmp_failed;
        self.sockets_touched += device_tx.sockets_touched;
        self.wakes_fired += device_tx.wakes_fired;
    }

    fn merge_arp_flush(&mut self, arp_flush: ArpFlushOutcome) {
        self.arp_sent += arp_flush.sent;
    }

    fn merge_forwarding(&mut self, forwarding: NetNamespaceForwardOutcome) {
        self.ipv4_forwarded += forwarding.forwarded;
        self.ipv4_forward_pending_resolution += forwarding.pending_resolution;
        self.ipv4_forward_dropped += forwarding.dropped;
    }
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

fn validate_route_config(route: NetNamespaceRouteConfig) -> Result<(), Errno> {
    if route.prefix_len > 32 {
        return Err(Errno::EINVAL);
    }
    if route.gateway.is_none() && route.oif_name.is_none() {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn route_matches_ipv4(route: NetNamespaceRouteInfo, dst: Ipv4Address) -> bool {
    ipv4_prefix_matches(dst, route.dst, route.prefix_len)
}

fn ipv4_network(addr: Ipv4Address, prefix_len: u8) -> Ipv4Address {
    let prefix_len = prefix_len.min(32);
    let mask = prefix_mask(prefix_len);
    Ipv4Address::new((ipv4_to_u32(addr) & mask).to_be_bytes())
}

fn ipv4_prefix_matches(addr: Ipv4Address, network: Ipv4Address, prefix_len: u8) -> bool {
    let mask = prefix_mask(prefix_len.min(32));
    (ipv4_to_u32(addr) & mask) == (ipv4_to_u32(network) & mask)
}

fn prefix_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    }
}

fn ipv4_to_u32(addr: Ipv4Address) -> u32 {
    u32::from_be_bytes(addr.octets())
}

impl NetNamespaceRouteEntry {
    fn from_config(config: NetNamespaceRouteConfig) -> Self {
        Self {
            dst: ipv4_network(config.dst, config.prefix_len),
            prefix_len: config.prefix_len,
            gateway: config.gateway,
            oif_name: config.oif_name,
            preferred_src: config.preferred_src,
            table: config.table,
            protocol: config.protocol,
            scope: config.scope,
            route_type: config.route_type,
        }
    }

    fn as_info(&self) -> NetNamespaceRouteInfo {
        NetNamespaceRouteInfo {
            kind: NetNamespaceRouteKind::Static,
            dst: self.dst,
            prefix_len: self.prefix_len,
            gateway: self.gateway,
            oif_name: self.oif_name,
            preferred_src: self.preferred_src,
            table: self.table,
            protocol: self.protocol,
            scope: self.scope,
            route_type: self.route_type,
        }
    }

    fn same_key(&self, other: Self) -> bool {
        self.dst == other.dst
            && self.prefix_len == other.prefix_len
            && self.gateway == other.gateway
            && self.oif_name == other.oif_name
            && self.table == other.table
    }

    fn matches_selector(&self, selector: NetNamespaceRouteSelector) -> bool {
        self.dst == ipv4_network(selector.dst, selector.prefix_len)
            && self.prefix_len == selector.prefix_len
            && self.table == selector.table
            && selector
                .gateway
                .is_none_or(|gateway| self.gateway == Some(gateway))
            && selector
                .oif_name
                .is_none_or(|oif_name| self.oif_name == Some(oif_name))
    }
}

impl NetNamespaceConnectedRouteKey {
    fn matches_selector(&self, selector: NetNamespaceRouteSelector) -> bool {
        self.dst == ipv4_network(selector.dst, selector.prefix_len)
            && self.prefix_len == selector.prefix_len
            && self.table == selector.table
            && selector.gateway.is_none()
            && selector
                .oif_name
                .is_none_or(|oif_name| self.oif_name == oif_name)
    }
}

// ---------------------------------------------------------------------------
// IPv6 routing helpers — parallel to the IPv4 helpers above. Prefix math is
// done on the 128-bit big-endian integer view of the address.
// ---------------------------------------------------------------------------

fn validate_route6_config(route: NetNamespaceRoute6Config) -> Result<(), Errno> {
    if route.prefix_len > 128 {
        return Err(Errno::EINVAL);
    }
    if route.gateway.is_none() && route.oif_name.is_none() {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn route6_matches_ipv6(route: NetNamespaceRoute6Info, dst: Ipv6Address) -> bool {
    ipv6_prefix_matches(dst, route.dst, route.prefix_len)
}

fn ipv6_prefix_mask(prefix_len: u8) -> u128 {
    if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix_len.min(128)))
    }
}

fn ipv6_network(addr: Ipv6Address, prefix_len: u8) -> Ipv6Address {
    let mask = ipv6_prefix_mask(prefix_len.min(128));
    Ipv6Address::new((u128::from_be_bytes(addr.octets()) & mask).to_be_bytes())
}

fn ipv6_prefix_matches(addr: Ipv6Address, network: Ipv6Address, prefix_len: u8) -> bool {
    let mask = ipv6_prefix_mask(prefix_len.min(128));
    (u128::from_be_bytes(addr.octets()) & mask) == (u128::from_be_bytes(network.octets()) & mask)
}

impl NetNamespaceRoute6Entry {
    fn from_config(config: NetNamespaceRoute6Config) -> Self {
        Self {
            dst: ipv6_network(config.dst, config.prefix_len),
            prefix_len: config.prefix_len,
            gateway: config.gateway,
            oif_name: config.oif_name,
            preferred_src: config.preferred_src,
            table: config.table,
            protocol: config.protocol,
            scope: config.scope,
            route_type: config.route_type,
        }
    }

    fn as_info(&self) -> NetNamespaceRoute6Info {
        NetNamespaceRoute6Info {
            kind: NetNamespaceRouteKind::Static,
            dst: self.dst,
            prefix_len: self.prefix_len,
            gateway: self.gateway,
            oif_name: self.oif_name,
            preferred_src: self.preferred_src,
            table: self.table,
            protocol: self.protocol,
            scope: self.scope,
            route_type: self.route_type,
        }
    }

    fn same_key(&self, other: Self) -> bool {
        self.dst == other.dst
            && self.prefix_len == other.prefix_len
            && self.gateway == other.gateway
            && self.oif_name == other.oif_name
            && self.table == other.table
    }

    fn matches_selector(&self, selector: NetNamespaceRoute6Selector) -> bool {
        self.dst == ipv6_network(selector.dst, selector.prefix_len)
            && self.prefix_len == selector.prefix_len
            && self.table == selector.table
            && selector
                .gateway
                .is_none_or(|gateway| self.gateway == Some(gateway))
            && selector
                .oif_name
                .is_none_or(|oif_name| self.oif_name == Some(oif_name))
    }
}

impl NetNamespaceConnectedRoute6Key {
    fn matches_selector(&self, selector: NetNamespaceRoute6Selector) -> bool {
        self.dst == ipv6_network(selector.dst, selector.prefix_len)
            && self.prefix_len == selector.prefix_len
            && self.table == selector.table
            && selector.gateway.is_none()
            && selector
                .oif_name
                .is_none_or(|oif_name| self.oif_name == oif_name)
    }
}

/// Drop runtime-list references to dead namespaces.
///
/// The list previously held a strong `PayloadCap` for every namespace ever
/// created and never released them, so each `unshare -n` permanently leaked a
/// ~256 KB `SocketTable` (the la net_stress walk OOM'd after a handful). A
/// namespace whose payload `retain_count()` is `1` is held ONLY by this list —
/// every identity, socket-registry, and process reference has already dropped,
/// so it is dead. Removing it drops the last strong ref, which retires the slot
/// (EBR) and runs `NetNamespacePayload::drop`, freeing the `SocketTable`. The
/// initial namespace is pinned by `INITIAL_NET_NAMESPACE` + boot state
/// (retain > 1) and its table is the shared static, so it is never pruned/freed.
fn prune_dead_net_namespaces(namespaces: &mut Vec<PayloadCap<NetNamespacePayload>>) {
    namespaces.retain(|namespace| namespace.retain_count() > 1);
}

fn remember_net_namespace_payload(payload: PayloadCap<NetNamespacePayload>) {
    let mut namespaces = NET_NAMESPACE_RUNTIME_LIST.lock();
    prune_dead_net_namespaces(&mut namespaces);
    if namespaces
        .iter()
        .any(|namespace| namespace.trace_id() == payload.trace_id())
    {
        return;
    }
    namespaces.push(payload);
}

pub fn net_namespace_payloads_snapshot() -> Vec<PayloadCap<NetNamespacePayload>> {
    let mut namespaces = NET_NAMESPACE_RUNTIME_LIST.lock();
    prune_dead_net_namespaces(&mut namespaces);
    namespaces.clone()
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    register_zone_for::<NetNamespaceIdentity>()?;
    register_zone_for::<NetNamespacePayload>()?;
    Ok(())
}

pub fn initial_net_namespace() -> Cap<NetNamespaceIdentity> {
    if let Some(namespace) = INITIAL_NET_NAMESPACE.lock().as_ref().cloned() {
        return namespace;
    }

    let namespace = create_initial_net_namespace().expect("initial net namespace zones registered");
    let mut slot = INITIAL_NET_NAMESPACE.lock();
    if let Some(existing) = slot.as_ref().cloned() {
        return existing;
    }
    *slot = Some(namespace.clone());
    namespace
}

pub fn initial_net_namespace_payload() -> PayloadCap<NetNamespacePayload> {
    initial_net_namespace()
        .payload_cap()
        .expect("initial net namespace payload installed")
}

/// Delete a neighbor entry described by a `/proc/net/tx_neigh_ctl` write line
/// (`"<addr> <dev>\n"`), used by the `ip neigh del` shim. Resolves `dev` to an
/// ifindex in the root netns and removes the static ARP (IPv4) or NDISC (IPv6)
/// entry. Returns true on success.
pub fn delete_neighbor_ctl(line: &[u8]) -> bool {
    let Ok(text) = core::str::from_utf8(line) else {
        return false;
    };
    let mut parts = text.split_whitespace();
    let (Some(addr_str), Some(dev)) = (parts.next(), parts.next()) else {
        return false;
    };
    let netns = initial_net_namespace_payload();
    let Some(ifindex) = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == dev)
        .map(|link| link.ifindex)
    else {
        return false;
    };
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    if addr_str.contains(':') {
        match parse_ipv6_ascii(addr_str) {
            Some(addr) => netns
                .delete_static_ndisc_by_ifindex(auth, ifindex, addr)
                .is_ok(),
            None => false,
        }
    } else {
        match parse_ipv4_ascii(addr_str) {
            Some(addr) => netns
                .delete_static_neighbor_by_ifindex(auth, ifindex, addr)
                .is_ok(),
            None => false,
        }
    }
}

fn parse_ipv4_ascii(s: &str) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut parts = s.split('.');
    for octet in &mut octets {
        *octet = parts.next()?.parse::<u8>().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Ipv4Address::new(octets))
}

fn parse_ipv6_ascii(s: &str) -> Option<Ipv6Address> {
    let mut octets = [0u8; 16];
    let put = |octets: &mut [u8; 16], pos: usize, g: &str| -> Option<usize> {
        let v = u16::from_str_radix(g, 16).ok()?;
        octets[pos] = (v >> 8) as u8;
        octets[pos + 1] = v as u8;
        Some(pos + 2)
    };
    if let Some(idx) = s.find("::") {
        let (left, right) = (&s[..idx], &s[idx + 2..]);
        let lgroups: Vec<&str> = if left.is_empty() {
            Vec::new()
        } else {
            left.split(':').collect()
        };
        let rgroups: Vec<&str> = if right.is_empty() {
            Vec::new()
        } else {
            right.split(':').collect()
        };
        if lgroups.len() + rgroups.len() > 7 {
            return None;
        }
        let mut pos = 0usize;
        for g in &lgroups {
            pos = put(&mut octets, pos, g)?;
        }
        let mut rpos = 16 - rgroups.len() * 2;
        for g in &rgroups {
            rpos = put(&mut octets, rpos, g)?;
        }
        Some(Ipv6Address::new(octets))
    } else {
        let groups: Vec<&str> = s.split(':').collect();
        if groups.len() != 8 {
            return None;
        }
        let mut pos = 0usize;
        for g in &groups {
            pos = put(&mut octets, pos, g)?;
        }
        Some(Ipv6Address::new(octets))
    }
}

pub fn initial_net_namespace_payload_with_owner(
    owner_user_ns: Cap<UserNamespace>,
) -> PayloadCap<NetNamespacePayload> {
    let payload = initial_net_namespace_payload();
    payload.install_owner_user_namespace_once(owner_user_ns);
    payload
}

pub const fn initial_socket_table() -> &'static SocketTable {
    &INITIAL_SOCKET_TABLE
}

pub fn initial_loopback_iface() -> &'static LoopbackIface {
    initial_net_namespace_payload().loopback_iface()
}

pub fn net_namespace_open_file_from_payload(
    payload: PayloadCap<NetNamespacePayload>,
) -> Result<Cap<OpenFile>, Errno> {
    let rnode = RNode::new_cap(
        allocate_netns_fs_object_id(),
        InodeMeta::new(InodeKind::Regular, 0o400),
        RNodeBacking::StructBacked {
            payload: StructPayload::NetNamespace {
                payload: payload.clone(),
            },
        },
    )
    .map_err(|_| Errno::ENOMEM)?;

    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .map_err(|_| Errno::ENOMEM)
}

pub fn net_namespace_payload_from_file(
    file: &Cap<OpenFile>,
) -> Option<PayloadCap<NetNamespacePayload>> {
    match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::NetNamespace { payload },
        } => Some(payload.clone()),
        _ => None,
    }
}

fn allocate_netns_fs_object_id() -> FsObjectId {
    FsObjectId::new(NEXT_NETNS_FS_OBJECT_ID.fetch_add(1, Ordering::AcqRel))
}

fn create_initial_net_namespace() -> Result<Cap<NetNamespaceIdentity>, ZoneError> {
    create_net_namespace("init", NetNamespacePayload::new_initial())
}

fn initial_socket_table_for_namespace() -> &'static SocketTable {
    #[cfg(any(test, feature = "test-support"))]
    {
        Box::leak(Box::new(SocketTable::new()))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        &INITIAL_SOCKET_TABLE
    }
}

fn create_net_namespace(
    name: &'static str,
    payload: NetNamespacePayload,
) -> Result<Cap<NetNamespaceIdentity>, ZoneError> {
    let identity_res = zone::reserve_for::<NetNamespaceIdentity>()?;
    let payload_res = zone::reserve_for::<NetNamespacePayload>()?;
    let identity = zone::sign_for(identity_res, NetNamespaceIdentity::new(name));
    let payload = PayloadCap::from_cap(zone::sign_for(payload_res, payload));
    identity.install_payload(payload.clone());
    remember_net_namespace_payload(payload);
    Ok(identity)
}

pub fn create_isolated_net_namespace(
    name: &'static str,
) -> Result<Cap<NetNamespaceIdentity>, ZoneError> {
    create_isolated_net_namespace_with_owner(name, None)
}

pub fn create_isolated_net_namespace_with_owner(
    name: &'static str,
    owner_user_ns: Option<Cap<UserNamespace>>,
) -> Result<Cap<NetNamespaceIdentity>, ZoneError> {
    // A `SocketTable` is large (one `Index` per protocol, each with hundreds of
    // slots). `Box::new(SocketTable::new())` materialises the whole table as a
    // stack temporary before moving it into the heap; if the slot counts grow
    // (e.g. `UNIX_STREAM_PEER_SLOTS`), that temporary can overflow the boot
    // stack into adjacent `.bss` and silently corrupt the `BootStaticBag`
    // (kernel panic "BootStaticBag not constructed" — see git history). Allocate
    // the table directly on the heap, zeroed, to avoid the stack temporary (and
    // the matching memcpy) entirely: every field of an empty table is all-zero
    // bytes — each `Index`/`Entry` starts in state `EMPTY` (0) with an unlocked
    // `SpinLock` (`false`/0) and an uninitialised key/value — so a zeroed
    // allocation is a valid `SocketTable::new()`.
    let table: &'static mut SocketTable = {
        let layout = core::alloc::Layout::new::<SocketTable>();
        // SAFETY: `SocketTable` has non-zero size; a zeroed `SocketTable` is a
        // valid empty table (see the comment above).
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut SocketTable;
        if ptr.is_null() {
            alloc::alloc::handle_alloc_error(layout);
        }
        // SAFETY: `ptr` is a fresh, properly-aligned, zero-initialised
        // allocation sized for `SocketTable`; leaked for the namespace lifetime
        // to match the previous `Box::leak`.
        unsafe { &mut *ptr }
    };
    create_net_namespace(
        name,
        NetNamespacePayload::new(owner_user_ns, table, true, loopback_iface(), false),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub fn create_isolated_net_namespace_for_test(
    name: &'static str,
) -> Result<Cap<NetNamespaceIdentity>, ZoneError> {
    create_isolated_net_namespace(name)
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_initial_net_namespace_for_test() {
    *INITIAL_NET_NAMESPACE.lock() = None;
    NET_NAMESPACE_RUNTIME_LIST.lock().clear();
}
