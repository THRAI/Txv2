use super::*;

use crate::linux_syscall::{
    IpcPermLayout, MsqidDsLayout, SembufLayout, SemidDsLayout, ShmInfoLayout, ShmidDsLayout,
    ShminfoLayout, NR_MSGCTL, NR_MSGGET, NR_MSGRCV, NR_MSGSND, NR_SEMCTL, NR_SEMGET, NR_SEMOP,
    NR_SHMAT, NR_SHMCTL, NR_SHMDT, NR_SHMGET,
};
use tx_subsystems::ipc::{sysv_msg, sysv_sem, sysv_shm};
use tx_subsystems::vm::{Prot, VmBacking, USER_PAGE_SIZE};

const E2BIG: i32 = 7;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Msgbuf8 {
    mtype: i64,
    mtext: [u8; 8],
}

#[test]
fn sysv_musl_lp64_control_layouts_match_reference_headers() {
    assert_eq!(core::mem::size_of::<IpcPermLayout>(), 48);
    assert_eq!(core::mem::size_of::<MsqidDsLayout>(), 120);
    assert_eq!(core::mem::size_of::<SemidDsLayout>(), 88);
    assert_eq!(core::mem::size_of::<ShmidDsLayout>(), 112);
    assert_eq!(core::mem::size_of::<ShminfoLayout>(), 72);
    assert_eq!(core::mem::size_of::<ShmInfoLayout>(), 48);
    assert_eq!(core::mem::size_of::<SembufLayout>(), 6);
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

    let mut ds = MsqidDsLayout::default();
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
            sysv_msg::execution::MSG_NOERROR as u64,
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
fn dispatch_sysv_msg_rmid_releases_namespace_key_for_recreate() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let key = 0x4d53524d;
    let msgget_flags = (sysv_shm::execution::IPC_CREAT | 0o600) as u64;
    let first_msqid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MSGGET, [key, msgget_flags, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("first msgget failed: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MSGCTL,
                [
                    first_msqid,
                    sysv_msg::execution::IPC_RMID as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let second_msqid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MSGGET, [key, msgget_flags, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("second msgget failed after IPC_RMID: {other:?}"),
    };
    assert_ne!(second_msqid, first_msqid);
}

#[test]
fn dispatch_sysv_msg_noerror_truncates_oversized_receive() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let msqid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MSGGET,
            [
                0x4d534e45,
                (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("msgget failed: {other:?}"),
    };

    let send = Msgbuf8 {
        mtype: 11,
        mtext: *b"abcdefgh",
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MSGSND,
                [
                    msqid,
                    (&send as *const Msgbuf8) as u64,
                    send.mtext.len() as u64,
                    sysv_shm::execution::IPC_NOWAIT as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    struct Msgbuf4 {
        mtype: i64,
        mtext: [u8; 4],
    }
    let mut recv = Msgbuf4 {
        mtype: 0,
        mtext: [0; 4],
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MSGRCV,
                [
                    msqid,
                    (&mut recv as *mut Msgbuf4) as u64,
                    recv.mtext.len() as u64,
                    11,
                    sysv_msg::execution::MSG_NOERROR as u64,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(4)
    );
    assert_eq!(recv.mtype, 11);
    assert_eq!(&recv.mtext, b"abcd");
}

#[test]
fn dispatch_sysv_semop_and_semctl_stat_use_musl_layout() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let semid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SEMGET,
            [
                0x53454d31,
                1,
                (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("semget failed: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 0, sysv_sem::execution::SETVAL as u64, 2, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let op = SembufLayout {
        sem_num: 0,
        sem_op: -1,
        sem_flg: sysv_shm::execution::IPC_NOWAIT as i16,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMOP,
                [semid, (&op as *const SembufLayout) as u64, 1, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 0, sysv_sem::execution::GETVAL as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );

    let mut ds = SemidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [
                    semid,
                    0,
                    sysv_sem::execution::IPC_STAT as u64,
                    (&mut ds as *mut SemidDsLayout) as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(ds.sem_perm.key, 0x53454d31);
    assert_eq!(ds.sem_perm.mode, 0o660);
    assert_eq!(ds.sem_nsems, 1);
}

#[test]
fn dispatch_sysv_semop_too_many_ops_returns_e2big() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let op = SembufLayout {
        sem_num: 0,
        sem_op: 0,
        sem_flg: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMOP,
                [0, (&op as *const SembufLayout) as u64, 501, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E2BIG)
    );
}

#[test]
fn dispatch_sysv_semctl_setall_getall_round_trip() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let semid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SEMGET,
            [
                0x5345414c,
                3,
                (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("semget failed: {other:?}"),
    };

    let set_values = [3u16, 5, 8];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [
                    semid,
                    0,
                    sysv_sem::execution::SETALL as u64,
                    set_values.as_ptr() as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut got_values = [0u16; 3];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [
                    semid,
                    0,
                    sysv_sem::execution::GETALL as u64,
                    got_values.as_mut_ptr() as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(got_values, set_values);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 1, sysv_sem::execution::GETVAL as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(5)
    );
}

#[test]
fn dispatch_sysv_semctl_getpid_tracks_last_modifier() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let expected_pid = process.pid.0 as i64;
    let ctx = make_ctx(process, thread);

    let semid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SEMGET,
            [
                0x53455049,
                1,
                (sysv_shm::execution::IPC_CREAT | 0o660) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("semget failed: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 0, sysv_sem::execution::SETVAL as u64, 2, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 0, sysv_sem::execution::GETPID as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(expected_pid)
    );

    let op = SembufLayout {
        sem_num: 0,
        sem_op: -1,
        sem_flg: sysv_shm::execution::IPC_NOWAIT as i16,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMOP,
                [semid, (&op as *const SembufLayout) as u64, 1, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [semid, 0, sysv_sem::execution::GETPID as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(expected_pid)
    );
}

#[test]
fn dispatch_sysv_sem_rmid_releases_namespace_key_for_recreate() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let key = 0x5345524d;
    let semget_flags = (sysv_shm::execution::IPC_CREAT | 0o600) as u64;
    let first_semid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SEMGET, [key, 1, semget_flags, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("first semget failed: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SEMCTL,
                [
                    first_semid,
                    0,
                    sysv_sem::execution::IPC_RMID as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let second_semid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SEMGET, [key, 1, semget_flags, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("second semget failed after IPC_RMID: {other:?}"),
    };
    assert_ne!(second_semid, first_semid);
}

#[test]
fn dispatch_sysv_shmctl_stat_and_info_use_musl_layout() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let shmid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMGET,
            [
                0x53484d31,
                4096,
                (sysv_shm::execution::IPC_CREAT | 0o640) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("shmget failed: {other:?}"),
    };

    let mut ds = ShmidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SHMCTL,
                [
                    shmid,
                    sysv_shm::execution::IPC_STAT as u64,
                    (&mut ds as *mut ShmidDsLayout) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(ds.shm_perm.key, 0x53484d31);
    assert_eq!(ds.shm_perm.mode, 0o640);
    assert_eq!(ds.shm_segsz, 4096);

    let mut info = ShminfoLayout::default();
    let highest_index = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMCTL,
            [
                0,
                sysv_shm::execution::IPC_INFO as u64,
                (&mut info as *mut ShminfoLayout) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(index) => index,
        other => panic!("IPC_INFO failed: {other:?}"),
    };
    assert!(info.shmmax >= info.shmmin);
    assert!(info.shmmni > 0);

    let mut indexed_ds = ShmidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SHMCTL,
                [
                    highest_index as u64,
                    sysv_shm::execution::SHM_STAT_ANY as u64,
                    (&mut indexed_ds as *mut ShmidDsLayout) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(shmid as i64)
    );
    assert_eq!(indexed_ds.shm_perm.key, 0x53484d31);

    let mut shm_info = ShmInfoLayout::default();
    let shm_info_ret = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMCTL,
            [
                0,
                sysv_shm::execution::SHM_INFO as u64,
                (&mut shm_info as *mut ShmInfoLayout) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(index) => index,
        other => panic!("SHM_INFO failed: {other:?}"),
    };
    assert_eq!(shm_info_ret, highest_index);
    assert!(shm_info.used_ids >= 1);
    assert!(shm_info.shm_tot >= 1);
}

#[test]
fn dispatch_sysv_shmctl_shm_stat_uses_index_not_shmid() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let shmid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMGET,
            [
                0x53484d33,
                4096,
                (sysv_shm::execution::IPC_CREAT | 0o640) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("shmget failed: {other:?}"),
    };

    let mut limits = ShminfoLayout::default();
    let highest_index = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMCTL,
            [
                0,
                sysv_shm::execution::IPC_INFO as u64,
                (&mut limits as *mut ShminfoLayout) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(index) => index,
        other => panic!("IPC_INFO failed: {other:?}"),
    };

    let mut ds = ShmidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SHMCTL,
                [
                    highest_index as u64,
                    sysv_shm::execution::SHM_STAT as u64,
                    (&mut ds as *mut ShmidDsLayout) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(shmid as i64)
    );
    assert_eq!(ds.shm_perm.key, 0x53484d33);
}

#[test]
fn dispatch_sysv_shmat_maps_pagebacked_vma_and_shmdt_unmaps_it() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    let shmid = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SHMGET,
            [
                0x53484d32,
                4096,
                (sysv_shm::execution::IPC_CREAT | 0o600) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(id) => id as u64,
        other => panic!("shmget failed: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SHMAT, [shmid, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(USER_PAGE_SIZE as i64)
    );
    let entry = ctx
        .aspace
        .lookup(tx_subsystems::vm::UserVirtAddr(USER_PAGE_SIZE))
        .expect("shmat installs VMA recipe");
    assert_eq!(entry.prot, Prot::READ_WRITE);
    assert!(entry.flags.shared);
    assert!(matches!(entry.backing, VmBacking::Page { offset: 0, .. }));

    let mut ds = ShmidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SHMCTL,
                [
                    shmid,
                    sysv_shm::execution::IPC_STAT as u64,
                    (&mut ds as *mut ShmidDsLayout) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(ds.shm_nattch, 1);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SHMDT, [USER_PAGE_SIZE as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(ctx
        .aspace
        .lookup(tx_subsystems::vm::UserVirtAddr(USER_PAGE_SIZE))
        .is_none());

    let mut after = ShmidDsLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SHMCTL,
                [
                    shmid,
                    sysv_shm::execution::IPC_STAT as u64,
                    (&mut after as *mut ShmidDsLayout) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(after.shm_nattch, 0);
}
