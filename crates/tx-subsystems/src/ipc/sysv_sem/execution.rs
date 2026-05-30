//! SysV semaphore step operations.
//!
//! Phase IPC-3. Multi-op atomic semop, SEM_UNDO tracking, changed_seq
//! sequencing, and `changed_channel` wake for blocked semop waiters.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::cred::Cred;
use crate::execution::Errno;
use crate::ipc::sysv_sem::checks;
use crate::ipc::sysv_sem::notification;
use crate::ipc::sysv_sem::structure::{self, sem_flg, SemBuf};
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL, IPC_PRIVATE};
use crate::ipc::sysv_shm::structure::IpcPerm;
use crate::process::adapter::step_engine::{
    Cap, InterestMask, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity, WaitSourceId,
    YieldShape,
};
use crate::process::nsproxy::SysvKey;
use crate::process::structure::ProcessIdentity;
use tx_substrate::step::{Errno as StepErrno, ResumeOutcome};

const SEM_CHANGED_MASK: u64 = 1;

fn fire_sem_changed(payload: &structure::SemArrayPayload) {
    payload.changed_seq.fetch_add(1, Ordering::Release);
    notification::notify_changed(&payload.changed_channel, &payload.changed_wait_source);
}

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

pub type SemopOutcome = StepOutcome<usize, NoProgress>;

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
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
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
    // observe/upgrade/reserve/commit/publish are delegated to the v3 op.
    // This legacy wrapper cannot drive waits, so it preserves the old
    // non-blocking Result shape by surfacing a yielded wait as EAGAIN.
    match step_semop_v3(semid, sops, cred, process) {
        StepOutcome::Done(applied) => Ok(applied),
        StepOutcome::Err(errno) => Err(errno.into()),
        StepOutcome::Yield { .. } => Err(Errno::EAGAIN),
        StepOutcome::Continue { .. } => Err(Errno::EIO),
    }
}

pub fn step_semop_v3(
    semid: u32,
    sops: &[SemBuf],
    cred: &Cap<Cred>,
    process: &Cap<ProcessIdentity>,
) -> SemopOutcome {
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    if sops.is_empty() {
        return StepOutcome::err(Errno::EINVAL.into());
    }

    let array = match checks::require_sem_exists(semid) {
        Ok(array) => array,
        Err(errno) => return StepOutcome::err(errno.into()),
    };
    if array.destroyed.load(Ordering::Acquire) {
        return StepOutcome::err(Errno::EIDRM.into());
    }
    if let Err(errno) = checks::require_can_write_sem(&array, cred) {
        return StepOutcome::err(errno.into());
    }

    let nowait = sops.iter().any(|s| (s.sem_flg & sem_flg::IPC_NOWAIT) != 0);

    let payload = match array.payload.lock().as_ref().cloned() {
        Some(payload) => payload,
        None => return StepOutcome::err(Errno::EIDRM.into()),
    };

    {
        let mut values = payload.values.lock();

        // Validate all ops can fit.
        for s in sops {
            if s.sem_num >= array.nsems {
                return StepOutcome::err(Errno::EFBIG.into());
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
                return StepOutcome::err(Errno::EAGAIN.into());
            }
            return notification::wait_for_change(payload.changed_source_id);
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
                .ok_or(Errno::ESRCH);
            let proc_payload = match proc_payload {
                Ok(proc_payload) => proc_payload,
                Err(errno) => return StepOutcome::err(errno.into()),
            };
            proc_payload.record_sem_undo(semid, undo_adjustments);
        }

        fire_sem_changed(&payload);
    }

    StepOutcome::done(sops.len())
}

pub struct SemopWaitOp<'a> {
    pub semid: u32,
    pub sops: &'a [SemBuf],
    pub cred: &'a Cap<Cred>,
    pub process: &'a Cap<ProcessIdentity>,
    pub tid: Option<u32>,
    watched_array: Option<Cap<structure::SemArrayIdentity>>,
    waiting: bool,
}

impl<'a> SemopWaitOp<'a> {
    pub fn new(
        semid: u32,
        sops: &'a [SemBuf],
        cred: &'a Cap<Cred>,
        process: &'a Cap<ProcessIdentity>,
        tid: Option<u32>,
    ) -> Self {
        Self {
            semid,
            sops,
            cred,
            process,
            tid,
            watched_array: None,
            waiting: false,
        }
    }

    fn changed_source(&mut self) -> Result<u64, Errno> {
        let array = if let Some(array) = self.watched_array.as_ref() {
            array.clone()
        } else {
            let array = checks::require_sem_exists(self.semid)?;
            self.watched_array = Some(array.clone());
            array
        };
        if array.destroyed.load(Ordering::Acquire) {
            return Err(Errno::EIDRM);
        }
        let payload = array.payload.lock().as_ref().cloned().ok_or(Errno::EIDRM)?;
        Ok(payload.changed_source_id)
    }
}

impl<I: SubjectIdentity> StepOp<I> for SemopWaitOp<'_> {
    type Output = usize;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match step_semop(self.semid, self.sops, self.cred, self.process) {
            Ok(applied) => StepOutcome::Done(applied),
            Err(Errno::EAGAIN) => match self.changed_source() {
                Ok(source_id) => {
                    if !self.waiting {
                        crate::futex::register_waiting_tid(self.tid);
                    }
                    self.waiting = true;
                    StepOutcome::Yield {
                        progress: NoProgress,
                        shape: YieldShape::OnWaitSource {
                            source: WaitSourceId::new(source_id),
                            interests: InterestMask::new(SEM_CHANGED_MASK),
                        },
                    }
                }
                Err(errno) => StepOutcome::Err(errno.into()),
            },
            Err(Errno::EINVAL)
                if self
                    .watched_array
                    .as_ref()
                    .is_some_and(|array| array.destroyed.load(Ordering::Acquire)) =>
            {
                StepOutcome::Err(Errno::EIDRM.into())
            }
            Err(errno) => StepOutcome::Err(errno.into()),
        }
    }

    fn apply_resume(&mut self, resume: ResumeOutcome) -> Result<(), StepErrno> {
        if matches!(resume, ResumeOutcome::Retry) {
            if self.waiting {
                crate::futex::unregister_waiting_tid(self.tid);
            }
            self.waiting = false;
            Ok(())
        } else {
            Err(StepErrno::EINVAL)
        }
    }
}

impl Drop for SemopWaitOp<'_> {
    fn drop(&mut self) {
        if self.waiting {
            crate::futex::unregister_waiting_tid(self.tid);
            self.waiting = false;
        }
    }
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
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
    match cmd {
        IPC_RMID => {
            let array = checks::require_sem_exists(semid)?;
            checks::require_owner_or_admin(&array, cred)?;
            if let Some(a) = structure::withdraw_sem(semid) {
                a.destroyed.store(true, Ordering::Release);
                if let Some(payload) = a.payload.lock().as_ref().cloned() {
                    fire_sem_changed(&payload);
                }
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
                fire_sem_changed(payload);
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
                fire_sem_changed(payload);
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
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
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
    // observe: inspect current subsystem state and validate inputs.
    // upgrade: acquire capabilities/guards needed for mutation.
    // reserve: reserve namespace, memory, or wait-source effects.
    // commit: apply the state transition.
    // publish: emit readiness, signal, or observable outcome.
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
        fire_sem_changed(&payload_guard);
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
