//! Back-compat shim. The wake-substrate primitives now live in
//! `wake::mailbox` (in the substrate crate) per
//! [`docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`].
//!
//! This module re-exports them so existing `crate::mailbox::*` and
//! `tx_reactor::mailbox::*` paths inside the workspace continue to
//! resolve. New code should import directly via the substrate adapter.

pub use crate::adapter::bus_wire::mailbox::*;
