//! Back-compat shim. The wake-substrate `WaitSource` and related types
//! now live in `wake::wait_source` (in the substrate crate) per
//! [`docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`].
//!
//! This module re-exports them so existing `crate::wait_source::*` and
//! `tx_reactor::wait_source::*` paths inside the workspace continue to
//! resolve. New code should import directly via the substrate adapter.

pub use crate::adapter::bus_wire::wait_source::*;
