//! devfs — read-only static device namespace.
//!
//! devfs is a tier-2 projection-shaped filesystem in the same family as
//! procfs and devpts: it has no on-disk state and no per-mount allocator.
//! Every entry resolves through a registry rather than an in-memory
//! BTreeMap, so all mutating operations reject with `EROFS` (per
//! `txdoc:TTY-LOOKUP-1` / `txdoc:TTY-RNODE-MATERIALIZATION-1`, lookups
//! are cheap reads against the live registry; the filesystem owns no
//! durable state to mutate).
//!
//! The key design lever: `RNodeBacking::StructBacked { payload:
//! StructPayload::Tty(Cap<TtyIdentity>) }` already routes through
//! `OpenFile::step_read` / `OpenFile::step_write` to
//! `tty::execution::step_read` / `step_write`. Once devfs's `lookup`
//! returns the right RNode, no further dispatch wiring is needed (per
//! the trio plan §"Part 3 — devfs FsOps surface" and the "RNode
//! materialisation" note that follows).
//!
//! Phase 3a deliverable. Mount wiring lives in Phase 3b
//! (`crates/tx-kernel/src/init.rs`); this module is the standalone
//! backend and the `open_console_for_init` bootstrap helper.
//!
//! Active-doc anchors:
//! - `txdoc:TTY-THE-HARDWARE-CONSOLE-PATH-1` (`docs/design/06_devices/TTY.md` §7)
//! - `txdoc:TTY-LOOKUP-1`, `txdoc:TTY-RNODE-MATERIALIZATION-1`
//!   (`docs/design/06_devices/TTY.md` §6.2 / §6.3) — devpts/devfs
//!   share the registry-projection lookup shape.
//! - `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`
//!   (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md` §mount-boundary)
//!   — devfs is a separate `FsOps` instance, never aliasing the parent
//!   namespace's backend.
//! - `txdoc:MOUNT-MOUNTPAYLOAD-1`
//!   (`docs/design/05_filesystem/MOUNT_v1.md` §MountPayload) — devfs
//!   has to satisfy the `Arc<dyn FsOps>` + `Arc<dyn FsPageBacking>`
//!   shape for Phase 3b's `MountIdentity::new_cap`.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub mod adapter;

use adapter::step_engine::{self as step_engine, ByteProgress, Cap, NoProgress, StepOutcome};
use tx_services::time::{
    DeadlineNs, DeviceTimerCallback, TimeError, TimerGuard, TimerRole, TimerTarget,
};
use tx_substrate::wake::MailboxEvent;
use tx_substrate::wake::TaskMailbox;
use tx_subsystems::device::{
    CharDeviceBinding, CharDeviceOps, DevT, RtcAlarm, RtcAlarmEmulation, RtcDeviceOps,
    RtcEventMask, RtcTime,
};
use tx_subsystems::execution::{Errno, Guard, WaitToken};
use tx_subsystems::mount::MountPayload;
use tx_subsystems::page_backed::{Frame, FsPageBacking};
use tx_subsystems::process;
use tx_subsystems::tty;
use tx_subsystems::vfs::{
    self, Credential, DirCursor, DirEntry, FsObjectId, FsOps, InodeKind, InodeMeta, OpenFile,
    OpenFileFlags, RNode, RNodeBacking, StructPayload, S_IFCHR, S_IFDIR,
};

/// Stable `FsObjectId` for the devfs root directory.
///
/// Picks a non-overlapping namespace from devpts (`0x7074_7300`) and
/// tmpfs (which uses `2..` starting from the reserved `FsObjectId::ROOT
/// = 1` sentinel, per the trio plan §"tmpfs root materialisation").
pub const DEVFS_ROOT_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7600);

/// Base id-space for devfs character-device entries. Each registered
/// alias gets `DEVFS_ENTRY_OBJECT_BASE + slot_index_within_snapshot`,
/// so the value is stable for one snapshot but does not pretend to be
/// stable across re-registrations. devfs has no inode persistence; the
/// `FsObjectId` here is observation-only.
const DEVFS_ENTRY_OBJECT_BASE: u64 = 0x6465_7601;

/// Stable `FsObjectId` for the synthetic `/dev/block` directory.
///
/// bdev-fs (`docs/design/05_filesystem/BDEV_FS.md` §7.1) is mounted on
/// this directory at boot. The directory is a static read-only stub
/// owned by devfs; its sole purpose is to be a mountpoint. Without
/// this entry there is no path for bdev-fs to attach to, because
/// devfs as a whole rejects `mkdir` with `EROFS`.
///
/// Disjoint from `DEVFS_ENTRY_OBJECT_BASE`'s id range
/// (`0x6465_7601..0x6465_77FF`) and the devfs root id.
pub const DEVFS_BLOCK_DIR_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7800);

/// `/dev/block` directory name as the lookup key.
const DEVFS_BLOCK_DIR_NAME: &[u8] = b"block";

/// Mode for the synthetic `/dev/block` mountpoint directory.
pub const DEVFS_BLOCK_DIR_MODE: u16 = S_IFDIR | 0o755;

/// Stable `FsObjectId` for the synthetic `/dev/shm` directory.
pub const DEVFS_SHM_DIR_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7801);

/// `/dev/shm` directory name as the lookup key.
const DEVFS_SHM_DIR_NAME: &[u8] = b"shm";

/// Mode for the synthetic `/dev/shm` mountpoint directory.
pub const DEVFS_SHM_DIR_MODE: u16 = S_IFDIR | 0o777;

/// Stable `FsObjectId` for the synthetic `/dev/null` character device.
pub const DEVFS_NULL_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7802);

/// Stable `FsObjectId` for the static `/dev/zero` character device.
pub const DEVFS_ZERO_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7803);

/// Stable `FsObjectId` for the synthetic `/dev/misc` directory.
pub const DEVFS_MISC_DIR_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7804);

/// Stable `FsObjectId` for the static `/dev/misc/rtc` character device.
pub const DEVFS_RTC_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7805);

/// Stable `FsObjectId` for the static `/dev/urandom` character device.
pub const DEVFS_URANDOM_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7806);

/// Stable `FsObjectId` for the static `/dev/random` character device.
pub const DEVFS_RANDOM_OBJECT_ID: FsObjectId = FsObjectId::new(0x6465_7807);

/// `/dev/null` character device name as the lookup key.
const DEVFS_NULL_NAME: &[u8] = b"null";

/// `/dev/zero` directory name as the lookup key.
const DEVFS_ZERO_NAME: &[u8] = b"zero";

/// `/dev/misc` directory name as the lookup key.
const DEVFS_MISC_DIR_NAME: &[u8] = b"misc";

/// `/dev/misc/rtc` character device name as the lookup key.
const DEVFS_RTC_NAME: &[u8] = b"rtc";

/// `/dev/urandom` character device name as the lookup key.
const DEVFS_URANDOM_NAME: &[u8] = b"urandom";

/// `/dev/random` character device name as the lookup key.
const DEVFS_RANDOM_NAME: &[u8] = b"random";

/// Mode for any character-device alias resolved by devfs (per the
/// Phase 3a plan §"devfs FsOps surface": `S_IFCHR | 0o620`).
pub const DEVFS_CHAR_MODE: u16 = S_IFCHR | 0o620;

/// Mode for `/dev/null`; libc tests expect the conventional world
/// readable/writable null device.
pub const DEVFS_NULL_MODE: u16 = S_IFCHR | 0o666;

/// Mode for the synthetic `/dev/misc` directory.
pub const DEVFS_MISC_DIR_MODE: u16 = S_IFDIR | 0o755;

/// Mode for `/dev/misc/rtc`; BusyBox `hwclock` opens this read-only.
pub const DEVFS_RTC_MODE: u16 = S_IFCHR | 0o644;

/// Mode for the devfs root directory (`S_IFDIR | 0o755`).
pub const DEVFS_ROOT_MODE: u16 = S_IFDIR | 0o755;

struct RandomOps;

static RANDOM_OPS_STATE: AtomicU64 = AtomicU64::new(0x7478_7632_6e65_7430);
static RANDOM_OPS: RandomOps = RandomOps;
static DEVFS_RANDOM_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 8),
    name: "random",
    ops: &RANDOM_OPS,
};
static DEVFS_URANDOM_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 9),
    name: "urandom",
    ops: &RANDOM_OPS,
};

const DEVFS_STATIC_CHAR_ENTRIES: [&CharDeviceBinding; 2] =
    [&DEVFS_RANDOM_BINDING, &DEVFS_URANDOM_BINDING];

impl CharDeviceOps for RandomOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        let mut state = RANDOM_OPS_STATE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        for byte in out.iter_mut() {
            state ^= state << 7;
            state ^= state >> 9;
            state ^= state << 8;
            *byte = state as u8;
        }
        RANDOM_OPS_STATE.store(state | 1, Ordering::Relaxed);
        StepOutcome::done(out.len())
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::done(bytes.len())
    }
}

/// Static read-only devfs backend.
///
/// Holds no state of its own — every observation resolves against the
/// TTY registry through `tty::project`. A single instance is enough for
/// the whole kernel, but the type stays unit-shaped so Phase 3b can
/// build an `Arc<dyn FsOps>` (and an `Arc<dyn FsPageBacking>`)
/// without needing a constructor.
#[derive(Clone, Copy, Debug, Default)]
pub struct Devfs;

/// `DevT` for a static devfs character node, by stable object id.
/// Path-walked `stat`/`statx` only have the `FsObjectId` (no rnode
/// with the `CharDeviceBinding`), so they resolve the device number
/// here. glibc's `daemon()` fstat-checks `/dev/null` against
/// `makedev(1,3)` and fails with `ENODEV` when `st_rdev` is 0.
pub fn devt_for_object_id(id: FsObjectId) -> Option<DevT> {
    if id == DEVFS_NULL_OBJECT_ID {
        Some(NULL_CHAR_BINDING.devt)
    } else if id == DEVFS_ZERO_OBJECT_ID {
        Some(ZERO_CHAR_BINDING.devt)
    } else if id == DEVFS_RTC_OBJECT_ID {
        Some(RTC_CHAR_BINDING.devt)
    } else if id == DEVFS_URANDOM_OBJECT_ID {
        Some(URANDOM_CHAR_BINDING.devt)
    } else if id == DEVFS_RANDOM_OBJECT_ID {
        Some(RANDOM_CHAR_BINDING.devt)
    } else {
        let idx = entry_index_from_object_id(id)?;
        let entries = tty::project::devfs_alias_entries();
        if let Some(entry) = entries.get(idx) {
            let (major, minor) = tty::project::devt_major_minor_for_tty(&entry.tty);
            return Some(DevT::new(major, minor));
        }
        static_char_entry_by_combined_index(idx).map(|binding| binding.devt)
    }
}

struct NullCharOps;

impl CharDeviceOps for NullCharOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::done(bytes.len())
    }
}

static NULL_CHAR_OPS: NullCharOps = NullCharOps;

static NULL_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 3),
    name: "null",
    ops: &NULL_CHAR_OPS,
};

struct ZeroCharOps;

impl CharDeviceOps for ZeroCharOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        out.fill(0);
        StepOutcome::done(out.len())
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::done(bytes.len())
    }
}

static ZERO_CHAR_OPS: ZeroCharOps = ZeroCharOps;

static ZERO_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 5),
    name: "zero",
    ops: &ZERO_CHAR_OPS,
};

type RtcReadTimeFn = fn(&Guard<'_>) -> Result<RtcTime, tx_subsystems::device::RtcError>;
type RtcSetTimeFn = fn(RtcTime, &Guard<'_>) -> Result<(), tx_subsystems::device::RtcError>;
type RtcSetAlarmFn = fn(RtcAlarm, &Guard<'_>) -> Result<(), tx_subsystems::device::RtcError>;
pub type RtcReadTimeNsFn = fn() -> Result<u64, TimeError>;
pub type RtcSetTimeNsFn = fn(u64) -> Result<(), TimeError>;
pub type RtcSetAlarmNsFn = fn(u64) -> Result<(), TimeError>;
pub type RtcClearAlarmFn = fn() -> Result<(), TimeError>;

pub const RTC_EVENT_READABLE: u64 = 0x1;

struct RtcEventQueue {
    queue: tx_substrate::bus::RawQueue,
    source_id: u64,
}

struct RtcEventState {
    queue: tx_substrate::SpinMutex<Option<RtcEventQueue>>,
}

impl RtcEventState {
    const fn new() -> Self {
        Self {
            queue: tx_substrate::SpinMutex::new(None),
        }
    }

    fn clear_readable(&self) {
        if let Some(queue) = self.queue.lock().as_ref() {
            queue.queue.clear(RTC_EVENT_READABLE);
        }
    }

    fn ensure_queue(&self) -> (tx_substrate::bus::RawQueue, u64) {
        let mut slot = self.queue.lock();
        if slot.is_none() {
            let queue = tx_substrate::bus::RawQueue::new();
            let source_id = tx_subsystems::wait_source::register_wait_queue(queue.clone());
            *slot = Some(RtcEventQueue { queue, source_id });
        }
        let queue = slot.as_ref().expect("rtc event queue initialized");
        (queue.queue.clone(), queue.source_id)
    }

    #[cfg(test)]
    fn queue_for_test(&self) -> Option<tx_substrate::bus::RawQueue> {
        self.queue.lock().as_ref().map(|queue| queue.queue.clone())
    }
}

static RTC_READ_TIME_FN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RTC_SET_TIME_FN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RTC_SET_ALARM_FN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static RTC_BACKEND_READ_TIME_NS_FN: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static RTC_BACKEND_SET_TIME_NS_FN: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static RTC_BACKEND_SET_ALARM_NS_FN: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static RTC_BACKEND_CLEAR_ALARM_FN: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static RTC_ALARM_NS: AtomicU64 = AtomicU64::new(0);
static RTC_ALARM_ENABLED: AtomicBool = AtomicBool::new(false);
static RTC_ALARM_PENDING: AtomicBool = AtomicBool::new(false);
static RTC_EVENT_PENDING_BITS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
static RTC_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_EVENT_STATE: RtcEventState = RtcEventState::new();
static RTC_ALARM_TIMER_GUARD: tx_substrate::SpinMutex<Option<TimerGuard>> =
    tx_substrate::SpinMutex::new(None);

pub fn install_rtc_backend(
    read_time_ns: RtcReadTimeNsFn,
    set_time_ns: RtcSetTimeNsFn,
    set_alarm_ns: RtcSetAlarmNsFn,
    clear_alarm: RtcClearAlarmFn,
) {
    RTC_BACKEND_READ_TIME_NS_FN.store(read_time_ns as usize, Ordering::Release);
    RTC_BACKEND_SET_TIME_NS_FN.store(set_time_ns as usize, Ordering::Release);
    RTC_BACKEND_SET_ALARM_NS_FN.store(set_alarm_ns as usize, Ordering::Release);
    RTC_BACKEND_CLEAR_ALARM_FN.store(clear_alarm as usize, Ordering::Release);
    RTC_READ_TIME_FN.store(typed_rtc_read_time as usize, Ordering::Release);
    RTC_SET_TIME_FN.store(typed_rtc_set_time as usize, Ordering::Release);
    RTC_SET_ALARM_FN.store(typed_rtc_set_alarm as usize, Ordering::Release);
}

pub fn reset_rtc_backend_for_test() {
    RTC_READ_TIME_FN.store(0, Ordering::Release);
    RTC_SET_TIME_FN.store(0, Ordering::Release);
    RTC_SET_ALARM_FN.store(0, Ordering::Release);
    RTC_BACKEND_READ_TIME_NS_FN.store(0, Ordering::Release);
    RTC_BACKEND_SET_TIME_NS_FN.store(0, Ordering::Release);
    RTC_BACKEND_SET_ALARM_NS_FN.store(0, Ordering::Release);
    RTC_BACKEND_CLEAR_ALARM_FN.store(0, Ordering::Release);
    RTC_ALARM_NS.store(0, Ordering::Release);
    RTC_ALARM_ENABLED.store(false, Ordering::Release);
    RTC_ALARM_PENDING.store(false, Ordering::Release);
    RTC_EVENT_PENDING_BITS.store(0, Ordering::Release);
    RTC_EVENT_COUNT.store(0, Ordering::Release);
    RTC_ALARM_TIMER_GUARD.lock().take();
    RTC_EVENT_STATE.clear_readable();
}

pub fn rtc_event_wait_token() -> WaitToken {
    WaitToken::new(rtc_event_source_id(), RTC_EVENT_READABLE)
}

fn ensure_rtc_event_queue() -> (tx_substrate::bus::RawQueue, u64) {
    RTC_EVENT_STATE.ensure_queue()
}

pub fn rtc_event_source_id() -> u64 {
    ensure_rtc_event_queue().1
}

#[cfg(test)]
pub(crate) fn rtc_event_queue_for_test() -> Option<tx_substrate::bus::RawQueue> {
    RTC_EVENT_STATE.queue_for_test()
}

fn record_rtc_event(mask: RtcEventMask) {
    RTC_EVENT_PENDING_BITS.fetch_or(mask.bits(), Ordering::AcqRel);
    RTC_EVENT_COUNT.fetch_add(1, Ordering::AcqRel);
    if mask.contains(RtcEventMask::ALARM) {
        RTC_ALARM_PENDING.store(true, Ordering::Release);
    }
}

pub fn publish_rtc_event_with_post<F>(mask: RtcEventMask, post: F)
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    if mask.is_empty() {
        return;
    }
    let mut post = post;
    record_rtc_event(mask);
    let (queue, _) = ensure_rtc_event_queue();
    queue.fire_with_post(RTC_EVENT_READABLE, |mailbox, event| post(mailbox, event));
}

fn typed_rtc_read_time(_guard: &Guard<'_>) -> Result<RtcTime, tx_subsystems::device::RtcError> {
    let ptr = RTC_BACKEND_READ_TIME_NS_FN.load(Ordering::Acquire);
    if ptr == 0 {
        return Err(tx_subsystems::device::RtcError::Unsupported);
    }
    let read_time_ns: RtcReadTimeNsFn = unsafe { core::mem::transmute(ptr) };
    let ns = read_time_ns().map_err(rtc_error_from_time)?;
    RtcTime::from_unix_ns(ns)
}

fn typed_rtc_set_time(
    time: RtcTime,
    _guard: &Guard<'_>,
) -> Result<(), tx_subsystems::device::RtcError> {
    let ptr = RTC_BACKEND_SET_TIME_NS_FN.load(Ordering::Acquire);
    if ptr == 0 {
        return Err(tx_subsystems::device::RtcError::Unsupported);
    }
    let set_time_ns: RtcSetTimeNsFn = unsafe { core::mem::transmute(ptr) };
    let ns = time.to_unix_ns()?;
    set_time_ns(ns).map_err(rtc_error_from_time)
}

fn typed_rtc_set_alarm(
    alarm: RtcAlarm,
    _guard: &Guard<'_>,
) -> Result<(), tx_subsystems::device::RtcError> {
    let ns = alarm.time.to_unix_ns()?;
    if alarm.enabled {
        let ptr = RTC_BACKEND_SET_ALARM_NS_FN.load(Ordering::Acquire);
        if ptr == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let set_alarm_ns: RtcSetAlarmNsFn = unsafe { core::mem::transmute(ptr) };
        set_alarm_ns(ns).map_err(rtc_error_from_time)?;
    } else {
        let ptr = RTC_BACKEND_CLEAR_ALARM_FN.load(Ordering::Acquire);
        if ptr == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let clear_alarm: RtcClearAlarmFn = unsafe { core::mem::transmute(ptr) };
        clear_alarm().map_err(rtc_error_from_time)?;
    }
    RTC_ALARM_NS.store(ns, Ordering::Release);
    RTC_ALARM_ENABLED.store(alarm.enabled, Ordering::Release);
    RTC_ALARM_PENDING.store(alarm.pending, Ordering::Release);
    Ok(())
}

fn rtc_error_from_time(error: TimeError) -> tx_subsystems::device::RtcError {
    match error {
        TimeError::Unsupported | TimeError::Unavailable => {
            tx_subsystems::device::RtcError::Unsupported
        }
        TimeError::Invalid => tx_subsystems::device::RtcError::InvalidTime,
        TimeError::Range => tx_subsystems::device::RtcError::Range,
        TimeError::Hardware => tx_subsystems::device::RtcError::Hardware,
    }
}

fn publish_rtc_alarm_event_from_timer(_payload: u64) {
    record_rtc_event(RtcEventMask::ALARM);
}

fn install_emulated_rtc_alarm(
    alarm: RtcAlarm,
    emulation: Option<RtcAlarmEmulation<'_>>,
) -> Result<(), tx_subsystems::device::RtcError> {
    let ns = alarm.time.to_unix_ns()?;
    RTC_ALARM_NS.store(ns, Ordering::Release);
    RTC_ALARM_ENABLED.store(alarm.enabled, Ordering::Release);
    RTC_ALARM_PENDING.store(alarm.pending, Ordering::Release);

    let mut guard_slot = RTC_ALARM_TIMER_GUARD.lock();
    guard_slot.take();
    if alarm.enabled {
        let emulation = emulation.ok_or(tx_subsystems::device::RtcError::Unsupported)?;
        let (event_queue, _) = ensure_rtc_event_queue();
        let guard = emulation
            .registrar
            .register_deadline(
                DeadlineNs::new(emulation.monotonic_deadline_ns),
                TimerRole::RtcAlarm,
                TimerTarget::DeviceCallback(
                    DeviceTimerCallback::new(publish_rtc_alarm_event_from_timer, 0)
                        .with_raw_queue_wake(event_queue, RTC_EVENT_READABLE),
                ),
            )
            .map_err(|_| tx_subsystems::device::RtcError::Unsupported)?;
        *guard_slot = Some(guard);
    }
    Ok(())
}

struct RtcCharOps;

impl CharDeviceOps for RtcCharOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        const RTC_IRQF: u64 = 0x80;
        const RTC_UF: u64 = 0x10;
        const RTC_AF: u64 = 0x20;
        const RTC_READ_RECORD_BYTES: usize = core::mem::size_of::<u64>();

        if out.len() < RTC_READ_RECORD_BYTES {
            return StepOutcome::err(Errno::EINVAL.into());
        }

        let bits = RTC_EVENT_PENDING_BITS.swap(0, Ordering::AcqRel);
        let mask = RtcEventMask::from_bits_truncate(bits);
        if mask.is_empty() {
            return StepOutcome::err(Errno::EAGAIN.into());
        }

        let count = RTC_EVENT_COUNT.swap(0, Ordering::AcqRel).max(1);
        let mut record = (count << 8) | RTC_IRQF;
        if mask.contains(RtcEventMask::UPDATE) {
            record |= RTC_UF;
        }
        if mask.contains(RtcEventMask::ALARM) {
            record |= RTC_AF;
            RTC_ALARM_PENDING.store(false, Ordering::Release);
        }
        out[..RTC_READ_RECORD_BYTES].copy_from_slice(&record.to_ne_bytes());
        if RTC_EVENT_PENDING_BITS.load(Ordering::Acquire) == 0 {
            RTC_EVENT_STATE.clear_readable();
        }
        StepOutcome::done(RTC_READ_RECORD_BYTES)
    }

    fn write(&self, _bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::err(Errno::EINVAL.into())
    }

    fn rtc_ops(&self) -> Option<&dyn RtcDeviceOps> {
        Some(self)
    }
}

impl RtcDeviceOps for RtcCharOps {
    fn read_time(&self, guard: &Guard<'_>) -> Result<RtcTime, tx_subsystems::device::RtcError> {
        let ptr = RTC_READ_TIME_FN.load(Ordering::Acquire);
        if ptr == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let read_time: RtcReadTimeFn = unsafe { core::mem::transmute(ptr) };
        read_time(guard)
    }

    fn set_time(
        &self,
        time: RtcTime,
        guard: &Guard<'_>,
    ) -> Result<(), tx_subsystems::device::RtcError> {
        let ptr = RTC_SET_TIME_FN.load(Ordering::Acquire);
        if ptr == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let set_time: RtcSetTimeFn = unsafe { core::mem::transmute(ptr) };
        set_time(time, guard)
    }

    fn read_alarm(&self, _guard: &Guard<'_>) -> Result<RtcAlarm, tx_subsystems::device::RtcError> {
        if RTC_SET_ALARM_FN.load(Ordering::Acquire) == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let ns = RTC_ALARM_NS.load(Ordering::Acquire);
        let time = RtcTime::from_unix_ns(ns)?;
        Ok(RtcAlarm {
            time,
            enabled: RTC_ALARM_ENABLED.load(Ordering::Acquire),
            pending: RTC_ALARM_PENDING.load(Ordering::Acquire),
        })
    }

    fn set_alarm(
        &self,
        alarm: RtcAlarm,
        guard: &Guard<'_>,
    ) -> Result<(), tx_subsystems::device::RtcError> {
        let ptr = RTC_SET_ALARM_FN.load(Ordering::Acquire);
        if ptr == 0 {
            return Err(tx_subsystems::device::RtcError::Unsupported);
        }
        let set_alarm: RtcSetAlarmFn = unsafe { core::mem::transmute(ptr) };
        set_alarm(alarm, guard)
    }

    fn set_alarm_with_emulation(
        &self,
        alarm: RtcAlarm,
        guard: &Guard<'_>,
        emulation: Option<RtcAlarmEmulation<'_>>,
    ) -> Result<(), tx_subsystems::device::RtcError> {
        match self.set_alarm(alarm, guard) {
            Ok(()) => {
                RTC_ALARM_TIMER_GUARD.lock().take();
                Ok(())
            }
            Err(tx_subsystems::device::RtcError::Unsupported) => {
                install_emulated_rtc_alarm(alarm, emulation)
            }
            Err(error) => Err(error),
        }
    }

    fn poll_events(
        &self,
        _guard: &Guard<'_>,
    ) -> Result<RtcEventMask, tx_subsystems::device::RtcError> {
        Ok(RtcEventMask::from_bits_truncate(
            RTC_EVENT_PENDING_BITS.load(Ordering::Acquire),
        ))
    }
}

static RTC_CHAR_OPS: RtcCharOps = RtcCharOps;

pub static RTC_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(10, 135),
    name: "rtc",
    ops: &RTC_CHAR_OPS,
};

/// Pseudo-random char device backing `/dev/urandom` and `/dev/random`.
///
/// `iperf3` (and other tools) generate their session cookie by reading bytes
/// from `/dev/urandom`; with no node present `open` returns `ENOENT` and the
/// run aborts before any throughput is measured. There is no hardware entropy
/// pool here, so reads are served from a self-contained SplitMix64 generator.
/// The state advances on every read so successive cookies differ within a boot;
/// this is non-cryptographic and only intended to satisfy callers that want a
/// stream of varied bytes (matching the `getrandom(2)` shim, which is likewise
/// a deterministic fill — see `numbers.rs` `GRND_RANDOM`).
struct RandomCharOps;

/// SplitMix64 state, seeded with the golden-ratio constant. Advanced in place by
/// [`splitmix64`] on each read; shared by `/dev/urandom` and `/dev/random`.
static RANDOM_STATE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15);

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl CharDeviceOps for RandomCharOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        use core::sync::atomic::Ordering;
        let mut state = RANDOM_STATE.load(Ordering::Relaxed);
        for chunk in out.chunks_mut(8) {
            let word = splitmix64(&mut state).to_le_bytes();
            for (dst, src) in chunk.iter_mut().zip(word.iter()) {
                *dst = *src;
            }
        }
        RANDOM_STATE.store(state, Ordering::Relaxed);
        StepOutcome::done(out.len())
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        // Writes seed the kernel pool on Linux; here they are accepted and
        // discarded so `> /dev/random` style callers do not see EINVAL.
        StepOutcome::done(bytes.len())
    }
}

static RANDOM_CHAR_OPS: RandomCharOps = RandomCharOps;

static URANDOM_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 9),
    name: "urandom",
    ops: &RANDOM_CHAR_OPS,
};

static RANDOM_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(1, 8),
    name: "random",
    ops: &RANDOM_CHAR_OPS,
};

impl Devfs {
    pub const fn new() -> Self {
        Self
    }

    /// v3 factory matching the `MountOutput` shape Phase 3b will pass
    /// into `MountIdentity::new_cap`.
    pub fn fs_ops_arc() -> Arc<dyn FsOps> {
        Arc::new(Self)
    }

    /// v3 page-backing factory.
    pub fn fs_page_backing_arc() -> Arc<dyn FsPageBacking> {
        Arc::new(Self)
    }
}

fn entry_index_from_object_id(id: FsObjectId) -> Option<usize> {
    let raw = id.as_u64();
    if raw < DEVFS_ENTRY_OBJECT_BASE {
        return None;
    }
    usize::try_from(raw - DEVFS_ENTRY_OBJECT_BASE).ok()
}

fn static_char_entry_index(name: &[u8]) -> Option<usize> {
    DEVFS_STATIC_CHAR_ENTRIES
        .iter()
        .position(|entry| entry.name.as_bytes() == name)
}

fn static_char_entry_by_combined_index(idx: usize) -> Option<&'static CharDeviceBinding> {
    let tty_len = tty::project::devfs_alias_entries().len();
    idx.checked_sub(tty_len)
        .and_then(|static_idx| DEVFS_STATIC_CHAR_ENTRIES.get(static_idx).copied())
}

/// Materialise an `RNode` for the named devfs alias.
///
/// Construction follows `txdoc:TTY-RNODE-MATERIALIZATION-1`: char-device
/// entries are `StructBacked { payload: StructPayload::Tty(...) }`, which
/// already routes `step_read` / `step_write` through the TTY subsystem
/// (see `crates/tx-subsystems/src/vfs/execution.rs` `OpenFile::step_read`
/// / `step_write` dispatch on `RNodeBacking::StructBacked`).
///
/// Returns `Errno::ENOENT` if the registry has no such alias,
/// `Errno::EIO` if RNode allocation fails.
pub fn resolve_console_rnode(name: &[u8]) -> StepOutcome<Cap<RNode>, NoProgress> {
    use StepOutcome as V3;
    let Some(tty) = tty::project::resolve_devfs_alias(name) else {
        return V3::err(Errno::ENOENT.into());
    };

    let entries = tty::project::devfs_alias_entries();
    let object_id = entries
        .iter()
        .position(|entry| entry.name == name)
        .map(|idx| FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + idx as u64))
        .unwrap_or(DEVFS_ROOT_OBJECT_ID);

    match RNode::new_cap(
        object_id,
        InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        },
    ) {
        Ok(rnode) => V3::done(rnode),
        Err(_) => V3::err(Errno::EIO.into()),
    }
}

/// One-shot helper: open `/dev/console` as a read+write `OpenFile`.
///
/// Phase 4 of the pre-ELF runtime plan
/// (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`)
/// retires the bootstrap exemption: when init's cwd is bound and the
/// rootfs+devfs mount table is populated, the body goes through
/// `vfs::step_open(root, b"/dev/console", RDWR, ...)` per
/// `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`.
///
/// The function stays synchronous because callers in
/// `crates/tx-kernel/src/init.rs::bind_init_cwd_and_root` are not in
/// async context. We drive the walker via a tiny in-module
/// [`block_on`] helper.
///
/// ## Fallback path
///
/// The Phase 1+4 sibling slices may land out of order with the
/// `mount_devfs_at_dev` wiring that *publishes* the `mounted` hint
/// on `/dev`. When init's cwd or the mount-hint tree is not yet
/// observable, the walker has no way to cross from rootfs into
/// devfs and `step_open(/dev/console)` would return `ENOENT`. To
/// keep the trio's existing tests green during the staged rollout,
/// the helper falls back to the legacy direct-RNode materialisation
/// (the same shape the bootstrap exemption used) whenever
/// `init_process()` is `None`. Once init.rs's
/// `mount_devfs_at_dev` is updated to set the mount hint and
/// `bind_init_cwd_and_root` runs before the helper is called, the
/// fallback is skipped and the walker resolves the path end-to-end.
///
/// # Panics
///
/// Panics if neither the walker nor the fallback can resolve
/// `/dev/console`. Both branches indicate the kernel cannot make
/// further bootstrap progress.
pub fn open_console_for_init() -> Cap<OpenFile> {
    open_console_for_init_legacy()
}

pub fn open_console_for_init_via_walker() -> Cap<OpenFile> {
    if let Some(init) = process::execution::init_process() {
        if let Some(root) = init.cwd() {
            // Bootstrap path: init opens /dev/console as root.
            let cred = Credential::root();
            let guard = step_engine::guard();
            use StepOutcome as V3;
            let outcome = vfs::step_open(
                root,
                b"/dev/console",
                OpenFileFlags {
                    read: true,
                    write: true,
                    append: false,
                    cloexec: false,
                    nonblocking: false,
                    packet: false,
                },
                0,
                &cred,
                &guard,
            );
            match outcome {
                V3::Done(file) => return file,
                _other => {
                    // Walker could not resolve the path under current
                    // bootstrap state — fall through to legacy
                    // direct-RNode materialisation. Once Phase 1's
                    // `mount_devfs_at_dev` mount-hint wiring lands,
                    // this branch is unreachable on the boot path.
                }
            }
        }
    }
    open_console_for_init_legacy()
}

/// Legacy direct-RNode path. Materialises an `OpenFile` over the
/// `console` alias's TTY without consulting the walker. Retained as
/// the fallback for `open_console_for_init`'s walker redirect (see
/// the function's "Fallback path" doc) and as the synchronous
/// primitive Phase 2a tests still rely on.
fn open_console_for_init_legacy() -> Cap<OpenFile> {
    let tty = tty::project::resolve_devfs_alias(b"console").expect(
        "open_console_for_init: /dev/console alias not registered before bootstrap fd preopen \
         (Phase 3b registers `console` via tty::execution::register_console_alias)",
    );

    let rnode = RNode::new_cap(
        FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE),
        InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        },
    )
    .expect("open_console_for_init: RNode reservation failed");

    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("open_console_for_init: OpenFile reservation failed")
}

/// Tiny synchronous future driver used by [`open_console_for_init`].
///
/// `vfs::step_open` is `async fn`. Init's bootstrap path is fully
/// synchronous, so we drive the future on a no-op waker until it
/// resolves. The walker today never actually parks (every
/// `FsOps` call site is synchronous in-memory), so this loop
/// terminates on the first poll for the bootstrap path; we cap at
/// 1024 polls to surface a runaway future during development.
#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
fn block_on<F: core::future::Future>(mut fut: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn raw_clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn raw_wake(_: *const ()) {}
    fn raw_wake_by_ref(_: *const ()) {}
    fn raw_drop(_: *const ()) {}
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: the vtable is `'static` and the data pointer is
    // unused (raw_* take it but never dereference).
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    // SAFETY: `fut` lives on the stack for the full loop; we never
    // move it after the unchecked pin.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("open_console_for_init::block_on: future did not resolve in 1024 polls");
}

// === FsOps / FsPageBacking impls ====================================
//
// Devfs is a read-only projection backend (every mutating op returns
// `EROFS`, every page-cache op returns `ENOSYS`) — every method
// returns synchronously, so each body is a direct
// `StepOutcome::done(...)` / `StepOutcome::err(...)` ladder. Per the
// wave-8 design doc
// (`docs/progress/decisions/2026-05-09-fsops-v3-design.md`), v3 callers
// (the walker entry points) opt into these impls via
// `Arc<dyn FsOps>` / `Arc<dyn FsPageBacking>`.
//
// Fully-qualified `adapter::step_engine::*` references at the impl
// sites avoid clashing with `tx_subsystems::execution::StepOutcome`
// already in scope, per the wave-4/6/7 trait-impl convention.

impl FsOps for Devfs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId, NoProgress> {
        if parent == DEVFS_MISC_DIR_OBJECT_ID {
            if name == DEVFS_RTC_NAME {
                return StepOutcome::done(DEVFS_RTC_OBJECT_ID);
            }
            return StepOutcome::err(Errno::ENOENT.into());
        }
        if parent != DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::err(Errno::ENOENT.into());
        }
        // `/dev/block` is a synthetic mountpoint directory: bdev-fs
        // (`docs/design/05_filesystem/BDEV_FS.md` §7.1) attaches here
        // at boot so `/dev/block/vda*` open as page-backed files.
        if name == DEVFS_BLOCK_DIR_NAME {
            return StepOutcome::done(DEVFS_BLOCK_DIR_OBJECT_ID);
        }
        if name == DEVFS_SHM_DIR_NAME {
            return StepOutcome::done(DEVFS_SHM_DIR_OBJECT_ID);
        }
        if name == DEVFS_NULL_NAME {
            return StepOutcome::done(DEVFS_NULL_OBJECT_ID);
        }
        if name == DEVFS_ZERO_NAME {
            return StepOutcome::done(DEVFS_ZERO_OBJECT_ID);
        }
        if name == DEVFS_URANDOM_NAME {
            return StepOutcome::done(DEVFS_URANDOM_OBJECT_ID);
        }
        if name == DEVFS_RANDOM_NAME {
            return StepOutcome::done(DEVFS_RANDOM_OBJECT_ID);
        }
        if name == DEVFS_MISC_DIR_NAME {
            return StepOutcome::done(DEVFS_MISC_DIR_OBJECT_ID);
        }
        let entries = tty::project::devfs_alias_entries();
        if tty::project::resolve_devfs_alias(name).is_some() {
            // Identify the entry by its position in the live alias
            // snapshot. Stable for one snapshot, opaque to the caller —
            // devfs makes no inode-persistence promise.
            for (idx, entry) in entries.iter().enumerate() {
                if entry.name == name {
                    return StepOutcome::done(FsObjectId::new(
                        DEVFS_ENTRY_OBJECT_BASE + idx as u64,
                    ));
                }
            }
        }
        if let Some(idx) = static_char_entry_index(name) {
            return StepOutcome::done(FsObjectId::new(
                DEVFS_ENTRY_OBJECT_BASE + entries.len() as u64 + idx as u64,
            ));
        }
        StepOutcome::err(Errno::ENOENT.into())
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta, NoProgress> {
        if fs_object_id == DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, DEVFS_ROOT_MODE));
        }
        if fs_object_id == DEVFS_BLOCK_DIR_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, DEVFS_BLOCK_DIR_MODE));
        }
        if fs_object_id == DEVFS_SHM_DIR_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, DEVFS_SHM_DIR_MODE));
        }
        if fs_object_id == DEVFS_MISC_DIR_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::Directory, DEVFS_MISC_DIR_MODE));
        }
        if fs_object_id == DEVFS_NULL_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::CharDevice, DEVFS_NULL_MODE));
        }
        if fs_object_id == DEVFS_ZERO_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::CharDevice, DEVFS_NULL_MODE));
        }
        if fs_object_id == DEVFS_URANDOM_OBJECT_ID || fs_object_id == DEVFS_RANDOM_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::CharDevice, DEVFS_NULL_MODE));
        }
        if fs_object_id == DEVFS_RTC_OBJECT_ID {
            return StepOutcome::done(InodeMeta::new(InodeKind::CharDevice, DEVFS_RTC_MODE));
        }
        if entry_index_from_object_id(fs_object_id)
            .and_then(|idx| tty::project::devfs_alias_entries().into_iter().nth(idx))
            .is_some()
            || entry_index_from_object_id(fs_object_id)
                .and_then(static_char_entry_by_combined_index)
                .is_some()
        {
            return StepOutcome::done(InodeMeta::new(InodeKind::CharDevice, DEVFS_CHAR_MODE));
        }
        StepOutcome::err(Errno::ENOENT.into())
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        if fs_object_id == DEVFS_BLOCK_DIR_OBJECT_ID {
            // bdev-fs is mounted on top of `/dev/block`; this stub
            // is an empty directory until that mount publishes (and
            // after publication the walker crosses into bdev-fs
            // before this readdir would observe contents).
            return StepOutcome::done(None);
        }
        if fs_object_id == DEVFS_SHM_DIR_OBJECT_ID {
            // tmpfs is mounted on top of `/dev/shm`; this stub is
            // an empty directory.
            return StepOutcome::done(None);
        }
        if fs_object_id == DEVFS_MISC_DIR_OBJECT_ID {
            if cursor.as_u64() == 0 {
                let dir_entry =
                    match DirEntry::new(DEVFS_RTC_OBJECT_ID, InodeKind::CharDevice, DEVFS_RTC_NAME)
                    {
                        Ok(de) => de,
                        Err(err) => return StepOutcome::err(err.into()),
                    };
                return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(1))));
            }
            return StepOutcome::done(None);
        }
        if fs_object_id != DEVFS_ROOT_OBJECT_ID {
            return StepOutcome::err(Errno::ENOTDIR.into());
        }
        let entries = tty::project::devfs_alias_entries();
        let index = cursor.as_u64() as usize;
        // Cursor 0..entries.len() emits the TTY aliases; the next
        // cursor slots emit static devfs nodes, followed by synthetic
        // mountpoint stubs.
        if index < entries.len() {
            let entry = &entries[index];
            let dir_entry = match DirEntry::new(
                FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + index as u64),
                InodeKind::CharDevice,
                &entry.name,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        let static_index = index.saturating_sub(entries.len());
        if let Some(binding) = DEVFS_STATIC_CHAR_ENTRIES.get(static_index) {
            let dir_entry = match DirEntry::new(
                FsObjectId::new(DEVFS_ENTRY_OBJECT_BASE + index as u64),
                InodeKind::CharDevice,
                binding.name.as_bytes(),
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        let synthetic_index = entries.len() + DEVFS_STATIC_CHAR_ENTRIES.len();
        if index == synthetic_index {
            let dir_entry =
                match DirEntry::new(DEVFS_NULL_OBJECT_ID, InodeKind::CharDevice, DEVFS_NULL_NAME) {
                    Ok(de) => de,
                    Err(err) => return StepOutcome::err(err.into()),
                };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == synthetic_index + 1 {
            let dir_entry =
                match DirEntry::new(DEVFS_ZERO_OBJECT_ID, InodeKind::CharDevice, DEVFS_ZERO_NAME) {
                    Ok(de) => de,
                    Err(err) => return StepOutcome::err(err.into()),
                };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == synthetic_index + 2 {
            let dir_entry = match DirEntry::new(
                DEVFS_BLOCK_DIR_OBJECT_ID,
                InodeKind::Directory,
                DEVFS_BLOCK_DIR_NAME,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == synthetic_index + 3 {
            let dir_entry = match DirEntry::new(
                DEVFS_SHM_DIR_OBJECT_ID,
                InodeKind::Directory,
                DEVFS_SHM_DIR_NAME,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == synthetic_index + 4 {
            let dir_entry = match DirEntry::new(
                DEVFS_MISC_DIR_OBJECT_ID,
                InodeKind::Directory,
                DEVFS_MISC_DIR_NAME,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == entries.len() + 5 {
            let dir_entry = match DirEntry::new(
                DEVFS_URANDOM_OBJECT_ID,
                InodeKind::CharDevice,
                DEVFS_URANDOM_NAME,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        if index == entries.len() + 6 {
            let dir_entry = match DirEntry::new(
                DEVFS_RANDOM_OBJECT_ID,
                InodeKind::CharDevice,
                DEVFS_RANDOM_NAME,
            ) {
                Ok(de) => de,
                Err(err) => return StepOutcome::err(err.into()),
            };
            return StepOutcome::done(Some((dir_entry, DirCursor::from_u64(cursor.as_u64() + 1))));
        }
        StepOutcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // Registry entries are owned by the TTY subsystem; devfs has no
        // inode storage to release. Successful no-op so the VFS layer
        // can drop its `RNode` without seeing a backend error.
        StepOutcome::done(())
    }

    /// Materialise an `RNode` for a character-device alias.
    ///
    /// Per `txdoc:TTY-RNODE-MATERIALIZATION-1`
    /// (`docs/design/06_devices/TTY.md` §6.3): char-device entries on
    /// devfs project the registered TTY as
    /// `RNodeBacking::StructBacked { payload: StructPayload::Tty(...) }`,
    /// which routes `OpenFile::step_read` / `step_write` through the
    /// TTY subsystem. The walker invokes this hook after `lookup` /
    /// `load_inode_meta` resolve a char-device inode; without it, the
    /// walker falls through to the trait default (`ENOSYS`) and the
    /// path resolution fails at the terminal component.
    ///
    /// Mirrors the standalone `resolve_console_rnode` helper above —
    /// the latter is retained for the bootstrap-fallback path
    /// (`open_console_for_init_legacy`); this hook is the production
    /// walker site.
    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, NoProgress> {
        if meta.kind() != InodeKind::CharDevice {
            // devfs only publishes the root directory and
            // char-device aliases. Directories are handled by the
            // walker's inline `Directory` arm; anything else is a
            // backend bug.
            return StepOutcome::err(Errno::ENOSYS.into());
        }
        if fs_object_id == DEVFS_NULL_OBJECT_ID {
            return match RNode::new_cap_in_mount(
                fs_object_id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::CharDevice(&NULL_CHAR_BINDING),
                },
                mount,
            ) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(Errno::EIO.into()),
            };
        }
        if fs_object_id == DEVFS_ZERO_OBJECT_ID {
            return match RNode::new_cap_in_mount(
                fs_object_id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::CharDevice(&ZERO_CHAR_BINDING),
                },
                mount,
            ) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(Errno::EIO.into()),
            };
        }
        if fs_object_id == DEVFS_RTC_OBJECT_ID {
            return match RNode::new_cap_in_mount(
                fs_object_id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::CharDevice(&RTC_CHAR_BINDING),
                },
                mount,
            ) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(Errno::EIO.into()),
            };
        }
        // `/dev/urandom` and `/dev/random`: `lookup`, `load_inode_meta`, and
        // `devt_for_object_id` resolve these by their own object ids (not the
        // combined tty/static-char index), so the open path needs an explicit
        // arm too — without it `open` falls through to ENOENT and entropy
        // readers (`iperf3` reads `/dev/urandom` for its session cookie) abort.
        if fs_object_id == DEVFS_URANDOM_OBJECT_ID || fs_object_id == DEVFS_RANDOM_OBJECT_ID {
            let binding = if fs_object_id == DEVFS_URANDOM_OBJECT_ID {
                &URANDOM_CHAR_BINDING
            } else {
                &RANDOM_CHAR_BINDING
            };
            return match RNode::new_cap_in_mount(
                fs_object_id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::CharDevice(binding),
                },
                mount,
            ) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(Errno::EIO.into()),
            };
        }
        if let Some(binding) =
            entry_index_from_object_id(fs_object_id).and_then(static_char_entry_by_combined_index)
        {
            return match RNode::new_cap_in_mount(
                fs_object_id,
                meta,
                RNodeBacking::StructBacked {
                    payload: StructPayload::CharDevice(binding),
                },
                mount,
            ) {
                Ok(rnode) => StepOutcome::done(rnode),
                Err(_) => StepOutcome::err(Errno::EIO.into()),
            };
        }
        let Some(idx) = entry_index_from_object_id(fs_object_id) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let entries = tty::project::devfs_alias_entries();
        let Some(entry) = entries.into_iter().nth(idx) else {
            return StepOutcome::err(Errno::ENOENT.into());
        };
        let tty = entry.tty;
        match RNode::new_cap_in_mount(
            fs_object_id,
            meta,
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            },
            mount,
        ) {
            Ok(rnode) => StepOutcome::done(rnode),
            Err(_) => StepOutcome::err(Errno::EIO.into()),
        }
    }

    fn chmod_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // devfs is a read-only projection-shaped backend; mode-bit
        // mutation is not supported.
        StepOutcome::err(Errno::EROFS.into())
    }

    fn chown_inode(
        &self,
        _fs_object_id: FsObjectId,
        _new_uid: Option<u32>,
        _new_gid: Option<u32>,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::EROFS.into())
    }
}

impl FsPageBacking for Devfs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame, NoProgress> {
        // Char-device I/O does not flow through the page cache; routing
        // happens via `OpenFile::step_read` / `step_write` against the
        // RNode's `StructBacked { Tty }` backing instead.
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        // Linux treats O_TRUNC on character devices as a no-op. Devfs only
        // page-backs projection metadata, so no device content changes here.
        match <Self as FsOps>::load_inode_meta(self, fs_object_id, _guard) {
            StepOutcome::Done(meta) if meta.kind() == InodeKind::CharDevice => {
                return StepOutcome::done(());
            }
            StepOutcome::Done(_) => {}
            StepOutcome::Err(errno) => return StepOutcome::Err(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return StepOutcome::err(Errno::EIO.into());
            }
        }
        StepOutcome::err(Errno::ENOSYS.into())
    }

    fn fsync_file(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::err(Errno::ENOSYS.into())
    }

    // `fallocate` keeps the v3 trait default (`Done(())`) — devfs has
    // no on-disk space to reserve, so a hint is a successful no-op.
    // `supports_reflink` keeps the trait default (`false`).
}

#[cfg(test)]
mod tests;
