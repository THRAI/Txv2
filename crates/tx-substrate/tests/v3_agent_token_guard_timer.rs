//! Phase 7 pin tests: delegate token lifetime is independent from timers.
//!
//! `tx-substrate` owns the delegate state machine only. The script driver
//! owns deadline registration through `tx-time`, so neither
//! `DelegateRegistry` nor `AgentTokenGuard` carries a timer guard.

use std::sync::Arc;

use tx_substrate::step::{
    AgentCancelPolicy, DelegateRegistry, DelegateRequest, DelegateState, TokenDropPolicy,
    TransitionOutcome,
};
use tx_substrate::wake::TaskMailbox;

#[test]
fn install_request_and_drop_preserve_cancel_semantics_without_a_timer() {
    let registry = DelegateRegistry::new();
    let mailbox = Arc::new(TaskMailbox::new());

    let id = {
        let guard = registry.install_request(
            DelegateRequest::Placeholder,
            0,
            AgentCancelPolicy::BestEffort,
            TokenDropPolicy::CancelOnDrop,
            Arc::downgrade(&mailbox),
        );
        assert_eq!(guard.state(), Some(DelegateState::Pending));
        guard.id()
    };

    assert_eq!(registry.state(id), Some(DelegateState::Canceled));
}

#[test]
fn externally_driven_timeout_remains_terminal_after_guard_drop() {
    let registry = DelegateRegistry::new();
    let guard = registry.install_request(
        DelegateRequest::Placeholder,
        0,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        std::sync::Weak::new(),
    );
    let id = guard.id();

    assert_eq!(
        registry.mark_timed_out_with_post(id, |_mailbox, _event| {}),
        TransitionOutcome::Applied
    );
    drop(guard);

    assert_eq!(registry.state(id), Some(DelegateState::TimedOut));
}
