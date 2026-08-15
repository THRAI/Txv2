// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::{
    self as step_engine, guard, page_allocator, reserve_for, sign_for, Cap, StepOutcome,
};
use crate::linux_syscall::clear_stat_meta_overrides;
use alloc::sync::Arc;
use alloc::vec;
use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_subsystems::cred::CapabilitySet;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions, MountPayload,
    SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::pipe::{step_pipe2, PipeFlags};
use tx_subsystems::process::{step_chdir_with_mount, step_set_mount_namespace};
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
    S_IFDIR, S_IFLNK,
};
use tx_subsystems::vfs::{FsOps, OpenFile};
use tx_subsystems::vm::{
    MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapRequest,
    USER_PAGE_SIZE,
};

use crate::linux_syscall::{
    AT_EMPTY_PATH, AT_FDCWD, NR_CHDIR, NR_FCHDIR, NR_FCHMODAT, NR_FSTAT, NR_FSTATFS, NR_GETCWD,
    NR_GETDENTS64, NR_LSEEK, NR_NEWFSTATAT, NR_STATFS, NR_STATX, NR_UMASK, SEEK_SET,
};

/// errno magnitudes the tests check against (positive Linux RV64
/// generic ABI values; the dispatcher returns the positive
/// magnitude in `SyscallResult::Error`).
const E_BADF: i32 = 9;
const E_NOENT: i32 = 2;
const E_NOTDIR: i32 = 20;
const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_NOSYS: i32 = 38;
const E_RANGE: i32 = 34;

/// Stat field offsets (verified against Linux's `asm-generic/stat.h`
/// and the `StatLayout` struct in `mod.rs`). Tests read directly out
/// of the kernel-side stat buffer using these offsets.
const STAT_INO_OFF: usize = 8;
const STAT_MODE_OFF: usize = 16;
const STAT_NLINK_OFF: usize = 20;
const STAT_UID_OFF: usize = 24;
const STAT_GID_OFF: usize = 28;
const STAT_SIZE_OFF: usize = 48;
const STAT_BLKSIZE_OFF: usize = 56;
/// Total `struct stat` byte size on RV64 generic ABI: matches
/// `size_of::<StatLayout>` per the field layout in `mod.rs`.
const STAT_BYTES: usize = 128;
const STATX_MASK_OFF: usize = 0;
const STATX_BLKSIZE_OFF: usize = 4;
const STATX_NLINK_OFF: usize = 16;
const STATX_MODE_OFF: usize = 28;
const STATX_INO_OFF: usize = 32;
const STATX_SIZE_OFF: usize = 40;
const STATX_BYTES: usize = 256;
const STATFS_BYTES: usize = 120;
const STATFS_TYPE_OFF: usize = 0;
const STATFS_BSIZE_OFF: usize = 8;
const STATFS_NAMELEN_OFF: usize = 64;
const STATFS_FRSIZE_OFF: usize = 72;

/// `linux_dirent64` fixed header byte size (8 + 8 + 2 + 1 = 19).
const DIRENT_HEADER_BYTES: usize = 19;

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for stat_family tests: {error:?}"),
    }
}

fn stat_setup() -> TestSetup {
    clear_stat_meta_overrides();
    let setup = setup();
    ensure_zero_frame_claimed();
    setup
}

/// Build a fresh tmpfs-backed mount + a root `Cap<DEntry>`. Returns
/// the dentry, the FsOps `Arc` (so callers can mint files
/// directly), and the root `Cap<RNode>` (so callers can build a
/// directory OpenFile for getdents64 tests).
fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>, Cap<RNode>, Cap<MountIdentity>) {
    let tmpfs = Arc::new(Tmpfs::new());
    let payload = MountPayload::new_cap(
        tmpfs.clone() as Arc<dyn tx_subsystems::vfs::FsOps>,
        tmpfs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBacking>,
        None,
        DevId::new(311),
        MountOptions::default(),
        "tmpfs-stat-family",
        SourceLabel::Static("tmpfs-stat-family"),
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

    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode.clone()).expect("root dentry");
    let root_mount = MountIdentity::new_cap_with_root_dentry(
        MountId::new(31),
        None,
        root_dentry.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");
    (root_dentry, tmpfs, root_rnode, root_mount)
}

/// Bootstrap an init process whose cwd is `root_dentry`. Returns
/// the `(process, leader-thread)` pair.
fn bootstrap_with_cwd(
    root_dentry: Cap<DEntry>,
    root_mount: Cap<MountIdentity>,
) -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let aspace = fresh_aspace();
    let process = bootstrap_init_process(aspace).expect("bootstrap init");
    let thread = process.nth_thread(0).expect("leader thread");
    let namespace = MountNamespace::new_cap(root_mount.clone()).expect("mount namespace");
    step_set_mount_namespace(&process, namespace).expect("install mount namespace");
    match step_chdir_with_mount(&process, root_dentry, root_mount) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
            panic!("init bootstrap zombified")
        }
    }
    (process, thread)
}

/// NUL-terminate a path slice into a kernel-side `Vec<u8>` so the
/// inline `read_user_cstr` reads it correctly.
fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn map_resident_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) -> u64 {
    let range = UserRange::new_aligned(UserVirtAddr(uaddr), USER_PAGE_SIZE).expect("user range");
    ctx.aspace
        .try_mmap(VmMapRequest::fixed(
            range,
            MapPlacement::FixedReplace,
            Prot::READ_WRITE,
            VmEntryFlags::PRIVATE,
            VmBacking::PrivateAnon,
        ))
        .expect("map statx user page");
    let guard = guard();
    assert_eq!(
        ctx.aspace
            .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard),
        StepOutcome::Done(bytes.len())
    );
    uaddr as u64
}

/// Build a directory OpenFile rooted at `root_rnode`. Used by the
/// `getdents64` tests so the per-fd readdir cursor can be exercised
/// independently of the walker.
fn directory_open_file(root_rnode: Cap<RNode>) -> Cap<OpenFile> {
    OpenFile::new_cap(
        root_rnode,
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("directory open file cap")
}

fn read_u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn read_u64_at(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
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

fn read_u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

// -----------------------------------------------------------------
// fstat
// -----------------------------------------------------------------

/// `fstat(fd_for_pagebacked_file, statbuf)` writes the
/// `(mode, size, ino)` triple from the inode meta into the user
/// buffer and returns `0`.
#[test]
fn dispatch_fstat_on_pagebacked_fd_writes_stat_struct() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let (file_id, _meta) = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(pair) => pair,
            other => panic!("create_inode: {other:?}"),
        }
    };
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);

    // Open the file via the openat path so the resulting OpenFile
    // is a real PageBacked rnode whose meta carries the 0o100644
    // mode + 0 size from create_inode.
    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        crate::linux_syscall::NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            crate::linux_syscall::O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = match block_on(dispatch::<ShimsTestPmap>(req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat /f: {other:?}"),
    };
    drop(path);

    // Now call fstat(fd, &statbuf).
    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(
        NR_FSTAT,
        [fd as u64, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u64_at(&statbuf, STAT_INO_OFF), file_id.as_u64());
    assert_eq!(read_u32_at(&statbuf, STAT_MODE_OFF), 0o100644);
    assert_eq!(read_u32_at(&statbuf, STAT_UID_OFF), 0);
    assert_eq!(read_u32_at(&statbuf, STAT_GID_OFF), 0);
    // `st_size` is `i64`; freshly-created file has size 0.
    assert_eq!(read_u64_at(&statbuf, STAT_SIZE_OFF), 0);
    assert_eq!(read_u32_at(&statbuf, STAT_BLKSIZE_OFF), 4096);
    assert_eq!(read_u32_at(&statbuf, STAT_NLINK_OFF), 1);
}

/// `fstat(stdout_fd, statbuf)` against a TTY-backed fd writes a
/// stat layout whose mode carries the `S_IFCHR` bits (the boot
/// console's RNode is constructed with kind = CharDevice).
#[test]
fn dispatch_fstat_on_tty_fd_writes_stat_struct() {
    let _setup = stat_setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(1, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(NR_FSTAT, [1, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    // S_IFCHR = 0o020000 in the upper nibble.
    let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
    assert_eq!(mode & 0o170000, 0o020000, "expected S_IFCHR; got {mode:#o}");
}

/// `fstat(unknown_fd, statbuf)` returns `-EBADF`.
#[test]
fn dispatch_fstat_unknown_fd_returns_neg_ebadf() {
    let _setup = stat_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(NR_FSTAT, [42, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `fstat(0, NULL)` returns `-EFAULT`. The arm rejects null
/// statbuf before resolving the fd.
#[test]
fn dispatch_fstat_null_buffer_returns_neg_efault() {
    let _setup = stat_setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_FSTAT, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

// -----------------------------------------------------------------
// newfstatat
// -----------------------------------------------------------------

/// `newfstatat(AT_FDCWD, "/f", &statbuf, 0)` walks the path from
/// the cwd and stats the resulting rnode, returning `0` plus the
/// expected mode bits.
#[test]
fn dispatch_newfstatat_with_valid_path_returns_zero() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100640, &owner_cred, &guard);
    }
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u32_at(&statbuf, STAT_MODE_OFF), 0o100640);
    drop(path);
}

/// `newfstatat(..., AT_SYMLINK_NOFOLLOW)` stats the final symlink
/// itself rather than the regular-file target.
#[test]
fn dispatch_newfstatat_at_symlink_nofollow_stats_link() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        match tmpfs.create_inode(
            TMPFS_ROOT_OBJECT_ID,
            b"target",
            0o100640,
            &owner_cred,
            &guard,
        ) {
            StepOutcome::Done(_) => {}
            other => panic!("create_inode target: {other:?}"),
        }
    }
    let link_id = {
        let guard = guard();
        match tmpfs.symlink(
            TMPFS_ROOT_OBJECT_ID,
            b"link",
            b"target",
            &owner_cred,
            &guard,
        ) {
            StepOutcome::Done((id, _meta)) => id,
            other => panic!("symlink link -> target: {other:?}"),
        }
    };
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/link");
    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            crate::linux_syscall::AT_SYMLINK_NOFOLLOW as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
    assert_eq!(
        mode as u16 & 0o170000,
        S_IFLNK,
        "expected S_IFLNK; got {mode:#o}"
    );
    assert_eq!(read_u64_at(&statbuf, STAT_INO_OFF), link_id.as_u64());
    drop(path);
}

/// `newfstatat(AT_FDCWD, "/missing", &statbuf, 0)` surfaces the
/// walker's `Errno::ENOENT` as `-ENOENT`.
#[test]
fn dispatch_newfstatat_with_nonexistent_path_returns_neg_enoent() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/missing");
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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(path);
}

/// `newfstatat(AT_FDCWD, "", &statbuf, AT_EMPTY_PATH)` stats the
/// cwd directly. The mode carries the directory `S_IFDIR` bits
/// from the tmpfs root inode.
#[test]
fn dispatch_newfstatat_at_empty_path_stats_cwd() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"");
    let mut statbuf = vec![0u8; STAT_BYTES];
    let req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            AT_EMPTY_PATH as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let mode = read_u32_at(&statbuf, STAT_MODE_OFF);
    assert_eq!(mode & 0o170000, 0o040000, "expected S_IFDIR; got {mode:#o}");
    drop(path);
}

/// `newfstatat(fd, "", &statbuf, AT_EMPTY_PATH)` mirrors
/// `fstat(fd)`. LA64 musl uses this shape for its public
/// `fstat(2)` wrapper.
#[test]
fn dispatch_newfstatat_at_empty_path_stats_fd() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let (file_id, _meta) = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(pair) => pair,
            other => panic!("create_inode: {other:?}"),
        }
    };
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);

    let path = nul_terminate(b"/f");
    let ctx = make_ctx(proc_cap.clone(), thread);
    let open_req = SyscallRequest::new(
        crate::linux_syscall::NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            crate::linux_syscall::O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat /f: {other:?}"),
    };
    drop(path);

    let empty = nul_terminate(b"");
    let mut statbuf = vec![0u8; STAT_BYTES];
    let stat_req = SyscallRequest::new(
        NR_NEWFSTATAT,
        [
            fd as u64,
            empty.as_ptr() as u64,
            statbuf.as_mut_ptr() as u64,
            AT_EMPTY_PATH as u64,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(stat_req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u64_at(&statbuf, STAT_INO_OFF), file_id.as_u64());
    assert_eq!(read_u32_at(&statbuf, STAT_MODE_OFF), 0o100644);
    drop(empty);
}

/// `statx(AT_FDCWD, "/", 0, STATX_BASIC_STATS, statxbuf)` follows
/// the same cwd-relative walker path as `newfstatat` and writes the
/// Linux `struct statx` byte image used by LA64 busybox `ls`.
#[test]
fn dispatch_statx_on_root_writes_statx_struct() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    // Mounted-node lookup is reachable from cached stat-family operations
    // while their walker guard is still active. It must borrow that guard
    // instead of trying to enter the non-nestable EBR domain again.
    let outer_guard = guard();
    assert!(
        crate::linux_syscall::fs_mounted::MountedNode::from_rnode_direct(&root_rnode).is_some()
    );
    drop(outer_guard);

    let path = nul_terminate(b"/");
    let mut statxbuf = vec![0u8; STATX_BYTES];
    let req = SyscallRequest::new(
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
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        read_u32_at(&statxbuf, STATX_MASK_OFF),
        crate::linux_syscall::numbers::STATX_BASIC_STATS
    );
    assert_eq!(read_u32_at(&statxbuf, STATX_BLKSIZE_OFF), 4096);
    assert_eq!(read_u32_at(&statxbuf, STATX_NLINK_OFF), 2);
    let mode = read_u16_at(&statxbuf, STATX_MODE_OFF);
    assert_eq!(mode & 0o170000, 0o040000, "expected S_IFDIR; got {mode:#o}");
    assert_eq!(
        read_u64_at(&statxbuf, STATX_INO_OFF),
        root_rnode.fs_object_id().as_u64()
    );
    assert_eq!(read_u64_at(&statxbuf, STATX_SIZE_OFF), 0);
    drop(path);
}

#[test]
fn direct_statx_reads_resident_cached_fd_without_reactor_handoff() {
    let _setup = stat_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (reader, _writer) = step_pipe2(PipeFlags::default()).expect("pipe for direct statx");
    let expected_ino = reader.rnode().fs_object_id().as_u64();
    proc_cap.set_fd(11, Some(reader));
    let ctx = make_ctx(proc_cap, thread);

    let path_uaddr = map_resident_user_bytes(&ctx, 0x63a0_0000, b"\0");
    // Materialise the anonymous destination page before entering the
    // cache-only trap lane; a cold user page must deliberately fall back.
    let statx_uaddr = map_resident_user_bytes(&ctx, 0x63a0_1000, &[0]);
    let request = SyscallRequest::new(
        NR_STATX,
        [
            11,
            path_uaddr,
            AT_EMPTY_PATH as u64,
            crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
            statx_uaddr,
            0,
        ],
    );
    assert_eq!(
        dispatch_direct_trap_oneshot::<ShimsTestPmap>(
            &request,
            &ctx.process,
            &ctx.thread,
            &ctx.aspace,
        ),
        Some(SyscallResult::Return(0))
    );

    let mut statxbuf = vec![0u8; STATX_BYTES];
    let guard = guard();
    assert_eq!(
        ctx.aspace.copy_from_user(
            &mut statxbuf,
            tx_hal::UserPtr::<u8>::new(statx_uaddr as usize),
            &guard,
        ),
        StepOutcome::Done(STATX_BYTES)
    );
    assert_eq!(read_u64_at(&statxbuf, STATX_INO_OFF), expected_ino);
    assert_eq!(read_u16_at(&statxbuf, STATX_MODE_OFF) & 0o170000, 0o010000);
}

/// `statx(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, statxbuf)`
/// mirrors `fstat(fd)`. LA64 musl may use this fd form for its public
/// `fstat(2)` wrapper.
#[test]
fn dispatch_statx_at_empty_path_stats_fd() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let (file_id, _meta) = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(pair) => pair,
            other => panic!("create_inode: {other:?}"),
        }
    };
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);

    let path = nul_terminate(b"/f");
    let ctx = make_ctx(proc_cap.clone(), thread);
    let open_req = SyscallRequest::new(
        crate::linux_syscall::NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            crate::linux_syscall::O_RDONLY as u64,
            0,
            0,
            0,
        ],
    );
    let fd = match block_on(dispatch::<ShimsTestPmap>(open_req, &ctx)) {
        SyscallResult::Return(fd) => fd,
        other => panic!("openat /f: {other:?}"),
    };
    drop(path);

    let empty = nul_terminate(b"");
    let mut statxbuf = vec![0u8; STATX_BYTES];
    let statx_req = SyscallRequest::new(
        NR_STATX,
        [
            fd as u64,
            empty.as_ptr() as u64,
            AT_EMPTY_PATH as u64,
            crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
            statxbuf.as_mut_ptr() as u64,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(statx_req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u64_at(&statxbuf, STATX_INO_OFF), file_id.as_u64());
    assert_eq!(read_u16_at(&statxbuf, STATX_MODE_OFF), 0o100644);
    drop(empty);
}

/// `statx(..., AT_SYMLINK_NOFOLLOW, ...)` reports the terminal
/// symlink's metadata rather than the target's metadata.
#[test]
fn dispatch_statx_at_symlink_nofollow_stats_link() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        match tmpfs.create_inode(
            TMPFS_ROOT_OBJECT_ID,
            b"target",
            0o100600,
            &owner_cred,
            &guard,
        ) {
            StepOutcome::Done(_) => {}
            other => panic!("create_inode target: {other:?}"),
        }
    }
    let link_id = {
        let guard = guard();
        match tmpfs.symlink(
            TMPFS_ROOT_OBJECT_ID,
            b"link",
            b"target",
            &owner_cred,
            &guard,
        ) {
            StepOutcome::Done((id, _meta)) => id,
            other => panic!("symlink link -> target: {other:?}"),
        }
    };
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/link");
    let mut statxbuf = vec![0u8; STATX_BYTES];
    let req = SyscallRequest::new(
        NR_STATX,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            crate::linux_syscall::AT_SYMLINK_NOFOLLOW as u64,
            crate::linux_syscall::numbers::STATX_BASIC_STATS as u64,
            statxbuf.as_mut_ptr() as u64,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let mode = read_u16_at(&statxbuf, STATX_MODE_OFF);
    assert_eq!(mode & 0o170000, S_IFLNK, "expected S_IFLNK; got {mode:#o}");
    assert_eq!(read_u64_at(&statxbuf, STATX_INO_OFF), link_id.as_u64());
    drop(path);
}

#[test]
fn dispatch_statx_after_fchmodat_observes_live_mode() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(_) => {}
            other => panic!("create_inode: {other:?}"),
        }
    }

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let path = nul_terminate(b"/f");

    let chmod_req = SyscallRequest::new(
        NR_FCHMODAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o600, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(chmod_req, &ctx)),
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
    assert_eq!(read_u16_at(&statxbuf, STATX_MODE_OFF), 0o100600);
    drop(path);
}

#[test]
fn dispatch_fchmodat_absolute_path_ignores_non_cwd_dirfd() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(_) => {}
            other => panic!("create_inode: {other:?}"),
        }
    }

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);
    let path = nul_terminate(b"/f");
    let chmod_req = SyscallRequest::new(NR_FCHMODAT, [3, path.as_ptr() as u64, 0o600, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(chmod_req, &ctx)),
        SyscallResult::Return(0)
    );
    drop(path);
}

// -----------------------------------------------------------------
// chdir / fchdir
// -----------------------------------------------------------------

/// `chdir("/")` against the bootstrap-with-cwd init succeeds and
/// returns 0; the cwd dentry is replaced by the resolved root.
#[test]
fn dispatch_chdir_to_existing_dir_succeeds() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/");
    let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(proc_cap.cwd().is_some(), "cwd should remain installed");
    drop(path);
}

/// `chdir("/missing")` returns `-ENOENT`.
#[test]
fn dispatch_chdir_to_nonexistent_path_returns_neg_enoent() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/missing");
    let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOENT));
    drop(path);
}

/// `chdir("/f")` against an existing regular file returns
/// `-ENOTDIR` (cwd must be a directory).
#[test]
fn dispatch_chdir_to_regular_file_returns_neg_enotdir() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    }
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(NR_CHDIR, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTDIR));
    drop(path);
}

#[test]
fn dispatch_fchdir_to_mounted_directory_fd_installs_complete_cwd_binding() {
    let _setup = stat_setup();
    let (root_dentry, tmpfs, _root_rnode, root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    {
        let guard = guard();
        match tmpfs.mkdir(
            TMPFS_ROOT_OBJECT_ID,
            b"dir",
            S_IFDIR | 0o755,
            &owner_cred,
            &guard,
        ) {
            StepOutcome::Done(_) => {}
            other => panic!("mkdir dir: {other:?}"),
        }
    }

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry.clone(), root_mount.clone());
    let namespace = proc_cap
        .mount_namespace_cap()
        .expect("process mount namespace");
    let resolved = match tx_subsystems::vfs::walker::step_walk_in_mount_namespace_with_origin_mount(
        root_dentry,
        &root_mount,
        b"dir",
        &owner_cred,
        &namespace,
        &guard(),
    ) {
        StepOutcome::Done(resolved) => resolved,
        other => panic!("resolve mounted directory: {other:?}"),
    };
    let directory = OpenFile::new_cap_with_dentry(
        resolved.dentry.rnode().clone(),
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
        resolved.dentry.clone(),
    )
    .expect("directory open file");
    assert!(proc_cap.install_fd(3, directory).is_none());

    let ctx = make_ctx(proc_cap.clone(), thread);
    let req = SyscallRequest::new(NR_FCHDIR, [3, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    let cwd = proc_cap.cwd_binding().expect("mounted cwd binding");
    assert_eq!(cwd.dentry.key(), resolved.dentry.key());
    assert_eq!(cwd.mount.key(), resolved.mount.key());
}

// -----------------------------------------------------------------
// statfs / fstatfs
// -----------------------------------------------------------------

/// `statfs(path, buf)` writes the generic LP64 Linux `struct statfs`
/// layout. The `f_namelen` and `f_frsize` offsets are especially
/// important: musl's `struct statfs` places them at bytes 64 and 72,
/// before `f_flags` and `f_spare`.
#[test]
fn dispatch_statfs_writes_musl_lp64_statfs_layout() {
    let _setup = stat_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/");
    let mut statfs = vec![0u8; STATFS_BYTES];
    let req = SyscallRequest::new(
        NR_STATFS,
        [path.as_ptr() as u64, statfs.as_mut_ptr() as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u64_at(&statfs, STATFS_TYPE_OFF), 0x0102_1994);
    assert_eq!(read_u64_at(&statfs, STATFS_BSIZE_OFF), 4096);
    assert_eq!(read_u64_at(&statfs, STATFS_NAMELEN_OFF), 255);
    assert_eq!(read_u64_at(&statfs, STATFS_FRSIZE_OFF), 4096);
}

/// `fstatfs(fd, buf)` uses the same byte layout as `statfs`.
#[test]
fn dispatch_fstatfs_writes_musl_lp64_statfs_layout() {
    let _setup = stat_setup();
    let (_root_dentry, _tmpfs, root_rnode, _root_mount) = build_tmpfs_root();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(8, Some(directory_open_file(root_rnode)));
    let ctx = make_ctx(proc_cap, thread);

    let mut statfs = vec![0u8; STATFS_BYTES];
    let req = SyscallRequest::new(NR_FSTATFS, [8, statfs.as_mut_ptr() as u64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(read_u64_at(&statfs, STATFS_NAMELEN_OFF), 255);
    assert_eq!(read_u64_at(&statfs, STATFS_FRSIZE_OFF), 4096);
}

// -----------------------------------------------------------------
// getcwd
// -----------------------------------------------------------------

/// `getcwd(&buf, sizeof buf)` after `step_chdir` returns the
/// rendered path bytes plus the byte count (incl. terminator).
#[test]
fn dispatch_getcwd_after_chdir_returns_path() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 64];
    let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 64, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    // Root path renders as `/` (1 byte) + NUL = 2.
    assert_eq!(result, SyscallResult::Return(2));
    assert_eq!(&buf[..2], b"/\0");
}

/// `getcwd(buf, 0)` (with non-NULL buf) returns `-EINVAL` per
/// Linux's syscall-side semantics (libc handles the
/// allocate-on-zero shape, not the kernel).
#[test]
fn dispatch_getcwd_zero_size_returns_neg_einval() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 8];
    let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `getcwd(buf, 1)` cannot fit `"/" + NUL` (2 bytes); returns
/// `-ERANGE`.
#[test]
fn dispatch_getcwd_too_small_buffer_returns_neg_erange() {
    let _setup = stat_setup();
    let (root_dentry, _tmpfs, _root_rnode, _root_mount) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry, _root_mount);
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = [0u8; 1];
    let req = SyscallRequest::new(NR_GETCWD, [buf.as_mut_ptr() as u64, 1, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_RANGE));
}

// -----------------------------------------------------------------
// getdents64
// -----------------------------------------------------------------

/// `getdents64(dir_fd, buf, buf_len)` against a tmpfs root with
/// two files writes two `linux_dirent64` records and returns the
/// total byte count. Each record's header carries the inode id +
/// the `DT_REG` `d_type` byte.
#[test]
fn dispatch_getdents64_on_directory_fd_writes_entries() {
    let _setup = stat_setup();
    let (_root_dentry, tmpfs, root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let id_a = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done((id, _)) => id,
            other => panic!("create_inode a: {other:?}"),
        }
    };
    let id_b = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"bb", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done((id, _)) => id,
            other => panic!("create_inode bb: {other:?}"),
        }
    };

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(7, Some(directory_open_file(root_rnode)));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let mut buf = vec![0u8; 256];
    let req = SyscallRequest::new(
        NR_GETDENTS64,
        [7, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let total = match result {
        SyscallResult::Return(n) => n as usize,
        other => panic!("getdents64: {other:?}"),
    };
    assert!(total >= 2 * (DIRENT_HEADER_BYTES + 1 + 1) /* min */);

    // Walk the records and verify both inode ids appear with
    // DT_REG.
    let mut seen_a = false;
    let mut seen_b = false;
    let mut off = 0usize;
    while off < total {
        let d_ino = read_u64_at(&buf, off);
        let d_reclen = read_u16_at(&buf, off + 16) as usize;
        let d_type = buf[off + 18];
        assert!(d_reclen > DIRENT_HEADER_BYTES);
        assert!(off + d_reclen <= total);
        assert_eq!(d_type, crate::linux_syscall::DT_REG);
        if d_ino == id_a.as_u64() {
            seen_a = true;
        }
        if d_ino == id_b.as_u64() {
            seen_b = true;
        }
        off += d_reclen;
    }
    assert!(seen_a && seen_b, "both entries should appear");

    // A second call after EOD returns 0.
    let req2 = SyscallRequest::new(
        NR_GETDENTS64,
        [7, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
    );
    let result2 = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
    assert_eq!(result2, SyscallResult::Return(0));
}

/// Directory fds are seekable as directory streams: `rewinddir()` in libc
/// is an `lseek(fd, 0, SEEK_SET)` underneath. LTP cleanup relies on this
/// when recursively removing large temporary directories.
#[test]
fn dispatch_lseek_directory_fd_resets_getdents64_cursor() {
    let _setup = stat_setup();
    let (_root_dentry, tmpfs, root_rnode, _root_mount) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let id_a = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"a", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done((id, _)) => id,
            other => panic!("create_inode a: {other:?}"),
        }
    };
    let id_b = {
        let guard = guard();
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"b", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done((id, _)) => id,
            other => panic!("create_inode b: {other:?}"),
        }
    };

    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(7, Some(directory_open_file(root_rnode)));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let mut first = vec![0u8; 24];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETDENTS64,
            [7, first.as_mut_ptr() as u64, first.len() as u64, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(24));

    let rewind = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_LSEEK, [7, 0, SEEK_SET as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(rewind, SyscallResult::Return(0));

    let mut all = vec![0u8; 128];
    let total = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETDENTS64,
            [7, all.as_mut_ptr() as u64, all.len() as u64, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(n) => n as usize,
        other => panic!("getdents64 after rewind: {other:?}"),
    };

    let mut seen = alloc::collections::BTreeSet::new();
    let mut off = 0usize;
    while off < total {
        let ino = read_u64_at(&all, off);
        let reclen = read_u16_at(&all, off + 16) as usize;
        seen.insert(ino);
        off += reclen;
    }
    assert!(seen.contains(&id_a.as_u64()));
    assert!(seen.contains(&id_b.as_u64()));
}

/// `getdents64(pipe_fd, buf, buf_len)` returns `-ENOTDIR`. Pipes
/// are not directory backings.
#[test]
fn dispatch_getdents64_on_pipe_fd_returns_neg_enotdir() {
    let _setup = stat_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (reader_cap, _writer_cap) =
        step_pipe2(PipeFlags::default()).expect("pipe2 for getdents64-enotdir test");
    proc_cap.set_fd(11, Some(reader_cap));
    let ctx = make_ctx(proc_cap, thread);

    let mut buf = vec![0u8; 256];
    let req = SyscallRequest::new(
        NR_GETDENTS64,
        [11, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOTDIR));
}

/// After the first `getdents64` consumed every record, a second
/// call returns `0` (end of directory).
#[test]
fn dispatch_getdents64_after_full_read_returns_zero() {
    let _setup = stat_setup();
    let (_root_dentry, _tmpfs, root_rnode, _root_mount) = build_tmpfs_root();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(8, Some(directory_open_file(root_rnode)));
    let ctx = make_ctx(proc_cap, thread);

    // First call against an empty directory returns 0
    // immediately — there are no entries to encode.
    let mut buf = vec![0u8; 256];
    let req = SyscallRequest::new(
        NR_GETDENTS64,
        [8, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

// -----------------------------------------------------------------
// umask
// -----------------------------------------------------------------

/// `umask(0o077)` swaps the per-process file-creation mask and
/// returns the previous default `0o022`.
#[test]
fn dispatch_umask_swaps_and_returns_old_value() {
    let _setup = stat_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_UMASK, [0o077, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0o022));
    assert_eq!(proc_cap.umask(), 0o077);

    // Second call returns the value installed by the first.
    let req2 = SyscallRequest::new(NR_UMASK, [0o000, 0, 0, 0, 0, 0]);
    let result2 = block_on(dispatch::<ShimsTestPmap>(req2, &ctx));
    assert_eq!(result2, SyscallResult::Return(0o077));
    assert_eq!(proc_cap.umask(), 0o000);
}
