use alloc::vec::Vec;
use tx_substrate::epoch::Guard;
use tx_substrate::index::{Index, IndexError};
use tx_substrate::mutation::{self, MutationError};
use tx_substrate::zone::Cap;

use super::identity::SocketIdentity;
use super::types::{IpEndpoint, Ipv4Address};

const LOCAL_ENDPOINT_SLOTS: usize = 256;
const LISTENER_SLOTS: usize = 128;
const CONNECTION_SLOTS: usize = 256;
const RAW_ICMP_SLOTS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalEndpointKey {
    pub addr: Ipv4Address,
    pub port: u16,
}

impl LocalEndpointKey {
    pub const fn new(endpoint: IpEndpoint) -> Self {
        Self {
            addr: endpoint.addr,
            port: endpoint.port,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionKey {
    pub local_addr: Ipv4Address,
    pub local_port: u16,
    pub remote_addr: Ipv4Address,
    pub remote_port: u16,
}

impl ConnectionKey {
    pub const fn new(local: IpEndpoint, remote: IpEndpoint) -> Self {
        Self {
            local_addr: local.addr,
            local_port: local.port,
            remote_addr: remote.addr,
            remote_port: remote.port,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListenerKey {
    pub local_addr: Ipv4Address,
    pub local_port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawIcmpSocketKey {
    pub socket_raw: u32,
}

impl ListenerKey {
    pub const fn exact(addr: Ipv4Address, port: u16) -> Self {
        Self {
            local_addr: addr,
            local_port: port,
        }
    }

    pub const fn wildcard(port: u16) -> Self {
        Self {
            local_addr: Ipv4Address::UNSPECIFIED,
            local_port: port,
        }
    }

    pub const fn from_endpoint(endpoint: IpEndpoint) -> Self {
        Self::exact(endpoint.addr, endpoint.port)
    }
}

pub struct SocketTable {
    tcp_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    tcp_listeners: Index<ListenerKey, Cap<SocketIdentity>, LISTENER_SLOTS>,
    tcp_connections: Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    udp_bound: Index<LocalEndpointKey, Cap<SocketIdentity>, LOCAL_ENDPOINT_SLOTS>,
    udp_connections: Index<ConnectionKey, Cap<SocketIdentity>, CONNECTION_SLOTS>,
    raw_icmp: Index<RawIcmpSocketKey, Cap<SocketIdentity>, RAW_ICMP_SLOTS>,
}

impl SocketTable {
    pub const fn new() -> Self {
        Self {
            tcp_bound: Index::new(),
            tcp_listeners: Index::new(),
            tcp_connections: Index::new(),
            udp_bound: Index::new(),
            udp_connections: Index::new(),
            raw_icmp: Index::new(),
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

    pub fn insert_tcp_connection(
        &self,
        key: ConnectionKey,
        socket: Cap<SocketIdentity>,
    ) -> Result<(), IndexError> {
        self.tcp_connections.reserve(key)?.commit(socket);
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

    pub fn withdraw_tcp_bound(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.tcp_bound, &LocalEndpointKey::new(endpoint))
    }

    pub fn withdraw_tcp_listener(
        &self,
        endpoint: IpEndpoint,
    ) -> Result<Cap<SocketIdentity>, MutationError> {
        mutation::withdraw(&self.tcp_listeners, &ListenerKey::from_endpoint(endpoint))
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

    pub fn lookup_tcp_bound(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.tcp_bound
            .lookup(&LocalEndpointKey::new(endpoint), guard)
            .and_then(|entry| entry.value().try_clone_live())
    }

    pub fn lookup_tcp_listener(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.lookup_tcp_listener_addr(endpoint.addr, endpoint.port, guard)
    }

    pub fn lookup_tcp_listener_addr(
        &self,
        addr: Ipv4Address,
        port: u16,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        let exact = ListenerKey::exact(addr, port);
        if let Some(entry) = self.tcp_listeners.lookup(&exact, guard) {
            if let Some(socket) = entry.value().try_clone_live() {
                return Some(socket);
            }
        }
        let wildcard = ListenerKey::wildcard(port);
        self.tcp_listeners
            .lookup(&wildcard, guard)
            .and_then(|entry| entry.value().try_clone_live())
    }

    pub fn lookup_tcp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.tcp_connections
            .lookup(&key, guard)
            .and_then(|entry| entry.value().try_clone_live())
    }

    pub fn lookup_udp_connection(
        &self,
        key: ConnectionKey,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.udp_connections
            .lookup(&key, guard)
            .and_then(|entry| entry.value().try_clone_live())
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

        let wildcard_local =
            ConnectionKey::new(IpEndpoint::new(Ipv4Address::UNSPECIFIED, dst.port), src);
        if let Some(socket) = self.lookup_udp_connection(wildcard_local, guard) {
            return Some(socket);
        }

        self.lookup_udp_bound(dst, guard)
    }

    pub fn snapshot_tcp_listeners(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_listeners
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_tcp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_tcp_connections(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.tcp_connections
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
        self.lookup_udp_bound_wildcard(endpoint, guard)
    }

    pub fn lookup_udp_bound_exact(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.udp_bound
            .lookup(&LocalEndpointKey::new(endpoint), guard)
            .and_then(|entry| entry.value().try_clone_live())
    }

    pub fn lookup_udp_bound_wildcard(
        &self,
        endpoint: IpEndpoint,
        guard: &Guard<'_>,
    ) -> Option<Cap<SocketIdentity>> {
        self.udp_bound
            .lookup(
                &LocalEndpointKey {
                    addr: Ipv4Address::UNSPECIFIED,
                    port: endpoint.port,
                },
                guard,
            )
            .and_then(|entry| entry.value().try_clone_live())
    }

    pub fn snapshot_udp_bound(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.udp_bound
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
    }

    pub fn snapshot_raw_icmp(&self, guard: &Guard<'_>) -> Vec<Cap<SocketIdentity>> {
        self.raw_icmp
            .snapshot_values_filter_map(guard, Cap::try_clone_live)
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
}

pub static SOCKET_TABLE: InitialSocketTableProxy = InitialSocketTableProxy;
