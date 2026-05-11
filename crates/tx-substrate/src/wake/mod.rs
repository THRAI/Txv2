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
//! existing `tx_reactor::TaskMailbox` paths continue to resolve.

pub mod mailbox;
pub mod timer;
pub mod wait_source;

pub use mailbox::{
    agent_event_matches, ActiveWait, MailboxEvent, SignalRouting, TaskMailbox, WaitGeneration,
    MAILBOX_QUEUE_BOUND,
};
pub use timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};
pub use wait_source::{
    PreparedWaitRegistration, SubscriberId, WaitRegistrationGuard, WaitSource,
};
