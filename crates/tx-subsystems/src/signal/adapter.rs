//! Substrate adapter for signal.
//!
//! Signal touches substrate via three surfaces:
//!
//! * `step_v3` — `StepOp` impls for the kill / sigaction mutators.
//! * `epoch` — EBR `Guard` + `guard()` constructor, used by every
//!   signal mutator that walks process / pgrp slots.
//! * `wake::SignalRouting` + `zone::{Cap, OperationalCapExt}` +
//!   `SpinMutex` — the signal-routing primitives (per-process
//!   pending masks, etc.).
//!
//! Single adapter domain `step_engine` collects all of these — signal
//! has no reactor surface and the substrate primitives are tightly
//! coupled (every kill mutator reads under an EBR guard, looks up the
//! target's signal-routing slot, and updates state through Cap APIs).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch", "wake"],
    reason = "wrap substrate step engine (StepOp), EBR guard/guard(), and signal-routing primitives (SignalRouting, OperationalCapExt, SpinMutex) used by the signal mutators"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::wake::SignalRouting;
    pub use tx_substrate::zone::{Cap, OperationalCapExt};
    pub use tx_substrate::SpinMutex;
}
