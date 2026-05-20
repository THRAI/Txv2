//! SysV / POSIX IPC syscall arms — Phase IPC-5.

use super::{errno_to_i32, SyscallCtx, SyscallResult, ENOSYS_VALUE};
use tx_subsystems::execution::Errno;
use tx_subsystems::ipc;

fn nsproxy_and_cred(
    ctx: &SyscallCtx<'_>,
) -> Result<
    (
        tx_subsystems::process::adapter::step_engine::Cap<tx_subsystems::process::nsproxy::NsProxy>,
        tx_subsystems::process::adapter::step_engine::Cap<tx_subsystems::cred::Cred>,
    ),
    Errno,
> {
    let nsproxy = ctx.process.nsproxy_cap().ok_or(Errno::ESRCH)?;
    let cred = ctx.cred_cap();
    Ok((nsproxy, cred))
}

pub(super) fn sys_shmget(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let key = args[0] as i32;
    let size = args[1] as usize;
    let shmflg = args[2] as i32;
    match ipc::sysv_shm::execution::step_shmget(key, size, shmflg, &cred, &ns) {
        Ok(shmid) => SyscallResult::Return(shmid as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_shmat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cred = ctx.cred_cap();
    match ipc::sysv_shm::execution::step_shmat(
        args[0] as u32,
        args[1] as usize,
        args[2] as i32,
        &cred,
    ) {
        Ok(addr) => SyscallResult::Return(addr as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_shmdt(args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    match ipc::sysv_shm::execution::step_shmdt(args[0] as usize) {
        Ok(()) => SyscallResult::Return(0i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_shmctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cred = ctx.cred_cap();
    match ipc::sysv_shm::execution::step_shmctl(args[0] as u32, args[1] as i32, &cred) {
        Ok(result) => match result {
            ipc::sysv_shm::execution::ShmCtlResult::Success => SyscallResult::Return(0i64),
            _ => SyscallResult::Return(0i64),
        },
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_msgget(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    match ipc::sysv_msg::execution::step_msgget(args[0] as i32, args[1] as i32, &cred, &ns) {
        Ok(msqid) => SyscallResult::Return(msqid as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_msgctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cred = ctx.cred_cap();
    match ipc::sysv_msg::execution::step_msgctl(args[0] as u32, args[1] as i32, &cred) {
        Ok(result) => match result {
            ipc::sysv_msg::execution::MsgCtlResult::Success => SyscallResult::Return(0i64),
            _ => SyscallResult::Return(0i64),
        },
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_semget(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    match ipc::sysv_sem::execution::step_semget(
        args[0] as i32,
        args[1] as u16,
        args[2] as i32,
        &cred,
        &ns,
    ) {
        Ok(semid) => SyscallResult::Return(semid as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_semop(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let semid = args[0] as u32;
    let _sops_ptr = args[1];
    let nsops = args[2] as usize;
    if nsops == 0 || nsops > 500 {
        return SyscallResult::Error(22);
    }
    // TODO(txdoc:IPC-V1-SEM-1): copy_from_user sembuf array.
    // Direct dereference page-faults because userspace stack pages
    // are not identity-mapped in the kernel half.
    let _ = (semid, _sops_ptr, nsops, cred);
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_semctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    match ipc::sysv_sem::execution::step_semctl(
        args[0] as u32,
        args[1] as u16,
        args[2] as i32,
        args[3] as i32,
        &cred,
    ) {
        Ok(result) => match result {
            ipc::sysv_sem::execution::SemCtlResult::Success => SyscallResult::Return(0i64),
            ipc::sysv_sem::execution::SemCtlResult::Val(v) => SyscallResult::Return(v as i64),
            _ => SyscallResult::Return(0i64),
        },
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

// TODO(txdoc:IPC-V1-MSG-1): msgsnd/msgrcv need copy_from_user for message buffers.
pub(super) fn sys_msgsnd(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_msgrcv(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_mq_open(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_mq_unlink(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}
