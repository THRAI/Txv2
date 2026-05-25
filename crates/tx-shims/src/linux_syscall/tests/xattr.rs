// xattr syscall dispatch tests.
#![cfg_attr(test, allow(unused_imports))]

use super::*;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::adapter::step_engine::{
    self as step_engine, guard, page_allocator, reserve_for, sign_for, Cap,
};
use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_subsystems::cred::CapabilitySet;
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
    AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW, NR_FGETXATTR, NR_FLISTXATTR, NR_FREMOVEXATTR,
    NR_FSETXATTR, NR_GETXATTR, NR_GETXATTRAT, NR_LGETXATTR, NR_LISTXATTR, NR_LISTXATTRAT,
    NR_LLISTXATTR, NR_LREMOVEXATTR, NR_LSETXATTR, NR_OPENAT, NR_REMOVEXATTR, NR_REMOVEXATTRAT,
    NR_SETXATTR, NR_SETXATTRAT, XATTR_CREATE, XATTR_REPLACE,
};

const E_BADF: i32 = 9;
const E_INVAL: i32 = 22;
const E_NODATA: i32 = 61;
const E_OPNOTSUPP: i32 = 95;
const E_RANGE: i32 = 34;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestXattrArgs {
    value: u64,
    size: u32,
    flags: u32,
}

fn ensure_zero_frame_claimed() {
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for xattr tests: {error:?}"),
    }
}

fn xattr_setup() -> TestSetup {
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
        DevId::new(501),
        MountOptions::default(),
        "tmpfs-xattr",
        SourceLabel::Static("tmpfs-xattr"),
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
        MountId::new(51),
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
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => panic!("init bootstrap zombified"),
    }
    (process, thread)
}

fn root_cred() -> Credential {
    Credential {
        uid: 0,
        gid: 0,
        effective_caps: CapabilitySet::FULL,
    }
}

fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn create_regular(tmpfs: &Arc<Tmpfs>, name: &[u8]) {
    let cred = root_cred();
    let guard = guard();
    match tmpfs.create_inode(TMPFS_ROOT_OBJECT_ID, name, 0o100644, &cred, &guard) {
        step_engine::StepOutcome::Done(_) => {}
        other => panic!("create_inode {:?}: {other:?}", name),
    }
}

fn open_file(ctx: &SyscallCtx<'_>, path: &[u8]) -> u64 {
    let path = nul_terminate(path);
    let req = SyscallRequest::new(
        NR_OPENAT,
        [AT_FDCWD as u64, path.as_ptr() as u64, 0, 0, 0, 0],
    );
    match block_on(dispatch::<ShimsTestPmap>(req, ctx)) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("open_file({path:?}) failed: {other:?}"),
    }
}

#[test]
fn dispatch_legacy_xattr_path_fd_and_list_round_trip() {
    let _setup = xattr_setup();
    let (root, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (process, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(process, thread);

    let path = nul_terminate(b"/f");
    let name = nul_terminate(b"user.alpha");
    let value = b"bravo";
    let set = SyscallRequest::new(
        NR_SETXATTR,
        [
            path.as_ptr() as u64,
            name.as_ptr() as u64,
            value.as_ptr() as u64,
            value.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(set, &ctx)),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; 8];
    let get = SyscallRequest::new(
        NR_GETXATTR,
        [
            path.as_ptr() as u64,
            name.as_ptr() as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(get, &ctx)),
        SyscallResult::Return(5)
    );
    assert_eq!(&out[..5], b"bravo");

    let fd = open_file(&ctx, b"/f");
    let replacement = b"charlie";
    let fset = SyscallRequest::new(
        NR_FSETXATTR,
        [
            fd,
            name.as_ptr() as u64,
            replacement.as_ptr() as u64,
            replacement.len() as u64,
            XATTR_REPLACE as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(fset, &ctx)),
        SyscallResult::Return(0)
    );

    let mut list = [0u8; 32];
    let flist = SyscallRequest::new(
        NR_FLISTXATTR,
        [fd, list.as_mut_ptr() as u64, list.len() as u64, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(flist, &ctx)),
        SyscallResult::Return(11)
    );
    assert_eq!(&list[..11], b"user.alpha\0");

    let remove = SyscallRequest::new(NR_FREMOVEXATTR, [fd, name.as_ptr() as u64, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(remove, &ctx)),
        SyscallResult::Return(0)
    );
    let missing = SyscallRequest::new(
        NR_FGETXATTR,
        [
            fd,
            name.as_ptr() as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(missing, &ctx)),
        SyscallResult::Error(E_NODATA)
    );
}

#[test]
fn dispatch_xattrat_real_dirfd_empty_path_and_args_validation() {
    let _setup = xattr_setup();
    let (root, tmpfs) = build_tmpfs_root();
    let guard = guard();
    let cred = root_cred();
    match tmpfs.mkdir(TMPFS_ROOT_OBJECT_ID, b"d", 0o755, &cred, &guard) {
        step_engine::StepOutcome::Done(_) => {}
        other => panic!("mkdir d: {other:?}"),
    }
    let dir_id = match tmpfs.lookup(TMPFS_ROOT_OBJECT_ID, b"d", &guard) {
        step_engine::StepOutcome::Done(id) => id,
        other => panic!("lookup d: {other:?}"),
    };
    match tmpfs.create_inode(dir_id, b"f", 0o100644, &cred, &guard) {
        step_engine::StepOutcome::Done(_) => {}
        other => panic!("create d/f: {other:?}"),
    }
    drop(guard);

    let (process, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(process, thread);
    let dirfd = open_file(&ctx, b"/d");
    let filefd = open_file(&ctx, b"/d/f");
    let rel = nul_terminate(b"f");
    let empty = [0u8];
    let name = nul_terminate(b"user.beta");
    let value = b"delta";
    let set_args = TestXattrArgs {
        value: value.as_ptr() as u64,
        size: value.len() as u32,
        flags: XATTR_CREATE,
    };
    let set = SyscallRequest::new(
        NR_SETXATTRAT,
        [
            dirfd,
            rel.as_ptr() as u64,
            0,
            name.as_ptr() as u64,
            &set_args as *const TestXattrArgs as u64,
            core::mem::size_of::<TestXattrArgs>() as u64,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(set, &ctx)),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; 8];
    let get_args = TestXattrArgs {
        value: out.as_mut_ptr() as u64,
        size: out.len() as u32,
        flags: 0,
    };
    let get_empty = SyscallRequest::new(
        NR_GETXATTRAT,
        [
            filefd,
            empty.as_ptr() as u64,
            AT_EMPTY_PATH as u64,
            name.as_ptr() as u64,
            &get_args as *const TestXattrArgs as u64,
            core::mem::size_of::<TestXattrArgs>() as u64,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(get_empty, &ctx)),
        SyscallResult::Return(5)
    );
    assert_eq!(&out[..5], b"delta");

    let bad_small = SyscallRequest::new(
        NR_GETXATTRAT,
        [
            dirfd,
            rel.as_ptr() as u64,
            0,
            name.as_ptr() as u64,
            &get_args as *const TestXattrArgs as u64,
            8,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(bad_small, &ctx)),
        SyscallResult::Error(E_INVAL)
    );

    let bad_flags = SyscallRequest::new(
        NR_LISTXATTRAT,
        [
            dirfd,
            rel.as_ptr() as u64,
            0x8000,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(bad_flags, &ctx)),
        SyscallResult::Error(E_INVAL)
    );
}

#[test]
fn dispatch_xattr_errors_for_flags_namespace_and_small_buffers() {
    let _setup = xattr_setup();
    let (root, tmpfs) = build_tmpfs_root();
    create_regular(&tmpfs, b"f");
    let (process, thread) = bootstrap_with_cwd(root);
    let ctx = make_ctx(process, thread);

    let path = nul_terminate(b"/f");
    let user_name = nul_terminate(b"user.alpha");
    let system_name = nul_terminate(b"system.posix_acl_access");
    let value = b"bravo";

    let replace_missing = SyscallRequest::new(
        NR_SETXATTR,
        [
            path.as_ptr() as u64,
            user_name.as_ptr() as u64,
            value.as_ptr() as u64,
            value.len() as u64,
            XATTR_REPLACE as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(replace_missing, &ctx)),
        SyscallResult::Error(E_NODATA)
    );

    let unsupported = SyscallRequest::new(
        NR_SETXATTR,
        [
            path.as_ptr() as u64,
            system_name.as_ptr() as u64,
            value.as_ptr() as u64,
            value.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(unsupported, &ctx)),
        SyscallResult::Error(E_OPNOTSUPP)
    );

    let set = SyscallRequest::new(
        NR_LSETXATTR,
        [
            path.as_ptr() as u64,
            user_name.as_ptr() as u64,
            value.as_ptr() as u64,
            value.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(set, &ctx)),
        SyscallResult::Return(0)
    );
    let mut tiny = [0u8; 2];
    let get_tiny = SyscallRequest::new(
        NR_LGETXATTR,
        [
            path.as_ptr() as u64,
            user_name.as_ptr() as u64,
            tiny.as_mut_ptr() as u64,
            tiny.len() as u64,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(get_tiny, &ctx)),
        SyscallResult::Error(E_RANGE)
    );

    let remove = SyscallRequest::new(
        NR_LREMOVEXATTR,
        [path.as_ptr() as u64, user_name.as_ptr() as u64, 0, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(remove, &ctx)),
        SyscallResult::Return(0)
    );
    let list = SyscallRequest::new(
        NR_LLISTXATTR,
        [
            path.as_ptr() as u64,
            tiny.as_mut_ptr() as u64,
            tiny.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(list, &ctx)),
        SyscallResult::Return(0)
    );
}
