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
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        AgentCancelPolicy, ByteProgress, CancelReason, DelegateReply, DelegateRequest,
        DelegateState, DelegateTokenId, Errno, InterestMask, NoProgress, OnBehalfOfAbort, ScriptCtx,
        StepOp, StepOutcome, SubjectAuthority, SubjectContext, SubjectIdentity, TokenDropPolicy,
        TransitionOutcome, UfdAccessKind, UfdReply, UfdRequest, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::{page_allocator, SpinMutex};

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "reactor_entry",
    reason = "wrap reactor::userspace re-exports used by tx-shims test scaffolding"
)]
pub mod reactor_entry {
    pub use tx_reactor::userspace;
    pub use tx_reactor::userspace::SyscallRequest;
}
