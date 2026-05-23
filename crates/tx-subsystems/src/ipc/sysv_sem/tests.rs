use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL};
use crate::process::bootstrap_init_process;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::vm::AddressSpace;
use crate::zones;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn cred(uid: u32, gid: u32) -> crate::process::adapter::step_engine::Cap<Cred> {
    crate::cred::sign_cred(Cred {
        uid: Uid(uid),
        euid: Uid(uid),
        suid: Uid(uid),
        gid: Gid(gid),
        egid: Gid(gid),
        sgid: Gid(gid),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    })
    .expect("cred cap")
}

#[test]
fn sem_namespace_entry_is_identity_cap_authority() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let key = 0x53454d01;
    let semid = execution::step_semget(key, 2, IPC_CREAT | IPC_EXCL | 0o600, &owner, &ns)
        .expect("semget keyed");

    let namespace_array = ns
        .ipc_ns
        .sysv_sem
        .lock()
        .get(&crate::process::nsproxy::SysvKey::new(key as u32))
        .expect("namespace entry")
        .clone();

    assert_eq!(namespace_array.semid, semid);
    assert_eq!(
        namespace_array.key().raw(),
        structure::lookup_sem(semid)
            .expect("global compatibility registry")
            .key()
            .raw(),
        "IpcNamespace.sysv_sem must be the authority for the sem identity cap"
    );

    execution::step_semctl_in_ns(
        semid,
        0,
        execution::IPC_RMID,
        execution::SemCtlArg::None,
        &owner,
        &ns,
        None,
    )
    .expect("cleanup");
}

#[test]
fn semctl_getpid_tracks_setall_and_semop_last_modifier() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let process = bootstrap_init_process(AddressSpace::new_cap().expect("aspace cap"))
        .expect("bootstrap init");
    let semid =
        execution::step_semget(IPC_EXCL, 2, IPC_CREAT | 0o600, &owner, &ns).expect("semget");

    execution::step_semctl(
        semid,
        0,
        execution::SETALL,
        execution::SemCtlArg::All(alloc::vec![2, 3]),
        &owner,
        Some(&process),
    )
    .expect("SETALL");

    for semnum in [0, 1] {
        match execution::step_semctl(
            semid,
            semnum,
            execution::GETPID,
            execution::SemCtlArg::None,
            &owner,
            None,
        )
        .expect("GETPID after SETALL")
        {
            execution::SemCtlResult::Val(pid) => assert_eq!(pid, process.pid.0 as i32),
            other => panic!("expected Val, got {other:?}"),
        }
    }

    execution::step_semop(
        semid,
        &[structure::SemBuf {
            sem_num: 1,
            sem_op: -1,
            sem_flg: 0,
        }],
        &owner,
        &process,
    )
    .expect("semop");

    match execution::step_semctl(
        semid,
        1,
        execution::GETPID,
        execution::SemCtlArg::None,
        &owner,
        None,
    )
    .expect("GETPID after semop")
    {
        execution::SemCtlResult::Val(pid) => assert_eq!(pid, process.pid.0 as i32),
        other => panic!("expected Val, got {other:?}"),
    }

    execution::step_semctl_in_ns(
        semid,
        0,
        execution::IPC_RMID,
        execution::SemCtlArg::None,
        &owner,
        &ns,
        Some(&process),
    )
    .expect("cleanup");
}
