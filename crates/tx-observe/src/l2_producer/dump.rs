//! L2 diagnostic dump and ring-reset helpers.
//!
//! These paths are outside the normal producer hot path. They provide bounded
//! trace dump triggers, guest-controlled ring reset, and serial-console hex
//! extraction for boards without a live shared-memory drain.

use core::sync::atomic::{AtomicU64, Ordering};

use super::runtime::{read_current_cpu_id, EMITTERS, HART_SLOTS, MAX_HARTS, OBSERVE_ENABLED};
use tx_hal::{ConsoleIf, CpuId, MonotonicCounterIf, ObserverIf, PercpuIf, PowerIf};
use tx_observe_types::{TxTraceHartRing, TxTraceRecord};
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
static DUMP_SHUTDOWN_FN: AtomicU64 = AtomicU64::new(0);
static TRACE_OFF_REQUESTS_DUMP: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

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

/// Request one trace dump at the next dispatcher/kernel boundary.
#[inline]
pub fn request_dump() {
    DUMP_REQUESTED.store(true, Ordering::Release);
}

/// Clear a pending trace dump request.
///
/// Live host-drain workflows use the same guest bracketing syscalls as the
/// serial dump path, but they drain the shared rings out-of-band and must not
/// let trace-off request a serial dump/shutdown.
#[inline]
pub fn clear_dump_request() {
    DUMP_REQUESTED.store(false, Ordering::Release);
}

/// Configure whether a private trace-off syscall should request a serial dump.
///
/// Serial-bracket traces keep the default `true`. Live shared-memory drains set
/// this to `false` so the bracket only gates producer emission and the host
/// owns final artifact generation.
#[inline]
pub fn set_trace_off_requests_dump(enabled: bool) {
    TRACE_OFF_REQUESTS_DUMP.store(enabled, Ordering::Relaxed);
}

/// Return whether trace-off should request a serial dump.
#[inline]
pub fn trace_off_requests_dump() -> bool {
    TRACE_OFF_REQUESTS_DUMP.load(Ordering::Relaxed)
}

type DumpShutdownFn = fn() -> !;
type PreDumpHookFn = fn();

static PRE_DUMP_HOOK_FN: AtomicU64 = AtomicU64::new(0);

#[cfg(any(test, feature = "testing"))]
pub(super) fn testing_reset_dump_state() {
    PRE_DUMP_HOOK_FN.store(0, Ordering::Relaxed);
}

/// Register a platform-specific diagnostic hook to run immediately before a
/// console trace dump. The hook is outside the ring, so it can print aggregate
/// counters that must survive a full/lossy observation buffer.
pub fn register_pre_dump_hook(f: PreDumpHookFn) {
    PRE_DUMP_HOOK_FN.store(f as *const () as usize as u64, Ordering::Relaxed);
}

fn run_pre_dump_hook() {
    let raw = PRE_DUMP_HOOK_FN.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: `raw` was written by `register_pre_dump_hook` from a valid
    // monomorphised function pointer.
    let f: PreDumpHookFn = unsafe { core::mem::transmute(raw as usize) };
    f();
}

fn dump_shutdown_for<P>() -> !
where
    P: ConsoleIf + ObserverIf + MonotonicCounterIf + PercpuIf + PowerIf + tx_hal::SmpIf,
{
    dump_console_hex_all::<P>();
    tx_hal::console_write_str::<P>(":observe:dump:threshold\n");
    <P as PowerIf>::system_off();
}

/// Register a platform-specific threshold dump/shutdown hook.
///
/// This lets low-level diagnostic probes that do not carry `P` still
/// force the same bounded trace dump once `check_dump_threshold` has
/// requested it.
pub fn register_dump_shutdown<P>()
where
    P: ConsoleIf + ObserverIf + MonotonicCounterIf + PercpuIf + PowerIf + tx_hal::SmpIf,
{
    DUMP_SHUTDOWN_FN.store(
        dump_shutdown_for::<P> as *const () as usize as u64,
        Ordering::Relaxed,
    );
}

/// If the global threshold requested a dump, run the registered platform hook.
///
/// No-op when no hook has been registered. This function may diverge.
pub fn dump_registered_if_requested() {
    if !should_dump_now() {
        return;
    }
    let raw = DUMP_SHUTDOWN_FN.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: `raw` was written by `register_dump_shutdown` from a valid
    // monomorphised function pointer.
    let f: DumpShutdownFn = unsafe { core::mem::transmute(raw as usize) };
    f();
}

#[inline]
pub(super) fn check_dump_threshold() {
    let threshold = DUMP_THRESHOLD.load(Ordering::Relaxed);
    if threshold == 0 {
        return;
    }
    let count = EMITTED_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count >= threshold {
        DUMP_REQUESTED.store(true, Ordering::Relaxed);
    }
}

fn compact_ring_order(record_count: usize) -> u8 {
    let mut order = 2u8;
    let mut slots = 1usize << order;
    while slots < record_count && order < 24 {
        order += 1;
        slots <<= 1;
    }
    order
}

#[cfg(any(test, feature = "testing"))]
pub fn testing_compact_ring_order(record_count: usize) -> u8 {
    compact_ring_order(record_count)
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

/// Reset every initialized hart ring and arm the global dump threshold.
///
/// This is the SMP counterpart to [`reset_ring_and_arm`]. Diagnostic guest
/// hooks use it before a bracketed trace so host replay sees one coherent
/// multi-hart window instead of stale records on remote harts.
pub fn reset_all_rings_and_arm(threshold: u64) {
    OBSERVE_ENABLED.store(false, Ordering::Relaxed);
    for idx in 0..MAX_HARTS {
        if EMITTERS.get(idx).is_none() {
            continue;
        }
        let slot = HART_SLOTS.get(idx);
        // SAFETY: the emitter gate proves init completed for this hart. The
        // global enabled flag has been lowered before clearing cursors, so new
        // emitters stop at `current()`.
        let ring = unsafe { &*slot.ring_hdr };
        ring.producer.store(0, Ordering::Relaxed);
        ring.consumer.store(0, Ordering::Relaxed);
        ring.seq.store(0, Ordering::Relaxed);
        ring.lost.store(0, Ordering::Relaxed);
        slot.span_counter.store(0, Ordering::Relaxed);
    }
    EMITTED_COUNT.store(0, Ordering::Relaxed);
    DUMP_REQUESTED.store(false, Ordering::Relaxed);
    DUMP_THRESHOLD.store(threshold, Ordering::Relaxed);
    OBSERVE_ENABLED.store(true, Ordering::Relaxed);
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
// id), clock_id (RiscvTime), and clock_freq_hz (from `MonotonicCounterIf`).
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
/// `P`'s `clock_shared`/`MonotonicCounterIf::frequency_hz` and the buffer size),
/// followed by the ring bytes. The combined hex stream is exactly the
/// shape `cargo xtask observe validate / replay / pftrace` expect for
/// a normal `.txtrace` file — so post-extraction the file is a regular
/// member of the observation pipeline.
pub fn dump_console_hex<P>(hart: CpuId)
where
    P: ConsoleIf + ObserverIf + MonotonicCounterIf,
{
    dump_console_hex_mask::<P>(tx_hal::CpuMask::single(hart), Some(hart.0 as u64));
}

/// Dump all platform-possible hart rings as one compact txtrace frame.
pub fn dump_console_hex_all<P>()
where
    P: ConsoleIf + ObserverIf + MonotonicCounterIf + tx_hal::SmpIf,
{
    dump_console_hex_mask::<P>(P::possible_cpus(), None);
}

const TX_TRACE_HART_RING_BYTES: usize = core::mem::size_of::<TxTraceHartRing>();
const TX_TRACE_RECORD_BYTES: usize = core::mem::size_of::<TxTraceRecord>();

#[derive(Copy, Clone)]
struct RingSnapshot {
    desc: tx_hal::RingDescriptor,
    actual_slots: usize,
    start: u64,
    count: usize,
    lost: u64,
}

fn ring_snapshot(desc: tx_hal::RingDescriptor) -> Option<RingSnapshot> {
    if desc.size < TX_TRACE_HART_RING_BYTES + TX_TRACE_RECORD_BYTES {
        return None;
    }
    let raw_slots = (desc.size - TX_TRACE_HART_RING_BYTES) / TX_TRACE_RECORD_BYTES;
    if raw_slots == 0 {
        return None;
    }
    let actual_slots = 1usize << (usize::BITS - 1 - raw_slots.leading_zeros());
    // SAFETY: `desc` came from `ObserverIf`; the region begins with a
    // `TxTraceHartRing` header for this hart.
    let hdr = unsafe { &*(desc.base.as_ptr() as *const TxTraceHartRing) };
    let producer = hdr.producer.load(Ordering::Acquire);
    let consumer = hdr.consumer.load(Ordering::Acquire);
    let available = producer.wrapping_sub(consumer);
    let count = if available > actual_slots as u64 {
        actual_slots
    } else {
        available as usize
    };
    Some(RingSnapshot {
        desc,
        actual_slots,
        start: producer.wrapping_sub(count as u64),
        count,
        lost: hdr.lost.load(Ordering::Acquire),
    })
}

fn dump_console_hex_mask<P>(mask: tx_hal::CpuMask, hart_label: Option<u64>)
where
    P: ConsoleIf + ObserverIf + MonotonicCounterIf,
{
    run_pre_dump_hook();
    use tx_observe_types::{TxTraceClockId, TxTraceHeader, TxTraceHeaderFlags, TX_TRACE_MAGIC};

    let mut hart_count = 0usize;
    let mut max_records = 0usize;
    for idx in 0..MAX_HARTS {
        let hart = CpuId(idx);
        if !mask.contains(hart) {
            continue;
        }
        if let Some(snapshot) = P::observation_ring(hart).and_then(ring_snapshot) {
            hart_count += 1;
            max_records = max_records.max(snapshot.count);
        }
    }
    if hart_count == 0 {
        return;
    }

    let ring_order = compact_ring_order(max_records);
    let slot_count = 1usize << ring_order;
    let ring_bytes = TX_TRACE_HART_RING_BYTES + slot_count * TX_TRACE_RECORD_BYTES;

    let header = TxTraceHeader {
        magic: TX_TRACE_MAGIC,
        version: 0,
        header_len: core::mem::size_of::<TxTraceHeader>() as u16,
        endian: 1,
        ptr_width: 8,
        record_size: TX_TRACE_RECORD_BYTES as u16,
        hart_count: hart_count as u16,
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
    let total_bytes = header_bytes.len() + hart_count * ring_bytes;
    tx_hal::console_write_str::<P>("TXTRACE-BEGIN bytes=");
    write_u64_decimal::<P>(total_bytes as u64);
    tx_hal::console_write_str::<P>(" hart=");
    if let Some(hart) = hart_label {
        write_u64_decimal::<P>(hart);
    } else {
        tx_hal::console_write_str::<P>("all");
    }
    tx_hal::console_write_str::<P>(" clock_hz=");
    write_u64_decimal::<P>(P::frequency_hz());
    tx_hal::console_write_str::<P>("\n");

    // ── Hex bytes ─────────────────────────────────────────────────────────
    // 64-byte chunks per console line keep extraction robust even when the
    // serial driver inserts CR/LF padding.
    let mut emitted = 0;
    emit_hex_bytes::<P>(&header_bytes, &mut emitted);
    for idx in 0..MAX_HARTS {
        let hart = CpuId(idx);
        if !mask.contains(hart) {
            continue;
        }
        let Some(snapshot) = P::observation_ring(hart).and_then(ring_snapshot) else {
            continue;
        };
        emit_ring_snapshot::<P>(hart, snapshot, slot_count, &mut emitted);
    }
    if emitted % 64 != 0 {
        tx_hal::console_write_str::<P>("\n");
    }

    // ── Frame end ─────────────────────────────────────────────────────────
    tx_hal::console_write_str::<P>("TXTRACE-END\n");
}

fn emit_ring_snapshot<P: ConsoleIf>(
    hart: CpuId,
    snapshot: RingSnapshot,
    slot_count: usize,
    emitted: &mut usize,
) {
    let count = snapshot.count.min(slot_count);
    let ring_hdr = TxTraceHartRing {
        hart_id: hart.0 as u16,
        flags: 0,
        _pad0: [0; 60],
        producer: AtomicU64::new(count as u64),
        _pad1: [0; 56],
        consumer: AtomicU64::new(0),
        _pad2: [0; 56],
        lost: AtomicU64::new(snapshot.lost),
        seq: AtomicU64::new(count as u64),
    };
    let header_bytes = unsafe {
        core::slice::from_raw_parts(
            &ring_hdr as *const TxTraceHartRing as *const u8,
            TX_TRACE_HART_RING_BYTES,
        )
    };
    emit_hex_bytes::<P>(header_bytes, emitted);

    let slots = unsafe {
        snapshot.desc.base.as_ptr().add(TX_TRACE_HART_RING_BYTES) as *const TxTraceRecord
    };
    for offset in 0..count {
        let cursor = snapshot.start.wrapping_add(offset as u64);
        let slot_idx = (cursor & (snapshot.actual_slots as u64 - 1)) as usize;
        let record = unsafe { slots.add(slot_idx) as *const u8 };
        let bytes = unsafe { core::slice::from_raw_parts(record, TX_TRACE_RECORD_BYTES) };
        emit_hex_bytes::<P>(bytes, emitted);
    }
    emit_zero_bytes::<P>((slot_count - count) * TX_TRACE_RECORD_BYTES, emitted);
}

fn emit_hex_bytes<P: ConsoleIf>(bytes: &[u8], emitted: &mut usize) {
    const CHUNK: usize = 64;
    for &b in bytes {
        write_hex_byte::<P>(b);
        *emitted += 1;
        if *emitted % CHUNK == 0 {
            tx_hal::console_write_str::<P>("\n");
        }
    }
}

fn emit_zero_bytes<P: ConsoleIf>(count: usize, emitted: &mut usize) {
    const CHUNK: usize = 64;
    for _ in 0..count {
        write_hex_byte::<P>(0);
        *emitted += 1;
        if *emitted % CHUNK == 0 {
            tx_hal::console_write_str::<P>("\n");
        }
    }
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
