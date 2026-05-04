//! Thread-runtime subsystem structure: identities and payloads.
//!
//! Per `THREAD_RUNTIME_v1`, the thread is the unit of execution: each
//! thread owns its own future + reactor task. This module realizes the
//! identity/payload split. Signal-related fields (`signal_mask`,
//! `signal_summary`, `thread_pending`) land in the signal pass and are
//! intentionally absent here.

use core::sync::atomic::{AtomicU32, Ordering};

use tx_reactor::TaskKey;
use tx_substrate::zone::{PayloadCap, Weak, Zone, ZoneAllocated};

use crate::process::ProcessIdentity;
use crate::sync::SpinMutex;

/// Thread identifier. TID 0 is reserved.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Tid(pub u32);

impl Tid {
    pub const RESERVED: Self = Self(0);
}

/// Thread identity. Persists across payload teardown so a zombie thread
/// can be `wait`-ed on without racing the runtime tear-down path.
///
/// `owner_proc` is `Weak` to break the ownership cycle: the parent
/// `ProcessPayload` retains threads via `Cap<ThreadIdentity>`, so the
/// thread cannot strongly reference the parent or the parent would never
/// drop.
pub struct ThreadIdentity {
    pub tid: Tid,
    pub(crate) owner_proc: Weak<ProcessIdentity>,
    pub(crate) exit_status: SpinMutex<Option<i32>>,
    pub(crate) payload: SpinMutex<Option<PayloadCap<ThreadPayload>>>,
}

impl ThreadIdentity {
    /// Read the recorded thread exit status. `Some` once
    /// `step_thread_exit` has run; otherwise `None`.
    pub fn exit_status(&self) -> Option<i32> {
        *self.exit_status.lock()
    }

    /// Whether the thread is a zombie (payload dropped, identity
    /// retained until reaped or the parent process drops).
    pub fn is_zombie(&self) -> bool {
        self.payload.lock().is_none()
    }

    /// Snapshot the owning process via `Weak::upgrade` under a fresh
    /// guard. Returns `None` if the process identity has been dropped.
    pub fn upgrade_owner_proc(&self) -> Option<tx_substrate::zone::Cap<ProcessIdentity>> {
        let guard = tx_substrate::epoch::guard();
        self.owner_proc.upgrade(&guard)
    }
}

/// Thread payload. Dropped on `step_thread_exit`.
///
/// `task` is the reactor `TaskKey` driving this thread's future. It is
/// `None` until the reactor coupling lands (β4); for now construction
/// paths leave it `None` and tests do not exercise reactor wiring.
pub struct ThreadPayload {
    pub(crate) task: SpinMutex<Option<TaskKey>>,
}

impl ThreadPayload {
    /// Snapshot the reactor task handle, if one has been bound. Always
    /// `None` until the reactor coupling lands.
    pub fn task(&self) -> Option<TaskKey> {
        *self.task.lock()
    }
}

static THREAD_IDENTITY_ZONE: Zone<ThreadIdentity> = Zone::const_new();
static THREAD_PAYLOAD_ZONE: Zone<ThreadPayload> = Zone::const_new();

unsafe impl ZoneAllocated for ThreadIdentity {
    fn zone() -> &'static Zone<Self> {
        &THREAD_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for ThreadPayload {
    fn zone() -> &'static Zone<Self> {
        &THREAD_PAYLOAD_ZONE
    }
}

/// Simple atomic TID allocator. TID 1 reserved for the init leader by
/// convention; allocator starts at 2.
static NEXT_TID: AtomicU32 = AtomicU32::new(2);

pub fn allocate_tid() -> Tid {
    Tid(NEXT_TID.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
pub(crate) fn reset_tid_counter_for_test() {
    NEXT_TID.store(2, Ordering::Relaxed);
}
