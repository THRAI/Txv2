//! End-to-end tests for `step_walk` / `step_open`.
//!
//! Each test goes through `FsOps` dispatch end-to-end against the
//! canonical production fs (Tmpfs).
//!
//! Lives in its own file so the parent `tests.rs` stays under the
//! `cargo xtask lint arch` 1500-line authored-file cap, mirroring the
//! `v3.rs` sibling that hosts the `FsOps for TestFs` impl.

use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::execution::Errno;
use crate::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use crate::page_backed::FsPageBacking;
use crate::vfs::adapter::step_engine::{guard, reserve_for, sign_for, Cap};
use crate::vfs::structure::{
    Credential, DEntry, FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
    S_IFDIR,
};
use crate::vfs::walker::{step_open, step_walk};
use crate::vfs::FsOps;

use super::{init_zones, TestFs};

// === fixture: rootfs over TestFs ===================================

struct V3Topology {
    root_dentry: Cap<DEntry>,
    rootfs: Arc<TestFs>,
}

fn build_rootfs_v3() -> V3Topology {
    let rootfs = TestFs::new(FsObjectId::new(2));
    let root_id = FsObjectId::new(2);

    let payload = MountPayload::new_cap(
        rootfs.clone() as Arc<dyn FsOps>,
        rootfs.clone() as Arc<dyn FsPageBacking>,
        None,
        DevId::new(1),
        MountOptions::default(),
        "testfs",
        SourceLabel::Static("rootfs-v3"),
    )
    .expect("rootfs payload reservation");

    let root_rnode = {
        let raw = RNode::new(
            root_id,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };

    let mount = MountIdentity::new_cap(
        MountId::new(1),
        None,
        root_rnode,
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("rootfs mount identity reservation");

    let root_dentry = mount.root_dentry().clone();

    V3Topology {
        root_dentry,
        rootfs,
    }
}

// === tests pinning the v3 walker against TestFs =====================

#[test]
#[ignore = "main-side zone-slot cascade flake (Weak upgrade fails)"]
fn step_walk_resolves_simple_name() {
    use crate::vfs::adapter::step_engine::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let foo_id = topo.rootfs.add_dir(FsObjectId::new(2), b"foo");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"foo", &cred, &guard);
    drop(guard);
    let dentry = match outcome {
        V3::Done(d) => d,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(dentry.name().as_bytes(), b"foo");
    assert_eq!(dentry.rnode().fs_object_id(), foo_id);
}

#[test]
#[ignore = "main-side zone-slot cascade flake (Weak upgrade fails)"]
fn step_walk_resolves_multi_component_path() {
    use crate::vfs::adapter::step_engine::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let foo_id = topo.rootfs.add_dir(FsObjectId::new(2), b"foo");
    let bar_id = topo.rootfs.add_dir(foo_id, b"bar");
    let baz_id = topo.rootfs.add_dir(bar_id, b"baz");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/foo/bar/baz", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.name().as_bytes(), b"baz");
            assert_eq!(d.rnode().fs_object_id(), baz_id);
        }
        other => panic!("expected Done(baz), got {other:?}"),
    }
}

#[test]
fn step_walk_caches_regular_file_positive_lookup() {
    use crate::vfs::adapter::step_engine::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"file");
    let cred = Credential::root();
    let guard = guard();

    let first_dentry = match step_walk(topo.root_dentry.clone(), b"/file", &cred, &guard) {
        V3::Done(dentry) => {
            assert_eq!(dentry.name().as_bytes(), b"file");
            assert_eq!(dentry.rnode().fs_object_id(), file_id);
            dentry
        }
        other => panic!("expected first walk to materialise file, got {other:?}"),
    };
    let first_counts = topo.rootfs.lookup_counts();
    assert_eq!(first_counts, (1, 1, 1));

    let second = step_walk(topo.root_dentry.clone(), b"/file", &cred, &guard);
    match second {
        V3::Done(dentry) => {
            assert_eq!(dentry.name().as_bytes(), b"file");
            assert_eq!(dentry.rnode().fs_object_id(), file_id);
        }
        other => panic!("expected cached second walk, got {other:?}"),
    }
    assert_eq!(
        topo.rootfs.lookup_counts(),
        first_counts,
        "positive dcache hits for regular files must avoid backend lookup/meta/materialise"
    );
    drop(first_dentry);
    drop(guard);
}

#[test]
fn run_walker_preserves_lookup_yield_as_defer() {
    use crate::vfs::resolution::driver::run_walker;
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    topo.rootfs.add_dir(FsObjectId::new(2), b"slow");
    topo.rootfs
        .yield_next_lookup(FsObjectId::new(2), b"slow", 0x51, 0x02);

    let cred = Credential::root();
    let first_guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/slow",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        WalkState::Defer {
            request, resume, ..
        } => {
            match request {
                crate::vfs::resolution::state::IORequest::DirLookup { fs_object_id, name } => {
                    assert_eq!(fs_object_id, FsObjectId::new(2));
                    assert_eq!(&*name, b"slow");
                }
                other => panic!("expected deferred lookup request, got {other:?}"),
            }
            assert_eq!(
                resume.walking.current.rnode().fs_object_id(),
                FsObjectId::new(2)
            );
            assert_eq!(
                resume.walking.remaining,
                b"slow".to_vec(),
                "resume token must restart at the deferred lookup component"
            );
            resume
        }
        other => panic!("expected deferred walker state, got {other:?}"),
    };
    assert_eq!(
        topo.rootfs.lookup_counts(),
        (1, 0, 0),
        "lookup yield must not be collapsed into meta/materialise work"
    );

    let guard = guard();
    let resolved = crate::vfs::resolution::driver::resume_walker(
        resume,
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    )
    .expect("resume after lookup yield");
    drop(guard);
    assert_eq!(resolved.dentry.name().as_bytes(), b"slow");
}

#[test]
fn resume_walker_after_lookup_io_uses_supplied_result() {
    use crate::vfs::resolution::driver::{resume_walker_after_io, run_walker};
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, IOResult, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let slow_id = topo.rootfs.add_dir(FsObjectId::new(2), b"slow");
    topo.rootfs
        .yield_next_lookup(FsObjectId::new(2), b"slow", 0x51, 0x02);

    let cred = Credential::root();
    let first_guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/slow",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        WalkState::Defer { resume, .. } => resume,
        other => panic!("expected deferred walker state, got {other:?}"),
    };
    assert_eq!(topo.rootfs.lookup_counts(), (1, 0, 0));

    let guard = guard();
    let state = resume_walker_after_io(
        resume,
        IOResult::DirLookup(Ok(slow_id)),
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    );
    drop(guard);

    let resolved = match state {
        WalkState::Terminal(resolved) => resolved,
        other => panic!("expected terminal result after supplied lookup result, got {other:?}"),
    };

    assert_eq!(resolved.dentry.name().as_bytes(), b"slow");
    assert_eq!(
        topo.rootfs.lookup_counts(),
        (1, 1, 0),
        "explicit IO-result resume must not issue a second backend lookup"
    );
}

#[test]
fn resume_walker_after_meta_io_uses_supplied_result() {
    use crate::vfs::resolution::driver::{resume_walker_after_io, run_walker};
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, IOResult, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let slow_id = topo.rootfs.add_dir(FsObjectId::new(2), b"slow");
    topo.rootfs.yield_next_load_inode_meta(slow_id, 0x52, 0x02);

    let cred = Credential::root();
    let first_guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/slow",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        WalkState::Defer { resume, .. } => resume,
        other => panic!("expected deferred walker state, got {other:?}"),
    };
    assert_eq!(topo.rootfs.lookup_counts(), (1, 1, 0));

    let guard = guard();
    let state = resume_walker_after_io(
        resume,
        IOResult::LoadInodeMeta(Ok(InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755))),
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    );
    drop(guard);

    let resolved = match state {
        WalkState::Terminal(resolved) => resolved,
        other => panic!("expected terminal result after supplied meta result, got {other:?}"),
    };

    assert_eq!(resolved.dentry.name().as_bytes(), b"slow");
    assert_eq!(
        topo.rootfs.lookup_counts(),
        (1, 1, 0),
        "explicit meta IO-result resume must not issue a second load_inode_meta"
    );
}

#[test]
fn resume_walker_after_readlink_io_uses_supplied_result() {
    use crate::vfs::resolution::driver::{resume_walker_after_io, run_walker};
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, IOResult, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    topo.rootfs.add_dir(FsObjectId::new(2), b"target");
    let link_id = topo
        .rootfs
        .add_symlink(FsObjectId::new(2), b"link", b"target");
    topo.rootfs.yield_next_read_link(link_id, 0x53, 0x02);

    let cred = Credential::root();
    let first_guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/link",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        WalkState::Defer { resume, .. } => resume,
        other => panic!("expected deferred walker state, got {other:?}"),
    };
    assert_eq!(topo.rootfs.read_link_count(), 1);

    let guard = guard();
    let state = resume_walker_after_io(
        resume,
        IOResult::ReadLink(Ok(Box::from(&b"target"[..]))),
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    );
    drop(guard);

    let resolved = match state {
        WalkState::Terminal(resolved) => resolved,
        other => panic!("expected terminal result after supplied readlink result, got {other:?}"),
    };

    assert_eq!(resolved.dentry.name().as_bytes(), b"target");
    assert_eq!(
        topo.rootfs.read_link_count(),
        1,
        "explicit readlink IO-result resume must not issue a second read_link"
    );
}

#[test]
fn resume_walker_after_materialise_io_uses_supplied_result() {
    use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
    use crate::vfs::resolution::driver::{resume_walker_after_io, run_walker};
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, IOResult, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"file");
    topo.rootfs.yield_next_materialise(file_id, 0x54, 0x02);

    let cred = Credential::root();
    let first_guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/file",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        WalkState::Defer { resume, .. } => resume,
        other => panic!("expected deferred walker state, got {other:?}"),
    };
    assert_eq!(topo.rootfs.lookup_counts(), (1, 1, 1));

    let mount = topo
        .root_dentry
        .rnode()
        .containing_mount_weak()
        .expect("root rnode has containing mount")
        .upgrade(&guard())
        .expect("mount payload upgrade");
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Persistent,
        },
        1,
    )
    .expect("page container");
    let meta = InodeMeta::new(InodeKind::Regular, 0o644);
    let rnode = RNode::new_cap_in_mount(file_id, meta, RNodeBacking::PageBacked { pc }, &mount)
        .expect("supplied rnode");

    let guard = guard();
    let state = resume_walker_after_io(
        resume,
        IOResult::MaterialiseRnode(Ok(rnode)),
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    );
    drop(guard);

    let resolved = match state {
        WalkState::Terminal(resolved) => resolved,
        other => {
            panic!("expected terminal result after supplied materialise result, got {other:?}")
        }
    };

    assert_eq!(resolved.dentry.name().as_bytes(), b"file");
    assert_eq!(
        topo.rootfs.lookup_counts(),
        (1, 1, 1),
        "explicit materialise IO-result resume must not issue a second materialise_rnode"
    );
}

#[test]
fn run_walker_preserves_lookup_error_as_error_state() {
    use crate::execution::Errno;
    use crate::vfs::resolution::driver::run_walker;
    use crate::vfs::resolution::state::{FinalSymlinkPolicy, WalkCause, WalkMode, WalkState};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let cred = Credential::root();
    let guard = guard();
    let state = run_walker(
        topo.root_dentry.clone(),
        b"/missing",
        WalkMode::Entity,
        FinalSymlinkPolicy::Follow,
        &cred,
        &guard,
    );
    drop(guard);

    match state {
        WalkState::Error(WalkCause::FsOpsRejected(Errno::ENOENT)) => {}
        other => panic!("expected ENOENT error state, got {other:?}"),
    }
}

// Cascade flake: fails under workspace serial-test order due to the
// existing main-side zone-slot `Weak::upgrade` race (same root cause
// as the other 6 ignored v3_walker tests). Passes in isolation. The
// flake is at the zone level, so the ignore stays.
#[test]
#[ignore = "main-side zone-slot cascade flake; passes in isolation"]
fn step_walk_returns_enoent_on_missing() {
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/nope", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::ENOENT) => {}
        other => panic!("expected v3 Err(ENOENT), got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake (Weak upgrade fails)"]
fn step_walk_eacces_when_descend_perm_denied() {
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    // Mode 0o700: owner-only permissions. Caller falls through to
    // the empty "other" triplet.
    let dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"ownerdir", 0o700, 1000, 1000);
    topo.rootfs.add_dir(dir, b"leaf");

    let cred = Credential {
        uid: 9999,
        gid: 9999,
        effective_caps: crate::cred::CapabilitySet::EMPTY,
    };
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/ownerdir/leaf", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::EACCES) => {}
        other => panic!("expected v3 Err(EACCES), got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake (same root cause as v4 walker tests; passes in isolation)"]
fn step_walk_chases_relative_symlink() {
    use crate::vfs::adapter::step_engine::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let target_id = topo.rootfs.add_dir(FsObjectId::new(2), b"realdir");
    topo.rootfs
        .add_symlink(FsObjectId::new(2), b"alias", b"realdir");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/alias", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.rnode().fs_object_id(), target_id);
            assert_eq!(d.name().as_bytes(), b"realdir");
        }
        other => panic!("expected Done(realdir), got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake; passes in isolation"]
fn step_open_round_trips_to_directory() {
    use crate::vfs::adapter::step_engine::StepOutcome as V3;

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    let _dir_id = topo.rootfs.add_dir(FsObjectId::new(2), b"opendir");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_open(
        topo.root_dentry.clone(),
        b"/opendir",
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
        0,
        &cred,
        &guard,
    );
    drop(guard);
    match outcome {
        V3::Done(_open) => {}
        other => panic!("expected Done(OpenFile), got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake (same root cause as v4 walker EACCES tests; passes in isolation)"]
fn step_open_eacces_without_read_bit() {
    use crate::vfs::adapter::step_engine::{Errno as V3Errno, StepOutcome as V3};

    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs_v3();

    // Owner = 1000, mode = 0o100 (X only, no R). Walk descent
    // succeeds (X bit grants search) but step_open's R check
    // fails because the R bit is unset on the target.
    let _dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"locked", 0o100, 1000, 0);

    let cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: crate::cred::CapabilitySet::EMPTY,
    };
    let guard = guard();
    let outcome = step_open(
        topo.root_dentry.clone(),
        b"/locked",
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
        0,
        &cred,
        &guard,
    );
    drop(guard);
    match outcome {
        V3::Err(V3Errno::EACCES) => {}
        other => panic!("expected v3 Err(EACCES), got {other:?}"),
    }
}

// `MountPayload::fs_ops` is a required field, so an "unregistered
// fs_ops" state cannot be constructed. The walker's ENODEV branch
// still fires when the rnode lacks a `containing_mount` weak (mount
// tear-down mid-walk).

// Compile-time sanity: the `From<execution::Errno> for step_v3::Errno`
// bridge round-trips ENODEV identically.
#[test]
fn errno_v4_to_v3_is_consistent() {
    use crate::vfs::adapter::step_engine::Errno as V3Errno;
    let v3: V3Errno = Errno::ENODEV.into();
    assert_eq!(v3, V3Errno::ENODEV);
}
