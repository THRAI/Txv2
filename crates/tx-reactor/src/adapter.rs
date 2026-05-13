//! Substrate adapter for tx-reactor.
//!
//! tx-reactor's substrate surface spans two sub-APIs:
//!
//! * `step_engine` — substrate step_v3 trait impls (`StepOp`) used by the
//!   hart-loop op wrappers and the agent-reply future.
//! * `bus_wire` — substrate bus and wake primitives re-exported by the
//!   back-compat shims (`mailbox`, `wait_source`, `timer`) and consumed
//!   by the wait-channel and runtime modules.
//!
//! Both domains are `#[platform_adapter]`-marked so boundary-report counts
//! their `tx_substrate::*` lines as inside-adapter.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step"],
    reason = "expose substrate step_v3 trait/type surface (StepOp, StepOutcome, NoProgress, ScriptCtx, SubjectIdentity, ProcessIdentity, AbortReason, DelegateRegistry, DelegateReply, DelegateTokenId) used by hart_loop StepOp impls and agent_reply future"
)]
pub mod step_engine {
    pub use tx_substrate::step::{
        AbortReason, AgentCancelPolicy, Deadline, DelegateRegistry, DelegateReply, DelegateRequest,
        DelegateState, DelegateTokenId, NoProgress, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity, TokenDropPolicy, TransitionOutcome,
    };
    // ProcessIdentity is used as a placeholder subject in tests.
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
}

#[platform_adapter(
    platform = "substrate",
    domain = "bus_wire",
    apis = ["bus", "wake"],
    reason = "expose substrate bus wire and wake primitives (bus ports/queues, wake mailbox/wait_source/timer/agent_event_matches) re-exported by tx-reactor back-compat shims and consumed by wait channels and agent_reply"
)]
pub mod bus_wire {
    pub use tx_substrate::bus::{
        DeclaredPort, DeclaredPortSubscription, DeclaredQueue, DeclaredQueueSubscription,
        DeclaredWireError, RawPort, RawPortSubscription, RawQueue, WireDeclaration,
        WireDeclarationError, WireEventSet,
    };
    pub use tx_substrate::wake::mailbox;
    pub use tx_substrate::wake::timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};
    pub use tx_substrate::wake::wait_source;
    pub use tx_substrate::wake::{agent_event_matches, MailboxEvent, TaskMailbox};

    // Re-export the bus DSL macros so test code can declare lifecycle
    // and readiness wire-protocols via `bus_wire::bus_lifecycle! { ... }`
    // instead of reaching past the adapter to `tx_substrate::bus::*`.
    // `pub use` of `#[macro_export]` macros lifts them into the adapter
    // module path while preserving the original definitions.
    pub use tx_substrate::{bus_lifecycle, bus_readiness};
}
