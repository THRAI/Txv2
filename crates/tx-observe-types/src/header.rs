//! `TxTraceHeader`, `TxTraceHartRing`, `TxTraceHeaderFlags`, `TxTraceClockId`.
//!
//! Layout spec: `08_OBSERVATION_SERIALIZATION_v0.md` §3–4.

use core::sync::atomic::AtomicU64;

// ---------------------------------------------------------------------------
// Global region header
// ---------------------------------------------------------------------------

/// Magic bytes "TXTR" as little-endian u32.
pub const TX_TRACE_MAGIC: u32 = 0x5254_5854;

/// Global header written at the start of the trace region (ivshmem BAR or
/// reserved DRAM buffer).  Self-describing: the host daemon reads this to
/// discover the ring count, clock kind, and region layout.
///
/// `sizeof::<TxTraceHeader>() == 72` — enforced by compile-time assertion in
/// `lib.rs`.
///
/// All multi-byte fields are native-endian (kernel and daemon share endianness
/// in the ivshmem-on-QEMU deployment; the `endian` field lets the daemon
/// detect mismatches).
#[repr(C)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct TxTraceHeader {
    /// `b"TXTR"` as little-endian u32 = `0x52545854`.
    pub magic: u32,

    /// txtrace header format version.  v0.
    pub version: u16,

    /// Size of this header in bytes (`== sizeof::<TxTraceHeader>()`).
    pub header_len: u16,

    /// `1` = little endian.  Other values reserved.
    pub endian: u8,

    /// Pointer width in bytes (8 on rv64/la64).
    pub ptr_width: u8,

    /// Size of one fixed record.  v0 = 80 bytes.
    pub record_size: u16,

    /// Number of harts (== number of `TxTraceHartRing` entries that follow).
    pub hart_count: u16,

    /// Each ring has `1 << ring_order` slots.  Power of two required.
    pub ring_order: u8,

    /// Flags (see [`TxTraceHeaderFlags`]).
    pub flags: u8,

    /// Explicit padding to align `boot_id` on 8 bytes.
    pub _pad0: u32,

    /// Random or monotone boot identifier.
    pub boot_id: u64,

    /// Kernel trace clock id (see [`TxTraceClockId`]).
    pub clock_id: u32,

    /// Explicit padding to align `clock_freq_hz` on 8 bytes.
    pub _pad1: u32,

    /// Trace clock frequency (Hz), if known.  0 = unknown.
    pub clock_freq_hz: u64,

    /// Byte offset from region base to optional kernel-embedded string table.
    /// 0 = absent; daemon falls back to out-of-band `names.json` keyed by
    /// `boot_id`.
    pub string_table_off: u64,

    /// Byte length of the kernel-embedded string table.  0 if absent.
    pub string_table_len: u64,

    /// Byte offset from region base to the first [`TxTraceHartRing`].
    pub rings_off: u64,
}

// ---------------------------------------------------------------------------
// Header flags
// ---------------------------------------------------------------------------

/// Bit flags for [`TxTraceHeader::flags`].
#[repr(transparent)]
#[derive(Copy, Clone)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub struct TxTraceHeaderFlags(pub u8);

impl TxTraceHeaderFlags {
    /// Trace clock is shared across all harts (cross-hart timestamps are
    /// trustworthy without calibration).  Set when
    /// `ObserverIf::clock_shared()` returns `true` (e.g. QEMU `time` CSR).
    pub const CLOCK_SHARED: Self = Self(1 << 0);
}

// ---------------------------------------------------------------------------
// Clock identifier
// ---------------------------------------------------------------------------

/// Identifies the hardware/software clock that stamps every trace record.
#[repr(u32)]
#[derive(Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "host", derive(Debug, serde::Serialize, serde::Deserialize))]
pub enum TxTraceClockId {
    Unknown = 0,
    RiscvTime = 1, // RV64 `time` CSR
    ArmCntvct = 2, // AArch64 `cntvct_el0`
    X86TscInv = 3, // x86_64 invariant TSC
    HostNanos = 4, // hosted/test: std::time monotonic ns
}

// ---------------------------------------------------------------------------
// Per-hart SPSC ring header
// ---------------------------------------------------------------------------

/// Per-hart SPSC ring header.  Sits at the start of each hart's ring slice
/// inside the trace region.  Followed immediately by `1 << ring_order`
/// [`TxTraceRecord`](crate::TxTraceRecord) slots.
///
/// `producer` and `consumer` each occupy their own cache line (64 bytes
/// assumed; `_pad0`/`_pad1`/`_pad2` enforce this) so the producer and
/// consumer do not false-share.
///
/// This type is **not** `Pod`: it contains `AtomicU64` fields that require
/// specific memory-ordering discipline.  Both kernel and daemon access it via
/// raw pointers with the protocol documented in
/// `08_OBSERVATION_SERIALIZATION_v0.md` §4.
///
/// Layout (offsets are fixed; changing them is a wire-format break):
/// ```text
/// offset   0 : hart_id : u16
/// offset   2 : flags   : u16
/// offset   4 : _pad0   : [u8; 60]   <- fills to offset 64
/// offset  64 : producer: AtomicU64
/// offset  72 : _pad1   : [u8; 56]   <- fills to offset 128
/// offset 128 : consumer: AtomicU64
/// offset 136 : _pad2   : [u8; 56]   <- fills to offset 192
/// offset 192 : lost    : AtomicU64
/// offset 200 : seq     : AtomicU64
/// ```
#[repr(C)]
pub struct TxTraceHartRing {
    pub hart_id: u16,
    pub flags: u16,

    /// Explicit padding to align `producer` on its own cache line (offset 64).
    pub _pad0: [u8; 60],

    /// Producer-owned monotone slot index.
    /// Written by the kernel on this hart with `Release` ordering.
    pub producer: AtomicU64,

    /// Pads `producer` to its own 64-byte cache line.
    pub _pad1: [u8; 56],

    /// Consumer-owned monotone drained index.
    /// Written by the host daemon with `Release` ordering.
    pub consumer: AtomicU64,

    /// Pads `consumer` to its own 64-byte cache line.
    pub _pad2: [u8; 56],

    /// Dropped-record counter, written by the producer on overflow.
    pub lost: AtomicU64,

    /// Per-hart monotone sequence number used to stamp records.
    pub seq: AtomicU64,
    // Immediately followed in the region by [TxTraceRecord; 1 << ring_order].
}
