//! SysV / POSIX IPC syscall arms — Phase IPC-5.

use super::{
    bootstrap_copy_from_user, bootstrap_copy_to_user, bootstrap_read_user, bootstrap_write_user,
    errno_to_i32, read_user_cstr, SyscallCtx, SyscallResult, EBADF_VALUE, EFAULT_VALUE,
    EINTR_VALUE, EINVAL_VALUE, ENAMETOOLONG_VALUE, ENOENT_VALUE, ENOMEM_VALUE, ENOSYS_VALUE,
    O_ACCMODE, O_CLOEXEC, O_CREAT, O_EXCL, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY,
};
use crate::adapter::step_engine::{InterestMask, WaitSourceId};
use alloc::vec::Vec;
use tx_hal::TimeIf;
use tx_scripts::drive;
use tx_substrate::step::{Deadline, DriveMode};
use tx_subsystems::execution::{Errno, WaitToken};
use tx_subsystems::ipc;
use tx_subsystems::ipc::posix_mq::structure::MqNotification;
use tx_subsystems::ipc::sysv_sem::structure::SemBuf;
use tx_subsystems::signal::Signum;
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;

use super::time::TimespecLayout;

const EMSGSIZE_VALUE: i32 = 90;
const MQ_NAME_MAX: usize = 255;
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IpcPermLayout {
    pub key: i32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub mode: u32,
    pub __seq: i32,
    pub __pad1: i64,
    pub __pad2: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmidDsLayout {
    pub shm_perm: IpcPermLayout,
    pub shm_segsz: u64,
    pub shm_atime: i64,
    pub shm_dtime: i64,
    pub shm_ctime: i64,
    pub shm_cpid: i32,
    pub shm_lpid: i32,
    pub shm_nattch: u64,
    pub __pad1: u64,
    pub __pad2: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShminfoLayout {
    pub shmmax: u64,
    pub shmmin: u64,
    pub shmmni: u64,
    pub shmseg: u64,
    pub shmall: u64,
    pub __unused: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShmInfoLayout {
    pub used_ids: i32,
    pub __pad: i32,
    pub shm_tot: u64,
    pub shm_rss: u64,
    pub shm_swp: u64,
    pub swap_attempts: u64,
    pub swap_successes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsqidDsLayout {
    pub msg_perm: IpcPermLayout,
    pub msg_stime: i64,
    pub msg_rtime: i64,
    pub msg_ctime: i64,
    pub msg_cbytes: u64,
    pub msg_qnum: u64,
    pub msg_qbytes: u64,
    pub msg_lspid: i32,
    pub msg_lrpid: i32,
    pub __unused: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsginfoLayout {
    pub msgpool: i32,
    pub msgmap: i32,
    pub msgmax: i32,
    pub msgmnb: i32,
    pub msgmni: i32,
    pub msgssz: i32,
    pub msgtql: i32,
    pub msgseg: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SemidDsLayout {
    pub sem_perm: IpcPermLayout,
    pub sem_otime: i64,
    pub sem_ctime: i64,
    pub sem_nsems: u16,
    pub __sem_nsems_pad: [u8; 6],
    pub __unused3: i64,
    pub __unused4: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SeminfoLayout {
    pub semmap: i32,
    pub semmni: i32,
    pub semmns: i32,
    pub semmnu: i32,
    pub semmsl: i32,
    pub semopm: i32,
    pub semume: i32,
    pub semusz: i32,
    pub semvmx: i32,
    pub semaem: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SembufLayout {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MqAttrLayout {
    pub mq_flags: i64,
    pub mq_maxmsg: i64,
    pub mq_msgsize: i64,
    pub mq_curmsgs: i64,
    pub __unused: [i64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SigeventPrefixLayout {
    pub(super) sigval: u64,
    pub(super) sigev_signo: i32,
    pub(super) sigev_notify: i32,
}

fn ipc_perm_layout(key: i32, uid: u32, gid: u32, cuid: u32, cgid: u32, mode: u16) -> IpcPermLayout {
    IpcPermLayout {
        key,
        uid,
        gid,
        cuid,
        cgid,
        mode: mode as u32,
        __seq: 0,
        __pad1: 0,
        __pad2: 0,
    }
}

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

fn read_mq_name(ctx: &SyscallCtx<'_>, name_ptr: u64) -> Result<Vec<u8>, SyscallResult> {
    if name_ptr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    let name = match read_user_cstr(&ctx.aspace, name_ptr, MQ_NAME_MAX + 1) {
        Ok(name) => name,
        Err(_) => return Err(SyscallResult::Error(ENAMETOOLONG_VALUE)),
    };
    if name.is_empty() || name.contains(&b'/') {
        return Err(SyscallResult::Error(ENOENT_VALUE));
    }
    Ok(name)
}

fn mq_access_flags(oflag: i32) -> Option<(bool, bool)> {
    match oflag & O_ACCMODE as i32 {
        x if x == O_RDONLY as i32 => Some((true, false)),
        x if x == O_WRONLY as i32 => Some((false, true)),
        x if x == O_RDWR as i32 => Some((true, true)),
        _ => None,
    }
}

fn mq_open_file_flags(oflag: i32) -> Result<OpenFileFlags, SyscallResult> {
    let (read, write) = mq_access_flags(oflag).ok_or(SyscallResult::Error(EINVAL_VALUE))?;
    Ok(OpenFileFlags {
        read,
        write,
        append: false,
        cloexec: (oflag & O_CLOEXEC as i32) != 0,
        nonblocking: (oflag & O_NONBLOCK as i32) != 0,
    })
}

fn mq_attr_to_layout(attr: ipc::posix_mq::execution::MqAttr) -> MqAttrLayout {
    MqAttrLayout {
        mq_flags: attr.flags,
        mq_maxmsg: attr.maxmsg,
        mq_msgsize: attr.msgsize,
        mq_curmsgs: attr.curmsgs,
        __unused: [0; 4],
    }
}

fn validate_abs_timeout(ctx: &SyscallCtx<'_>, timeout_ptr: u64) -> Result<(), SyscallResult> {
    if timeout_ptr == 0 {
        return Ok(());
    }
    let ts: TimespecLayout = match bootstrap_read_user(&ctx.aspace, timeout_ptr) {
        Ok(v) => v,
        Err(errno) => return Err(SyscallResult::Error(errno_to_i32(errno))),
    };
    if !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MqWakeReason {
    WaitSource,
    ProcessTimer,
}

async fn wait_for_mq_readiness<P: TimeIf>(
    ctx: &SyscallCtx<'_>,
    mq: &ipc::posix_mq::structure::PosixMqInstance,
    write: bool,
) -> MqWakeReason {
    let Ok(info) = ipc::posix_mq::execution::step_mq_poll_info(mq) else {
        return MqWakeReason::WaitSource;
    };
    let source_id = if write {
        info.write_source_id
    } else {
        info.read_source_id
    };
    let source = WaitSourceId::new(source_id);
    let interests = InterestMask::new(1);
    let Some(process_timer_deadline) = ctx.process.next_process_timer_deadline_ns() else {
        super::await_wait_source(ctx, source, interests).await;
        return MqWakeReason::WaitSource;
    };
    let Some(timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(process_timer_deadline)
    else {
        super::await_wait_source(ctx, source, interests).await;
        return MqWakeReason::WaitSource;
    };

    let source_future = super::await_wait_source(ctx, source, interests);
    let mut source_future = core::pin::pin!(source_future);
    let mut timer_future = core::pin::pin!(timer_future);

    use core::future::{poll_fn, Future};
    use core::task::Poll;

    poll_fn(|cx| {
        if source_future.as_mut().poll(cx).is_ready() {
            Poll::Ready(MqWakeReason::WaitSource)
        } else if timer_future.as_mut().poll(cx).is_ready() {
            let now_ns = P::read_ns().max(process_timer_deadline);
            super::time::poll_expired_process_timers_at(ctx, now_ns);
            Poll::Ready(MqWakeReason::ProcessTimer)
        } else {
            Poll::Pending
        }
    })
    .await
}

fn validate_sem_timeout(ctx: &SyscallCtx<'_>, timeout_ptr: u64) -> Result<(), SyscallResult> {
    if timeout_ptr == 0 {
        return Ok(());
    }
    let ts: TimespecLayout = match bootstrap_read_user(&ctx.aspace, timeout_ptr) {
        Ok(v) => v,
        Err(errno) => return Err(SyscallResult::Error(errno_to_i32(errno))),
    };
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    Ok(())
}

fn read_semops(args: [u64; 6], ctx: &SyscallCtx<'_>) -> Result<(u32, Vec<SemBuf>), SyscallResult> {
    let semid = args[0] as u32;
    let sops_ptr = args[1];
    let nsops = args[2] as usize;
    if nsops == 0 || nsops > 500 {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    if sops_ptr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    let byte_len = match nsops.checked_mul(core::mem::size_of::<SembufLayout>()) {
        Some(len) => len,
        None => return Err(SyscallResult::Error(EINVAL_VALUE)),
    };
    let mut bytes = alloc::vec![0; byte_len];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, sops_ptr) {
        return Err(SyscallResult::Error(errno_to_i32(errno)));
    }
    let sops = bytes
        .chunks_exact(core::mem::size_of::<SembufLayout>())
        .map(|chunk| SemBuf {
            sem_num: u16::from_ne_bytes([chunk[0], chunk[1]]),
            sem_op: i16::from_ne_bytes([chunk[2], chunk[3]]),
            sem_flg: i16::from_ne_bytes([chunk[4], chunk[5]]),
        })
        .collect();
    Ok((semid, sops))
}

fn mq_file(
    ctx: &SyscallCtx<'_>,
    fd: u32,
) -> Result<tx_subsystems::process::adapter::step_engine::Cap<OpenFile>, SyscallResult> {
    ctx.process.fd(fd).ok_or(SyscallResult::Error(EBADF_VALUE))
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

pub(super) async fn sys_shmat(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cred = ctx.cred_cap();
    match ipc::sysv_shm::execution::script_shmat(
        args[0] as u32,
        args[1] as usize,
        args[2] as i32,
        &cred,
        &ctx.aspace,
    )
    .await
    {
        Ok(addr) => SyscallResult::Return(addr as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) async fn sys_shmdt(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    match ipc::sysv_shm::execution::script_shmdt(args[0] as usize, &ctx.aspace).await {
        Ok(()) => SyscallResult::Return(0i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_shmctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let shmid = args[0] as u32;
    let cmd = args[1] as i32;
    let buf_ptr = args[2];
    let cred = ctx.cred_cap();

    let set_fields = if cmd == ipc::sysv_shm::execution::IPC_SET {
        let ds: ShmidDsLayout = match bootstrap_read_user(&ctx.aspace, buf_ptr) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        Some((ds.shm_perm.mode as u16, ds.shm_perm.uid, ds.shm_perm.gid))
    } else {
        None
    };

    let ns = match cmd == ipc::sysv_shm::execution::IPC_RMID {
        true => match ctx.process.nsproxy_cap() {
            Some(ns) => Some(ns),
            None => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
        },
        false => None,
    };
    let result = match ns {
        Some(ns) => ipc::sysv_shm::execution::step_shmctl_in_ns(shmid, cmd, set_fields, &cred, &ns),
        None => ipc::sysv_shm::execution::step_shmctl(shmid, cmd, set_fields, &cred),
    };

    match result {
        Ok(ipc::sysv_shm::execution::ShmCtlResult::Success) => SyscallResult::Return(0),
        Ok(ipc::sysv_shm::execution::ShmCtlResult::Stat(info)) => {
            let ds = ShmidDsLayout {
                shm_perm: ipc_perm_layout(
                    info.key,
                    info.uid,
                    info.gid,
                    info.cuid,
                    info.cgid,
                    info.perm.mode,
                ),
                shm_segsz: info.size as u64,
                shm_atime: 0,
                shm_dtime: 0,
                shm_ctime: 0,
                shm_cpid: 0,
                shm_lpid: 0,
                shm_nattch: info.attach_count as u64,
                __pad1: 0,
                __pad2: 0,
            };
            if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, ds) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            SyscallResult::Return(info.return_value)
        }
        Ok(ipc::sysv_shm::execution::ShmCtlResult::Info {
            return_value,
            shmmni,
            shmmax,
            shmmin,
            shmall,
            shmseg,
        }) => {
            let info = ShminfoLayout {
                shmmax,
                shmmin,
                shmmni,
                shmseg,
                shmall,
                __unused: [0; 4],
            };
            if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, info) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            SyscallResult::Return(return_value)
        }
        Ok(ipc::sysv_shm::execution::ShmCtlResult::ShmInfo {
            return_value,
            used_ids,
            shm_tot,
            shm_rss,
            shm_swp,
            swap_attempts,
            swap_successes,
        }) => {
            let info = ShmInfoLayout {
                used_ids,
                __pad: 0,
                shm_tot,
                shm_rss,
                shm_swp,
                swap_attempts,
                swap_successes,
            };
            if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, info) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            SyscallResult::Return(return_value)
        }
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
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let msqid = args[0] as u32;
    let cmd = args[1] as i32;
    let buf_ptr = args[2];
    let set_fields = if cmd == ipc::sysv_msg::execution::IPC_SET {
        let ds: MsqidDsLayout = match bootstrap_read_user(&ctx.aspace, buf_ptr) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        Some((ds.msg_perm.mode as u16, ds.msg_perm.uid, ds.msg_perm.gid))
    } else {
        None
    };

    match ipc::sysv_msg::execution::step_msgctl_in_ns(msqid, cmd, set_fields, &cred, &ns) {
        Ok(result) => match result {
            ipc::sysv_msg::execution::MsgCtlResult::Success => SyscallResult::Return(0i64),
            ipc::sysv_msg::execution::MsgCtlResult::Stat(info) => {
                let ds = MsqidDsLayout {
                    msg_perm: ipc_perm_layout(
                        info.key,
                        info.uid,
                        info.gid,
                        info.cuid,
                        info.cgid,
                        info.perm.mode,
                    ),
                    msg_stime: 0,
                    msg_rtime: 0,
                    msg_ctime: 0,
                    msg_cbytes: info.current_bytes as u64,
                    msg_qnum: info.msg_count as u64,
                    msg_qbytes: info.qbytes as u64,
                    msg_lspid: 0,
                    msg_lrpid: 0,
                    __unused: [0; 2],
                };
                if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, ds) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                SyscallResult::Return(0)
            }
            ipc::sysv_msg::execution::MsgCtlResult::Info {
                msgmni,
                msgmax,
                msgmnb,
            } => {
                let msgmni_i32 = msgmni.min(i32::MAX as u64) as i32;
                let info = MsginfoLayout {
                    msgpool: 0,
                    msgmap: 0,
                    msgmax: msgmax.min(i32::MAX as u64) as i32,
                    msgmnb: msgmnb.min(i32::MAX as u64) as i32,
                    msgmni: msgmni_i32,
                    msgssz: 0,
                    msgtql: msgmni_i32,
                    msgseg: 0,
                };
                if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, info) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                SyscallResult::Return(0)
            }
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

fn read_relative_timeout_ns(
    ctx: &SyscallCtx<'_>,
    timeout_ptr: u64,
) -> Result<Option<u64>, SyscallResult> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let ts: TimespecLayout = match bootstrap_read_user(&ctx.aspace, timeout_ptr) {
        Ok(v) => v,
        Err(errno) => return Err(SyscallResult::Error(errno_to_i32(errno))),
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    Ok(Some(
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64),
    ))
}

async fn drive_semop(
    semid: u32,
    sops: &[SemBuf],
    ctx: &SyscallCtx<'_>,
    deadline_ns: Option<u64>,
) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };

    match ipc::sysv_sem::execution::step_semop(semid, sops, &cred, &ctx.process) {
        Ok(applied) => return SyscallResult::Return(applied as i64),
        Err(Errno::EAGAIN)
            if sops
                .iter()
                .all(|op| (op.sem_flg & ipc::sysv_sem::structure::sem_flg::IPC_NOWAIT) == 0) => {}
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    }

    let mut script_ctx = super::build_subject_script_ctx(ctx);
    if let Some(deadline_ns) = deadline_ns {
        script_ctx = script_ctx.with_deadline(Deadline::from_raw(deadline_ns));
    }
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();

    let op = ipc::sysv_sem::execution::SemopWaitOp::new(
        semid,
        sops,
        &cred,
        &ctx.process,
        Some(ctx.process.pid.0),
    );
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(applied) => SyscallResult::Return(applied as i64),
        Err(v3errno) => {
            let errno: Errno = v3errno.into();
            if deadline_ns.is_some() && errno == Errno::ETIMEDOUT {
                return SyscallResult::Error(errno_to_i32(Errno::EAGAIN));
            }
            SyscallResult::error_from(errno)
        }
    }
}

pub(super) async fn sys_semop(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (semid, sops) = match read_semops(args, ctx) {
        Ok(v) => v,
        Err(result) => return result,
    };
    drive_semop(semid, &sops, ctx, None).await
}

pub(super) async fn sys_semtimedop<P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let (semid, sops) = match read_semops(args, ctx) {
        Ok(v) => v,
        Err(result) => return result,
    };
    let timeout_ns = match read_relative_timeout_ns(ctx, args[3]) {
        Ok(v) => v,
        Err(result) => return result,
    };
    let deadline_ns = timeout_ns.map(|ns| P::read_ns().saturating_add(ns));
    drive_semop(semid, &sops, ctx, deadline_ns).await
}

pub(super) fn sys_semctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let semid = args[0] as u32;
    let semnum = args[1] as u16;
    let cmd = args[2] as i32;
    let arg_raw = args[3];
    let sem_arg = match cmd {
        ipc::sysv_sem::execution::IPC_SET => {
            let ds: SemidDsLayout = match bootstrap_read_user(&ctx.aspace, arg_raw) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            ipc::sysv_sem::execution::SemCtlArg::IpcSet {
                mode: ds.sem_perm.mode as u16,
                uid: ds.sem_perm.uid,
                gid: ds.sem_perm.gid,
            }
        }
        ipc::sysv_sem::execution::SETVAL => {
            ipc::sysv_sem::execution::SemCtlArg::Val(arg_raw as i32)
        }
        ipc::sysv_sem::execution::SETALL => {
            let nsems = match ipc::sysv_sem::execution::step_semctl_in_ns(
                semid,
                semnum,
                ipc::sysv_sem::execution::IPC_STAT,
                ipc::sysv_sem::execution::SemCtlArg::None,
                &cred,
                &ns,
                Some(&ctx.process),
            ) {
                Ok(ipc::sysv_sem::execution::SemCtlResult::Stat(info)) => info.nsems,
                Ok(_) => return SyscallResult::Error(EINVAL_VALUE),
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let byte_len = (nsems as usize) * core::mem::size_of::<u16>();
            let mut bytes = alloc::vec![0u8; byte_len];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, arg_raw) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let values = bytes
                .chunks_exact(core::mem::size_of::<u16>())
                .map(|chunk| u16::from_ne_bytes([chunk[0], chunk[1]]))
                .collect();
            ipc::sysv_sem::execution::SemCtlArg::All(values)
        }
        _ => ipc::sysv_sem::execution::SemCtlArg::None,
    };

    match ipc::sysv_sem::execution::step_semctl_in_ns(
        semid,
        semnum,
        cmd,
        sem_arg,
        &cred,
        &ns,
        Some(&ctx.process),
    ) {
        Ok(result) => match result {
            ipc::sysv_sem::execution::SemCtlResult::Success => SyscallResult::Return(0i64),
            ipc::sysv_sem::execution::SemCtlResult::Val(v) => SyscallResult::Return(v as i64),
            ipc::sysv_sem::execution::SemCtlResult::All(values) => {
                let mut bytes = Vec::with_capacity(values.len() * core::mem::size_of::<u16>());
                for value in values {
                    bytes.extend_from_slice(&value.to_ne_bytes());
                }
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, arg_raw, &bytes) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                SyscallResult::Return(0)
            }
            ipc::sysv_sem::execution::SemCtlResult::Stat(info) => {
                let ds = SemidDsLayout {
                    sem_perm: ipc_perm_layout(
                        info.key,
                        info.uid,
                        info.gid,
                        info.cuid,
                        info.cgid,
                        info.perm.mode,
                    ),
                    sem_otime: 0,
                    sem_ctime: 0,
                    sem_nsems: info.nsems,
                    __sem_nsems_pad: [0; 6],
                    __unused3: 0,
                    __unused4: 0,
                };
                if let Err(errno) = bootstrap_write_user(&ctx.aspace, arg_raw, ds) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                SyscallResult::Return(0)
            }
            ipc::sysv_sem::execution::SemCtlResult::Info {
                semmni,
                semmns,
                semmsl,
                semopm,
            } => {
                let info = SeminfoLayout {
                    semmap: 0,
                    semmni: semmni.min(i32::MAX as u64) as i32,
                    semmns: semmns.min(i32::MAX as u64) as i32,
                    semmnu: 0,
                    semmsl: semmsl.min(i32::MAX as u64) as i32,
                    semopm: semopm.min(i32::MAX as u64) as i32,
                    semume: 0,
                    semusz: 0,
                    semvmx: 32767,
                    semaem: 0,
                };
                if let Err(errno) = bootstrap_write_user(&ctx.aspace, arg_raw, info) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                SyscallResult::Return(0)
            }
        },
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_msgsnd(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let msqid = args[0] as u32;
    let msgp = args[1];
    let msgsz = args[2] as usize;
    let msgflg = args[3] as i32;
    if msgp == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let mtype: i64 = match bootstrap_read_user(&ctx.aspace, msgp) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let mut mtext = alloc::vec![0; msgsz];
    if msgsz > 0 {
        let text_ptr = match msgp.checked_add(core::mem::size_of::<i64>() as u64) {
            Some(ptr) => ptr,
            None => return SyscallResult::Error(EFAULT_VALUE),
        };
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut mtext, text_ptr) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    match ipc::sysv_msg::execution::step_msgsnd(msqid, mtype, mtext, msgflg, &cred) {
        Ok(_) => SyscallResult::Return(0),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_msgrcv(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let msqid = args[0] as u32;
    let msgp = args[1];
    let msgsz = args[2] as usize;
    let msgtyp = args[3] as i64;
    let msgflg = args[4] as i32;
    if msgp == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    match ipc::sysv_msg::execution::step_msgrcv(msqid, msgsz, msgtyp, msgflg, &cred) {
        Ok((mtype, mtext)) => {
            if let Err(errno) = bootstrap_write_user(&ctx.aspace, msgp, mtype) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            if !mtext.is_empty() {
                let text_ptr = match msgp.checked_add(core::mem::size_of::<i64>() as u64) {
                    Some(ptr) => ptr,
                    None => return SyscallResult::Error(EFAULT_VALUE),
                };
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, text_ptr, &mtext) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
            }
            SyscallResult::Return(mtext.len() as i64)
        }
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_mq_open(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let name = match read_mq_name(ctx, args[0]) {
        Ok(v) => v,
        Err(result) => return result,
    };
    let oflag = args[1] as i32;
    let mode = args[2] as u16;
    let open_flags = match mq_open_file_flags(oflag) {
        Ok(flags) => flags,
        Err(result) => return result,
    };
    let attr = if (oflag & O_CREAT as i32) != 0 && args[3] != 0 {
        let layout: MqAttrLayout = match bootstrap_read_user(&ctx.aspace, args[3]) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        Some(ipc::posix_mq::execution::MqCreateAttr {
            maxmsg: layout.mq_maxmsg,
            msgsize: layout.mq_msgsize,
        })
    } else {
        None
    };

    let mq = match ipc::posix_mq::execution::step_mq_open(&name, oflag, mode, attr, &cred, &ns) {
        Ok(mq) => mq,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let open_cap = match OpenFile::new_posix_mq_cap(mq, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(fd, open_cap);
    if open_flags.cloexec {
        ctx.process.set_fd_cloexec(fd, true);
    }
    SyscallResult::Return(fd as i64)
}

pub(super) fn sys_mq_unlink(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (ns, _cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let name = match read_mq_name(ctx, args[0]) {
        Ok(v) => v,
        Err(result) => return result,
    };
    match ipc::posix_mq::execution::step_mq_unlink(&name, &ns) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) async fn sys_mq_timedsend<P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if let Err(result) = validate_abs_timeout(ctx, args[4]) {
        return result;
    }
    let fd = args[0] as u32;
    let msg_ptr = args[1];
    let msg_len = args[2] as usize;
    if msg_ptr == 0 && msg_len != 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let file = match mq_file(ctx, fd) {
        Ok(file) => file,
        Err(result) => return result,
    };
    if !file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let mq = match file.posix_mq() {
        Some(mq) => mq,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if msg_len > mq.msgsize() as usize {
        return SyscallResult::Error(EMSGSIZE_VALUE);
    }
    let mut msg = alloc::vec![0; msg_len];
    if msg_len > 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut msg, msg_ptr) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    let cred = ctx.cred_cap();
    loop {
        match ipc::posix_mq::execution::step_mq_send(mq, &msg, args[3] as u32, &cred) {
            Ok(()) => return SyscallResult::Return(0),
            Err(Errno::EAGAIN) if args[4] == 0 && !file.flags().nonblocking => {
                if wait_for_mq_readiness::<P>(ctx, mq, true).await == MqWakeReason::ProcessTimer {
                    return SyscallResult::Error(EINTR_VALUE);
                }
            }
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

pub(super) async fn sys_mq_timedreceive<P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if let Err(result) = validate_abs_timeout(ctx, args[4]) {
        return result;
    }
    let fd = args[0] as u32;
    let msg_ptr = args[1];
    let msg_len = args[2] as usize;
    if msg_ptr == 0 && msg_len != 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let file = match mq_file(ctx, fd) {
        Ok(file) => file,
        Err(result) => return result,
    };
    if !file.flags().read {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let mq = match file.posix_mq() {
        Some(mq) => mq,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if msg_len < mq.msgsize() as usize {
        return SyscallResult::Error(EMSGSIZE_VALUE);
    }
    let cred = ctx.cred_cap();
    loop {
        match ipc::posix_mq::execution::step_mq_receive(mq, msg_len, &cred) {
            Ok((msg, prio)) => {
                if !msg.is_empty() {
                    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, msg_ptr, &msg) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                if args[3] != 0 {
                    if let Err(errno) = bootstrap_write_user(&ctx.aspace, args[3], prio) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                return SyscallResult::Return(msg.len() as i64);
            }
            Err(Errno::EAGAIN) if args[4] == 0 && !file.flags().nonblocking => {
                if wait_for_mq_readiness::<P>(ctx, mq, false).await == MqWakeReason::ProcessTimer {
                    return SyscallResult::Error(EINTR_VALUE);
                }
            }
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

pub(super) fn sys_mq_getsetattr(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let newattr_ptr = args[1];
    let oldattr_ptr = args[2];
    let file = match mq_file(ctx, fd) {
        Ok(file) => file,
        Err(result) => return result,
    };
    let mq = match file.posix_mq() {
        Some(mq) => mq,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let old = match ipc::posix_mq::execution::step_mq_getattr(mq) {
        Ok(attr) => attr,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if oldattr_ptr != 0 {
        if let Err(errno) = bootstrap_write_user(&ctx.aspace, oldattr_ptr, mq_attr_to_layout(old)) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if newattr_ptr != 0 {
        let new_layout: MqAttrLayout = match bootstrap_read_user(&ctx.aspace, newattr_ptr) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        if let Err(errno) = ipc::posix_mq::execution::step_mq_setattr(mq, new_layout.mq_flags) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        file.set_nonblocking((new_layout.mq_flags & O_NONBLOCK as i64) != 0);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_mq_notify(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let file = match mq_file(ctx, fd) {
        Ok(file) => file,
        Err(result) => return result,
    };
    let mq = match file.posix_mq() {
        Some(mq) => mq,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let notification = if args[1] == 0 {
        None
    } else {
        let sev: SigeventPrefixLayout = match bootstrap_read_user(&ctx.aspace, args[1]) {
            Ok(v) => v,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        match sev.sigev_notify {
            SIGEV_SIGNAL | SIGEV_THREAD_ID => {
                let Some(signum) = u8::try_from(sev.sigev_signo).ok().and_then(Signum::new) else {
                    return SyscallResult::Error(EINVAL_VALUE);
                };
                Some(MqNotification::Signal {
                    signum,
                    owner: ctx.process.downgrade(),
                })
            }
            SIGEV_NONE => Some(MqNotification::None),
            SIGEV_THREAD => return SyscallResult::Error(EINVAL_VALUE),
            _ => return SyscallResult::Error(EINVAL_VALUE),
        }
    };
    match ipc::posix_mq::execution::step_mq_notify(mq, notification) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}
