// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::{
    self as step_engine, Cap, StepOutcome, guard as ebr_guard, page_allocator, reserve_for,
    sign_for,
};
use alloc::sync::Arc;
use alloc::vec;
use tx_fs::tmpfs::{TMPFS_ROOT_OBJECT_ID, Tmpfs};
use tx_subsystems::cred::{CapabilitySet, Uid, step_setresuid};
use tx_subsystems::cross_crate_test_support::clear_caps_for_test;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::process::step_chdir;
use tx_subsystems::vfs::FsOps;
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
};

use crate::linux_syscall::{
    AT_FDCWD, EXECVE_PATH_MAX, NR_CLOSE, NR_DUP, NR_DUP3, NR_OPENAT, O_CLOEXEC, O_CREAT, O_EXCL,
    O_RDONLY, O_RDWR, O_TRUNC,
};

/// errno magnitudes: positive Linux RV64 generic ABI values.
const E_BADF: i32 = 9;
const E_NOENT: i32 = 2;
const E_EXIST: i32 = 17;
const E_INVAL: i32 = 22;
const E_ACCES: i32 = 13;
const E_NAMETOOLONG: i32 = 36;

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
fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
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

    let _mount = MountIdentity::new_cap(
        MountId::new(21),
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

// ----------------------------------------------------------------
// openat
// ----------------------------------------------------------------

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

/// `openat(.., O_RDONLY | O_CLOEXEC)` sets the cloexec bit on the
/// returned fd. The bit is consulted by `step_close_cloexec_fds`
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
