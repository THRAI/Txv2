//! Walker tests: path resolution, mount-point crossing, symlink chasing.
//!
//! These tests construct an in-process rootfs+devfs topology by hand
//! (the kernel's `init.rs` does the same wiring at boot) so the walker
//! can be exercised without the full reactor + bootstrap state.

use alloc::boxed::Box;
use alloc::sync::Arc;

use tx_substrate::zone::{self, Cap};

use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Errno, Guard, StepOutcome};
use crate::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use crate::page_backed::{Frame, FsPageBacking};
use crate::tty::execution::{register_console_alias, register_hardware};
use crate::tty::structure::TtyIdentity;
use crate::vfs::structure::{
    Credential, DEntry, DirCursor, DirEntry, FsObjectId, InlineName, InodeKind, InodeMeta,
    OpenFileFlags, RNode, RNodeBacking, S_IFDIR, S_IFLNK,
};
use crate::vfs::FsOps;

use super::{step_open, step_walk, SYMLOOP_MAX};

// === capturing char-device binding for the console TTY ================

struct CapturingOps;

impl CharDeviceOps for CapturingOps {
    fn read(&self, _out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(0)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        StepOutcome::Done(bytes.len())
    }
}

static CAPTURING_OPS: CapturingOps = CapturingOps;
static CAPTURING_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "console-walker",
    ops: &CAPTURING_OPS,
};

fn init_zones() {
    tx_substrate::testing::init_host_for_test_once();
    crate::zones::register_all().expect("register all subsystem zones");
}

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
    inner: tx_substrate::SpinMutex<TestFsInner>,
}

struct TestFsInner {
    /// Parent → (name → child).
    children: alloc::collections::BTreeMap<
        FsObjectId,
        alloc::collections::BTreeMap<alloc::vec::Vec<u8>, FsObjectId>,
    >,
    /// fs_object_id → (kind, optional symlink target).
    inodes: alloc::collections::BTreeMap<FsObjectId, (InodeKind, Option<alloc::vec::Vec<u8>>)>,
    next_id: u64,
}

impl TestFs {
    fn new(root_id: FsObjectId) -> Arc<Self> {
        let mut inner = TestFsInner {
            children: alloc::collections::BTreeMap::new(),
            inodes: alloc::collections::BTreeMap::new(),
            next_id: root_id.as_u64() + 1,
        };
        inner
            .children
            .insert(root_id, alloc::collections::BTreeMap::new());
        inner.inodes.insert(root_id, (InodeKind::Directory, None));
        Arc::new(Self {
            inner: tx_substrate::SpinMutex::new(inner),
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
        inner.inodes.insert(id, (InodeKind::Directory, None));
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
        inner
            .inodes
            .insert(id, (InodeKind::Symlink, Some(target.to_vec())));
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
        inner.inodes.insert(id, (InodeKind::Regular, None));
        id
    }
}

impl FsOps for TestFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<FsObjectId> {
        let inner = self.inner.lock();
        let Some(map) = inner.children.get(&parent) else {
            return StepOutcome::Err(Errno::ENOTDIR);
        };
        match map.get(name) {
            Some(id) => StepOutcome::Done(*id),
            None => StepOutcome::Err(Errno::ENOENT),
        }
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<InodeMeta> {
        let inner = self.inner.lock();
        let Some((kind, _)) = inner.inodes.get(&fs_object_id) else {
            return StepOutcome::Err(Errno::ENOENT);
        };
        let mode = match kind {
            InodeKind::Directory => S_IFDIR | 0o755,
            InodeKind::Symlink => S_IFLNK | 0o777,
            _ => 0o644,
        };
        StepOutcome::Done(InodeMeta::new(*kind, mode))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta)> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>> {
        StepOutcome::Done(None)
    }

    fn destroy_inode(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn read_link(&self, fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<Box<[u8]>> {
        let inner = self.inner.lock();
        match inner.inodes.get(&fs_object_id) {
            Some((InodeKind::Symlink, Some(target))) => {
                StepOutcome::Done(target.clone().into_boxed_slice())
            }
            Some(_) => StepOutcome::Err(Errno::EINVAL),
            None => StepOutcome::Err(Errno::ENOENT),
        }
    }
}

impl FsPageBacking for TestFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<Frame> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn flush_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn truncate(
        &self,
        _fs_object_id: FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        StepOutcome::Err(Errno::ENOSYS)
    }

    fn fsync(&self, _fs_object_id: FsObjectId, _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
    }
}

// === fixture: rootfs + (optional) devfs at /dev =======================

struct Topology {
    root_dentry: Cap<DEntry>,
    rootfs: Arc<TestFs>,
}

/// Build a single-mount rootfs over `TestFs` and produce a root DEntry
/// whose RNode has `with_containing_mount` wired so the walker can
/// resolve the in-scope FsOps.
fn build_rootfs() -> Topology {
    let rootfs = TestFs::new(FsObjectId::new(2));
    let root_id = FsObjectId::new(2);

    let payload = MountPayload::new_cap(
        rootfs.clone() as Arc<dyn FsOps>,
        rootfs.clone() as Arc<dyn FsPageBacking>,
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
        let res = zone::reserve_for::<RNode>().expect("rnode reservation");
        zone::sign_for(res, raw)
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

    Topology {
        root_dentry,
        rootfs,
    }
}

/// Produce a `tty` `Cap` for the console alias, registered through the
/// canonical TTY-side surface so devfs `lookup`-style materialisation
/// can resolve it.
fn install_console_tty() -> Cap<TtyIdentity> {
    let guard = tx_substrate::epoch::guard();
    let tty = match register_hardware("console-walker-hw", 0, &CAPTURING_BINDING, &guard) {
        StepOutcome::Done(t) => t,
        other => panic!("register_hardware failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty.clone()),
        StepOutcome::Done(())
    );
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
        devfs.clone() as Arc<dyn FsOps>,
        devfs.clone() as Arc<dyn FsPageBacking>,
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
        let res = zone::reserve_for::<RNode>().expect("devfs root rnode reservation");
        zone::sign_for(res, raw)
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
        let res = zone::reserve_for::<RNode>().expect("dev mount-point rnode");
        zone::sign_for(res, raw)
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

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"foo/bar",
        &cred,
        &guard,
    ));
    drop(guard);
    let dentry = match outcome {
        StepOutcome::Done(d) => d,
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

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/foo/bar",
        &cred,
        &guard,
    ));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) => assert_eq!(d.name().as_bytes(), b"bar"),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[test]
fn step_walk_returns_enoent_on_missing() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(topo.root_dentry.clone(), b"/nope", &cred, &guard));
    drop(guard);
    assert_eq!(outcome, StepOutcome::Err(Errno::ENOENT));
}

#[test]
fn step_walk_returns_enotdir_on_trailing_slash_after_file() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let _file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"thing");

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/thing/",
        &cred,
        &guard,
    ));
    drop(guard);
    assert_eq!(outcome, StepOutcome::Err(Errno::ENOTDIR));
}

#[test]
fn step_walk_returns_enotdir_when_traversing_through_file() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let _file_id = topo.rootfs.add_regular(FsObjectId::new(2), b"thing");

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/thing/under",
        &cred,
        &guard,
    ));
    drop(guard);
    assert_eq!(outcome, StepOutcome::Err(Errno::ENOTDIR));
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

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/alias",
        &cred,
        &guard,
    ));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) => {
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

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(topo.root_dentry.clone(), b"/jump", &cred, &guard));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) => {
            assert_eq!(d.rnode().fs_object_id(), bar_id);
            assert_eq!(d.name().as_bytes(), b"bar");
        }
        other => panic!("expected Done(bar), got {other:?}"),
    }
}

#[test]
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

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(topo.root_dentry.clone(), b"/s0", &cred, &guard));
    drop(guard);
    assert_eq!(outcome, StepOutcome::Err(Errno::ELOOP));
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
        match topo
            .rootfs
            .lookup(FsObjectId::new(2), b"dev", &tx_substrate::epoch::guard())
        {
            StepOutcome::Done(id) => id,
            other => panic!("rootfs lookup(dev) failed: {other:?}"),
        };
    let rootfs_payload = topo
        .root_dentry
        .rnode()
        .containing_mount_weak()
        .expect("root rnode has containing_mount")
        .upgrade(&tx_substrate::epoch::guard())
        .expect("rootfs payload upgrade");

    // Register the mount in the kernel's mount table; the walker
    // then crosses on lookup.
    crate::mount::register_mount(&rootfs_payload, root_dev_id, dev_mount.clone());

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_walk(
        topo.root_dentry.clone(),
        b"/dev/consoledir",
        &cred,
        &guard,
    ));
    drop(guard);
    match outcome {
        StepOutcome::Done(d) => {
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
fn step_open_round_trips_to_directory_dentry() {
    let _serial = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_zones();
    let topo = build_rootfs();

    let _dir_id = topo.rootfs.add_dir(FsObjectId::new(2), b"opendir");

    let cred = Credential::default();
    let guard = tx_substrate::epoch::guard();
    let outcome = block_on(step_open(
        topo.root_dentry.clone(),
        b"/opendir",
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
        },
        0,
        &cred,
        &guard,
    ));
    drop(guard);
    let file = match outcome {
        StepOutcome::Done(f) => f,
        other => panic!("expected Done(OpenFile), got {other:?}"),
    };
    let _ = file;
}
