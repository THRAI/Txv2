//! Substrate / reactor adapter for vm.
//!
//! VM has the richest substrate surface of any subsystem so far: in
//! addition to the standard step_v3 / zone / epoch / SpinMutex set,
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
//!   except the reactor `Channel`/`Mask`. Plus `sign_zone_for` and
//!   pass-through `reserve_for` / `sign_for`.
//!
//! * `wait_routing` — stacked substrate + reactor. `RangeLock`
//!   exposes a wait source for fault-resolution range conflicts;
//!   standard wakeup verbs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch", "page_allocator", "shootdown", "wake"],
    reason = "expose substrate step engine (including delegate-registry plumbing for userfaultfd: DelegateRequest/Reply, UfdRequest/Reply, AbortReason, AgentCancelPolicy, TokenDropPolicy, YieldShape), zone role types, EBR guard, page-allocator primitives, shootdown surface (AddressSpaceShootdownBatch, ShootdownError), TaskMailbox, and SpinMutex used by vm fault resolver, address-space ops, range-lock wait sources, and the recipe/private mapping structures"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{self as epoch_mod, guard, Guard};
    pub use tx_substrate::page_allocator::{
        self, BitmapPageAllocator, CachePin, ZeroPolicy,
    };
    pub use tx_substrate::shootdown::{AddressSpaceShootdownBatch, ShootdownError};
    pub use tx_substrate::step_v3::{
        AbortReason, AgentCancelPolicy, ByteProgress, DelegateRegistry, DelegateReply,
        DelegateRequest, Errno, InterestMask, NoProgress, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity, TokenDropPolicy, UfdAccessKind, UfdReply, UfdRequest, WaitSourceId,
        YieldShape,
    };
    pub use tx_substrate::wake::TaskMailbox;
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["step_v3"],
    reason = "wrap WaitSourceId minting for vm range-lock wait sources (range_lock.rs)"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as vm range-lock legacy wakeup verbs"
)]
pub mod wait_routing {
    pub use tx_reactor::wait::{Channel, Mask};
}
