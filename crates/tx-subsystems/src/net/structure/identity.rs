use tx_substrate::bus::RawPort;
use tx_substrate::zone::PayloadCap;

use crate::sync::SpinMutex;
use crate::wait_source;

use super::payload::{SocketOperationalEvidence, SocketPayload};
use super::readiness::{new_urgent_port, SocketReadiness};
use super::types::SocketKind;

pub struct SocketWaitCarriers {
    pub recv: u64,
    pub send: u64,
    pub accept: u64,
    pub urgent: u64,
}

pub struct SocketIdentity {
    pub kind: SocketKind,
    pub readiness: SocketReadiness,
    pub wait_carriers: SocketWaitCarriers,
    pub urgent_port: RawPort,
    pub(crate) payload: SpinMutex<Option<PayloadCap<SocketPayload>>>,
}

impl SocketIdentity {
    pub fn new(kind: SocketKind) -> Self {
        let readiness = SocketReadiness::new();
        let urgent_port = new_urgent_port();
        let wait_carriers = SocketWaitCarriers::register(&readiness, &urgent_port);

        Self {
            kind,
            readiness,
            wait_carriers,
            urgent_port,
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

    pub(crate) fn with_payload_for_check<R>(
        &self,
        f: impl FnOnce(Option<&SocketPayload>) -> R,
    ) -> R {
        let payload = self.payload.lock();
        f(payload.as_ref().map(|payload| &**payload))
    }
}

impl SocketWaitCarriers {
    fn register(readiness: &SocketReadiness, urgent_port: &RawPort) -> Self {
        Self {
            recv: wait_source::register_wait_queue(readiness.recv_wq.clone()),
            send: wait_source::register_wait_queue(readiness.send_wq.clone()),
            accept: wait_source::register_wait_queue(readiness.accept_wq.clone()),
            urgent: wait_source::register_wait_port(urgent_port.clone()),
        }
    }

    fn release(&self) {
        wait_source::release_wait_source(self.recv);
        wait_source::release_wait_source(self.send);
        wait_source::release_wait_source(self.accept);
        wait_source::release_wait_source(self.urgent);
    }
}

impl Drop for SocketIdentity {
    fn drop(&mut self) {
        self.wait_carriers.release();
    }
}
