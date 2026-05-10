use tx_substrate::bus::{RawQueue, StaticRawQueue};

use crate::execution::WaitToken;
use crate::sync::SpinMutex;
use crate::wait_carrier;

tx_substrate::bus::bus_readiness! {
    pub struct DelegateWireSet {
        pub const POLL = 0x1;
        pub const TICK = 0x2;
    }
}

static NET_DELEGATE_QUEUE_STORAGE: StaticRawQueue = StaticRawQueue::new();
static NET_DELEGATE_CARRIER_ID: SpinMutex<Option<u64>> = SpinMutex::new(None);

pub fn net_delegate_queue() -> RawQueue {
    NET_DELEGATE_QUEUE_STORAGE.raw()
}

pub fn net_delegate_carrier_id() -> u64 {
    let mut id = NET_DELEGATE_CARRIER_ID.lock();
    if let Some(id) = *id {
        return id;
    }

    let registered = wait_carrier::register_wait_queue(net_delegate_queue());
    *id = Some(registered);
    registered
}

pub fn net_delegate_wait_token() -> WaitToken {
    WaitToken::new(
        net_delegate_carrier_id(),
        (DelegateWireSet::POLL | DelegateWireSet::TICK).bits(),
    )
}

pub fn net_delegate_kick_poll() -> usize {
    net_delegate_queue().fire(DelegateWireSet::POLL.bits())
}

pub fn net_delegate_kick_tick() -> usize {
    net_delegate_queue().fire(DelegateWireSet::TICK.bits())
}

pub fn net_delegate_clear(bits: DelegateWireSet) {
    net_delegate_queue().clear(bits.bits());
}
