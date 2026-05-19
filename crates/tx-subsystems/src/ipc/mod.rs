//! SysV + POSIX IPC subsystem family.
//!
//! Canonical spec: `docs/Txv3/08_SYSV_IPC_v1.md`.
//!
//! Module layout per `SUBSYSTEM_ANATOMY_v2_1.md`: each IPC kind is a
//! four-module subsystem (structure / execution / checks / projection).
//! The `namespace/` module houses namespace-level operations.
//!
//! All blocking operations yield `YieldShape::OnWaitSource` with a
//! `PreparedPredicate`. Zero new closed-catalog members. Zero new
//! framework cells.

pub mod namespace;
pub mod posix_mq;
pub mod sysv_msg;
pub mod sysv_sem;
pub mod sysv_shm;

// Re-export key types for crate-internal consumers.
pub use namespace::structure::IpcLimits;
pub use sysv_msg::structure::{MsgQueueIdentity, MsgQueuePayload};
pub use sysv_sem::structure::{SemArrayIdentity, SemArrayPayload};
pub use sysv_shm::structure::IpcPerm;
pub use sysv_shm::structure::{ShmSegmentIdentity, ShmSegmentPayload};

/// Register all IPC subsystem zones. Called from
/// `crate::zones::register_all`.
pub(crate) fn register_zones() -> Result<(), crate::process::adapter::step_engine::ZoneError> {
    sysv_shm::structure::register_zones()?;
    sysv_msg::structure::register_zones()?;
    sysv_sem::structure::register_zones()?;
    posix_mq::structure::register_zones()?;
    Ok(())
}
