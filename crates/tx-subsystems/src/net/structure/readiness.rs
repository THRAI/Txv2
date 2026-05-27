use tx_substrate::bus::{RawPort, RawQueue};

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
}

impl SocketReadiness {
    pub fn new() -> Self {
        Self {
            recv_wq: RawQueue::new(),
            send_wq: RawQueue::new(),
            accept_wq: RawQueue::new(),
        }
    }

    pub fn fire_recv(&self, set: RecvWireSet) -> usize {
        self.recv_wq.fire(set.bits())
    }

    pub fn clear_recv(&self, set: RecvWireSet) {
        self.recv_wq.clear(set.bits());
    }

    pub fn fire_send(&self, set: SendWireSet) -> usize {
        self.send_wq.fire(set.bits())
    }

    pub fn clear_send(&self, set: SendWireSet) {
        self.send_wq.clear(set.bits());
    }

    pub fn fire_accept(&self, set: AcceptWireSet) -> usize {
        self.accept_wq.fire(set.bits())
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
