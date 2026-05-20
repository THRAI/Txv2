//! SysV shared memory step operations.
//!
//! Phase IPC-1. Zero new WaitSource — the only blocking operation
//! is page-fault-on-attach, already handled by the existing VM fault
//! path. All four step functions are immediate (synchronous).

use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_shm::checks;
use crate::ipc::sysv_shm::structure::{self, IpcPerm};
use crate::process::adapter::step_engine::Cap;
use crate::process::nsproxy::SysvKey;

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

/// shmat flags.
pub const SHM_RDONLY: i32 = 0o10000;
pub const SHM_REMAP: i32 = 0o4000;

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
        if let Some(&existing_shmid) = table.get(k) {
            drop(table);
            let seg = checks::require_shm_exists(existing_shmid)?;
            if exclusive {
                return Err(Errno::EEXIST);
            }
            // Verify size: if segment exists, size must be <= existing size.
            if size > seg.size {
                return Err(Errno::EINVAL);
            }
            // Check permissions.
            checks::require_can_read_shm(&seg, cred)?;
            return Ok(existing_shmid);
        }
    }

    // Must create.
    if !create && ipc_key.is_some() {
        return Err(Errno::ENOENT);
    }

    // Allocate the segment.
    let shmid =
        structure::register_shm(ipc_key, cred.clone(), size, perm, cred.euid.0, cred.egid.0)
            .map_err(|_| Errno::ENOMEM)?;

    // Register in the namespace's key→shmid table.
    if let Some(ref k) = ipc_key {
        nsproxy.ipc_ns.sysv_shm.lock().insert(*k, shmid);
    }

    Ok(shmid)
}

// ---------------------------------------------------------------------------
// step_shmat
// ---------------------------------------------------------------------------

/// `shmat(shmid, shmaddr, shmflg)` — attach a shared memory segment.
///
/// Returns the virtual address where the segment was mapped.
/// Day-1: returns 0 (stub) — page-backed materialization is deferred
/// to the VM fault path integration.
pub fn step_shmat(
    shmid: u32,
    _shmaddr: usize,
    shmflg: i32,
    cred: &Cap<Cred>,
) -> Result<usize, Errno> {
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

    // TODO(txdoc:IPC-V1-SHM-1): map pages into caller's address space.
    // For IPC-1 stub, return a placeholder address. The real
    // implementation allocates VMA entries via AddressSpace and
    // materializes pages through the existing VM fault path.
    //
    // The current shape:
    // 1. Reserve VMA region in caller's AddressSpace
    // 2. Create PageContainer for the segment (or reuse existing)
    // 3. Install VMA pointing at PageContainer's RNodeBacking
    // 4. Return the start address of the VMA

    let _ = readonly;
    Ok(0) // stub — real address returned after VMA integration
}

// ---------------------------------------------------------------------------
// step_shmdt
// ---------------------------------------------------------------------------

/// `shmdt(shmaddr)` — detach a shared memory segment.
///
/// Day-1: no-op stub. Real impl walks the caller's VMAs, finds the
/// one at `shmaddr`, unmaps it, and decrements the segment's
/// attach_count.
pub fn step_shmdt(_shmaddr: usize) -> Result<(), Errno> {
    // TODO(txdoc:IPC-V1-SHM-1): unmap VMA, decrement attach_count,
    // reclaim segment if destroyed && attach_count==0.
    Ok(())
}

// ---------------------------------------------------------------------------
// step_shmctl
// ---------------------------------------------------------------------------

/// `shmctl(shmid, cmd, buf)` — control a shared memory segment.
///
/// Supported commands: IPC_RMID, IPC_SET, IPC_STAT, IPC_INFO.
pub fn step_shmctl(shmid: u32, cmd: i32, cred: &Cap<Cred>) -> Result<ShmCtlResult, Errno> {
    match cmd {
        IPC_RMID => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_owner_or_admin(&segment, cred)?;

            // Withdraw from the namespace key table.
            // TODO: the key table withdrawal happens via the nsproxy
            // in the syscall arm. This function only marks destroyed.

            // Withdraw from global table and mark destroyed.
            if let Some(seg) = structure::withdraw_shm(shmid) {
                seg.destroyed.store(true, Ordering::Release);
                // The cap is dropped here; if attach_count == 0, EBR
                // will clean up. If attaches remain, the payload is
                // held alive by the attachers' VMAs.
            }
            // If the segment was already withdrawn (race), succeed
            // silently — Linux returns 0 for double IPC_RMID.
            Ok(ShmCtlResult::Success)
        }
        IPC_SET => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_owner_or_admin(&segment, cred)?;
            // TODO(txdoc:IPC-V1-SHM-1): copy shmid_ds from userspace,
            // update perm, uid, gid fields on the identity.
            let _ = segment;
            Err(Errno::ENOSYS)
        }
        IPC_STAT => {
            let segment = checks::require_shm_exists(shmid)?;
            checks::require_can_read_shm(&segment, cred)?;
            Ok(ShmCtlResult::Stat(ShmInfo {
                shmid: segment.shmid,
                size: segment.size,
                perm: segment.perm,
                cuid: segment.cuid,
                cgid: segment.cgid,
                attach_count: 0, // TODO: read from payload
            }))
        }
        IPC_INFO => {
            // System-wide shm info — return limit values.
            // For now, return placeholder; the real impl reads
            // from IpcLimits.
            Ok(ShmCtlResult::Info {
                shmmni: 4096,
                shmmax: u64::MAX,
                shmmin: 1,
                shmall: u64::MAX,
                shmseg: 4096,
            })
        }
        _ => Err(Errno::EINVAL),
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
        shmmni: u64,
        shmmax: u64,
        shmmin: u64,
        shmall: u64,
        shmseg: u64,
    },
}

/// IPC_STAT return data — maps to `struct shmid_ds`.
#[derive(Clone, Debug)]
pub struct ShmInfo {
    pub shmid: u32,
    pub size: usize,
    pub perm: IpcPerm,
    pub cuid: u32,
    pub cgid: u32,
    pub attach_count: u32,
}
