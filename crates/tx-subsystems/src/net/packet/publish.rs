use tx_substrate::zone::Cap;

use crate::net::structure::{AcceptWireSet, RecvWireSet, SendWireSet, SocketIdentity, UrgentEvent};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkPublish {
    pub recv_has_data: bool,
    pub send_has_space: bool,
    pub accept_has_pending: bool,
    pub recv_broken: bool,
    pub send_broken: bool,
    pub urgent: bool,
}

impl NetworkPublish {
    pub const fn none() -> Self {
        Self {
            recv_has_data: false,
            send_has_space: false,
            accept_has_pending: false,
            recv_broken: false,
            send_broken: false,
            urgent: false,
        }
    }

    pub fn publish_to(self, socket: &SocketIdentity) -> usize {
        let mut wakes = 0;
        if self.recv_has_data {
            wakes += socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
        }
        if self.send_has_space {
            wakes += socket.readiness.fire_send(SendWireSet::SPACE);
        }
        if self.accept_has_pending {
            wakes += socket.readiness.fire_accept(AcceptWireSet::HAS_PENDING);
        }
        if self.recv_broken {
            wakes += socket.readiness.fire_recv(RecvWireSet::BROKEN);
        }
        if self.send_broken {
            wakes += socket.readiness.fire_send(SendWireSet::BROKEN);
        }
        if self.urgent {
            wakes += socket.urgent_port.fire(UrgentEvent::URGENT.bits());
        }
        wakes
    }

    pub const fn has_any(self) -> bool {
        self.recv_has_data
            || self.send_has_space
            || self.accept_has_pending
            || self.recv_broken
            || self.send_broken
            || self.urgent
    }
}

#[derive(Clone)]
pub struct NetworkPublishTarget {
    pub socket: Cap<SocketIdentity>,
    pub publish: NetworkPublish,
}

impl NetworkPublishTarget {
    pub fn new(socket: Cap<SocketIdentity>, publish: NetworkPublish) -> Self {
        Self { socket, publish }
    }

    pub fn publish(self) -> usize {
        self.publish.publish_to(&self.socket)
    }
}
