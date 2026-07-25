//! Delegate timeout routing through the reactor-owned deadline domain.

use std::sync::Arc;

use tx_reactor::{MailboxEvent, Reactor, TaskMailbox};
use tx_services::time::{DeadlineNs, DeadlineRegistrar, TimerRole, TimerTarget};
use tx_substrate::step::agent::AgentTokenGuard;
use tx_substrate::step::{
    AbortReason, AgentCancelPolicy, DelegateRegistry, DelegateReply, DelegateRequest,
    DelegateState, TokenDropPolicy, TransitionOutcome,
};

fn install_delegate<'a>(
    registry: &'a DelegateRegistry,
    mailbox: &Arc<TaskMailbox>,
) -> AgentTokenGuard<'a> {
    registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::Abandon,
        Arc::downgrade(mailbox),
    )
}

#[test]
fn expired_delegate_deadline_transitions_registry_and_posts_abort() {
    let reactor = Reactor::new();
    let registry = reactor.delegate_registry_handle();
    let mailbox = Arc::new(TaskMailbox::new());
    let delegate = install_delegate(&registry, &mailbox);
    let token = delegate.id();
    let _deadline = reactor
        .deadline_registrar_handle()
        .register_deadline(
            DeadlineNs::new(50),
            TimerRole::DelegateTimeout,
            TimerTarget::DelegateToken(token),
        )
        .expect("delegate deadline registration");

    assert_eq!(reactor.advance_time_to(49), 0);
    assert_eq!(registry.state(token), Some(DelegateState::Pending));
    assert_eq!(reactor.advance_time_to(50), 1);
    assert_eq!(registry.state(token), Some(DelegateState::TimedOut));
    assert_eq!(
        mailbox.poll(),
        Some(MailboxEvent::Abort {
            token_id: token,
            reason: AbortReason::TimedOut,
        })
    );
    assert_eq!(reactor.advance_time_to(51), 0);

    let _ = delegate.forget();
}

#[test]
fn late_delegate_deadline_retirement_does_not_override_a_reply() {
    let reactor = Reactor::new();
    let registry = reactor.delegate_registry_handle();
    let mailbox = Arc::new(TaskMailbox::new());
    let delegate = install_delegate(&registry, &mailbox);
    let token = delegate.id();
    let _deadline = reactor
        .deadline_registrar_handle()
        .register_deadline(
            DeadlineNs::new(50),
            TimerRole::DelegateTimeout,
            TimerTarget::DelegateToken(token),
        )
        .expect("delegate deadline registration");

    assert_eq!(
        registry.mark_replied_with_post(token, DelegateReply::placeholder(), |mailbox, event| {
            if let Some(mailbox) = mailbox.upgrade() {
                let _ = mailbox.post(event);
            }
        }),
        TransitionOutcome::Applied
    );
    assert_eq!(reactor.advance_time_to(50), 1);
    assert_eq!(registry.state(token), Some(DelegateState::Replied));
    assert!(
        matches!(mailbox.poll(), Some(MailboxEvent::AgentReplied { token_id }) if token_id == token)
    );
    assert!(mailbox.poll().is_none());

    let _ = delegate.forget();
}
