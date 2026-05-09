// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use alloc::sync::Arc;
use alloc::vec;

use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_substrate::{page_allocator, zone};
use tx_subsystems::cred::CapabilitySet;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::pipe::{step_pipe2, PipeFlags};
use tx_subsystems::process::step_chdir;
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
};
use tx_subsystems::vfs::FsOps;

use crate::linux_syscall::{
    AT_FDCWD, AT_REMOVEDIR, NR_FTRUNCATE, NR_LINKAT, NR_MKDIRAT, NR_READLINKAT, NR_RENAMEAT2,
    NR_SYMLINKAT, NR_TRUNCATE, NR_UNLINKAT, NR_UTIMENSAT, RENAME_EXCHANGE, RENAME_NOREPLACE,
};

/// errno magnitudes (positive Linux RV64 generic ABI values).
const E_NOENT: i32 = 2;
const E_BADF: i32 = 9;
const E_EXIST: i32 = 17;
const E_NOTDIR: i32 = 20;
const E_ISDIR: i32 = 21;
const E_INVAL: i32 = 22;
const E_PERM: i32 = 1;
const E_NOSYS: i32 = 38;

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
        let res = zone::reserve_for::<RNode>().expect("rnode reservation");
        zone::sign_for(res, raw)
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

fn root_cred() -> Credential {
    Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    }
}

fn create_regular(tmpfs: &Arc<Tmpfs>, name: &[u8]) {
    use tx_substrate::step_v3::StepOutcome;
    let cred = root_cred();
    let guard = tx_substrate::epoch::guard();
    match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, name, 0o100644, &cred, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("create_inode {:?}: {other:?}", name),
    }
}

fn make_dir(tmpfs: &Arc<Tmpfs>, name: &[u8]) {
    use tx_substrate::step_v3::StepOutcome;
    let cred = root_cred();
    let guard = tx_substrate::epoch::guard();
    match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, name, 0o755, &cred, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("mkdir {:?}: {other:?}", name),
    }
}

fn make_symlink(tmpfs: &Arc<Tmpfs>, name: &[u8], target: &[u8]) {
    use tx_substrate::step_v3::StepOutcome;
    let cred = root_cred();
    let guard = tx_substrate::epoch::guard();
    match tmpfs.symlink(TMPFS_ROOT_OBJECT_ID, name, target, &cred, &guard) {
        StepOutcome::Done(_) => {}
        other => panic!("symlink {:?}: {other:?}", name),
    }
}

fn lookup_exists(tmpfs: &Arc<Tmpfs>, name: &[u8]) -> bool {
    use tx_substrate::step_v3::StepOutcome;
    let guard = tx_substrate::epoch::guard();
    matches!(
        tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, name, &guard),
        StepOutcome::Done(_)
    )
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

/// `mkdirat` with a non-cwd dirfd surfaces as `-EBADF`.
#[test]
fn dispatch_mkdirat_non_cwd_dirfd_returns_neg_ebadf() {
    let _setup = fm_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/d");
    let req = SyscallRequest::new(NR_MKDIRAT, [3, path.as_ptr() as u64, 0o755, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
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

/// `linkat` surfaces the tmpfs `FsOps::link` `-ENOSYS` carryover
/// (Phase 3b — hard links unimplemented in tmpfs day-1).
#[test]
fn dispatch_linkat_returns_tmpfs_enosys() {
    let _setup = fm_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"src");
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let oldpath = nul_terminate(b"/src");
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
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
    drop(oldpath);
    drop(newpath);
}

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

/// `renameat2(.., RENAME_NOREPLACE)` against an existing destination
/// returns `-EEXIST`.
#[test]
fn dispatch_renameat2_noreplace_existing_returns_neg_eexist() {
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
            RENAME_NOREPLACE as u64,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_EXIST));
    // Both still exist.
    assert!(lookup_exists(&tmpfs, b"a"));
    assert!(lookup_exists(&tmpfs, b"b"));
    drop(oldpath);
    drop(newpath);
}

/// `renameat2(.., RENAME_EXCHANGE)` returns `-ENOSYS` (atomic swap
/// unsupported in Slice 8).
#[test]
fn dispatch_renameat2_exchange_returns_neg_enosys() {
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
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
    drop(oldpath);
    drop(newpath);
}

// -----------------------------------------------------------------
// utimensat
// -----------------------------------------------------------------

/// `utimensat` returns `-ENOSYS` (Slice 8 carryover — no
/// `FsOps::set_times` hook yet).
#[test]
fn dispatch_utimensat_returns_neg_enosys() {
    let _setup = fm_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_UTIMENSAT, [AT_FDCWD as i64 as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
}
