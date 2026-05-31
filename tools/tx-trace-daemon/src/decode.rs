//! Record and payload decoding — binary → typed [`DecodedEvent`].
//!
//! # Memory-safety note on `ptr::read_unaligned`
//!
//! `TxTraceRecord` and all `Payload*` structs are `#[repr(C)]` POD types
//! (marked `unsafe impl tx_hal::Pod`).  The record buffer is a `&[u8]` slice
//! whose length is exactly `size_of::<TxTraceRecord>()` (80 bytes), guaranteed
//! by the caller.  `ptr::read_unaligned` is safe here because:
//!   1. The source pointer is valid and in-bounds (the slice covers the bytes).
//!   2. The type is `Copy + #[repr(C)]` with no padding bytes that could be
//!      uninit — the kernel always writes the full record before advancing the
//!      producer index (Release store), so all 80 bytes are initialized by the
//!      time we read them.
//!   3. We do not require the pointer to be aligned; `read_unaligned` handles
//!      any alignment.
//! The same argument applies to the 16-byte payload sub-slice.

use serde::Serialize;
use std::mem::size_of;
use tx_observe_types::{payload::*, TxTraceKind, TxTraceLevel, TxTraceRecord};

/// The record-magic constant per the spec (§5).
pub const RECORD_MAGIC: u16 = 0x5254;

/// The only supported record version.
const SUPPORTED_VERSION: u8 = 0;

/// A fully decoded event ready for JSON emission.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum DecodedEvent {
    /// A successfully decoded trace record.
    Record(DecodedRecord),
    /// A synthetic repair marker for a framing/decode error.
    Repair(RepairRecord),
}

/// A successfully decoded trace record.
#[derive(Debug, Clone, Serialize)]
pub struct DecodedRecord {
    pub hart: u16,
    pub seq: u64,
    pub ts: u64,
    pub kind: &'static str,
    pub level: &'static str,
    /// Span id as `"0x<hex>"`.
    pub span: String,
    /// Parent span id as `"0x<hex>"`.
    pub parent: String,
    /// Event name id as `"0x<hex>"`.
    pub name_id: String,
    /// Raw `TxPayloadTag` discriminant — used by the Perfetto writer to route
    /// `WaitSourceNotify` and `Resume` instants through the flow-reconstruction
    /// path (OBS-3b / OBS-4).
    pub payload_tag: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

/// Repair marker emitted on any framing or decode error (§10 of the host doc).
#[derive(Debug, Clone, Serialize)]
pub struct RepairRecord {
    pub category: &'static str,
    pub hart: u16,
    pub seq_around: u64,
    pub details: String,
}

impl RepairRecord {
    pub fn bad_magic(hart: u16, seq_around: u64, got: u16) -> Self {
        Self {
            category: "txtrace.repair.bad_magic",
            hart,
            seq_around,
            details: format!("expected 0x{RECORD_MAGIC:04x} got 0x{got:04x}"),
        }
    }

    pub fn version_mismatch(hart: u16, seq_around: u64, got: u8) -> Self {
        Self {
            category: "txtrace.repair.version_mismatch",
            hart,
            seq_around,
            details: format!("expected version {SUPPORTED_VERSION} got {got}"),
        }
    }

    pub fn payload_len_exceeded(hart: u16, seq_around: u64, len: u16) -> Self {
        Self {
            category: "txtrace.repair.payload_len_exceeded",
            hart,
            seq_around,
            details: format!("payload_len {len} exceeds 16-byte inline buffer"),
        }
    }

    pub fn payload_tag_unknown(hart: u16, seq_around: u64, tag: u16) -> Self {
        Self {
            category: "txtrace.repair.payload_tag_unknown",
            hart,
            seq_around,
            details: format!("unknown payload_tag 0x{tag:04x}"),
        }
    }
}

/// Decode one 80-byte raw record slot into a [`DecodedEvent`].
///
/// `hart` is the ring index; `slot_bytes` must be exactly 80 bytes.
pub fn decode_slot(hart: u16, slot_bytes: &[u8]) -> DecodedEvent {
    assert_eq!(
        slot_bytes.len(),
        size_of::<TxTraceRecord>(),
        "slot_bytes must be exactly {} bytes",
        size_of::<TxTraceRecord>(),
    );

    // Safety: see module-level doc comment.
    let rec: TxTraceRecord =
        unsafe { std::ptr::read_unaligned(slot_bytes.as_ptr() as *const TxTraceRecord) };

    // Per-record magic check.
    if rec.magic != RECORD_MAGIC {
        return DecodedEvent::Repair(RepairRecord::bad_magic(hart, rec.seq, rec.magic));
    }

    // Version check.
    if rec.version != SUPPORTED_VERSION {
        return DecodedEvent::Repair(RepairRecord::version_mismatch(hart, rec.seq, rec.version));
    }

    // Payload len bound.
    if rec.payload_len > 16 {
        return DecodedEvent::Repair(RepairRecord::payload_len_exceeded(
            hart,
            rec.seq,
            rec.payload_len,
        ));
    }

    let kind_str = kind_name(rec.kind);
    let level_str = level_name(rec.level);

    // Decode payload.
    let payload_result = decode_payload(rec.payload_tag, &rec.payload[..rec.payload_len as usize]);

    let payload = match payload_result {
        Ok(p) => p,
        Err(_) => {
            return DecodedEvent::Repair(RepairRecord::payload_tag_unknown(
                hart,
                rec.seq,
                rec.payload_tag,
            ));
        }
    };

    DecodedEvent::Record(DecodedRecord {
        hart: rec.hart,
        seq: rec.seq,
        ts: rec.ts,
        kind: kind_str,
        level: level_str,
        span: format!("0x{:x}", rec.span),
        parent: format!("0x{:x}", rec.parent),
        name_id: format!("0x{:x}", rec.name),
        payload_tag: rec.payload_tag,
        payload,
    })
}

/// Map `TxTraceKind` raw byte to a static string name.
fn kind_name(kind: u8) -> &'static str {
    match kind {
        k if k == TxTraceKind::Nop as u8 => "Nop",
        k if k == TxTraceKind::ClockSnapshot as u8 => "ClockSnapshot",
        k if k == TxTraceKind::TrackDescriptor as u8 => "TrackDescriptor",
        k if k == TxTraceKind::StringDescriptor as u8 => "StringDescriptor",
        k if k == TxTraceKind::SpanBegin as u8 => "SpanBegin",
        k if k == TxTraceKind::SpanEnd as u8 => "SpanEnd",
        k if k == TxTraceKind::Instant as u8 => "Instant",
        k if k == TxTraceKind::Counter as u8 => "Counter",
        k if k == TxTraceKind::TrackTombstone as u8 => "TrackTombstone",
        k if k == TxTraceKind::PanicMarker as u8 => "PanicMarker",
        k if k == TxTraceKind::ArgContinuation as u8 => "ArgContinuation",
        _ => "Unknown",
    }
}

/// Map `TxTraceLevel` raw byte to a static string name.
fn level_name(level: u8) -> &'static str {
    match level {
        l if l == TxTraceLevel::Boundary as u8 => "Boundary",
        l if l == TxTraceLevel::Script as u8 => "Script",
        l if l == TxTraceLevel::Drive as u8 => "Drive",
        l if l == TxTraceLevel::Yield as u8 => "Yield",
        l if l == TxTraceLevel::Step as u8 => "Step",
        l if l == TxTraceLevel::Phase as u8 => "Phase",
        l if l == TxTraceLevel::Mutation as u8 => "Mutation",
        l if l == TxTraceLevel::Sched as u8 => "Sched",
        _ => "Unknown",
    }
}

/// Decode the payload bytes by tag.
///
/// Returns `Ok(None)` for known-zero-payload tags (`None`, unknown-but-valid).
/// Returns `Err(())` for truly unknown tags that should become a repair marker.
///
/// Per spec (§8): unknown payload_tag with payload_len <= 16 is skipped (the
/// record kind is still consumed).  We return `Ok(None)` in that case and let
/// the caller emit the record without a payload rather than emitting a repair.
fn decode_payload(tag: u16, bytes: &[u8]) -> Result<Option<serde_json::Value>, ()> {
    // Map tag to TxPayloadTag.
    let parsed_tag = match tag {
        t if t == TxPayloadTag::None as u16 => return Ok(None),
        t if t == TxPayloadTag::SyscallEnter as u16 => TxPayloadTag::SyscallEnter,
        t if t == TxPayloadTag::SyscallExit as u16 => TxPayloadTag::SyscallExit,
        t if t == TxPayloadTag::DriveBegin as u16 => TxPayloadTag::DriveBegin,
        t if t == TxPayloadTag::DriveEnd as u16 => TxPayloadTag::DriveEnd,
        t if t == TxPayloadTag::StepOutcome as u16 => TxPayloadTag::StepOutcome,
        t if t == TxPayloadTag::YieldBegin as u16 => TxPayloadTag::YieldBegin,
        t if t == TxPayloadTag::Resume as u16 => TxPayloadTag::Resume,
        t if t == TxPayloadTag::WaitSourceNotify as u16 => TxPayloadTag::WaitSourceNotify,
        t if t == TxPayloadTag::AgentStateChange as u16 => TxPayloadTag::AgentStateChange,
        t if t == TxPayloadTag::TrackDescriptor as u16 => TxPayloadTag::TrackDescriptor,
        t if t == TxPayloadTag::CounterValue as u16 => TxPayloadTag::CounterValue,
        t if t == TxPayloadTag::StringDescriptor as u16 => TxPayloadTag::StringDescriptor,
        t if t == TxPayloadTag::ClockSnapshot as u16 => TxPayloadTag::ClockSnapshot,
        t if t == TxPayloadTag::ArgValue as u16 => TxPayloadTag::ArgValue,
        t if t == TxPayloadTag::MutationZoneSign as u16 => TxPayloadTag::MutationZoneSign,
        t if t == TxPayloadTag::MutationIndexCommit as u16 => TxPayloadTag::MutationIndexCommit,
        t if t == TxPayloadTag::PhaseTransition as u16 => TxPayloadTag::PhaseTransition,
        t if t == TxPayloadTag::SchedSwitch as u16 => TxPayloadTag::SchedSwitch,
        t if t == TxPayloadTag::ProcessLabel as u16 => TxPayloadTag::ProcessLabel,
        t if t == TxPayloadTag::ProcessGroup as u16 => TxPayloadTag::ProcessGroup,
        t if t == TxPayloadTag::ProcessFork as u16 => TxPayloadTag::ProcessFork,
        t if t == TxPayloadTag::Panic as u16 => TxPayloadTag::Panic,
        // Unknown tag with valid payload_len: skip payload bytes but keep record.
        _ => return Ok(None),
    };

    let v = read_payload(parsed_tag, bytes)?;
    Ok(Some(v))
}

/// Read and JSON-serialize a known payload by tag.
fn read_payload(tag: TxPayloadTag, bytes: &[u8]) -> Result<serde_json::Value, ()> {
    macro_rules! read_as {
        ($T:ty) => {{
            if bytes.len() < size_of::<$T>() {
                return Err(());
            }
            // Safety: same POD argument as module-level doc comment.
            let v: $T = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const $T) };
            serde_json::to_value(v).map_err(|_| ())
        }};
    }

    match tag {
        TxPayloadTag::SyscallEnter => read_as!(PayloadSyscallEnter),
        TxPayloadTag::SyscallExit => read_as!(PayloadSyscallExit),
        TxPayloadTag::DriveBegin => read_as!(PayloadDriveBegin),
        TxPayloadTag::DriveEnd => read_as!(PayloadDriveEnd),
        TxPayloadTag::StepOutcome => read_as!(PayloadStepOutcome),
        TxPayloadTag::YieldBegin => read_as!(PayloadYieldBegin),
        TxPayloadTag::Resume => read_as!(PayloadResume),
        TxPayloadTag::WaitSourceNotify => read_as!(PayloadWaitSourceNotify),
        TxPayloadTag::AgentStateChange => Ok(serde_json::Value::Null), // schema reserved
        TxPayloadTag::TrackDescriptor => read_as!(PayloadTrackDescriptor),
        TxPayloadTag::CounterValue => read_as!(PayloadCounterValue),
        TxPayloadTag::StringDescriptor => Ok(serde_json::Value::Null), // schema reserved
        TxPayloadTag::ClockSnapshot => read_as!(PayloadClockSnapshot),
        TxPayloadTag::ArgValue => read_as!(PayloadArgValue),
        TxPayloadTag::MutationZoneSign => read_as!(PayloadMutationZoneSign),
        TxPayloadTag::MutationIndexCommit => read_as!(PayloadMutationIndexCommit),
        // OBS-8: L5 Phase transition payload.
        TxPayloadTag::PhaseTransition => read_as!(PayloadPhaseTransition),
        // OBS-9: L7 Sched switch payload (reactor scheduler track).
        TxPayloadTag::SchedSwitch => read_as!(PayloadSchedSwitch),
        // OBS-9 §15.7: one-shot PCB `comm` mapping.
        TxPayloadTag::ProcessLabel => read_as!(PayloadProcessLabel),
        // OBS-9 §15.8: one-shot PCB pgrp/session mapping.
        TxPayloadTag::ProcessGroup => read_as!(PayloadProcessGroup),
        // OBS-9 §15.9: parent → child fork edge.
        TxPayloadTag::ProcessFork => read_as!(PayloadProcessFork),
        TxPayloadTag::Panic => read_as!(PayloadPanic),
        TxPayloadTag::None => Ok(serde_json::Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    fn make_record(
        magic: u16,
        version: u8,
        kind: u8,
        level: u8,
        hart: u16,
        seq: u64,
        ts: u64,
        span: u64,
        parent: u64,
        name: u32,
        payload_tag: u16,
        payload_len: u16,
        payload_bytes: [u8; 16],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; size_of::<TxTraceRecord>()];
        let rec = TxTraceRecord {
            magic,
            version,
            kind,
            level,
            flags: 0,
            arg_count: 0,
            _pad0: 0,
            hart,
            _pad1: 0,
            _pad2: 0,
            seq,
            ts,
            span,
            parent,
            name,
            payload_tag,
            payload_len,
            payload: payload_bytes,
            _pad3: [0u8; 8],
        };
        // Safety: TxTraceRecord is POD; we copy the full struct into the buf.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &rec as *const TxTraceRecord as *const u8,
                buf.as_mut_ptr(),
                size_of::<TxTraceRecord>(),
            );
        }
        buf
    }

    #[test]
    fn decode_span_begin_valid() {
        let bytes = make_record(
            RECORD_MAGIC,
            0,
            TxTraceKind::SpanBegin as u8,
            TxTraceLevel::Drive as u8,
            0,
            1,
            1000,
            0xdead,
            0,
            0xabc,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let ev = decode_slot(0, &bytes);
        match ev {
            DecodedEvent::Record(r) => {
                assert_eq!(r.kind, "SpanBegin");
                assert_eq!(r.level, "Drive");
                assert_eq!(r.seq, 1);
            }
            DecodedEvent::Repair(r) => panic!("expected Record, got Repair: {r:?}"),
        }
    }

    #[test]
    fn decode_bad_magic_yields_repair() {
        let bytes = make_record(
            0xDEAD,
            0,
            TxTraceKind::SpanBegin as u8,
            0,
            1,
            99,
            0,
            0,
            0,
            0,
            TxPayloadTag::None as u16,
            0,
            [0u8; 16],
        );
        let ev = decode_slot(1, &bytes);
        match ev {
            DecodedEvent::Repair(r) => {
                assert_eq!(r.category, "txtrace.repair.bad_magic");
            }
            _ => panic!("expected Repair"),
        }
    }

    #[test]
    fn decode_payload_len_exceeded_yields_repair() {
        let bytes = make_record(
            RECORD_MAGIC,
            0,
            TxTraceKind::SpanBegin as u8,
            0,
            0,
            5,
            0,
            0,
            0,
            0,
            TxPayloadTag::None as u16,
            17, // > 16 — invalid
            [0u8; 16],
        );
        let ev = decode_slot(0, &bytes);
        match ev {
            DecodedEvent::Repair(r) => {
                assert_eq!(r.category, "txtrace.repair.payload_len_exceeded");
            }
            _ => panic!("expected Repair"),
        }
    }
}
