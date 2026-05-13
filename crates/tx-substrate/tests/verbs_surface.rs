//! Surface pin for `tx_substrate::verbs`. If a future refactor moves a
//! type out of the verbs catalogue, this test stops compiling — and
//! the author makes a deliberate choice about whether to relocate or
//! re-export.

#[allow(unused_imports)]
use tx_substrate::verbs::{
    // EBR
    drain_with_budget,
    guard,
    // Zone allocation
    reserve_for,
    sign,
    sign_for,
    // Sync
    AtomicSlot,
    // Step execution
    ByteProgress,
    Cap,
    Dead,
    Deadline,
    Entity,
    Errno,
    Guard,
    InterestMask,
    // Wake / mailbox
    MailboxEvent,
    NoProgress,
    OperationalCapExt,
    PayloadCap,
    // Bus wire
    RawPort,
    RawQueue,
    ScriptCtx,
    SignalRouting,
    SpinMutex,
    StepOp,
    StepOutcome,
    StepProgress,
    SubjectIdentity,
    TaskMailbox,
    WaitGeneration,
    WaitRegistrationGuard,
    WaitSource,
    WaitSourceId,
    Weak,
    YieldShape,
    Zone,
    ZoneAllocated,
    ZoneError,
};

#[test]
fn verbs_surface_compiles() {}

// Send + Sync pins for the types where that is part of the contract,
// following the convention established in crates/tx-substrate/tests/bus.rs.
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn verbs_send_sync() {
    assert_send_sync::<RawPort>();
    assert_send_sync::<RawQueue>();
    assert_send_sync::<WaitSource>();
    assert_send_sync::<TaskMailbox>();
}
