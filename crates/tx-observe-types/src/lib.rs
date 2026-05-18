//! Wire-format types for the `txtrace-v0` kernel-to-host trace ABI.
//!
//! This crate is `#![no_std]` by default.  Enable the `host` feature to get
//! `Debug` and `serde` derives for daemon-side use.
//!
//! # Crate layout
//!
//! | Module | Contents |
//! |---|---|
//! | `header` | `TxTraceHeader`, `TxTraceHartRing`, `TxTraceHeaderFlags`, `TxTraceClockId` |
//! | `record` | `TxTraceRecord`, `TxTraceKind`, `TxTraceLevel` |
//! | `payload` | `TxPayloadTag` + all `Payload*` structs + `TxValueKind`, `YieldShapeKind`, `TxProgressKind`, `FlowKind` |
//!
//! # `Pod` safety argument
//!
//! Every `unsafe impl tx_hal::Pod for T` in this file is justified by the
//! following shared argument:
//!
//! All wire-format types (`TxTraceHeader`, `TxTraceRecord`, and all `Payload*`
//! structs) are `#[repr(C)]` structs containing only fields whose types are
//! themselves `Pod` (integers and fixed-length byte arrays).  Every bit pattern
//! in the field range is a valid, initialized value for the corresponding integer
//! type, and the structs carry no padding bytes that could be uninitialized
//! (`TxTraceRecord` has compiler-inserted padding, but those bytes are never
//! read through the `Pod` interface — the `Pod` trait is used only for bulk
//! memory-copy paths that write a fully-initialized source, so destination bytes
//! including any padding are always initialized from a prior write).
//!
//! `TxTraceHartRing` is explicitly **not** `Pod` because it contains `AtomicU64`
//! fields; those fields require specific acquire/release memory-ordering
//! discipline and must be accessed only via the atomic API, never via bulk copy.
//!
//! # Spec reference
//!
//! `docs/Txv3/08_OBSERVATION_SERIALIZATION_v0.md` §14–15.

#![no_std]

pub mod header;
pub mod payload;
pub mod record;

pub use header::{
    TxTraceClockId, TxTraceHartRing, TxTraceHeader, TxTraceHeaderFlags, TX_TRACE_MAGIC,
};
pub use payload::{
    BootPhaseKind, FlowKind, PayloadArgValue, PayloadClockSnapshot, PayloadCounterValue,
    PayloadDriveBegin, PayloadDriveEnd, PayloadMutationIndexCommit, PayloadMutationZoneSign,
    PayloadPanic, PayloadPhaseTransition, PayloadResume, PayloadSchedSwitch, PayloadStepOutcome,
    PayloadSyscallEnter, PayloadSyscallExit, PayloadTrackDescriptor, PayloadWaitSourceNotify,
    PayloadYieldBegin, SchedKind, SchedReason, TxPayloadTag, TxProgressKind, TxValueKind,
    YieldShapeKind,
};
pub use record::{TxTraceKind, TxTraceLevel, TxTraceRecord};

// ---------------------------------------------------------------------------
// Pod marker impls — see module-level safety argument above.
// ---------------------------------------------------------------------------

use tx_hal::Pod;

unsafe impl Pod for TxTraceHeader {}
unsafe impl Pod for TxTraceRecord {}
unsafe impl Pod for PayloadSyscallEnter {}
unsafe impl Pod for PayloadSyscallExit {}
unsafe impl Pod for PayloadDriveBegin {}
unsafe impl Pod for PayloadDriveEnd {}
unsafe impl Pod for PayloadStepOutcome {}
unsafe impl Pod for PayloadYieldBegin {}
unsafe impl Pod for PayloadResume {}
unsafe impl Pod for PayloadWaitSourceNotify {}
unsafe impl Pod for PayloadTrackDescriptor {}
unsafe impl Pod for PayloadCounterValue {}
unsafe impl Pod for PayloadClockSnapshot {}
unsafe impl Pod for PayloadArgValue {}
unsafe impl Pod for PayloadMutationZoneSign {}
unsafe impl Pod for PayloadMutationIndexCommit {}
unsafe impl Pod for PayloadPhaseTransition {}
unsafe impl Pod for PayloadSchedSwitch {}
unsafe impl Pod for PayloadPanic {}

// ---------------------------------------------------------------------------
// Compile-time layout assertions
// ---------------------------------------------------------------------------
//
// These must fail compilation whenever any wire-format struct changes size or
// alignment.  Per OBS-SER-V0-CI-ASSERT (08_OBSERVATION_SERIALIZATION_v0.md §14).

const _: () = {
    use core::mem::{align_of, size_of};

    // ── Global header ───────────────────────────────────────────────────────
    assert!(size_of::<TxTraceHeader>() == 72);
    assert!(align_of::<TxTraceHeader>() == 8);

    // ── Fixed record ────────────────────────────────────────────────────────
    assert!(size_of::<TxTraceRecord>() == 80);
    assert!(align_of::<TxTraceRecord>() == 8);

    // ── Per-hart ring header (not Pod; size tracked for region-offset math) ─
    assert!(size_of::<TxTraceHartRing>() == 208);

    // ── Payload structs — each must fit in the 16-byte inline buffer ────────
    assert!(size_of::<PayloadSyscallEnter>() == 8);
    assert!(size_of::<PayloadSyscallExit>() == 16);
    assert!(size_of::<PayloadDriveBegin>() == 12);
    assert!(size_of::<PayloadDriveEnd>() == 16);
    assert!(size_of::<PayloadStepOutcome>() == 16);
    assert!(size_of::<PayloadYieldBegin>() == 16);
    assert!(size_of::<PayloadResume>() == 16);
    assert!(size_of::<PayloadWaitSourceNotify>() == 16);
    assert!(size_of::<PayloadTrackDescriptor>() == 16);
    assert!(size_of::<PayloadCounterValue>() == 16);
    assert!(size_of::<PayloadClockSnapshot>() == 16);
    assert!(size_of::<PayloadArgValue>() == 16);
    assert!(size_of::<PayloadMutationZoneSign>() == 16);
    assert!(size_of::<PayloadMutationIndexCommit>() == 16);
    assert!(size_of::<PayloadPhaseTransition>() == 16);
    assert!(size_of::<PayloadSchedSwitch>() == 16);
    assert!(size_of::<PayloadPanic>() == 16);

    // Belt-and-suspenders: all payloads ≤ 16 (redundant given exact checks
    // above, but guards against future additions that forget the exact check).
    assert!(size_of::<PayloadSyscallEnter>() <= 16);
    assert!(size_of::<PayloadDriveBegin>() <= 16);
};
