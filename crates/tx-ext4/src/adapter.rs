//! Substrate adapter for tx-ext4.
//!
//! tx-ext4's substrate surface is dominated by step_v3 outcome types
//! in the `FsOps` impl (~67 lines), plus a small page_allocator and
//! epoch::guard footprint. Single `step_engine` domain.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and page-allocator primitives used by tx-ext4 FsOps implementation"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "route mount-local ext4 mutation-admission waits through the registered WaitSource substrate"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::step::{InterestMask, WaitSourceId};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source() -> Arc<WaitSource> {
        let source_id = tx_subsystems::allocate_notification_source_id();
        let source = tx_substrate::wake::new_source(source_id);
        tx_subsystems::wait_source::register_wait_source_with_diagnostic_kind(
            source_id,
            Arc::clone(&source),
            "ext4-metadata-admission",
        );
        source
    }

    pub fn notify_all(source: &Arc<WaitSource>, interests: u64) {
        source.notify_emit(InterestMask::new(interests));
    }

    pub fn unregister_source(source: &Arc<WaitSource>) {
        tx_subsystems::wait_source::release_wait_source(source.id().raw());
        tx_substrate::wake::unregister_source(WaitSourceId::new(source.id().raw()));
    }
}
