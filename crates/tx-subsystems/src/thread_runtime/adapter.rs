//! Substrate / reactor adapter for thread_runtime.
//!
//! Thread_runtime spans three files (structure.rs, execution.rs, tests.rs)
//! and touches:
//! - `step_v3`: `StepOp`, `StepOutcome`, `NoProgress`, `ScriptCtx`,
//!   `SubjectIdentity`, `MailboxEvent`, `SignalRouting` — used by the
//!   two `StepOp` wraps in `execution.rs` and related test code.
//! - `zone`: `Cap`, `PayloadCap`, `Weak`, `Dead`, `Entity`,
//!   `OperationalCapExt`, `Zone`, `ZoneAllocated` — for `ThreadIdentity`
//!   and `ThreadPayload` zone wiring.
//! - `epoch`: `guard()`, `Guard` — for EBR-protected weak upgrades.
//! - `wake`: `TaskMailbox`, `MailboxEvent`, `SignalRouting` — for D9-A
//!   signal-wake mailbox plumbing.
//! - `SpinMutex` — throughout structure.rs for payload slots.
//! - `reactor::userspace`: `UserspaceRunRequest`, `UserspaceRunSlot`
//!   (in structure.rs); `SyscallRequest`, `UserspaceTrapInfo` (in tests).
//! - `reactor::TaskKey` — in `ThreadPayload.task`.
//!
//! Two domains:
//! * `step_engine` — substrate: all step_v3/zone/epoch/wake/SpinMutex types.
//! * `reactor_entry` — reactor: userspace run-slot, task key, syscall types.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "wake"],
    reason = "expose substrate step engine (StepOp/StepOutcome/NoProgress/ScriptCtx/SubjectIdentity), D9-A signal-wake mailbox (MailboxEvent/SignalRouting/TaskMailbox), zone role types (Cap/PayloadCap/Weak/Dead/Entity/OperationalCapExt/Zone/ZoneAllocated), EBR guard, and SpinMutex used by thread_runtime structure, execution, and tests"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        drive_oneshot, Errno, NoProgress, OneShotStepOp, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity,
    };
    pub use tx_substrate::wake::{MailboxEvent, SignalRouting, TaskMailbox};
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}

#[platform_adapter(
    platform = "reactor",
    domain = "reactor_entry",
    reason = "wrap reactor::userspace (UserspaceRunSlot, UserspaceRunRequest, SyscallRequest, UserspaceTrapInfo) and TaskKey used by ThreadPayload trap-shell handoff fields and tests"
)]
pub mod reactor_entry {
    pub use tx_reactor::userspace::{
        SyscallRequest, UserspaceRunRequest, UserspaceRunSlot, UserspaceTrapInfo,
    };
    pub use tx_reactor::TaskKey;
}
