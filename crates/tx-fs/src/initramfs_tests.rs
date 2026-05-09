//! Integration tests for `tx_subsystems::initramfs::unpack_into_root_mount`
//! against a freshly-mounted tmpfs.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use tx_substrate::step_v3::{Errno, NoProgress, StepOutcome};
use tx_substrate::zone::Cap;
use tx_subsystems::initramfs::{unpack_into_root_mount, UnpackError};
use tx_subsystems::mount::{MountFlags, MountIdentity, MountOptions, MountPayload, SourceLabel};
use tx_subsystems::vfs::{
    DEntry, FsObjectId, FsOpsV3, InlineName, InodeKind, RNode, RNodeBacking, S_IFMT,
};

use crate::tmpfs::Tmpfs;

const NEWC_HEADER_LEN: usize = 110;

fn init_substrate() {
    tx_substrate::testing::init_host_for_test_once();
    tx_subsystems::zones::register_all().expect("tx-subsystems zones");
    match tx_substrate::page_allocator::claim_zero_frame() {
        Ok(_) | Err(tx_substrate::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for initramfs tests: {error:?}"),
    }
}

/// Build a minimal `Cap<MountIdentity>` over a fresh `Tmpfs`. Mirrors
/// `tx-kernel::init::CoreInit::mount_rootfs_tmpfs` but inline so the
/// tx-fs test binary can drive `unpack_into_root_mount` without
/// pulling in tx-kernel.
fn fresh_rootfs_mount() -> Cap<MountIdentity> {
    let (_tmpfs, mount_output) = Tmpfs::new_root();
    let payload = MountPayload::new_cap(
        mount_output.fs_ops.clone(),
        mount_output.fs_ops_v3.clone(),
        mount_output.fs_page_backing.clone(),
        mount_output.fs_page_backing_v3.clone(),
        None,
        tx_subsystems::mount::DevId::new(0xfeed),
        MountOptions::default(),
        "tmpfs",
        SourceLabel::Static("rootfs"),
    )
    .expect("payload");

    let root_rnode = {
        let raw = RNode::new(
            mount_output.root_fs_object_id,
            mount_output.root_inode_meta,
            RNodeBacking::Directory,
        )
        .with_containing_mount(&payload);
        let res = tx_substrate::zone::reserve_for::<RNode>().expect("rnode");
        tx_substrate::zone::sign_for(res, raw)
    };
    let _root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode.clone()).expect("root dentry");

    MountIdentity::new_cap(
        tx_subsystems::mount::MountId::new(0xfeed),
        None,
        root_rnode,
        None,
        payload,
        MountFlags::empty(),
    )
    .expect("mount")
}

fn build_test_cpio(entries: &[(&[u8], u32, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut next_ino: u32 = 1;
    for (name, mode, data) in entries {
        emit_entry(&mut buf, name, *mode, data, next_ino);
        next_ino = next_ino.wrapping_add(1);
    }
    emit_entry(&mut buf, b"TRAILER!!!", 0, &[], 0);
    buf
}

fn emit_entry(buf: &mut Vec<u8>, name: &[u8], mode: u32, data: &[u8], ino: u32) {
    let namesize = name.len() + 1;
    let header_start = buf.len();
    buf.extend_from_slice(b"070701");
    buf.extend_from_slice(&hex8(ino));
    buf.extend_from_slice(&hex8(mode));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(1));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(data.len() as u32));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(0));
    buf.extend_from_slice(&hex8(namesize as u32));
    buf.extend_from_slice(&hex8(0));
    debug_assert_eq!(buf.len() - header_start, NEWC_HEADER_LEN);
    buf.extend_from_slice(name);
    buf.push(0);
    while (buf.len() & 3) != 0 {
        buf.push(0);
    }
    buf.extend_from_slice(data);
    while (buf.len() & 3) != 0 {
        buf.push(0);
    }
}

fn hex8(value: u32) -> [u8; 8] {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = [0u8; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = (7 - i) * 4;
        *slot = HEX[((value >> shift) & 0xf) as usize];
    }
    out
}

/// Walk the mount's root, look up `name`, and return the resolved id.
fn lookup_in_root(
    mount: &Cap<MountIdentity>,
    name: &[u8],
) -> StepOutcome<FsObjectId, NoProgress> {
    let payload = mount
        .payload_cap()
        .expect("mount payload alive in test")
        .into_cap();
    let root_id = mount.root().fs_object_id();
    let guard = tx_substrate::epoch::guard();
    payload.fs_ops_v3.lookup(root_id, name, &guard)
}

fn lookup_in(
    mount: &Cap<MountIdentity>,
    parent: FsObjectId,
    name: &[u8],
) -> StepOutcome<FsObjectId, NoProgress> {
    let payload = mount
        .payload_cap()
        .expect("mount payload alive in test")
        .into_cap();
    let guard = tx_substrate::epoch::guard();
    payload.fs_ops_v3.lookup(parent, name, &guard)
}

fn fs_ops_of(mount: &Cap<MountIdentity>) -> Arc<dyn FsOpsV3> {
    mount
        .payload_cap()
        .expect("mount payload alive in test")
        .into_cap()
        .fs_ops_v3
        .clone()
}

#[test]
fn unpack_writes_single_regular_file_with_correct_size() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let archive = build_test_cpio(&[(b"hello", 0o100644, b"world\n")]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.files, 1);
    assert_eq!(stats.dirs, 0);
    assert_eq!(stats.symlinks, 0);
    assert_eq!(stats.bytes_total, b"world\n".len() as u64);

    let id = match lookup_in_root(&mount, b"hello") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup hello: {other:?}"),
    };
    let fs_ops = fs_ops_of(&mount);
    let guard = tx_substrate::epoch::guard();
    let meta = match fs_ops.load_inode_meta(id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
    assert_eq!(meta.size, b"world\n".len() as u64);
    assert_eq!(meta.mode & !S_IFMT, 0o644);
}

#[test]
fn unpack_creates_parent_dirs_implicitly() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let archive = build_test_cpio(&[(b"bin/sh", 0o100755, b"shellbytes")]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.files, 1);

    let bin_id = match lookup_in_root(&mount, b"bin") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup bin: {other:?}"),
    };
    let sh_id = match lookup_in(&mount, bin_id, b"sh") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup bin/sh: {other:?}"),
    };
    let fs_ops = fs_ops_of(&mount);
    let guard = tx_substrate::epoch::guard();
    let meta = match fs_ops.load_inode_meta(sh_id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Regular);
    assert_eq!(meta.size, b"shellbytes".len() as u64);
}

#[test]
fn unpack_writes_directory_entry() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let archive = build_test_cpio(&[(b"etc", 0o040755, b"")]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.dirs, 1);
    assert_eq!(stats.files, 0);

    let id = match lookup_in_root(&mount, b"etc") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup etc: {other:?}"),
    };
    let fs_ops = fs_ops_of(&mount);
    let guard = tx_substrate::epoch::guard();
    let meta = match fs_ops.load_inode_meta(id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Directory);
}

#[test]
fn unpack_writes_symlink_entry() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let archive = build_test_cpio(&[(b"link", 0o120777, b"/bin/sh")]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.symlinks, 1);

    let id = match lookup_in_root(&mount, b"link") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup link: {other:?}"),
    };
    let fs_ops = fs_ops_of(&mount);
    let guard = tx_substrate::epoch::guard();
    let meta = match fs_ops.load_inode_meta(id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.kind(), InodeKind::Symlink);
    let target = match fs_ops.read_link(id, &guard) {
        StepOutcome::Done(t) => t,
        other => panic!("read_link: {other:?}"),
    };
    assert_eq!(target.as_ref(), b"/bin/sh");
}

#[test]
fn unpack_skips_unsupported_kinds_without_panicking() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    // Mode 0o020666 = S_IFCHR | rw-rw-rw-
    let archive = build_test_cpio(&[(b"null", 0o020666, b"")]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.unsupported, 1);
    assert_eq!(stats.files, 0);

    // The entry is skipped: looking it up returns ENOENT.
    match lookup_in_root(&mount, b"null") {
        StepOutcome::Err(Errno::ENOENT) => {}
        other => panic!("expected ENOENT, got {other:?}"),
    }
}

#[test]
fn unpack_handles_multi_page_files() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    // Build a payload spanning ~3 user pages so the page-by-page
    // memcpy loop iterates more than once.
    let page_size = tx_subsystems::vm::USER_PAGE_SIZE;
    let target_len = page_size * 2 + 1234;
    let mut data: Vec<u8> = Vec::with_capacity(target_len);
    for i in 0..target_len {
        data.push((i & 0xff) as u8);
    }
    let archive = build_test_cpio(&[(b"big", 0o100644, &data)]);
    let mount = fresh_rootfs_mount();
    let stats = unpack_into_root_mount(&archive, &mount).expect("unpack");
    assert_eq!(stats.files, 1);
    assert_eq!(stats.bytes_total, target_len as u64);

    let id = match lookup_in_root(&mount, b"big") {
        StepOutcome::Done(id) => id,
        other => panic!("lookup big: {other:?}"),
    };
    let fs_ops = fs_ops_of(&mount);
    let guard = tx_substrate::epoch::guard();
    let meta = match fs_ops.load_inode_meta(id, &guard) {
        StepOutcome::Done(m) => m,
        other => panic!("load_inode_meta: {other:?}"),
    };
    assert_eq!(meta.size, target_len as u64);
}

#[test]
fn unpack_returns_parse_error_on_bad_magic() {
    let _serial = crate::test_support::FS_TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    init_substrate();

    let archive = vec![0u8; NEWC_HEADER_LEN];
    let mount = fresh_rootfs_mount();
    let err = unpack_into_root_mount(&archive, &mount).unwrap_err();
    assert!(matches!(err, UnpackError::Parse(_)));
}
