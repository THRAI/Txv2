//! SysV / POSIX IPC syscall arms — Phase IPC-5.

use super::{
    bootstrap_copy_from_user, bootstrap_copy_to_user, bootstrap_read_user, bootstrap_write_user,
    errno_to_i32, SyscallCtx, SyscallResult, EFAULT_VALUE, EINVAL_VALUE, ENOSYS_VALUE,
};
use alloc::vec::Vec;
use tx_subsystems::execution::Errno;
use tx_subsystems::ipc;
use tx_subsystems::ipc::sysv_sem::structure::SemBuf;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IpcPermLayout {
    pub key: u32,
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
#[derive(Clone, Copy, Debug)]
pub struct ShmidDsLayout {
    pub shm_perm: IpcPermLayout,
    pub shm_segsz: u64,
    pub shm_atime: i64,
    pub shm_dtime: i64,
    pub shm_ctime: i64,
    pub shm_cpid: i32,
    pub shm_lpid: i32,
    pub shm_nattch: u64,
    pub __unused4: u64,
    pub __unused5: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShminfoLayout {
    pub shmmax: u64,
    pub shmmin: u64,
    pub shmmni: u64,
    pub shmseg: u64,
    pub shmall: u64,
    pub __unused: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
pub struct MsginfoLayout {
    pub msgpool: i32,
    pub msgmap: i32,
    pub msgmax: i32,
    pub msgmnb: i32,
    pub msgmni: i32,
    pub msgssz: i32,
    pub msgtql: i32,
    pub msgseg: u16,
    pub __pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
pub struct SembufLayout {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

const MSG_NOERROR: i32 = 0o10000;
const MAX_SYSV_MSG_TEXT: usize = 8192;

fn ipc_perm_layout(key: u32, uid: u32, gid: u32, cuid: u32, cgid: u32, mode: u16) -> IpcPermLayout {
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
    let shmid = args[0] as u32;
    let cmd = args[1] as i32;
    let buf_ptr = args[2];
    let cred = ctx.cred_cap();

    match cmd {
        ipc::sysv_shm::execution::IPC_INFO => {
            match ipc::sysv_shm::execution::step_shmctl(shmid, cmd, None, &cred) {
                Ok(ipc::sysv_shm::execution::ShmCtlResult::Info {
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
                    SyscallResult::Return(0)
                }
                Ok(_) => SyscallResult::Error(22), // EINVAL
                Err(e) => SyscallResult::Error(errno_to_i32(e)),
            }
        }
        ipc::sysv_shm::execution::IPC_STAT => {
            match ipc::sysv_shm::execution::step_shmctl(shmid, cmd, None, &cred) {
                Ok(ipc::sysv_shm::execution::ShmCtlResult::Stat(info)) => {
                    let ds = ShmidDsLayout {
                        shm_perm: IpcPermLayout {
                            key: info.key,
                            uid: info.uid,
                            gid: info.gid,
                            cuid: info.cuid,
                            cgid: info.cgid,
                            mode: info.perm.mode as u32,
                            __seq: 0,
                            __pad1: 0,
                            __pad2: 0,
                        },
                        shm_segsz: info.size as u64,
                        shm_atime: 0,
                        shm_dtime: 0,
                        shm_ctime: 0,
                        shm_cpid: 0,
                        shm_lpid: 0,
                        shm_nattch: info.attach_count as u64,
                        __unused4: 0,
                        __unused5: 0,
                    };
                    if let Err(errno) = bootstrap_write_user(&ctx.aspace, buf_ptr, ds) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    SyscallResult::Return(0)
                }
                Ok(_) => SyscallResult::Error(22), // EINVAL
                Err(e) => SyscallResult::Error(errno_to_i32(e)),
            }
        }
        ipc::sysv_shm::execution::IPC_SET => {
            let ds: ShmidDsLayout = match bootstrap_read_user(&ctx.aspace, buf_ptr) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let set_fields = Some((ds.shm_perm.mode as u16, ds.shm_perm.uid, ds.shm_perm.gid));
            match ipc::sysv_shm::execution::step_shmctl(shmid, cmd, set_fields, &cred) {
                Ok(ipc::sysv_shm::execution::ShmCtlResult::Success) => SyscallResult::Return(0),
                Ok(_) => SyscallResult::Error(22), // EINVAL
                Err(e) => SyscallResult::Error(errno_to_i32(e)),
            }
        }
        _ => match ipc::sysv_shm::execution::step_shmctl(shmid, cmd, None, &cred) {
            Ok(ipc::sysv_shm::execution::ShmCtlResult::Success) => SyscallResult::Return(0),
            Ok(_) => SyscallResult::Return(0),
            Err(e) => SyscallResult::Error(errno_to_i32(e)),
        },
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
    match ipc::sysv_msg::execution::step_msgctl(msqid, cmd, set_fields, &cred) {
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
                    __pad: 0,
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

pub(super) fn sys_semop(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
        Ok(v) => v,
        Err(e) => return SyscallResult::Error(errno_to_i32(e)),
    };
    let semid = args[0] as u32;
    let sops_ptr = args[1];
    let nsops = args[2] as usize;
    if nsops == 0 || nsops > 500 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if sops_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let byte_len = match nsops.checked_mul(core::mem::size_of::<SembufLayout>()) {
        Some(len) => len,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let mut bytes = Vec::new();
    bytes.resize(byte_len, 0);
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, sops_ptr) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let mut sops = Vec::with_capacity(nsops);
    for chunk in bytes.chunks_exact(core::mem::size_of::<SembufLayout>()) {
        let sem_num = u16::from_ne_bytes([chunk[0], chunk[1]]);
        let sem_op = i16::from_ne_bytes([chunk[2], chunk[3]]);
        let sem_flg = i16::from_ne_bytes([chunk[4], chunk[5]]);
        sops.push(SemBuf {
            sem_num,
            sem_op,
            sem_flg,
        });
    }
    match ipc::sysv_sem::execution::step_semop(semid, &sops, &cred, ctx.process.pid.0 as u64) {
        Ok(applied) => SyscallResult::Return(applied as i64),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_semctl(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let (_ns, cred) = match nsproxy_and_cred(ctx) {
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
        _ => ipc::sysv_sem::execution::SemCtlArg::None,
    };
    match ipc::sysv_sem::execution::step_semctl(semid, semnum, cmd, sem_arg, &cred) {
        Ok(result) => match result {
            ipc::sysv_sem::execution::SemCtlResult::Success => SyscallResult::Return(0i64),
            ipc::sysv_sem::execution::SemCtlResult::Val(v) => SyscallResult::Return(v as i64),
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
                    semusz: core::mem::size_of::<ipc::sysv_sem::structure::SemUndo>() as i32,
                    semvmx: 32767,
                    semaem: 32767,
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
    let cred = ctx.cred_cap();
    let msqid = args[0] as u32;
    let msgp = args[1];
    let msgsz = args[2] as usize;
    let msgflg = args[3] as i32;
    if msgp == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if msgsz > MAX_SYSV_MSG_TEXT {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let mut header = [0u8; 8];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut header, msgp) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let mtype = i64::from_ne_bytes(header);
    let mut text = Vec::new();
    text.resize(msgsz, 0);
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut text, msgp.wrapping_add(8)) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    match ipc::sysv_msg::execution::step_msgsnd(msqid, mtype, text, msgflg, &cred) {
        Ok(_) => SyscallResult::Return(0),
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_msgrcv(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let cred = ctx.cred_cap();
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
            if mtext.len() > msgsz && (msgflg & MSG_NOERROR) == 0 {
                return SyscallResult::Error(errno_to_i32(Errno::E2BIG));
            }
            if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, msgp, &mtype.to_ne_bytes()) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let copy_len = core::cmp::min(mtext.len(), msgsz);
            if let Err(errno) =
                bootstrap_copy_to_user(&ctx.aspace, msgp.wrapping_add(8), &mtext[..copy_len])
            {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            SyscallResult::Return(copy_len as i64)
        }
        Err(e) => SyscallResult::Error(errno_to_i32(e)),
    }
}

pub(super) fn sys_mq_open(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

pub(super) fn sys_mq_unlink(_args: [u64; 6], _ctx: &SyscallCtx<'_>) -> SyscallResult {
    SyscallResult::Error(ENOSYS_VALUE)
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn ipc_perm_layout_matches_musl_generic() {
        assert_eq!(offset_of!(IpcPermLayout, key), 0);
        assert_eq!(offset_of!(IpcPermLayout, uid), 4);
        assert_eq!(offset_of!(IpcPermLayout, gid), 8);
        assert_eq!(offset_of!(IpcPermLayout, cuid), 12);
        assert_eq!(offset_of!(IpcPermLayout, cgid), 16);
        assert_eq!(offset_of!(IpcPermLayout, mode), 20);
        assert_eq!(offset_of!(IpcPermLayout, __seq), 24);
        assert_eq!(offset_of!(IpcPermLayout, __pad1), 32);
        assert_eq!(offset_of!(IpcPermLayout, __pad2), 40);
        assert_eq!(size_of::<IpcPermLayout>(), 48);
    }

    #[test]
    fn shmid_ds_layout_matches_musl_generic() {
        assert_eq!(offset_of!(ShmidDsLayout, shm_perm), 0);
        assert_eq!(offset_of!(ShmidDsLayout, shm_segsz), 48);
        assert_eq!(offset_of!(ShmidDsLayout, shm_atime), 56);
        assert_eq!(offset_of!(ShmidDsLayout, shm_dtime), 64);
        assert_eq!(offset_of!(ShmidDsLayout, shm_ctime), 72);
        assert_eq!(offset_of!(ShmidDsLayout, shm_cpid), 80);
        assert_eq!(offset_of!(ShmidDsLayout, shm_lpid), 84);
        assert_eq!(offset_of!(ShmidDsLayout, shm_nattch), 88);
        assert_eq!(offset_of!(ShmidDsLayout, __unused4), 96);
        assert_eq!(offset_of!(ShmidDsLayout, __unused5), 104);
        assert_eq!(size_of::<ShmidDsLayout>(), 112);
    }

    #[test]
    fn shminfo_layout_matches_musl_generic() {
        assert_eq!(offset_of!(ShminfoLayout, shmmax), 0);
        assert_eq!(offset_of!(ShminfoLayout, shmmin), 8);
        assert_eq!(offset_of!(ShminfoLayout, shmmni), 16);
        assert_eq!(offset_of!(ShminfoLayout, shmseg), 24);
        assert_eq!(offset_of!(ShminfoLayout, shmall), 32);
        assert_eq!(offset_of!(ShminfoLayout, __unused), 40);
        assert_eq!(size_of::<ShminfoLayout>(), 72);
    }

    #[test]
    fn msqid_ds_layout_matches_musl_generic() {
        assert_eq!(offset_of!(MsqidDsLayout, msg_perm), 0);
        assert_eq!(offset_of!(MsqidDsLayout, msg_stime), 48);
        assert_eq!(offset_of!(MsqidDsLayout, msg_rtime), 56);
        assert_eq!(offset_of!(MsqidDsLayout, msg_ctime), 64);
        assert_eq!(offset_of!(MsqidDsLayout, msg_cbytes), 72);
        assert_eq!(offset_of!(MsqidDsLayout, msg_qnum), 80);
        assert_eq!(offset_of!(MsqidDsLayout, msg_qbytes), 88);
        assert_eq!(offset_of!(MsqidDsLayout, msg_lspid), 96);
        assert_eq!(offset_of!(MsqidDsLayout, msg_lrpid), 100);
        assert_eq!(offset_of!(MsqidDsLayout, __unused), 104);
        assert_eq!(size_of::<MsqidDsLayout>(), 120);
    }

    #[test]
    fn semid_ds_layout_matches_musl_generic() {
        assert_eq!(offset_of!(SemidDsLayout, sem_perm), 0);
        assert_eq!(offset_of!(SemidDsLayout, sem_otime), 48);
        assert_eq!(offset_of!(SemidDsLayout, sem_ctime), 56);
        assert_eq!(offset_of!(SemidDsLayout, sem_nsems), 64);
        assert_eq!(offset_of!(SemidDsLayout, __sem_nsems_pad), 66);
        assert_eq!(offset_of!(SemidDsLayout, __unused3), 72);
        assert_eq!(offset_of!(SemidDsLayout, __unused4), 80);
        assert_eq!(size_of::<SemidDsLayout>(), 88);
    }

    #[test]
    fn msginfo_and_seminfo_layouts_match_musl() {
        assert_eq!(offset_of!(MsginfoLayout, msgpool), 0);
        assert_eq!(offset_of!(MsginfoLayout, msgseg), 28);
        assert_eq!(size_of::<MsginfoLayout>(), 32);
        assert_eq!(offset_of!(SeminfoLayout, semmap), 0);
        assert_eq!(offset_of!(SeminfoLayout, semaem), 36);
        assert_eq!(size_of::<SeminfoLayout>(), 40);
    }
}
