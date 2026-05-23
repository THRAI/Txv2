//! SysV shared memory step operations.
//!
//! Phase IPC-1. Zero new WaitSource — the only blocking operation
//! is page-fault-on-attach, already handled by the existing VM fault
//! path. All four step functions are immediate (synchronous).

use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_shm::checks;
use crate::ipc::sysv_shm::structure::{self, IpcPerm, ShmAttach};
use crate::process::adapter::step_engine::Cap;
use crate::process::nsproxy::SysvKey;
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapError,
    VmMapRequest, USER_PAGE_SIZE,
};

// ---------------------------------------------------------------------------
// IPC constants (matching Linux's <sys/ipc.h> and <sys/shm.h>)
// ---------------------------------------------------------------------------

/// Create a new segment. Fails if key already exists.
pub const IPC_CREAT: i32 = 0o1000;
/// Fail if key doesn't exist (used with IPC_CREAT).
pub const IPC_EXCL: i32 = 0o2000;
/// Private key — no key lookup.
pub const IPC_PRIVATE: i32 = 0;

/// shmctl commands.
pub const IPC_RMID: i32 = 0;
pub const IPC_SET: i32 = 1;
pub const IPC_STAT: i32 = 2;
pub const IPC_INFO: i32 = 3;
pub const SHM_LOCK: i32 = 11;
pub const SHM_UNLOCK: i32 = 12;
pub const SHM_STAT: i32 = 13;
pub const SHM_INFO: i32 = 14;
pub const SHM_STAT_ANY: i32 = 15;

/// shmat flags.
pub const SHM_RDONLY: i32 = 0o10000;
pub const SHM_RND: i32 = 0o20000;
pub const SHM_REMAP: i32 = 0o40000;
pub const SHM_EXEC: i32 = 0o100000;
const SUPPORTED_SHMAT_FLAGS: i32 = SHM_RDONLY | SHM_RND | SHM_REMAP | SHM_EXEC;

/// Non-blocking flag for msgsnd/msgrcv/semop (IPC_NOWAIT = 0o4000).
pub const IPC_NOWAIT: i32 = 0o4000;

// ---------------------------------------------------------------------------
// step_shmget
// ---------------------------------------------------------------------------

/// `shmget(key, size, shmflg)` — get or create a shared memory segment.
///
/// Returns the shmid on success.
pub fn step_shmget(
    key: i32,
    size: usize,
    shmflg: i32,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<u32, Errno> {
    let ipc_key = if key == IPC_PRIVATE {
        None
    } else {
        Some(SysvKey::new(key as u32))
    };

    let create = (shmflg & IPC_CREAT) != 0;
    let exclusive = (shmflg & IPC_EXCL) != 0;

    // Pull the perm bits from shmflg (low 9 bits, matching Linux).
    let perm = IpcPerm::new((shmflg & 0o777) as u16);

    // Check limits.
    let limits = nsproxy.ipc_ns.limits.lock();
    if size < limits.shmmin as usize {
        return Err(Errno::EINVAL);
    }
    if size > limits.shmmax as usize {
        return Err(Errno::EINVAL);
    }
    drop(limits);

    // Check if segment already exists for this key.
    if let Some(ref k) = ipc_key {
        let table = nsproxy.ipc_ns.sysv_shm.lock();
        if let Some(seg) = table.get(k).cloned() {
            drop(table);
            if exclusive {
                return Err(Errno::EEXIST);
            }
            // Verify size: if segment exists, size must be <= existing size.
            if size > seg.size {
                return Err(Errno::EINVAL);
            }
            // Check permissions.
            checks::require_can_read_shm(&seg, cred)?;
            return Ok(seg.shmid);
        }
    }

    // Must create.
    if !create && ipc_key.is_some() {
        return Err(Errno::ENOENT);
    }

    // Allocate the segment.
    let segment =
        structure::register_shm(ipc_key, cred.clone(), size, perm, cred.euid.0, cred.egid.0)
            .map_err(|_| Errno::ENOMEM)?;

    // Register in the namespace's key→shmid table.
    if let Some(ref k) = ipc_key {
        nsproxy.ipc_ns.sysv_shm.lock().insert(*k, segment.clone());
    }

    Ok(segment.shmid)
}

// ---------------------------------------------------------------------------
// step_shmat
// ---------------------------------------------------------------------------

/// `shmat(shmid, shmaddr, shmflg)` — attach a shared memory segment.
///
/// Returns the virtual address where the segment was mapped.
pub async fn step_shmat(
    shmid: u32,
    shmaddr: usize,
    shmflg: i32,
    cred: &Cap<Cred>,
    aspace: &Cap<AddressSpace>,
) -> Result<usize, Errno> {
    if shmflg & !SUPPORTED_SHMAT_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let segment = checks::require_shm_exists(shmid)?;

    if segment.destroyed.load(Ordering::Acquire) {
        return Err(Errno::EIDRM);
    }

    let readonly = (shmflg & SHM_RDONLY) != 0;
    if readonly {
        checks::require_can_read_shm(&segment, cred)?;
    } else {
        checks::require_can_write_shm(&segment, cred)?;
    }

    let rounded_addr = if shmaddr == 0 {
        0
    } else if (shmflg & SHM_RND) != 0 {
        shmaddr & !(USER_PAGE_SIZE - 1)
    } else if !shmaddr.is_multiple_of(USER_PAGE_SIZE) {
        return Err(Errno::EINVAL);
    } else {
        shmaddr
    };
    let len = segment
        .size
        .checked_next_multiple_of(USER_PAGE_SIZE)
        .ok_or(Errno::EINVAL)?;
    let prot = match (readonly, (shmflg & SHM_EXEC) != 0) {
        (true, true) => Prot::READ_EXECUTE,
        (true, false) => Prot::READ,
        (false, true) => Prot::new(true, true, true),
        (false, false) => Prot::READ_WRITE,
    };
    let backing = VmBacking::Page {
        pc: segment.payload.page_container.clone(),
        offset: 0,
    };
    let request = if rounded_addr == 0 {
        let window = UserRange::new_aligned(
            UserVirtAddr(USER_PAGE_SIZE),
            UserRange::full_user_v1().len() - USER_PAGE_SIZE,
        )
        .map_err(|_| Errno::EINVAL)?;
        VmMapRequest::anywhere(
            window,
            len / USER_PAGE_SIZE,
            prot,
            VmEntryFlags::SHARED,
            backing,
        )
    } else {
        let range =
            UserRange::new_aligned(UserVirtAddr(rounded_addr), len).map_err(|_| Errno::EINVAL)?;
        let placement = if (shmflg & SHM_REMAP) != 0 {
            MapPlacement::FixedReplace
        } else {
            MapPlacement::RequireFree
        };
        VmMapRequest::fixed(range, placement, prot, VmEntryFlags::SHARED, backing)
    };
    let outcome = aspace
        .mmap_script(request)
        .await
        .map_err(shm_vm_error_to_errno)?;
    segment.payload.attach_count.fetch_add(1, Ordering::AcqRel);
    segment.payload.attaches.lock().push(ShmAttach {
        aspace_key: aspace.key().raw(),
        addr: outcome.range.start().as_usize(),
        len: outcome.range.len(),
    });
    Ok(outcome.range.start().as_usize())
}

// ---------------------------------------------------------------------------
// step_shmdt
// ---------------------------------------------------------------------------

/// `shmdt(shmaddr)` — detach a shared memory segment.
///
/// Detach the mapping whose start address exactly matches `shmaddr`.
pub async fn step_shmdt(shmaddr: usize, aspace: &Cap<AddressSpace>) -> Result<(), Errno> {
    if !shmaddr.is_multiple_of(USER_PAGE_SIZE) {
        return Err(Errno::EINVAL);
    }
    let aspace_key = aspace.key().raw();
    let entry = aspace.lookup(UserVirtAddr(shmaddr)).ok_or(Errno::EINVAL)?;
    if entry.range.start().as_usize() != shmaddr || !entry.flags.shared {
        return Err(Errno::EINVAL);
    }
    let VmBacking::Page { pc: entry_pc, .. } = entry.backing else {
        return Err(Errno::EINVAL);
    };
    let mut found = None;
    for segment in structure::all_shm_segments() {
        if segment.payload.page_container != entry_pc {
            continue;
        }
        let mut attaches = segment.payload.attaches.lock();
        if let Some(pos) = attaches
            .iter()
            .position(|attach| attach.aspace_key == aspace_key && attach.addr == shmaddr)
        {
            found = Some((segment.clone(), attaches.remove(pos)));
            break;
        }
    }
    let Some((segment, attach)) = found else {
        return Err(Errno::EINVAL);
    };
    let range =
        UserRange::new_aligned(UserVirtAddr(attach.addr), attach.len).map_err(|_| Errno::EINVAL)?;
    if let Err(errno) = aspace
        .munmap_script(range)
        .await
        .map_err(shm_vm_error_to_errno)
    {
        segment.payload.attaches.lock().push(attach);
        return Err(errno);
    }
    decrement_attach_count_and_reclaim(&segment);
    Ok(())
}

/// Detach every SysV shm VMA recorded for an exiting address space.
///
/// This is the synchronous process-exit companion to `shmdt(2)`.
/// Process teardown is not currently an async script, so this uses
/// `AddressSpace::try_munmap` and reports any VM failures while still
/// dropping shm attach bookkeeping. The process is exiting and will
/// drop its `AddressSpace` cap immediately after this hook; the shm
/// segment's `shm_nattch` accounting must not retain dead process
/// attaches just because teardown could not wait on a range lock.
pub(crate) fn detach_all_for_aspace(aspace: &Cap<AddressSpace>) -> ShmDetachSweep {
    let aspace_key = aspace.key().raw();
    let mut sweep = ShmDetachSweep::default();
    for segment in structure::all_shm_segments() {
        let removed = {
            let mut attaches = segment.payload.attaches.lock();
            let mut removed = alloc::vec::Vec::new();
            let mut index = 0;
            while index < attaches.len() {
                if attaches[index].aspace_key == aspace_key {
                    removed.push(attaches.remove(index));
                } else {
                    index += 1;
                }
            }
            removed
        };

        for attach in removed {
            sweep.detached += 1;
            let range = match UserRange::new_aligned(UserVirtAddr(attach.addr), attach.len) {
                Ok(range) => range,
                Err(_) => {
                    sweep.vm_errors += 1;
                    decrement_attach_count_and_reclaim(&segment);
                    continue;
                }
            };
            if aspace.try_munmap(range).is_err() {
                sweep.vm_errors += 1;
            }
            decrement_attach_count_and_reclaim(&segment);
        }
    }
    sweep
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShmDetachSweep {
    pub detached: usize,
    pub vm_errors: usize,
}

fn shm_vm_error_to_errno(error: VmMapError) -> Errno {
    match error {
        VmMapError::AlreadyMapped
        | VmMapError::InvalidRange
        | VmMapError::MissingMapping
        | VmMapError::BackingOffsetOverflow => Errno::EINVAL,
        VmMapError::NoFreeRange | VmMapError::Private(_) => Errno::ENOMEM,
        VmMapError::WouldBlock => Errno::EAGAIN,
        VmMapError::Pmap(_) => Errno::EIO,
    }
}

fn decrement_attach_count_and_reclaim(segment: &Cap<structure::ShmSegmentIdentity>) {
    if segment.payload.attach_count.fetch_sub(1, Ordering::AcqRel) == 1 {
        structure::reclaim_shm_if_unattached(segment);
    }
}

// ---------------------------------------------------------------------------
// step_shmctl
// ---------------------------------------------------------------------------

/// `shmctl(shmid, cmd, buf)` — control a shared memory segment.
///
/// Supported commands: IPC_RMID, IPC_SET, IPC_STAT, IPC_INFO, SHM_INFO.
pub fn step_shmctl(
    shmid: u32,
    cmd: i32,
    set_fields: Option<(u16, u32, u32)>,
    cred: &Cap<Cred>,
) -> Result<ShmCtlResult, Errno> {
    match cmd {
        IPC_RMID => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_owner_or_admin(&segment, cred)?;

            if let Some(seg) = structure::mark_shm_removed(shmid) {
                structure::reclaim_shm_if_unattached(&seg);
            }
            // If the segment was already withdrawn (race), succeed
            // silently — Linux returns 0 for double IPC_RMID.
            Ok(ShmCtlResult::Success)
        }
        IPC_SET => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_owner_or_admin(&segment, cred)?;
            let Some((mode, uid, gid)) = set_fields else {
                return Err(Errno::EINVAL);
            };
            segment.mode.store(mode & 0o777, Ordering::Release);
            segment.uid.store(uid, Ordering::Release);
            segment.gid.store(gid, Ordering::Release);
            Ok(ShmCtlResult::Success)
        }
        IPC_STAT => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_can_read_shm(&segment, cred)?;
            Ok(ShmCtlResult::Stat(shm_info_from_segment(&segment, 0)))
        }
        SHM_STAT | SHM_STAT_ANY => {
            let segment = structure::lookup_shm_by_index(shmid).ok_or(Errno::EINVAL)?;
            if cmd == SHM_STAT {
                checks::require_can_read_shm(&segment, cred)?;
            }
            let shmid = segment.shmid as i64;
            Ok(ShmCtlResult::Stat(shm_info_from_segment(&segment, shmid)))
        }
        IPC_INFO => {
            // System-wide shm limit info. The per-namespace limits
            // table is not threaded into this step yet, so keep the
            // existing Linux-like defaults for the limit projection.
            Ok(ShmCtlResult::Info {
                return_value: structure::highest_shm_index(),
                shmmni: 4096,
                shmmax: u64::MAX,
                shmmin: 1,
                shmall: u64::MAX,
                shmseg: 4096,
            })
        }
        SHM_INFO => {
            let segments = structure::all_shm_segments();
            let used_ids = segments.len().try_into().unwrap_or(i32::MAX);
            let shm_tot = segments
                .iter()
                .map(|seg| seg.size.div_ceil(crate::vm::USER_PAGE_SIZE) as u64)
                .sum();
            Ok(ShmCtlResult::ShmInfo {
                return_value: structure::highest_shm_index(),
                used_ids,
                shm_tot,
                shm_rss: 0,
                shm_swp: 0,
                swap_attempts: 0,
                swap_successes: 0,
            })
        }
        _ => Err(Errno::EINVAL),
    }
}

/// Namespace-aware `shmctl` wrapper for syscall paths that can withdraw
/// keyed namespace bindings on `IPC_RMID`.
pub fn step_shmctl_in_ns(
    shmid: u32,
    cmd: i32,
    set_fields: Option<(u16, u32, u32)>,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<ShmCtlResult, Errno> {
    let key = if cmd == IPC_RMID {
        Some(checks::require_shm_exists(shmid)?.key)
    } else {
        None
    };
    let result = step_shmctl(shmid, cmd, set_fields, cred)?;
    if let Some(Some(key)) = key {
        let mut table = nsproxy.ipc_ns.sysv_shm.lock();
        if table.get(&key).map(|segment| segment.shmid) == Some(shmid) {
            table.remove(&key);
        }
    }
    Ok(result)
}

fn shm_info_from_segment(segment: &structure::ShmSegmentIdentity, return_value: i64) -> ShmInfo {
    ShmInfo {
        return_value,
        key: segment.key_raw(),
        shmid: segment.shmid,
        size: segment.size,
        perm: segment.perm(),
        uid: segment.uid(),
        gid: segment.gid(),
        cuid: segment.cuid,
        cgid: segment.cgid,
        attach_count: segment.payload.attach_count.load(Ordering::Acquire),
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of a shmctl operation.
#[derive(Clone, Debug)]
pub enum ShmCtlResult {
    /// IPC_RMID, IPC_SET — no return data.
    Success,
    /// IPC_STAT — segment info for userspace shmid_ds copy.
    Stat(ShmInfo),
    /// IPC_INFO — system-wide shm limits.
    Info {
        return_value: i64,
        shmmni: u64,
        shmmax: u64,
        shmmin: u64,
        shmall: u64,
        shmseg: u64,
    },
    /// SHM_INFO — current shared-memory usage counters.
    ShmInfo {
        return_value: i64,
        used_ids: i32,
        shm_tot: u64,
        shm_rss: u64,
        shm_swp: u64,
        swap_attempts: u64,
        swap_successes: u64,
    },
}

/// IPC_STAT return data — maps to `struct shmid_ds`.
#[derive(Clone, Debug)]
pub struct ShmInfo {
    pub return_value: i64,
    pub key: i32,
    pub shmid: u32,
    pub size: usize,
    pub perm: IpcPerm,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub attach_count: u32,
}
