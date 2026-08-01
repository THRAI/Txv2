use core::sync::atomic::{AtomicU32, Ordering};

use tx_substrate::bus::RawPort;
use tx_substrate::step::WaitSourceId;
use tx_substrate::zone::PayloadCap;

use crate::net::adapter::wait_routing;
use crate::sync::SpinMutex;
use crate::wait_source;

use super::payload::{SocketOperationalEvidence, SocketPayload};
use super::readiness::{new_urgent_port, SocketReadiness};
use super::types::{AddressFamily, SocketKind};

pub struct SocketWaitCarriers {
    pub recv: u64,
    pub send: u64,
    pub accept: u64,
    pub urgent: u64,
}

pub struct SocketIdentity {
    pub kind: SocketKind,
    pub family: AddressFamily,
    pub readiness: SocketReadiness,
    pub wait_carriers: SocketWaitCarriers,
    pub urgent_port: RawPort,
    /// User-visible fd-table entries referring to this socket's open-file
    /// description. Capability clones are lifetime pins, not dup/fork fds.
    fd_refs: AtomicU32,
    pub(crate) payload: SpinMutex<Option<PayloadCap<SocketPayload>>>,
}

impl SocketIdentity {
    pub fn new(kind: SocketKind) -> Self {
        Self::new_with_family(kind, default_family_for_kind(kind))
    }

    pub fn new_with_family(kind: SocketKind, family: AddressFamily) -> Self {
        let readiness = SocketReadiness::new();
        let urgent_port = new_urgent_port();
        let wait_carriers = SocketWaitCarriers::register(&readiness, &urgent_port);

        Self {
            kind,
            family,
            readiness,
            wait_carriers,
            urgent_port,
            fd_refs: AtomicU32::new(1),
            payload: SpinMutex::new(None),
        }
    }

    pub fn install_payload(&self, payload: PayloadCap<SocketPayload>) {
        *self.payload.lock() = Some(payload);
    }

    pub fn take_payload(&self) -> Option<PayloadCap<SocketPayload>> {
        self.payload.lock().take()
    }

    pub fn live_payload(&self) -> Option<PayloadCap<SocketPayload>> {
        self.payload.lock().clone()
    }

    pub fn acquire_operational(&self) -> Option<SocketOperationalEvidence> {
        self.live_payload()
    }

    pub fn is_payload_live(&self) -> bool {
        self.live_payload().is_some()
    }

    pub(crate) fn incr_fd_ref(&self) {
        self.fd_refs.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn decr_fd_ref(&self) {
        let mut current = self.fd_refs.load(Ordering::Acquire);
        while current != 0 {
            match self.fd_refs.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    pub fn fd_ref_count(&self) -> u32 {
        self.fd_refs.load(Ordering::Acquire)
    }

    pub(crate) fn with_payload_for_check<R>(
        &self,
        f: impl FnOnce(Option<&SocketPayload>) -> R,
    ) -> R {
        let payload = self.payload.lock();
        f(payload.as_ref().map(|payload| &**payload))
    }
}

const fn default_family_for_kind(kind: SocketKind) -> AddressFamily {
    match kind {
        SocketKind::UnixDatagram | SocketKind::UnixStream => AddressFamily::Unix,
        SocketKind::Tcp | SocketKind::Udp | SocketKind::Sctp | SocketKind::RawIcmp => {
            AddressFamily::Inet
        }
        SocketKind::NetlinkRoute | SocketKind::NetlinkXfrm | SocketKind::NetlinkNetfilter => {
            AddressFamily::Netlink
        }
        SocketKind::Packet => AddressFamily::Packet,
        SocketKind::RdsSeqPacket => AddressFamily::Rds,
    }
}

impl SocketWaitCarriers {
    /// P3-S1 (R4a): dual-register every readiness carrier under ONE id
    /// from the v3 notification namespace — the subsystems registry (for
    /// `wait_on_token`: ppoll/pselect and the legacy socket park paths)
    /// AND the substrate registry (for `await_wait_source`: epoll).
    /// Mirrors the eventfd pattern (`eventfd/notification.rs`
    /// `new_wait_points`). Before this, epoll looked socket carriers up
    /// in a registry they were never in, so `epoll_wait` on a
    /// pure-socket set returned 0 immediately instead of blocking.
    fn register(readiness: &SocketReadiness, urgent_port: &RawPort) -> Self {
        let recv = crate::allocate_notification_source_id();
        let send = crate::allocate_notification_source_id();
        let accept = crate::allocate_notification_source_id();
        let urgent = crate::allocate_notification_source_id();

        // `register_wait_queue`/`register_wait_port` honour an id the carrier
        // already carries and only allocate when it is still 0, so stamping the
        // notification id first pins the registration to that id. The two id
        // spaces do not overlap: notification ids start at 1 << 32, the
        // registry's own allocator starts at 1.
        readiness.recv_wq.set_source_id(WaitSourceId::new(recv));
        readiness.send_wq.set_source_id(WaitSourceId::new(send));
        readiness.accept_wq.set_source_id(WaitSourceId::new(accept));
        urgent_port.set_source_id(WaitSourceId::new(urgent));
        wait_source::register_wait_queue(readiness.recv_wq.clone());
        wait_source::register_wait_queue(readiness.send_wq.clone());
        wait_source::register_wait_queue(readiness.accept_wq.clone());
        wait_source::register_wait_port(urgent_port.clone());

        readiness.install_substrate_mirrors(
            wait_routing::new_wait_source(recv),
            wait_routing::new_wait_source(send),
            wait_routing::new_wait_source(accept),
        );

        Self {
            recv,
            send,
            accept,
            urgent,
        }
    }

    fn release(&self) {
        wait_source::release_wait_source(self.recv);
        wait_source::release_wait_source(self.send);
        wait_source::release_wait_source(self.accept);
        wait_source::release_wait_source(self.urgent);
        wait_routing::unregister_source(self.recv);
        wait_routing::unregister_source(self.send);
        wait_routing::unregister_source(self.accept);
    }
}

impl Drop for SocketIdentity {
    fn drop(&mut self) {
        self.wait_carriers.release();
    }
}
