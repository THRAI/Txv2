//! Substrate / reactor adapter for tx-kernel.
//!
//! tx-kernel's substrate / reactor surface is the richest of the
//! consumer crates because init.rs orchestrates the BSP/AP reactor
//! loop, registers zones, wires hart-loop deadlines, and brings up
//! every subsystem. Two domains:
//!
//! * `step_engine` — substrate. Step engine, zone role types, EBR
//!   Guard, SpinMutex. Re-exports `zone::sign`.
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
    pub use tx_substrate::epoch::{
        self as epoch, cpu_summary, drain_with_budget, guard, summary, Guard,
    };
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, PayloadCap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::{init, init_on_ap, page_allocator, SpinMutex};
}

#[platform_adapter(
    platform = "reactor",
    domain = "boot_runtime",
    reason = "wrap reactor boot primitives (HartId, hart_loop, userspace, wait, SharedReactor, InitialSchedMeta, RescheduleSignal, ast) used by tx-kernel init and trap_handoff"
)]
pub mod boot_runtime {
    pub use tx_reactor::{
        ast, hart_loop, userspace, wait, HartId, InitialSchedMeta, RescheduleSignal, SharedReactor,
    };
}
