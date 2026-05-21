use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::execution::Errno;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::zones;

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

#[test]
fn test_shm_lifecycle_and_ctl() {
    let _g = setup();

    // 1. Create a credential for user 1000, group 1000
    let cred_struct = Cred {
        uid: Uid(1000),
        euid: Uid(1000),
        suid: Uid(1000),
        gid: Gid(1000),
        egid: Gid(1000),
        sgid: Gid(1000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let cred_cap = crate::cred::sign_cred(cred_struct).expect("cred cap");
    let ns_cap = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");

    // 2. Create a private segment (key = 0, size = 4096)
    let shmid =
        execution::step_shmget(0, 4096, 0o666 | 0x200, &cred_cap, &ns_cap).expect("shmget private");
    assert!(shmid > 0);

    // 3. STAT the segment
    let res =
        execution::step_shmctl(shmid, execution::IPC_STAT, None, &cred_cap).expect("shmctl stat");
    let info = match res {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {:?}", other),
    };
    assert_eq!(info.shmid, shmid);
    assert_eq!(info.size, 4096);
    assert_eq!(info.perm.mode, 0o666);
    assert_eq!(info.cuid, 1000);
    assert_eq!(info.cgid, 1000);
    assert_eq!(info.attach_count, 0);

    // 4. Update owner/permissions via IPC_SET (change mode to 0o600, uid to 2000, gid to 2000)
    execution::step_shmctl(
        shmid,
        execution::IPC_SET,
        Some((0o600, 2000, 2000)),
        &cred_cap,
    )
    .expect("shmctl set");

    // 5. STAT again and verify modifications
    let res2 =
        execution::step_shmctl(shmid, execution::IPC_STAT, None, &cred_cap).expect("shmctl stat 2");
    let info2 = match res2 {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {:?}", other),
    };
    assert_eq!(info2.perm.mode, 0o600);
    assert_eq!(info2.cuid, 1000); // creator remains 1000
    assert_eq!(info2.cgid, 1000); // creator group remains 1000

    // 6. Test permissions checks with a different credential (user 3000, group 3000)
    let cred3_struct = Cred {
        uid: Uid(3000),
        euid: Uid(3000),
        suid: Uid(3000),
        gid: Gid(3000),
        egid: Gid(3000),
        sgid: Gid(3000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let cred3_cap = crate::cred::sign_cred(cred3_struct).expect("cred3 cap");
    let segment = checks::require_shm_exists(shmid).expect("shm exists");

    // Mode is 0o600, and owner is now 2000. User 3000 is not owner/creator, so EACCES.
    let err_read = checks::require_can_read_shm(&segment, &*cred3_cap).expect_err("should fail");
    assert_eq!(err_read, Errno::EACCES);

    // If owner (user 2000) calls, it should succeed
    let cred_owner_struct = Cred {
        uid: Uid(2000),
        euid: Uid(2000),
        suid: Uid(2000),
        gid: Gid(2000),
        egid: Gid(2000),
        sgid: Gid(2000),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    };
    let cred_owner_cap = crate::cred::sign_cred(cred_owner_struct).expect("cred_owner cap");
    checks::require_can_read_shm(&segment, &*cred_owner_cap).expect("owner should read");
    checks::require_can_write_shm(&segment, &*cred_owner_cap).expect("owner should write");

    // 7. RMID the segment
    execution::step_shmctl(shmid, execution::IPC_RMID, None, &cred_cap).expect("shmctl rmid");

    // Verify it cannot be STATed anymore (lookup returns EINVAL)
    let err_stat = execution::step_shmctl(shmid, execution::IPC_STAT, None, &cred_cap)
        .expect_err("should fail");
    assert_eq!(err_stat, Errno::EINVAL);
}
