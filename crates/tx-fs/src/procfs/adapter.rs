//! Substrate adapter for procfs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone"],
    reason = "expose substrate step engine and zone types for procfs FsOps impl"
)]
pub mod step_engine {
    pub use tx_substrate::step::{ByteProgress, Errno, NoProgress, StepOp, StepOutcome};
    pub use tx_substrate::zone::Cap;
}
