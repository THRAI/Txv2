use alloc::sync::Arc;

use tx_substrate::bus::{RawPort, RawQueue};
use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};

use crate::net::adapter::wait_routing;
use crate::sync::SpinMutex;

tx_substrate::bus::bus_readiness! {
    pub struct RecvWireSet {
        pub const HAS_DATA = 0x1;
        pub const BROKEN = 0x2;
    }
}

tx_substrate::bus::bus_readiness! {
    pub struct SendWireSet {
        pub const SPACE = 0x1;
        pub const BROKEN = 0x2;
    }
}

tx_substrate::bus::bus_readiness! {
    pub struct AcceptWireSet {
        pub const HAS_PENDING = 0x1;
        pub const BROKEN = 0x2;
    }
}

tx_substrate::bus::bus_lifecycle! {
    pub struct UrgentEvent {
        pub const URGENT = 0x1;
    }
}

pub struct SocketReadiness {
    pub recv_wq: RawQueue,
    pub send_wq: RawQueue,
    pub accept_wq: RawQueue,
    /// P3-S1 (R4a): substrate-side `WaitSource` mirrors sharing the same
    /// carrier ids as the `RawQueue`s above. Installed by
    /// `SocketWaitCarriers::register`; `fire_*` notifies BOTH sides so
    /// `await_wait_source`/epoll subscribers wake alongside
    /// `wait_on_token` subscribers.
    recv_src: SpinMutex<Option<Arc<wait_routing::WaitSource>>>,
    send_src: SpinMutex<Option<Arc<wait_routing::WaitSource>>>,
    accept_src: SpinMutex<Option<Arc<wait_routing::WaitSource>>>,
}

impl SocketReadiness {
    pub fn new() -> Self {
        Self {
            recv_wq: RawQueue::new(),
            send_wq: RawQueue::new(),
            accept_wq: RawQueue::new(),
            recv_src: SpinMutex::new(None),
            send_src: SpinMutex::new(None),
            accept_src: SpinMutex::new(None),
        }
    }

    pub(crate) fn install_substrate_mirrors(
        &self,
        recv: Arc<wait_routing::WaitSource>,
        send: Arc<wait_routing::WaitSource>,
        accept: Arc<wait_routing::WaitSource>,
    ) {
        *self.recv_src.lock() = Some(recv);
        *self.send_src.lock() = Some(send);
        *self.accept_src.lock() = Some(accept);
    }

    fn notify_mirror(slot: &SpinMutex<Option<Arc<wait_routing::WaitSource>>>, bits: u64) {
        if let Some(source) = &*slot.lock() {
            wait_routing::notify_v3_source(source, bits);
        }
    }

    pub fn fire_recv(&self, set: RecvWireSet) -> usize {
        let wakes = self.recv_wq.fire(set.bits());
        Self::notify_mirror(&self.recv_src, set.bits());
        wakes
    }

    pub fn fire_recv_with_post<F>(&self, set: RecvWireSet, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let wakes = self.recv_wq.fire_with_post(set.bits(), post);
        Self::notify_mirror(&self.recv_src, set.bits());
        wakes
    }

    pub fn clear_recv(&self, set: RecvWireSet) {
        self.recv_wq.clear(set.bits());
    }

    pub fn fire_send(&self, set: SendWireSet) -> usize {
        let wakes = self.send_wq.fire(set.bits());
        Self::notify_mirror(&self.send_src, set.bits());
        wakes
    }

    pub fn fire_send_with_post<F>(&self, set: SendWireSet, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let wakes = self.send_wq.fire_with_post(set.bits(), post);
        Self::notify_mirror(&self.send_src, set.bits());
        wakes
    }

    pub fn clear_send(&self, set: SendWireSet) {
        self.send_wq.clear(set.bits());
    }

    pub fn fire_accept(&self, set: AcceptWireSet) -> usize {
        let wakes = self.accept_wq.fire(set.bits());
        Self::notify_mirror(&self.accept_src, set.bits());
        wakes
    }

    pub fn fire_accept_with_post<F>(&self, set: AcceptWireSet, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let wakes = self.accept_wq.fire_with_post(set.bits(), post);
        Self::notify_mirror(&self.accept_src, set.bits());
        wakes
    }

    pub fn clear_accept(&self, set: AcceptWireSet) {
        self.accept_wq.clear(set.bits());
    }

    pub fn debug_current_bits(&self) -> u64 {
        self.recv_wq.peek() | self.send_wq.peek() | self.accept_wq.peek()
    }
}

impl Default for SocketReadiness {
    fn default() -> Self {
        Self::new()
    }
}

pub fn new_urgent_port() -> RawPort {
    RawPort::new()
}
