//! Substrate / reactor adapter for tx-kernel.
//!
//! tx-kernel's substrate / reactor surface is the richest of the
//! consumer crates because init.rs orchestrates the BSP/AP reactor
//! loop, registers zones, wires hart-loop deadlines, and brings up
//! every subsystem. Two domains:
//!
//! * `step_engine` — substrate. Step engine, zone role types, EBR
//!   Guard, and the tx-kernel lock facade. Re-exports `zone::sign`.
//!
//! * `boot_runtime` — stacked substrate + reactor. The boot-side
//!   primitives kernel init pulls from reactor (HartId, hart_loop,
//!   userspace, wait, SharedReactor, InitialSchedMeta,
//!   RescheduleSignal, mailbox wake event types, etc.).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and the tx-kernel lock facade used by init and bootstrap wiring"
)]
pub mod step_engine {
    pub(crate) use crate::sync::{spin_mutex, SpinMutex};
    pub use tx_substrate::epoch::{
        self as epoch, borrow_current_guard, cpu_summary, drain_requested_with_budget,
        drain_with_budget, guard, summary, Guard,
    };
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::{init, init_on_ap, page_allocator};
}

#[platform_adapter(
    platform = "reactor",
    domain = "boot_runtime",
    reason = "wrap reactor boot primitives (HartId, hart_loop, userspace, wait, SharedReactor, InitialSchedMeta, RescheduleSignal, ast) and substrate mailbox wake event types used by tx-kernel init and trap_handoff"
)]
pub mod boot_runtime {
    pub use tx_reactor::dispatch::WakeDispatchReport;
    pub use tx_reactor::{
        ast, current_deadline_registrar, current_delegate_registry, current_task_mailbox,
        hart_loop, userspace, wait, yield_now, HartId, HartPollBudget, InitialSchedMeta,
        Phase1QueueKind, Reactor, RescheduleSignal, SharedReactor, SliceClock, StopReason,
        SubscriberId, TaskId, TaskKey, TaskRunOwner, TaskStatus, WaitSource,
    };
    pub use tx_services::time::TimerGuard;
    pub use tx_substrate::step::{
        AgentCancelPolicy, Deadline, DelegateRegistry, DelegateReply, DelegateRequest,
        DelegateTokenId, InterestMask, TokenDropPolicy, TransitionOutcome, WaitSourceId,
    };
    pub use tx_substrate::wake::mailbox::MailboxPollAction;
    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox};
}
