//! Substrate adapter for tx-scripts.
//!
//! Small surface — the exec script uses step_v3 outcome types, EBR
//! guard, and the zone Cap role type. Single `step_engine` domain.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, and EBR guard used by the tx-scripts exec script"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        AcceptOutcome, AgentCancelPolicy, ByteProgress, Deadline, DelegateEndpoint,
        DelegateRequest, DelegateToken, DriveMode, Errno, InterestMask, NoProgress,
        ProcessIdentity, ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress,
        SubjectIdentity, TimerId, Translation, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::{page_allocator, SpinMutex};
}

#[platform_adapter(
    platform = "substrate",
    domain = "wake",
    apis = ["wake"],
    reason = "expose substrate wake primitives (TaskMailbox, ActiveWait, MailboxEvent) for drive() reactor integration"
)]
pub mod wake {
    pub use tx_substrate::wake::timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};
    pub use tx_substrate::wake::{
        agent_event_matches, ActiveWait, MailboxEvent, TaskMailbox, WaitGeneration,
    };
}

#[platform_adapter(
    platform = "substrate",
    domain = "delegate_runtime",
    apis = ["step"],
    reason = "expose delegate runtime types (DelegateRegistry, DelegateReply, DelegateState, TransitionOutcome) for drive() OnAgent resolution"
)]
pub mod delegate_runtime {
    pub use tx_substrate::step::agent::{
        AbortReason, AgentTokenGuard, DelegateRegistry, DelegateReply, DelegateState,
        DelegateTokenId, TokenDropPolicy, TransitionOutcome,
    };
    pub use tx_substrate::step::{DelegateEndpoint, DelegateRequest, DelegateToken};
}
