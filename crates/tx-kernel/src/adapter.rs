//! Substrate / reactor adapter for tx-kernel.
//!
//! tx-kernel's substrate / reactor surface is the richest of the
//! consumer crates because init.rs orchestrates the BSP/AP reactor
//! loop, registers zones, wires hart-loop deadlines, and brings up
//! every subsystem. Two domains:
//!
//! * `step_engine` — substrate. Step engine, zone role types, EBR
//!   Guard, SpinMutex, plus `sign_zone_for`.
//!
//! * `boot_runtime` — stacked substrate + reactor. The boot-side
//!   primitives kernel init pulls from reactor (HartId, hart_loop,
//!   userspace, wait, SharedReactor, InitialSchedMeta,
//!   RescheduleSignal, etc.).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and SpinMutex used by tx-kernel init and bootstrap wiring"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{drain_with_budget, guard, Guard};
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, PayloadCap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::{init, init_on_ap, page_allocator, SpinMutex};

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "boot_runtime",
    reason = "wrap reactor boot primitives (HartId, hart_loop, userspace, wait, SharedReactor, InitialSchedMeta, RescheduleSignal, ast) used by tx-kernel init and trap_handoff"
)]
pub mod boot_runtime {
    pub use tx_reactor::{
        ast, hart_loop, userspace, wait, HartId, InitialSchedMeta, RescheduleSignal,
        SharedReactor,
    };
}
