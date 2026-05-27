//! Substrate adapter for sysfs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone"],
    reason = "expose substrate step engine outcome types and zone caps for sysfs FsOps impl"
)]
pub mod step_engine {
    pub use tx_substrate::step::{NoProgress, StepOutcome};
    pub use tx_substrate::zone::Cap;
}
