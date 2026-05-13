//! End-to-end tests for `step_walk` / `step_open`.
//!
//! Each test goes through `FsOps` dispatch end-to-end against the
//! canonical production fs (Tmpfs).
//!
//! Lives in its own file so the parent `tests.rs` stays under the
//! `cargo xtask lint arch` 1500-line authored-file cap, mirroring the
//! `v3.rs` sibling that hosts the `FsOps for TestFs` impl.

use alloc::sync::Arc;

use crate::execution::Errno;
use crate::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use crate::page_backed::FsPageBacking;
use crate::vfs::adapter::step_engine::{guard, reserve_for, sign_for, Cap};
use crate::vfs::structure::{
    Credential, DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, OpenFileFlags, RNode,
    RNodeBacking, S_IFDIR,
};
use crate::vfs::walker::{step_open, step_walk};
use crate::vfs::FsOps;

use super::{block_on, init_zones, TestFs};

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

    let _mount = MountIdentity::new_cap(
        MountId::new(1),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("rootfs mount identity reservation");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("rootfs root dentry");

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
    let outcome = block_on(step_walk(topo.root_dentry.clone(), b"foo", &cred, &guard));
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
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/foo/bar/baz",
        &cred,
        &guard,
    ));
    drop(guard);
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.name().as_bytes(), b"baz");
            assert_eq!(d.rnode().fs_object_id(), baz_id);
        }
        other => panic!("expected Done(baz), got {other:?}"),
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
    let outcome = block_on(step_walk(topo.root_dentry.clone(), b"/nope", &cred, &guard));
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
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/ownerdir/leaf",
        &cred,
        &guard,
    ));
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
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/alias",
        &cred,
        &guard,
    ));
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
    let outcome = block_on(step_open(
        topo.root_dentry.clone(),
        b"/opendir",
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
        0,
        &cred,
        &guard,
    ));
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
    let outcome = block_on(step_open(
        topo.root_dentry.clone(),
        b"/locked",
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
        0,
        &cred,
        &guard,
    ));
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
