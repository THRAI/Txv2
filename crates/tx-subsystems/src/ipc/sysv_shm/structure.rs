//! SysV shared memory — identity and payload types.
//!
//! `ShmSegmentIdentity`: key, cred, size, shmid.
//! `ShmSegmentPayload`: page-backed storage, attach count, shmctl state.
//!
//! Day-1 single-namespace: a global `SHM_TABLE` maps shmid → Cap.
//! When `AllocIndex` lands (per `NAMESPACE_VIEW_v1.md` §3), the
//! table moves into `IpcNamespace.sysv_shm` as an
//! `IndexTable<SysvKey, Cap<ShmSegmentIdentity>>`.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use crate::process::adapter::step_engine::{Cap, SpinMutex, Zone, ZoneAllocated, ZoneError};

use crate::cred::Cred;
use crate::process::nsproxy::SysvKey;

// ---------------------------------------------------------------------------
// IpcPerm — shared across all IPC kinds
// ---------------------------------------------------------------------------

/// POSIX IPC permission flags — owner/group/other rw bits.
///
/// Shared across SysV shm, sem, and msg. The `IpcPerm` type lives here
/// because `sysv_shm` is the first landing module; sem and msg import
/// it via `crate::ipc::sysv_shm::structure::IpcPerm`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcPerm {
    pub mode: u16,
}

impl IpcPerm {
    pub const fn new(mode: u16) -> Self {
        Self { mode: mode & 0o777 }
    }

    pub const fn owner_read(self) -> bool {
        (self.mode & 0o400) != 0
    }
    pub const fn owner_write(self) -> bool {
        (self.mode & 0o200) != 0
    }
    pub const fn group_read(self) -> bool {
        (self.mode & 0o040) != 0
    }
    pub const fn group_write(self) -> bool {
        (self.mode & 0o020) != 0
    }
    pub const fn other_read(self) -> bool {
        (self.mode & 0o004) != 0
    }
    pub const fn other_write(self) -> bool {
        (self.mode & 0o002) != 0
    }
}

// ---------------------------------------------------------------------------
// ShmSegmentIdentity / ShmSegmentPayload
// ---------------------------------------------------------------------------

/// System V shared memory segment identity.
///
/// Key semantics: `Some(k)` for `shmget(key, ...)`; `None` for
/// `shmget(IPC_PRIVATE, ...)`. The id (`shmid`) is assigned by
/// the shm allocator on creation and is never reused (monotonic
/// counter per the day-1 single-namespace design).
pub struct ShmSegmentIdentity {
    pub key: Option<SysvKey>,
    pub shmid: u32,
    pub cred: Cap<Cred>,
    pub size: usize,
    pub mode: AtomicU16,
    pub uid: AtomicU32,
    pub gid: AtomicU32,
    /// Creator uid/gid — used by shmctl IPC_STAT.
    pub cuid: u32,
    pub cgid: u32,
    /// Marked for deletion (IPC_RMID has been called). No new
    /// attaches are allowed after this flag is set; existing
    /// attaches continue to work. The segment is destroyed when
    /// the last attach is removed.
    ///
    /// `AtomicBool` because `Cap<T>` only provides `Deref` (immutable
    /// shared access). Mutation goes through atomic stores so the
    /// flag can be set without `DerefMut`.
    pub destroyed: core::sync::atomic::AtomicBool,
    /// Live segment payload.
    pub payload: Cap<ShmSegmentPayload>,
}

impl ShmSegmentIdentity {
    pub fn perm(&self) -> IpcPerm {
        IpcPerm::new(self.mode.load(Ordering::Relaxed))
    }

    pub fn uid(&self) -> u32 {
        self.uid.load(Ordering::Relaxed)
    }

    pub fn gid(&self) -> u32 {
        self.gid.load(Ordering::Relaxed)
    }

    pub fn key_raw(&self) -> u32 {
        self.key.map(|key| key.0).unwrap_or(0)
    }
}

/// System V shared memory segment payload — the live backing.
///
/// Phase IPC-1: the backing is an anonymous `PageContainer` allocated
/// at `step_shmget` time. `step_shmat` maps it into the caller's
/// address space via the existing VM fault path; `step_shmdt` unmaps.
/// `attach_count` is bumped on shmat, decremented on shmdt.
pub struct ShmSegmentPayload {
    /// Number of active attaches. When zero and `destroyed` is true,
    /// the segment is reclaimed.
    ///
    /// `AtomicU32` because `Cap<T>` only provides `Deref` — mutation
    /// goes through atomic fetch_add / fetch_sub.
    pub attach_count: AtomicU32,
    /// PageContainer holding the segment's pages.
    /// `None` until the page-backed storage is materialized.
    ///
    /// TODO(txdoc:IPC-V1-SHM-1): allocate pages on step_shmget and
    /// integrate with PageBacked/RNodeBacking for demand-paged
    /// materialization on shmat. For IPC-1 stub, the attacher is
    /// responsible for faulting in pages.
    pub page_container: Option<Cap<crate::page_backed::PageContainer>>,
}

// ---------------------------------------------------------------------------
// Global shm registry — day-1 single-namespace
// ---------------------------------------------------------------------------

/// Global SysV shm segment table. Maps shmid → identity Cap.
/// Replaced by `IpcNamespace.sysv_shm` when `AllocIndex` lands.
static SHM_TABLE: SpinMutex<BTreeMap<u32, Cap<ShmSegmentIdentity>>> =
    SpinMutex::new(BTreeMap::new());

/// Monotonic shmid allocator. Starts at 0 and increments; shmids
/// are never reused (matching Linux's `ipc_ids` allocator).
static NEXT_SHMID: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Zone registration
// ---------------------------------------------------------------------------

static SHM_IDENTITY_ZONE: Zone<ShmSegmentIdentity> = Zone::const_new();
static SHM_PAYLOAD_ZONE: Zone<ShmSegmentPayload> = Zone::const_new();

unsafe impl ZoneAllocated for ShmSegmentIdentity {
    fn zone() -> &'static Zone<Self> {
        &SHM_IDENTITY_ZONE
    }
}

unsafe impl ZoneAllocated for ShmSegmentPayload {
    fn zone() -> &'static Zone<Self> {
        &SHM_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    use crate::process::adapter::step_engine::register_zone_for;
    register_zone_for::<ShmSegmentIdentity>()?;
    register_zone_for::<ShmSegmentPayload>()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public registry accessors
// ---------------------------------------------------------------------------

/// Look up a shm segment by id. Returns `None` if the id is not
/// registered or the segment has been destroyed and all attaches
/// have been removed.
pub(crate) fn lookup_shm(shmid: u32) -> Option<Cap<ShmSegmentIdentity>> {
    SHM_TABLE.lock().get(&shmid).cloned()
}

/// Register a newly created segment and return its shmid.
pub(crate) fn register_shm(
    key: Option<SysvKey>,
    cred: Cap<Cred>,
    size: usize,
    perm: IpcPerm,
    cuid: u32,
    cgid: u32,
) -> Result<u32, ZoneError> {
    use crate::process::adapter::step_engine::sign;
    let shmid = NEXT_SHMID.fetch_add(1, Ordering::Relaxed);
    let payload = sign(ShmSegmentPayload {
        attach_count: AtomicU32::new(0),
        page_container: None,
    })?;
    let identity = sign(ShmSegmentIdentity {
        key,
        shmid,
        cred,
        size,
        mode: AtomicU16::new(perm.mode),
        uid: AtomicU32::new(cuid),
        gid: AtomicU32::new(cgid),
        cuid,
        cgid,
        destroyed: AtomicBool::new(false),
        payload,
    })?;
    SHM_TABLE.lock().insert(shmid, identity);
    Ok(shmid)
}

/// Withdraw a segment from the global table (IPC_RMID).
/// Returns the identity cap so the caller can mark it destroyed.
pub(crate) fn withdraw_shm(shmid: u32) -> Option<Cap<ShmSegmentIdentity>> {
    SHM_TABLE.lock().remove(&shmid)
}

/// Iterate all live segments (for /proc/sysvipc/shm projection).
#[expect(
    dead_code,
    reason = "txdoc:IPC-V1-SHM-1 — consumed by procfs projection when wired"
)]
pub(crate) fn all_shm_segments() -> alloc::vec::Vec<Cap<ShmSegmentIdentity>> {
    SHM_TABLE.lock().values().cloned().collect()
}
