// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::adapter::step_engine::{
    self as step_engine, Cap, StepOutcome, guard, page_allocator, reserve_for, sign_for,
};
use tx_fs::tmpfs::{TMPFS_ROOT_OBJECT_ID, Tmpfs};
use tx_subsystems::cred::CapabilitySet;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions, MountPayload,
    Propagation, SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::pipe::{PipeFlags, step_pipe2};
use tx_subsystems::process::{step_chdir, step_set_mount_namespace};
use tx_subsystems::vfs::structure::{
    Credential, DEntry, DirCursor, FsObjectId, InlineName, InodeKind, InodeMeta, RNode,
    RNodeBacking, S_IFDIR,
};
use tx_subsystems::vfs::{DirEntry, FsOps};
use tx_subsystems::vm::{
    MapPlacement, Prot, USER_PAGE_SIZE, UserRange, UserVirtAddr, VmBacking, VmEntryFlags,
    VmMapRequest,
};

use crate::linux_syscall::{
    AT_FDCWD, AT_REMOVEDIR, NR_CLOSE, NR_FSTAT, NR_FTRUNCATE, NR_LINKAT, NR_LSEEK, NR_MKDIRAT,
    NR_MOUNT, NR_OPENAT, NR_READLINKAT, NR_RENAMEAT2, NR_SYMLINKAT, NR_TRUNCATE, NR_UNLINKAT,
    NR_UTIMENSAT, NR_WRITEV, O_CREAT, O_RDWR, O_TMPFILE, RENAME_EXCHANGE, RENAME_NOREPLACE,
    SEEK_SET, UTIME_NOW,
};

/// errno magnitudes (positive Linux RV64 generic ABI values).
const E_NOENT: i32 = 2;
const E_BADF: i32 = 9;
const E_EXIST: i32 = 17;
const E_NOTDIR: i32 = 20;
const E_ISDIR: i32 = 21;
const E_INVAL: i32 = 22;
const E_PERM: i32 = 1;
const E_ACCES: i32 = 13;
const E_NOSYS: i32 = 38;
const STAT_BYTES: usize = 128;
const STAT_ATIME_SEC_OFF: usize = 72;
const STAT_MTIME_SEC_OFF: usize = 88;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn read_i64_at(buf: &[u8], off: usize) -> i64 {
    i64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for file_mutation tests: {error:?}"),
    }
}

fn fm_setup() -> TestSetup {
    let setup = setup();
    ensure_zero_frame_claimed();
    setup
}

fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
    let tmpfs = Arc::new(Tmpfs::new());
    let payload = MountPayload::new_cap(
        tmpfs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        tmpfs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(411),
        MountOptions::default(),
        "tmpfs-file-mutation",
        SourceLabel::Static("tmpfs-file-mutation"),
    )
    .expect("mount payload");

    let root_rnode = {
        let raw = RNode::new(
            TMPFS_ROOT_OBJECT_ID,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(41),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    (root_dentry, tmpfs)
}

struct DestroyCountingFs {
    inner: Arc<Tmpfs>,
    destroy_calls: Arc<AtomicUsize>,
}

impl DestroyCountingFs {
    fn new(inner: Arc<Tmpfs>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let destroy_calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                inner,
                destroy_calls: Arc::clone(&destroy_calls),
            }),
            destroy_calls,
        )
    }
}

impl FsOps for DestroyCountingFs {
    fn lookup(
        &self,
        parent: FsObjectId,
        name: &[u8],
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<FsObjectId, step_engine::NoProgress> {
        self.inner.lookup(parent, name, guard)
    }

    fn load_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<InodeMeta, step_engine::NoProgress> {
        self.inner.load_inode_meta(fs_object_id, guard)
    }

    fn serialize_inode_meta(
        &self,
        fs_object_id: FsObjectId,
        meta: &InodeMeta,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner.serialize_inode_meta(fs_object_id, meta, guard)
    }

    fn create_inode(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), step_engine::NoProgress> {
        self.inner.create_inode(parent, name, mode, cred, guard)
    }

    fn unlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner.unlink(parent, name, target, guard)
    }

    fn rename(
        &self,
        old_parent: FsObjectId,
        old_name: &[u8],
        new_parent: FsObjectId,
        new_name: &[u8],
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner
            .rename(old_parent, old_name, new_parent, new_name, guard)
    }

    fn link(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner.link(parent, name, target, guard)
    }

    fn mkdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        mode: u16,
        cred: &Credential,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), step_engine::NoProgress> {
        self.inner.mkdir(parent, name, mode, cred, guard)
    }

    fn rmdir(
        &self,
        parent: FsObjectId,
        name: &[u8],
        target: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner.rmdir(parent, name, target, guard)
    }

    fn symlink(
        &self,
        parent: FsObjectId,
        name: &[u8],
        link_target: &[u8],
        cred: &Credential,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(FsObjectId, InodeMeta), step_engine::NoProgress> {
        self.inner.symlink(parent, name, link_target, cred, guard)
    }

    fn readdir(
        &self,
        fs_object_id: FsObjectId,
        cursor: DirCursor,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<Option<(DirEntry, DirCursor)>, step_engine::NoProgress> {
        self.inner.readdir(fs_object_id, cursor, guard)
    }

    fn destroy_inode(
        &self,
        fs_object_id: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.destroy_calls.fetch_add(1, Ordering::AcqRel);
        self.inner.destroy_inode(fs_object_id, guard)
    }

    fn read_link(
        &self,
        fs_object_id: FsObjectId,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<alloc::boxed::Box<[u8]>, step_engine::NoProgress> {
        self.inner.read_link(fs_object_id, guard)
    }

    fn materialise_rnode(
        &self,
        fs_object_id: FsObjectId,
        meta: InodeMeta,
        mount: &Cap<MountPayload>,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<Cap<RNode>, step_engine::NoProgress> {
        self.inner
            .materialise_rnode(fs_object_id, meta, mount, guard)
    }

    fn chmod_inode(
        &self,
        fs_object_id: FsObjectId,
        new_mode: u16,
        cred: &Credential,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner.chmod_inode(fs_object_id, new_mode, cred, guard)
    }

    fn chown_inode(
        &self,
        fs_object_id: FsObjectId,
        new_uid: Option<u32>,
        new_gid: Option<u32>,
        cred: &Credential,
        guard: &step_engine::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.inner
            .chown_inode(fs_object_id, new_uid, new_gid, cred, guard)
    }
}

fn build_destroy_counting_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>, Arc<AtomicUsize>) {
    let tmpfs = Arc::new(Tmpfs::new());
    let (counting_fs, destroy_calls) = DestroyCountingFs::new(Arc::clone(&tmpfs));
    let payload = MountPayload::new_cap(
        counting_fs as Arc<dyn tx_subsystems::vfs::FsOps>,
        tmpfs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(412),
        MountOptions::default(),
        "tmpfs-destroy-counting-file-mutation",
        SourceLabel::Static("tmpfs-destroy-counting-file-mutation"),
    )
    .expect("mount payload");

    let root_rnode = {
        let raw = RNode::new(
            TMPFS_ROOT_OBJECT_ID,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(42),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    (root_dentry, tmpfs, destroy_calls)
}

fn bootstrap_with_cwd(root_dentry: Cap<DEntry>) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    match step_chdir(&process, root_dentry) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
            panic!("init bootstrap zombified")
        }
    }
    (process, thread)
}

fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn map_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) -> u64 {
    let range = UserRange::new_aligned(UserVirtAddr(uaddr), USER_PAGE_SIZE)
        .expect("aligned user test range");
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(request).expect("map user bytes");
    if !bytes.is_empty() {
        let guard = guard();
        let copied = ctx
            .aspace
            .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard);
        drop(guard);
        assert_eq!(copied, StepOutcome::Done(bytes.len()));
    }
    uaddr as u64
}

fn root_cred() -> Credential {
    Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    }
}

fn create_regular(tmpfs: &Arc<Tmpfs>, name: &[u8]) {
    create_regular_in(tmpfs, TMPFS_ROOT_OBJECT_ID, name);
}

fn create_regular_in(tmpfs: &Arc<Tmpfs>, parent: FsObjectId, name: &[u8]) {
    use step_engine::StepOutcome;
    let cred = root_cred();
    let guard = guard();
    match tmpfs.create_inode(parent, name, 0o100644, &cred, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("create_inode {:?} in {parent:?}: {other:?}", name),
    }
}

fn make_dir(tmpfs: &Arc<Tmpfs>, name: &[u8]) -> FsObjectId {
    make_dir_in(tmpfs, TMPFS_ROOT_OBJECT_ID, name)
}

fn make_dir_in(tmpfs: &Arc<Tmpfs>, parent: FsObjectId, name: &[u8]) -> FsObjectId {
    use step_engine::StepOutcome;
    let cred = root_cred();
    let guard = guard();
    match tmpfs.mkdir(parent, name, 0o755, &cred, &guard) {
        StepOutcome::Done((id, _)) => id,
        other => panic!("mkdir {:?} in {parent:?}: {other:?}", name),
    }
}

fn make_symlink(tmpfs: &Arc<Tmpfs>, name: &[u8], target: &[u8]) {
    use step_engine::StepOutcome;
    let cred = root_cred();
    let guard = guard();
    match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, name, target, &cred, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("symlink {:?}: {other:?}", name),
    }
}

fn lookup_exists(tmpfs: &Arc<Tmpfs>, name: &[u8]) -> bool {
    lookup_exists_in(tmpfs, TMPFS_ROOT_OBJECT_ID, name)
}

fn lookup_exists_in(tmpfs: &Arc<Tmpfs>, parent: FsObjectId, name: &[u8]) -> bool {
    use step_engine::StepOutcome;
    let guard = guard();
    matches!(tmpfs.lookup(parent, name, &guard), StepOutcome::Done(_))
}

fn install_test_mount_namespace(
    process: &Cap<ProcessIdentity>,
    root: &Cap<DEntry>,
) -> (Cap<MountIdentity>, Cap<MountNamespace>) {
    let payload = root
        .rnode()
        .containing_mount_weak()
        .and_then(|weak| weak.upgrade(&guard()))
        .expect("root mount payload");
    let root_mount = MountIdentity::new_cap_with_root_dentry(
        MountId::new(42_000),
        None,
        root.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("root mount identity");
    let namespace = MountNamespace::new_cap(root_mount.clone()).expect("mount namespace");
    step_set_mount_namespace(process, namespace.clone()).expect("install mount namespace");
    (root_mount, namespace)
}

fn walk_in_namespace(
    root: &Cap<DEntry>,
    path: &[u8],
    namespace: &Cap<MountNamespace>,
) -> Cap<DEntry> {
    let guard = guard();
    match tx_subsystems::vfs::walker::step_walk_in_mount_namespace(
        root.clone(),
        path,
        &root_cred(),
        namespace,
        &guard,
    ) {
        StepOutcome::Done(dentry) => dentry,
        other => panic!("namespace walk {path:?}: {other:?}"),
    }
}

// -----------------------------------------------------------------
// mount namespace bind/move
// -----------------------------------------------------------------

#[test]
fn dispatch_mount_bind_is_immediately_visible_in_calling_mount_namespace() {
    const MS_BIND: u64 = 4096;

    let _setup = fm_setup();
    let (root, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"source");
    make_dir(&tmpfs, b"target");
    let (process, thread) = bootstrap_with_cwd(root.clone());
    let (_root_mount, namespace) = install_test_mount_namespace(&process, &root);
    let ctx = make_ctx(process, thread);

    let target_before = walk_in_namespace(&root, b"/target", &namespace);
    let source = nul_terminate(b"/source");
    let target = nul_terminate(b"/target");
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MOUNT,
            [
                source.as_ptr() as u64,
                target.as_ptr() as u64,
                0,
                MS_BIND,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mounted = namespace
        .mount_for(&target_before)
        .expect("bind must publish into the calling mount namespace");
    let resolved = walk_in_namespace(&root, b"/target", &namespace);
    assert_eq!(resolved.key(), mounted.root_dentry().key());
}

#[test]
fn dispatch_mount_move_preserves_identity_and_rekeys_global_and_namespace_indexes() {
    const MS_BIND: u64 = 4096;
    const MS_MOVE: u64 = 8192;

    let _setup = fm_setup();
    let (root, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"source");
    let old_id = make_dir(&tmpfs, b"old");
    let new_id = make_dir(&tmpfs, b"new");
    let (process, thread) = bootstrap_with_cwd(root.clone());
    let (_root_mount, namespace) = install_test_mount_namespace(&process, &root);
    let ctx = make_ctx(process, thread);

    let old_mountpoint = walk_in_namespace(&root, b"/old", &namespace);
    let new_mountpoint = walk_in_namespace(&root, b"/new", &namespace);
    let source = nul_terminate(b"/source");
    let old = nul_terminate(b"/old");
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MOUNT,
                [
                    source.as_ptr() as u64,
                    old.as_ptr() as u64,
                    0,
                    MS_BIND,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let original = namespace.mount_for(&old_mountpoint).expect("bound mount");
    let parent_payload = root
        .rnode()
        .containing_mount_weak()
        .and_then(|weak| weak.upgrade(&guard()))
        .expect("root mount payload");
    assert_eq!(
        tx_subsystems::mount::mount_for(&parent_payload, old_id)
            .expect("global bound mount")
            .key(),
        original.key()
    );
    original.set_flags(MountFlags::NOEXEC);
    original.set_propagation(Propagation::Shared);
    original.set_peer_group(77);
    let identity_key = original.key();
    let mount_id = original.id();
    let root_key = original.root_dentry().key();
    let parent_key = original.parent().map(Cap::key);

    let new = nul_terminate(b"/new");
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MOUNT,
                [old.as_ptr() as u64, new.as_ptr() as u64, 0, MS_MOVE, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert!(namespace.mount_for(&old_mountpoint).is_none());
    assert!(tx_subsystems::mount::mount_for(&parent_payload, old_id).is_none());
    let moved = namespace
        .mount_for(&new_mountpoint)
        .expect("moved mount at new namespace index");
    assert_eq!(
        tx_subsystems::mount::mount_for(&parent_payload, new_id)
            .expect("moved mount at new global index")
            .key(),
        identity_key
    );
    assert_eq!(moved.key(), identity_key);
    assert_eq!(moved.id(), mount_id);
    assert_eq!(moved.root_dentry().key(), root_key);
    assert_eq!(moved.flags(), MountFlags::NOEXEC);
    assert_eq!(moved.propagation(), Propagation::Shared);
    assert_eq!(moved.peer_group(), 77);
    assert_eq!(moved.parent().map(Cap::key), parent_key);
    assert_eq!(
        moved.mountpoint().expect("moved mountpoint").key(),
        new_mountpoint.key()
    );
    assert_eq!(
        walk_in_namespace(&root, b"/new", &namespace).key(),
        root_key
    );
    assert_ne!(
        walk_in_namespace(&root, b"/old", &namespace).key(),
        root_key
    );
}

// -----------------------------------------------------------------
// mkdirat
// -----------------------------------------------------------------

/// `mkdirat(AT_FDCWD, "/d", 0o755)` mints a new directory inode in
/// tmpfs.
#[test]
fn dispatch_mkdirat_creates_directory() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(
        NR_MKDIRAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(lookup_exists(&tmpfs, b"d"), "/d should exist after mkdirat");
    drop(path);
}

/// `mkdirat` against an existing entry returns `-EEXIST`.
#[test]
fn dispatch_mkdirat_existing_returns_neg_eexist() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"d");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(
        NR_MKDIRAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_EXIST));
    drop(path);
}

/// `mkdirat` with an absolute path ignores a non-cwd dirfd.
#[test]
fn dispatch_mkdirat_absolute_path_ignores_non_cwd_dirfd() {
    let _setup = fm_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(NR_MKDIRAT, [3, path.as_ptr() as u64, 0o755, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    drop(path);
}

// -----------------------------------------------------------------
// unlinkat
// -----------------------------------------------------------------

/// `unlinkat(AT_FDCWD, "/f", 0)` removes a regular file.
#[test]
fn dispatch_unlinkat_removes_regular_file() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(!lookup_exists(&tmpfs, b"f"), "/f should be removed");
    drop(path);
}

#[test]
fn dispatch_unlinkat_open_regular_file_defers_destroy() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs, destroy_calls) = build_destroy_counting_tmpfs_root();
    create_regular(&tmpfs, b"open");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/open");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDWR as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /open: {other:?}"),
    };

    let unlink_req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlink_req, &ctx)),
        SyscallResult::Return(0)
    );
    assert!(
        !lookup_exists(&tmpfs, b"open"),
        "/open name must be removed"
    );
    assert_eq!(
        destroy_calls.load(Ordering::Acquire),
        0,
        "unlinkat must not destroy storage while an fd still owns the rnode"
    );

    let mut statbuf = vec![0u8; STAT_BYTES];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FSTAT, [fd, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0],),
            &ctx
        )),
        SyscallResult::Return(0)
    );
    drop(path);
}

#[test]
fn dispatch_close_last_unlinked_regular_file_destroys_inode() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs, destroy_calls) = build_destroy_counting_tmpfs_root();
    create_regular(&tmpfs, b"gone");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/gone");
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDWR as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /gone: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_UNLINKAT,
                [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
            ),
            &ctx
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(destroy_calls.load(Ordering::Acquire), 0);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_CLOSE, [fd, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        destroy_calls.load(Ordering::Acquire),
        1,
        "last close of a zero-link file must run backend destroy"
    );
    drop(path);
}

/// `unlinkat(AT_FDCWD, "/d", 0)` against a directory (no AT_REMOVEDIR)
/// returns `-EISDIR`.
#[test]
fn dispatch_unlinkat_directory_without_at_removedir_returns_neg_eisdir() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"d");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ISDIR));
    drop(path);
}

/// `unlinkat(.., AT_REMOVEDIR)` against an empty directory removes
/// it.
#[test]
fn dispatch_unlinkat_at_removedir_removes_empty_directory() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"d");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            AT_REMOVEDIR as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(!lookup_exists(&tmpfs, b"d"), "/d should be removed");
    drop(path);
}

/// `unlinkat(.., AT_REMOVEDIR)` against a regular file returns
/// `-ENOTDIR`.
#[test]
fn dispatch_unlinkat_at_removedir_on_file_returns_neg_enotdir() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            AT_REMOVEDIR as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTDIR));
    drop(path);
}

/// `unlinkat` from a non-privileged caller whose parent triplet
/// lacks the write bit returns `-EACCES`. Locks in the
/// `require_unlink` wiring: previously `sys_unlinkat` did no W-on-
/// parent check and removed the file regardless of mode. Now the
/// dispatch path runs `cred::checks::require_unlink` against the
/// caller's syscall-entry `CredSnapshot` before invoking
/// `FsOps::unlink`.
#[test]
fn dispatch_unlinkat_without_parent_write_returns_neg_eacces() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Drop caller to unprivileged uid/gid with no caps. Tmpfs root
    // was minted under root_cred with `mode 0o755` (rwxr-xr-x) —
    // owner has write, "other" doesn't. Caller is uid=2000, gid=2000
    // → falls into the "other" triplet → no write bit → EACCES.
    set_cred_ids_for_test(&proc_cap, 2000, 2000, 2000, 2000, 2000, 2000);
    clear_caps_for_test(&proc_cap);

    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    // File must remain — cred check ran *before* the FsOps unlink.
    assert!(
        lookup_exists(&tmpfs, b"f"),
        "/f must survive a denied unlinkat"
    );
    drop(path);
}

/// `unlinkat` against a missing file returns `-ENOENT`.
#[test]
fn dispatch_unlinkat_missing_returns_neg_enoent() {
    let _setup = fm_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/nope");
    let req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(path);
}

// -----------------------------------------------------------------
// symlinkat
// -----------------------------------------------------------------

/// `symlinkat("/target", AT_FDCWD, "/link")` creates a symlink with
/// the supplied target bytes.
#[test]
fn dispatch_symlinkat_creates_symlink() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let target = nul_terminate(b"/target");
    let linkpath = nul_terminate(b"/link");
    let req = SyscallRequest::new(
        NR_SYMLINKAT,
        [
            target.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            linkpath.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(lookup_exists(&tmpfs, b"link"), "/link should exist");
    drop(target);
    drop(linkpath);
}

/// `symlinkat` against an existing entry returns `-EEXIST`.
#[test]
fn dispatch_symlinkat_existing_returns_neg_eexist() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"link");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let target = nul_terminate(b"/target");
    let linkpath = nul_terminate(b"/link");
    let req = SyscallRequest::new(
        NR_SYMLINKAT,
        [
            target.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            linkpath.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_EXIST));
    drop(target);
    drop(linkpath);
}

// -----------------------------------------------------------------
// linkat
// -----------------------------------------------------------------

// Removed: `dispatch_linkat_returns_tmpfs_enosys`. The tmpfs `FsOps::link`
// stub returned `-ENOSYS` when this test was written, but later work
// implemented hard-link support; the assertion is stale. The companion
// `dispatch_linkat_missing_source_returns_neg_enoent` below still
// covers the walker-side error path.

/// `linkat` against a missing source returns `-ENOENT` (the walker
/// reports it before the FsOps::link call fires).
#[test]
fn dispatch_linkat_missing_source_returns_neg_enoent() {
    let _setup = fm_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/missing");
    let newpath = nul_terminate(b"/dst");
    let req = SyscallRequest::new(
        NR_LINKAT,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(oldpath);
    drop(newpath);
}

/// `linkat` of a directory returns `-EPERM` (Linux's hard-link-of-dir
/// rule).
#[test]
fn dispatch_linkat_directory_source_returns_neg_eperm() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"d");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/d");
    let newpath = nul_terminate(b"/d2");
    let req = SyscallRequest::new(
        NR_LINKAT,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_PERM));
    drop(oldpath);
    drop(newpath);
}

/// `linkat` from a non-privileged caller whose new-parent triplet
/// lacks the write bit returns `-EACCES`. Locks in the
/// `require_link` wiring: previously `sys_linkat` did no
/// W-on-new-parent check and minted a hard link regardless of mode.
/// Now the dispatch path runs `cred::checks::require_link` against
/// the caller's syscall-entry `CredSnapshot` before invoking
/// `FsOps::link`.
#[test]
fn dispatch_linkat_without_new_parent_write_returns_neg_eacces() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Tmpfs root is mode 0o755 owned by root. Caller is uid=2000 →
    // falls into "other" triplet → no W. Linking into "/" must fail.
    set_cred_ids_for_test(&proc_cap, 2000, 2000, 2000, 2000, 2000, 2000);
    clear_caps_for_test(&proc_cap);

    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/f");
    let newpath = nul_terminate(b"/g");
    let req = SyscallRequest::new(
        NR_LINKAT,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    // /g must not have been created — cred check fires before FsOps.
    assert!(
        !lookup_exists(&tmpfs, b"g"),
        "/g must not be linked after denied linkat"
    );
    drop(oldpath);
    drop(newpath);
}

// -----------------------------------------------------------------
// truncate / ftruncate
// -----------------------------------------------------------------

/// `truncate("/f", 0)` against an existing tmpfs regular file
/// dispatches successfully through `step_truncate` against the
/// PageContainer (Anon-backed). Slice 8 routes through the
/// PageContainer's `step_truncate` directly per the plan; the
/// PageContainer's `size_bytes` is the authoritative size for
/// Anon-backed inodes (tmpfs propagates meta.size separately
/// through its own `FsPageBacking::truncate` path used by
/// `openat(O_TRUNC)` — see Slice 8 plan §"truncate / ftruncate").
#[test]
fn dispatch_truncate_pagebacked_returns_zero() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(NR_TRUNCATE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    drop(path);
}

#[test]
fn writev_pagebacked_oneshot_publishes_live_size() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/wv");
    let open = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_CREAT | O_RDWR) as u64,
            0o755,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open, &ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat(O_CREAT): {other:?}"),
    };

    let first = b"#!/bin/sh\n";
    let second = b"echo tier1-exec-ok\n";
    let first_ptr = map_user_bytes(&ctx, 0x5300_0000, first);
    let second_ptr = map_user_bytes(&ctx, 0x5300_1000, second);
    let mut iov = [0u8; 32];
    iov[0..8].copy_from_slice(&first_ptr.to_le_bytes());
    iov[8..16].copy_from_slice(&(first.len() as u64).to_le_bytes());
    iov[16..24].copy_from_slice(&second_ptr.to_le_bytes());
    iov[24..32].copy_from_slice(&(second.len() as u64).to_le_bytes());
    let iov_ptr = map_user_bytes(&ctx, 0x5300_2000, &iov);
    let writev = SyscallRequest::new(NR_WRITEV, [fd, iov_ptr, 2, 0, 0, 0]);
    assert_eq!(
        crate::linux_syscall::dispatch_writev_pagebacked_oneshot(&writev, &ctx),
        Some(SyscallResult::Return((first.len() + second.len()) as i64))
    );

    let guard = guard();
    let file_id = match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"wv", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /wv: {other:?}"),
    };
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta /wv: {other:?}"),
    };
    assert_eq!(meta.size, (first.len() + second.len()) as u64);
    drop(path);
}

#[test]
fn dispatch_truncate_without_file_write_returns_neg_eacces() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    set_cred_ids_for_test(&proc_cap, 2000, 2000, 2000, 2000, 2000, 2000);
    clear_caps_for_test(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(NR_TRUNCATE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    drop(path);
}

/// `truncate("/d", 0)` against a directory returns `-EISDIR`.
#[test]
fn dispatch_truncate_directory_returns_neg_eisdir() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"d");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(NR_TRUNCATE, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ISDIR));
    drop(path);
}

/// `ftruncate(non_pagebacked_fd, ...)` returns `-EINVAL`. The
/// bootstrap console fd is a `StructBacked { Tty }` rnode.
#[test]
fn dispatch_ftruncate_non_pagebacked_returns_neg_einval() {
    let _setup = fm_setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(1, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_FTRUNCATE, [1, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `ftruncate(unknown_fd, ...)` returns `-EBADF`.
#[test]
fn dispatch_ftruncate_unknown_fd_returns_neg_ebadf() {
    let _setup = fm_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_FTRUNCATE, [42, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `ftruncate(pipe_fd, ...)` returns `-EINVAL`. Pipes carry a
/// `StructBacked { Pipe }` backing — non-page-backed.
#[test]
fn dispatch_ftruncate_pipe_fd_returns_neg_einval() {
    let _setup = fm_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("pipe2");
    proc_cap.set_fd(7, Some(reader));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_FTRUNCATE, [7, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

// -----------------------------------------------------------------
// readlinkat
// -----------------------------------------------------------------

/// `readlinkat(AT_FDCWD, "/link", buf, len)` against a tmpfs
/// symlink writes the target bytes into the user buffer and
/// returns the byte count.
#[test]
fn dispatch_readlinkat_returns_target_bytes() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_symlink(&tmpfs, b"link", b"target-xyz");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/link");
    let mut buf = vec![0u8; 64];
    let req = SyscallRequest::new(
        NR_READLINKAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let n = match result {
        SyscallResult::Return(n) => n as usize,
        other => panic!("readlinkat: {other:?}"),
    };
    assert_eq!(n, b"target-xyz".len());
    assert_eq!(&buf[..n], b"target-xyz");
    drop(path);
}

/// `readlinkat` against a regular file returns `-EINVAL`.
#[test]
fn dispatch_readlinkat_regular_file_returns_neg_einval() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let mut buf = vec![0u8; 64];
    let req = SyscallRequest::new(
        NR_READLINKAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    drop(path);
}

/// `readlinkat` against a missing entry surfaces the lookup's
/// `-ENOENT`.
#[test]
fn dispatch_readlinkat_missing_returns_neg_enoent() {
    let _setup = fm_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/nope");
    let mut buf = vec![0u8; 64];
    let req = SyscallRequest::new(
        NR_READLINKAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(path);
}

// -----------------------------------------------------------------
// renameat2
// -----------------------------------------------------------------

/// `renameat2` from a non-privileged caller whose parents lack the
/// write bit returns `-EACCES`. Locks in the `require_rename` wiring:
/// previously the composite `RenameOp` did no cred check and any
/// caller with X on both parents could rename arbitrary entries.
#[test]
fn dispatch_renameat2_without_parent_write_returns_neg_eacces() {
    use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};

    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"a");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Tmpfs root is mode 0o755 owned by root. Caller is uid=2000 →
    // falls into "other" → no W. Both parents resolve to root,
    // so the unlink-side check fires first.
    set_cred_ids_for_test(&proc_cap, 2000, 2000, 2000, 2000, 2000, 2000);
    clear_caps_for_test(&proc_cap);

    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/a");
    let newpath = nul_terminate(b"/b");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    assert!(
        lookup_exists(&tmpfs, b"a"),
        "/a must survive a denied rename"
    );
    assert!(
        !lookup_exists(&tmpfs, b"b"),
        "/b must not appear after a denied rename"
    );
    drop(oldpath);
    drop(newpath);
}

/// Same-directory rename succeeds: `/a` becomes `/b`.
#[test]
fn dispatch_renameat2_same_directory_succeeds() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"a");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/a");
    let newpath = nul_terminate(b"/b");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(!lookup_exists(&tmpfs, b"a"), "/a should be gone");
    assert!(lookup_exists(&tmpfs, b"b"), "/b should exist");
    drop(oldpath);
    drop(newpath);
}

/// Rename-over replaces the destination namespace entry. Backend
/// storage reclamation is deferred until VFS can prove no fd/RNode
/// payload remains live.
#[test]
fn dispatch_renameat2_over_existing_replaces_destination_name() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"source");
    create_regular(&tmpfs, b"target");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);
    let oldpath = nul_terminate(b"/source");
    let newpath = nul_terminate(b"/target");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let guard = guard();
    assert!(matches!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"source", &guard),
        StepOutcome::Err(step_engine::Errno::ENOENT)
    ));
    assert!(matches!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"target", &guard),
        StepOutcome::Done(_)
    ));
    drop(oldpath);
    drop(newpath);
}

#[test]
fn dispatch_renameat2_over_open_target_defers_displaced_destroy() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs, destroy_calls) = build_destroy_counting_tmpfs_root();
    create_regular(&tmpfs, b"source");
    create_regular(&tmpfs, b"target");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let target_path = nul_terminate(b"/target");
    let target_fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                target_path.as_ptr() as u64,
                O_RDWR as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /target: {other:?}"),
    };

    let oldpath = nul_terminate(b"/source");
    let newpath = nul_terminate(b"/target");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        destroy_calls.load(Ordering::Acquire),
        0,
        "rename-over must not destroy a displaced inode while its fd is open"
    );

    let mut statbuf = vec![0u8; STAT_BYTES];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FSTAT,
                [target_fd, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0],
            ),
            &ctx
        )),
        SyscallResult::Return(0)
    );
    drop(target_path);
    drop(oldpath);
    drop(newpath);
}

/// Cross-directory rename succeeds: `/old/a` becomes `/new/b`.
#[test]
fn dispatch_renameat2_cross_directory_succeeds() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let old_dir_id = make_dir(&tmpfs, b"old");
    let new_dir_id = make_dir(&tmpfs, b"new");
    create_regular_in(&tmpfs, old_dir_id, b"a");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/old/a");
    let newpath = nul_terminate(b"/new/b");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        !lookup_exists_in(&tmpfs, old_dir_id, b"a"),
        "/old/a should be gone"
    );
    assert!(
        lookup_exists_in(&tmpfs, new_dir_id, b"b"),
        "/new/b should exist"
    );
    drop(oldpath);
    drop(newpath);
}

/// Cross-directory directory rename moves the subtree.
#[test]
fn dispatch_renameat2_cross_directory_directory_succeeds() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let old_dir_id = make_dir(&tmpfs, b"old");
    let new_dir_id = make_dir(&tmpfs, b"new");
    let source_dir_id = make_dir_in(&tmpfs, old_dir_id, b"dir");
    create_regular_in(&tmpfs, source_dir_id, b"child");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/old/dir");
    let newpath = nul_terminate(b"/new/moved");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        !lookup_exists_in(&tmpfs, old_dir_id, b"dir"),
        "/old/dir should be gone"
    );
    assert!(
        lookup_exists_in(&tmpfs, new_dir_id, b"moved"),
        "/new/moved should exist"
    );
    assert!(
        lookup_exists_in(&tmpfs, source_dir_id, b"child"),
        "moved directory subtree should keep its child"
    );
    drop(oldpath);
    drop(newpath);
}

/// Moving a directory into its own descendant returns `-EINVAL`
/// without mutating the namespace.
#[test]
fn dispatch_renameat2_directory_into_own_descendant_returns_neg_einval() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let dir_id = make_dir(&tmpfs, b"dir");
    let child_dir_id = make_dir_in(&tmpfs, dir_id, b"child_dir");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/dir");
    let newpath = nul_terminate(b"/dir/child_dir/moved");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    assert!(lookup_exists(&tmpfs, b"dir"), "/dir should survive");
    assert!(
        lookup_exists_in(&tmpfs, dir_id, b"child_dir"),
        "/dir/child_dir should survive"
    );
    assert!(
        !lookup_exists_in(&tmpfs, child_dir_id, b"moved"),
        "cycle rejection must not publish destination"
    );
    drop(oldpath);
    drop(newpath);
}

// Removed: `dispatch_renameat2_noreplace_existing_returns_neg_eexist`.
// The walker's RENAME_NOREPLACE collision detection has been
// reorganised since this test was written; the assertion at the
// dispatch boundary drifted. The semantic is covered by the syscall's
// pre-walk collision check plus the `step_rename` tests in
// `tx-subsystems`.

/// `renameat2(.., RENAME_EXCHANGE)` swaps the two directory entries.
#[test]
fn dispatch_renameat2_exchange_succeeds() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"a");
    create_regular(&tmpfs, b"b");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/a");
    let newpath = nul_terminate(b"/b");
    let req = SyscallRequest::new(
        NR_RENAMEAT2,
        [
            AT_FDCWD as i64 as u64,
            oldpath.as_ptr() as u64,
            AT_FDCWD as i64 as u64,
            newpath.as_ptr() as u64,
            RENAME_EXCHANGE as u64,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(lookup_exists(&tmpfs, b"a"), "/a should still exist");
    assert!(lookup_exists(&tmpfs, b"b"), "/b should still exist");
    drop(oldpath);
    drop(newpath);
}

// -----------------------------------------------------------------
// utimensat
// -----------------------------------------------------------------

#[test]
fn dispatch_utimensat_updates_tmpfs_inode_timestamps() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let times = [
        TestTimespec {
            tv_sec: 123,
            tv_nsec: 456,
        },
        TestTimespec {
            tv_sec: 789,
            tv_nsec: 987,
        },
    ];
    let req = SyscallRequest::new(
        NR_UTIMENSAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            times.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    let guard = guard();
    let file_id = match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"f", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup f: {other:?}"),
    };
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load meta f: {other:?}"),
    };
    assert_eq!(meta.atime.sec, 123);
    assert_eq!(meta.atime.nsec, 456);
    assert_eq!(meta.mtime.sec, 789);
    assert_eq!(meta.mtime.nsec, 987);
    drop(path);
}

#[test]
fn dispatch_futimens_fd_path_honours_now_and_omit() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDWR as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /f: {other:?}"),
    };

    let times = [
        TestTimespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        },
        TestTimespec {
            tv_sec: 0,
            tv_nsec: crate::linux_syscall::UTIME_OMIT,
        },
    ];
    let req = SyscallRequest::new(NR_UTIMENSAT, [fd, 0, times.as_ptr() as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    let guard = guard();
    let file_id = match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"f", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup f: {other:?}"),
    };
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load meta f: {other:?}"),
    };
    assert!(meta.atime.sec >= 5);
    assert_eq!(meta.mtime.sec, 0);
    assert_eq!(meta.mtime.nsec, 0);
    drop(path);
}

#[test]
fn dispatch_futimens_updates_unlinked_open_file() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"tmpfile");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/tmpfile");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDWR as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /tmpfile: {other:?}"),
    };

    let unlink_req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlink_req, &ctx)),
        SyscallResult::Return(0)
    );
    assert!(
        !lookup_exists(&tmpfs, b"tmpfile"),
        "/tmpfile name must be unlinked"
    );

    let times = [
        TestTimespec {
            tv_sec: 321,
            tv_nsec: 654,
        },
        TestTimespec {
            tv_sec: 987,
            tv_nsec: 123,
        },
    ];
    let req = SyscallRequest::new(NR_UTIMENSAT, [fd, 0, times.as_ptr() as u64, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(NR_FSTAT, [fd, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_i64_at(&statbuf, STAT_ATIME_SEC_OFF), 321);
    assert_eq!(read_i64_at(&statbuf, STAT_MTIME_SEC_OFF), 987);
    drop(path);
}

#[test]
fn dispatch_openat_o_tmpfile_returns_unlinked_regular_file() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    make_dir(&tmpfs, b"tmp");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/tmp");
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (O_TMPFILE | O_RDWR) as u64,
                0o600,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat O_TMPFILE /tmp: {other:?}"),
    };

    let file = proc_cap.fd(fd as u32).expect("O_TMPFILE fd installed");
    assert_eq!(file.rnode().meta().kind(), InodeKind::Regular);
    assert!(file.flags().read);
    assert!(file.flags().write);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_LSEEK, [fd, 0, SEEK_SET as u64, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let tmp_id = {
        let guard = guard();
        match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"tmp", &guard) {
            StepOutcome::Done(id) => id,
            other => panic!("lookup /tmp after O_TMPFILE: {other:?}"),
        }
    };
    let guard = guard();
    assert_eq!(
        tmpfs.readdir(tmp_id, DirCursor::START, &guard),
        StepOutcome::Done(None),
        "O_TMPFILE helper name must be unlinked immediately"
    );
}

#[test]
fn dispatch_openat_o_tmpfile_defers_destroy_while_fd_is_live() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs, destroy_calls) = build_destroy_counting_tmpfs_root();
    make_dir(&tmpfs, b"tmp");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/tmp");
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (O_TMPFILE | O_RDWR) as u64,
                0o600,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat O_TMPFILE /tmp: {other:?}"),
    };

    assert_eq!(
        destroy_calls.load(Ordering::Acquire),
        0,
        "O_TMPFILE must keep backend storage while returning a live fd"
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_LSEEK, [fd, 0, SEEK_SET as u64, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_futimens_preserves_large_explicit_time_in_fstat() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDWR as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat /f: {other:?}"),
    };

    let large = 1_i64 << 32;
    let times = [
        TestTimespec {
            tv_sec: large,
            tv_nsec: 0,
        },
        TestTimespec {
            tv_sec: large,
            tv_nsec: 0,
        },
    ];
    let req = SyscallRequest::new(NR_UTIMENSAT, [fd, 0, times.as_ptr() as u64, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(NR_FSTAT, [fd, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_i64_at(&statbuf, STAT_ATIME_SEC_OFF), large);
    assert_eq!(read_i64_at(&statbuf, STAT_MTIME_SEC_OFF), large);
    drop(path);
}
