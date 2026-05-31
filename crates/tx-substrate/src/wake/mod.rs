//! Wake substrate primitives.
//!
//! Per [`docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`]
//! and [`docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md`],
//! the wake substrate (`TaskMailbox`, `WaitSource`, `ActiveWait`, and
//! related primitives) lives **below** the reactor task abstraction.
//! Bus, generation, mailbox, and source all share the same architectural
//! layer here so that semantic objects (pipes, futexes, exit channels,
//! …) can publish wake events without a back-edge into `tx-reactor`.
//!
//! ## Layout
//!
//! - [`mailbox`] — task-owned wake delivery (`TaskMailbox`,
//!   `MailboxEvent`, `WaitGeneration`, `ActiveWait`).
//! - [`wait_source`] — object-owned wait publication (`WaitSource`,
//!   `PreparedWaitRegistration`, `WaitRegistrationGuard`, `SubscriberId`).
//! - [`timer`] — role-tagged deadline registry (`TimerWheel`,
//!   `TimerGuard`, `TimerToken`, `TimerGuardRole`). Relocated from
//!   `tx-reactor::timer` per
//!   [`docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md`].
//!
//! `tx-reactor` re-exports these at its crate root for back-compat;
//! existing reactor `TaskMailbox` paths continue to resolve.
//!
//! ## Canonical free-function verbs
//!
//! [`new_source`] and [`notify`] are the promoted canonical verbs that
//! replace the per-subsystem `wait_routing::new_wait_source` and
//! `wait_routing::notify_v3_source` wrappers. Subsystem adapters
//! delegate to these; observation hooks attach here in one place.

extern crate alloc;

use alloc::sync::Arc;

pub mod mailbox;
pub mod timer;
pub mod wait_source;

pub use mailbox::{
    agent_event_matches, ActiveWait, MailboxEvent, MailboxSchedulerHint, SignalRouting,
    TaskMailbox, WaitGeneration, MAILBOX_QUEUE_BOUND,
};
pub use timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};
pub use wait_source::{
    lookup_source, register_source, unregister_source, PreparedWaitRegistration, SubscriberId,
    WaitRegistrationGuard, WaitSource,
};

/// Construct a new `WaitSource` wrapped in an `Arc`, keyed by `id`.
///
/// Replaces the per-subsystem adapter pattern
/// `Arc::new(WaitSource::new(WaitSourceId::new(id)))`.
/// Future observation hooks (tracing, metrics) attach here in one place.
pub fn new_source(id: u64) -> Arc<WaitSource> {
    use crate::step::WaitSourceId;
    Arc::new(WaitSource::new(WaitSourceId::new(id)))
}

/// Notify a `WaitSource` with a raw interest-mask value.
///
/// Replaces the per-subsystem adapter pattern
/// `source.notify(InterestMask::new(mask_bits))`.
/// Future observation hooks attach here in one place.
pub fn notify(source: &WaitSource, mask_bits: u64) {
    use crate::step::InterestMask;
    source.notify(InterestMask::new(mask_bits));
}
