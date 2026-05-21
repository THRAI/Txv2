//! Substrate / reactor adapter for tx-shims.
//!
//! tx-shims is mostly a thin layer of syscall arms; substrate use is
//! dominated by step_v3 outcome types in the dispatch return-shape
//! plus heavy `epoch::guard()` calls for EBR-protected fd/zone
//! reads.
//!
//! One `step_engine` domain (substrate) plus a small `reactor_entry`
//! domain (reactor::userspace).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and SpinMutex used by tx-shims syscall dispatch arms"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        drive_oneshot, AgentCancelPolicy, ByteProgress, CancelReason, DelegateReply,
        DelegateRequest, DelegateState, DelegateTokenId, DriveMode, Errno, InterestMask,
        NoProgress, OnBehalfOfAbort, OneShotStepOp, ScriptCtx, StepOp, StepOutcome,
        SubjectAuthority, SubjectContext, SubjectIdentity, TokenDropPolicy, TransitionOutcome,
        UfdAccessKind, UfdReply, UfdRequest, WaitSourceId, YieldShape,
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
    platform = "reactor",
    domain = "reactor_entry",
    reason = "wrap reactor userspace and wait re-exports used by tx-shims syscall and test scaffolding"
)]
pub mod reactor_entry {
    pub use tx_reactor::current_task_mailbox;
    pub use tx_reactor::userspace;
    pub use tx_reactor::userspace::SyscallRequest;
    pub use tx_reactor::wait::{Mask, WaitProtocol};
    pub use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};
}
