use super::*;

use crate::linux_syscall::{
    IpcPermLayout, MsqidDsLayout, SembufLayout, SemidDsLayout, NR_MSGCTL, NR_MSGGET, NR_MSGRCV,
    NR_MSGSND, NR_SEMCTL, NR_SEMGET, NR_SEMOP,
};
use tx_subsystems::ipc::{sysv_msg, sysv_sem, sysv_shm};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Msgbuf8 {
    mtype: i64,
    mtext: [u8; 8],
}

#[test]
fn dispatch_sysv_msg_round_trip_and_ipc_stat_use_musl_layout() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let msgget = SyscallRequest::new(
        NR_MSGGET,
        [
            0x4d534751,
            (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
            0,
            0,
            0,
            0,
        ],
    );
    let msqid = match block_on(dispatch::<ShimsTestPmap>(msgget, &ctx)) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("msgget failed: {other:?}"),
    };

    let send = Msgbuf8 {
        mtype: 7,
        mtext: *b"tx-msg!!",
    };
    let send_req = SyscallRequest::new(
        NR_MSGSND,
        [
            msqid,
            (&send as *const Msgbuf8) as u64,
            send.mtext.len() as u64,
            sysv_shm::execution::IPC_NOWAIT as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(send_req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut ds = MsqidDsLayout {
        msg_perm: IpcPermLayout {
            key: 0,
            uid: 0,
            gid: 0,
            cuid: 0,
            cgid: 0,
            mode: 0,
            __seq: 0,
            __pad1: 0,
            __pad2: 0,
        },
        msg_stime: 0,
        msg_rtime: 0,
        msg_ctime: 0,
        msg_cbytes: 0,
        msg_qnum: 0,
        msg_qbytes: 0,
        msg_lspid: 0,
        msg_lrpid: 0,
        __unused: [0; 2],
    };
    let stat_req = SyscallRequest::new(
        NR_MSGCTL,
        [
            msqid,
            sysv_msg::execution::IPC_STAT as u64,
            (&mut ds as *mut MsqidDsLayout) as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(ds.msg_perm.key, 0x4d534751);
    assert_eq!(ds.msg_perm.mode, 0o660);
    assert_eq!(ds.msg_qnum, 1);
    assert_eq!(ds.msg_cbytes, 8);

    let mut recv = Msgbuf8 {
        mtype: 0,
        mtext: [0; 8],
    };
    let recv_req = SyscallRequest::new(
        NR_MSGRCV,
        [
            msqid,
            (&mut recv as *mut Msgbuf8) as u64,
            recv.mtext.len() as u64,
            7,
            sysv_shm::execution::IPC_NOWAIT as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(recv_req, &ctx)),
        SyscallResult::Return(8)
    );
    assert_eq!(recv.mtype, 7);
    assert_eq!(&recv.mtext, b"tx-msg!!");
}

#[test]
fn dispatch_sysv_semop_and_semctl_stat_use_musl_layout() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let semget = SyscallRequest::new(
        NR_SEMGET,
        [
            0x53454d31,
            1,
            (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
            0,
            0,
            0,
        ],
    );
    let semid = match block_on(dispatch::<ShimsTestPmap>(semget, &ctx)) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("semget failed: {other:?}"),
    };

    let setval = SyscallRequest::new(
        NR_SEMCTL,
        [semid, 0, sysv_sem::execution::SETVAL as u64, 2, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(setval, &ctx)),
        SyscallResult::Return(0)
    );

    let op = SembufLayout {
        sem_num: 0,
        sem_op: -1,
        sem_flg: sysv_shm::execution::IPC_NOWAIT as i16,
    };
    let semop = SyscallRequest::new(
        NR_SEMOP,
        [semid, (&op as *const SembufLayout) as u64, 1, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(semop, &ctx)),
        SyscallResult::Return(1)
    );

    let getval = SyscallRequest::new(
        NR_SEMCTL,
        [semid, 0, sysv_sem::execution::GETVAL as u64, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(getval, &ctx)),
        SyscallResult::Return(1)
    );

    let mut ds = SemidDsLayout {
        sem_perm: IpcPermLayout {
            key: 0,
            uid: 0,
            gid: 0,
            cuid: 0,
            cgid: 0,
            mode: 0,
            __seq: 0,
            __pad1: 0,
            __pad2: 0,
        },
        sem_otime: 0,
        sem_ctime: 0,
        sem_nsems: 0,
        __sem_nsems_pad: [0; 6],
        __unused3: 0,
        __unused4: 0,
    };
    let stat_req = SyscallRequest::new(
        NR_SEMCTL,
        [
            semid,
            0,
            sysv_sem::execution::IPC_STAT as u64,
            (&mut ds as *mut SemidDsLayout) as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(ds.sem_perm.key, 0x53454d31);
    assert_eq!(ds.sem_perm.mode, 0o660);
    assert_eq!(ds.sem_nsems, 1);
}
