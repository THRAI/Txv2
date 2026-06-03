//! Substrate / reactor adapter for vm.
//!
//! VM has the richest substrate surface of any subsystem so far: in
//! addition to the standard step_v3 / zone / epoch / lock-facade set,
//! it consumes the userfaultfd-delegate plumbing (DelegateRegistry,
//! DelegateRequest / DelegateReply, UfdAccessKind / UfdRequest /
//! UfdReply, AbortReason, AgentCancelPolicy, TokenDropPolicy,
//! YieldShape), the wake `TaskMailbox`, the page-allocator
//! primitives (BitmapPageAllocator, CachePin, ZeroPolicy), and the
//! `shootdown` sub-API (AddressSpaceShootdownBatch, ShootdownError)
//! that no earlier subsystem needed.
//!
//! Two domains:
//!
//! * `step_engine` — substrate. Everything in the above paragraph
//!   except the reactor `Channel`/`Mask`. Re-exports `zone::sign` plus
//!   pass-through `reserve_for` / `sign_for`.
//!
//! * `wait_routing` — stacked substrate + reactor. `RangeLock`
//!   exposes a wait source for fault-resolution range conflicts;
//!   standard wakeup verbs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator", "shootdown", "wake"],
    reason = "expose substrate step engine (including delegate-registry plumbing for userfaultfd: DelegateRequest/Reply, UfdRequest/Reply, AbortReason, AgentCancelPolicy, TokenDropPolicy, YieldShape), zone role types, EBR guard, page-allocator primitives, shootdown surface (AddressSpaceShootdownBatch, ShootdownError), TaskMailbox, and SpinMutex used by vm fault resolver, address-space ops, range-lock wait sources, and the recipe/private mapping structures"
)]
pub mod step_engine {
    #[cfg(not(tx_lock_metrics_vm))]
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{self as epoch_mod, Guard, guard};
    pub use tx_substrate::page_allocator::{self, BitmapPageAllocator, CachePin, ZeroPolicy};
    pub use tx_substrate::shootdown::{AddressSpaceShootdownBatch, ShootdownError};
    pub use tx_substrate::step::{
        AbortReason, AgentCancelPolicy, ByteProgress, DelegateRegistry, DelegateReply,
        DelegateRequest, DelegateState, DelegateTokenId, Errno, InterestMask, NoProgress,
        PageProgress, ProcessIdentity as PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome,
        StepProgress, SubjectIdentity, TokenDropPolicy, TransitionOutcome, UfdAccessKind, UfdReply,
        UfdRequest, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox};
    pub use tx_substrate::zone::{
        Cap, CapProducingPolicy, CoLocatedEntity, Dead, Entity, IdentRef, IdentitySlot,
        IsPayloadPolicy, ObserverNodePolicy, OperationalCapExt, OperationalRefExt, PayloadBinding,
        PayloadCap, PayloadPolicy, RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError,
        ZonePolicy, register_zone_for, reserve_for, sign, sign_for,
    };
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["step"],
    reason = "wrap WaitSourceId minting for vm range-lock wait sources (range_lock.rs)"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as vm range-lock legacy wakeup verbs"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::await_agent_reply;
    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }
}
