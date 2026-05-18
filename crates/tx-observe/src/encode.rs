//! Payload byte-encoding helpers for OBS-3a.
//!
//! Each helper takes a materialized `Payload*` struct, serializes it into a
//! `[u8; 16]` inline buffer, and returns `(TxPayloadTag, [u8; 16], payload_len)`.
//!
//! Rules (OBS-2 / OBS-13):
//! - No bytemuck — we use direct field-by-field copies into the buffer.
//! - No allocation — all buffers are stack-local.
//! - No `Debug` / `Serialize` — kernel-side only; host enables those via the
//!   `host` feature on `tx-observe-types`.
//!
//! Spec ref: `08_OBSERVATION_SERIALIZATION_v0.md` §8.

use tx_observe_types::{
    PayloadArgValue, PayloadDriveBegin, PayloadDriveEnd, PayloadMutationIndexCommit,
    PayloadMutationZoneSign, PayloadPhaseTransition, PayloadResume, PayloadSchedSwitch,
    PayloadStepOutcome, PayloadSyscallEnter, PayloadSyscallExit, PayloadWaitSourceNotify,
    PayloadYieldBegin, TxPayloadTag,
};

// ---------------------------------------------------------------------------
// Internal: write a little-endian integer at byte offset `off` in `buf`.
// ---------------------------------------------------------------------------

#[inline(always)]
fn write_u8(buf: &mut [u8; 16], off: usize, v: u8) {
    buf[off] = v;
}

#[inline(always)]
fn write_u16_le(buf: &mut [u8; 16], off: usize, v: u16) {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
}

#[inline(always)]
fn write_u32_le(buf: &mut [u8; 16], off: usize, v: u32) {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    buf[off + 2] = b[2];
    buf[off + 3] = b[3];
}

#[inline(always)]
fn write_i32_le(buf: &mut [u8; 16], off: usize, v: i32) {
    write_u32_le(buf, off, v as u32);
}

#[inline(always)]
fn write_u64_le(buf: &mut [u8; 16], off: usize, v: u64) {
    let b = v.to_le_bytes();
    buf[off] = b[0];
    buf[off + 1] = b[1];
    buf[off + 2] = b[2];
    buf[off + 3] = b[3];
    buf[off + 4] = b[4];
    buf[off + 5] = b[5];
    buf[off + 6] = b[6];
    buf[off + 7] = b[7];
}

#[inline(always)]
fn write_i64_le(buf: &mut [u8; 16], off: usize, v: i64) {
    write_u64_le(buf, off, v as u64);
}

// ---------------------------------------------------------------------------
// L0 — Syscall payloads
// ---------------------------------------------------------------------------

/// Encode a [`PayloadSyscallEnter`] into a 16-byte buffer.
///
/// Wire layout (§8.1):
/// ```text
/// offset 0: sysno  u32  LE
/// offset 4: abi    u16  LE
/// offset 6: argc   u16  LE
/// total = 8
/// ```
#[inline]
pub fn encode_syscall_enter(p: &PayloadSyscallEnter) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.sysno);
    write_u16_le(&mut buf, 4, p.abi);
    write_u16_le(&mut buf, 6, p.argc);
    let len = core::mem::size_of::<PayloadSyscallEnter>() as u16;
    (buf, len)
}

/// Encode a [`PayloadSyscallExit`] into a 16-byte buffer.
///
/// Wire layout (§8.1):
/// ```text
/// offset 0:  ret         i64  LE
/// offset 8:  errno       i32  LE
/// offset 12: result_kind u8
/// offset 13: _pad        [u8; 3]
/// total = 16
/// ```
#[inline]
pub fn encode_syscall_exit(p: &PayloadSyscallExit) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_i64_le(&mut buf, 0, p.ret);
    write_i32_le(&mut buf, 8, p.errno);
    write_u8(&mut buf, 12, p.result_kind);
    // _pad bytes stay 0
    let len = core::mem::size_of::<PayloadSyscallExit>() as u16;
    (buf, len)
}

// ---------------------------------------------------------------------------
// L2 — Drive payloads
// ---------------------------------------------------------------------------

/// Encode a [`PayloadDriveBegin`] into a 16-byte buffer.
///
/// Wire layout (§8.2):
/// ```text
/// offset 0: op_type      u32  LE
/// offset 4: mode         u8
/// offset 5: interrupt    u8
/// offset 6: has_deadline u8
/// offset 7: _pad         u8
/// offset 8: task_id_low  u32  LE
/// total = 12
/// ```
#[inline]
pub fn encode_drive_begin(p: &PayloadDriveBegin) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.op_type);
    write_u8(&mut buf, 4, p.mode);
    write_u8(&mut buf, 5, p.interrupt);
    write_u8(&mut buf, 6, p.has_deadline);
    // _pad at 7 stays 0
    write_u32_le(&mut buf, 8, p.task_id_low);
    let len = core::mem::size_of::<PayloadDriveBegin>() as u16;
    (buf, len)
}

/// Encode a [`PayloadDriveEnd`] into a 16-byte buffer.
///
/// Wire layout (§8.2):
/// ```text
/// offset 0:  ret         i64  LE
/// offset 8:  errno       i32  LE
/// offset 12: result_kind u8
/// offset 13: _pad        [u8; 3]
/// total = 16
/// ```
#[inline]
pub fn encode_drive_end(p: &PayloadDriveEnd) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_i64_le(&mut buf, 0, p.ret);
    write_i32_le(&mut buf, 8, p.errno);
    write_u8(&mut buf, 12, p.result_kind);
    // _pad bytes stay 0
    let len = core::mem::size_of::<PayloadDriveEnd>() as u16;
    (buf, len)
}

// ---------------------------------------------------------------------------
// L4 — Step outcome payload
// ---------------------------------------------------------------------------

/// Encode a [`PayloadStepOutcome`] into a 16-byte buffer.
///
/// Wire layout (§8.3):
/// ```text
/// offset 0: variant        u8
/// offset 1: progress_empty u8
/// offset 2: progress_kind  u8
/// offset 3: shape_kind     u8
/// offset 4: errno          i32  LE
/// offset 8: progress_value u32  LE
/// offset 12: _pad          u32
/// total = 16
/// ```
#[inline]
pub fn encode_step_outcome(p: &PayloadStepOutcome) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u8(&mut buf, 0, p.variant);
    write_u8(&mut buf, 1, p.progress_empty);
    write_u8(&mut buf, 2, p.progress_kind);
    write_u8(&mut buf, 3, p.shape_kind);
    write_i32_le(&mut buf, 4, p.errno);
    write_u32_le(&mut buf, 8, p.progress_value);
    // _pad at 12-15 stays 0
    let len = core::mem::size_of::<PayloadStepOutcome>() as u16;
    (buf, len)
}

// ---------------------------------------------------------------------------
// Convenience: tag+bytes pair for passing to span_begin/span_end
// ---------------------------------------------------------------------------

/// Return the correct `TxPayloadTag` for a `PayloadSyscallEnter`.
#[inline]
pub const fn syscall_enter_tag() -> TxPayloadTag {
    TxPayloadTag::SyscallEnter
}

/// Return the correct `TxPayloadTag` for a `PayloadSyscallExit`.
#[inline]
pub const fn syscall_exit_tag() -> TxPayloadTag {
    TxPayloadTag::SyscallExit
}

/// Return the correct `TxPayloadTag` for a `PayloadDriveBegin`.
#[inline]
pub const fn drive_begin_tag() -> TxPayloadTag {
    TxPayloadTag::DriveBegin
}

/// Return the correct `TxPayloadTag` for a `PayloadDriveEnd`.
#[inline]
pub const fn drive_end_tag() -> TxPayloadTag {
    TxPayloadTag::DriveEnd
}

/// Return the correct `TxPayloadTag` for a `PayloadStepOutcome`.
#[inline]
pub const fn step_outcome_tag() -> TxPayloadTag {
    TxPayloadTag::StepOutcome
}

// ---------------------------------------------------------------------------
// L3 — WaitSource notify (producer-side wake observation, OBS-4)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadWaitSourceNotify`] into a 16-byte buffer.
///
/// Wire layout (§8.4 / OBS-4):
/// ```text
/// offset 0:  source_id_low       u32  LE   — low 32 bits of WaitSourceId
/// offset 4:  mask_bits           u32  LE   — interest overlap bits fired
/// offset 8:  task_id_low         u32  LE   — low 32 bits of woken task id
/// offset 12: wait_generation_low u32  LE   — low 32 bits of WaitGeneration
/// total = 16
/// ```
#[inline]
pub fn encode_wait_source_notify(p: &PayloadWaitSourceNotify) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.source_id_low);
    write_u32_le(&mut buf, 4, p.mask_bits);
    write_u32_le(&mut buf, 8, p.task_id_low);
    write_u32_le(&mut buf, 12, p.wait_generation_low);
    let len = core::mem::size_of::<PayloadWaitSourceNotify>() as u16;
    (buf, len)
}

/// Return the correct `TxPayloadTag` for a `PayloadWaitSourceNotify`.
#[inline]
pub const fn wait_source_notify_tag() -> TxPayloadTag {
    TxPayloadTag::WaitSourceNotify
}

// ---------------------------------------------------------------------------
// L6 — Mutation payloads (OBS-8)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadMutationZoneSign`] into a 16-byte buffer.
///
/// Wire layout (`08_OBSERVATION_SERIALIZATION_v0.md` §8.7):
/// ```text
/// offset 0: object_id  u64  LE   — TraceObjectId packed form
/// offset 8: kind       u8        — ZoneKindTag discriminant
/// offset 9: _pad       [u8; 7]
/// total = 16
/// ```
#[inline]
pub fn encode_mutation_zone_sign(p: &PayloadMutationZoneSign) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u64_le(&mut buf, 0, p.object_id);
    write_u8(&mut buf, 8, p.kind);
    // _pad bytes stay 0
    let len = core::mem::size_of::<PayloadMutationZoneSign>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadMutationZoneSign`].
#[inline]
pub const fn mutation_zone_sign_tag() -> TxPayloadTag {
    TxPayloadTag::MutationZoneSign
}

/// Encode a [`PayloadMutationIndexCommit`] into a 16-byte buffer.
///
/// Wire layout (`08_OBSERVATION_SERIALIZATION_v0.md` §8.7):
/// ```text
/// offset 0:  index_id         u32  LE
/// offset 4:  key_low          u32  LE
/// offset 8:  value_object_id  u64  LE   — Cap<T> committed under the key
/// total = 16
/// ```
#[inline]
pub fn encode_mutation_index_commit(p: &PayloadMutationIndexCommit) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.index_id);
    write_u32_le(&mut buf, 4, p.key_low);
    write_u64_le(&mut buf, 8, p.value_object_id);
    let len = core::mem::size_of::<PayloadMutationIndexCommit>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadMutationIndexCommit`].
#[inline]
pub const fn mutation_index_commit_tag() -> TxPayloadTag {
    TxPayloadTag::MutationIndexCommit
}

// ---------------------------------------------------------------------------
// L5 — Phase transition payload (OBS-8)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadPhaseTransition`] into a 16-byte buffer.
///
/// Wire layout (`08_OBSERVATION_SERIALIZATION_v0.md` §8 OBS-8):
/// ```text
/// offset 0:  phase_kind  u8
/// offset 1:  hart_id     u8
/// offset 2:  _pad        [u8; 14]
/// total = 16
/// ```
#[inline]
pub fn encode_phase_transition(p: &PayloadPhaseTransition) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u8(&mut buf, 0, p.phase_kind);
    write_u8(&mut buf, 1, p.hart_id);
    // _pad bytes stay 0
    let len = core::mem::size_of::<PayloadPhaseTransition>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadPhaseTransition`].
#[inline]
pub const fn phase_transition_tag() -> TxPayloadTag {
    TxPayloadTag::PhaseTransition
}

// ---------------------------------------------------------------------------
// L7 — SchedSwitch (OBS-9 reactor scheduler track)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadSchedSwitch`] into a 16-byte buffer.
///
/// Wire layout (`08_OBSERVATION_v1.md` §15.6 OBS-9):
/// ```text
/// offset 0:  task_id_low  u32
/// offset 4:  hart_id      u8
/// offset 5:  kind         u8   (0 = Dispatch, 1 = Yield)
/// offset 6:  reason       u8   (0..3, see SchedReason)
/// offset 7:  _pad         [u8; 9]
/// total = 16
/// ```
#[inline]
pub fn encode_sched_switch(p: &PayloadSchedSwitch) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.task_id_low);
    write_u8(&mut buf, 4, p.hart_id);
    write_u8(&mut buf, 5, p.kind);
    write_u8(&mut buf, 6, p.reason);
    // _pad bytes stay 0
    let len = core::mem::size_of::<PayloadSchedSwitch>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadSchedSwitch`].
#[inline]
pub const fn sched_switch_tag() -> TxPayloadTag {
    TxPayloadTag::SchedSwitch
}

// ---------------------------------------------------------------------------
// ArgValue (continuation records — `08_OBSERVATION_v1.md` §8.6)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadArgValue`] into a 16-byte buffer.
///
/// Wire layout (§8.6):
/// ```text
/// offset 0:  key         u32   (DebugAnnotationNameId; daemon → "fd", "buf", …)
/// offset 4:  value_kind  u8    (TxValueKind: 1=U64, 2=I64, 4=Ptr, 5=Errno, …)
/// offset 5:  _pad        [u8; 3]
/// offset 8:  value0      u64   (numeric value, low-64 of object id, etc.)
/// total = 16
/// ```
#[inline]
pub fn encode_arg_value(p: &PayloadArgValue) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u32_le(&mut buf, 0, p.key);
    write_u8(&mut buf, 4, p.value_kind);
    // _pad bytes stay 0
    write_u64_le(&mut buf, 8, p.value0);
    let len = core::mem::size_of::<PayloadArgValue>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadArgValue`].
#[inline]
pub const fn arg_value_tag() -> TxPayloadTag {
    TxPayloadTag::ArgValue
}

// ---------------------------------------------------------------------------
// L3 — YieldBegin / Resume (OBS-3b)
// ---------------------------------------------------------------------------

/// Encode a [`PayloadYieldBegin`] into a 16-byte buffer.
///
/// Wire layout (§8.4):
/// ```text
/// offset 0:  shape_kind     u8
/// offset 1:  _pad           [u8; 3]
/// offset 4:  task_id_low    u32  LE
/// offset 8:  wait_generation u64 LE
/// total = 16
/// ```
#[inline]
pub fn encode_yield_begin(p: &PayloadYieldBegin) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u8(&mut buf, 0, p.shape_kind);
    // _pad at 1-3 stays 0
    write_u32_le(&mut buf, 4, p.task_id_low);
    write_u64_le(&mut buf, 8, p.wait_generation);
    let len = core::mem::size_of::<PayloadYieldBegin>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadYieldBegin`].
#[inline]
pub const fn yield_begin_tag() -> TxPayloadTag {
    TxPayloadTag::YieldBegin
}

/// Encode a [`PayloadResume`] into a 16-byte buffer.
///
/// Wire layout (§8.4):
/// ```text
/// offset 0:  resume_kind    u8
/// offset 1:  abort_reason   u8
/// offset 2:  _pad           [u8; 2]
/// offset 4:  object_id_low  u32  LE
/// offset 8:  wait_generation u64 LE
/// total = 16
/// ```
#[inline]
pub fn encode_resume(p: &PayloadResume) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u8(&mut buf, 0, p.resume_kind);
    write_u8(&mut buf, 1, p.abort_reason);
    // _pad at 2-3 stays 0
    write_u32_le(&mut buf, 4, p.object_id_low);
    write_u64_le(&mut buf, 8, p.wait_generation);
    let len = core::mem::size_of::<PayloadResume>() as u16;
    (buf, len)
}

/// Return the correct [`TxPayloadTag`] for a [`PayloadResume`].
#[inline]
pub const fn resume_tag() -> TxPayloadTag {
    TxPayloadTag::Resume
}
