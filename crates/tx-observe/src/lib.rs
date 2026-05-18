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

use tx_hal::{CpuId, ObserverIf, PercpuIf, TimeIf};
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
    let idx = read_current_cpu_id().map(|c| c.0 as usize).unwrap_or(MAX_HARTS);
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
    let idx = read_current_cpu_id().map(|c| c.0 as usize).unwrap_or(MAX_HARTS);
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
