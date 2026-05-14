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
//! * `wait_routing` — reactor: interrupt and wait primitives used by
//!   the D9-C interrupt-wake integration test (Channel, WaitOutcome,
//!   WaitProtocol, InterruptSource, InterruptSummary, Reactor).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "wake"],
    reason = "wrap substrate step engine (StepOp), EBR guard/guard(), and signal-routing primitives (SignalRouting, OperationalCapExt, SpinMutex) used by the signal mutators"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::wake::{MailboxEvent, SignalRouting, TaskMailbox};
    pub use tx_substrate::zone::{
        sign, sign_for, reserve_for, register_zone_for,
        Cap, PayloadCap, Weak, IdentRef,
        Dead, ZoneError,
        Entity, CoLocatedEntity, OperationalCapExt, OperationalRefExt, PayloadBinding,
        Zone, ZoneAllocated,
        IdentitySlot, IsPayloadPolicy, CapProducingPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor interrupt and wait primitives (InterruptSource, InterruptSummary, Channel, Mask, WaitOutcome, WaitProtocol, Reactor) used by the D9-C interrupt-wake integration test"
)]
pub mod wait_routing {
    pub use tx_reactor::interrupt::{InterruptSource, InterruptSummary};
    pub use tx_reactor::wait::{Channel, Mask, WaitOutcome, WaitProtocol};
    pub use tx_reactor::Reactor;
}
