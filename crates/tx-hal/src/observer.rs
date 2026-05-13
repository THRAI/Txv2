// crates/tx-hal/src/observer.rs
//
// HAL contract for the observation subsystem (OBS-0).
//
// Spec refs:
//   txdoc:OBS-V1-HAL-1   — trait surface + RingDescriptor definition
//   txdoc:OBS-V1-HAL-SUPERTRAIT — added to TxPlatform aggregate
//
// Only the trait and the backing descriptor live here.  Wire-format types
// (`TxTraceRecord`, `TxTraceHeader`, etc.) belong to the `tx-observe-types`
// crate (OBS-1).  Emitter code belongs to `tx-observe` (OBS-2).

use core::ptr::NonNull;

use super::CpuId;

/// Per-hart trace ring descriptor handed to substrate at boot.
///
/// `base` and `size` describe a contiguous region the kernel may write
/// into using release-store semantics.  `doorbell`, when present, is a
/// device-specific notification register (ivshmem MSI base, etc.).
///
/// Lifetime: the region must remain valid for the kernel's runtime.
/// Boards backing the region with memory-backed files for crash recovery
/// keep the file mapped for the kernel's lifetime.
#[derive(Copy, Clone)]
pub struct RingDescriptor {
    pub base: NonNull<u8>,
    pub size: usize, // bytes; must be power of two
    pub doorbell: Option<NonNull<u32>>,
}

// SAFETY: RingDescriptor is sent across thread boundaries during boot
// init; the underlying region is shared but writes are coordinated by
// the per-hart SPSC discipline in tx-observe.
unsafe impl Send for RingDescriptor {}
unsafe impl Sync for RingDescriptor {}

/// HAL contract for per-hart observation backing.
///
/// Boards that provide a trace transport implement this trait; boards
/// without one rely on the default implementations, which disable
/// observation at runtime (returning `None` from `observation_ring`).
///
/// This is a supertrait of [`TxPlatform`](super::TxPlatform).  All
/// existing boards inherit the default-`None` impl; no board requires
/// source changes for OBS-0 to compile.
///
/// `txdoc:OBS-V1-HAL-1`
pub trait ObserverIf {
    /// Called once during per-hart init.  Returns this hart's ring slab
    /// or `None` if the board has no transport.  `None` ⇒ kernel-side
    /// emit becomes a runtime no-op even when compile-time gates are on.
    fn observation_ring(_hart: CpuId) -> Option<RingDescriptor> {
        None
    }

    /// Optional flush hint after a batch of emits.  Default no-op.
    /// ivshmem: nothing (host polls).  JTAG-TPIU: flush.  Hosted-file: fsync.
    fn observation_flush(_hart: CpuId) {}

    /// True if the platform's trace clock is shared across harts
    /// (so cross-hart ordering is trustworthy without calibration).
    /// QEMU `time` CSR → true.  Real RV64 silicon with per-hart timers → false.
    fn clock_shared() -> bool {
        false
    }
}
