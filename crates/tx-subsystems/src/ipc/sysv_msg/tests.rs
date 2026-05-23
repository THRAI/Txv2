use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL};
use crate::test_support::EPOCH_TEST_LOCK;
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
fn msg_namespace_entry_is_identity_cap_authority() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let key = 0x4d534701;
    let msqid = execution::step_msgget(key, IPC_CREAT | IPC_EXCL | 0o600, &owner, &ns)
        .expect("msgget keyed");

    let namespace_queue = ns
        .ipc_ns
        .sysv_msg
        .lock()
        .get(&crate::process::nsproxy::SysvKey::new(key as u32))
        .expect("namespace entry")
        .clone();

    assert_eq!(namespace_queue.msqid, msqid);
    assert_eq!(
        namespace_queue.key().raw(),
        structure::lookup_msg(msqid)
            .expect("global compatibility registry")
            .key()
            .raw(),
        "IpcNamespace.sysv_msg must be the authority for the msg identity cap"
    );

    execution::step_msgctl_in_ns(msqid, execution::IPC_RMID, None, &owner, &ns).expect("cleanup");
}
