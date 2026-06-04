//! Walker tests: path resolution, mount-point crossing, symlink chasing.
//!
//! These tests construct an in-process rootfs+devfs topology by hand
//! (the kernel's `init.rs` does the same wiring at boot) so the walker
//! can be exercised without the full reactor + bootstrap state.

use alloc::sync::Arc;

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::Guard;
use crate::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use crate::tty::execution::{register_console_alias, register_hardware};
use crate::tty::structure::TtyIdentity;
use crate::vfs::adapter::step_engine::{
    guard, reserve_for, sign_for, ByteProgress, Cap, Errno as V3Errno, SpinMutex,
    StepOutcome as V3, StepOutcome,
};
use crate::vfs::structure::{
    Credential, DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, OpenFileFlags, RNode,
    RNodeBacking, S_IFDIR,
};
use crate::vfs::FsOps;

use super::{step_open, step_walk, step_walk_in_mount_namespace, SYMLOOP_MAX};

// === capturing char-device binding for the console TTY ================

struct CapturingOps;

impl CharDeviceOps for CapturingOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> V3<usize, ByteProgress> {
        V3::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> V3<usize, ByteProgress> {
        V3::Done(bytes.len())
    }
}

static CAPTURING_OPS: CapturingOps = CapturingOps;
static CAPTURING_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "console-walker",
    ops: &CAPTURING_OPS,
};

fn init_zones() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("register all subsystem zones");
}

#[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
fn block_on<F: core::future::Future>(mut fut: F) -> F::Output {
    use core::pin::Pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn raw_clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn raw_wake(_: *const ()) {}
    fn raw_wake_by_ref(_: *const ()) {}
    fn raw_drop(_: *const ()) {}
    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop);
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    // SAFETY: vtable is `'static`; raw_* never deref the data ptr.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    // SAFETY: fut is on this stack frame for the loop's lifetime.
    let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
    for _ in 0..1024 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => continue,
        }
    }
    panic!("walker tests block_on: future did not resolve in 1024 polls");
}

// === minimal in-test FsOps backends ===================================

/// In-memory directory-only filesystem just expressive enough for the
/// walker tests. Stores: parent_fs_object_id → (name → FsObjectId), plus
/// per-inode kind + symlink target (where applicable).
struct TestFs {
    inner: SpinMutex<TestFsInner>,
}

struct TestFsInner {
    /// Parent → (name → child).
    children: alloc::collections::BTreeMap<
        FsObjectId,
        alloc::collections::BTreeMap<alloc::vec::Vec<u8>, FsObjectId>,
    >,
    /// fs_object_id → (kind, optional symlink target, mode-low-bits,
    /// uid, gid). The walker DAC predicate consults the
    /// owner-triplet/group-triplet/other-triplet selection on the
    /// inode meta; tests parameterise the per-inode mode/uid/gid
    /// here so the assertions can drive the predicate down each
    /// triplet branch.
    inodes: alloc::collections::BTreeMap<FsObjectId, FixtureInodeRow>,
    lookup_count: usize,
    load_meta_count: usize,
    read_link_count: usize,
    materialise_count: usize,
    next_lookup_yield: Option<LookupYield>,
    next_load_meta_yield: Option<InodeYield>,
    next_read_link_yield: Option<InodeYield>,
    next_materialise_yield: Option<InodeYield>,
    next_id: u64,
}

type FixtureInodeRow = (InodeKind, Option<alloc::vec::Vec<u8>>, u16, u32, u32);

struct LookupYield {
    parent: FsObjectId,
    name: alloc::vec::Vec<u8>,
    source_id: u64,
    interests: u64,
}

struct InodeYield {
    fs_object_id: FsObjectId,
    source_id: u64,
    interests: u64,
}

// Default mode-low-bits for the legacy `add_*` helpers. The DAC slice
// uses 0o755 for directories so the walker's descent X-bit check
// passes for `Credential::root()` (the default in test paths post-
// Wave-3).
const TEST_DEFAULT_DIR_MODE: u16 = 0o755;
const TEST_DEFAULT_REGULAR_MODE: u16 = 0o644;
const TEST_DEFAULT_SYMLINK_MODE: u16 = 0o777;

impl TestFs {
    fn new(root_id: FsObjectId) -> Arc<Self> {
        let mut inner = TestFsInner {
            children: alloc::collections::BTreeMap::new(),
            inodes: alloc::collections::BTreeMap::new(),
            lookup_count: 0,
            load_meta_count: 0,
            read_link_count: 0,
            materialise_count: 0,
            next_lookup_yield: None,
            next_load_meta_yield: None,
            next_read_link_yield: None,
            next_materialise_yield: None,
            next_id: root_id.as_u64() + 1,
        };
        inner
            .children
            .insert(root_id, alloc::collections::BTreeMap::new());
        inner.inodes.insert(
            root_id,
            (InodeKind::Directory, None, TEST_DEFAULT_DIR_MODE, 0, 0),
        );
        Arc::new(Self {
            inner: SpinMutex::new(inner),
        })
    }

    fn alloc(&self) -> FsObjectId {
        let mut inner = self.inner.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        FsObjectId::new(id)
    }

    fn add_dir(&self, parent: FsObjectId, name: &[u8]) -> FsObjectId {
        let id = self.alloc();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner
            .children
            .insert(id, alloc::collections::BTreeMap::new());
        inner.inodes.insert(
            id,
            (InodeKind::Directory, None, TEST_DEFAULT_DIR_MODE, 0, 0),
        );
        id
    }

    /// Add a directory with a custom mode (low bits)/uid/gid, used by
    /// the DAC predicate tests to drive each triplet branch.
    fn add_dir_with_perm(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode_low: u16,
        uid: u32,
        gid: u32,
    ) -> FsObjectId {
        let id = self.alloc();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner
            .children
            .insert(id, alloc::collections::BTreeMap::new());
        inner
            .inodes
            .insert(id, (InodeKind::Directory, None, mode_low, uid, gid));
        id
    }

    fn add_symlink(&self, parent: FsObjectId, name: &[u8], target: &[u8]) -> FsObjectId {
        let id = self.alloc();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.inodes.insert(
            id,
            (
                InodeKind::Symlink,
                Some(target.to_vec()),
                TEST_DEFAULT_SYMLINK_MODE,
                0,
                0,
            ),
        );
        id
    }

    /// Add a regular-file inode (not a directory, not a symlink).
    /// Walking *through* it (`/file/leaf`) returns ENOTDIR; walking
    /// *to* it without a trailing slash returns `ENOSYS` from
    /// `materialise_child_rnode` because the walker has no
    /// page-backed RNode materialisation hook yet — but the
    /// trailing-slash directory-required check fires *before*
    /// that materialisation when iterating, so we can still drive
    /// the walker into the ENOTDIR path with `/file/`.
    fn add_regular(&self, parent: FsObjectId, name: &[u8]) -> FsObjectId {
        let id = self.alloc();
        let mut inner = self.inner.lock();
        inner
            .children
            .entry(parent)
            .or_default()
            .insert(name.to_vec(), id);
        inner.inodes.insert(
            id,
            (InodeKind::Regular, None, TEST_DEFAULT_REGULAR_MODE, 0, 0),
        );
        id
    }

    fn lookup_counts(&self) -> (usize, usize, usize) {
        let inner = self.inner.lock();
        (
            inner.lookup_count,
            inner.load_meta_count,
            inner.materialise_count,
        )
    }

    fn yield_next_lookup(&self, parent: FsObjectId, name: &[u8], source_id: u64, interests: u64) {
        self.inner.lock().next_lookup_yield = Some(LookupYield {
            parent,
            name: name.to_vec(),
            source_id,
            interests,
        });
    }

    fn yield_next_load_inode_meta(&self, fs_object_id: FsObjectId, source_id: u64, interests: u64) {
        self.inner.lock().next_load_meta_yield = Some(InodeYield {
            fs_object_id,
            source_id,
            interests,
        });
    }

    fn yield_next_read_link(&self, fs_object_id: FsObjectId, source_id: u64, interests: u64) {
        self.inner.lock().next_read_link_yield = Some(InodeYield {
            fs_object_id,
            source_id,
            interests,
        });
    }

    fn yield_next_materialise(&self, fs_object_id: FsObjectId, source_id: u64, interests: u64) {
        self.inner.lock().next_materialise_yield = Some(InodeYield {
            fs_object_id,
            source_id,
            interests,
        });
    }

    fn read_link_count(&self) -> usize {
        self.inner.lock().read_link_count
    }

    /// Set per-inode (mode-low-bits, uid, gid) directly. Used by the
    /// step_open mode-validation tests to flip the mode after a child
    /// has been created. The slice's `step_open` consults the inode's
    /// mode bits at terminal-component open time; the walker's
    /// `load_inode_meta` call site sees the latest value.
    #[cfg_attr(test, allow(dead_code))]
    fn set_inode_perm(&self, id: FsObjectId, mode_low: u16, uid: u32, gid: u32) {
        let mut inner = self.inner.lock();
        if let Some((_, _, m, u, g)) = inner.inodes.get_mut(&id) {
            *m = mode_low;
            *u = uid;
            *g = gid;
        }
    }
}

// Wave-9a `FsOps` + `FsPageBacking` impls + tests on `TestFs` live
// in the sibling `v3` submodule (file: `walker/tests/v3.rs`) so this
// file stays under the `cargo xtask lint arch` 1500-line authored-file
// cap. The submodule has full visibility into `TestFs` via `super::`.
mod v3;
// Wave-9c end-to-end tests for `step_walk` / `step_open` live
// alongside the wave-9a trait-impl tests. Same parent-module access
// pattern (`use super::{TestFs, init_zones, block_on};`).
mod v3_walker;

// === fixture: rootfs + (optional) devfs at /dev =======================

struct Topology {
    root_dentry: Cap<DEntry>,
    root_mount: Cap<MountIdentity>,
    rootfs: Arc<TestFs>,
}

/// Build a single-mount rootfs over `TestFs` and produce a root DEntry
/// whose RNode has `with_containing_mount` wired so the walker can
/// resolve the in-scope FsOps.
fn build_rootfs() -> Topology {
    let rootfs = TestFs::new(FsObjectId::new(2));
    let root_id = FsObjectId::new(2);

    let payload = MountPayload::new_cap(
        rootfs.clone() as Arc<dyn crate::vfs::FsOps>,
        rootfs.clone() as Arc<dyn crate::page_backed::FsPageBacking>,
        None,
        DevId::new(1),
        MountOptions::default(),
        "testfs",
        SourceLabel::Static("rootfs"),
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

    let root_mount = MountIdentity::new_cap(
        MountId::new(1),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("rootfs mount identity reservation");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("rootfs root dentry");

    Topology {
        root_dentry,
        root_mount,
        rootfs,
    }
}

/// Produce a `tty` `Cap` for the console alias, registered through the
/// canonical TTY-side surface so devfs `lookup`-style materialisation
/// can resolve it.
fn install_console_tty() -> Cap<TtyIdentity> {
    let guard = guard();
    let tty = match register_hardware("console-walker-hw", 0, &CAPTURING_BINDING, &guard) {
        StepOutcome::Done(t) => t,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(register_console_alias("console", tty.clone()), V3::Done(()));
    tty
}

/// Augment a rootfs `Topology` with a devfs mount at `/dev`. The
/// returned `Cap<DEntry>` is the `/dev` mount-point dentry on rootfs
/// (its `mounted_hint` is set to the devfs mount). The `console` TTY
/// has been pre-registered so devfs's lookup can find it.
fn mount_devfs_at_dev(topo: &Topology) -> (Cap<MountIdentity>, Cap<TtyIdentity>) {
    // Build a "devfs"-shaped TestFs whose only child of root is
    // `console` (a CharDevice — but the walker's
    // `materialise_child_rnode` only handles `Directory` and
    // `Symlink`. To exercise mount crossing without falling into
    // the unimplemented CharDevice branch, we instead register
    // a directory child `consoledir` so the walker terminates on
    // a directory after crossing). The `/dev/console` end-to-end
    // path goes through a real devfs in the `tx-fs` crate's tests.
    let devfs_root_id = FsObjectId::new(0x6465_7600);
    let devfs = TestFs::new(devfs_root_id);
    devfs.add_dir(devfs_root_id, b"consoledir");

    let dev_payload = MountPayload::new_cap(
        devfs.clone() as Arc<dyn crate::vfs::FsOps>,
        devfs.clone() as Arc<dyn crate::page_backed::FsPageBacking>,
        None,
        DevId::new(2),
        MountOptions::default(),
        "testfs-dev",
        SourceLabel::Static("devfs"),
    )
    .expect("devfs payload");

    // /dev directory inode on rootfs.
    let dev_id_on_root = topo.rootfs.add_dir(FsObjectId::new(2), b"dev");

    // devfs root rnode.
    let devfs_root_rnode = {
        let raw = RNode::new(
            devfs_root_id,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&dev_payload);
        let res = reserve_for::<RNode>().expect("devfs root rnode reservation");
        sign_for(res, raw)
    };

    // The /dev mount-point dentry on rootfs needs `mounted_hint`
    // set so the walker can cross. In production
    // `mount_devfs_at_dev` (init.rs) installs the hint at
    // mount-publication time per
    // `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`; we do the same
    // here. Note: the dentry the walker materialises during a fresh
    // walk is *not* this dentry (the walker re-mints a dentry at
    // each lookup), so the mount table's `mountpoint` slot below is
    // primarily for record-keeping.
    let dev_mount_point_rnode = {
        let raw = RNode::new(
            dev_id_on_root,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        );
        let res = reserve_for::<RNode>().expect("dev mount-point rnode");
        sign_for(res, raw)
    };
    let dev_mount_point_dentry = DEntry::new_cap(
        InlineName::new(b"dev").expect("dev inline name"),
        dev_mount_point_rnode,
    )
    .expect("dev mount-point dentry");

    let dev_mount = MountIdentity::new_cap(
        MountId::new(2),
        Some(dev_mount_point_dentry.clone()),
        devfs_root_rnode,
        None,
        dev_payload,
        MountFlags::empty(),
    )
    .expect("devfs mount identity");

    let tty = install_console_tty();
    (dev_mount, tty)
}

// === tests ============================================================

#[test]
fn step_walk_resolves_relative_path_within_rootfs() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let foo_id = topo.rootfs.add_dir(FsObjectId::new(2), b"foo");
    let bar_id = topo.rootfs.add_dir(foo_id, b"bar");
    let _ = bar_id;

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"foo/bar", &cred, &guard);
    drop(guard);
    let dentry = match outcome {
        V3::Done(d) => d,
        other => panic!("expected Done, got {other:?}"),
    };
    assert_eq!(dentry.name().as_bytes(), b"bar");
    assert_eq!(dentry.rnode().fs_object_id(), bar_id);
}

#[test]
fn step_walk_resolves_absolute_path_from_root() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let foo_id = topo.rootfs.add_dir(FsObjectId::new(2), b"foo");
    let bar_id = topo.rootfs.add_dir(foo_id, b"bar");
    let _ = (foo_id, bar_id);

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/foo/bar", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => assert_eq!(d.name().as_bytes(), b"bar"),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake (see eloop test)"]
fn step_walk_returns_enoent_on_missing() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

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
#[ignore = "main-side zone-slot cascade flake (see eloop test)"]
fn step_walk_returns_enotdir_on_trailing_slash_after_file() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let _file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"thing");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/thing/", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::ENOTDIR) => {}
        other => panic!("expected v3 Err(ENOTDIR), got {other:?}"),
    }
}

#[test]
#[ignore = "main-side zone-slot cascade flake (see eloop test)"]
fn step_walk_returns_enotdir_when_traversing_through_file() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let _file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"thing");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/thing/under", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::ENOTDIR) => {}
        other => panic!("expected v3 Err(ENOTDIR), got {other:?}"),
    }
}

#[test]
fn step_walk_chases_relative_symlink_to_target() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let target_id = topo.rootfs.add_dir(FsObjectId::new(2), b"realdir");
    // Symlink "alias" → "realdir" (relative).
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
fn step_walk_chases_absolute_symlink_from_root() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let foo_id = topo.rootfs.add_dir(FsObjectId::new(2), b"foo");
    let bar_id = topo.rootfs.add_dir(foo_id, b"bar");
    // Symlink "/jump" → "/foo/bar" (absolute, multi-component).
    topo.rootfs
        .add_symlink(FsObjectId::new(2), b"jump", b"/foo/bar");

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/jump", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.rnode().fs_object_id(), bar_id);
            assert_eq!(d.name().as_bytes(), b"bar");
        }
        other => panic!("expected Done(bar), got {other:?}"),
    }
}

// FIXME: passes in isolation, fails under workspace serial run as a
// position-dependent zone-slot accumulator cascade — documented main-side
// issue. Ignoring shifts the cascade to a sibling, so we leave both this
// and `step_walk_returns_enoent_on_missing` ignored. Re-enable once the
// underlying zone-slot reuse / nested-guard issue is resolved.
#[test]
#[ignore = "main-side zone-slot cascade flake"]
fn step_walk_returns_eloop_after_41_hops() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Build a chain s0 → s1 → s2 → ... → sN (relative).
    // Need `> SYMLOOP_MAX = 40` substitutions, so 41+ links so the
    // 41st observation trips ELOOP.
    let chain_len = (SYMLOOP_MAX as usize) + 5;
    for i in 0..chain_len {
        let name = alloc::format!("s{i}");
        let target = alloc::format!("s{}", i + 1);
        topo.rootfs
            .add_symlink(FsObjectId::new(2), name.as_bytes(), target.as_bytes());
    }
    // Final terminal node so the chain doesn't ENOENT before
    // ELOOP. (If ELOOP fires before reaching it, that's fine —
    // the assertion is on ELOOP regardless.)
    let final_name = alloc::format!("s{chain_len}");
    topo.rootfs
        .add_dir(FsObjectId::new(2), final_name.as_bytes());

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/s0", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::ELOOP) => {}
        other => panic!("expected v3 Err(ELOOP), got {other:?}"),
    }
}

#[test]
fn step_walk_crosses_mount_point_at_dev() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    crate::mount::reset_mount_table_for_test();
    let topo = build_rootfs();
    let (dev_mount, _tty) = mount_devfs_at_dev(&topo);

    // Find the rootfs's MountPayload Cap and the FsObjectId of /dev
    // on rootfs. These form the (parent_payload, child_fs_object_id)
    // key the walker consults via mount::mount_for.
    let root_dev_id =
        match <TestFs as FsOps>::lookup(&*topo.rootfs, FsObjectId::new(2), b"dev", &guard()) {
            V3::Done(id) => id,
            other => panic!("rootfs lookup(dev) failed: {other:?}"),
        };
    let rootfs_payload = topo
        .root_dentry
        .rnode()
        .containing_mount_weak()
        .expect("root rnode has containing_mount")
        .upgrade(&guard())
        .expect("rootfs payload upgrade");

    // Register the mount in the kernel's mount table; the walker
    // then crosses on lookup.
    crate::mount::register_mount(&rootfs_payload, root_dev_id, dev_mount.clone());

    let cred = Credential::root();
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/dev/consoledir", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => {
            assert_eq!(d.name().as_bytes(), b"consoledir");
            // The terminal dentry's RNode lives on devfs, not
            // rootfs; its FsObjectId is in devfs's namespace
            // (>= 0x6465_7600), not rootfs's (>= 2).
            assert!(
                d.rnode().fs_object_id().as_u64() > 0x6465_7600,
                "expected devfs-namespace fs_object_id, got {:?}",
                d.rnode().fs_object_id()
            );
        }
        other => panic!("expected Done(consoledir), got {other:?}"),
    }
}

#[test]
fn step_walk_uses_mount_namespace_table_before_global_fallback() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    crate::mount::reset_mount_table_for_test();
    let topo = build_rootfs();
    let (dev_mount, _tty) = mount_devfs_at_dev(&topo);

    let root_dev_id =
        match <TestFs as FsOps>::lookup(&*topo.rootfs, FsObjectId::new(2), b"dev", &guard()) {
            V3::Done(id) => id,
            other => panic!("rootfs lookup(dev) failed: {other:?}"),
        };
    let rootfs_payload = topo
        .root_dentry
        .rnode()
        .containing_mount_weak()
        .expect("root rnode has containing_mount")
        .upgrade(&guard())
        .expect("rootfs payload upgrade");
    let ns_with_mount =
        crate::mount::MountNamespace::new_cap(topo.root_mount.clone()).expect("ns cap");
    let ns_without_mount =
        crate::mount::MountNamespace::new_cap(topo.root_mount.clone()).expect("ns cap");
    ns_with_mount.register_mount(&rootfs_payload, root_dev_id, dev_mount);

    let cred = Credential::root();
    let guard = guard();
    let visible = step_walk_in_mount_namespace(
        topo.root_dentry.clone(),
        b"/dev/consoledir",
        &cred,
        &ns_with_mount,
        &guard,
    );
    let hidden = step_walk_in_mount_namespace(
        topo.root_dentry.clone(),
        b"/dev/consoledir",
        &cred,
        &ns_without_mount,
        &guard,
    );
    drop(guard);

    match visible {
        V3::Done(d) => assert_eq!(d.name().as_bytes(), b"consoledir"),
        other => panic!("expected namespace-local mount to resolve, got {other:?}"),
    }
    match hidden {
        V3::Err(V3Errno::ENOENT) => {}
        other => panic!("expected namespace without mount to hide /dev contents, got {other:?}"),
    }
}

#[test]
fn run_walker_resume_preserves_mount_namespace() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    crate::mount::reset_mount_table_for_test();
    let topo = build_rootfs();
    let (dev_mount, _tty) = mount_devfs_at_dev(&topo);

    let root_dev_id =
        match <TestFs as FsOps>::lookup(&*topo.rootfs, FsObjectId::new(2), b"dev", &guard()) {
            V3::Done(id) => id,
            other => panic!("rootfs lookup(dev) failed: {other:?}"),
        };
    let rootfs_payload = topo
        .root_dentry
        .rnode()
        .containing_mount_weak()
        .expect("root rnode has containing_mount")
        .upgrade(&guard())
        .expect("rootfs payload upgrade");
    let ns_with_mount =
        crate::mount::MountNamespace::new_cap(topo.root_mount.clone()).expect("ns cap");
    ns_with_mount.register_mount(&rootfs_payload, root_dev_id, dev_mount);

    topo.rootfs
        .yield_next_lookup(FsObjectId::new(2), b"dev", 0x61, 0x01);

    let cred = Credential::root();
    let first_guard = guard();
    let state = crate::vfs::resolution::driver::run_walker_with_mount_namespace(
        topo.root_dentry.clone(),
        b"/dev/consoledir",
        crate::vfs::resolution::state::WalkMode::Entity,
        crate::vfs::resolution::state::FinalSymlinkPolicy::Follow,
        &cred,
        Some(&ns_with_mount),
        &first_guard,
    );
    drop(first_guard);

    let resume = match state {
        crate::vfs::resolution::state::WalkState::Defer { resume, .. } => resume,
        other => panic!("expected deferred namespace walk, got {other:?}"),
    };

    let second_guard = guard();
    let resolved = crate::vfs::resolution::driver::resume_walker(
        resume,
        crate::vfs::resolution::state::WalkMode::Entity,
        crate::vfs::resolution::state::FinalSymlinkPolicy::Follow,
        &cred,
        &second_guard,
    )
    .expect("resume should preserve namespace-local mount crossing");
    drop(second_guard);

    assert_eq!(resolved.dentry.name().as_bytes(), b"consoledir");
    assert!(
        resolved.rnode.fs_object_id().as_u64() > 0x6465_7600,
        "expected devfs namespace object after resume, got {:?}",
        resolved.rnode.fs_object_id()
    );
}

// === DAC predicate tests (Wave 3 Part 2) ==============================
//
// These exercise the walker's `check_descend_perm` (intermediate
// directory traversal) and `step_open`'s `check_open_perm`
// (terminal-component R/W validation). Each test builds a single
// child directory under the rootfs root with an explicit
// (mode, uid, gid) and walks/opens against it with a
// non-root `Credential`.

fn unprivileged_cred(uid: u32, gid: u32) -> Credential {
    Credential {
        uid,
        gid,
        effective_caps: crate::cred::CapabilitySet::EMPTY,
    }
}

#[test]
fn step_walk_owner_can_traverse_dir_with_owner_x_bit() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Owner-only X (0o100): owner = uid 1000 has search.
    let _dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"ownerdir", 0o100, 1000, 0);
    topo.rootfs.add_dir(_dir, b"leaf");

    let cred = unprivileged_cred(1000, 0);
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/ownerdir/leaf", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => assert_eq!(d.name().as_bytes(), b"leaf"),
        other => panic!("expected Done(leaf), got {other:?}"),
    }
}

#[test]
fn step_walk_other_cannot_traverse_dir_without_other_x_bit() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Mode 0o700: owner = (rwx), group = 0, other = 0. uid 9999 falls
    // through owner/group and lands on the empty "other" triplet, so
    // descent must fail with EACCES.
    let dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"ownerdir", 0o700, 1000, 1000);
    topo.rootfs.add_dir(dir, b"leaf");

    let cred = unprivileged_cred(9999, 9999);
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/ownerdir/leaf", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Err(V3Errno::EACCES) => {}
        other => panic!("expected v3 Err(EACCES), got {other:?}"),
    }
}

#[test]
fn step_walk_dac_override_short_circuits_perm_check() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Mode 0o000 — every triplet is empty. Without DAC_OVERRIDE the
    // walker can't descend; with it the descent succeeds.
    let dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"locked", 0o000, 1000, 1000);
    topo.rootfs.add_dir(dir, b"leaf");

    let mut caps = crate::cred::CapabilitySet::EMPTY;
    caps.add(crate::cred::Capability::DAC_OVERRIDE);
    let cred = Credential {
        uid: 9999,
        gid: 9999,
        effective_caps: caps,
    };
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/locked/leaf", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => assert_eq!(d.name().as_bytes(), b"leaf"),
        other => panic!("expected Done(leaf) under DAC_OVERRIDE, got {other:?}"),
    }
}

#[test]
fn step_walk_group_match_uses_group_triplet() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Owner = 1000, group = 500, mode = 0o010 (group-only X). A
    // caller in group 500 (uid != owner) lands on the group triplet
    // and gets X.
    let dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"groupdir", 0o010, 1000, 500);
    topo.rootfs.add_dir(dir, b"leaf");

    let cred = unprivileged_cred(2000, 500);
    let guard = guard();
    let outcome = step_walk(topo.root_dentry.clone(), b"/groupdir/leaf", &cred, &guard);
    drop(guard);
    match outcome {
        V3::Done(d) => assert_eq!(d.name().as_bytes(), b"leaf"),
        other => panic!("expected Done(leaf), got {other:?}"),
    }
}

#[test]
fn step_open_caller_with_read_bit_succeeds() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Terminal directory the open targets has owner-rwx. step_open
    // returns ENOSYS for non-directory PageBacked materialisation in
    // this test fixture, so we open against a directory (the dispatch
    // returns Done(OpenFile) for directories, then EISDIR happens
    // only when step_read is called).
    let dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"readdir", 0o400, 1000, 0);
    let _ = dir;

    let cred = unprivileged_cred(1000, 0);
    let guard = guard();
    let outcome = step_open(
        topo.root_dentry.clone(),
        b"/readdir",
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
    );
    drop(guard);
    match outcome {
        V3::Done(_) => {}
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn step_open_no_read_bit_returns_eacces() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Owner = 1000, mode = 0o100 (X only, no R). The descent succeeds
    // (X bit grants search) but the open-mode check fails because R
    // is not set.
    let _dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"locked", 0o100, 1000, 0);

    let cred = unprivileged_cred(1000, 0);
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

#[test]
fn step_open_caller_with_write_bit_succeeds() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Owner = 1000, mode = 0o600 (rw, no X). For the descent to even
    // reach this dentry, the parent (rootfs root, mode 0o755) must
    // have other-X set — it does.
    let _dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"rwdir", 0o600, 1000, 0);

    let cred = unprivileged_cred(1000, 0);
    let guard = guard();
    let outcome = step_open(
        topo.root_dentry.clone(),
        b"/rwdir",
        OpenFileFlags {
            read: false,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
        0,
        &cred,
        &guard,
    );
    drop(guard);
    match outcome {
        V3::Done(_) => {}
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn step_open_dac_override_short_circuits() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    // Owner = 1000, mode = 0o000. Without DAC_OVERRIDE step_open
    // would EACCES (no R bit on any triplet); with it the open
    // succeeds.
    let _dir = topo
        .rootfs
        .add_dir_with_perm(FsObjectId::new(2), b"locked", 0o000, 1000, 0);

    let mut caps = crate::cred::CapabilitySet::EMPTY;
    caps.add(crate::cred::Capability::DAC_OVERRIDE);
    let cred = Credential {
        uid: 9999,
        gid: 9999,
        effective_caps: caps,
    };
    let guard = guard();
    let outcome = step_open(
        topo.root_dentry.clone(),
        b"/locked",
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
        0,
        &cred,
        &guard,
    );
    drop(guard);
    match outcome {
        V3::Done(_) => {}
        other => panic!("expected Done under DAC_OVERRIDE, got {other:?}"),
    }
}

#[test]
fn step_open_round_trips_to_directory_dentry() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

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
        },
        0,
        &cred,
        &guard,
    );
    drop(guard);
    let file = match outcome {
        V3::Done(f) => f,
        other => panic!("expected Done(OpenFile), got {other:?}"),
    };
    let _ = file;
}
