//! Back-compat shim. The wake-substrate primitives now live in
//! [`tx_substrate::wake::mailbox`] per
//! [`docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`].
//!
//! This module re-exports them so existing `crate::mailbox::*` and
//! `tx_reactor::mailbox::*` paths inside the workspace continue to
//! resolve. New code should import directly from
//! `tx_substrate::wake::mailbox`.

pub use tx_substrate::wake::mailbox::*;
