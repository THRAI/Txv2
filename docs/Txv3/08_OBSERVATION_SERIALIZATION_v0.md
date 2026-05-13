# Observation Serialization — txtrace-v0

<!-- txdoc:TXV3-OBSERVATION-SERIALIZATION-V0 -->

**Status.** v0 wire format (Txv3, 2026-05). First subscriber spec for the observation subsystem.
**Purpose.** Specify the kernel-to-host binary ABI: per-hart ring layout, header, fixed-size record format, payload schemas, versioning. The kernel side writes records directly into ring slots; the host daemon decodes by reading the same layout via mmap or file-replay.
**Audience.** Subagents implementing `tx-observe-types` (kernel-side definitions), `tx-observe::ring` (emitter), and `tools/tx-trace-daemon/src/decode/` (host decoder).
**Companion documents.**
- [`08_OBSERVATION_v1.md`](08_OBSERVATION_v1.md) — framework, invariants, hook surface.
- [`08_OBSERVATION_HOST_v0.md`](08_OBSERVATION_HOST_v0.md) — daemon reconstruction.

---

## 1. Goals and non-goals

<!-- txdoc:OBS-SER-V0-GOALS-1 -->

**Goals.**
- Fixed-size, native-endian, `#[repr(C)]` POD records — direct `core::ptr::write` into ring slot.
- Per-hart SPSC ring with release-store producer / acquire-load consumer.
- Self-describing region: magic + version + clock + layout in a header.
- Versioned at two levels (header version, record version) for graceful evolution.
- Same record format kernel-side and host-side via a shared `tx-observe-types` crate with a `host` Cargo feature for `Debug + serde` derives.

**Non-goals.**
- Stable user ABI. txtrace-v0 is a kernel-to-host implementation contract, not a userspace interface.
- General logging format. Records carry observation events, not arbitrary structured logs.
- Truth source. Records are observations; semantic truth lives in subsystem state (OBS-5 in the framework doc).
- Schema evolution within a single version. Field reorder / size change requires version bump (OBS-10).

## 2. Region layout

<!-- txdoc:OBS-SER-V0-REGION-1 -->

```text
TxTraceRegion (the ivshmem BAR / reserved DRAM / hosted buffer)
┌───────────────────────────────┐
│ TxTraceHeader                 │  magic, version, clock, layout offsets
├───────────────────────────────┤
│ Static string table (optional)│  pointed to by header.string_table_off/len
├───────────────────────────────┤
│ TxTraceHartRing[0]            │  hart 0's ring: header + slots
├───────────────────────────────┤
│ TxTraceHartRing[1]            │
├───────────────────────────────┤
│ ...                           │
├───────────────────────────────┤
│ TxTraceHartRing[N-1]          │
└───────────────────────────────┘
```

Region size: `header_size + string_table_size + N * ring_size`, where `ring_size = ring_header_size + (1 << ring_order) * record_size`.

OBS-SER-LAYOUT-RIGID: the layout is fully described by the header. The host daemon mmaps the region, parses the header, then walks N rings starting at `header.rings_off`. No additional discovery is required.

## 3. Global header

<!-- txdoc:OBS-SER-V0-HEADER-1 -->

```rust
// tx-observe-types/src/header.rs
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct TxTraceHeader {
    /// b"TXTR" interpreted as little-endian u32 = 0x52545854.
    pub magic: u32,

    /// txtrace header format version. v0.
    pub version: u16,

    /// Size of this header in bytes (== sizeof::<TxTraceHeader>()).
    pub header_len: u16,

    /// 1 = little endian. Other values reserved.
    pub endian: u8,

    /// Pointer width in bytes (8 on rv64/la64).
    pub ptr_width: u8,

    /// Size of one fixed record. v0 = 80 bytes.
    pub record_size: u16,

    /// Number of harts (== number of TxTraceHartRing entries that follow).
    pub hart_count: u16,

    /// Each ring has 1 << ring_order slots. Power of two required.
    pub ring_order: u8,

    /// Flags (see TxTraceHeaderFlags).
    pub flags: u8,

    /// Explicit padding to align boot_id on 8 bytes.
    pub _pad0: u32,

    /// Random or monotone boot identifier.
    pub boot_id: u64,

    /// Kernel trace clock id (see TxTraceClockId).
    pub clock_id: u32,

    /// Explicit padding to align clock_freq_hz on 8 bytes.
    pub _pad1: u32,

    /// Trace clock frequency (Hz), if known. 0 = unknown.
    pub clock_freq_hz: u64,

    /// Byte offset from region base to optional kernel-embedded string table.
    /// 0 = absent; daemon falls back to out-of-band names.json keyed by boot_id.
    pub string_table_off: u64,

    /// Byte length of the kernel-embedded string table. 0 if absent.
    pub string_table_len: u64,

    /// Byte offset from region base to the first TxTraceHartRing.
    pub rings_off: u64,
}

/// Header flag bits.
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct TxTraceHeaderFlags(pub u8);

impl TxTraceHeaderFlags {
    /// Trace clock is shared across all harts (cross-hart timestamps trustworthy).
    /// Set by boards where ObserverIf::clock_shared() returns true (e.g. QEMU `time` CSR).
    pub const CLOCK_SHARED: Self = Self(1 << 0);
}

/// Trace clock identifier.
#[repr(u32)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceClockId {
    Unknown    = 0,
    RiscvTime  = 1,  // RV64 `time` CSR
    ArmCntvct  = 2,  // AArch64 `cntvct_el0`
    X86TscInv  = 3,  // x86_64 invariant TSC
    HostNanos  = 4,  // hosted/test: std::time monotonic ns
}
```

OBS-SER-HEADER-SIZE: `sizeof::<TxTraceHeader>() == 72` bytes with the explicit padding. Verify in CI via `assert_eq!(core::mem::size_of::<TxTraceHeader>(), 72)`.

OBS-SER-HEADER-REJECT: the host daemon must reject the region if `magic != 0x52545854`, `version > supported_version`, `record_size != 80`, `hart_count > MAX_HARTS_DAEMON`, or layout offsets fall outside the mapped region.

## 4. Per-hart ring

<!-- txdoc:OBS-SER-V0-RING-1 -->

Each hart owns one SPSC ring. The producer (kernel-side, this hart) and consumer (host daemon) coordinate via the `producer` and `consumer` u64 counters.

```rust
// tx-observe-types/src/header.rs
#[repr(C)]
#[derive(Copy, Clone)]
// AtomicU64 fields are not Copy; the type lives in shared memory and is read
// via raw pointers on both ends. We derive nothing for it.
pub struct TxTraceHartRing {
    pub hart_id: u16,
    pub flags: u16,

    /// Explicit padding to align producer on a cacheline.
    pub _pad0: [u8; 60],

    /// Producer-owned: monotone slot index.
    /// Written by kernel-side this hart with Release ordering.
    pub producer: AtomicU64,

    /// Pad producer to its own cacheline to avoid false-sharing with consumer.
    pub _pad1: [u8; 56],

    /// Consumer-owned: monotone drained index.
    /// Written by host daemon with Release ordering.
    pub consumer: AtomicU64,

    /// Pad consumer to its own cacheline.
    pub _pad2: [u8; 56],

    /// Dropped-record counter, written by producer.
    pub lost: AtomicU64,

    /// Per-hart monotone sequence number used to stamp records.
    pub seq: AtomicU64,

    // Followed by [TxTraceRecord; 1 << ring_order]
}
```

OBS-SER-RING-CACHELINE: `producer` and `consumer` sit on *separate cachelines* (assumed line size: 64 bytes; padding adjusts if the platform's `CACHE_LINE_SIZE` from [`PlatformConfig`](../../crates/tx-hal/src/lib.rs) differs).

OBS-SER-RING-SLOT-COUNT: slot count is `1 << header.ring_order`, derived from the header. The ring header does *not* duplicate the count.

### 4.1 Producer rule

```text
let p = self.producer.load(Acquire);
let c = self.consumer.load(Acquire);
if p.wrapping_sub(c) >= slot_count {
    self.lost.fetch_add(1, Relaxed);
    return;  // OBS-6: ring overflow drops; never blocks.
}
// Write fields directly into slot[p & (slot_count - 1)] via volatile or
// non-aliasing raw pointer writes. All field stores happen before the
// release of producer below.
core::sync::atomic::fence(Release);
self.producer.store(p.wrapping_add(1), Release);
```

OBS-SER-RING-PRODUCER-MO: the field writes must happen-before the producer store. Use a Release fence followed by Release store, or a single Release store after Relaxed field writes — the host's Acquire load of producer must observe all field stores.

### 4.2 Consumer rule

```text
let p = self.producer.load(Acquire);
let c = self.consumer.load(Acquire);
while c != p {
    let slot = &self.slots[c & (slot_count - 1)];
    // Decode slot. All field reads happen-before the consumer store below.
    decode_record(slot);
    c = c.wrapping_add(1);
}
self.consumer.store(c, Release);
```

OBS-SER-RING-OVERTAKEN: if `producer.load(Acquire) - consumer >= slot_count`, the consumer has been overtaken — record(s) were overwritten before being read. The daemon synthesizes `LostRecords` events for the gap by reading the `lost` counter delta.

## 5. Fixed record format

<!-- txdoc:OBS-SER-V0-RECORD-1 -->

```rust
// tx-observe-types/src/record.rs
#[repr(C, align(8))]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct TxTraceRecord {
    /// b"TR" little-endian u16 = 0x5254. Per-record magic for resync.
    pub magic: u16,

    /// Record format version. v0.
    pub version: u8,

    /// One of TxTraceKind.
    pub kind: u8,

    /// One of TxTraceLevel.
    pub level: u8,

    /// Record-kind-specific flag bits.
    pub flags: u8,

    /// Number of ArgCont continuation records that follow this one in the
    /// same span. Daemon uses this to bound reconstruction.
    pub arg_count: u8,

    pub _pad0: u8,

    /// Hart id from which this record was emitted. Redundant with the ring
    /// it sits in; kept for self-describing-record property.
    pub hart: u16,

    pub _pad1: u16,

    /// Explicit fill of the 4-byte alignment gap that would otherwise be
    /// inserted before `seq: u64`. Present so the struct has no implicit
    /// padding (verified by the size assertion below).
    pub _pad2: u32,

    /// Per-hart monotone sequence number assigned at emit.
    pub seq: u64,

    /// Raw trace-clock timestamp (units defined by header.clock_id).
    pub ts: u64,

    /// Span id (per-hart counter with hart_id in high byte). 0 for orphan.
    /// Layout: bits 0..56 local counter; bits 56..64 hart_id.
    pub span: u64,

    /// Parent span id, or 0 if absent. For SpanBegin: enclosing span.
    /// For Instant: the span the instant belongs to.
    pub parent: u64,

    /// Static event name id (resolved by daemon via names.json or
    /// kernel-embedded string table).
    pub name: u32,

    /// One of TxPayloadTag. Identifies the in-record payload schema.
    pub payload_tag: u16,

    /// Bytes used inside the payload buffer (0 if payload_tag = None).
    pub payload_len: u16,

    /// Fixed inline payload buffer. Schemas live in `payload.rs`.
    pub payload: [u8; 16],

    /// Explicit tail padding so the struct is exactly 80 bytes with zero
    /// implicit padding. The 8-byte alignment of the struct alone would not
    /// require this (offsets so far sum to 72), but making it explicit means
    /// every byte of the record has a named purpose, which the daemon's
    /// `repr(C)` decode can rely on.
    pub _pad3: [u8; 8],
}
```

**Offset table (the canonical layout, no implicit padding).**

| offset | field         | size | notes                                |
|-------:|---------------|-----:|--------------------------------------|
|      0 | `magic`       |    2 | 0x5254 = b"TR" LE                    |
|      2 | `version`     |    1 |                                      |
|      3 | `kind`        |    1 | `TxTraceKind`                        |
|      4 | `level`       |    1 | `TxTraceLevel`                       |
|      5 | `flags`       |    1 |                                      |
|      6 | `arg_count`   |    1 |                                      |
|      7 | `_pad0`       |    1 |                                      |
|      8 | `hart`        |    2 |                                      |
|     10 | `_pad1`       |    2 |                                      |
|     12 | `_pad2`       |    4 | fills the u64 alignment gap          |
|     16 | `seq`         |    8 |                                      |
|     24 | `ts`          |    8 |                                      |
|     32 | `span`        |    8 |                                      |
|     40 | `parent`      |    8 |                                      |
|     48 | `name`        |    4 |                                      |
|     52 | `payload_tag` |    2 |                                      |
|     54 | `payload_len` |    2 |                                      |
|     56 | `payload`     |   16 |                                      |
|     72 | `_pad3`       |    8 | tail padding                         |
|     80 | (end)         |      |                                      |

OBS-SER-RECORD-SIZE: `sizeof::<TxTraceRecord>() == 80` bytes. Verify in CI: `assert_eq!(core::mem::size_of::<TxTraceRecord>(), 80)` and `assert_eq!(core::mem::align_of::<TxTraceRecord>(), 8)`.

OBS-SER-RECORD-LAYOUT: bytes are written native-endian. Cross-architecture decoding is not supported in v0 (kernel and daemon must share endianness; trivially true for ivshmem-on-QEMU where daemon runs on the host that runs QEMU).

OBS-SER-RECORD-MAGIC: per-record magic enables resync after observed corruption (rare; only on simultaneous overrun + slot-tear races, which the ring discipline forecloses).

## 6. Record kinds

<!-- txdoc:OBS-SER-V0-KINDS-1 -->

```rust
// tx-observe-types/src/record.rs
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceKind {
    /// No-op slot (e.g., daemon-attach-late initial state). Skip.
    Nop                = 0,

    /// Clock-translation snapshot record.
    ClockSnapshot      = 1,

    /// Track-descriptor record (kernel-emitted track creation).
    TrackDescriptor    = 2,

    /// String-descriptor record (kernel-embedded interning).
    StringDescriptor   = 3,

    /// Span open. payload identifies the entity; span id is in `span`.
    SpanBegin          = 10,

    /// Span close. payload may carry result data (e.g., SyscallExit).
    SpanEnd            = 11,

    /// Point-in-time event attached to `span`.
    Instant            = 12,

    /// Counter sample.
    Counter            = 13,

    /// Track-tombstone (object reclaimed). Daemon ages out the track.
    TrackTombstone     = 14,

    /// Argument continuation attached to the most-recent record's `span`.
    ArgContinuation    = 40,

    /// Panic marker emitted by kernel panic handler before halt.
    PanicMarker        = 31,
}
```

OBS-SER-KIND-UNKNOWN: decoders must treat unknown kind values as `Nop` and skip the record. Forward-compat for daemon meeting newer kernel.

OBS-SER-KIND-LOSTRECORDS-NOT-KERNEL: there is no `LostRecords` kind. Loss is observed by the daemon reading the `lost` atomic; it synthesizes a Perfetto data-loss event from the delta. The kernel never emits a "records were lost" record itself (it can't reliably do so when the ring is full).

## 7. Trace levels

<!-- txdoc:OBS-SER-V0-LEVELS-1 -->

```rust
// tx-observe-types/src/record.rs
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceLevel {
    Boundary = 0,
    Script   = 1,
    Drive    = 2,
    Yield    = 3,
    Step     = 4,
    Phase    = 5,
    Mutation = 6,
}
```

OBS-SER-LEVEL-ROLE: the runtime `level` tag on each record allows host-side filtering of records that are compiled into the kernel image. Compile-time gates (`level_syscall`, `level_drive`, etc., in `tx-observe`) decide which records exist at all. See [`08_OBSERVATION_v1.md §3`](08_OBSERVATION_v1.md).

## 8. Payload tags and schemas

<!-- txdoc:OBS-SER-V0-PAYLOADS-1 -->

```rust
// tx-observe-types/src/payload.rs
#[repr(u16)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxPayloadTag {
    None             = 0,

    // ── L0 boundary ────────────────────────────────────────────────────
    SyscallEnter     = 1,
    SyscallExit      = 2,

    // ── L2 drive / L4 step ──────────────────────────────────────────────
    DriveBegin       = 10,
    DriveEnd         = 11,
    StepOutcome      = 12,

    // ── L3 yield/resume / wake ──────────────────────────────────────────
    YieldBegin       = 20,
    Resume           = 21,
    WaitSourceNotify = 22,
    /// **Reserved, no payload schema in v0.** Reserved for OBS-3b/OBS-8 use
    /// when DelegateEndpoint state-machine transitions get their own record
    /// type. Until then: no kernel call site emits this tag, and daemon
    /// decoders MUST treat its presence as `unknown_payload` (the tag is
    /// known, the schema is absent), emitting a `txtrace.repair.unknown_payload`
    /// marker per [`08_OBSERVATION_HOST_v0.md §10`](08_OBSERVATION_HOST_v0.md).
    AgentStateChange = 23,

    // ── Track / metadata ───────────────────────────────────────────────
    TrackDescriptor  = 30,
    CounterValue     = 31,
    StringDescriptor = 32,
    ClockSnapshot    = 33,

    // ── Argument continuation ──────────────────────────────────────────
    ArgValue         = 40,

    // ── Mutation (L6) ──────────────────────────────────────────────────
    MutationZoneSign     = 50,
    MutationIndexCommit  = 51,

    // ── Panic (special) ────────────────────────────────────────────────
    Panic            = 60,
}
```

OBS-SER-PAYLOAD-UNKNOWN: decoders skip records whose payload_tag is unknown but whose payload_len is valid (≤16). The record kind is still consumed; only the payload bytes are discarded.

OBS-SER-PAYLOAD-LEN-BOUND: `payload_len <= 16` is invariant. Decoders reject records with `payload_len > 16` as malformed.

### 8.1 Syscall payloads

<!-- txdoc:OBS-SER-V0-SYSCALL-1 -->

```rust
// All sizes ≤ 16 bytes, fits inline.

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadSyscallEnter {
    pub sysno: u32,
    pub abi: u16,         // 0 = LinuxRv64, 1 = LinuxLa64
    pub argc: u16,        // Number of ArgValue continuations that follow.
}
// size = 8

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadSyscallExit {
    /// Field order chosen so the largest field (i64) lands on its
    /// natural alignment without internal padding. Total: 16 bytes.
    pub ret: i64,         // Syscall return value (or partial count).
    pub errno: i32,       // 0 if Ok.
    pub result_kind: u8,  // 0=Ok, 1=Err, 2=Restart, 3=Fatal, 4=NoReturn
    pub _pad: [u8; 3],
}
// size = 16
```

OBS-SER-SYSCALL-ARGS: register-shaped syscall args are emitted as `ArgValue` continuation records ([§8.6](#86-argument-continuation)). Decoded user data (paths, comms) is *not* emitted in v0 (no dynamic string interning); the shim may emit an opaque pointer value via `ArgValue { kind: Ptr, value0: <user va> }`.

### 8.2 Drive payloads

<!-- txdoc:OBS-SER-V0-DRIVE-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadDriveBegin {
    /// EventNameId from TypeId::of::<O>() truncated to u32 (see
    /// 08_OBSERVATION_v1.md §11). Daemon resolves human name via names.json.
    pub op_type: u32,

    pub mode: u8,             // 0=Nonblocking, 1=Waiting, 2=Selecting
    pub interrupt: u8,        // 0=Uninterruptible, 1=Interruptible, 2=Killable
    pub has_deadline: u8,
    pub _pad: u8,

    /// task_id_low (32-bit truncation; combined with hart for daemon-side
    /// uniqueness within a single kernel run).
    pub task_id_low: u32,
}
// size = 12

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadDriveEnd {
    /// Mirrors PayloadSyscallExit shape for symmetric reading.
    pub ret: i64,         // Final Output for Done; 0 for Err.
    pub errno: i32,       // Final errno for Err; 0 for Done.
    pub result_kind: u8,  // 0=Done, 1=Err
    pub _pad: [u8; 3],
}
// size = 16
```

### 8.3 Step outcome payload

<!-- txdoc:OBS-SER-V0-STEP-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadStepOutcome {
    /// 0=Continue, 1=Yield, 2=Done, 3=Err
    pub variant: u8,

    /// 0 = progress is EMPTY; 1 = progress has data.
    pub progress_empty: u8,

    /// One of TxProgressKind below.
    pub progress_kind: u8,

    /// One of YieldShapeKind (valid iff variant == 1).
    pub shape_kind: u8,

    /// Errno (valid iff variant == 3).
    pub errno: i32,

    /// The numeric progress count (bytes / pages / entries / iovecs done).
    /// Captures the "did we move data?" debug case directly. u32 is plenty
    /// for one step iteration (max 4GB per step).
    pub progress_value: u32,

    pub _pad: u32,
}
// size = 16

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxProgressKind {
    NoProgress    = 0,
    ByteProgress  = 1,
    PageProgress  = 2,
    EntryProgress = 3,
    IoVecProgress = 4,
}

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum YieldShapeKind {
    OnWaitSource = 1,
    OnAgent      = 2,
    OnTimer      = 3,
    // Future: OnEdge = 4, OnHandoff = 5.
}
```

OBS-SER-STEP-AS-SPAN: a step iteration emits *both* `SpanBegin(step.iteration)` (before `op.step(ctx)`) and `SpanEnd(step.iteration)` (after). The outcome payload sits on the SpanEnd. This gives Perfetto a slice with real duration, not a zero-width Instant.

### 8.4 Yield/resume payloads

<!-- txdoc:OBS-SER-V0-YIELD-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadYieldBegin {
    pub shape_kind: u8,        // YieldShapeKind
    pub _pad: [u8; 3],
    pub task_id_low: u32,
    pub wait_generation: u64,
}
// size = 16

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadResume {
    pub resume_kind: u8,       // 0=Retry, 1=WithReply, 2=TimerExpired, 3=Aborted
    pub abort_reason: u8,      // 0=Signal, 1=Canceled, 2=TimedOut, 3=AgentDied, 4=BorrowerExited
    pub _pad: [u8; 2],
    pub object_id_low: u32,    // Compact source/token/timer id (lower 32 bits).
    pub wait_generation: u64,
}
// size = 16

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadWaitSourceNotify {
    pub source_id_low: u32,
    pub mask_bits: u32,
    /// Material for flow_id reconstruction. Daemon hashes (task_id, wait_gen, kind).
    pub task_id_low: u32,
    pub wait_generation_low: u32,
    // wait_generation_high lives in the seq field of the record (no room here).
}
// size = 16
```

OBS-SER-YIELD-FLOW-MATERIAL: producer-side notify and consumer-side resume both carry `(task_id_low, wait_generation)` material. The daemon hashes `(task_id, wait_gen, flow_kind)` to produce Perfetto flow_ids ([`08_OBSERVATION_v1.md §13.2`](08_OBSERVATION_v1.md)).

OBS-SER-YIELD-GENERATION-FITS: `wait_generation` is u64 in [`Reactor_concept_v5_RefactorSpec`](Reactor_concept_v5_RefactorSpec%20v4.md). The producer payload only carries the low 32 bits; daemon may collide if a single task accumulates >4G yields in one run (negligible). Sufficient for v0.

### 8.5 Track / metadata payloads

<!-- txdoc:OBS-SER-V0-TRACK-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadTrackDescriptor {
    pub track_id: u64,
    // Field order: `name` (u32) must come before `track_kind` (u8); the
    // reverse order interleaves a u8 between two u32-aligned fields and
    // forces the compiler to insert 3 bytes of implicit padding, blowing
    // the 16-byte payload budget. Same principle as PayloadSyscallExit.
    pub name: u32,          // EventNameId; daemon resolves human name.
    pub track_kind: u8,     // 0=Hart, 1=Task, 2=Process, 3=Scope, 4=Endpoint, 5=Timer
    pub _pad: [u8; 3],
}
// size = 16

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadCounterValue {
    pub counter_id: u32,
    pub _pad: u32,
    pub value: u64,         // i64 reinterpretation OK for signed counters.
}
// size = 16

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadClockSnapshot {
    pub trace_ns: u64,      // Trace-clock value at snapshot.
    pub wall_ns: u64,       // Approximate wall-time (host-injected before run).
}
// size = 16
```

OBS-SER-TRACK-LIFECYCLE: kernel emits `TrackDescriptor` records once per static track at boot (hart tracks) and on object creation for dynamic tracks (ReactorTask, ProcessIdentity, DelegateEndpoint). Reclamation emits `TrackTombstone` kind so the daemon can age out tracks in long runs.

### 8.6 Argument continuation

<!-- txdoc:OBS-SER-V0-ARGCONT-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadArgValue {
    /// DebugAnnotationNameId (e.g., "fd", "buf", "len", "errno").
    pub key: u32,
    pub value_kind: u8,     // TxValueKind below
    pub _pad: [u8; 3],
    pub value0: u64,        // Numeric value, or low-64 of TraceObjectId.
}
// size = 16

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxValueKind {
    None     = 0,
    U64      = 1,
    I64      = 2,
    Bool     = 3,
    Ptr      = 4,    // Raw user VA, opaque.
    Errno    = 5,
    NameId   = 6,    // Reference to interned string table.
    ObjectId = 7,    // Packed (kind, generation, slot) — see 08_OBSERVATION_v1.md §12.
    FlowId   = 8,
}
```

OBS-SER-ARGCONT-ATTACH: `ArgValue` records attach to the *most recent* span on the same hart that has remaining `arg_count` capacity (set on the SpanBegin/Instant they belong to). Daemon attaches by walking back through hart's sequence, never across harts.

OBS-SER-ARGCONT-NO-STRINGS: v0 does not support arbitrary-length string values. Names are `NameId`; pointers are opaque `u64`. Path strings, comm names, and similar dynamic content are deferred ([`08_OBSERVATION_v1.md §15.4`](08_OBSERVATION_v1.md)).

### 8.7 Mutation payloads

<!-- txdoc:OBS-SER-V0-MUTATION-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadMutationZoneSign {
    pub object_id: u64,     // TraceObjectId packed form.
    pub kind: u8,           // ZoneKindTag
    pub _pad: [u8; 7],
}
// size = 16

#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadMutationIndexCommit {
    pub index_id: u32,
    pub key_low: u32,
    pub value_object_id: u64,  // The Cap<T> committed under the key.
}
// size = 16
```

These payloads are L6 (`Mutation`).  Landed in OBS-8; emit is gated by
`tx_substrate::zone::MUTATION_EMIT_ENABLED` (zone sign) and
`tx_substrate::index::INDEX_MUTATION_EMIT_ENABLED` (index commit), both
default-off.  The daemon decodes both tags via `read_as!` in
`tools/tx-trace-daemon/src/decode.rs`.

### 8.9 Phase transition payload (OBS-8)

<!-- txdoc:OBS-SER-V0-PHASE-1 -->

```rust
/// Boot-phase discriminant for PayloadPhaseTransition.
#[repr(u8)]
#[derive(Copy, Clone)]
pub enum BootPhaseKind {
    SubstrateBsp = 0,  // tx_substrate::init (BSP)
    SubstrateAp  = 1,  // tx_substrate::init_on_ap (per AP)
}

/// Payload for SpanBegin(phase.<kind>) / SpanEnd at kernel boot phase
/// boundaries (L5, TxPayloadTag::PhaseTransition = 52).
///
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
pub struct PayloadPhaseTransition {
    pub phase_kind: u8,   // BootPhaseKind
    pub hart_id: u8,      // CpuId.0 truncated to u8
    pub _pad: [u8; 14],   // explicit padding to 16 bytes
}
```

**Wire layout (OBS-8):**

| offset | field        | size | notes                        |
|-------:|--------------|-----:|------------------------------|
|      0 | `phase_kind` |    1 | `BootPhaseKind` discriminant |
|      1 | `hart_id`    |    1 | hart index (lower 8 bits)    |
|      2 | `_pad`       |   14 | explicit zero padding        |
|     16 | (end)        |      |                              |

L5 Phase spans fire at substrate `init` (BSP, `hart_id = 0`) and
`init_on_ap` (AP, `hart_id = cpu.0`).  No runtime gate — L5 is
low-frequency (a handful per boot) and always on when observation is
wired.  Daemon decodes via `read_as!(PayloadPhaseTransition)` added in
`tools/tx-trace-daemon/src/decode.rs`.

### 8.8 Panic payload

<!-- txdoc:OBS-SER-V0-PANIC-1 -->

```rust
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadPanic {
    /// EventNameId for the panic site (often the file:line via interned location id).
    pub site_name: u32,
    pub _pad: u32,
    /// The panic-handler's hart_id and a marker for the in-band quiesce signal.
    pub panic_hart: u16,
    pub flags: u16,         // bit 0: kernel halted; bit 1: ring truncated.
    pub _pad2: u32,
}
// size = 16
```

OBS-SER-PANIC-FLOW: the kernel panic handler walks each hart's ring and writes one `PanicMarker` record into its own hart's ring (the only hart still running). It sets `flags |= 0x1` (halted) and quiesces. Daemon detects PanicMarker and surfaces "kernel panicked at X" with a synthetic flame-graph end-of-trace marker.

Out of MVP; v0 reserves the schema. OBS-3a does not implement the panic-time path.

## 9. Static string descriptor

<!-- txdoc:OBS-SER-V0-STRING-1 -->

When `header.string_table_off != 0`, the region contains a kernel-embedded string table at that offset, format:

```text
StringTable {
  count: u32          // Number of entries.
  entries: [Entry; count]
}

Entry {
  id: u32             // NameId / EventNameId / ArgNameId
  offset: u32         // Byte offset into the bytes blob below.
  length: u32         // Byte length of the string.
}

bytes: [u8; ...]      // Concatenated string bytes; no null terminator.
```

OBS-SER-STRING-EMBEDDED: when the table is present, the daemon uses it as the canonical name resolver. When absent (`string_table_off == 0`), the daemon falls back to an out-of-band `names.json` keyed by `boot_id`. Precedence: embedded > names.json.

OBS-SER-STRING-MVP: v0 MVP does *not* require the embedded table. `names.json` generated post-build from the kernel ELF symbol table is sufficient.

## 10. Versioning

<!-- txdoc:OBS-SER-V0-VERSION-1 -->

Two version axes:
- `TxTraceHeader.version` — region/header schema version.
- `TxTraceRecord.version` — record/payload schema version.

Rules:
- v0 records are fixed-size (80 bytes).
- Unknown `TxTraceKind` values are skipped (treated as `Nop`).
- Unknown `TxPayloadTag` values are skipped if `payload_len <= 16`.
- Adding a field to an existing payload struct requires a new payload tag OR a header version bump (do not silently extend; readers won't notice).
- Changing `record_size` requires `TxTraceHeader.record_size` update and a header version bump.
- Daemon must report unsupported versions clearly and refuse to decode silently.

OBS-SER-VERSION-FORWARD: v0 daemon decoding a v0 region with unknown kind 99 must produce one warning ("unknown record kind 99 at seq N; skipped") and continue. It must not silently produce a malformed trace.

## 11. Memory ordering recap

<!-- txdoc:OBS-SER-V0-ORDER-1 -->

Producer:
```text
1. Acquire-load producer (own value); Acquire-load consumer.
2. If full, fetch_add lost (Relaxed), return.
3. Write all record fields with Relaxed stores.
4. Release-fence (or Release-store of producer; either works).
5. Release-store producer + 1.
```

Consumer:
```text
1. Acquire-load producer.
2. While consumer != producer:
3.   Read slot[consumer & (slot_count - 1)] — all field reads see the
     state at the moment producer was advanced past this slot, via
     happens-before from producer's Release.
4.   consumer += 1.
5. Release-store consumer.
```

OBS-SER-ORDER-FENCE-CHOICE: either single Release-store of producer at end (with Relaxed field stores before it) or Release fence + Relaxed store of producer. Both establish the required happens-before. Implementation picks based on instruction count.

## 12. Region discovery

<!-- txdoc:OBS-SER-V0-DISCOVERY-1 -->

How the daemon finds the region is *not* specified by txtrace-v0; that's transport-level. See [`08_OBSERVATION_HOST_v0.md §3`](08_OBSERVATION_HOST_v0.md). The wire format only specifies the region's *layout* once the daemon has a `&[u8]` slice over it.

## 13. Open schema points

<!-- txdoc:OBS-SER-V0-OPEN-1 -->

13.1. **String/blob continuation (`ArgBlob`).** A multi-record continuation variant for path strings would chain by `span` and a sub-sequence counter. Schema reserved; not in v0.

13.2. **64-bit `wait_generation` in producer payloads.** Current `PayloadWaitSourceNotify` carries only the low 32 bits to fit in 16-byte inline. If 4G yields per task per run becomes constraining, a v0.1 bump expands to two records (one for the source, one for the high half).

13.3. **Counter histograms.** `PayloadCounterValue` is a single sample. Aggregated/histogrammed counters (mailbox occupancy distribution, EBR retire batch size) are reserved for v0.x.

13.4. **TrackTombstone schema.** Kind reserved (14); payload schema unspecified. Likely `{ track_id: u64 }`. Land when long-running observation traces materialize.

13.5. **PanicMarker emission discipline.** [§8.8](#88-panic-payload) reserves the schema; the producer-side discipline (which hart writes the marker, how other harts are quiesced) is in [`08_OBSERVATION_v1.md §15.x`](08_OBSERVATION_v1.md) and out of MVP.

## 14. Compile-time assertions

<!-- txdoc:OBS-SER-V0-ASSERT-1 -->

The `tx-observe-types` crate must include compile-time assertions for every layout commitment:

```rust
// tx-observe-types/src/lib.rs (or a dedicated assert.rs)
const _: () = {
    assert!(core::mem::size_of::<TxTraceHeader>() == 72);
    assert!(core::mem::align_of::<TxTraceHeader>() == 8);

    assert!(core::mem::size_of::<TxTraceRecord>() == 80);
    assert!(core::mem::align_of::<TxTraceRecord>() == 8);

    // Each payload struct ≤ 16 bytes.
    assert!(core::mem::size_of::<PayloadSyscallEnter>() <= 16);
    assert!(core::mem::size_of::<PayloadSyscallExit>() <= 16);
    assert!(core::mem::size_of::<PayloadDriveBegin>() <= 16);
    assert!(core::mem::size_of::<PayloadDriveEnd>() <= 16);
    assert!(core::mem::size_of::<PayloadStepOutcome>() <= 16);
    assert!(core::mem::size_of::<PayloadYieldBegin>() <= 16);
    assert!(core::mem::size_of::<PayloadResume>() <= 16);
    assert!(core::mem::size_of::<PayloadWaitSourceNotify>() <= 16);
    assert!(core::mem::size_of::<PayloadTrackDescriptor>() <= 16);
    assert!(core::mem::size_of::<PayloadArgValue>() <= 16);
    // (etc. for all payload structs)

    // Ring header cacheline padding sanity.
    // producer is at offset 64; consumer is at offset 128; lost is at 192.
    // ... verify via offset_of! when stable, or struct offset_of macro.
};
```

OBS-SER-V0-CI-ASSERT: these assertions must fail compilation if any wire-format struct changes size. Pair with a `cargo expand` review in PR-OBS-1 to confirm field layout.

## 15. Pod marker

<!-- txdoc:OBS-SER-V0-POD-1 -->

Every wire-format struct in `tx-observe-types` is marked with the existing `tx-hal::Pod` trait:

```rust
// tx-observe-types/src/lib.rs
use tx_hal::Pod;

unsafe impl Pod for TxTraceHeader {}
unsafe impl Pod for TxTraceRecord {}
unsafe impl Pod for PayloadSyscallEnter {}
unsafe impl Pod for PayloadSyscallExit {}
// ... etc.
```

`TxTraceHartRing` is *not* `Pod` because of its `AtomicU64` fields. It is accessed via raw pointer reads on the host side, with the same memory-ordering protocol as the kernel side.

OBS-SER-V0-POD-REUSE: this is the existing [`tx-hal::Pod`](../../crates/tx-hal/src/lib.rs#L814), not a new trait. No bytemuck dependency is added.
