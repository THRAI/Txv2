//! SysV semaphore step operations.
//!
//! Phase IPC-3. Multi-op atomic semop, SEM_UNDO tracking, changed_seq
//! sequencing, and `changed_channel` wake for blocked semop waiters.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_sem::checks;
use crate::ipc::sysv_sem::structure::{self, sem_flg, SemBuf};
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL, IPC_PRIVATE};
use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::Cap;
use crate::process::adapter::wait_routing::Mask;
use crate::process::nsproxy::SysvKey;
use crate::process::structure::ProcessIdentity;

// semctl commands
pub use crate::ipc::sysv_shm::execution::{IPC_INFO, IPC_RMID, IPC_SET, IPC_STAT};
pub const SEM_STAT: i32 = 18;
pub const GETALL: i32 = 13;
pub const GETNCNT: i32 = 14;
pub const GETPID: i32 = 11;
pub const GETVAL: i32 = 12;
pub const GETZCNT: i32 = 15;
pub const SETALL: i32 = 17;
pub const SETVAL: i32 = 16;
pub const SEM_INFO: i32 = 19;
pub const SEM_STAT_ANY: i32 = 20;

macro_rules! with_payload {
    ($array:expr, $guard:ident, $block:block) => {{
        let payload_guard = $array.payload.lock();
        let $guard = payload_guard.as_ref().ok_or(Errno::EIDRM)?;
        $block
    }};
}

// ---------------------------------------------------------------------------
// step_semget
// ---------------------------------------------------------------------------

pub fn step_semget(
    key: i32,
    nsems: u16,
    semflg: i32,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
) -> Result<u32, Errno> {
    let ipc_key = if key == IPC_PRIVATE {
        None
    } else {
        Some(SysvKey::new(key as u32))
    };

    let create = (semflg & IPC_CREAT) != 0;
    let exclusive = (semflg & IPC_EXCL) != 0;
    let perm = IpcPerm::new((semflg & 0o777) as u16);

    if nsems == 0 || nsems > nsproxy.ipc_ns.limits.lock().semmsl as u16 {
        return Err(Errno::EINVAL);
    }

    if let Some(ref k) = ipc_key {
        let table = nsproxy.ipc_ns.sysv_sem.lock();
        if let Some(a) = table.get(k).cloned() {
            if exclusive {
                return Err(Errno::EEXIST);
            }
            if nsems > a.nsems {
                return Err(Errno::EINVAL);
            }
            checks::require_can_read_sem(&a, cred)?;
            return Ok(a.semid);
        }
    }

    if !create && ipc_key.is_some() {
        return Err(Errno::ENOENT);
    }

    let array =
        structure::register_sem(ipc_key, cred.clone(), nsems, perm, cred.euid.0, cred.egid.0)
            .map_err(|_| Errno::ENOMEM)?;

    if let Some(ref k) = ipc_key {
        nsproxy.ipc_ns.sysv_sem.lock().insert(*k, array.clone());
    }

    Ok(array.semid)
}

// ---------------------------------------------------------------------------
// step_semop
// ---------------------------------------------------------------------------

/// `semop(semid, sops, nsops)` — apply an array of operations atomically.
///
/// Returns the number of ops applied (always nsops on success).
/// IPC_NOWAIT on any op makes the entire call non-blocking.
/// SEM_UNDO on any op records the inverse adjustment.
pub fn step_semop(
    semid: u32,
    sops: &[SemBuf],
    cred: &Cap<Cred>,
    process: &Cap<ProcessIdentity>,
) -> Result<usize, Errno> {
    if sops.is_empty() {
        return Err(Errno::EINVAL);
    }

    let array = checks::require_sem_exists(semid)?;
    if array.destroyed.load(Ordering::Acquire) {
        return Err(Errno::EIDRM);
    }
    checks::require_can_write_sem(&array, cred)?;

    let nowait = sops.iter().any(|s| (s.sem_flg & sem_flg::IPC_NOWAIT) != 0);

    with_payload!(array, payload, {
        let mut values = payload.values.lock();

        // Validate all ops can fit.
        for s in sops {
            if s.sem_num >= array.nsems {
                return Err(Errno::EFBIG);
            }
        }

        // Check if any op would block.
        let mut would_block = false;
        for s in sops {
            let val = values[s.sem_num as usize].val;
            if s.sem_op == 0 {
                if val != 0 {
                    would_block = true;
                    break;
                }
            } else if s.sem_op < 0 && val < -s.sem_op {
                would_block = true;
                break;
            }
        }

        if would_block {
            if nowait {
                return Err(Errno::EAGAIN);
            }
            // TODO: blocking semop — yield OnWaitSource with changed_channel
            return Err(Errno::EAGAIN);
        }

        // Apply all ops and track SEM_UNDO.
        let mut undo_adjustments: Vec<i16> = alloc::vec![0i16; array.nsems as usize];
        let mut has_undo = false;

        for s in sops {
            let v = &mut values[s.sem_num as usize].val;
            if s.sem_op > 0 {
                *v = v.saturating_add(s.sem_op);
            } else if s.sem_op < 0 {
                *v = v.saturating_sub(-s.sem_op);
            }
            values[s.sem_num as usize].last_pid = process.pid.0;
            // sem_op == 0: already validated above, no change needed.

            if (s.sem_flg & sem_flg::SEM_UNDO) != 0 {
                undo_adjustments[s.sem_num as usize] =
                    undo_adjustments[s.sem_num as usize].wrapping_sub(s.sem_op);
                has_undo = true;
            }
        }

        // Record SEM_UNDO if any op had it.
        if has_undo {
            let proc_payload = process
                .payload_slot()
                .lock()
                .as_ref()
                .cloned()
                .ok_or(Errno::ESRCH)?;
            proc_payload.record_sem_undo(semid, undo_adjustments);
        }

        // Changed seq bump + wake.
        payload.changed_seq.fetch_add(1, Ordering::Release);
        payload.changed_channel.fire(Mask::from_bits(1));
    });

    Ok(sops.len())
}

// ---------------------------------------------------------------------------
// step_semctl
// ---------------------------------------------------------------------------

pub fn step_semctl(
    semid: u32,
    semnum: u16,
    cmd: i32,
    arg: SemCtlArg,
    cred: &Cap<Cred>,
    process: Option<&Cap<ProcessIdentity>>,
) -> Result<SemCtlResult, Errno> {
    match cmd {
        IPC_RMID => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_owner_or_admin(&array, cred)?;
            if let Some(a) = structure::withdraw_sem(semid) {
                a.destroyed.store(true, Ordering::Release);
            }
            Ok(SemCtlResult::Success)
        }
        IPC_SET => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_owner_or_admin(&array, cred)?;
            let SemCtlArg::IpcSet { mode, uid, gid } = arg else {
                return Err(Errno::EINVAL);
            };
            array.mode.store(mode & 0o777, Ordering::Release);
            array.uid.store(uid, Ordering::Release);
            array.gid.store(gid, Ordering::Release);
            Ok(SemCtlResult::Success)
        }
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_read_sem(&array, cred)?;
            Ok(SemCtlResult::Stat(SemInfo {
                key: array.key_raw(),
                semid: array.semid,
                nsems: array.nsems,
                uid: array.uid(),
                gid: array.gid(),
                cuid: array.cuid,
                cgid: array.cgid,
                perm: array.perm(),
            }))
        }
        GETVAL => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_read_sem(&array, cred)?;
            if semnum >= array.nsems {
                return Err(Errno::EINVAL);
            }
            with_payload!(array, payload, {
                let values = payload.values.lock();
                Ok(SemCtlResult::Val(values[semnum as usize].val as i32))
            })
        }
        SETVAL => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_write_sem(&array, cred)?;
            if semnum >= array.nsems {
                return Err(Errno::EINVAL);
            }
            let SemCtlArg::Val(setval) = arg else {
                return Err(Errno::EINVAL);
            };
            if !(0..=32767).contains(&setval) {
                return Err(Errno::ERANGE);
            }
            with_payload!(array, payload, {
                let mut values = payload.values.lock();
                values[semnum as usize].val = setval as i16;
                if let Some(process) = process {
                    values[semnum as usize].last_pid = process.pid.0;
                }
                payload.changed_seq.fetch_add(1, Ordering::Release);
                payload.changed_channel.fire(Mask::from_bits(1));
            });
            Ok(SemCtlResult::Success)
        }
        GETALL => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_read_sem(&array, cred)?;
            with_payload!(array, payload, {
                let values = payload.values.lock();
                Ok(SemCtlResult::All(
                    values.iter().map(|value| value.val as u16).collect(),
                ))
            })
        }
        SETALL => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_write_sem(&array, cred)?;
            let SemCtlArg::All(set_values) = arg else {
                return Err(Errno::EINVAL);
            };
            if set_values.len() != array.nsems as usize
                || set_values.iter().any(|value| *value > 32767)
            {
                return Err(Errno::ERANGE);
            }
            with_payload!(array, payload, {
                let mut values = payload.values.lock();
                for (slot, set_value) in values.iter_mut().zip(set_values.iter()) {
                    slot.val = *set_value as i16;
                    if let Some(process) = process {
                        slot.last_pid = process.pid.0;
                    }
                }
                payload.changed_seq.fetch_add(1, Ordering::Release);
                payload.changed_channel.fire(Mask::from_bits(1));
            });
            Ok(SemCtlResult::Success)
        }
        GETPID => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_can_read_sem(&array, cred)?;
            if semnum >= array.nsems {
                return Err(Errno::EINVAL);
            }
            with_payload!(array, payload, {
                let values = payload.values.lock();
                Ok(SemCtlResult::Val(values[semnum as usize].last_pid as i32))
            })
        }
        GETNCNT | GETZCNT => {
            // Count queries — return 0 stubs.
            Ok(SemCtlResult::Val(0))
        }
        IPC_INFO | SEM_INFO => Ok(SemCtlResult::Info {
            semmni: 32000,
            semmns: 1024000000,
            semmsl: 32000,
            semopm: 500,
        }),
        _ => Err(Errno::EINVAL),
    }
}

/// Namespace-aware `semctl` wrapper for syscall paths that can withdraw
/// keyed namespace bindings on `IPC_RMID`.
pub fn step_semctl_in_ns(
    semid: u32,
    semnum: u16,
    cmd: i32,
    arg: SemCtlArg,
    cred: &Cap<Cred>,
    nsproxy: &Cap<crate::process::nsproxy::NsProxy>,
    process: Option<&Cap<ProcessIdentity>>,
) -> Result<SemCtlResult, Errno> {
    let key = if cmd == IPC_RMID {
        Some(checks::require_sem_exists(semid)?.key)
    } else {
        None
    };
    let result = step_semctl(semid, semnum, cmd, arg, cred, process)?;
    if let Some(Some(key)) = key {
        let mut table = nsproxy.ipc_ns.sysv_sem.lock();
        if table.get(&key).map(|array| array.semid) == Some(semid) {
            table.remove(&key);
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// step_sem_undo — called from process exit
// ---------------------------------------------------------------------------

/// Walk all SEM_UNDO entries for `process` and reverse the adjustments.
/// Called from `step_process_exit` before the process payload is torn down.
pub fn step_sem_undo(process: &Cap<ProcessIdentity>) {
    let Some(proc_payload) = process.payload_slot().lock().as_ref().cloned() else {
        return;
    };
    for undo in proc_payload.drain_sem_undos().into_values() {
        let Some(array) = structure::lookup_sem(undo.semid) else {
            continue;
        };
        let Some(payload_guard) = array.payload.lock().as_ref().cloned() else {
            continue;
        };
        let mut values = payload_guard.values.lock();
        for (i, adj) in undo.adjustments.iter().enumerate() {
            if *adj != 0 && i < values.len() {
                values[i].val = values[i].val.wrapping_add(*adj);
            }
        }
        payload_guard.changed_seq.fetch_add(1, Ordering::Release);
        payload_guard.changed_channel.fire(Mask::from_bits(1));
    }
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum SemCtlResult {
    Success,
    Val(i32),
    All(Vec<u16>),
    Stat(SemInfo),
    Info {
        semmni: u64,
        semmns: u64,
        semmsl: u64,
        semopm: u64,
    },
}

#[derive(Clone, Debug)]
pub struct SemInfo {
    pub key: i32,
    pub semid: u32,
    pub nsems: u16,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub perm: IpcPerm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemCtlArg {
    None,
    Val(i32),
    All(Vec<u16>),
    IpcSet { mode: u16, uid: u32, gid: u32 },
}
