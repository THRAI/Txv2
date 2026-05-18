//! `tx-observe` — kernel-side observation runtime (OBS-2).
//!
//! This crate owns:
//! - Per-hart emitter state (`HartEmitter`) placed via `HartLocalArray`.
//! - SPSC ring producer logic: slot claim, record write, cursor advance.
//! - Span / flow ID namespacing (`SpanId`, `EventNameId`).
//! - The public emit API that OBS-3 will wire to convergence points.
//!
//! # Invariants upheld (spec §7)
//!
//! - **OBS-1**: every emit path is `O(1)` and bounded.
//! - **OBS-3**: zero allocation (`#![no_std]`, no allocator).
//! - **OBS-4**: never blocks — no mutex, no CAS on the producer path.
//! - **OBS-5**: lossy on overrun — bumps `ring.lost` (Relaxed), returns
//!   immediately.
//! - **OBS-9**: span IDs are unique within `(hart, boot)` — hart_id occupies
//!   bits 56..64, local counter occupies bits 0..56.
//!
//! # Anti-pattern OBS-A-1
//!
//! Do **not** call emit functions from inside `StepOp::poll` bodies.
//! All emit call sites are boundary, drive, and yield convergence points
//! wired by OBS-3a.
//!
//! Spec refs:
//!   txdoc:OBS-V1-RUNTIME-1  — `HartEmitter` struct and placement
//!   txdoc:OBS-V1-RUNTIME-2  — producer ring dance
//!   txdoc:OBS-V1-RUNTIME-3  — span/flow ID namespacing
//!   txdoc:OBS-V1-RUNTIME-4  — overrun handling

#![no_std]

use core::sync::atomic::{AtomicU64, Ordering};

use tx_hal::{ConsoleIf, CpuId, ObserverIf, PercpuIf, TimeIf};
use tx_observe_types::{
    PayloadCounterValue, TxPayloadTag, TxTraceHartRing, TxTraceKind, TxTraceRecord,
};
// TxTraceLevel is imported via pub use below so the same name is available
// both inside this module and as a macro-accessible re-export.
pub use tx_observe_types::TxTraceLevel;

mod hart_local;
use hart_local::{HartLocalArray, HartLocalOptionArray};

pub mod encode;
mod macros;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

// Re-exports used by the `traced_syscall!` macro so crates that use the macro
// do not need to directly depend on `tx-observe-types`.
#[doc(hidden)]
pub use tx_observe_types::{PayloadSyscallEnter, PayloadSyscallExit};

// ---------------------------------------------------------------------------
// Public newtypes
// ---------------------------------------------------------------------------

/// Span id.
///
/// Bits 0..56: hart-local monotone counter.
/// Bits 56..64: `hart_id` (lower 8 bits of `CpuId`).
///
/// Opaque newtype so callers cannot fabricate one.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct SpanId(u64);

/// FNV-1a 32-bit hash over a byte slice. `const fn` so emit call sites
/// build stable `EventNameId`s for fixed event names (e.g. `"resume"`,
/// `"wake.notify"`, `"mutation.zone_sign"`) at compile time. Matches the
/// kernel-side `op_name_id::<S>()` hash used in `tx_scripts::drive` so a
/// single hash space covers every `EventNameId` source.
#[inline]
pub const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

impl SpanId {
    /// The zero sentinel ("no span / orphan").
    pub const NONE: Self = Self(0);

    /// Raw inner value — for writing into wire records.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct a `SpanId` from its raw wire value.
    ///
    /// Used when a parent span id has been threaded through a non-trace
    /// channel (e.g. the per-hart parent-span slot or `ScriptCtx`)
    /// and needs to be fed back into `HartEmitter::span_begin` as
    /// `parent`. A raw of `0` maps to `SpanId::NONE` (no ancestor).
    #[inline]
    pub const fn from_raw_or_none(raw: u64) -> Self {
        Self(raw)
    }
}

// ---------------------------------------------------------------------------
// Per-hart current-parent-span slot
//
// Used to thread the "currently open ancestor span" id from L0 (syscall
// boundary) down to L2 (`drive`) without having to thread it through every
// `ScriptCtx` construction in tx-shims. The slot is installed by the
// dispatcher with `set_current_parent_span(l0_span)` and read by `drive`
// with `current_parent_span()`. Each `set_*` returns the previous value so
// the caller can restore it (RAII-style) at scope exit.
//
// Concurrency: the slot is per-hart (`HartLocalArray`-style indexing); the
// dispatcher always executes on the hart that opened the L0 span. Cross-
// hart task migration during a yield is handled by the daemon-side parent
// reconstruction fallback (timestamp + hart_id correlation per OBS-V1
// §13.1). For single-hart syscall arms (the common case), the parent
// linkage is exact.
// ---------------------------------------------------------------------------

static PARENT_SPANS: [AtomicU64; MAX_HARTS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_HARTS]
};

/// Install a parent span for the current hart's emit calls.
///
/// Returns the previous value so the caller can restore it once the scope
/// that owns the new parent ends (typical pattern: save → run inner →
/// restore). Returns `SpanId::NONE` when no emitter is installed on this
/// hart (the per-hart slot is still updated, but reading it later will
/// behave identically to never having set it).
#[inline]
pub fn set_current_parent_span(span: SpanId) -> SpanId {
    let idx = read_current_cpu_id().map(|c| c.0).unwrap_or(MAX_HARTS);
    if idx >= MAX_HARTS {
        return SpanId::NONE;
    }
    let prev = PARENT_SPANS[idx].swap(span.0, Ordering::Relaxed);
    SpanId(prev)
}

/// Read the parent span installed on the current hart.
///
/// Returns `SpanId::NONE` if no parent was installed or the hart id is
/// out of range. Used by [`HartEmitter`] internals and by the `drive`
/// loop to attach `PayloadDriveBegin` to its L0 ancestor.
#[inline]
pub fn current_parent_span() -> SpanId {
    let idx = read_current_cpu_id().map(|c| c.0).unwrap_or(MAX_HARTS);
    if idx >= MAX_HARTS {
        return SpanId::NONE;
    }
    SpanId(PARENT_SPANS[idx].load(Ordering::Relaxed))
}

/// Event name id — low 32 bits of `core::any::TypeId`.
///
/// Opaque newtype so callers cannot fabricate one without going through
/// the approved constructors.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct EventNameId(u32);

impl EventNameId {
    /// Derive an `EventNameId` from the `TypeId` of type `T`.
    ///
    /// Takes the low 32 bits of the stable `TypeId` hash.  Collisions are
    /// benign: the daemon may merge two distinct events into one name entry.
    #[inline]
    pub fn of<T: 'static>() -> Self {
        let raw: u64 = type_id_as_u64::<T>();
        Self(raw as u32)
    }

    /// Construct directly from a known u32 literal (for board-embedded names).
    #[inline]
    pub const fn from_raw(v: u32) -> Self {
        Self(v)
    }

    /// Raw inner value for wire records.
    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// InitError
// ---------------------------------------------------------------------------

/// Errors returned from `init`.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum InitError {
    /// `P::observation_ring` returned `None` — no ring for this hart.
    NoRing,
    /// The ring region is too small to hold even the `TxTraceHartRing` header.
    RegionTooSmall,
    /// `ring.size` is not a power of two.
    SizeNotPowerOfTwo,
    /// `ring.size` cannot accommodate any records after the header.
    NoSlots,
    /// The hart index exceeds `MAX_HARTS`.
    HartIndexTooLarge,
}

// ---------------------------------------------------------------------------
// Per-hart storage
// ---------------------------------------------------------------------------

/// Maximum number of harts tracked (matches `CpuMask` width).
const MAX_HARTS: usize = 64;

/// Per-record wire magic: `b"TR"` as little-endian u16 = `0x5254`.
const RECORD_MAGIC: u16 = 0x5254;

/// Per-record format version.
const RECORD_VERSION: u8 = 0;

/// Internal per-hart state.  Stored in `HART_SLOTS`.
struct HartSlot {
    /// Descriptor of this hart's ring region.
    ring_desc: tx_hal::RingDescriptor,
    /// Pointer to the `TxTraceHartRing` header at the start of the region.
    ///
    /// SAFETY invariant: valid for `'static` — the board keeps the region
    /// alive for the kernel's lifetime.
    ring_hdr: *mut TxTraceHartRing,
    /// Pointer to the first record slot immediately after the ring header.
    slots: *mut TxTraceRecord,
    /// Number of slots (power of two).
    slot_count: u64,
    /// Slot index mask (`slot_count - 1`).
    slot_mask: u64,
    /// Hart id stored in bits 56..64 of each `SpanId`.
    hart_id: u8,
    /// Per-hart span-id local counter, **separate** from the wire
    /// `TxTraceHartRing.seq` (which is the per-record sequence number).
    ///
    /// Sharing the counters conflates two independent namespaces: every
    /// non-SpanBegin emit would consume a span-id slot, so the 2^56 span
    /// budget would deplete at the rate of every event rather than every
    /// span begin.  Keeping them separate also clarifies that span-ids are
    /// kernel-private state — the wire format carries the materialised
    /// `SpanId` in each record but never the underlying counter.
    span_counter: core::sync::atomic::AtomicU64,
}

// SAFETY: `HartSlot` is only accessed from its owning hart (SPSC discipline).
// The raw pointers point into a static region.
unsafe impl Send for HartSlot {}
unsafe impl Sync for HartSlot {}

/// Per-hart slot storage.  Each entry is initialised by `init`.
///
/// SAFETY: Before any `HART_SLOTS.get(idx)` call, `init` must have called
/// `HART_SLOTS.init_slot(idx, ...)`.  This is upheld by `init` writing the
/// slot before registering the `HartEmitter`.
static HART_SLOTS: HartLocalArray<HartSlot, MAX_HARTS> =
    // SAFETY: HartSlot contains raw pointers.  Zeroed MaybeUninit is safe
    // because `init` writes every slot before `HartEmitter` is registered,
    // and `get` only runs after registration.
    unsafe {
        // We cannot call HartLocalArray::new with a non-Copy T, so we
        // use a direct const-unsafe construction.
        HartLocalArray::new_zeroed()
    };

// ---------------------------------------------------------------------------
// Emitter storage
// ---------------------------------------------------------------------------

/// Static array of `HartEmitter` instances, one per hart.
/// Populated during `init`.
static EMITTERS: HartLocalOptionArray<HartEmitter, MAX_HARTS> = HartLocalOptionArray::new_none();

// ---------------------------------------------------------------------------
// Public facade: HartEmitter
// ---------------------------------------------------------------------------

/// Per-hart emitter facade.
///
/// Obtained via `current()`.  All emit methods are `&self` — the
/// per-hart guarantee removes the need for any lock.
pub struct HartEmitter {
    /// Index into `HART_SLOTS` and `EMITTERS`.
    slot_idx: usize,
}

impl HartEmitter {
    // -----------------------------------------------------------------------
    // Span begin
    // -----------------------------------------------------------------------

    /// Emit a `SpanBegin` record, mint a fresh `SpanId`, and return it.
    ///
    /// # Precondition (OBS-A-1)
    ///
    /// Must not be called from inside a `StepOp::poll` body.
    pub fn span_begin(
        &self,
        level: TxTraceLevel,
        name: EventNameId,
        parent: SpanId,
        payload_tag: TxPayloadTag,
        payload: &[u8],
    ) -> SpanId {
        let slot = self.slot();
        let span = self.mint_span_id(slot);
        self.emit(
            slot,
            TxTraceKind::SpanBegin,
            level,
            name,
            span,
            parent,
            payload_tag,
            payload,
        );
        span
    }

    // -----------------------------------------------------------------------
    // Span end
    // -----------------------------------------------------------------------

    /// Emit a `SpanEnd` record closing `span`.
    ///
    /// # Precondition (OBS-A-1)
    ///
    /// Must not be called from inside a `StepOp::poll` body.
    pub fn span_end(&self, span: SpanId, payload_tag: TxPayloadTag, payload: &[u8]) {
        let slot = self.slot();
        self.emit(
            slot,
            TxTraceKind::SpanEnd,
            TxTraceLevel::Boundary,
            EventNameId::from_raw(0),
            span,
            SpanId::NONE,
            payload_tag,
            payload,
        );
    }

    // -----------------------------------------------------------------------
    // Instant
    // -----------------------------------------------------------------------

    /// Emit an `Instant` record attached to `parent`.
    ///
    /// # Precondition (OBS-A-1)
    ///
    /// Must not be called from inside a `StepOp::poll` body.
    pub fn instant(
        &self,
        level: TxTraceLevel,
        name: EventNameId,
        parent: SpanId,
        payload_tag: TxPayloadTag,
        payload: &[u8],
    ) {
        let slot = self.slot();
        self.emit(
            slot,
            TxTraceKind::Instant,
            level,
            name,
            SpanId::NONE,
            parent,
            payload_tag,
            payload,
        );
    }

    // -----------------------------------------------------------------------
    // Counter
    // -----------------------------------------------------------------------

    /// Emit a `Counter` record.
    ///
    /// # Precondition (OBS-A-1)
    ///
    /// Must not be called from inside a `StepOp::poll` body.
    pub fn counter(&self, name: EventNameId, value: i64) {
        let cv = PayloadCounterValue {
            counter_id: name.raw(),
            _pad: 0,
            value: value as u64,
        };
        let payload_bytes = unsafe {
            core::slice::from_raw_parts(
                &cv as *const PayloadCounterValue as *const u8,
                core::mem::size_of::<PayloadCounterValue>(),
            )
        };
        let slot = self.slot();
        self.emit(
            slot,
            TxTraceKind::Counter,
            TxTraceLevel::Boundary,
            name,
            SpanId::NONE,
            SpanId::NONE,
            TxPayloadTag::CounterValue,
            payload_bytes,
        );
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    #[inline(always)]
    fn slot(&self) -> &HartSlot {
        HART_SLOTS.get(self.slot_idx)
    }

    /// Mint a fresh span id for this hart.
    ///
    /// Layout: `((hart_id as u64) << 56) | (local_counter & 0x00FF_FFFF_FFFF_FFFF)`.
    ///
    /// The counter lives in `slot.span_counter` — a kernel-private,
    /// hart-local atomic, **separate** from the wire `TxTraceHartRing.seq`
    /// (which is the per-record sequence number).  This separation keeps
    /// the span-id namespace independent of event volume: only `span_begin`
    /// consumes a span id, not every emit.
    ///
    /// In debug builds: asserts the counter has not overflowed into the
    /// hart_id bits (requires 2^56 ≈ 72 quadrillion span begins per hart).
    #[inline]
    fn mint_span_id(&self, slot: &HartSlot) -> SpanId {
        let counter = slot.span_counter.fetch_add(1, Ordering::Relaxed);
        // Span-id local counter is 0-based; bump to 1-based so the value 0
        // remains a unique sentinel (`SpanId::NONE`).  A span begin with
        // counter==0 would otherwise produce the same `SpanId` as NONE for
        // hart 0.
        let counter = counter.wrapping_add(1);
        #[cfg(debug_assertions)]
        {
            assert!(
                counter < (1u64 << 56),
                "tx-observe: span counter overflow on hart {}",
                slot.hart_id
            );
        }
        SpanId(((slot.hart_id as u64) << 56) | (counter & 0x00FF_FFFF_FFFF_FFFF))
    }

    /// Core SPSC producer emit path.  O(1), no allocation, no blocking.
    ///
    /// Memory ordering (spec §9):
    /// 1. Load `consumer` with **Acquire** — pairs with daemon's Release store.
    /// 2. Load `producer` with **Relaxed** — producer-owned, no cross-hart
    ///    visibility needed for the load.
    /// 3. If `producer - consumer >= slot_count` → full ring: increment
    ///    `lost` with **Relaxed** and return (OBS-5).
    /// 4. Write the complete record into `slots[producer & mask]`.
    /// 5. Store `producer + 1` with **Release** — pairs with daemon's
    ///    Acquire load of `producer`.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn emit(
        &self,
        slot: &HartSlot,
        kind: TxTraceKind,
        level: TxTraceLevel,
        name: EventNameId,
        span: SpanId,
        parent: SpanId,
        payload_tag: TxPayloadTag,
        payload: &[u8],
    ) {
        // SAFETY: ring_hdr and slots are valid for 'static and only accessed
        // by this hart (SPSC discipline).
        let ring = unsafe { &*slot.ring_hdr };

        // Step 1: read consumer (Acquire — pairs with daemon's Release store).
        let consumer = ring.consumer.load(Ordering::Acquire);

        // Step 2: read producer (we own it; Relaxed is fine).
        let producer = ring.producer.load(Ordering::Relaxed);

        // Step 3: check space.
        let used = producer.wrapping_sub(consumer);
        if used >= slot.slot_count {
            // OBS-5: lossy on overrun — bump lost counter (Relaxed is fine;
            // the daemon synthesises a LostRecords event from the delta).
            ring.lost.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // Assign a sequence number (separate from the span-id counter; the
        // seq field on TxTraceHartRing doubles as both because we use
        // fetch_add for each purpose independently — this is correct: each
        // call returns a unique monotone value for that hart).
        let seq = ring.seq.fetch_add(1, Ordering::Relaxed);

        // Read the trace timestamp.
        let ts = read_ts();

        // Copy payload into the 16-byte inline buffer.
        let copy_len = payload.len().min(16);
        let mut payload_buf = [0u8; 16];
        payload_buf[..copy_len].copy_from_slice(&payload[..copy_len]);
        let payload_len = copy_len as u16;

        // Step 4: write the record.
        let idx = (producer & slot.slot_mask) as usize;
        // SAFETY: idx < slot_count, slots has slot_count entries.
        let rec = unsafe { &mut *slot.slots.add(idx) };
        *rec = TxTraceRecord {
            magic: RECORD_MAGIC,
            version: RECORD_VERSION,
            kind: kind as u8,
            level: level as u8,
            flags: 0,
            arg_count: 0,
            _pad0: 0,
            hart: slot.hart_id as u16,
            _pad1: 0,
            _pad2: 0,
            seq,
            ts,
            span: span.raw(),
            parent: parent.raw(),
            name: name.raw(),
            payload_tag: payload_tag as u16,
            payload_len,
            payload: payload_buf,
            _pad3: [0u8; 8],
        };

        // Step 5: publish the slot (Release — pairs with daemon's Acquire of
        // `producer`).
        ring.producer
            .store(producer.wrapping_add(1), Ordering::Release);

        // Doorbell: v0 uses polling; no write needed.
        let _ = slot.ring_desc.doorbell;

        // Step 6: tick the global emit counter; the kernel's syscall
        // dispatcher polls `should_dump_now()` after each return and, on a
        // threshold crossing, dumps the ring + halts. Off when the
        // threshold is 0 (the default), so production / smoke runs pay
        // only a Relaxed atomic load per emit.
        check_dump_threshold();
    }
}

// ---------------------------------------------------------------------------
// Timestamp reader
// ---------------------------------------------------------------------------

type TsFn = fn() -> u64;

/// Platform-registered timestamp function stored as a raw function pointer.
/// Written once per boot before the first emit, then only read.
static TS_FN: AtomicU64 = AtomicU64::new(0);

/// Register the platform timestamp function.  Idempotent — safe to call from
/// every hart; the last write wins (all harts write the same value).
#[inline]
fn init_ts(f: TsFn) {
    TS_FN.store(f as usize as u64, Ordering::Relaxed);
}

/// Read the trace clock.  Returns 0 if no platform function is registered yet.
#[inline]
fn read_ts() -> u64 {
    let raw = TS_FN.load(Ordering::Relaxed);
    if raw == 0 {
        return 0;
    }
    // SAFETY: `raw` was written by `init_ts` from a valid `fn` pointer.
    let f: TsFn = unsafe { core::mem::transmute(raw as usize) };
    f()
}

// ---------------------------------------------------------------------------
// CPU-id reader (mirrors TS_FN pattern — Option C, D16)
// ---------------------------------------------------------------------------

type CpuIdFn = fn() -> CpuId;

/// Platform-registered cpu-id function stored as a raw function pointer.
///
/// Written once per boot by `init<P>` (alongside `TS_FN`) before any emit
/// can fire.  After init, every `current()` call reads this with a single
/// `Relaxed` atomic load then makes one non-virtual indirect call —
/// identical cost to `read_ts()`.  Returns `None` from `current()` if not
/// yet installed (same no-op behaviour as an uninstalled TS_FN).
///
/// Install-order invariant: `init<P>` must run before any emit path calls
/// `current()`; the same discipline already applies to `TS_FN`.
static CPU_ID_FN: AtomicU64 = AtomicU64::new(0);

/// Register the platform cpu-id function.  Idempotent — safe to call from
/// every hart; the last write wins (all harts write the same value).
#[inline]
fn init_cpu_id_fn(f: CpuIdFn) {
    CPU_ID_FN.store(f as usize as u64, Ordering::Relaxed);
}

/// Read the current hart's cpu-id.  Returns `None` if no platform function
/// is registered yet (before `init` has run).
#[inline]
fn read_current_cpu_id() -> Option<CpuId> {
    let raw = CPU_ID_FN.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    // SAFETY: `raw` was written by `init_cpu_id_fn` from a valid `fn` pointer.
    let f: CpuIdFn = unsafe { core::mem::transmute(raw as usize) };
    Some(f())
}

// ---------------------------------------------------------------------------
// `current` accessor
// ---------------------------------------------------------------------------

/// Get the current hart's emitter.
///
/// Returns `None` if `CPU_ID_FN` is not yet installed (before `init` runs),
/// if the cpu-id exceeds `MAX_HARTS`, or if `P::observation_ring` returned
/// `None` for this hart at boot time.
///
/// Complexity: one `Relaxed` atomic load + one non-virtual indirect call +
/// one indexed array access — `O(1)`, no locks.  Identical cost to
/// `read_ts()` (D16 Option C).
#[inline]
pub fn current() -> Option<&'static HartEmitter> {
    let cpu_id = read_current_cpu_id()?;
    let idx = cpu_id.0;
    if idx >= MAX_HARTS {
        return None;
    }
    EMITTERS.get(idx).as_ref()
}

// ---------------------------------------------------------------------------
// Per-hart init
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Threshold-based dump trigger
//
// rv64-qemu's static-buffer ring is 4 MiB / 52428 slots. A typical oscomp
// run emits records over many test groups (basic-musl, busybox-musl,
// libctest, cyclictest, LTP…) and init never naturally exits until all
// groups finish — which can take hours. To get a finite trace covering the
// basic-musl group only (~10k records), the kernel watches an "emitted
// records since boot" counter and, once it crosses a configurable
// threshold, sets a flag the syscall dispatcher checks on every return.
// On flag set, the dispatcher dumps the ring over the console and triggers
// an SBI shutdown so the run terminates cleanly with a captured trace.
//
// Threshold of 0 = disabled (the default). Boards/platforms that want
// bounded-dump runs install a non-zero value at boot.
// ---------------------------------------------------------------------------

static EMITTED_COUNT: AtomicU64 = AtomicU64::new(0);
static DUMP_THRESHOLD: AtomicU64 = AtomicU64::new(0);
static DUMP_REQUESTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Configure a "bounded dump" threshold for the current run.
///
/// Once the global emitted-record counter exceeds `n`, the next call to
/// [`should_dump_now`] returns `true` exactly once. The dispatcher (or
/// any other code with `P` in scope) is expected to react by calling
/// [`dump_console_hex`] and triggering shutdown.
///
/// Passing `0` disables the trigger.
#[inline]
pub fn set_dump_threshold(n: u64) {
    DUMP_THRESHOLD.store(n, Ordering::Relaxed);
}

/// Test-and-clear the "dump requested" flag. Returns `true` exactly once
/// per threshold crossing.
#[inline]
pub fn should_dump_now() -> bool {
    DUMP_REQUESTED
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
}

#[inline]
fn check_dump_threshold() {
    let threshold = DUMP_THRESHOLD.load(Ordering::Relaxed);
    if threshold == 0 {
        return;
    }
    let count = EMITTED_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count >= threshold {
        DUMP_REQUESTED.store(true, Ordering::Relaxed);
    }
}

/// Reset the observation ring for the calling hart: clear
/// producer/consumer/seq/lost on the ring header and the per-hart
/// span counter, and zero the global `EMITTED_COUNT`. Useful for
/// debug runs where the workload runs many phases before the
/// interesting one and the ring fills with uninteresting records:
/// call this at the phase boundary (e.g. just before launching the
/// libctest test suite) so the next `OBSERVE_DUMP_THRESHOLD`
/// crossing captures the interesting phase rather than the early
/// noise.
///
/// SPSC discipline: must be called on the hart that owns the ring,
/// while the producer is otherwise quiescent (i.e. not mid-emit).
/// In practice the syscall dispatcher path is the only legitimate
/// caller — between syscalls, the producer is quiescent by
/// construction.
///
/// No-op if the hart has no initialised ring slot.
pub fn reset_ring_and_arm(threshold: u64) {
    // Auto-detect the calling hart via the `PercpuIf`-installed
    // function pointer (same path `check_dump_threshold` /
    // `set_current_parent_span` use). Avoids forcing every caller
    // to take an `SmpIf` bound — debug callers like the
    // exec-marker hook stay generic-free.
    let Some(cpu) = read_current_cpu_id() else {
        return;
    };
    let idx = cpu.0;
    if idx >= MAX_HARTS {
        return;
    }
    // Guard against pre-init by checking the parallel EMITTERS
    // option-array first. `init` writes EMITTERS *after*
    // initialising HART_SLOTS, so `EMITTERS.get(idx).is_some()` is
    // the safe gate that the slot is fully constructed.
    if EMITTERS.get(idx).is_none() {
        return;
    }
    // SAFETY: EMITTERS gate above proves init ran for this hart;
    // per the doc-comment contract the caller is on the owning
    // hart and the producer is quiescent.
    let slot = HART_SLOTS.get(idx);
    let ring = unsafe { &*slot.ring_hdr };
    ring.producer.store(0, Ordering::Relaxed);
    ring.consumer.store(0, Ordering::Relaxed);
    ring.seq.store(0, Ordering::Relaxed);
    ring.lost.store(0, Ordering::Relaxed);
    slot.span_counter.store(0, Ordering::Relaxed);
    EMITTED_COUNT.store(0, Ordering::Relaxed);
    // Re-arm the threshold trigger: clear the `DUMP_REQUESTED`
    // flag so a stale signal from before the reset doesn't fire on
    // the very next emit, then install the configured threshold.
    // Passing `0` keeps the threshold disabled across the reset.
    DUMP_REQUESTED.store(false, Ordering::Relaxed);
    DUMP_THRESHOLD.store(threshold, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Serial-console hex dump (host extraction path)
//
// rv64-qemu (and other boards that back observation with a static buffer
// rather than a live HOST-shared transport) can call this helper at
// shutdown to emit the entire ring region — including a synthesised
// `TxTraceHeader` — over the console as hex bytes framed with
// `TXTRACE-BEGIN`/`TXTRACE-END` sentinels. `cargo xtask observe extract`
// recovers the framed blob from the serial log and writes a standalone
// `.txtrace` file the daemon can decode.
//
// The synthesised header carries the same fields the daemon would expect
// from a live transport: magic, version, hart_count, ring_order,
// boot_id (always 0 here — single-run captures don't need a unique boot
// id), clock_id (RiscvTime), and clock_freq_hz (from `TimeIf`).
//
// This path is `unsafe` only at the FFI boundary (raw ring pointer
// access); the visible surface stays no_std/safe.
// ---------------------------------------------------------------------------

/// Dump hart `hart`'s observation ring as a hex-framed blob over the
/// platform console.
///
/// The output is framed by ASCII sentinels so a host-side extractor
/// can recover the bytes from a serial log:
///
/// ```text
/// TXTRACE-BEGIN bytes=<N> hart=<H> clock_hz=<F>
/// <2*N hex nibbles, no whitespace>
/// TXTRACE-END
/// ```
///
/// `ring` is the raw backing buffer of the per-hart ring (typically
/// obtained from the board's `obs_ring_bytes(hart)`); `hart` is the
/// hart id used to keep cross-hart dumps disambiguated.
///
/// The 72-byte `TxTraceHeader` is emitted first (synthesised here from
/// `P`'s `clock_shared`/`TimeIf::clock_freq_hz` and the buffer size),
/// followed by the ring bytes. The combined hex stream is exactly the
/// shape `cargo xtask observe validate / replay / pftrace` expect for
/// a normal `.txtrace` file — so post-extraction the file is a regular
/// member of the observation pipeline.
pub fn dump_console_hex<P>(hart: CpuId)
where
    P: ConsoleIf + ObserverIf + TimeIf,
{
    let Some(desc) = P::observation_ring(hart) else {
        // No ring backing on this board → nothing to dump.
        return;
    };
    // SAFETY: `desc` came from `P::observation_ring(hart)`. The board's
    // contract is that the region is valid for the kernel's lifetime
    // and exclusively owned by hart `hart` on the producer side. By the
    // time the dump is invoked, the producer has quiesced (init has
    // zombified), so a read of the full region is sound.
    let ring_full: &[u8] = unsafe { core::slice::from_raw_parts(desc.base.as_ptr(), desc.size) };
    // For partial-ring dumps (e.g. when triggered by the bounded-trace
    // threshold), reading the producer cursor lets us emit only the
    // populated slots instead of the entire ring. The 4 MiB rv64
    // backing produces an ~8 MiB hex stream that takes ~25 minutes
    // at 115 200 baud; trimming to the filled portion cuts that to
    // seconds for early-phase dumps. The records past `producer` are
    // either zeros or stale pre-reset data — neither is wanted in
    // the trace.
    let ring: &[u8] = {
        const TX_TRACE_HART_RING_BYTES: usize = 208;
        const TX_TRACE_RECORD_BYTES: usize = 80;
        if ring_full.len() < TX_TRACE_HART_RING_BYTES {
            ring_full
        } else {
            // SAFETY: header layout matches `TxTraceHartRing`. The
            // first AtomicU64 field is `producer` at offset 0 of the
            // header.
            let ring_hdr_ptr = desc.base.as_ptr() as *const TxTraceHartRing;
            let producer = unsafe { (*ring_hdr_ptr).producer.load(Ordering::Acquire) };
            let slot_capacity_max =
                (ring_full.len() - TX_TRACE_HART_RING_BYTES) / TX_TRACE_RECORD_BYTES;
            // prev power of two
            let slot_capacity = if slot_capacity_max == 0 {
                0
            } else {
                1usize << (usize::BITS - 1 - slot_capacity_max.leading_zeros())
            };
            let used_slots = if (producer as usize) >= slot_capacity {
                slot_capacity
            } else {
                producer as usize
            };
            let used_bytes = TX_TRACE_HART_RING_BYTES + used_slots * TX_TRACE_RECORD_BYTES;
            &ring_full[..used_bytes.min(ring_full.len())]
        }
    };
    use tx_observe_types::{TxTraceClockId, TxTraceHeader, TxTraceHeaderFlags, TX_TRACE_MAGIC};

    // `ring_order` is `log2(slot_count)`. The ring slot count is
    // `(ring.len() - 208) / 80` where 208 is `size_of::<TxTraceHartRing>()`
    // and 80 is `size_of::<TxTraceRecord>()`. For the rv64-qemu 64 KiB
    // backing, `(65536 - 208) / 80 = 816` slots — round down to the
    // nearest power of two = 512 = 2^9.
    const TX_TRACE_HART_RING_BYTES: usize = 208;
    const TX_TRACE_RECORD_BYTES: usize = 80;
    let usable = ring.len().saturating_sub(TX_TRACE_HART_RING_BYTES);
    let slot_count_max = usable / TX_TRACE_RECORD_BYTES;
    // Largest power-of-two ≤ slot_count_max:
    let ring_order = if slot_count_max == 0 {
        0u8
    } else {
        (usize::BITS - 1 - slot_count_max.leading_zeros()) as u8
    };

    let header = TxTraceHeader {
        magic: TX_TRACE_MAGIC,
        version: 0,
        header_len: core::mem::size_of::<TxTraceHeader>() as u16,
        endian: 1,
        ptr_width: 8,
        record_size: TX_TRACE_RECORD_BYTES as u16,
        hart_count: 1,
        ring_order,
        flags: if P::clock_shared() {
            TxTraceHeaderFlags::CLOCK_SHARED.0
        } else {
            0
        },
        _pad0: 0,
        boot_id: 0,
        // `init_ts(P::read_ns)` (see `init::<P>`) installs the platform's
        // nanosecond-resolution monotonic clock as the `TS_FN` source,
        // so the `ts` field on every record is already in absolute
        // nanoseconds. The header advertises `clock_id = HostNanos` +
        // `clock_freq_hz = 1 GHz` so the daemon's unit-multiplier
        // calculation (`1e9 / clock_freq_hz`) yields `1 ns/tick` and the
        // wire ts values pass through unscaled.  Reporting the raw
        // hardware `frequency_hz` here (e.g. 10 MHz for QEMU virt) would
        // cause a 100× temporal inflation in Perfetto.
        clock_id: TxTraceClockId::HostNanos as u32,
        _pad1: 0,
        clock_freq_hz: 1_000_000_000,
        string_table_off: 0,
        string_table_len: 0,
        rings_off: core::mem::size_of::<TxTraceHeader>() as u64,
    };

    // SAFETY: `TxTraceHeader` is `#[repr(C)]`, `Pod`-marked, no padding
    // beyond the explicit `_padN` fields. Transmuting to a byte slice
    // is a sound read of the static representation.
    let header_bytes: [u8; core::mem::size_of::<TxTraceHeader>()] = unsafe {
        core::mem::transmute::<TxTraceHeader, [u8; core::mem::size_of::<TxTraceHeader>()]>(header)
    };

    // ── Frame begin ───────────────────────────────────────────────────────
    let total_bytes = header_bytes.len() + ring.len();
    tx_hal::console_write_str::<P>("TXTRACE-BEGIN bytes=");
    write_u64_decimal::<P>(total_bytes as u64);
    tx_hal::console_write_str::<P>(" hart=");
    write_u64_decimal::<P>(hart.0 as u64);
    tx_hal::console_write_str::<P>(" clock_hz=");
    write_u64_decimal::<P>(P::frequency_hz());
    tx_hal::console_write_str::<P>("\n");

    // ── Hex bytes ─────────────────────────────────────────────────────────
    // 64-byte chunks per console line keep extraction robust even when the
    // serial driver inserts CR/LF padding.
    const CHUNK: usize = 64;
    let mut emitted = 0;
    let mut emit_bytes = |bytes: &[u8]| {
        for &b in bytes {
            write_hex_byte::<P>(b);
            emitted += 1;
            if emitted % CHUNK == 0 {
                tx_hal::console_write_str::<P>("\n");
            }
        }
    };
    emit_bytes(&header_bytes);
    emit_bytes(ring);
    if emitted % CHUNK != 0 {
        tx_hal::console_write_str::<P>("\n");
    }

    // ── Frame end ─────────────────────────────────────────────────────────
    tx_hal::console_write_str::<P>("TXTRACE-END\n");
}

#[inline]
fn write_hex_byte<P: ConsoleIf>(b: u8) {
    const NIBBLE: [u8; 16] = *b"0123456789abcdef";
    let buf = [NIBBLE[(b >> 4) as usize], NIBBLE[(b & 0x0f) as usize]];
    // Two-byte slice; ASCII, so utf-8 is trivially valid.
    let s = unsafe { core::str::from_utf8_unchecked(&buf) };
    tx_hal::console_write_str::<P>(s);
}

#[inline]
fn write_u64_decimal<P: ConsoleIf>(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut idx = buf.len();
    if v == 0 {
        idx -= 1;
        buf[idx] = b'0';
    } else {
        while v > 0 {
            idx -= 1;
            buf[idx] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    let s = unsafe { core::str::from_utf8_unchecked(&buf[idx..]) };
    tx_hal::console_write_str::<P>(s);
}

/// One-time per-hart initialisation.
///
/// Called from board `start_secondary` / `start_primary` during per-hart boot.
///
/// Queries `P::observation_ring(hart)`, validates the ring region, writes the
/// `TxTraceHartRing` header, and registers the `HartEmitter`.
///
/// Returns `Err(InitError::NoRing)` when the board has no transport — not a
/// fatal error; emit becomes a no-op via `current` returning `None`.
pub fn init<P: ObserverIf + TimeIf + PercpuIf>(hart: CpuId) -> Result<(), InitError> {
    let idx = hart.0;
    if idx >= MAX_HARTS {
        return Err(InitError::HartIndexTooLarge);
    }

    // Register the platform timestamp function (harmless to call again).
    init_ts(P::read_ns);
    // Register the platform cpu-id function alongside TS_FN so that
    // `current()` is available even on boards with no observation ring
    // (the lossy fallback path still needs the cpu-id to attempt slot lookup).
    init_cpu_id_fn(P::current_cpu_id);

    let desc = match P::observation_ring(hart) {
        Some(d) => d,
        None => return Err(InitError::NoRing),
    };

    let hdr_size = core::mem::size_of::<TxTraceHartRing>();
    let rec_size = core::mem::size_of::<TxTraceRecord>();

    if desc.size < hdr_size {
        return Err(InitError::RegionTooSmall);
    }
    if !desc.size.is_power_of_two() {
        return Err(InitError::SizeNotPowerOfTwo);
    }

    let data_bytes = desc.size - hdr_size;
    let raw_slots = data_bytes / rec_size;
    if raw_slots == 0 {
        return Err(InitError::NoSlots);
    }
    // Round down to nearest power of two so `& slot_mask` works.
    let slot_count = prev_power_of_two(raw_slots) as u64;
    let slot_mask = slot_count - 1;

    // SAFETY: base is NonNull and the region is valid for 'static.
    let ring_hdr = desc.base.as_ptr() as *mut TxTraceHartRing;
    let slots = unsafe { desc.base.as_ptr().add(hdr_size) as *mut TxTraceRecord };

    // Initialise the ring header in-place using addr_of_mut! on the raw
    // pointer's fields to avoid going through any reference.
    // SAFETY: ring_hdr points into a writable static region; no other code
    // accesses this hart's ring during init (single-hart init sequence).
    unsafe {
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr).hart_id), idx as u16);
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr).flags), 0u16);
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr)._pad0), [0u8; 60]);
        // Initialise AtomicU64 fields via addr_of_mut! — avoids creating
        // any intermediate reference, which would conflict with
        // `invalid_reference_casting`.
        core::ptr::write(
            core::ptr::addr_of_mut!((*ring_hdr).producer),
            AtomicU64::new(0),
        );
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr)._pad1), [0u8; 56]);
        core::ptr::write(
            core::ptr::addr_of_mut!((*ring_hdr).consumer),
            AtomicU64::new(0),
        );
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr)._pad2), [0u8; 56]);
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr).lost), AtomicU64::new(0));
        core::ptr::write(core::ptr::addr_of_mut!((*ring_hdr).seq), AtomicU64::new(0));
        // Zero-fill record slots so unread slots read as Nop (kind = 0).
        core::ptr::write_bytes(slots, 0, slot_count as usize);
    }

    // Store the validated slot (init_slot writes into MaybeUninit so the
    // zeroed storage is not read as a T before we write it).
    HART_SLOTS.init_slot(
        idx,
        HartSlot {
            ring_desc: desc,
            ring_hdr,
            slots,
            slot_count,
            slot_mask,
            hart_id: idx as u8,
            span_counter: AtomicU64::new(0),
        },
    );

    // Register the emitter.
    *EMITTERS.get_mut(idx) = Some(HartEmitter { slot_idx: idx });

    Ok(())
}

// ---------------------------------------------------------------------------
// Testing utilities (used by `crate::testing`)
// ---------------------------------------------------------------------------

/// Reset all per-hart observation state for `hart_idx`.
///
/// Clears the emitter slot, marks the hart slot as invalid, and zeroes the
/// function-pointer atomics (`TS_FN`, `CPU_ID_FN`).  After this call, `init`
/// can be called again for the same hart as if it were first boot.
///
/// # Safety
///
/// Must only be called from a test that holds the `TEST_LOCK` mutex so that
/// no other code is concurrently writing into or reading from the observation
/// statics.  Calling this while an emitter is in use is undefined behaviour.
#[cfg(any(test, feature = "testing"))]
pub(crate) unsafe fn testing_reset(hart_idx: usize) {
    use core::sync::atomic::Ordering;
    if hart_idx < MAX_HARTS {
        // Clear the emitter.
        *EMITTERS.get_mut(hart_idx) = None;
        // Mark the hart slot as cleared so init() can reinitialise it.
    }
    TS_FN.store(0, Ordering::Relaxed);
    CPU_ID_FN.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Largest power of two ≤ `n`.  Panics in debug if `n == 0`.
#[inline]
const fn prev_power_of_two(n: usize) -> usize {
    debug_assert!(n > 0);
    1usize << (usize::BITS - n.leading_zeros() - 1)
}

/// Low 32 bits of `core::any::TypeId::of::<T>()` (via stable hash).
///
/// `TypeId`'s in-memory representation changed to 128 bits in recent Rust
/// versions.  We accommodate both sizes: transmute to `[u8; size_of::<TypeId>()]`
/// and XOR-fold to u32.  This is purely a stable hash — not cryptographic,
/// but collision-benign for the observation use case.
#[inline]
fn type_id_as_u64<T: 'static>() -> u64 {
    use core::hash::{Hash, Hasher};

    // Use a simple FNV-1a hasher that works in no_std.
    struct Fnv1aHasher(u64);
    impl Hasher for Fnv1aHasher {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            const FNV_PRIME: u64 = 0x00000100000001B3;
            const FNV_OFFSET: u64 = 0xcbf29ce484222325;
            let mut h = if self.0 == 0 { FNV_OFFSET } else { self.0 };
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
            self.0 = h;
        }
    }

    let mut hasher = Fnv1aHasher(0);
    core::any::TypeId::of::<T>().hash(&mut hasher);
    hasher.finish()
}
