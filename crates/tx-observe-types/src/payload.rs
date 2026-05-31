//! Payload tag enum and all inline payload structs.
//!
//! Every payload struct must fit in 16 bytes (the inline buffer in
//! [`TxTraceRecord`](crate::TxTraceRecord)).  Sizes are enforced by
//! compile-time assertions in `lib.rs`.
//!
//! Layout spec: `08_OBSERVATION_SERIALIZATION_v0.md` §8.

// ---------------------------------------------------------------------------
// Payload tag
// ---------------------------------------------------------------------------

/// Identifies the schema of the bytes in
/// [`TxTraceRecord::payload`](crate::TxTraceRecord::payload).
#[repr(u16)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxPayloadTag {
    None = 0,

    // ── L0 boundary ──────────────────────────────────────────────────────
    SyscallEnter = 1,
    SyscallExit = 2,

    // ── L2 drive / L4 step ───────────────────────────────────────────────
    DriveBegin = 10,
    DriveEnd = 11,
    StepOutcome = 12,

    // ── L3 yield/resume / wake ───────────────────────────────────────────
    YieldBegin = 20,
    Resume = 21,
    WaitSourceNotify = 22,
    AgentStateChange = 23,

    // ── Track / metadata ─────────────────────────────────────────────────
    TrackDescriptor = 30,
    CounterValue = 31,
    StringDescriptor = 32,
    ClockSnapshot = 33,

    // ── Argument continuation ─────────────────────────────────────────────
    ArgValue = 40,

    // ── Mutation (L6) ─────────────────────────────────────────────────────
    MutationZoneSign = 50,
    MutationIndexCommit = 51,

    // ── Phase (L5) ────────────────────────────────────────────────────────
    /// Kernel boot phase transition (OBS-8).
    ///
    /// Carried on `SpanBegin` / `SpanEnd` records at the substrate-level
    /// phase boundaries: BSP `init` and per-AP `init_on_ap`.  Payload:
    /// [`PayloadPhaseTransition`].
    ///
    /// Layout spec: `08_OBSERVATION_SERIALIZATION_v0.md` §8 (OBS-8, added).
    PhaseTransition = 52,

    // ── Sched (L7 — OBS-9) ────────────────────────────────────────────────
    /// Reactor task-on-hart slice payload, paired across
    /// `SpanBegin(Sched)` (dispatch) and `SpanEnd(Sched)` (yield).
    /// Payload: [`PayloadSchedSwitch`].
    SchedSwitch = 53,

    // ── Process identity label (OBS-V1 §15.7) ────────────────────────────
    /// One-shot mapping from `process_id_low` → human-readable
    /// program name (PCB `comm` — short 16-byte Linux-style identity).
    ///
    /// Emitted as an `Instant` once per process at submit time so the
    /// trace daemon can build a `ProcessDescriptor.process_name` from
    /// the actual program identity (e.g. `busybox`, `basic_exec`)
    /// instead of falling back to the synthetic `pid-<N>` label.
    /// Payload: [`PayloadProcessLabel`].
    ProcessLabel = 54,

    /// One-shot mapping from `process_id_low` → owning process-group
    /// id + session id (PCB `pgid` / `sid`). Lets the daemon nest
    /// per-process Perfetto tracks under per-pgrp / per-session
    /// swimlanes so test runners + their fork()ed children render as
    /// a coherent group rather than scattered top-level lanes.
    /// Emitted alongside [`Self::ProcessLabel`]. Payload:
    /// [`PayloadProcessGroup`].
    ProcessGroup = 55,

    /// One-shot parent → child fork edge. Emitted once on the child's
    /// submission so the daemon can draw a control-flow arrow from
    /// the parent's `clone()` syscall slice to the child's first
    /// `Sched` dispatch — making process-tree spawning visible on
    /// the timeline instead of just appearing as a new top-level
    /// track out of nowhere. Payload: [`PayloadProcessFork`].
    ProcessFork = 56,

    // ── Panic (special) ───────────────────────────────────────────────────
    Panic = 60,
}

// ---------------------------------------------------------------------------
// §8.1 Syscall payloads
// ---------------------------------------------------------------------------

/// Payload for `SpanBegin` at the syscall entry boundary (L0).
/// size = 8
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadSyscallEnter {
    /// Linux syscall number.
    pub sysno: u32,
    /// ABI identifier: 0 = LinuxRv64, 1 = LinuxLa64.
    pub abi: u16,
    /// Number of `ArgValue` continuation records that follow.
    pub argc: u16,
}

/// Payload for `SpanEnd` at the syscall exit boundary (L0).
///
/// Field order chosen so the largest field (`i64`) lands on its natural
/// alignment without internal padding.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadSyscallExit {
    /// Syscall return value (or partial count).
    pub ret: i64,
    /// Errno.  0 if Ok.
    pub errno: i32,
    /// 0=Ok, 1=Err, 2=Restart, 3=Fatal, 4=NoReturn
    pub result_kind: u8,
    pub _pad: [u8; 3],
}

// ---------------------------------------------------------------------------
// §8.2 Drive payloads
// ---------------------------------------------------------------------------

/// Payload for `SpanBegin` at drive-loop entry (L2).
/// size = 12
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadDriveBegin {
    /// `EventNameId` from `TypeId::of::<O>()` truncated to `u32`.
    pub op_type: u32,
    /// 0=Nonblocking, 1=Waiting, 2=Selecting
    pub mode: u8,
    /// 0=Uninterruptible, 1=Interruptible, 2=Killable
    pub interrupt: u8,
    pub has_deadline: u8,
    pub _pad: u8,
    /// `task_id_low` (32-bit truncation).
    pub task_id_low: u32,
}

/// Payload for `SpanEnd` at drive-loop exit (L2).
///
/// Mirrors [`PayloadSyscallExit`] shape for symmetric reading.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadDriveEnd {
    /// Final `Output` for `Done`; 0 for `Err`.
    pub ret: i64,
    /// Final errno for `Err`; 0 for `Done`.
    pub errno: i32,
    /// 0=Done, 1=Err
    pub result_kind: u8,
    pub _pad: [u8; 3],
}

// ---------------------------------------------------------------------------
// §8.3 Step outcome payload
// ---------------------------------------------------------------------------

/// Payload carried on the `SpanEnd(step.iteration)` record (L4).
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadStepOutcome {
    /// 0=Continue, 1=Yield, 2=Done, 3=Err
    pub variant: u8,
    /// 0 = progress is EMPTY; 1 = progress has data.
    pub progress_empty: u8,
    /// One of [`TxProgressKind`].
    pub progress_kind: u8,
    /// One of [`YieldShapeKind`] (valid iff `variant == 1`).
    pub shape_kind: u8,
    /// Errno (valid iff `variant == 3`).
    pub errno: i32,
    /// Numeric progress count (bytes / pages / entries / iovecs done).
    pub progress_value: u32,
    pub _pad: u32,
}

/// Progress kind for [`PayloadStepOutcome`].
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxProgressKind {
    NoProgress = 0,
    ByteProgress = 1,
    PageProgress = 2,
    EntryProgress = 3,
    IoVecProgress = 4,
}

/// Compact wire encoding of `YieldShape` variant for trace records.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum YieldShapeKind {
    OnWaitSource = 1,
    OnAgent = 2,
    OnTimer = 3,
    // Future: OnEdge = 4, OnHandoff = 5.
}

// ---------------------------------------------------------------------------
// §8.4 Yield/resume payloads
// ---------------------------------------------------------------------------

/// Payload for `SpanBegin(yield.<shape>)` record (L3).
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadYieldBegin {
    /// [`YieldShapeKind`]
    pub shape_kind: u8,
    pub _pad: [u8; 3],
    pub task_id_low: u32,
    pub wait_generation: u64,
}

/// Payload for `Instant(resume)` record (L3).
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadResume {
    /// 0=Retry, 1=WithReply, 2=TimerExpired, 3=Aborted
    pub resume_kind: u8,
    /// 0=Signal, 1=Canceled, 2=TimedOut, 3=AgentDied, 4=BorrowerExited
    pub abort_reason: u8,
    pub _pad: [u8; 2],
    /// Compact source/token/timer id (lower 32 bits).
    pub object_id_low: u32,
    pub wait_generation: u64,
}

/// Payload for `Instant(wake.notify)` record (L3, producer side).
///
/// `wait_generation_high` lives in the `seq` field of the record; there is no
/// room for it in the 16-byte inline buffer.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadWaitSourceNotify {
    pub source_id_low: u32,
    pub mask_bits: u32,
    /// Material for flow-id reconstruction.  Daemon hashes
    /// `(task_id, wait_gen, flow_kind)` to produce Perfetto flow ids.
    pub task_id_low: u32,
    pub wait_generation_low: u32,
}

// ---------------------------------------------------------------------------
// §8.5 Track / metadata payloads
// ---------------------------------------------------------------------------

/// Payload for `TrackDescriptor` records.
/// size = 16
///
/// Field order: `track_id` (u64) followed by `name` (u32) followed by
/// `track_kind` (u8) followed by `_pad` ([u8; 3]).  This ordering avoids
/// implicit padding that the spec's original listing (`track_kind` before
/// `name`) would produce (an implicit 3-byte gap between the u8 and u32 would
/// push the struct to 24 bytes, exceeding the 16-byte payload budget).  The
/// reordering is a spec clarification, not an ABI divergence, because no
/// producer/consumer existed prior to OBS-1.
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadTrackDescriptor {
    pub track_id: u64,
    /// `EventNameId`; daemon resolves human name.
    pub name: u32,
    /// 0=Hart, 1=Task, 2=Process, 3=Scope, 4=Endpoint, 5=Timer
    pub track_kind: u8,
    pub _pad: [u8; 3],
}

/// Payload for `Counter` records.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadCounterValue {
    pub counter_id: u32,
    pub _pad: u32,
    /// `i64` reinterpretation is OK for signed counters.
    pub value: u64,
}

/// Payload for `ClockSnapshot` records.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadClockSnapshot {
    /// Trace-clock value at snapshot.
    pub trace_ns: u64,
    /// Approximate wall-time (host-injected before run).
    pub wall_ns: u64,
}

// ---------------------------------------------------------------------------
// §8.6 Argument continuation
// ---------------------------------------------------------------------------

/// Payload for `ArgContinuation` records.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadArgValue {
    /// `DebugAnnotationNameId` (e.g. "fd", "buf", "len", "errno").
    pub key: u32,
    /// [`TxValueKind`]
    pub value_kind: u8,
    pub _pad: [u8; 3],
    /// Numeric value, or low-64 of `TraceObjectId`.
    pub value0: u64,
}

/// Value kind for [`PayloadArgValue`].
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxValueKind {
    None = 0,
    U64 = 1,
    I64 = 2,
    Bool = 3,
    /// Raw user VA, opaque.
    Ptr = 4,
    Errno = 5,
    /// Reference to interned string table.
    NameId = 6,
    /// Packed `(kind, generation, slot)` — see `08_OBSERVATION_v1.md §12`.
    ObjectId = 7,
    FlowId = 8,
}

// ---------------------------------------------------------------------------
// Explicit diagnostic allocation tracks
// ---------------------------------------------------------------------------

/// High nibble used in an `Instant.parent` field to mean "route this instant
/// to an explicit diagnostic track id" instead of treating `parent` as a span.
///
/// This preserves the fixed 80-byte record and 16-byte payload ABI: allocation
/// probes use `Instant + PayloadArgValue`, with `parent` carrying one of the
/// `ALLOC_TRACK_*` constants below.
pub const EXPLICIT_TRACK_ID_PREFIX: u64 = 0xD500_0000_0000_0000;
pub const EXPLICIT_TRACK_ID_MASK: u64 = 0xFF00_0000_0000_0000;

pub const ALLOC_TRACK_ZONE_SLAB: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0001;
pub const ALLOC_TRACK_PAGE_FRAME: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0002;
pub const ALLOC_TRACK_PAGE_RUN: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0003;
pub const ALLOC_TRACK_VM_RECIPE_NODE: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0004;
pub const ALLOC_TRACK_VM_PRIVATE_PAGE_NODE: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0005;
pub const ALLOC_TRACK_PAGEBACKED_CACHE: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0006;
pub const ALLOC_TRACK_VM_ADDRESS_SPACE: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0007;
pub const ALLOC_TRACK_PAGEBACKED_CONTAINER: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0008;

// ---------------------------------------------------------------------------
// §8.7 Mutation payloads (L6, deferred from MVP — schemas reserved)
// ---------------------------------------------------------------------------

/// Payload for `Instant(mutation.zone_sign)` records (L6).
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadMutationZoneSign {
    /// `TraceObjectId` packed form.
    pub object_id: u64,
    /// `ZoneKindTag`
    pub kind: u8,
    pub _pad: [u8; 7],
}

/// Payload for `Instant(mutation.index_commit)` records (L6).
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadMutationIndexCommit {
    pub index_id: u32,
    pub key_low: u32,
    /// The `Cap<T>` committed under the key.
    pub value_object_id: u64,
}

// ---------------------------------------------------------------------------
// §8.8 Panic payload (reserved for post-MVP OBS-3a panic path)
// ---------------------------------------------------------------------------

/// Payload for `PanicMarker` records.
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadPanic {
    /// `EventNameId` for the panic site (often the file:line interned
    /// location id).
    pub site_name: u32,
    pub _pad: u32,
    /// The panic handler's `hart_id`.
    pub panic_hart: u16,
    /// bit 0: kernel halted; bit 1: ring truncated.
    pub flags: u16,
    pub _pad2: u32,
}

// ---------------------------------------------------------------------------
// §8.9 Phase transition payload (L5, OBS-8)
// ---------------------------------------------------------------------------

/// Boot-phase discriminant for [`PayloadPhaseTransition`].
///
/// Identifies the substrate or kernel subsystem phase that is beginning
/// or ending.  The numeric values are stable wire ABI.
///
/// Layout spec: `08_OBSERVATION_SERIALIZATION_v0.md` §8 (OBS-8, added).
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum BootPhaseKind {
    /// Kernel BSP init (`tx_hal::init_early`).
    SubstrateBsp = 0,
    /// Kernel AP init (`tx_hal::init_later`).
    SubstrateAp = 1,
}

/// Payload for `SpanBegin(phase.<name>)` / `SpanEnd` at kernel boot
/// phase boundaries (L5, OBS-8).
///
/// Wire layout (OBS-8):
/// ```text
/// offset 0: phase_kind  u8   — one of BootPhaseKind
/// offset 1: hart_id     u8   — hart index (lower 8 bits of CpuId)
/// offset 2: _pad        [u8; 14]
/// total = 16
/// ```
///
/// `_pad` is explicit to make every byte named and avoid implicit compiler
/// padding (OBS-SER-V0-PAYLOADS-1 discipline).
///
/// size = 16
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadPhaseTransition {
    /// One of [`BootPhaseKind`].
    pub phase_kind: u8,
    /// Hart index (`CpuId.0` truncated to u8).
    pub hart_id: u8,
    pub _pad: [u8; 14],
}

// ---------------------------------------------------------------------------
// §8.10 Sched payload (L7 — OBS-9)
// ---------------------------------------------------------------------------

/// Reactor task-on-hart slice payload.
///
/// Carried twice per task-poll cycle:
/// - on `SpanBegin(Sched)` with `kind = SchedKind::Dispatch (0)` and
///   `reason = 0` when the reactor picks the task off its hart's
///   runqueue and is about to call `Future::poll`;
/// - on `SpanEnd(Sched)` with `kind = SchedKind::Yield (1)` and
///   `reason` populated from [`SchedReason`] after the poll returns.
///
/// Layout spec: `08_OBSERVATION_v1.md` §15.6 (OBS-9 reactor scheduler
/// track).  size = 8 bytes used, 16 bytes total (matches the inline
/// payload buffer in `TxTraceRecord`).
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadSchedSwitch {
    /// `TaskMailbox::task_id_low()` of the task gaining (Dispatch) or
    /// releasing (Yield) the hart.  Matches the `task_id_low` field on
    /// other observation payloads so the daemon can build per-task
    /// Perfetto tracks.
    pub task_id_low: u32,
    /// `TaskMailbox::process_id_low()` — the thread-group leader's PID.
    /// For user threads this is the process's TGID; for kernel-only
    /// tasks it is `0`. Lets the daemon parent each `task.<tid>`
    /// thread track under the right `process.<pid>` Perfetto process
    /// track instead of the single kernel-wide `txKernel` aggregate.
    pub process_id_low: u32,
    /// Hart the switch is happening on.  Mirrors `TxTraceRecord.hart`
    /// for grep-stability — the wire carries it twice intentionally so
    /// the daemon's per-hart track lifecycle stays self-contained even
    /// if the per-record hart field is filtered.
    pub hart_id: u8,
    /// One of [`SchedKind`] — `Dispatch` (0) on SpanBegin, `Yield` (1)
    /// on SpanEnd.
    pub kind: u8,
    /// One of [`SchedReason`] — populated on `Yield` SpanEnd records;
    /// `0` (None) on Dispatch SpanBegin.
    pub reason: u8,
    pub _pad: [u8; 5],
}

/// Sched-switch direction tag for [`PayloadSchedSwitch::kind`].
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum SchedKind {
    /// Reactor is about to call `Future::poll` on the task.  Opens the
    /// task-on-hart slice.
    Dispatch = 0,
    /// `Future::poll` returned.  Closes the task-on-hart slice.
    Yield = 1,
}

/// Why a task released the hart (set on `Yield` records).
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum SchedReason {
    /// No reason recorded (Dispatch records and synthetic fallbacks).
    None = 0,
    /// `Future::poll` returned `Poll::Pending` — the task parked.
    Parked = 1,
    /// `Future::poll` returned `Poll::Ready(())` — the task completed.
    Completed = 2,
    /// `Future::poll` returned `Poll::Pending` but the task's wake bit
    /// was already set during the poll itself, so the reactor will
    /// re-dispatch immediately.  (`mark_runnable_from_hart` path.)
    WokeDuringPoll = 3,
}

/// One-shot mapping from `process_id_low` to a 12-byte slice of the
/// PCB short program name.  Emitted as an `Instant` once per process,
/// immediately after submit.  The daemon caches `pid → name` and
/// uses it as the `ProcessDescriptor.process_name` when first
/// materialising the per-process Perfetto track, so the timeline
/// shows real program names (`busybox`, `basic_exec`) instead of the
/// synthetic `pid-<N>` fallback.
///
/// `comm` is truncated to 12 bytes (vs Linux's 16) so the whole
/// payload fits in `TxTraceRecord.payload` (16 bytes inline). Names
/// longer than 11 chars + NUL are truncated; this is fine for the
/// oscomp + busybox workloads where `comm` is typically ≤ 8 bytes.
///
/// Layout spec: `08_OBSERVATION_v1.md` §15.7 (OBS-9 process labels).
/// size = 16.
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadProcessLabel {
    /// PCB PID low 32 bits — keyed against `PayloadSchedSwitch.process_id_low`.
    pub process_id_low: u32,
    /// First 12 bytes of `ProcessIdentity::comm()` (NUL-padded ASCII).
    /// Truncated from the full 16-byte `TASK_COMM_LEN`-style buffer to
    /// fit the inline payload size.
    pub comm: [u8; 12],
}

/// One-shot mapping from `process_id_low` → owning `pgid` + `sid`.
/// Emitted as an `Instant` alongside [`PayloadProcessLabel`] at
/// submit / post-exec time. The daemon caches `pid → (pgid, sid)`
/// and parents each per-process Perfetto track under a per-pgrp
/// swimlane so a shell + its fork()ed children render together.
///
/// Layout spec: `08_OBSERVATION_v1.md` §15.8 (OBS-9 process groups).
/// size = 16.
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadProcessGroup {
    /// PCB PID low 32 bits.
    pub process_id_low: u32,
    /// PCB `Pgid` (process-group id) low 32 bits.
    pub pgid_low: u32,
    /// PCB `Sid` (session id) low 32 bits.
    pub sid_low: u32,
    /// Reserved (kept zero).
    pub _pad: u32,
}

/// Parent → child fork edge. Emitted as an Instant once per
/// new process at submit time. The daemon hashes
/// `(parent_pid, child_pid)` into a Perfetto `flow_id` and emits
/// two `FlowEvent`s — one anchored to the parent's most recent
/// `clone()` slice, one anchored to the child's first Sched
/// dispatch — so the timeline shows an arrow from the parent's
/// fork-point to the child's first run.
///
/// Layout spec: `08_OBSERVATION_v1.md` §15.9 (OBS-9 fork edges).
/// size = 16.
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct PayloadProcessFork {
    /// Forking parent's PID low 32 bits.
    pub parent_pid_low: u32,
    /// Newly-spawned child's PID low 32 bits.
    pub child_pid_low: u32,
    /// Reserved for `flags` (CLONE_*) on a future revision.
    pub _flags: u32,
    /// Reserved for alignment / future fields.
    pub _pad: u32,
}

// ---------------------------------------------------------------------------
// §13.2 FlowKind (shared between host and kernel wire material)
// ---------------------------------------------------------------------------

/// Flow-id computation discriminant.  The kernel emits the *material*
/// (`task_id_low`, `wait_generation`, `shape_kind`/`resume_kind`); the daemon
/// computes the Perfetto `flow_id` by hashing `(task_id, wait_gen, FlowKind)`.
///
/// Lives in the shared types crate so the daemon can refer to the same enum
/// the spec defines.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum FlowKind {
    SourceWake = 1,    // WaitSource::notify → resume
    AgentReply = 2,    // DelegateToken::reply → resume
    TimerExpire = 3,   // TimerToken expiry → resume
    AbortDelivery = 4, // generationless abort → resume
}
