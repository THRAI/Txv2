//! L4 host readers and capture-integrity entry points.
//!
//! This layer owns txtrace file replay, raw-record/live guest-memory draining,
//! and shared completeness/loss accounting before records become canonical
//! events.

use std::path::PathBuf;

mod live;
pub mod replay;

/// Host input modes accepted by the observe pipeline.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum TraceInputKind {
    TxTraceRegion,
    RawRecords,
    LiveGuestMem,
}

/// User-facing trace input descriptor.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceInput {
    pub kind: TraceInputKind,
    pub path: PathBuf,
    pub schema_version: u16,
}

/// Shared capture-integrity interpretation for host readers.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct TraceIntegrity {
    pub input_kind: TraceInputKind,
    pub complete: bool,
    pub drained_records: u64,
    pub retained_records: u64,
    pub lost_records: u64,
    pub overwritten_records: u64,
    pub repair_count: u64,
}

impl TraceIntegrity {
    pub fn from_trace_stats(input_kind: TraceInputKind, stats: &replay::TraceStats) -> Self {
        Self {
            input_kind,
            complete: stats.complete,
            drained_records: stats.total_records,
            retained_records: stats.total_records,
            lost_records: stats.total_lost,
            overwritten_records: stats.overwritten_records,
            repair_count: stats.framing_errors,
        }
    }

    #[allow(dead_code)]
    pub fn raw_records(
        input_kind: TraceInputKind,
        retained_records: u64,
        repair_count: u64,
    ) -> Self {
        Self {
            input_kind,
            complete: repair_count == 0,
            drained_records: retained_records,
            retained_records,
            lost_records: 0,
            overwritten_records: 0,
            repair_count,
        }
    }
}

/// Fixed-size raw record frame passed from L4 into the L5 decoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawRecordFrame {
    pub hart: u16,
    pub seq_hint: Option<u64>,
    bytes: [u8; 80],
}

impl RawRecordFrame {
    pub fn from_slot(hart: u16, seq_hint: Option<u64>, slot_bytes: &[u8]) -> Result<Self, String> {
        let bytes: [u8; 80] = slot_bytes.try_into().map_err(|_| {
            format!(
                "raw record frame must be 80 bytes, got {}",
                slot_bytes.len()
            )
        })?;
        Ok(Self {
            hart,
            seq_hint,
            bytes,
        })
    }

    pub(crate) fn bytes_for_decode(&self) -> &[u8; 80] {
        &self.bytes
    }
}

/// L4 reader trait. Implementations own byte access and expose frames only.
#[allow(dead_code)]
pub trait TraceReader {
    fn integrity(&self) -> &TraceIntegrity;
    fn next_frame(&mut self) -> std::io::Result<Option<RawRecordFrame>>;
}
