use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};
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
                .fire_with_post(UrgentEvent::URGENT.bits(), post);
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
        // 发布可能晚于目标 socket 的并发 close(如 lo 队列里滞留的包在
        // 对端退休后才被处理)。对死 socket 的唤醒是空操作,不是 panic:
        // 经 observe(guard) 检活,避免 Cap 裸解引用。调用方多半已持
        // guard——EBR 禁止嵌套,先借当前窗口,没有再新开。
        let guard =
            tx_substrate::epoch::borrow_current_guard().unwrap_or_else(tx_substrate::epoch::guard);
        let Some(socket) = self.socket.downgrade().observe(&guard) else {
            return 0;
        };
        self.publish.publish_to(&socket)
    }

    pub fn publish_with_post<F>(self, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let guard =
            tx_substrate::epoch::borrow_current_guard().unwrap_or_else(tx_substrate::epoch::guard);
        let Some(socket) = self.socket.downgrade().observe(&guard) else {
            return 0;
        };
        self.publish.publish_to_with_post(&socket, post)
    }
}
