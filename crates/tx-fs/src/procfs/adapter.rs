//! Substrate adapter for procfs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine, zone types, and guard creation for procfs FsOps impl and tests"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::guard;
    pub use tx_substrate::step::{ByteProgress, Errno, NoProgress, StepOp, StepOutcome};
    pub use tx_substrate::zone::Cap;
}
