// Dispatch-level coverage for the N71M3 network namespace user ABI.

use super::*;

use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountOptions, MountPayload, SourceLabel,
};
use tx_subsystems::process::step_chdir;
use tx_subsystems::vfs::structure::{DEntry, InlineName, RNodeBacking, StructPayload, S_IFREG};
use tx_subsystems::vfs::FsOps;

use crate::linux_syscall::{AT_FDCWD, CLONE_NEWNET, NR_OPENAT, NR_SETNS, NR_UNSHARE, O_RDONLY};

fn netns_req(nr: u64, args: [u64; 6], ctx: &SyscallCtx<'static>) -> SyscallResult {
    block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(nr, args),
        ctx,
    ))
}

fn nul_terminate(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn build_procfs_root(dev_id: u32, mount_id: u64) -> Cap<DEntry> {
    let procfs = tx_fs::procfs::Procfs::new();
    let fs_ops = tx_fs::procfs::Procfs::fs_ops_arc();
    let backing: Arc<dyn tx_subsystems::page_backed::FsPageBacking> =
        Arc::new(tx_fs::procfs::Procfs::new());
    let mount = MountPayload::new_cap(
        fs_ops,
        backing,
        None,
        DevId::new(dev_id),
        MountOptions::default(),
        "proc",
        SourceLabel::Static("proc"),
    )
    .expect("procfs mount payload");
    let guard = guard();
    let meta = match procfs.load_inode_meta(tx_fs::procfs::PROCFS_ROOT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load procfs root meta failed: {other:?}"),
    };
    let root_rnode =
        match procfs.materialise_rnode(tx_fs::procfs::PROCFS_ROOT_ID, meta, &mount, &guard) {
            StepOutcome::Done(rnode) => rnode,
            other => panic!("materialise procfs root failed: {other:?}"),
        };
    let _mount = MountIdentity::new_cap(
        MountId::new(mount_id),
        None,
        root_rnode.clone(),
        None,
        mount,
        MountFlags::empty(),
    )
    .expect("procfs mount identity");

    DEntry::new_cap(InlineName::ROOT, root_rnode).expect("procfs root dentry")
}

#[test]
fn dispatch_unshare_clone_newnet_replaces_process_namespace() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let before = process.net_namespace().expect("initial net namespace");

    let result = netns_req(NR_UNSHARE, [CLONE_NEWNET, 0, 0, 0, 0, 0], &ctx);

    assert_eq!(result, SyscallResult::Return(0));
    let after = process.net_namespace().expect("unshared net namespace");
    assert_ne!(
        before.key(),
        after.key(),
        "unshare(CLONE_NEWNET) must publish a fresh namespace payload"
    );
}

#[test]
fn dispatch_setns_clone_newnet_joins_namespace_fd_payload() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let target = tx_subsystems::net::create_isolated_net_namespace("setns-target")
        .expect("target namespace")
        .payload_cap()
        .expect("target namespace payload");
    let file =
        tx_subsystems::net::net_namespace_open_file_from_payload(target.clone()).expect("netns fd");
    process.set_fd(7, Some(file));

    let result = netns_req(NR_SETNS, [7, CLONE_NEWNET, 0, 0, 0, 0], &ctx);

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(
        process.net_namespace().expect("joined net namespace").key(),
        target.key()
    );
}

#[test]
fn dispatch_openat_proc_self_ns_net_installs_calling_namespace_fd_payload() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let netns = process.net_namespace().expect("process net namespace");

    let path = nul_terminate(b"/proc/self/ns/net");
    let result = netns_req(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
        &ctx,
    );

    let fd = match result {
        SyscallResult::Return(fd) if fd >= 0 => fd as u32,
        other => panic!("openat(/proc/self/ns/net) failed: {other:?}"),
    };
    let file = process.fd(fd).expect("namespace fd installed");
    let payload =
        tx_subsystems::net::net_namespace_payload_from_file(&file).expect("namespace payload");
    assert_eq!(payload.key(), netns.key());
}

#[test]
fn dispatch_openat_proc_pid_ns_net_walks_intermediate_ns_directory() {
    let _setup = setup();
    let procfs_root = build_procfs_root(73, 73);
    let parent = bootstrap();
    match step_chdir(&parent, procfs_root) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
            panic!("parent process zombified")
        }
    }
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false)
        .expect("fork child process");
    let child_netns = child.net_namespace().expect("child net namespace");
    let thread = first_thread(&parent);
    let ctx = make_ctx(parent.clone(), thread);

    let path = nul_terminate(alloc::format!("/{}/ns/net", child.pid.0).as_bytes());
    let result = netns_req(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
        &ctx,
    );

    let fd = match result {
        SyscallResult::Return(fd) if fd >= 0 => fd as u32,
        other => panic!("openat(/<pid>/ns/net) failed: {other:?}"),
    };
    let file = parent.fd(fd).expect("namespace fd installed");
    let payload =
        tx_subsystems::net::net_namespace_payload_from_file(&file).expect("namespace payload");
    assert_eq!(payload.key(), child_netns.key());
}

#[test]
fn procfs_pid_ns_net_materialises_namespace_fd_payload() {
    let _setup = setup();
    let process = bootstrap();
    let netns = process.net_namespace().expect("process net namespace");
    let procfs = tx_fs::procfs::Procfs::new();
    let fs_ops = tx_fs::procfs::Procfs::fs_ops_arc();
    let backing: Arc<dyn tx_subsystems::page_backed::FsPageBacking> =
        Arc::new(tx_fs::procfs::Procfs::new());
    let mount = MountPayload::new_cap(
        fs_ops,
        backing,
        None,
        DevId::new(71),
        MountOptions::default(),
        "proc",
        SourceLabel::Static("proc"),
    )
    .expect("procfs mount payload");
    let guard = guard();

    let pid = process.pid.0.to_string();
    let pid_id = match procfs.lookup(tx_fs::procfs::PROCFS_ROOT_ID, pid.as_bytes(), &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<pid> failed: {other:?}"),
    };
    let ns_id = match procfs.lookup(pid_id, b"ns", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<pid>/ns failed: {other:?}"),
    };
    let net_id = match procfs.lookup(ns_id, b"net", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<pid>/ns/net failed: {other:?}"),
    };
    let meta = match procfs.load_inode_meta(net_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta /proc/<pid>/ns/net failed: {other:?}"),
    };
    let rnode = match procfs.materialise_rnode(net_id, meta, &mount, &guard) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise /proc/<pid>/ns/net failed: {other:?}"),
    };

    match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::NetNamespace { payload },
        } => assert_eq!(payload.key(), netns.key()),
        other => panic!("/proc/<pid>/ns/net should be netns struct-backed, got {other:?}"),
    }
}

#[test]
fn procfs_non_init_pid_ns_net_does_not_collide_with_next_pid_ns_dir() {
    let _setup = setup();
    let parent = bootstrap();
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false)
        .expect("fork child process");
    let netns = child.net_namespace().expect("child net namespace");
    let procfs = tx_fs::procfs::Procfs::new();
    let fs_ops = tx_fs::procfs::Procfs::fs_ops_arc();
    let backing: Arc<dyn tx_subsystems::page_backed::FsPageBacking> =
        Arc::new(tx_fs::procfs::Procfs::new());
    let mount = MountPayload::new_cap(
        fs_ops,
        backing,
        None,
        DevId::new(72),
        MountOptions::default(),
        "proc",
        SourceLabel::Static("proc"),
    )
    .expect("procfs mount payload");
    let guard = guard();

    let pid = child.pid.0.to_string();
    let pid_id = match procfs.lookup(tx_fs::procfs::PROCFS_ROOT_ID, pid.as_bytes(), &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<child-pid> failed: {other:?}"),
    };
    let ns_id = match procfs.lookup(pid_id, b"ns", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<child-pid>/ns failed: {other:?}"),
    };
    let net_id = match procfs.lookup(ns_id, b"net", &guard) {
        StepOutcome::Done(id) => id,
        other => panic!("lookup /proc/<child-pid>/ns/net failed: {other:?}"),
    };
    let meta = match procfs.load_inode_meta(net_id, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load_inode_meta /proc/<child-pid>/ns/net failed: {other:?}"),
    };
    assert_eq!(meta.mode & S_IFREG, S_IFREG);
    let rnode = match procfs.materialise_rnode(net_id, meta, &mount, &guard) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise /proc/<child-pid>/ns/net failed: {other:?}"),
    };

    match rnode.backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::NetNamespace { payload },
        } => assert_eq!(payload.key(), netns.key()),
        other => panic!("/proc/<child-pid>/ns/net should be netns struct-backed, got {other:?}"),
    }
}
