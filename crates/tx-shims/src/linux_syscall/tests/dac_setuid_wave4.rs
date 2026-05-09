// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use alloc::sync::Arc;
use alloc::vec;

use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_substrate::{page_allocator, zone};
use tx_subsystems::cred::{step_setresuid, Capability, CapabilitySet, Uid};
use tx_subsystems::cross_crate_test_support::{
    clear_caps_for_test, install_caps_for_test, set_cred_ids_for_test,
};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::process::step_chdir;
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_IFDIR,
};
use tx_subsystems::vfs::FsOps;

use crate::linux_syscall::{
    AT_EACCESS, AT_FDCWD, EXECVE_PATH_MAX, F_OK, NR_FACCESSAT, NR_FACCESSAT2, NR_FCHMODAT,
    NR_FCHOWNAT, R_OK, W_OK, X_OK,
};

/// errno magnitudes the tests check against (positive Linux RV64
/// generic ABI values; the dispatcher returns the positive
/// magnitude in `SyscallResult::Error`).
const E_PERM: i32 = 1;
const E_BADF: i32 = 9;
const E_ACCES: i32 = 13;
const E_ROFS: i32 = 30;
const E_NAMETOOLONG: i32 = 36;

/// `(u32) -1` — Linux's "leave unchanged" sentinel for
/// `fchownat`'s `uid` / `gid` args. Same convention as the
/// `setre{u,g}id` family; reused here for symmetry.
const NEG_ONE_U32: u64 = u32::MAX as u64;

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for wave4 tests: {error:?}"),
    }
}

fn wave4_setup() -> TestSetup {
    let setup = setup();
    ensure_zero_frame_claimed();
    setup
}

/// Build a fresh tmpfs-backed mount and a root `Cap<DEntry>`
/// pointing at the tmpfs root inode. Returns the dentry plus the
/// `Arc<Tmpfs>` so callers can mint files with specific
/// `(uid, gid, mode)` directly through the FsOps surface.
fn build_tmpfs_root() -> (Cap<DEntry>, Arc<Tmpfs>) {
    let tmpfs = Arc::new(Tmpfs::new());
    let payload = MountPayload::new_cap(
        tmpfs.clone() as Arc<dyn tx_subsystems::vfs::FsOpsV3>,
        tmpfs.clone() as Arc<dyn tx_subsystems::page_backed::FsPageBackingV3>,
        None,
        DevId::new(101),
        MountOptions::default(),
        "tmpfs-wave4",
        SourceLabel::Static("tmpfs-wave4"),
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
        MountId::new(11),
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

/// Devfs analog of `build_tmpfs_root`. Mounts the shipping
/// `tx_fs::devfs::Devfs` (FsOps + FsPageBacking) at root.
fn build_devfs_root() -> Cap<DEntry> {
    let payload = MountPayload::new_cap(
        tx_fs::devfs::Devfs::fs_ops_v3_arc(),
        tx_fs::devfs::Devfs::fs_page_backing_v3_arc(),
        None,
        DevId::new(102),
        MountOptions::default(),
        "devfs-wave4",
        SourceLabel::Static("devfs-wave4"),
    )
    .expect("mount payload");

    let root_rnode = {
        let raw = RNode::new(
            tx_fs::devfs::DEVFS_ROOT_OBJECT_ID,
            InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = zone::reserve_for::<RNode>().expect("rnode reservation");
        zone::sign_for(res, raw)
    };

    let _mount = MountIdentity::new_cap(
        MountId::new(12),
        None,
        root_rnode.clone(),
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount identity");

    DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry")
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

/// Drop privileges on `proc_cap`: setresuid to `(uid, uid, uid)`
/// then clear effective + permitted caps. Mirrors
/// `dac_setuid_wave2::drop_privs_to`.
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
/// inline `read_user_cstr` reads it correctly. Returns the buffer
/// + a u64 pointer suitable for the syscall args.
fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

// -----------------------------------------------------------------
// fchmodat
// -----------------------------------------------------------------

/// `fchmodat(AT_FDCWD, "/f", 0o600, 0)` against a tmpfs file owned
/// by uid 1000 succeeds when the caller is uid 1000. Verifies the
/// arm wires `walker_cred` (effective ids) into
/// `FsOps::step_chmod` and that mode is masked to 0o7777.
#[test]
fn dispatch_fchmodat_owner_succeeds() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    // Create the file as uid 1000 so the inode is owned by them.
    let guard = tx_substrate::epoch::guard();
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FCHMODAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o600, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    // Backend reflects the new mode.
    let guard = tx_substrate::epoch::guard();
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.mode & 0o7777, 0o600);
    drop(path);
}

/// `fchmodat` against a file owned by someone else, called from a
/// non-privileged caller, returns `-EPERM` (matches Linux's
/// `chmod(2)` errno for "not owner, no CAP_FOWNER").
#[test]
fn dispatch_fchmodat_non_owner_returns_neg_eperm() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File owned by uid 1000.
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Caller is uid 2000 — not the owner.
    drop_privs_to(&proc_cap, 2000);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FCHMODAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_PERM));
    drop(path);
}

/// `fchmodat` against devfs (a read-only projection) returns
/// `-EROFS` per Wave 3 Part 2's devfs-side `Errno::EROFS`.
#[test]
fn dispatch_fchmodat_devfs_returns_neg_erofs() {
    let _setup = wave4_setup();
    let root_dentry = build_devfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    // The devfs root inode itself is targetable — chmod on it
    // routes through `step_chmod` which devfs short-circuits to
    // EROFS regardless of fs_object_id. Use the root path "/"
    // which always resolves.
    let path = nul_terminate(b"/");
    let req = SyscallRequest::new(
        NR_FCHMODAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o755, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ROFS));
    drop(path);
}

/// Any non-`AT_FDCWD` dirfd value returns `-EBADF`. The slice's
/// fd table doesn't carry directory-fd semantics yet
/// (TODO(phase-dirfd)).
#[test]
fn dispatch_fchmodat_invalid_dirfd_returns_neg_ebadf() {
    let _setup = wave4_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    // dirfd = 3 (a positive fd value); not AT_FDCWD = -100.
    let req = SyscallRequest::new(NR_FCHMODAT, [3u64, path.as_ptr() as u64, 0o600, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
    drop(path);
}

/// A path with no NUL terminator within `EXECVE_PATH_MAX` returns
/// `-ENAMETOOLONG`. Matches the `read_user_cstr` budget.
#[test]
fn dispatch_fchmodat_path_too_long_returns_neg_enametoolong() {
    let _setup = wave4_setup();
    let (root_dentry, _tmpfs) = build_tmpfs_root();
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    // 4097 'A' bytes, no NUL — `read_user_cstr` walks the full
    // budget and gives up.
    let path: Vec<u8> = vec![b'A'; EXECVE_PATH_MAX + 1];
    let req = SyscallRequest::new(
        NR_FCHMODAT,
        [AT_FDCWD as i64 as u64, path.as_ptr() as u64, 0o600, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NAMETOOLONG));
    drop(path);
}

// -----------------------------------------------------------------
// fchownat
// -----------------------------------------------------------------

/// Non-privileged caller chowning to its own uid+gid succeeds (the
/// no-op-shaped self-chown POSIX explicitly allows for non-root
/// callers).
#[test]
fn dispatch_fchownat_unprivileged_to_self_succeeds() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 1000,
        gid: 200,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Set caller to (uid=1000, gid=200) — matching the file's
    // owner so the self-chown is a no-op-shaped success. Use the
    // direct test helper rather than chaining setresuid/setresgid:
    // the file's gid is 200, not 0, so the shipping mutators
    // would need a valid starting state.
    set_cred_ids_for_test(&proc_cap, 1000, 1000, 1000, 200, 200, 200);
    clear_caps_for_test(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FCHOWNAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            1000,
            200,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    let guard = tx_substrate::epoch::guard();
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.uid, 1000);
    assert_eq!(meta.gid, 200);
    drop(path);
}

/// Non-privileged caller chowning to a foreign uid returns
/// `-EPERM`. Matches POSIX `chown(2)` ("only superuser may change
/// the file's owner").
#[test]
fn dispatch_fchownat_unprivileged_to_other_returns_neg_eperm() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    drop_privs_to(&proc_cap, 1000);
    let ctx = make_ctx(proc_cap, thread);

    // Try to chown to uid 2000 (foreign). Must EPERM.
    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FCHOWNAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            2000,
            NEG_ONE_U32,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_PERM));
    drop(path);
}

/// `fchownat(.., -1, -1, ..)` (both ids = sentinel) is a "leave
/// unchanged" no-op. Confirms `decode_uid_arg` / `decode_gid_arg`
/// flow correctly into `step_chown`'s `(None, None)`.
#[test]
fn dispatch_fchownat_minus_one_leaves_unchanged() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 1000,
        gid: 200,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let (file_id, _) =
        match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard) {
            StepOutcome::Done(out) => out,
            other => panic!("create_inode: {other:?}"),
        };
    drop(guard);

    // Stay root + CAP_FOWNER so step_chown is permitted; this test
    // is about the sentinel decoding, not the privilege check.
    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FCHOWNAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            NEG_ONE_U32,
            NEG_ONE_U32,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));

    // uid/gid unchanged (still 1000/200 from create_inode).
    let guard = tx_substrate::epoch::guard();
    let meta = match tmpfs.load_inode_meta(file_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.uid, 1000);
    assert_eq!(meta.gid, 200);
    drop(path);
}

// -----------------------------------------------------------------
// faccessat / faccessat2
// -----------------------------------------------------------------

/// `faccessat(AT_FDCWD, "/f", F_OK)` against an existing tmpfs
/// file returns 0. F_OK = 0 short-circuits the permission-bit
/// check after path resolution succeeds.
#[test]
fn dispatch_faccessat_existing_file_f_ok_returns_zero() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FACCESSAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            F_OK as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    drop(path);
}

/// `faccessat(.., R_OK)` on a file with no read bits in any
/// triplet returns `-EACCES`. Caller is non-root, non-owner so
/// the "other" triplet (0o0) applies.
#[test]
fn dispatch_faccessat_no_read_bit_returns_neg_eacces() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File owned by uid 1000, mode 0o000 (no perms anywhere).
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Caller uid 2000 — not the owner.
    drop_privs_to(&proc_cap, 2000);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FACCESSAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            R_OK as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    drop(path);
}

/// `faccessat(.., R_OK | W_OK)` from a non-owner caller carrying
/// `CAP_DAC_OVERRIDE` returns 0 — read/write are always granted
/// to DAC_OVERRIDE callers regardless of the inode's mode bits.
#[test]
fn dispatch_faccessat_dac_override_bypasses() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File owned by uid 1000, mode 0o000.
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100000, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Caller is uid 2000 (non-owner) but carries CAP_DAC_OVERRIDE.
    drop_privs_to(&proc_cap, 2000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::DAC_OVERRIDE);
    install_caps_for_test(&proc_cap, caps);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    // R_OK | W_OK only — X_OK has the Linux quirk tested below.
    let req = SyscallRequest::new(
        NR_FACCESSAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            (R_OK | W_OK) as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    drop(path);
}

/// `faccessat2(.., AT_EACCESS)` switches to **effective** ids.
/// File owned by uid 1000; caller's real uid is 2000 (non-owner)
/// but its effective uid is 1000 (owner). With AT_EACCESS, the
/// owner triplet applies and R_OK is granted.
#[test]
fn dispatch_faccessat2_at_eaccess_uses_effective_uid() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File owned by uid 1000, mode 0o400 (owner-read only).
    let owner_cred = Credential {
        uid: 1000,
        gid: 0,
        effective_caps: CapabilitySet::EMPTY,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100400, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Real uid 2000, effective uid 1000 — the AT_EACCESS shape.
    // Reach this directly via the test-only setter; the shipping
    // `step_setresuid` rules can't move from root → (real=2000,
    // effective=1000) in one call.
    set_cred_ids_for_test(&proc_cap, 2000, 1000, 1000, 0, 0, 0);
    clear_caps_for_test(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");

    // Without AT_EACCESS: real uid 2000 is "other" → 0 perm bits → EACCES.
    let req_real = SyscallRequest::new(
        NR_FACCESSAT2,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            R_OK as u64,
            0,
            0,
            0,
        ],
    );
    let result_real = block_on(dispatch::<ShimsTestPmap>(req_real, &ctx));
    assert_eq!(
        result_real,
        SyscallResult::Error(E_ACCES),
        "real-id walk should treat caller as 'other' and deny R_OK"
    );

    // With AT_EACCESS: effective uid 1000 matches inode owner →
    // owner triplet (0o4) → R_OK granted.
    let req_effective = SyscallRequest::new(
        NR_FACCESSAT2,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            R_OK as u64,
            AT_EACCESS as u64,
            0,
            0,
        ],
    );
    let result_effective = block_on(dispatch::<ShimsTestPmap>(req_effective, &ctx));
    assert_eq!(result_effective, SyscallResult::Return(0));
    drop(path);
}

/// Linux X-bit quirk: `access(X_OK)` fails with EACCES if no
/// execute bit is set anywhere on the inode, even when the caller
/// holds `CAP_DAC_OVERRIDE`. Matches `fs/namei.c::generic_permission`.
#[test]
fn dispatch_faccessat2_no_x_bit_returns_neg_eacces_even_with_dac_override() {
    let _setup = wave4_setup();
    let (root_dentry, tmpfs) = build_tmpfs_root();
    // File mode 0o644 — no execute bit anywhere.
    let owner_cred = Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, b"f", 0o100644, &owner_cred, &guard);
    drop(guard);

    let (proc_cap, thread) = bootstrap_with_cwd(root_dentry);
    // Drop to non-owner, then re-install CAP_DAC_OVERRIDE only.
    drop_privs_to(&proc_cap, 2000);
    let mut caps = CapabilitySet::EMPTY;
    caps.add(Capability::DAC_OVERRIDE);
    install_caps_for_test(&proc_cap, caps);
    let ctx = make_ctx(proc_cap, thread);

    let path = nul_terminate(b"/f");
    let req = SyscallRequest::new(
        NR_FACCESSAT2,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            X_OK as u64,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
    drop(path);
}
