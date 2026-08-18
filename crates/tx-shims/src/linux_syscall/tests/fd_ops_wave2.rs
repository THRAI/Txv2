// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::{
    self as step_engine, guard as ebr_guard, page_allocator, reserve_for, sign_for, Cap,
    StepOutcome,
};
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_subsystems::cred::{step_setresuid, CapabilitySet, Uid};
use tx_subsystems::cross_crate_test_support::clear_caps_for_test;
use tx_subsystems::device::{CharDeviceBinding, CharDeviceOps, DevT};
use tx_subsystems::execution::Guard;
use tx_subsystems::mount::{
    mount_for, DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions,
    MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::process::{
    step_chdir, step_chdir_with_mount, step_fork, step_set_mount_namespace,
};
use tx_subsystems::tty::execution::{register_console_alias, register_hardware};
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, StructPayload,
    S_IFDIR,
};
use tx_subsystems::vfs::FsOps;

use crate::linux_syscall::{
    AT_FDCWD, EXECVE_PATH_MAX, F_OK, NR_CLOSE, NR_DUP, NR_DUP3, NR_FACCESSAT, NR_MOUNT,
    NR_NEWFSTATAT, NR_OPENAT, NR_READLINKAT, NR_STATX, NR_SYNCFS, NR_UMOUNT2, NR_UNLINKAT,
    O_CLOEXEC, O_CREAT, O_DIRECT, O_DIRECTORY, O_EXCL, O_NOCTTY, O_NOFOLLOW, O_NONBLOCK, O_RDONLY,
    O_RDWR, O_TRUNC, R_OK, W_OK,
};

/// errno magnitudes: positive Linux RV64 generic ABI values.
const E_BADF: i32 = 9;
const E_NOENT: i32 = 2;
const E_EXIST: i32 = 17;
const E_NOTDIR: i32 = 20;
const E_INVAL: i32 = 22;
const E_ACCES: i32 = 13;
const E_NAMETOOLONG: i32 = 36;
const E_MFILE: i32 = 24;
const E_ISDIR: i32 = 21;
const E_NXIO: i32 = 6;
const E_LOOP: i32 = 40;

const STAT_BYTES: usize = 128;
const STAT_MODE_OFF: usize = 16;
const STAT_RDEV_OFF: usize = 32;
const STATX_BYTES: usize = 256;
const STATX_MODE_OFF: usize = 28;
const STATX_RDEV_MAJOR_OFF: usize = 128;
const STATX_RDEV_MINOR_OFF: usize = 132;

struct NoopTtyOps;

impl CharDeviceOps for NoopTtyOps {
    fn read(
        &self,
        _out: &mut [u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, step_engine::ByteProgress> {
        StepOutcome::Done(0)
    }

    fn write(
        &self,
        bytes: &[u8],
        _guard: &Guard<'_>,
    ) -> StepOutcome<usize, step_engine::ByteProgress> {
        StepOutcome::Done(bytes.len())
    }
}

static NOOP_TTY_OPS: NoopTtyOps = NoopTtyOps;
static NOOP_TTY_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(4, 64),
    name: "fdops-tty",
    ops: &NOOP_TTY_OPS,
};

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for fd-ops wave2 tests: {error:?}"),
    }
}

fn fd_ops_setup() -> TestSetup {
    let setup = setup();
    ensure_zero_frame_claimed();
    setup
}

/// Build a fresh tmpfs-backed mount + a root `Cap<DEntry>`.
/// Mirrors the wave4_setup helper's shape but lives in this module
/// so the fd-ops tests don't depend on wave4's path.
fn build_tmpfs_root_with_mount() -> (Cap<DEntry>, Arc<Tmpfs>, Cap<MountIdentity>) {
    let tmpfs = Arc::new(Tmpfs::new());
    let payload = MountPayload::new_cap(
        tmpfs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        tmpfs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(201),
        MountOptions::default(),
        "tmpfs-fdops",
        SourceLabel::Static("tmpfs-fdops"),
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

    let mount = MountIdentity::new_cap(
        MountId::new(21),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    (root_dentry, tmpfs, mount)
}

fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
    let (root_dentry, tmpfs, _mount) = build_tmpfs_root_with_mount();
    (root_dentry, tmpfs)
}

#[derive(Default)]
struct RecordingFsyncBacking {
    fsyncs: AtomicUsize,
}

#[derive(Default)]
struct RejectingFilesystemSyncBacking {
    sync_filesystem_calls: AtomicUsize,
}

impl FsPageBacking for RejectingFilesystemSyncBacking {
    fn fetch_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<tx_subsystems::page_backed::Frame, step_engine::NoProgress> {
        unreachable!("syncfs mount-settlement test has no resident pages")
    }

    fn flush_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _offset: u64,
        _frame: &tx_subsystems::page_backed::Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::err(tx_subsystems::execution::Errno::ENOSYS.into())
    }

    fn sync_filesystem(&self, _guard: &Guard<'_>) -> StepOutcome<(), step_engine::NoProgress> {
        self.sync_filesystem_calls.fetch_add(1, Ordering::AcqRel);
        StepOutcome::err(tx_subsystems::execution::Errno::ENOSYS.into())
    }
}

fn build_mount_settlement_only_root() -> (
    Cap<DEntry>,
    Arc<RejectingFilesystemSyncBacking>,
    Cap<MountIdentity>,
) {
    let tmpfs = Arc::new(Tmpfs::new());
    let backing = Arc::new(RejectingFilesystemSyncBacking::default());
    let payload = MountPayload::new_cap(
        tmpfs as Arc<dyn tx_subsystems::vfs::FsOps>,
        backing.clone(),
        None,
        DevId::new(205),
        MountOptions::default(),
        "mount-settlement-only",
        SourceLabel::Static("mount-settlement-only"),
    )
    .expect("mount-settlement-only payload");
    let root_rnode = {
        let raw = RNode::new(
            TMPFS_ROOT_OBJECT_ID,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let reservation = reserve_for::<RNode>().expect("root rnode reservation");
        sign_for(reservation, raw)
    };
    let mount = MountIdentity::new_cap(
        MountId::new(22),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount-settlement-only identity");
    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    (root_dentry, backing, mount)
}

#[test]
fn dispatch_syncfs_uses_mount_settlement_without_legacy_pagebacking_fallback() {
    let _setup = fd_ops_setup();
    let (root_dentry, backing, _mount) = build_mount_settlement_only_root();
    let (process, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(process, thread);
    let root_path = nul_terminate(b"/");
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                root_path.as_ptr() as u64,
                (O_RDONLY | O_DIRECTORY) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("open root for syncfs: {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SYNCFS, [fd, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        backing.sync_filesystem_calls.load(Ordering::Acquire),
        0,
        "MountSettlementOp is the canonical syncfs durability boundary"
    );
}

impl FsPageBacking for RecordingFsyncBacking {
    fn fetch_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<tx_subsystems::page_backed::Frame, step_engine::NoProgress> {
        unreachable!("dup3 flush test has no resident pages")
    }

    fn flush_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _offset: u64,
        _frame: &tx_subsystems::page_backed::Frame,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _new_size: u64,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        self.fsyncs.fetch_add(1, Ordering::AcqRel);
        StepOutcome::done(())
    }
}

fn recording_page_backed_open_file(
    fs_ops: Arc<Tmpfs>,
) -> (
    Cap<tx_subsystems::vfs::OpenFile>,
    Arc<RecordingFsyncBacking>,
) {
    use tx_subsystems::mount::MountPayloadPin;
    use tx_subsystems::page_backed::PageContainer;
    use tx_subsystems::vfs::OpenFileFlags;

    let backing = Arc::new(RecordingFsyncBacking::default());
    let mount = MountPayload::new_cap(
        fs_ops,
        backing.clone(),
        None,
        DevId::new(204),
        MountOptions::default(),
        "recording-fsync",
        SourceLabel::Static("recording-fsync"),
    )
    .expect("recording fsync mount payload");
    let pc = PageContainer::new_file_cap(
        MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
        tx_subsystems::vfs::FsObjectId::new(2),
        0,
    )
    .expect("recording file page container");
    let rnode = RNode::new_cap(
        tx_subsystems::vfs::FsObjectId::new(2),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("recording file rnode");
    let file = tx_subsystems::vfs::OpenFile::new_cap(rnode, OpenFileFlags::default())
        .expect("recording open file");
    (file, backing)
}

fn build_devfs_root() -> Cap<DEntry> {
    let payload = MountPayload::new_cap(
        tx_fs::devfs::Devfs::fs_ops_arc(),
        tx_fs::devfs::Devfs::fs_page_backing_arc(),
        None,
        DevId::new(202),
        MountOptions::default(),
        "devfs-fdops",
        SourceLabel::Static("devfs-fdops"),
    )
    .expect("devfs mount payload");

    let root_rnode = {
        let raw = RNode::new(
            tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = reserve_for::<RNode>().expect("devfs rnode reservation");
        sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(22),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("devfs mount identity");

    DEntry::new_cap(InlineName::ROOT, root_rnode).expect("devfs root dentry")
}

fn build_procfs_root() -> Cap<DEntry> {
    let procfs = tx_fs::procfs::Procfs::new();
    let fs_ops = tx_fs::procfs::Procfs::fs_ops_arc();
    let page_backing = Arc::new(tx_fs::procfs::Procfs::new())
        as Arc<dyn tx_subsystems::page_backed::FsPageBacking>;
    let payload = MountPayload::new_cap(
        fs_ops,
        page_backing,
        None,
        DevId::new(203),
        MountOptions::default(),
        "procfs-fdops",
        SourceLabel::Static("procfs-fdops"),
    )
    .expect("procfs mount payload");

    let guard = ebr_guard();
    let meta = match procfs.load_inode_meta(tx_fs::procfs::PROCFS_ROOT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load procfs root meta failed: {other:?}"),
    };
    let root_rnode =
        match procfs.materialise_rnode(tx_fs::procfs::PROCFS_ROOT_ID, meta, &payload, &guard) {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("materialise procfs root failed: {other:?}"),
        };

    let _mount = MountIdentity::new_cap(
        MountId::new(23),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("procfs mount identity");

    DEntry::new_cap(InlineName::ROOT, root_rnode).expect("procfs root dentry")
}

fn install_ttys0_console() {
    let guard = ebr_guard();
    let tty = match register_hardware("ttyS0", 0, &NOOP_TTY_BINDING, &guard) {
        StepOutcome::Done(tty) => tty,
        other => panic!("register_hardware(ttyS0) failed: {other:?}"),
    };
    assert_eq!(
        register_console_alias("console", tty),
        StepOutcome::Done(())
    );
}

fn install_ttys1() {
    let guard = ebr_guard();
    match register_hardware("ttyS1", 1, &NOOP_TTY_BINDING, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("register_hardware(ttyS1) failed: {other:?}"),
    }
}

/// Bootstrap an init process whose cwd is `root_dentry`. Returns
/// the (process, leader-thread) pair.
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

fn drop_privs_to(proc_cap: &Cap<ProcessIdentity>, uid: u32) {
    let target = Uid(uid);
    let outcome = step_setresuid(proc_cap, Some(target), Some(target), Some(target));
    assert!(
        matches!(outcome, tx_subsystems::cred::CredChange::Replaced { .. }),
        "drop_privs_to({uid}) must succeed from root: got {outcome:?}"
    );
    clear_caps_for_test(proc_cap);
}

/// NUL-terminate a path slice into a kernel-side `Vec<u8>` so the
/// inline `read_user_cstr` reads it correctly.
fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn openat_path(ctx: &SyscallCtx<'static>, dirfd: i32, path: &[u8], flags: u32) -> SyscallResult {
    let path = nul_terminate(path);
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            dirfd as i64 as u64,
            path.as_ptr() as u64,
            flags as u64,
            0,
            0,
            0,
        ],
    );
    block_on(dispatch::<ShimsTestPmap>(req, ctx))
}

fn read_u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes(buf[off..off + 2].try_into().expect("u16 field"))
}

fn read_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().expect("u32 field"))
}

fn read_u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(buf[off..off + 8].try_into().expect("u64 field"))
}

// ----------------------------------------------------------------
// openat
// ----------------------------------------------------------------

#[test]
fn dispatch_openat_ttys0_auto_acquires_controlling_tty() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let session = proc_cap.pgrp_cap().session_cap();
    assert!(session.controlling_tty_cap().is_none());

    let ctx = make_ctx(proc_cap.clone(), thread);
    let result = openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR);
    let fd = match result {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat ttyS0 failed: {other:?}"),
    };
    assert!(proc_cap.fd(fd).is_some());
    assert!(
        session.controlling_tty_cap().is_some(),
        "session leader opening ttyS0 without O_NOCTTY should acquire ctty"
    );
}

#[test]
fn dispatch_openat_ttys0_o_noctty_does_not_acquire_controlling_tty() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let session = proc_cap.pgrp_cap().session_cap();

    let ctx = make_ctx(proc_cap.clone(), thread);
    let result = openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR | O_NOCTTY);
    match result {
        SyscallResult::Return(fd) => assert!(proc_cap.fd(fd as u32).is_some()),
        other => panic!("openat ttyS0 O_NOCTTY failed: {other:?}"),
    }
    assert!(
        session.controlling_tty_cap().is_none(),
        "O_NOCTTY open should not acquire controlling tty"
    );
}

#[test]
fn dispatch_openat_ttys0_non_session_leader_does_not_acquire_controlling_tty() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (parent, _parent_thread) = bootstrap_with_cwd(root);
    let child = step_fork::<ShimsTestPmap>(&parent, false, false).expect("fork child");
    let child_thread = child.nth_thread(0).expect("child leader thread");
    let session = child.pgrp_cap().session_cap();
    assert_ne!(
        child.pid.0, session.sid.0,
        "forked child should not be a session leader"
    );

    let ctx = make_ctx(child.clone(), child_thread);
    let result = openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR);
    match result {
        SyscallResult::Return(fd) => assert!(child.fd(fd as u32).is_some()),
        other => panic!("openat ttyS0 by non-session-leader failed: {other:?}"),
    }
    assert!(
        session.controlling_tty_cap().is_none(),
        "non-session-leader open must not acquire controlling tty"
    );
}

#[test]
fn dispatch_openat_ttys1_after_existing_ctty_does_not_rebind() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    install_ttys1();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let session = proc_cap.pgrp_cap().session_cap();
    let ctx = make_ctx(proc_cap.clone(), thread);

    match openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR) {
        SyscallResult::Return(_) => {}
        other => panic!("openat ttyS0 failed: {other:?}"),
    }
    let first = session
        .controlling_tty_cap()
        .expect("ttyS0 open should bind ctty");

    match openat_path(&ctx, AT_FDCWD, b"ttyS1", O_RDWR) {
        SyscallResult::Return(fd) => assert!(proc_cap.fd(fd as u32).is_some()),
        other => panic!("openat ttyS1 should still return fd: {other:?}"),
    }
    let after = session
        .controlling_tty_cap()
        .expect("existing controlling tty should remain bound");
    assert_eq!(after, first, "second TTY open must not rebind ctty");
}

#[test]
fn dispatch_openat_dev_tty_without_controlling_tty_returns_enxio() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(proc_cap, thread);

    let result = openat_path(&ctx, AT_FDCWD, b"tty", O_RDWR);
    assert_eq!(result, SyscallResult::Error(E_NXIO));
}

#[test]
fn dispatch_openat_dev_tty_after_binding_opens_same_tty() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let session = proc_cap.pgrp_cap().session_cap();
    let ctx = make_ctx(proc_cap.clone(), thread);

    let first = openat_path(&ctx, AT_FDCWD, b"console", O_RDWR);
    match first {
        SyscallResult::Return(_) => {}
        other => panic!("openat console failed: {other:?}"),
    }
    let controlling = session
        .controlling_tty_cap()
        .expect("console open should bind ctty");

    let second = openat_path(&ctx, AT_FDCWD, b"tty", O_RDWR);
    let fd = match second {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat tty failed: {other:?}"),
    };
    let file = proc_cap.fd(fd).expect("fd from /dev/tty open");
    match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => assert_eq!(*tty, controlling),
        other => panic!("expected /dev/tty StructBacked::Tty, got {other:?}"),
    }
}

#[test]
fn dispatch_openat_tty_char_o_trunc_is_noop() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let first = openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR | O_TRUNC);
    match first {
        SyscallResult::Return(fd) => assert!(proc_cap.fd(fd as u32).is_some()),
        other => panic!("openat ttyS0 O_TRUNC should be a char-device no-op: {other:?}"),
    }

    let second = openat_path(&ctx, AT_FDCWD, b"tty", O_RDWR | O_TRUNC);
    match second {
        SyscallResult::Return(fd) => assert!(proc_cap.fd(fd as u32).is_some()),
        other => panic!("openat /dev/tty O_TRUNC should be a char-device no-op: {other:?}"),
    }
}

#[test]
fn dispatch_dev_tty_stat_access_and_statx_are_caller_relative() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(proc_cap, thread);

    match openat_path(&ctx, AT_FDCWD, b"console", O_RDWR) {
        SyscallResult::Return(_) => {}
        other => panic!("openat console should bind controlling tty: {other:?}"),
    }

    let path = nul_terminate(b"tty");
    let mut statbuf = vec![0u8; STAT_BYTES];
    let stat_req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx)),
        SyscallResult::Return(0)
    );
    let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
    assert_eq!(mode & 0o170000, 0o020000, "expected S_IFCHR; got {mode:#o}");
    assert_ne!(
        read_u64_at(&statbuf, STAT_RDEV_OFF),
        0,
        "/dev/tty needs a device rdev"
    );

    let access_req = SyscallRequest::new(
        NR_FACCESSAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (F_OK | R_OK | W_OK) as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(access_req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut statxbuf = vec![0u8; STATX_BYTES];
    let statx_req = SyscallRequest::new(
        NR_STATX,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            0,
            crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
            statxbuf.as_mut_ptr() as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(statx_req, &ctx)),
        SyscallResult::Return(0)
    );
    let statx_mode = read_u16_at(&statxbuf, STATX_MODE_OFF);
    assert_eq!(
        statx_mode & 0o170000,
        0o020000,
        "expected statx S_IFCHR; got {statx_mode:#o}"
    );
    assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MAJOR_OFF), 5);
    assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MINOR_OFF), 0);
    drop(path);
}

#[test]
fn dispatch_devfs_tty_alias_stat_reports_hardware_rdev() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(proc_cap, thread);

    for name in [b"ttyS0".as_slice(), b"console".as_slice()] {
        let path = nul_terminate(name);
        let mut statbuf = vec![0u8; STAT_BYTES];
        let stat_req = SyscallRequest::new(
            NR_NEWFSTATAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                statbuf.as_mut_ptr() as u64,
                0,
                0,
                0,
            ],
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx)),
            SyscallResult::Return(0),
            "newfstatat({})",
            core::str::from_utf8(name).unwrap()
        );
        let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
        assert_eq!(mode & 0o170000, 0o020000, "expected S_IFCHR");
        assert_eq!(
            read_u64_at(&statbuf, STAT_RDEV_OFF),
            0x440,
            "{} should report ttyS0 rdev 4:64",
            core::str::from_utf8(name).unwrap()
        );

        let mut statxbuf = vec![0u8; STATX_BYTES];
        let statx_req = SyscallRequest::new(
            NR_STATX,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                0,
                crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
                statxbuf.as_mut_ptr() as u64,
                0,
            ],
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(statx_req, &ctx)),
            SyscallResult::Return(0),
            "statx({})",
            core::str::from_utf8(name).unwrap()
        );
        assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MAJOR_OFF), 4);
        assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MINOR_OFF), 64);
        drop(path);
    }
}

#[test]
fn dispatch_dev_tty_stat_without_controlling_tty_returns_enxio() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let root = build_devfs_root();
    let (_proc_cap, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(_proc_cap, thread);

    let path = nul_terminate(b"tty");
    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_NXIO)
    );
    drop(path);
}

#[test]
fn dispatch_procfs_fd_readlink_renders_tty_target_path() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let dev_root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(dev_root);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let fd = match openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat ttyS0 should succeed: {other:?}"),
    };

    match step_chdir(&proc_cap, build_procfs_root()) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => panic!("init bootstrap zombified"),
    }

    let proc_path = alloc::format!("{}/fd/{}", proc_cap.pid.0, fd);
    let path = nul_terminate(proc_path.as_bytes());
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
    let n = match block_on(dispatch::<ShimsTestPmap>(req, &ctx)) {
        SyscallResult::Return(n) => n as usize,
        other => panic!("readlinkat /proc/<pid>/fd/{fd}: {other:?}"),
    };
    assert_eq!(&buf[..n], b"/dev/ttyS0");
    drop(path);
}

#[test]
fn dispatch_proc_self_fd_readlink_uses_caller_process_fd_table() {
    let _setup = fd_ops_setup();
    install_ttys0_console();
    let dev_root = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(dev_root);
    let ctx = make_ctx(proc_cap, thread);

    let fd = match openat_path(&ctx, AT_FDCWD, b"ttyS0", O_RDWR) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat ttyS0 should succeed: {other:?}"),
    };

    let proc_path = alloc::format!("/proc/self/fd/{}", fd);
    let path = nul_terminate(proc_path.as_bytes());
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
    let n = match block_on(dispatch::<ShimsTestPmap>(req, &ctx)) {
        SyscallResult::Return(n) => n as usize,
        other => panic!("readlinkat /proc/self/fd/{fd}: {other:?}"),
    };
    assert_eq!(&buf[..n], b"/dev/ttyS0");

    let mut statbuf = vec![0u8; STAT_BYTES];
    let stat_req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx)),
        SyscallResult::Return(0)
    );
    let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
    assert_eq!(mode & 0o170000, 0o020000, "expected S_IFCHR");
    assert_eq!(read_u64_at(&statbuf, STAT_RDEV_OFF), 0x440);

    let mut statxbuf = vec![0u8; STATX_BYTES];
    let statx_req = SyscallRequest::new(
        NR_STATX,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            0,
            crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
            statxbuf.as_mut_ptr() as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(statx_req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MAJOR_OFF), 4);
    assert_eq!(read_u32_at(&statxbuf, STATX_RDEV_MINOR_OFF), 64);
    drop(path);
}

/// `openat(AT_FDCWD, "/f", O_RDONLY)` against an existing tmpfs
/// file returns the lowest unused fd (which, with the bootstrap
/// init process having no preopened fds, is 0).
#[test]
fn dispatch_openat_existing_file_o_rdonly_returns_fd() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    match result {
        SyscallResult::Return(fd) => {
            assert!(fd >= 0, "openat returned negative fd: {fd}");
            // fd table now carries the OpenFile.
            assert!(
                proc_cap.fd(fd as u32).is_some(),
                "fd {fd} should be installed"
            );
        }
        other => panic!("openat existing file: {other:?}"),
    }
    drop(path);
}

#[test]
fn dispatch_openat_o_nofollow_on_final_symlink_returns_neg_eloop() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    match tmpfs.create_inode(
        TMPFS_ROOT_OBJECT_ID,
        b"target",
        0o100644,
        &owner_cred,
        &guard,
    ) {
        StepOutcome::Done(_) => {}
        other => panic!("create_inode target: {other:?}"),
    }
    match tmpfs.symlink(
        TMPFS_ROOT_OBJECT_ID,
        b"link",
        b"target",
        &owner_cred,
        &guard,
    ) {
        StepOutcome::Done(_) => {}
        other => panic!("symlink link -> target: {other:?}"),
    }
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let result = openat_path(&ctx, AT_FDCWD, b"/link", O_RDONLY | O_NOFOLLOW);
    assert_eq!(result, SyscallResult::Error(E_LOOP));
}

/// `openat` reports `EMFILE` before path lookup once the visible fd
/// table is full. Musl's `daemon_failure` relies on this ordering for
/// `open("/dev/null")` after `t_fdfill()`.
#[test]
fn dispatch_openat_full_fd_table_returns_neg_emfile_before_enoent() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat /f: {other:?}"),
    };
    let file = proc_cap.fd(oldfd).expect("oldfd installed");
    for fd in 1..1024 {
        let _ = proc_cap.install_fd(fd, file.clone());
    }

    let missing = nul_terminate(b"/missing");
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                missing.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_MFILE));
    drop(path);
    drop(missing);
}

/// `openat(AT_FDCWD, "/f", O_RDONLY | O_DIRECTORY)` against a
/// regular file returns `-ENOTDIR`. LTP's recursive tmpdir cleanup
/// uses exactly this probe before deciding whether to recurse or
/// `unlink(2)` the entry.
#[test]
fn dispatch_openat_o_directory_regular_file_returns_neg_enotdir() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDONLY | O_DIRECTORY) as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTDIR));
    drop(path);
}

#[test]
fn dispatch_openat_o_direct_preserves_regular_file_status_flag() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let path = nul_terminate(b"/f");
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (O_RDONLY | O_DIRECT) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    let fd = match result {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat O_DIRECT: {other:?}"),
    };
    assert!(
        proc_cap
            .fd(fd)
            .expect("O_DIRECT fd installed")
            .flags()
            .packet
    );
    drop(path);
}

#[test]
fn dispatch_o_direct_write_rejects_unaligned_user_buffer() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);
    let path = nul_terminate(b"/f");
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                (O_RDWR | O_DIRECT) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("openat O_DIRECT: {other:?}"),
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            crate::linux_syscall::NR_WRITE,
            [fd, 1, tx_subsystems::vm::USER_PAGE_SIZE as u64, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    drop(path);
}

/// `O_DIRECTORY` succeeds when the terminal path is actually a
/// directory.
#[test]
fn dispatch_openat_o_directory_directory_returns_fd() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDONLY | O_DIRECTORY) as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    match result {
        SyscallResult::Return(fd) => assert!(proc_cap.fd(fd as u32).is_some()),
        other => panic!("openat O_DIRECTORY on directory: {other:?}"),
    }
    drop(path);
}

/// Linux rejects write-capable opens of directory targets with `EISDIR`.
#[test]
fn dispatch_openat_rdwr_directory_returns_neg_eisdir() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/");
    let req = SyscallRequest::new(
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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ISDIR));
    drop(path);
}

/// Existing directory targets are not creatable regular files, even if the
/// requested access mode is read-only.
#[test]
fn dispatch_openat_creat_directory_returns_neg_eisdir() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDONLY | O_CREAT) as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ISDIR));
    drop(path);
}

/// `openat(AT_FDCWD, "/new", O_RDWR | O_CREAT, 0o644)` against a
/// missing file creates it via `FsOps::create_inode` and opens
/// the result. The new inode's mode is the supplied 0o644 plus
/// the regular-file kind bits.
#[test]
fn dispatch_openat_o_creat_creates_new_file() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/new");
    let flags = (O_RDWR | O_CREAT) as u64;
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            flags,
            0o644,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let fd = match result {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat O_CREAT: {other:?}"),
    };
    assert!(fd >= 0);
    assert!(proc_cap.fd(fd as u32).is_some());

    // Verify the new inode landed in tmpfs's directory.
    let guard = ebr_guard();
    let outcome = tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"new", &guard);
    assert!(
        matches!(outcome, StepOutcome::Done(_)),
        "tmpfs should now resolve /new: {outcome:?}"
    );
    drop(path);
}

#[test]
fn dispatch_openat_dirfd_relative_o_creat_creates_in_directory() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (mnt_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"mnt", 0o755, &owner_cred, &guard) {
        StepOutcome::Done(created) => created,
        other => panic!("mkdir mnt for openat dirfd test: {other:?}"),
    };
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let dir_path = nul_terminate(b"/mnt");
    let dir_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            dir_path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let dir_fd = match block_on(dispatch::<ShimsTestPmap>(dir_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat /mnt should return a directory fd: {other:?}"),
    };

    let child_path = nul_terminate(b"test_openat.txt");
    let child_req = SyscallRequest::new(
        NR_OPENAT,
        [
            dir_fd as u64,
            child_path.as_ptr() as u64,
            (O_RDWR | O_CREAT) as u64,
            0o600,
            0,
            0,
        ],
    );
    let child_fd = match block_on(dispatch::<ShimsTestPmap>(child_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("dirfd-relative openat O_CREAT should create file: {other:?}"),
    };
    assert!(child_fd >= 0);
    assert!(proc_cap.fd(child_fd as u32).is_some());

    let guard = ebr_guard();
    let lookup = tmpfs.lookup(mnt_id, b"test_openat.txt", &guard);
    assert!(
        matches!(lookup, StepOutcome::Done(_)),
        "dirfd-relative O_CREAT should create under /mnt: {lookup:?}"
    );
    drop(child_path);
    drop(dir_path);
}

#[test]
fn dispatch_openat_after_mount_umount_creates_on_uncovered_mountpoint() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs, root_mount) = build_tmpfs_root_with_mount();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (mnt_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"mnt", 0o755, &owner_cred, &guard) {
        StepOutcome::Done(created) => created,
        other => panic!("mkdir mnt for mount/umount openat test: {other:?}"),
    };
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let mnt_ns = MountNamespace::new_cap(root_mount).expect("mount namespace");
    step_set_mount_namespace(&proc_cap, mnt_ns).expect("install mount namespace");
    let ctx = make_ctx(proc_cap.clone(), thread);

    let source = nul_terminate(b"none");
    let target = nul_terminate(b"/mnt");
    let fstype = nul_terminate(b"tmpfs");
    let mount_req = SyscallRequest::new(
        NR_MOUNT,
        [
            source.as_ptr() as u64,
            target.as_ptr() as u64,
            fstype.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(mount_req, &ctx)),
        SyscallResult::Return(0)
    );

    let umount_req = SyscallRequest::new(NR_UMOUNT2, [target.as_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(umount_req, &ctx)),
        SyscallResult::Return(0)
    );

    let dir_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            target.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let dir_fd = match block_on(dispatch::<ShimsTestPmap>(dir_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat /mnt after umount should return a directory fd: {other:?}"),
    };

    let child_path = nul_terminate(b"test_openat.txt");
    let child_req = SyscallRequest::new(
        NR_OPENAT,
        [
            dir_fd as u64,
            child_path.as_ptr() as u64,
            (O_RDWR | O_CREAT) as u64,
            0o600,
            0,
            0,
        ],
    );
    let child_fd = match block_on(dispatch::<ShimsTestPmap>(child_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("dirfd-relative openat O_CREAT after umount should create file: {other:?}"),
    };
    assert!(child_fd >= 0);

    let guard = ebr_guard();
    let lookup = tmpfs.lookup(mnt_id, b"test_openat.txt", &guard);
    assert!(
        matches!(lookup, StepOutcome::Done(_)),
        "file should be created on the uncovered /mnt directory, not the stale mounted tmpfs: {lookup:?}"
    );
    drop(child_path);
    drop(fstype);
    drop(target);
    drop(source);
}

#[test]
fn dispatch_openat_creat_uses_process_mount_namespace_without_global_mount_table() {
    let _setup = fd_ops_setup();
    let (_legacy_root_dentry, root_tmpfs, root_mount) = build_tmpfs_root_with_mount();
    let root_dentry = root_mount.root_dentry().clone();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (mnt_id, _) =
        match root_tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"mnt", 0o755, &owner_cred, &guard) {
            StepOutcome::Done(created) => created,
            other => panic!("mkdir mnt for namespace-only openat test: {other:?}"),
        };
    let mountpoint = match tx_subsystems::vfs::walker::step_walk(
        root_dentry.clone(),
        b"/mnt",
        &owner_cred,
        &guard,
    ) {
        StepOutcome::Done(dentry) => dentry,
        other => panic!("walk /mnt for namespace-only openat test: {other:?}"),
    };
    drop(guard);

    let (child_root, child_tmpfs, child_mount) = build_tmpfs_root_with_mount();
    let child_mount = MountIdentity::new_cap_with_root_dentry(
        MountId::new(121),
        Some(mountpoint.clone()),
        child_root,
        Some(root_mount.clone()),
        child_mount
            .payload_cap()
            .expect("child payload alive")
            .into_cap(),
        MountFlags::empty(),
    )
    .expect("namespace-only child mount");

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry.clone());
    let namespace = MountNamespace::new_cap(root_mount.clone()).expect("mount namespace");
    namespace.register_mount(&mountpoint, child_mount.clone());
    step_set_mount_namespace(&proc_cap, namespace).expect("install mount namespace");
    match step_chdir_with_mount(&proc_cap, root_dentry, root_mount) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => panic!("init bootstrap zombified"),
    }
    let cwd_binding = proc_cap
        .cwd_binding()
        .expect("namespace-only openat test needs mounted cwd");
    let mount_namespace = proc_cap
        .mount_namespace_cap()
        .expect("namespace-only openat test needs mount namespace");
    let guard = ebr_guard();
    match tx_subsystems::vfs::walker::step_walk_in_mount_namespace_with_origin_mount(
        cwd_binding.dentry,
        &cwd_binding.mount,
        b"/mnt",
        &owner_cred,
        &mount_namespace,
        &guard,
    ) {
        StepOutcome::Done(resolved) => assert_eq!(
            resolved.dentry.rnode().fs_object_id(),
            TMPFS_ROOT_OBJECT_ID,
            "namespace walk must cross /mnt to child tmpfs root"
        ),
        other => panic!("namespace walk /mnt before openat failed: {other:?}"),
    }
    drop(guard);

    let ctx = make_ctx(proc_cap.clone(), thread);
    let path = nul_terminate(b"/mnt/testshm");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK) as u64,
            0o600,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("namespace-only openat O_CREAT should create in mounted tmpfs: {other:?}"),
    };
    assert!(fd >= 0);

    let guard = ebr_guard();
    let child_lookup = child_tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"testshm", &guard);
    assert!(
        matches!(child_lookup, StepOutcome::Done(_)),
        "openat must create in namespace-mounted child tmpfs: {child_lookup:?}"
    );
    let root_lookup = root_tmpfs.lookup(mnt_id, b"testshm", &guard);
    assert!(
        matches!(root_lookup, StepOutcome::Err(step_engine::Errno::ENOENT)),
        "openat must not fall back to the uncovered parent tmpfs: {root_lookup:?}"
    );
    drop(guard);

    // Unlink must not depend on the positive dentry left behind by openat.
    // LTP creates its shared result file under /dev/shm and immediately
    // unlinks it; at early boot this lookup is commonly cache-cold.  The
    // relative no-follow basename walk must retain the mounted tmpfs as its
    // origin rather than interpreting its local inode ids through rootfs.
    child_mount.root_dentry().clear_cached_children();

    let unlink_req = SyscallRequest::new(
        NR_UNLINKAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unlink_req, &ctx)),
        SyscallResult::Return(0)
    );
    let guard = ebr_guard();
    let child_after_unlink = child_tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"testshm", &guard);
    assert!(
        matches!(
            child_after_unlink,
            StepOutcome::Err(step_engine::Errno::ENOENT)
        ),
        "unlinkat must remove from namespace-mounted child tmpfs: {child_after_unlink:?}"
    );
    drop(path);
}

#[test]
fn dispatch_umount2_in_cloned_mount_namespace_preserves_parent_and_global_mount() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs, root_mount) = build_tmpfs_root_with_mount();
    let root_payload = root_mount
        .payload_cap()
        .expect("root mount payload")
        .into_cap();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (mnt_id, _) = match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"mnt", 0o755, &owner_cred, &guard) {
        StepOutcome::Done(created) => created,
        other => panic!("mkdir mnt for cloned umount test: {other:?}"),
    };
    drop(guard);

    let (parent, parent_thread) = bootstrap_with_cwd(root_dentry.clone());
    let parent_namespace = MountNamespace::new_cap(root_mount).expect("parent mount namespace");
    step_set_mount_namespace(&parent, parent_namespace.clone()).expect("install parent namespace");
    let parent_ctx = make_ctx(parent.clone(), parent_thread);

    let source = nul_terminate(b"none");
    let target = nul_terminate(b"/mnt");
    let fstype = nul_terminate(b"tmpfs");
    let mount_req = SyscallRequest::new(
        NR_MOUNT,
        [
            source.as_ptr() as u64,
            target.as_ptr() as u64,
            fstype.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(mount_req, &parent_ctx)),
        SyscallResult::Return(0)
    );

    let mountpoint = parent_namespace
        .root_dentry()
        .cached_child(InlineName::new(b"mnt").expect("mountpoint name"))
        .expect("cached mountpoint");
    let parent_mount = parent_namespace
        .mount_for(&mountpoint)
        .expect("parent namespace mount");
    assert_eq!(
        mount_for(&root_payload, mnt_id)
            .expect("legacy global mount")
            .key(),
        parent_mount.key()
    );

    let child = tx_subsystems::process::step_fork_with_options::<ShimsTestPmap>(
        &parent,
        tx_subsystems::process::ForkOptions {
            clone_newns: true,
            ..tx_subsystems::process::ForkOptions::default()
        },
    )
    .expect("fork child with CLONE_NEWNS");
    let child_namespace = child.mount_namespace_cap().expect("child mount namespace");
    let child_mount = child_namespace
        .mount_for(&mountpoint)
        .expect("child namespace mount");
    assert_ne!(child_mount.key(), parent_mount.key());

    let child_ctx = make_ctx(child.clone(), first_thread(&child));
    let child_umount = SyscallRequest::new(NR_UMOUNT2, [target.as_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(child_umount, &child_ctx)),
        SyscallResult::Return(0)
    );
    assert!(child_namespace.mount_for(&mountpoint).is_none());
    assert_eq!(
        parent_namespace
            .mount_for(&mountpoint)
            .expect("parent mount remains")
            .key(),
        parent_mount.key()
    );
    assert_eq!(
        mount_for(&root_payload, mnt_id)
            .expect("legacy global mount remains")
            .key(),
        parent_mount.key()
    );

    let parent_umount = SyscallRequest::new(NR_UMOUNT2, [target.as_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(parent_umount, &parent_ctx)),
        SyscallResult::Return(0)
    );
    assert!(parent_namespace.mount_for(&mountpoint).is_none());
    assert!(mount_for(&root_payload, mnt_id).is_none());
}

/// `openat(.., O_CREAT | O_EXCL)` against an *existing* file
/// returns `-EEXIST` per POSIX. The `O_EXCL` lock-file primitive.
#[test]
fn dispatch_openat_o_creat_o_excl_existing_returns_neg_eexist() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let flags = (O_RDWR | O_CREAT | O_EXCL) as u64;
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            flags,
            0o644,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_EXIST));
    drop(path);
}

/// `openat(.., O_RDWR | O_TRUNC)` against an existing file with
/// non-zero size truncates the file via the in-scope
/// `FsPageBacking::truncate`. After the call the inode meta
/// reports `size == 0`.
#[test]
fn dispatch_openat_o_trunc_truncates_existing() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"big", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    // Pre-stuff the file's page-backing so its size is non-zero.
    // tmpfs's FsPageBacking::truncate doubles as a "set size" op.
    match tmpfs.truncate(file_id, 4096, &guard) {
        StepOutcome::Done(()) => {}
        other => panic!("preload truncate: {other:?}"),
    }
    drop(guard);

    // Verify the precondition.
    {
        let guard = ebr_guard();
        let meta = match tmpfs.load_inode_meta(file_id, &guard) {
            StepOutcome::Done(m) => m,
            other => panic!("load_inode_meta: {other:?}"),
        };
        assert_eq!(meta.size, 4096, "precondition: size should be 4096");
    }

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/big");
    let flags = (O_RDWR | O_TRUNC) as u64;
    let req = SyscallRequest::new(
        NR_OPENAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, flags, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    match result {
        SyscallResult::Return(_) => {}
        other => panic!("openat O_TRUNC: {other:?}"),
    }

    // Postcondition: tmpfs reports size 0 for the file.
    let guard = ebr_guard();
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.size, 0, "O_TRUNC should have truncated to 0");
    drop(path);
}

/// `openat(O_TRUNC)` must not trust the cached `RNode` size. A prior
/// path walk can cache a dentry while the tmpfs inode is still empty;
/// later writes/truncates update the backend inode size, not that cached
/// metadata snapshot.
#[test]
fn dispatch_openat_o_trunc_truncates_stale_cached_dentry() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let (file_id, _) = match tmpfs.create_inode(
        TMPFS_ROOT_OBJECT_ID,
        b"stale",
        0o100644,
        &owner_cred,
        &guard,
    ) {
        StepOutcome::Done(out) => out,
        other => panic!("create_inode: {other:?}"),
    };
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);
    let path = nul_terminate(b"/stale");

    let first_open = SyscallRequest::new(
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
    match block_on(dispatch::<ShimsTestPmap>(first_open, &ctx)) {
        SyscallResult::Return(_) => {}
        other => panic!("initial openat: {other:?}"),
    }

    let guard = ebr_guard();
    match tmpfs.truncate(file_id, 4096, &guard) {
        StepOutcome::Done(()) => {}
        other => panic!("backend truncate: {other:?}"),
    }
    drop(guard);

    let trunc_open = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDWR | O_TRUNC) as u64,
            0,
            0,
            0,
        ],
    );
    match block_on(dispatch::<ShimsTestPmap>(trunc_open, &ctx)) {
        SyscallResult::Return(_) => {}
        other => panic!("openat O_TRUNC stale dentry: {other:?}"),
    }

    let guard = ebr_guard();
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.size, 0, "O_TRUNC must clear backend size");
    drop(path);
}

/// `openat(.., O_RDONLY | O_CLOEXEC)` sets the cloexec bit on the
/// returned fd. The bit is consulted by `CloseCloexecFdsOp`
/// at exec time.
#[test]
fn dispatch_openat_o_cloexec_sets_fd_cloexec_bit() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let flags = (O_RDONLY | O_CLOEXEC) as u64;
    let req = SyscallRequest::new(
        NR_OPENAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, flags, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let fd = match result {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat O_CLOEXEC: {other:?}"),
    };
    assert!(
        proc_cap.fd_cloexec(fd as u32),
        "O_CLOEXEC should set the cloexec bit on fd {fd}"
    );
    drop(path);
}

/// `openat(AT_FDCWD, "/missing", O_RDONLY)` returns `-ENOENT` —
/// no `O_CREAT`, file doesn't exist.
#[test]
fn dispatch_openat_path_not_found_returns_neg_enoent() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/missing");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(path);
}

/// Non-`AT_FDCWD` dirfd values return `-EBADF`. The slice's fd
/// table doesn't carry directory-fd semantics yet.
#[test]
fn dispatch_openat_dirfd_not_at_fdcwd_returns_neg_ebadf() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    // dirfd = 5 (a positive fd value); not AT_FDCWD = -100.
    let req = SyscallRequest::new(
        NR_OPENAT,
        [5u64, path.as_ptr() as u64, O_RDONLY as u64, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
    drop(path);
}

/// A path with no NUL terminator within `EXECVE_PATH_MAX` returns
/// `-ENAMETOOLONG`.
#[test]
fn dispatch_openat_path_too_long_returns_neg_enametoolong() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path: Vec<u8> = vec![b'A'; EXECVE_PATH_MAX + 1];
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NAMETOOLONG));
    drop(path);
}

/// `openat(.., O_RDONLY)` against a file with no read perm in any
/// triplet returns `-EACCES`. The walker's terminal-component
/// `check_open_perm` predicate fires (DAC Wave 3).
#[test]
fn dispatch_openat_no_read_perm_returns_neg_eacces() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File owned by uid 1000, mode 0o100000 — no read bit anywhere.
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Caller uid 2000 — not the owner.
    drop_privs_to(&proc_cap, 2000);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    drop(path);
}

// ----------------------------------------------------------------
// close
// ----------------------------------------------------------------

/// `close(fd)` against an open fd returns 0 and removes the cap
/// from the fd table.
#[test]
fn dispatch_close_open_fd_returns_zero_and_clears_slot() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open then close.
    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };
    assert!(proc_cap.fd(fd).is_some());

    let close_req = SyscallRequest::new(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(close_req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        proc_cap.fd(fd).is_none(),
        "close should have cleared fd {fd}"
    );
    drop(path);
}

/// `close(fd)` against an already-closed fd returns `-EBADF`.
#[test]
fn dispatch_close_closed_fd_returns_neg_ebadf() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_CLOSE, [42u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `close(fd)` clears the cloexec bit so a future `fcntl(F_GETFD)`
/// against the same fd number after re-open reports a clean state.
#[test]
fn dispatch_close_clears_cloexec_bit() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open with O_CLOEXEC.
    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (O_RDONLY | O_CLOEXEC) as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };
    assert!(proc_cap.fd_cloexec(fd), "cloexec set after O_CLOEXEC open");

    let close_req = SyscallRequest::new(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    let _ = block_on(dispatch::<ShimsTestPmap>(close_req, &ctx));
    assert!(!proc_cap.fd_cloexec(fd), "cloexec cleared after close");
    drop(path);
}

// ----------------------------------------------------------------
// dup
// ----------------------------------------------------------------

/// `dup(oldfd)` returns the lowest unused fd; the new fd
/// references the same `OpenFile` as `oldfd` (cap clone semantics).
#[test]
fn dispatch_dup_returns_lowest_unused_fd_with_same_openfile() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open a file → fd 0.
    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };

    let dup_req = SyscallRequest::new(NR_DUP, [oldfd as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(dup_req, &ctx));
    let newfd = match result {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("dup: {other:?}"),
    };
    assert_ne!(oldfd, newfd, "dup must return a different fd");
    assert!(proc_cap.fd(oldfd).is_some(), "oldfd still open");
    assert!(proc_cap.fd(newfd).is_some(), "newfd installed");
    // POSIX: dup-derived fd has cloexec cleared.
    assert!(!proc_cap.fd_cloexec(newfd), "dup must clear cloexec");
    drop(path);
}

/// `dup(closed_fd)` returns `-EBADF`.
#[test]
fn dispatch_dup_closed_fd_returns_neg_ebadf() {
    let _setup = fd_ops_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_DUP, [99u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `dup(oldfd)` must stop at the advertised `RLIMIT_NOFILE` ceiling.
/// Musl's `t_fdfill()` depends on this returning `EMFILE`.
#[test]
fn dispatch_dup_at_rlimit_nofile_returns_neg_emfile() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };
    let file = proc_cap.fd(oldfd).expect("oldfd installed");
    for fd in 1..1024 {
        let _ = proc_cap.install_fd(fd, file.clone());
    }

    let req = SyscallRequest::new(NR_DUP, [oldfd as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_MFILE));
    assert!(proc_cap.fd(1024).is_none(), "fd 1024 must stay uninstalled");
    drop(path);
}

// ----------------------------------------------------------------
// dup3
// ----------------------------------------------------------------

/// `dup3(oldfd, newfd, 0)` against an existing `newfd` silently
/// closes the previous occupant and binds `newfd` to the same
/// `OpenFile` as `oldfd`. Atomic-replace semantic per Linux.
#[test]
fn dispatch_dup3_at_specific_fd_replaces_existing() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard);
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"b", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    // Open /a → fd_a; open /b → fd_b.
    let path_a = nul_terminate(b"/a");
    let req_a = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path_a.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd_a = match block_on(dispatch::<ShimsTestPmap>(req_a, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat /a: {other:?}"),
    };
    let path_b = nul_terminate(b"/b");
    let req_b = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path_b.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd_b = match block_on(dispatch::<ShimsTestPmap>(req_b, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat /b: {other:?}"),
    };
    assert_ne!(fd_a, fd_b);

    // dup3(fd_a, fd_b, 0) — fd_b was open against /b, now points at /a.
    let dup3_req = SyscallRequest::new(NR_DUP3, [fd_a as u64, fd_b as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(dup3_req, &ctx));
    assert_eq!(result, SyscallResult::Return(fd_b as i64));
    // fd_b is still occupied (now binds to /a's OpenFile).
    assert!(proc_cap.fd(fd_b).is_some());
    // fd_a is unchanged.
    assert!(proc_cap.fd(fd_a).is_some());
    drop(path_a);
    drop(path_b);
}

#[test]
fn dispatch_dup3_flushes_replaced_page_backed_file() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let path_a = nul_terminate(b"/a");
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path_a.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat /a: {other:?}"),
    };
    let newfd = 100;
    let (replaced, backing) = recording_page_backed_open_file(tmpfs);
    assert!(proc_cap.install_fd(newfd, replaced).is_none());

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_DUP3, [oldfd as u64, newfd as u64, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(newfd as i64));
    assert_eq!(backing.fsyncs.load(Ordering::Acquire), 1);
    drop(path_a);
}

/// `dup3(oldfd, newfd, O_CLOEXEC)` sets the cloexec bit on the
/// newfd specifically (not on oldfd).
#[test]
fn dispatch_dup3_with_o_cloexec_sets_cloexec() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };
    // dup3 to a fresh slot 100 with O_CLOEXEC.
    let dup3_req = SyscallRequest::new(NR_DUP3, [oldfd as u64, 100u64, O_CLOEXEC as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(dup3_req, &ctx));
    assert_eq!(result, SyscallResult::Return(100));
    assert!(proc_cap.fd(100).is_some());
    assert!(proc_cap.fd_cloexec(100), "dup3(O_CLOEXEC) sets cloexec");
    // oldfd's cloexec is unchanged (was clear after openat).
    assert!(!proc_cap.fd_cloexec(oldfd), "oldfd cloexec unchanged");
    drop(path);
}

/// `dup3(fd, fd, 0)` returns `-EINVAL`. Linux dup3 rejects the
/// same-fd shape that legacy dup2 would accept as a no-op.
#[test]
fn dispatch_dup3_same_fd_returns_neg_einval() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };

    let req = SyscallRequest::new(NR_DUP3, [fd as u64, fd as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    drop(path);
}

/// `dup3(oldfd, newfd, junk_flags)` with bits other than
/// `O_CLOEXEC` set returns `-EINVAL`. Linux dup3 specifically
/// rejects junk flags rather than ignoring them (open() ignores).
#[test]
fn dispatch_dup3_invalid_flags_returns_neg_einval() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let open_req = SyscallRequest::new(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };

    // A non-O_CLOEXEC junk flag (use O_RDWR's bit pattern as the
    // "unrecognised by dup3" sentinel — dup3's flag arg has only
    // O_CLOEXEC defined).
    let junk_flags: u64 = 0o100; // O_CREAT bit, not O_CLOEXEC
    let req = SyscallRequest::new(NR_DUP3, [fd as u64, 50u64, junk_flags, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    drop(path);
}

/// `dup3(oldfd, newfd, 0)` rejects target fds at or beyond the
/// process-visible fd limit.
#[test]
fn dispatch_dup3_target_at_rlimit_nofile_returns_neg_ebadf() {
    let _setup = fd_ops_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = ebr_guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let oldfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_OPENAT,
            [
                AT_FDCWD as i64 as u64,
                path.as_ptr() as u64,
                O_RDONLY as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("openat: {other:?}"),
    };

    let req = SyscallRequest::new(NR_DUP3, [oldfd as u64, 1024, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
    drop(path);
}
