//! Substrate / reactor adapter for signal.
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
//! Two adapter domains:
//! * `step_engine` — substrate: step_v3, epoch, wake, zone.
//! * `wait_routing` — reactor: interrupt summary and wait mask primitives
//!   used by the D9-C interrupt-wake integration test.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "wake"],
    reason = "wrap substrate step engine (StepOp), EBR guard/guard(), and signal-routing primitives (SignalRouting, OperationalCapExt, SpinMutex) used by the signal mutators"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, OneShotStepOp, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity,
    };
    pub use tx_substrate::wake::{MailboxEvent, SignalRouting, TaskMailbox};
    pub use tx_substrate::zone::{reserve_for, sign_for, Cap, OperationalCapExt, PayloadCap, Weak};
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor interrupt summary and wait mask primitives used by the D9-C interrupt-wake integration test"
)]
pub mod wait_routing {
    pub use tx_reactor::interrupt::InterruptSummary;
    pub use tx_reactor::wait::Mask;
}
