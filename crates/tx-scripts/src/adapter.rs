//! Substrate adapter for tx-scripts.
//!
//! Small surface — the exec script uses step_v3 outcome types, EBR
//! guard, and the zone Cap role type. Single `step_engine` domain.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, and EBR guard used by the tx-scripts exec script"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        AcceptOutcome, AgentCancelPolicy, ByteProgress, Deadline, DelegateEndpoint,
        DelegateRequest, DelegateToken, DriveMode, Errno, InterestMask, NoProgress,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        Translation, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::{page_allocator, SpinMutex};
}
