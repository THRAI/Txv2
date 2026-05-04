use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use tx_substrate::bus::RawQueue;
use tx_substrate::{epoch, zone};

use super::*;
use crate::mount::structure::testing::{
    make_bootstrap_pair_for_test, make_bootstrap_pair_with_backend_for_test,
    make_bootstrap_pair_with_fs_ops_for_test, make_dentry_for_test, make_payload_for_test,
    make_rnode_for_test,
};
use crate::page_backed::FsPageBacking;
use crate::vfs::checks::require::{require_parent_and_name, ResolveCtx};
use crate::vfs::checks::resolution::state::RootCtxCaps;
use crate::vfs::fs_ops::{Credential, DirCursor, DirEntry, FsOps};
use crate::vfs::structure::{DEntryChildLookup, InodeMeta, NameOwned};

mod create_backend;
mod ext4_backend;
mod read_backend;

use ext4_backend::Ext4MemImage;

struct CreateOnlyFs;
struct BlockThenCreateFs {
    queue: RawQueue,
    blocked_once: AtomicBool,
}
struct AdvanceThenBlockCreateFs {
    queue: RawQueue,
    blocked_once: AtomicBool,
}
struct AdvanceTwiceThenBlockThenCreateFs {
    queue: RawQueue,
    call_count: core::sync::atomic::AtomicU8,
}
struct LookupReadFs;
struct BlockThenReadFs {
    queue: RawQueue,
    blocked_once: AtomicBool,
}
struct AdvanceThenBlockReadFs {
    queue: RawQueue,
    second_page_calls: core::sync::atomic::AtomicU8,
}
struct BackendInterfaceFs {
    backing: BackendInterfaceBacking,
}
enum BackendInterfaceBacking {
    ProvidedPc(Cap<crate::page_backed::PageContainer>),
    Projected,
    StructBacked,
}
struct TestProjectionSchema;
struct TestSpinMutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}
struct TestSpinMutexGuard<'a, T> {
    mutex: &'a TestSpinMutex<T>,
}
struct Ext4FormatReadFs {
    pager: TestSpinMutex<tx_ext4_format::pager::Ext4Pager<Ext4MemImage>>,
}

fn test_inode_meta(size: u64) -> InodeMeta {
    InodeMeta {
        mode: InodeMeta::TYPE_REGULAR | 0o644,
        uid: 1,
        gid: 2,
        size,
        atime: crate::vfs::structure::Timespec { sec: 1, nsec: 0 },
        mtime: crate::vfs::structure::Timespec { sec: 2, nsec: 0 },
        ctime: crate::vfs::structure::Timespec { sec: 3, nsec: 0 },
        nlinks: 1,
        blocks: 1,
        flags: 0,
    }
}

fn backend_interface_ctx(fs: Arc<BackendInterfaceFs>) -> (ResolveCtx, Cap<DEntry>) {
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    (ctx, root)
}

fn setup() {
    tx_substrate::testing::init_host_for_test_once();
    let _ = zone::register_zone_for::<crate::vfs::structure::RNode>();
    let _ = zone::register_zone_for::<crate::vfs::structure::DEntry>();
    let _ = zone::register_zone_for::<crate::vfs::structure::OpenFile>();
    let _ = zone::register_zone_for::<crate::mount::structure::MountIdentity>();
    let _ = zone::register_zone_for::<crate::mount::structure::MountNamespace>();
    let _ = zone::register_zone_for::<crate::mount::structure::MountPayload>();
    let _ = zone::register_zone_for::<crate::page_backed::PageContainer>();
}

fn make_ctx_for_empty_root() -> (
    ResolveCtx,
    tx_substrate::zone::Cap<crate::vfs::structure::DEntry>,
) {
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root_cap = make_dentry_for_test(70, b".", root_rn);
    let (mi_cap, ns_cap) = make_bootstrap_pair_for_test(root_cap.clone());

    (
        ResolveCtx::new(RootCtxCaps {
            mnt_ns: ns_cap,
            mnt_ns_root: root_cap.clone(),
            chroot: None,
            cwd: root_cap.clone(),
            root_mount: mi_cap.clone(),
            cwd_mount: mi_cap,
        }),
        root_cap,
    )
}

#[test]
fn open_create_upgrade_helper_extracts_stable_inputs() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let (ctx, _) = make_ctx_for_empty_root();
    let guard = epoch::guard();
    let witness = require_parent_and_name(b"/new", &ctx, &guard).expect("parent witness");

    let ready = upgrade_open_create_parent(witness).expect("upgrade ready");

    assert_eq!(ready.name.as_bytes(), b"new");
    assert!(ready.parent.raw() != 0);
    assert!(ready.parent_mount.raw() != 0);
}

#[test]
fn open_create_reserve_helper_reaches_insert_reservation() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let (ctx, _) = make_ctx_for_empty_root();
    let guard = epoch::guard();
    let witness = require_parent_and_name(b"/new", &ctx, &guard).expect("parent witness");
    let ready = upgrade_open_create_parent(witness).expect("upgrade ready");

    let reserved_name = with_open_create_insert_reservation(&ready, |ready, reservation| {
        assert_eq!(reservation.key().as_bytes(), b"new");
        StepOutcome::Done(ready.name.clone())
    });
    let reserved_name = match reserved_name {
        StepOutcome::Done(name) => name,
        _ => panic!("expected reserve insert success"),
    };

    assert_eq!(reserved_name.as_bytes(), b"new");
}

#[test]
fn step_open_create_returns_open_file_after_commit() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_fs_ops_for_test(root.clone(), Arc::new(CreateOnlyFs));
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let guard = epoch::guard();
    let witness = require_parent_and_name(b"/new", &ctx, &guard).expect("parent witness");
    let mut resume = None;

    let outcome = step_open_create(witness, &guard, &mut resume);
    let open = match outcome {
        StepOutcome::Done(open) => open,
        _ => panic!("expected open file success"),
    };
    assert!(resume.is_none());
    drop(guard);

    let check_guard = epoch::guard();
    let root_ref = root.ident_ref(&check_guard);
    let child_rnode_raw = match root_ref.children.lookup(
        &NameOwned::from_component(b"new").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => (*entry).into_ident_ref().rnode.raw(),
        DEntryChildLookup::Missing => panic!("expected committed child"),
    };
    let root_ref = root.ident_ref(&check_guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"new").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert_eq!(entry.name().as_bytes(), b"new");
        }
        DEntryChildLookup::Missing => panic!("expected committed child"),
    }

    let open_ref = open.ident_ref(&check_guard);
    assert_eq!(open_ref.mount.raw(), mount.raw());
    assert!(open_ref.flags.read);
    assert_eq!(open_ref.rnode.raw(), child_rnode_raw);
    assert_eq!(
        open_ref.rnode.fs_object_id,
        FsObjectId(
            open_ref.mount.root_dentry.rnode.fs_object_id.0
                ^ 0x4444_0000_0000_0000
                ^ u64::from(b'n')
        )
    );
    assert_eq!(open_ref.rnode.meta.mode, InodeMeta::TYPE_REGULAR | 0o600);
    assert_eq!(open_ref.rnode.meta.uid, 12);
    assert_eq!(open_ref.rnode.meta.gid, 34);
}

#[test]
fn step_open_create_retries_after_backend_blocked() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(80, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(80, b".", root_rn);
    let fs = Arc::new(BlockThenCreateFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(false),
    });
    let (mount, ns, _payload) = make_bootstrap_pair_with_fs_ops_for_test(root.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });

    let first_guard = epoch::guard();
    let first_witness =
        require_parent_and_name(b"/later", &ctx, &first_guard).expect("parent witness");
    let mut resume = None;

    let first = step_open_create(first_witness, &first_guard, &mut resume);
    match first {
        StepOutcome::Blocked(crate::step::WakeCarrier::Queue(_), interests) => {
            assert_eq!(interests.bits, 0x10);
        }
        _ => panic!("expected backend blocked"),
    }
    assert!(matches!(
        resume,
        Some(OpenCreateResume::BackendCreateInFlight(_))
    ));
    drop(first_guard);

    let second_guard = epoch::guard();
    let second_witness =
        require_parent_and_name(b"/later", &ctx, &second_guard).expect("parent witness");
    let second = step_open_create(second_witness, &second_guard, &mut resume);
    let open = match second {
        StepOutcome::Done(open) => open,
        _ => panic!("expected resumed open file success"),
    };
    assert!(resume.is_none());
    drop(second_guard);

    let check_guard = epoch::guard();
    let root_ref = root.ident_ref(&check_guard);
    let child_rnode_raw = match root_ref.children.lookup(
        &NameOwned::from_component(b"later").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => (*entry).into_ident_ref().rnode.raw(),
        DEntryChildLookup::Missing => panic!("expected committed child after resume"),
    };
    let open_ref = open.ident_ref(&check_guard);
    assert_eq!(open_ref.rnode.raw(), child_rnode_raw);
    assert_eq!(open_ref.rnode.meta.mode, InodeMeta::TYPE_REGULAR | 0o640);
    assert_eq!(open_ref.rnode.meta.uid, 56);
    assert_eq!(open_ref.rnode.meta.gid, 78);
}

#[test]
fn step_open_create_retries_after_backend_advanced_then_blocked() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(90, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(90, b".", root_rn);
    let fs = Arc::new(AdvanceThenBlockCreateFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(false),
    });
    let (mount, ns, _payload) = make_bootstrap_pair_with_fs_ops_for_test(root.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });

    let first_guard = epoch::guard();
    let first_witness =
        require_parent_and_name(b"/again", &ctx, &first_guard).expect("parent witness");
    let mut resume = None;

    let first = step_open_create(first_witness, &first_guard, &mut resume);
    match first {
        StepOutcome::AdvancedThenBlocked(
            Progress::Units(1),
            crate::step::WakeCarrier::Queue(_),
            interests,
        ) => {
            assert_eq!(interests.bits, 0x20);
        }
        _ => panic!("expected backend advanced-then-blocked"),
    }
    assert!(matches!(
        resume,
        Some(OpenCreateResume::BackendCreateInFlight(_))
    ));
    drop(first_guard);

    let second_guard = epoch::guard();
    let second_witness =
        require_parent_and_name(b"/again", &ctx, &second_guard).expect("parent witness");
    let second = step_open_create(second_witness, &second_guard, &mut resume);
    let open = match second {
        StepOutcome::Done(open) => open,
        _ => panic!("expected resumed open file success"),
    };
    assert!(resume.is_none());
    drop(second_guard);

    let check_guard = epoch::guard();
    let root_ref = root.ident_ref(&check_guard);
    let child_rnode_raw = match root_ref.children.lookup(
        &NameOwned::from_component(b"again").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => (*entry).into_ident_ref().rnode.raw(),
        DEntryChildLookup::Missing => panic!("expected committed child after composite resume"),
    };
    let open_ref = open.ident_ref(&check_guard);
    assert_eq!(open_ref.rnode.raw(), child_rnode_raw);
    assert_eq!(open_ref.rnode.meta.mode, InodeMeta::TYPE_REGULAR | 0o644);
    assert_eq!(open_ref.rnode.meta.uid, 90);
    assert_eq!(open_ref.rnode.meta.gid, 91);
}

#[test]
fn drive_open_create_retries_to_completion() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(100, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(100, b".", root_rn);
    let fs = Arc::new(BlockThenCreateFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(false),
    });
    let (mount, ns, _payload) = make_bootstrap_pair_with_fs_ops_for_test(root.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let mut op = OpenCreateOperation::new(b"/driver").expect("valid path");

    let first = drive_open_create(&mut op, &ctx);
    match first {
        StepOutcome::Blocked(crate::step::WakeCarrier::Queue(_), interests) => {
            assert_eq!(interests.bits, 0x10);
        }
        _ => panic!("expected driver-facing blocked"),
    }
    assert!(op.is_waiting());

    let second = drive_open_create(&mut op, &ctx);
    let open = match second {
        StepOutcome::Done(open) => open,
        _ => panic!("expected driver-facing completion"),
    };
    assert!(!op.is_waiting());

    let check_guard = epoch::guard();
    let root_ref = root.ident_ref(&check_guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"driver").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert_eq!(entry.name().as_bytes(), b"driver");
            assert_eq!(
                (*entry).into_ident_ref().rnode.raw(),
                open.ident_ref(&check_guard).rnode.raw()
            );
        }
        DEntryChildLookup::Missing => panic!("expected child after driver-facing completion"),
    }
}

#[test]
fn step_open_create_rejects_resumed_different_name_context() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(110, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(110, b".", root_rn);
    let fs = Arc::new(BlockThenCreateFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(false),
    });
    let (mount, ns, _payload) = make_bootstrap_pair_with_fs_ops_for_test(root.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });

    let first_guard = epoch::guard();
    let first_witness =
        require_parent_and_name(b"/later", &ctx, &first_guard).expect("parent witness");
    let mut resume = None;
    let first = step_open_create(first_witness, &first_guard, &mut resume);
    match first {
        StepOutcome::Blocked(crate::step::WakeCarrier::Queue(_), interests) => {
            assert_eq!(interests.bits, 0x10);
        }
        _ => panic!("expected backend blocked"),
    }
    drop(first_guard);

    let second_guard = epoch::guard();
    let second_witness =
        require_parent_and_name(b"/other", &ctx, &second_guard).expect("parent witness");
    let second = step_open_create(second_witness, &second_guard, &mut resume);
    assert!(matches!(second, StepOutcome::Err(Errno::Stale)));
    assert!(resume.is_none());
}

#[test]
fn drive_open_create_until_boundary_coalesces_immediate_progress_before_wait() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(120, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(120, b".", root_rn);
    let fs = Arc::new(AdvanceTwiceThenBlockThenCreateFs {
        queue: RawQueue::new(),
        call_count: core::sync::atomic::AtomicU8::new(0),
    });
    let (mount, ns, _payload) = make_bootstrap_pair_with_fs_ops_for_test(root.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let mut op = OpenCreateOperation::new(b"/coalesced").expect("valid path");

    let first = drive_open_create_until_boundary(&mut op, &ctx);
    match first {
        StepOutcome::AdvancedThenBlocked(
            Progress::Units(2),
            crate::step::WakeCarrier::Queue(_),
            interests,
        ) => {
            assert_eq!(interests.bits, 0x40);
        }
        _ => panic!("expected coalesced progress before wait"),
    }
    assert!(op.is_waiting());

    let second = drive_open_create_until_boundary(&mut op, &ctx);
    let open = match second {
        StepOutcome::Done(open) => open,
        _ => panic!("expected completion after wake"),
    };
    assert!(!op.is_waiting());

    let check_guard = epoch::guard();
    let root_ref = root.ident_ref(&check_guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"coalesced").expect("valid name"),
        &check_guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert_eq!(entry.name().as_bytes(), b"coalesced");
            let open_ref = open.ident_ref(&check_guard);
            assert_eq!((*entry).into_ident_ref().rnode.raw(), open_ref.rnode.raw());
            assert_eq!(open_ref.rnode.meta.mode, InodeMeta::TYPE_REGULAR | 0o666);
        }
        DEntryChildLookup::Missing => panic!("expected child after boundary-driven completion"),
    }
}

#[test]
fn drive_open_read_first_page_completes_cold_lookup_and_returns_bytes() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(LookupReadFs);
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let mut target = [0u8; 10];
    let mut op = OpenReadOperation::new(b"/cold-read", 0, &mut target).expect("valid path");

    let outcome = drive_open_read_first_page(&mut op, &ctx);
    let frame = match outcome {
        StepOutcome::Done(frame) => frame,
        _ => panic!("expected read completion"),
    };
    assert_eq!(frame.as_bytes(), b"tx-read-ok");

    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"cold-read").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            let child = (*entry).into_ident_ref();
            assert_eq!(child.name.as_bytes(), b"cold-read");
            assert_eq!(child.rnode.fs_object_id, FsObjectId(0x5151));
            assert!(matches!(
                child.rnode.backing,
                RNodeBacking::PageBacked { .. }
            ));
        }
        DEntryChildLookup::Missing => panic!("expected cold lookup materialization"),
    }
}

#[test]
fn drive_open_read_completes_cold_lookup_and_returns_requested_bytes() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(LookupReadFs);
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let mut target = [0u8; 4];
    let mut op = OpenReadOperation::new(b"/cold-read", 3, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    let read = match outcome {
        StepOutcome::Done(read) => read,
        _ => panic!("expected read completion"),
    };

    assert_eq!(read, 4);
    assert_eq!(&target[..read], b"read");
}

#[test]
fn default_backing_hook_falls_back_to_file_page_backed() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(LookupReadFs);
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let mut target = [0u8; 2];
    let mut op = OpenReadOperation::new(b"/cold-read", 0, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    assert!(matches!(outcome, StepOutcome::Done(2)));

    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"cold-read").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert!(matches!(
                (*entry).into_ident_ref().rnode.backing,
                RNodeBacking::PageBacked { .. }
            ));
        }
        DEntryChildLookup::Missing => panic!("expected fallback materialized child"),
    }
}

#[test]
fn backend_provided_page_container_is_materialized_as_page_backed() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let pc_payload = make_payload_for_test();
    let pc = create_file_page_container(pc_payload, FsObjectId(0x8282)).expect("pc");
    let fs = Arc::new(BackendInterfaceFs {
        backing: BackendInterfaceBacking::ProvidedPc(pc.clone()),
    });
    let (ctx, root) = backend_interface_ctx(fs);
    let mut target = [0u8; 8];
    let mut op = OpenReadOperation::new(b"/provided-pc", 0, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    let read = match outcome {
        StepOutcome::Done(read) => read,
        _ => panic!("expected backend-provided pc read completion"),
    };

    assert_eq!(&target[..read], b"provided");
    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"provided-pc").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => match &(*entry).into_ident_ref().rnode.backing {
            RNodeBacking::PageBacked { pc: materialized } => {
                assert_eq!(materialized.raw(), pc.raw());
            }
            _ => panic!("expected PageBacked child"),
        },
        DEntryChildLookup::Missing => panic!("expected backend-provided pc child"),
    }
}

#[test]
fn projected_backing_reads_through_projection_schema() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let fs = Arc::new(BackendInterfaceFs {
        backing: BackendInterfaceBacking::Projected,
    });
    let (ctx, root) = backend_interface_ctx(fs);
    let mut target = [0u8; 9];
    let mut op = OpenReadOperation::new(b"/projected", 10, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    let read = match outcome {
        StepOutcome::Done(read) => read,
        _ => panic!("expected projected read completion"),
    };

    assert_eq!(read, 9);
    assert_eq!(&target[..read], b"interface");
    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"projected").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert!(matches!(
                (*entry).into_ident_ref().rnode.backing,
                RNodeBacking::Projected { .. }
            ));
        }
        DEntryChildLookup::Missing => panic!("expected projected child"),
    }
}

#[test]
fn struct_backed_read_returns_not_implemented() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let fs = Arc::new(BackendInterfaceFs {
        backing: BackendInterfaceBacking::StructBacked,
    });
    let (ctx, root) = backend_interface_ctx(fs);
    let mut target = [0u8; 4];
    let mut op = OpenReadOperation::new(b"/struct-backed", 0, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    assert!(matches!(outcome, StepOutcome::Err(Errno::NotImplemented)));

    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"struct-backed").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert!(matches!(
                (*entry).into_ident_ref().rnode.backing,
                RNodeBacking::StructBacked { .. }
            ));
        }
        DEntryChildLookup::Missing => panic!("expected struct-backed child"),
    }
}

#[test]
fn drive_open_read_reads_across_page_boundary() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(BlockThenReadFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(true),
    });
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let offset = crate::page_backed::FRAME_CAPACITY as u64 - 4;
    let mut target = [0u8; 8];
    let mut op = OpenReadOperation::new(b"/blocked-read", offset, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    let read = match outcome {
        StepOutcome::Done(read) => read,
        _ => panic!("expected cross-page read completion"),
    };

    assert_eq!(read, 8);
    assert_eq!(&target[..read], b"aaaatail");
}

#[test]
fn drive_open_read_returns_advanced_then_blocked_and_resumes() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(BlockThenReadFs {
        queue: RawQueue::new(),
        blocked_once: AtomicBool::new(false),
    });
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let offset = crate::page_backed::FRAME_CAPACITY as u64 - 4;
    let mut target = [0u8; 8];
    let mut op = OpenReadOperation::new(b"/blocked-read", offset, &mut target).expect("valid path");

    let first = drive_open_read(&mut op, &ctx);
    match first {
        StepOutcome::AdvancedThenBlocked(
            Progress::Units(4),
            crate::step::WakeCarrier::Queue(_),
            interests,
        ) => {
            assert_eq!(interests.bits, 0x88);
        }
        _ => panic!("expected partial progress then blocked"),
    }

    let second = drive_open_read(&mut op, &ctx);
    let read = match second {
        StepOutcome::Done(read) => read,
        _ => panic!("expected resumed read completion"),
    };

    assert_eq!(read, 8);
    assert_eq!(&target[..read], b"aaaatail");
}

#[test]
fn drive_open_read_hides_backend_fetch_progress_from_read_progress() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(70, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(70, b".", root_rn);
    let fs = Arc::new(AdvanceThenBlockReadFs {
        queue: RawQueue::new(),
        second_page_calls: core::sync::atomic::AtomicU8::new(0),
    });
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let offset = crate::page_backed::FRAME_CAPACITY as u64 - 4;
    let mut target = [0u8; 8];
    let mut op =
        OpenReadOperation::new(b"/advance-block-read", offset, &mut target).expect("valid path");

    let first = drive_open_read(&mut op, &ctx);
    match first {
        StepOutcome::AdvancedThenBlocked(
            Progress::Units(4),
            crate::step::WakeCarrier::Queue(_),
            interests,
        ) => {
            assert_eq!(interests.bits, 0x99);
        }
        _ => panic!("expected only copied-byte progress before wait"),
    }

    let second = drive_open_read(&mut op, &ctx);
    let read = match second {
        StepOutcome::Done(read) => read,
        _ => panic!("expected resumed read completion"),
    };

    assert_eq!(read, 8);
    assert_eq!(&target[..read], b"bbbbtail");
}

#[test]
fn drive_open_read_reads_nested_file_from_ext4_format_backend() {
    let _serial = crate::test_support::EpochTestGuard::acquire();
    setup();
    let root_rn = make_rnode_for_test(2, InodeMeta::TYPE_DIRECTORY | 0o755);
    let root = make_dentry_for_test(2, b".", root_rn);
    let fs = Arc::new(Ext4FormatReadFs::new(ext4_read_image()));
    let (mount, ns, _payload) =
        make_bootstrap_pair_with_backend_for_test(root.clone(), fs.clone(), fs);
    let ctx = ResolveCtx::new(RootCtxCaps {
        mnt_ns: ns,
        mnt_ns_root: root.clone(),
        chroot: None,
        cwd: root.clone(),
        root_mount: mount.clone(),
        cwd_mount: mount.clone(),
    });
    let offset = tx_ext4_format::pager::BLOCK_SIZE as u64 - 8;
    let mut target = [0u8; 16];
    let mut op = OpenReadOperation::new(b"/nested/child", offset, &mut target).expect("valid path");

    let outcome = drive_open_read(&mut op, &ctx);
    let read = match outcome {
        StepOutcome::Done(read) => read,
        _ => panic!("expected ext4-backed VFS read completion"),
    };

    assert_eq!(read, 16);
    assert_eq!(&target[..8], &[0x20; 8]);
    assert_eq!(&target[8..read], &[0x21; 8]);

    let guard = epoch::guard();
    let root_ref = root.ident_ref(&guard);
    match root_ref.children.lookup(
        &NameOwned::from_component(b"nested").expect("valid component"),
        &guard,
    ) {
        DEntryChildLookup::Found(entry) => {
            assert_eq!((*entry).into_ident_ref().rnode.fs_object_id, FsObjectId(13));
        }
        DEntryChildLookup::Missing => panic!("expected ext4 directory dentry materialized"),
    }
}

fn ext4_read_image() -> Ext4MemImage {
    let mut blocks = vec![[0u8; tx_ext4_format::pager::BLOCK_SIZE]; 64];

    let sb = tx_ext4_format::ondisk::Superblock {
        inodes_count: 64,
        blocks_count: 64,
        log_block_size: 2,
        blocks_per_group: 64,
        inodes_per_group: 64,
        inode_size: 256,
        feature_incompat: tx_ext4_format::ondisk::Superblock::FEATURE_INCOMPAT_EXTENTS,
        feature_ro_compat: tx_ext4_format::ondisk::Superblock::FEATURE_RO_COMPAT_HUGE_FILE,
        journal_inode: 8,
        ..tx_ext4_format::ondisk::Superblock::default()
    };
    sb.encode(&mut blocks[0][1024..2048])
        .expect("superblock encode");

    tx_ext4_format::ondisk::GroupDesc {
        block_bitmap: 2,
        inode_bitmap: 3,
        inode_table: 4,
        free_blocks_count: 32,
        free_inodes_count: 52,
        used_dirs_count: 2,
        ..tx_ext4_format::ondisk::GroupDesc::default()
    }
    .encode(&mut blocks[1][..64])
    .expect("group desc encode");

    let mut journal_inode = tx_ext4_format::ondisk::Inode::default();
    journal_inode.mode = InodeMeta::TYPE_REGULAR | 0o600;
    journal_inode.size = 4 * tx_ext4_format::pager::BLOCK_SIZE as u64;
    journal_inode.blocks_512 = 32;
    journal_inode.links_count = 1;
    journal_inode.flags = tx_ext4_format::ondisk::Inode::EXTENTS_FL;
    journal_inode
        .set_extent_root(&[tx_ext4_format::ondisk::Extent {
            logical_block: 0,
            len: 4,
            physical_start: 40,
        }])
        .expect("journal extent");
    ext4_write_inode(&mut blocks, 8, &journal_inode);

    let mut root_inode = tx_ext4_format::ondisk::Inode::default();
    root_inode.mode = InodeMeta::TYPE_DIRECTORY | 0o755;
    root_inode.size = tx_ext4_format::pager::BLOCK_SIZE as u64;
    root_inode.blocks_512 = 8;
    root_inode.links_count = 3;
    root_inode.flags = tx_ext4_format::ondisk::Inode::EXTENTS_FL;
    root_inode
        .set_extent_root(&[tx_ext4_format::ondisk::Extent {
            logical_block: 0,
            len: 1,
            physical_start: 16,
        }])
        .expect("root extent");
    ext4_write_inode(&mut blocks, 2, &root_inode);

    let mut file_inode = tx_ext4_format::ondisk::Inode::default();
    file_inode.mode = InodeMeta::TYPE_REGULAR | 0o644;
    file_inode.size = 2 * tx_ext4_format::pager::BLOCK_SIZE as u64;
    file_inode.links_count = 1;
    file_inode.blocks_512 = 16;
    file_inode.flags = tx_ext4_format::ondisk::Inode::EXTENTS_FL;
    file_inode
        .set_extent_root(&[tx_ext4_format::ondisk::Extent {
            logical_block: 0,
            len: 2,
            physical_start: 20,
        }])
        .expect("file extent");
    ext4_write_inode(&mut blocks, 12, &file_inode);

    let mut nested_inode = tx_ext4_format::ondisk::Inode::default();
    nested_inode.mode = InodeMeta::TYPE_DIRECTORY | 0o755;
    nested_inode.size = tx_ext4_format::pager::BLOCK_SIZE as u64;
    nested_inode.blocks_512 = 8;
    nested_inode.links_count = 2;
    nested_inode.flags = tx_ext4_format::ondisk::Inode::EXTENTS_FL;
    nested_inode
        .set_extent_root(&[tx_ext4_format::ondisk::Extent {
            logical_block: 0,
            len: 1,
            physical_start: 17,
        }])
        .expect("nested extent");
    ext4_write_inode(&mut blocks, 13, &nested_inode);

    blocks[16] = ext4_directory_block(&[
        (2, 2, b".".as_slice()),
        (2, 2, b"..".as_slice()),
        (13, 2, b"nested".as_slice()),
    ]);
    blocks[17] = ext4_directory_block(&[
        (13, 2, b".".as_slice()),
        (2, 2, b"..".as_slice()),
        (12, 1, b"child".as_slice()),
    ]);
    blocks[20] = [0x20; tx_ext4_format::pager::BLOCK_SIZE];
    blocks[21] = [0x21; tx_ext4_format::pager::BLOCK_SIZE];

    Ext4MemImage::from_blocks(blocks)
}

fn ext4_write_inode(
    blocks: &mut [tx_ext4_format::pager::Page4K],
    ino: u32,
    inode: &tx_ext4_format::ondisk::Inode,
) {
    let index = (ino - 1) as usize;
    let offset = index * 256;
    let block = 4 + offset / tx_ext4_format::pager::BLOCK_SIZE;
    let in_block = offset % tx_ext4_format::pager::BLOCK_SIZE;
    inode
        .encode(&mut blocks[block][in_block..in_block + 256])
        .expect("inode encode");
}

fn ext4_directory_block(entries: &[(u32, u8, &[u8])]) -> tx_ext4_format::pager::Page4K {
    let mut block = [0u8; tx_ext4_format::pager::BLOCK_SIZE];
    let mut offset = 0usize;
    for (idx, (inode, file_type, name)) in entries.iter().enumerate() {
        let rec_len = if idx == entries.len() - 1 {
            (tx_ext4_format::pager::BLOCK_SIZE - offset) as u16
        } else {
            (8 + name.len()).next_multiple_of(4) as u16
        };
        tx_ext4_format::ondisk::encode_dir_entry(
            *inode,
            rec_len,
            *file_type,
            name,
            &mut block[offset..],
        )
        .expect("dir entry encode");
        offset += rec_len as usize;
    }
    block
}
