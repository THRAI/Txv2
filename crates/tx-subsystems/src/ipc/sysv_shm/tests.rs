use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::execution::Errno;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::vm::{
    AddressSpace, MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest, USER_PAGE_SIZE,
};
use crate::zones;
use core::future::Future;
use core::pin::Pin;
use core::ptr::null;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(null(), &NOOP_WAKER_VTABLE),
    |_| {},
    |_| {},
    |_| {},
);

fn noop_waker() -> Waker {
    unsafe { Waker::from_raw(RawWaker::new(null(), &NOOP_WAKER_VTABLE)) }
}

fn block_on<F: Future>(mut future: F) -> F::Output {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // Safety: the future is stack-pinned for the duration of this helper.
    let mut pinned = unsafe { Pin::new_unchecked(&mut future) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => {}
        }
    }
    panic!("sysv_shm test block_on: future did not resolve in 1024 polls");
}

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
fn shmat_maps_pagebacked_segment_and_shmdt_tracks_attach_count() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let aspace = AddressSpace::new_cap().expect("aspace cap");
    let shmid = execution::step_shmget(
        execution::IPC_PRIVATE,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("shmget private");

    let addr = block_on(execution::step_shmat(shmid, 0, 0, &creator, &aspace)).expect("shmat");
    assert_eq!(addr, USER_PAGE_SIZE);
    let entry = aspace
        .lookup(UserVirtAddr(addr))
        .expect("shmat installs VMA");
    assert_eq!(entry.prot, Prot::READ_WRITE);
    assert!(entry.flags.shared);
    assert!(matches!(entry.backing, VmBacking::Page { offset: 0, .. }));

    let attached = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat after attach")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(attached.attach_count, 1);

    block_on(execution::step_shmdt(addr, &aspace)).expect("shmdt");
    assert!(aspace.lookup(UserVirtAddr(addr)).is_none());
    let detached = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat after detach")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(detached.attach_count, 0);

    execution::step_shmctl(shmid, execution::IPC_RMID, None, &creator).expect("rmid");
}

#[test]
fn shm_namespace_entry_is_identity_cap_authority() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let key = 0x5348_4d01;
    let shmid = execution::step_shmget(
        key,
        4096,
        execution::IPC_CREAT | execution::IPC_EXCL | 0o600,
        &creator,
        &ns,
    )
    .expect("shmget keyed");

    let namespace_segment = ns
        .ipc_ns
        .sysv_shm
        .lock()
        .get(&crate::process::nsproxy::SysvKey::new(key as u32))
        .expect("namespace entry")
        .clone();

    assert_eq!(namespace_segment.shmid, shmid);
    assert_eq!(
        namespace_segment.key().raw(),
        crate::ipc::sysv_shm::structure::lookup_shm(shmid)
            .expect("global compatibility registry")
            .key()
            .raw(),
        "IpcNamespace.sysv_shm must be the authority for the shm identity cap"
    );

    execution::step_shmctl_in_ns(shmid, execution::IPC_RMID, None, &creator, &ns).expect("cleanup");
}

#[test]
fn shmdt_rejects_same_address_mapping_from_other_address_space() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let attached_aspace = AddressSpace::new_cap().expect("attached aspace cap");
    let other_aspace = AddressSpace::new_cap().expect("other aspace cap");
    let shmid = execution::step_shmget(
        execution::IPC_PRIVATE,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("shmget private");
    let addr = block_on(execution::step_shmat(
        shmid,
        0,
        0,
        &creator,
        &attached_aspace,
    ))
    .expect("shmat");

    let same_range = UserRange::new_aligned(UserVirtAddr(addr), USER_PAGE_SIZE).expect("range");
    other_aspace
        .try_mmap(VmMapRequest::fixed(
            same_range,
            MapPlacement::RequireFree,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("unrelated mapping in other address space");

    assert_eq!(
        block_on(execution::step_shmdt(addr, &other_aspace))
            .expect_err("wrong address space rejects"),
        Errno::EINVAL
    );
    assert!(other_aspace.lookup(UserVirtAddr(addr)).is_some());
    assert!(attached_aspace.lookup(UserVirtAddr(addr)).is_some());
    let still_attached = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat after rejected detach")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(still_attached.attach_count, 1);

    block_on(execution::step_shmdt(addr, &attached_aspace)).expect("real detach");
    execution::step_shmctl(shmid, execution::IPC_RMID, None, &creator).expect("rmid");
}

#[test]
fn shmdt_keeps_attach_records_keyed_by_address_space_identity() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let first_aspace = AddressSpace::new_cap().expect("first aspace cap");
    let second_aspace = AddressSpace::new_cap().expect("second aspace cap");
    let shmid = execution::step_shmget(
        execution::IPC_PRIVATE,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("shmget private");

    let fixed_addr = USER_PAGE_SIZE;
    let first_addr = block_on(execution::step_shmat(
        shmid,
        fixed_addr,
        0,
        &creator,
        &first_aspace,
    ))
    .expect("first shmat");
    let second_addr = block_on(execution::step_shmat(
        shmid,
        fixed_addr,
        0,
        &creator,
        &second_aspace,
    ))
    .expect("second shmat");
    assert_eq!(first_addr, fixed_addr);
    assert_eq!(second_addr, fixed_addr);

    block_on(execution::step_shmdt(second_addr, &second_aspace)).expect("detach second");

    let segment = checks::require_shm_exists(shmid).expect("segment exists");
    let attaches = segment.payload.attaches.lock();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].aspace_key, first_aspace.key().raw());
    assert_eq!(attaches[0].addr, fixed_addr);
    drop(attaches);

    assert!(first_aspace.lookup(UserVirtAddr(fixed_addr)).is_some());
    assert!(second_aspace.lookup(UserVirtAddr(fixed_addr)).is_none());
    let still_attached = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat after one detach")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(still_attached.attach_count, 1);

    block_on(execution::step_shmdt(first_addr, &first_aspace)).expect("detach first");
    execution::step_shmctl(shmid, execution::IPC_RMID, None, &creator).expect("rmid");
}

#[test]
fn shmctl_ipc_set_updates_owner_group_and_mode_without_rewriting_creator() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let shmid = execution::step_shmget(
        execution::IPC_PRIVATE,
        4096,
        execution::IPC_CREAT | 0o666,
        &creator,
        &ns,
    )
    .expect("shmget private");

    let before = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat before")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(before.perm.mode, 0o666);
    assert_eq!(before.cuid, 1000);
    assert_eq!(before.cgid, 1000);
    assert_eq!(before.uid, 1000);
    assert_eq!(before.gid, 1000);

    execution::step_shmctl(
        shmid,
        execution::IPC_SET,
        Some((0o600, 2000, 2000)),
        &creator,
    )
    .expect("ipc set");

    let after = match execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
        .expect("stat after")
    {
        execution::ShmCtlResult::Stat(info) => info,
        other => panic!("expected Stat, got {other:?}"),
    };
    assert_eq!(after.perm.mode, 0o600);
    assert_eq!(after.cuid, 1000);
    assert_eq!(after.cgid, 1000);
    assert_eq!(after.uid, 2000);
    assert_eq!(after.gid, 2000);

    let other = cred(3000, 3000);
    let segment = checks::require_shm_exists(shmid).expect("segment exists");
    assert_eq!(
        checks::require_can_read_shm(&segment, &other).expect_err("other should not read"),
        Errno::EACCES
    );

    let owner = cred(2000, 2000);
    checks::require_can_read_shm(&segment, &owner).expect("new owner can read");
    checks::require_can_write_shm(&segment, &owner).expect("new owner can write");

    execution::step_shmctl(shmid, execution::IPC_RMID, None, &creator).expect("rmid");
    assert_eq!(
        execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
            .expect_err("removed segment should not stat"),
        Errno::EINVAL
    );
}

#[test]
fn ipc_rmid_keeps_attached_segment_detachable_until_last_shmdt() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let aspace = AddressSpace::new_cap().expect("aspace cap");
    let shmid = execution::step_shmget(
        execution::IPC_PRIVATE,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("shmget private");
    let addr = block_on(execution::step_shmat(shmid, 0, 0, &creator, &aspace)).expect("shmat");

    execution::step_shmctl(shmid, execution::IPC_RMID, None, &creator).expect("rmid");
    assert_eq!(
        block_on(execution::step_shmat(shmid, 0, 0, &creator, &aspace))
            .expect_err("new attaches reject after rmid"),
        Errno::EIDRM
    );

    block_on(execution::step_shmdt(addr, &aspace)).expect("attached mapping stays detachable");
    assert!(aspace.lookup(UserVirtAddr(addr)).is_none());
    assert_eq!(
        execution::step_shmctl(shmid, execution::IPC_STAT, None, &creator)
            .expect_err("last detach reclaims removed segment"),
        Errno::EINVAL
    );
}

#[test]
fn ipc_rmid_withdraws_keyed_segment_from_namespace_for_recreate() {
    let _g = setup();

    let creator = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let shmid = execution::step_shmget(
        0x5348_4d31,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("first keyed shmget");

    execution::step_shmctl_in_ns(shmid, execution::IPC_RMID, None, &creator, &ns).expect("rmid");

    let recreated = execution::step_shmget(
        0x5348_4d31,
        4096,
        execution::IPC_CREAT | 0o600,
        &creator,
        &ns,
    )
    .expect("key should be reusable after rmid");
    assert_ne!(recreated, shmid);

    execution::step_shmctl_in_ns(recreated, execution::IPC_RMID, None, &creator, &ns)
        .expect("cleanup recreated segment");
}
