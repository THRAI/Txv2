//! `TxTraceRecord`, `TxTraceKind`, `TxTraceLevel`.
//!
//! Layout spec: `08_OBSERVATION_SERIALIZATION_v0.md` §5–7.

// ---------------------------------------------------------------------------
// Fixed record
// ---------------------------------------------------------------------------

/// One 80-byte fixed-size trace record.
///
/// `#[repr(C, align(8))]` — the natural alignment of the heaviest field
/// (`u64`) is 8 bytes; `align(8)` is explicit to match the spec.
///
/// `sizeof::<TxTraceRecord>() == 80` — enforced by compile-time assertion in
/// `lib.rs`.
///
/// Verified field layout (native-endian):
/// ```text
/// offset  0 : magic        u16        2
/// offset  2 : version      u8         1
/// offset  3 : kind         u8         1
/// offset  4 : level        u8         1
/// offset  5 : flags        u8         1
/// offset  6 : arg_count    u8         1
/// offset  7 : _pad0        u8         1    ← spec-named; total = 8
/// offset  8 : hart         u16        2
/// offset 10 : _pad1        u16        2    ← spec-named; total = 12
/// offset 12 : _pad2        u32        4    ← fills the gap before seq; total = 16
/// offset 16 : seq          u64        8
/// offset 24 : ts           u64        8
/// offset 32 : span         u64        8
/// offset 40 : parent       u64        8
/// offset 48 : name         u32        4
/// offset 52 : payload_tag  u16        2
/// offset 54 : payload_len  u16        2
/// offset 56 : payload      [u8; 16]  16
/// offset 72 : _pad3        [u8; 8]    8    ← tail padding to reach 80
/// total = 80
/// ```
///
/// ## Spec note on padding fields
///
/// The txtrace-v0 spec (§5) shows the record struct with only `_pad0: u8` and
/// `_pad1: u16` named, yet asserts `sizeof == 80`.  With those two named
/// fields alone the compiler produces 72 bytes: a 4-byte implicit gap at
/// offsets 12-15 (before the 8-byte-aligned `seq`) and no tail padding (72 is
/// already a multiple of 8).  To reach 80 bytes we add:
/// - `_pad2: u32` at offset 12 (explicit, replaces the implicit gap so the
///   layout is fully controlled), and
/// - `_pad3: [u8; 8]` at offset 72 (explicit tail padding).
///
/// This is an OBS-1 implementer clarification, not a semantic change: no
/// consumer existed before this PR, so there is no ABI break.
#[repr(C, align(8))]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct TxTraceRecord {
    /// `b"TR"` little-endian u16 = `0x5254`.  Per-record magic for resync.
    pub magic: u16,

    /// Record format version.  v0.
    pub version: u8,

    /// One of [`TxTraceKind`].
    pub kind: u8,

    /// One of [`TxTraceLevel`].
    pub level: u8,

    /// Record-kind-specific flag bits.
    pub flags: u8,

    /// Number of `ArgCont` continuation records that follow this one in the
    /// same span.  Daemon uses this to bound reconstruction.
    pub arg_count: u8,

    /// Spec-named pad byte (offset 7).
    pub _pad0: u8,

    /// Hart id from which this record was emitted.  Redundant with the ring
    /// it sits in; kept for the self-describing-record property.
    pub hart: u16,

    /// Spec-named pad word (offsets 10-11).
    pub _pad1: u16,

    /// Explicit pad to place `seq` at offset 16 and eliminate the implicit
    /// 4-byte compiler gap the spec relied on.  Bytes at offsets 12-15.
    pub _pad2: u32,

    /// Per-hart monotone sequence number assigned at emit.
    pub seq: u64,

    /// Raw trace-clock timestamp (units defined by `header.clock_id`).
    pub ts: u64,

    /// Span id (per-hart counter with `hart_id` in high byte).  0 for orphan.
    /// Layout: bits 0..56 local counter; bits 56..64 `hart_id`.
    pub span: u64,

    /// Parent span id, or 0 if absent.  For `SpanBegin`: enclosing span.
    /// For `Instant`: the span the instant belongs to.
    pub parent: u64,

    /// Static event name id (resolved by daemon via `names.json` or
    /// kernel-embedded string table).
    pub name: u32,

    /// One of [`crate::payload::TxPayloadTag`].  Identifies the in-record
    /// payload schema.
    pub payload_tag: u16,

    /// Bytes used inside the payload buffer (0 if `payload_tag` = None).
    pub payload_len: u16,

    /// Fixed inline payload buffer.  Schemas live in `payload.rs`.
    pub payload: [u8; 16],

    /// Explicit tail padding to reach 80 bytes total (offsets 72-79).
    pub _pad3: [u8; 8],
}

// ---------------------------------------------------------------------------
// Record kind
// ---------------------------------------------------------------------------

/// Discriminant for [`TxTraceRecord::kind`].
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceKind {
    /// No-op slot (e.g. daemon-attach-late initial state).  Skip.
    Nop = 0,

    /// Clock-translation snapshot record.
    ClockSnapshot = 1,

    /// Track-descriptor record (kernel-emitted track creation).
    TrackDescriptor = 2,

    /// String-descriptor record (kernel-embedded interning).
    StringDescriptor = 3,

    /// Span open.  Payload identifies the entity; span id is in `span`.
    SpanBegin = 10,

    /// Span close.  Payload may carry result data (e.g. `SyscallExit`).
    SpanEnd = 11,

    /// Point-in-time event attached to `span`.
    Instant = 12,

    /// Counter sample.
    Counter = 13,

    /// Track tombstone (object reclaimed).  Daemon ages out the track.
    TrackTombstone = 14,

    /// Panic marker emitted by the kernel panic handler before halt.
    PanicMarker = 31,

    /// Argument continuation attached to the most-recent record's `span`.
    ArgContinuation = 40,
}

// ---------------------------------------------------------------------------
// Trace level
// ---------------------------------------------------------------------------

/// Runtime verbosity level tag stamped on every record.
///
/// Compile-time feature gates (`level_syscall`, `level_drive`, …) decide
/// which records exist at all; this field lets the daemon filter compiled-in
/// records by level.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceLevel {
    Boundary = 0,
    Script = 1,
    Drive = 2,
    Yield = 3,
    Step = 4,
    Phase = 5,
    Mutation = 6,
}
