//! Minimal hand-derived Perfetto protobuf message types for OBS-6.
//!
//! These are the *only* packets the MVP emits:
//!   TrackDescriptor, TrackEvent, InternedData.event_names, ClockSnapshot.
//!
//! Source schema references (for reproducibility):
//!   https://github.com/google/perfetto/blob/main/protos/perfetto/trace/trace.proto
//!   https://github.com/google/perfetto/blob/main/protos/perfetto/trace/trace_packet.proto
//!   https://github.com/google/perfetto/blob/main/protos/perfetto/trace/track_event/track_event.proto
//!   https://github.com/google/perfetto/blob/main/protos/perfetto/trace/track_event/track_descriptor.proto
//!   https://github.com/google/perfetto/blob/main/protos/perfetto/trace/clock_snapshot.proto
//!
//! Only fields that txKernel MVP actually populates are included here.
//! Empty `Message` impls for unused fields are omitted to keep the encoder
//! small.  Add fields here as needed by later OBS phases.

use prost::Message;

// ── Trace (field 1: repeated TracePacket) ────────────────────────────────────

/// `perfetto.protos.Trace` — the top-level container.
///
/// proto field 1 = repeated TracePacket packet.
#[derive(Clone, PartialEq, Message)]
pub struct Trace {
    #[prost(message, repeated, tag = "1")]
    pub packet: Vec<TracePacket>,
}

// ── TracePacket ───────────────────────────────────────────────────────────────

/// `perfetto.protos.TracePacket`
///
/// We use the `oneof data` fields: track_descriptor (field 60),
/// track_event (field 11), clock_snapshot (field 6).
/// Trusted packet sequence id (field 10), interned_data (field 12),
/// sequence_flags (field 13) are also used.
#[derive(Clone, PartialEq, Message)]
pub struct TracePacket {
    /// Timestamp in trace-clock units (ns when CLOCK_BOOTTIME).
    #[prost(uint64, optional, tag = "8")]
    pub timestamp: Option<u64>,

    /// Clock domain for `timestamp`.  64 = BUILTIN_CLOCK_BOOTTIME.
    #[prost(uint32, optional, tag = "58")]
    pub timestamp_clock_id: Option<u32>,

    /// Identifies the producer sequence — must be non-zero and consistent
    /// across all packets from a single producer.
    #[prost(uint32, optional, tag = "10")]
    pub trusted_packet_sequence_id: Option<u32>,

    /// Interned data embedded in this packet (event names, arg names).
    #[prost(message, optional, tag = "12")]
    pub interned_data: Option<InternedData>,

    /// SEQ_INCREMENTAL_STATE_CLEARED (1) or SEQ_NEEDS_INCREMENTAL_STATE (2).
    #[prost(uint32, optional, tag = "13")]
    pub sequence_flags: Option<u32>,

    // ── oneof data ────────────────────────────────────────────────────────────

    /// TrackDescriptor packet (field 60).
    #[prost(message, optional, tag = "60")]
    pub track_descriptor: Option<TrackDescriptor>,

    /// TrackEvent packet (field 11).
    #[prost(message, optional, tag = "11")]
    pub track_event: Option<TrackEvent>,

    /// ClockSnapshot packet (field 6).
    #[prost(message, optional, tag = "6")]
    pub clock_snapshot: Option<ClockSnapshot>,
}

// ── TrackDescriptor ───────────────────────────────────────────────────────────

/// `perfetto.protos.TrackDescriptor`
#[derive(Clone, PartialEq, Message)]
pub struct TrackDescriptor {
    /// Globally unique track identifier (UUID).
    #[prost(uint64, optional, tag = "1")]
    pub uuid: Option<u64>,

    /// UUID of the parent track (0 / absent = top-level).
    #[prost(uint64, optional, tag = "5")]
    pub parent_uuid: Option<u64>,

    /// Human-readable name for the track.
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,

    /// Thread descriptor (if this is a thread track).
    #[prost(message, optional, tag = "4")]
    pub thread: Option<ThreadDescriptor>,

    /// Process descriptor (if this is a process track).
    #[prost(message, optional, tag = "3")]
    pub process: Option<ProcessDescriptor>,
}

/// `perfetto.protos.ThreadDescriptor`
#[derive(Clone, PartialEq, Message)]
pub struct ThreadDescriptor {
    #[prost(int32, optional, tag = "1")]
    pub pid: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub tid: Option<i32>,
    #[prost(string, optional, tag = "5")]
    pub thread_name: Option<String>,
}

/// `perfetto.protos.ProcessDescriptor`
#[derive(Clone, PartialEq, Message)]
pub struct ProcessDescriptor {
    #[prost(int32, optional, tag = "1")]
    pub pid: Option<i32>,
    #[prost(string, optional, tag = "6")]
    pub process_name: Option<String>,
}

// ── TrackEvent ────────────────────────────────────────────────────────────────

/// `perfetto.protos.TrackEvent`
///
/// TYPE_SLICE_BEGIN = 1, TYPE_SLICE_END = 2, TYPE_INSTANT = 4.
#[derive(Clone, PartialEq, Message)]
pub struct TrackEvent {
    /// Track UUID this event belongs to.
    #[prost(uint64, optional, tag = "11")]
    pub track_uuid: Option<u64>,

    /// Event type.
    #[prost(enumeration = "TrackEventType", optional, tag = "9")]
    pub r#type: Option<i32>,

    /// Interned name iid (references InternedData.event_names).
    #[prost(uint64, optional, tag = "10")]
    pub name_iid: Option<u64>,

    /// Fallback: non-interned name string (used when name_iid not set).
    #[prost(string, optional, tag = "23")]
    pub name: Option<String>,

    /// Categories (e.g., "txtrace.repair.orphan_end").
    #[prost(string, repeated, tag = "22")]
    pub categories: Vec<String>,

    /// Flow ids produced by this event.
    #[prost(uint64, repeated, tag = "47")]
    pub flow_ids: Vec<u64>,

    /// Terminating flow ids consumed by this event.
    #[prost(uint64, repeated, tag = "48")]
    pub terminating_flow_ids: Vec<u64>,

    /// Debug annotations.
    #[prost(message, repeated, tag = "4")]
    pub debug_annotations: Vec<DebugAnnotation>,
}

/// `perfetto.protos.TrackEvent.Type`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum TrackEventType {
    Unspecified = 0,
    SliceBegin  = 1,
    SliceEnd    = 2,
    Instant     = 4,
}

// ── DebugAnnotation ───────────────────────────────────────────────────────────

/// `perfetto.protos.DebugAnnotation`
#[derive(Clone, PartialEq, Message)]
pub struct DebugAnnotation {
    /// Interned name iid (references InternedData.debug_annotation_names).
    #[prost(uint64, optional, tag = "1")]
    pub name_iid: Option<u64>,

    /// Non-interned fallback name.
    #[prost(string, optional, tag = "10")]
    pub name: Option<String>,

    // oneof value — we only populate the uint field for now.
    #[prost(uint64, optional, tag = "4")]
    pub uint_value: Option<u64>,

    #[prost(int64, optional, tag = "5")]
    pub int_value: Option<i64>,

    #[prost(string, optional, tag = "7")]
    pub string_value: Option<String>,
}

// ── InternedData ──────────────────────────────────────────────────────────────

/// `perfetto.protos.InternedData`
#[derive(Clone, PartialEq, Message)]
pub struct InternedData {
    #[prost(message, repeated, tag = "2")]
    pub event_names: Vec<EventName>,

    #[prost(message, repeated, tag = "16")]
    pub debug_annotation_names: Vec<DebugAnnotationName>,
}

/// `perfetto.protos.EventName`
#[derive(Clone, PartialEq, Message)]
pub struct EventName {
    #[prost(uint64, optional, tag = "1")]
    pub iid: Option<u64>,

    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
}

/// `perfetto.protos.DebugAnnotationName`
#[derive(Clone, PartialEq, Message)]
pub struct DebugAnnotationName {
    #[prost(uint64, optional, tag = "1")]
    pub iid: Option<u64>,

    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
}

// ── ClockSnapshot ─────────────────────────────────────────────────────────────

/// `perfetto.protos.ClockSnapshot`
#[derive(Clone, PartialEq, Message)]
pub struct ClockSnapshot {
    #[prost(message, repeated, tag = "1")]
    pub clocks: Vec<Clock>,
}

/// `perfetto.protos.ClockSnapshot.Clock`
#[derive(Clone, PartialEq, Message)]
pub struct Clock {
    /// Perfetto builtin clock ids:
    ///   BUILTIN_CLOCK_REALTIME  = 1
    ///   BUILTIN_CLOCK_REALTIME_COARSE = 2
    ///   BUILTIN_CLOCK_MONOTONIC = 3
    ///   BUILTIN_CLOCK_MONOTONIC_COARSE = 4
    ///   BUILTIN_CLOCK_MONOTONIC_RAW = 5
    ///   BUILTIN_CLOCK_BOOTTIME  = 6
    ///   Custom range: 64+
    #[prost(uint32, optional, tag = "1")]
    pub clock_id: Option<u32>,

    /// Clock value at snapshot time (ns for builtins; trace-ticks for custom).
    #[prost(uint64, optional, tag = "2")]
    pub timestamp: Option<u64>,

    /// Whether the clock is incremental (defaults false).
    #[prost(bool, optional, tag = "3")]
    pub is_incremental: Option<bool>,

    /// Multiplier to convert ticks to ns. 1 for ns-resolution clocks.
    #[prost(uint64, optional, tag = "4")]
    pub unit_multiplier_ns: Option<u64>,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Perfetto sequence flags bit: incremental state was reset (new interning epoch).
pub const SEQ_INCREMENTAL_STATE_CLEARED: u32 = 1;

/// Perfetto builtin clock id: BOOTTIME (compatible with TRACE_CLOCK_BOOT).
pub const CLOCK_BOOTTIME: u32 = 6;

/// Custom clock base for txtrace clocks that don't map to a Perfetto builtin.
/// Must be >= 64 per the Perfetto spec.
pub const CLOCK_CUSTOM_TXTRACE_BASE: u32 = 64;

/// Trusted packet sequence id we use for all our packets (single producer).
pub const TRUSTED_SEQ_ID: u32 = 1;
