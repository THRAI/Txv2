use tx_substrate::zone::Cap;

use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};

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

    pub fn publish_to_with_post<F>(self, socket: &SocketIdentity, mut post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mut wakes = 0;
        if self.recv_has_data {
            wakes += socket
                .readiness
                .fire_recv_with_post(RecvWireSet::HAS_DATA, &mut post);
        }
        if self.send_has_space {
            wakes += socket
                .readiness
                .fire_send_with_post(SendWireSet::SPACE, &mut post);
        }
        if self.accept_has_pending {
            wakes += socket
                .readiness
                .fire_accept_with_post(AcceptWireSet::HAS_PENDING, &mut post);
        }
        if self.recv_broken {
            wakes += socket
                .readiness
                .fire_recv_with_post(RecvWireSet::BROKEN, &mut post);
        }
        if self.send_broken {
            wakes += socket
                .readiness
                .fire_send_with_post(SendWireSet::BROKEN, &mut post);
        }
        if self.urgent {
            wakes += socket
                .urgent_port
                .fire_with_post(UrgentEvent::URGENT.bits(), &mut post);
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

    pub fn publish_with_post<F>(self, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.publish.publish_to_with_post(&self.socket, post)
    }
}
