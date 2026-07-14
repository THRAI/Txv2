//! `tx-observe` kernel-side observation facade.
//!
//! The implementation is split by the L0-L3 kernel-side architecture:
//! - `l0_schema`: generated schema catalog from `schema/txobserve.toml`.
//! - `l1_probe_api`: callsite-facing probe macros and semantic API surface.
//! - `l2_producer`: bounded per-hart producer runtime.
//! - `l3_wire`: `txtrace-v0` payload encoding helpers.
//!
//! External callers keep using the stable crate-root API while implementation
//! ownership lives under the explicit layer directories.

#![no_std]

pub mod l0_schema;
mod l1_probe_api;
mod l2_producer;
pub mod l3_wire;

/// Compatibility alias for existing callers while L3 wire helpers live under
/// `l3_wire`.
pub mod encode {
    pub use crate::l3_wire::encode::*;
}

/// Compatibility alias for the generated L0 schema catalog.
pub mod generated {
    pub use crate::l0_schema::schema_catalog;
}

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(any(test, feature = "testing"))]
pub use l2_producer::testing_compact_ring_order;
#[cfg(any(test, feature = "testing"))]
pub(crate) use l2_producer::testing_reset;
pub use l2_producer::{
    clear_dump_request, clock_now_ns, current, current_parent_span, dump_console_hex,
    dump_console_hex_all, dump_registered_if_requested, fnv1a32, init, is_enabled,
    register_dump_shutdown, register_pre_dump_hook, request_dump, reset_all_rings_and_arm,
    reset_ring_and_arm, set_current_parent_span, set_dump_threshold, set_enabled,
    set_trace_off_requests_dump, should_dump_now, trace_off_requests_dump, AllocationTrack,
    EventNameId, HartEmitter, InitError, SpanId,
};

pub use tx_observe_types::TxTraceLevel;

// Re-exports used by the `traced_syscall!` macro so crates that use the macro
// do not need to directly depend on `tx-observe-types`.
#[doc(hidden)]
pub use tx_observe_types::{
    PayloadProcessFork, PayloadProcessGroup, PayloadProcessLabel, PayloadSyscallEnter,
    PayloadSyscallExit,
};
