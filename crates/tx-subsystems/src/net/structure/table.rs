use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_substrate::epoch::Guard;
use tx_substrate::index::{Index, IndexError, IndexReservation};
use tx_substrate::mutation::{self, MutationError, WithdrawReservation};
use tx_substrate::zone::Cap;

use super::identity::SocketIdentity;
use super::types::{AddressFamily, IpEndpoint, Ipv4Address, Ipv6Address, UnixSocketPath};

const LOCAL_ENDPOINT_SLOTS: usize = 256;
const LISTENER_SLOTS: usize = 128;
const CONNECTION_SLOTS: usize = 256;
const RAW_ICMP_SLOTS: usize = 128;
const PACKET_SOCKET_SLOTS: usize = 128;
const UNIX_PATH_NODE_SLOTS: usize = 256;
const UNIX_BOUND_SLOTS: usize = 256;
// hackbench (cyclictest's stress phase) creates hundreds of AF_UNIX
// socketpairs concurrently; each pair inserts two peer entries. main bumped
// this to 4096 to clear an `insert_unix_peer` ENOMEM ("Creating fdpair
// (error: Out of memory)"). But each net namespace owns a whole `SocketTable`
// and the per-namespace table is currently `Box::leak`-ed (never freed), so a
// 4096-slot index makes every namespace leak ~500KB; a handful of LTP net
// tests (each in its own netns) then exhaust the kernel heap
// ("memory allocation of N bytes failed"). Until the namespace teardown frees
// its `SocketTable`, keep the feature-tested 256 (the leak stays at ~30KB/ns).
// Re-raising this requires fixing the per-namespace `SocketTable` leak first.
const UNIX_STREAM_PEER_SLOTS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalEndpointKey {
    pub family: AddressFamily,
    pub addr: Ipv4Address,
    pub addr6: Ipv6Address,
    pub port: u16,
}

impl LocalEndpointKey {
    pub const fn new(endpoint: IpEndpoint) -> Self {
        Self {
            family: endpoint.family,
            addr: endpoint.addr,
            addr6: endpoint.addr6,
            port: endpoint.port,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionKey {
    pub local_family: AddressFamily,
    pub local_addr: Ipv4Address,
    pub local_addr6: Ipv6Address,
    pub local_port: u16,
    pub remote_family: AddressFamily,
    pub remote_addr: Ipv4Address,
    pub remote_addr6: Ipv6Address,
    pub remote_port: u16,
}

impl ConnectionKey {
    pub const fn new(local: IpEndpoint, remote: IpEndpoint) -> Self {
        Self {
            local_family: local.family,
            local_addr: local.addr,
            local_addr6: local.addr6,
            local_port: local.port,
            remote_family: remote.family,
            remote_addr: remote.addr,
            remote_addr6: remote.addr6,
            remote_port: remote.port,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenerKey {
    pub local_family: AddressFamily,
    pub local_addr: Ipv4Address,
    pub local_addr6: Ipv6Address,
    pub local_port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawIcmpSocketKey {
    pub socket_raw: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketSocketKey {
    pub socket_raw: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixStreamPeerKey {
    pub socket_raw: u32,
}

impl ListenerKey {
    pub const fn exact(addr: IpEndpoint) -> Self {
        Self {
            local_family: addr.family,
            local_addr: addr.addr,
            local_addr6: addr.addr6,
            local_port: addr.port,
        }
    }

    pub const fn wildcard_for_family(family: AddressFamily, port: u16) -> Self {
        Self {
            local_family: family,
            local_addr: Ipv4Address::UNSPECIFIED,
            local_addr6: Ipv6Address::UNSPECIFIED,
            local_port: port,
        }
    }

    pub const fn from_endpoint(endpoint: IpEndpoint) -> Self {
        Self::exact(endpoint)
    }
}

pub struct SocketTable {
    tcp_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    tcp_listeners: Index<ListenerKey, Cap<SocketIdentity>, LISTENER_SLOTS>,
    tcp_connections: Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    sctp_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    sctp_listeners: Index<ListenerKey, Cap<SocketIdentity>, LISTENER_SLOTS>,
    sctp_connections: Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    rds_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    udp_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    udp_connections: Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    raw_icmp: Index<RawIcmpSocketKey, Cap<SocketIdentity>, RAW_ICMP_SLOTS>,
    packet_sockets: Index<PacketSocketKey, Cap<SocketIdentity>, PACKET_SOCKET_SLOTS>,
    unix_path_nodes: Index<UnixSocketPath, (), UNIX_PATH_NODE_SLOTS>,
    unix_bound: Index<UnixSocketPath, Cap<SocketIdentity>, UNIX_BOUND_SLOTS>,
    unix_stream_peers: Index<UnixStreamPeerKey, Cap<SocketIdentity>, UNIX_STREAM_PEER_SLOTS>,
    tcp_loopback_poll_cursor: AtomicUsize,
}

pub(crate) struct TcpDisconnectIndexReservations<'a> {
    forward: Option<WithdrawReservation<'a, ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>>,
    reverse: Option<TcpReverseIndexReservation<'a>>,
    bound: Option<
        WithdrawReservation<'a, LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    >,
}

enum TcpReverseIndexReservation<'a> {
    Vacant(IndexReservation<'a, ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>),
    Occupied(WithdrawReservation<'a, ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>),
}

impl TcpDisconnectIndexReservations<'_> {
    pub(crate) fn has_owned_forward(&self) -> bool {
        self.forward.is_some()
    }

    pub(crate) fn reverse_socket(&self) -> Option<Cap<SocketIdentity>> {
        match self.reverse.as_ref()? {
            TcpReverseIndexReservation::Vacant(_) => None,
            TcpReverseIndexReservation::Occupied(reservation) => {
                reservation.value().try_clone_live()
            }
        }
    }

    pub(crate) fn commit(&mut self, withdraw_reverse: bool) {
        if let Some(reservation) = self.forward.take() {
            let _ = reservation.withdraw();
        }
        if let Some(reverse) = self.reverse.take() {
            match reverse {
                TcpReverseIndexReservation::Occupied(reservation) if withdraw_reverse => {
                    let _ = reservation.withdraw();
                }
                TcpReverseIndexReservation::Vacant(reservation) => drop(reservation),
                TcpReverseIndexReservation::Occupied(reservation) => drop(reservation),
            }
        }
        if let Some(reservation) = self.bound.take() {
            let _ = reservation.withdraw();
        }
    }
}

impl SocketTable {
    fn lookup_cap<K: Eq, const N: usize>(
        index: &Index<K, Cap<SocketIdentity>, N>,
        key: &K,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        let raw = index.lookup_project(key, Cap::raw)?;
        Cap::try_clone_raw_live(raw, guard)
    }

    pub const fn new() -> Self {
        Self {
            tcp_bound: Index::new(),
            tcp_listeners: Index::new(),
            tcp_connections: Index::new(),
            sctp_bound: Index::new(),
            sctp_listeners: Index::new(),
            sctp_connections: Index::new(),
            rds_bound: Index::new(),
            udp_bound: Index::new(),
            udp_connections: Index::new(),
            raw_icmp: Index::new(),
            packet_sockets: Index::new(),
            unix_path_nodes: Index::new(),
            unix_bound: Index::new(),
            unix_stream_peers: Index::new(),
            tcp_loopback_poll_cursor: AtomicUsize::new(0),
        }
    }

    pub fn bind_tcp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.tcp_bound
            .reserve(LocalEndpointKey::new(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn bind_udp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.udp_bound
            .reserve(LocalEndpointKey::new(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn bind_sctp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.sctp_bound
            .reserve(LocalEndpointKey::new(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn bind_rds(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.rds_bound
            .reserve(LocalEndpointKey::new(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn listen_tcp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.tcp_listeners
            .reserve(ListenerKey::from_endpoint(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn listen_sctp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.sctp_listeners
            .reserve(ListenerKey::from_endpoint(endpoint))?
            .commit(socket);
        Ok(())
    }

    pub fn insert_tcp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.tcp_connections.reserve(key)?.commit(socket);
        Ok(())
    }

    pub fn insert_sctp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.sctp_connections.reserve(key)?.commit(socket);
        Ok(())
    }

    pub fn insert_sctp_connection_pair(
        &self,
        first_key: ConnectionKey,
        first_socket: Cap<SocketIdentity>,
        second_key: ConnectionKey,
        second_socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        let first = self.sctp_connections.reserve(first_key)?;
        let second = self.sctp_connections.reserve(second_key)?;
        first.commit(first_socket);
        second.commit(second_socket);
        Ok(())
    }

    pub fn insert_udp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.udp_connections.reserve(key)?.commit(socket);
        Ok(())
    }

    pub fn register_raw_icmp(&self, socket: Cap<SocketIdentity>) -> Result<(), IndexError> {
        self.raw_icmp
            .reserve(RawIcmpSocketKey {
                socket_raw: socket.raw(),
            })?
            .commit(socket);
        Ok(())
    }

    pub fn register_packet_socket(&self, socket: Cap<SocketIdentity>) -> Result<(), IndexError> {
        self.packet_sockets
            .reserve(PacketSocketKey {
                socket_raw: socket.raw(),
            })?
            .commit(socket);
        Ok(())
    }

    pub fn bind_unix(
        &self,
        path: UnixSocketPath,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        let path_node = self.unix_path_nodes.reserve(path)?;
        let bound = self.unix_bound.reserve(path)?;
        path_node.commit(());
        bound.commit(socket);
        Ok(())
    }

    pub fn insert_unix_stream_peer(
        &self,
        socket_raw: u32,
        peer: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.insert_unix_peer(socket_raw, peer)
    }

    pub fn insert_unix_peer(
        &self,
        socket_raw: u32,
        peer: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.unix_stream_peers
            .reserve(UnixStreamPeerKey { socket_raw })?
            .commit(peer);
        Ok(())
    }

    pub fn insert_tcp_connection_pair(
        &self,
        first_key: ConnectionKey,
        first_socket: Cap<SocketIdentity>,
        second_key: ConnectionKey,
        second_socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        let first = self.tcp_connections.reserve(first_key)?;
        let second = self.tcp_connections.reserve(second_key)?;
        first.commit(first_socket);
        second.commit(second_socket);
        Ok(())
    }

    pub fn withdraw_tcp_connection(
        &self,
        key: ConnectionKey,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.tcp_connections, &key)
    }

    pub fn withdraw_tcp_connection_if_owner(
        &self,
        key: ConnectionKey,
        socket_raw: u32,
    ) -> Result<Option<Cap<SocketIdentity>>, MutationError> {
        mutation::withdraw_if(&self.tcp_connections, &key, |socket| {
            socket.raw() == socket_raw
        })
    }

    pub(crate) fn reserve_tcp_disconnect_if_owners(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        forward_owner: u32,
    ) -> Result<TcpDisconnectIndexReservations<'_>, MutationError> {
        let forward = reserve_owned_socket(
            &self.tcp_connections,
            &ConnectionKey::new(local, remote),
            forward_owner,
        )?;
        let reverse = Some(reserve_tcp_reverse_slot(
            &self.tcp_connections,
            remote,
            local,
        )?);
        let bound = reserve_owned_socket(
            &self.tcp_bound,
            &LocalEndpointKey::new(local),
            forward_owner,
        )?;
        Ok(TcpDisconnectIndexReservations {
            forward,
            reverse,
            bound,
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve_tcp_connection_if_owner_for_test(
        &self,
        key: ConnectionKey,
        owner: u32,
    ) -> Result<
        Option<WithdrawReservation<'_, ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>>,
        MutationError,
    > {
        reserve_owned_socket(&self.tcp_connections, &key, owner)
    }

    pub fn withdraw_sctp_connection(
        &self,
        key: ConnectionKey,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.sctp_connections, &key)
    }

    pub fn withdraw_tcp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.tcp_bound, &LocalEndpointKey::new(endpoint))
    }

    pub fn withdraw_tcp_bound_if_owner(
        &self,
        endpoint: IpEndpoint,
        socket_raw: u32,
    ) -> Result<Option<Cap<SocketIdentity>>, MutationError> {
        mutation::withdraw_if(
            &self.tcp_bound,
            &LocalEndpointKey::new(endpoint),
            |socket| socket.raw() == socket_raw,
        )
    }

    pub fn withdraw_sctp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.sctp_bound, &LocalEndpointKey::new(endpoint))
    }

    pub fn withdraw_rds_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.rds_bound, &LocalEndpointKey::new(endpoint))
    }

    pub fn withdraw_tcp_listener(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.tcp_listeners, &ListenerKey::from_endpoint(endpoint))
    }

    pub fn withdraw_sctp_listener(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.sctp_listeners, &ListenerKey::from_endpoint(endpoint))
    }

    pub fn withdraw_udp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.udp_bound, &LocalEndpointKey::new(endpoint))
    }

    pub fn withdraw_udp_connection(
        &self,
        key: ConnectionKey,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.udp_connections, &key)
    }

    pub fn withdraw_raw_icmp(&self, socket_raw: u32) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.raw_icmp, &RawIcmpSocketKey { socket_raw })
    }

    pub fn withdraw_packet_socket(
        &self,
        socket_raw: u32,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.packet_sockets, &PacketSocketKey { socket_raw })
    }

    pub fn withdraw_unix_bound(
        &self,
        path: UnixSocketPath,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.unix_bound, &path)
    }

    pub fn unlink_unix_path(&self, path: UnixSocketPath) -> Result<(), MutationError> {
        mutation::withdraw(&self.unix_path_nodes, &path)?;
        let _ = mutation::withdraw(&self.unix_bound, &path);
        Ok(())
    }

    pub fn withdraw_unix_stream_peer(
        &self,
        socket_raw: u32,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.withdraw_unix_peer(socket_raw)
    }

    pub fn withdraw_unix_peer(
        &self,
        socket_raw: u32,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.unix_stream_peers, &UnixStreamPeerKey { socket_raw })
    }

    pub fn lookup_tcp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.tcp_bound, &LocalEndpointKey::new(endpoint), guard)
    }

    pub fn lookup_sctp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.sctp_bound, &LocalEndpointKey::new(endpoint), guard)
    }

    pub fn lookup_rds_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.rds_bound, &LocalEndpointKey::new(endpoint), guard)
    }

    pub fn lookup_tcp_listener(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.lookup_tcp_listener_endpoint(endpoint, guard)
    }

    pub fn lookup_sctp_listener(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.lookup_sctp_listener_endpoint(endpoint, guard)
    }

    pub fn lookup_tcp_listener_endpoint(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        let exact = ListenerKey::from_endpoint(endpoint);
        if let Some(socket) = Self::lookup_cap(&self.tcp_listeners, &exact, guard) {
            return Some(socket);
        }
        let wildcard = ListenerKey::wildcard_for_family(endpoint.family, endpoint.port);
        Self::lookup_cap(&self.tcp_listeners, &wildcard, guard)
    }

    pub fn lookup_sctp_listener_endpoint(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        let exact = ListenerKey::from_endpoint(endpoint);
        if let Some(socket) = Self::lookup_cap(&self.sctp_listeners, &exact, guard) {
            return Some(socket);
        }
        let wildcard = ListenerKey::wildcard_for_family(endpoint.family, endpoint.port);
        Self::lookup_cap(&self.sctp_listeners, &wildcard, guard)
    }

    pub fn lookup_tcp_listener_dual_stack_endpoint(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        if let Some(socket) = self.lookup_tcp_listener_endpoint(endpoint, guard) {
            return Some(socket);
        }
        if endpoint.family != AddressFamily::Inet {
            return None;
        }
        let wildcard6 = ListenerKey::wildcard_for_family(AddressFamily::Inet6, endpoint.port);
        Self::lookup_cap(&self.tcp_listeners, &wildcard6, guard)
    }

    pub fn lookup_sctp_listener_dual_stack_endpoint(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        if let Some(socket) = self.lookup_sctp_listener_endpoint(endpoint, guard) {
            return Some(socket);
        }
        if endpoint.family != AddressFamily::Inet {
            return None;
        }
        let wildcard6 = ListenerKey::wildcard_for_family(AddressFamily::Inet6, endpoint.port);
        Self::lookup_cap(&self.sctp_listeners, &wildcard6, guard)
    }

    pub fn lookup_tcp_listener_addr(
        &self,
        addr: Ipv4Address,
        port: u16,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.lookup_tcp_listener_endpoint(IpEndpoint::new(addr, port), guard)
    }

    pub fn lookup_tcp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.tcp_connections, &key, guard)
    }

    pub fn lookup_sctp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.sctp_connections, &key, guard)
    }

    pub fn lookup_udp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.udp_connections, &key, guard)
    }

    pub fn lookup_udp_ingress(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        let exact = ConnectionKey::new(dst, src);
        if let Some(socket) = self.lookup_udp_connection(exact, guard) {
            return Some(socket);
        }

        let wildcard_local = ConnectionKey::new(
            IpEndpoint::unspecified_for_family(dst.family, dst.port),
            src,
        );
        if let Some(socket) = self.lookup_udp_connection(wildcard_local, guard) {
            return Some(socket);
        }

        // Dual-stack: an IPv6 socket bound to `[::]` and connect()ed to an IPv4
        // peer keys its connection with an IPv6 wildcard local, but an inbound
        // IPv4 datagram looks up with the IPv4 destination. Match it against the
        // `[::]:port` connected key so the server keeps receiving after it
        // connect()s back to the client (iperf3's UDP server connect()s its data
        // socket to the peer it just heard from; without this the post-connect
        // datagrams are dropped and the server reports 0 bytes received).
        if dst.family == AddressFamily::Inet {
            let wildcard6_local = ConnectionKey::new(
                IpEndpoint::unspecified_for_family(AddressFamily::Inet6, dst.port),
                src,
            );
            if let Some(socket) = self.lookup_udp_connection(wildcard6_local, guard) {
                return Some(socket);
            }
        }

        self.lookup_udp_bound(dst, guard)
    }

    pub fn lookup_unix_bound(
        &self,
        path: UnixSocketPath,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.unix_bound, &path, guard)
    }

    pub fn lookup_unix_path_node(&self, path: UnixSocketPath, guard: &Guard<'_>) -> bool {
        self.unix_path_nodes.lookup(&path, guard).is_some()
    }

    pub fn lookup_unix_stream_peer(
        &self,
        socket_raw: u32,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.lookup_unix_peer(socket_raw, guard)
    }

    pub fn lookup_unix_peer(
        &self,
        socket_raw: u32,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(
            &self.unix_stream_peers,
            &UnixStreamPeerKey { socket_raw },
            guard,
        )
    }

    pub fn snapshot_tcp_listeners(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_listeners
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_sctp_listeners(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.sctp_listeners
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_tcp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_sctp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.sctp_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_rds_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.rds_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_tcp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_connections
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    /// Claim the start of the next bounded loopback TCP poll window.
    ///
    /// The connection index is stable-slot ordered, so repeatedly taking its
    /// first `budget` entries can permanently starve later active flows.
    /// Advancing a per-table cursor makes consecutive delegate passes cover
    /// every active candidate without introducing a global scheduler lock.
    pub fn claim_tcp_loopback_poll_start(
        &self,
        candidate_count: usize,
        window_len: usize,
    ) -> usize {
        if candidate_count == 0 {
            return 0;
        }
        self.tcp_loopback_poll_cursor
            .fetch_add(window_len.max(1), Ordering::Relaxed)
            % candidate_count
    }

    pub fn snapshot_sctp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.sctp_connections
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_udp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.udp_connections
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn lookup_udp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        if let Some(socket) = self.lookup_udp_bound_exact(endpoint, guard) {
            return Some(socket);
        }
        if let Some(socket) = self.lookup_udp_bound_wildcard(endpoint, guard) {
            return Some(socket);
        }
        // Dual-stack: an inbound IPv4 datagram also matches an IPv6 socket bound
        // to the unspecified address (`[::]`) on the same port (default
        // `IPV6_V6ONLY=0`), via v4-mapped delivery. Mirrors
        // `lookup_tcp_listener_dual_stack_endpoint` — without it the TCP control
        // path accepts a v4 client onto a `[::]` listener but the UDP data path
        // drops the v4 datagram. iperf3 binds its UDP data socket to `[::]:PORT`
        // and the IPv4 client sends to `127.0.0.1:PORT`.
        if endpoint.family == AddressFamily::Inet {
            return self.lookup_udp_bound_wildcard6(endpoint.port, guard);
        }
        None
    }

    /// IPv6 unspecified (`[::]`) wildcard match on `port`, for v4-mapped
    /// delivery of an inbound IPv4 datagram to a dual-stack socket.
    pub fn lookup_udp_bound_wildcard6(
        &self,
        port: u16,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(
            &self.udp_bound,
            &LocalEndpointKey {
                family: AddressFamily::Inet6,
                addr: Ipv4Address::UNSPECIFIED,
                addr6: Ipv6Address::UNSPECIFIED,
                port,
            },
            guard,
        )
    }

    pub fn lookup_udp_bound_exact(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(&self.udp_bound, &LocalEndpointKey::new(endpoint), guard)
    }

    pub fn lookup_udp_bound_wildcard(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        Self::lookup_cap(
            &self.udp_bound,
            &LocalEndpointKey {
                family: endpoint.family,
                addr: Ipv4Address::UNSPECIFIED,
                addr6: Ipv6Address::UNSPECIFIED,
                port: endpoint.port,
            },
            guard,
        )
    }

    pub fn snapshot_udp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.udp_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_raw_icmp(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.raw_icmp
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_packet_sockets(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.packet_sockets
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_unix_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.unix_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }
}

fn reserve_owned_socket<'a, K: Eq, const N: usize>(
    index: &'a Index<K, Cap<SocketIdentity>, N>,
    key: &K,
    owner: u32,
) -> Result<Option<WithdrawReservation<'a, K, Cap<SocketIdentity>, N>>, MutationError> {
    match mutation::reserve_withdraw_if(index, key, |socket| socket.raw() == owner) {
        Ok(Some(reservation)) => Ok(Some(reservation)),
        // An occupied key owned by a replacement flow is a conflict, not an
        // optional absence. Let the caller roll the whole disconnect
        // transaction back rather than clearing stale local state around the
        // replacement's indexes.
        Ok(None) => Err(MutationError::AlreadyPresent),
        Err(MutationError::Missing) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reserve_tcp_reverse_slot(
    index: &Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> Result<TcpReverseIndexReservation<'_>, MutationError> {
    let key = ConnectionKey::new(local, remote);
    match mutation::reserve_withdraw_if(index, &key, |_| true) {
        Ok(Some(reservation)) => Ok(TcpReverseIndexReservation::Occupied(reservation)),
        Ok(None) => unreachable!("unconditional reverse reservation predicate"),
        Err(MutationError::Missing) => index
            .reserve(key)
            .map(TcpReverseIndexReservation::Vacant)
            .map_err(|_| MutationError::Busy),
        Err(error) => Err(error),
    }
}

impl Default for SocketTable {
    fn default() -> Self {
        Self::new()
    }
}

pub struct InitialSocketTableProxy;

impl InitialSocketTableProxy {
    pub fn as_table(&self) -> &'static SocketTable {
        crate::net::namespace::initial_net_namespace_payload().socket_table()
    }

    fn with_table<R>(&self, f: impl FnOnce(&SocketTable) -> R) -> R {
        let namespace = crate::net::namespace::initial_net_namespace_payload();
        f(namespace.socket_table())
    }

    pub fn bind_tcp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.bind_tcp(endpoint, socket))
    }

    pub fn bind_udp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.bind_udp(endpoint, socket))
    }

    pub fn listen_tcp(
        &self,
        endpoint: IpEndpoint,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.listen_tcp(endpoint, socket))
    }

    pub fn insert_tcp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.insert_tcp_connection(key, socket))
    }

    pub fn insert_udp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.insert_udp_connection(key, socket))
    }

    pub fn register_raw_icmp(&self, socket: Cap<SocketIdentity>) -> Result<(), IndexError> {
        self.with_table(|table| table.register_raw_icmp(socket))
    }

    pub fn register_packet_socket(&self, socket: Cap<SocketIdentity>) -> Result<(), IndexError> {
        self.with_table(|table| table.register_packet_socket(socket))
    }

    pub fn bind_unix(
        &self,
        path: UnixSocketPath,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.bind_unix(path, socket))
    }

    pub fn insert_unix_stream_peer(
        &self,
        socket_raw: u32,
        peer: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| table.insert_unix_stream_peer(socket_raw, peer))
    }

    pub fn insert_tcp_connection_pair(
        &self,
        first_key: ConnectionKey,
        first_socket: Cap<SocketIdentity>,
        second_key: ConnectionKey,
        second_socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.with_table(|table| {
            table.insert_tcp_connection_pair(first_key, first_socket, second_key, second_socket)
        })
    }

    pub fn withdraw_tcp_connection(
        &self,
        key: ConnectionKey,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_tcp_connection(key))
    }

    pub fn withdraw_tcp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_tcp_bound(endpoint))
    }

    pub fn withdraw_tcp_listener(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_tcp_listener(endpoint))
    }

    pub fn withdraw_udp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_udp_bound(endpoint))
    }

    pub fn withdraw_udp_connection(
        &self,
        key: ConnectionKey,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_udp_connection(key))
    }

    pub fn withdraw_raw_icmp(&self, socket_raw: u32) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_raw_icmp(socket_raw))
    }

    pub fn withdraw_packet_socket(
        &self,
        socket_raw: u32,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_packet_socket(socket_raw))
    }

    pub fn withdraw_unix_bound(
        &self,
        path: UnixSocketPath,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_unix_bound(path))
    }

    pub fn unlink_unix_path(&self, path: UnixSocketPath) -> Result<(), MutationError> {
        self.with_table(|table| table.unlink_unix_path(path))
    }

    pub fn withdraw_unix_stream_peer(
        &self,
        socket_raw: u32,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        self.with_table(|table| table.withdraw_unix_stream_peer(socket_raw))
    }

    pub fn lookup_tcp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_tcp_bound(endpoint, guard))
    }

    pub fn lookup_tcp_listener(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_tcp_listener(endpoint, guard))
    }

    pub fn lookup_tcp_listener_endpoint(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_tcp_listener_endpoint(endpoint, guard))
    }

    pub fn lookup_tcp_listener_addr(
        &self,
        addr: Ipv4Address,
        port: u16,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_tcp_listener_addr(addr, port, guard))
    }

    pub fn lookup_tcp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_tcp_connection(key, guard))
    }

    pub fn lookup_udp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_udp_connection(key, guard))
    }

    pub fn lookup_udp_ingress(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_udp_ingress(src, dst, guard))
    }

    pub fn lookup_unix_bound(
        &self,
        path: UnixSocketPath,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_unix_bound(path, guard))
    }

    pub fn lookup_unix_path_node(&self, path: UnixSocketPath, guard: &Guard<'_>) -> bool {
        self.with_table(|table| table.lookup_unix_path_node(path, guard))
    }

    pub fn lookup_unix_stream_peer(
        &self,
        socket_raw: u32,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_unix_stream_peer(socket_raw, guard))
    }

    pub fn snapshot_tcp_listeners(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_tcp_listeners(guard))
    }

    pub fn snapshot_tcp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_tcp_bound(guard))
    }

    pub fn snapshot_tcp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_tcp_connections(guard))
    }

    pub fn snapshot_udp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_udp_connections(guard))
    }

    pub fn lookup_udp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_udp_bound(endpoint, guard))
    }

    pub fn lookup_udp_bound_exact(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_udp_bound_exact(endpoint, guard))
    }

    pub fn lookup_udp_bound_wildcard(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.with_table(|table| table.lookup_udp_bound_wildcard(endpoint, guard))
    }

    pub fn snapshot_udp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_udp_bound(guard))
    }

    pub fn snapshot_raw_icmp(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_raw_icmp(guard))
    }

    pub fn snapshot_packet_sockets(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_packet_sockets(guard))
    }

    pub fn snapshot_unix_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.with_table(|table| table.snapshot_unix_bound(guard))
    }
}

pub static SOCKET_TABLE: InitialSocketTableProxy = InitialSocketTableProxy;
