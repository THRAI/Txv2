//! Curated re-export module: the named verbs and types adapters reach for.
//!
//! Adding to this module is a convergence-layer design decision. If a
//! type or function is needed by 2+ adapters across different domains,
//! it belongs here. If only one adapter needs it, keep it in its home
//! module instead.
//!
//! This module is self-documenting: when writing a new adapter, the
//! answer to "what's the named verb for X?" is "look in
//! `tx_substrate::verbs`."
//!
//! **Exclusions:**
//! - Bus macros (`bus_lifecycle!`, `bus_readiness!`) — `#[macro_export]`
//!   semantics mean the per-adapter `pub use` is already the canonical
//!   path; collecting them here adds no ergonomic value.
//! - Entire-module wildcards — that's not curation.
//! - Items used by only one adapter (subsystem-specific).

// ── Step execution ─────────────────────────────────────────────────────────
// Core step-engine types used by virtually every subsystem adapter.
pub use crate::step::{
    ByteProgress, Deadline, Errno, InterestMask, NoProgress, ScriptCtx, StepOp, StepOutcome,
    StepProgress, SubjectIdentity, WaitSourceId, YieldShape,
};

// ── Zone allocation ────────────────────────────────────────────────────────
// Role-typed allocation primitives: `sign` is the one-step convenience that
// collapses `reserve_for + sign_for`; the two-step pair is still available
// when resources must be reserved before value construction.
pub use crate::zone::{
    reserve_for, sign, sign_for, Cap, Dead, Entity, OperationalCapExt, PayloadCap, Weak, Zone,
    ZoneAllocated, ZoneError,
};

// ── EBR ────────────────────────────────────────────────────────────────────
// Enter an epoch (guard) and drain the retired pool (drain_with_budget).
pub use crate::epoch::{drain_with_budget, guard, Guard};

// ── Wake / mailbox ─────────────────────────────────────────────────────────
// Object-side wake publication (WaitSource, registration guards) and
// task-side mailbox delivery (TaskMailbox, MailboxEvent, SignalRouting).
pub use crate::wake::{
    MailboxEvent, SignalRouting, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
};

// ── Bus wire ───────────────────────────────────────────────────────────────
// Raw port and queue primitives used by tty identity bus paths and the
// reactor bus-wire adapter.
pub use crate::bus::{RawPort, RawQueue};

// ── Sync ───────────────────────────────────────────────────────────────────
// In-kernel lock and atomic-slot primitives used across subsystem structures.
pub use crate::sync::SpinMutex;
pub use crate::slot::AtomicSlot;
