//! Shared service-runtime helper values for I/O manager queues.

use alloc::sync::{Arc, Weak};

use crate::io_manager::adapter::service_wake::{
    InterestMask, MailboxEvent, MailboxSchedulerHint, SubscriberId, TaskMailbox, WaitGeneration,
    WaitSource,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoServiceKind {
    Page,
    Writeback,
    Block,
    Driver,
}

impl IoServiceKind {
    pub const fn mask_bits(self) -> u64 {
        match self {
            Self::Page => 1 << 0,
            Self::Writeback => 1 << 1,
            Self::Block => 1 << 2,
            Self::Driver => 1 << 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceKick {
    pub service: IoServiceKind,
}

impl ServiceKick {
    pub const fn new(service: IoServiceKind) -> Self {
        Self { service }
    }
}

pub struct ServiceWakeSource {
    source_id: u64,
    source: Arc<WaitSource>,
}

impl ServiceWakeSource {
    pub fn new(source_id: u64) -> Self {
        Self {
            source_id,
            source: crate::io_manager::adapter::service_wake::new_wait_source(source_id),
        }
    }

    pub fn source_id(&self) -> u64 {
        self.source_id
    }

    pub fn wake_endpoint(&self) -> &Arc<WaitSource> {
        &self.source
    }

    pub fn subscribe(
        &self,
        service: IoServiceKind,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> SubscriberId {
        self.source
            .register(mailbox, generation, InterestMask::new(service.mask_bits()))
    }

    pub fn kick_with_post<F>(&self, kick: ServiceKick, mut post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.source.notify_with_owner_post(
            InterestMask::new(kick.service.mask_bits()),
            MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        )
    }
}

impl Drop for ServiceWakeSource {
    fn drop(&mut self) {
        crate::io_manager::adapter::service_wake::unregister_source(self.source_id());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueDepth {
    limit: usize,
    in_flight: usize,
}

impl QueueDepth {
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            in_flight: 0,
        }
    }

    pub const fn limit(self) -> usize {
        self.limit
    }

    pub const fn in_flight(self) -> usize {
        self.in_flight
    }

    pub const fn has_capacity(self) -> bool {
        self.in_flight < self.limit
    }

    pub fn try_start(&mut self) -> Result<(), QueueDepthError> {
        if !self.has_capacity() {
            return Err(QueueDepthError::Full);
        }
        self.in_flight += 1;
        Ok(())
    }

    pub fn complete_one(&mut self) -> bool {
        if self.in_flight == 0 {
            return false;
        }
        self.in_flight -= 1;
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDepthError {
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceBudget {
    remaining: usize,
}

impl ServiceBudget {
    pub const fn new(remaining: usize) -> Self {
        Self { remaining }
    }

    pub const fn remaining(self) -> usize {
        self.remaining
    }

    pub fn take_one(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_manager::adapter::service_wake::{MailboxEvent, TaskMailbox};
    use alloc::sync::Arc;

    #[test]
    fn service_wake_endpoint_matches_source_id() {
        let source = ServiceWakeSource::new(0x7000);

        assert_eq!(
            crate::io_manager::adapter::service_wake::source_id(source.wake_endpoint()),
            source.source_id()
        );
    }

    #[test]
    fn service_wake_source_posts_page_kick_to_page_waiter_mailbox() {
        let source = ServiceWakeSource::new(0x7001);
        let mailbox = Arc::new(TaskMailbox::new());
        let generation = mailbox.next_generation();
        let _subscription =
            source.subscribe(IoServiceKind::Page, Arc::downgrade(&mailbox), generation);
        let mut injected_posts = 0usize;

        let delivered =
            source.kick_with_post(ServiceKick::new(IoServiceKind::Page), |mailbox, event| {
                injected_posts += 1;
                mailbox.post(event)
            });

        assert_eq!(delivered, 1);
        assert_eq!(injected_posts, 1);
        match mailbox.poll().expect("service wake event") {
            MailboxEvent::SourceFired {
                generation: seen_generation,
                source: seen_source,
                interests,
            } => {
                assert_eq!(seen_generation, generation);
                assert_eq!(seen_source.raw(), 0x7001);
                assert_eq!(interests.raw(), IoServiceKind::Page.mask_bits());
            }
            other => panic!("expected service SourceFired, got {other:?}"),
        }
    }

    #[test]
    fn service_wake_source_does_not_post_page_kick_to_block_waiter() {
        let source = ServiceWakeSource::new(0x7002);
        let mailbox = Arc::new(TaskMailbox::new());
        let generation = mailbox.next_generation();
        let _subscription =
            source.subscribe(IoServiceKind::Block, Arc::downgrade(&mailbox), generation);
        let mut injected_posts = 0usize;

        let delivered =
            source.kick_with_post(ServiceKick::new(IoServiceKind::Page), |mailbox, event| {
                injected_posts += 1;
                mailbox.post(event)
            });

        assert_eq!(delivered, 0);
        assert_eq!(injected_posts, 0);
        assert!(mailbox.poll().is_none());
    }
}
