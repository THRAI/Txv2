// Dispatch-level coverage for the N71M3 network namespace user ABI.

use super::*;

use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tx_fs::tmpfs::{Tmpfs, TMPFS_ROOT_OBJECT_ID};
use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};
use tx_subsystems::mount::{
    DevId, MountFlags, MountId, MountIdentity, MountNamespace, MountOptions, MountPayload,
    SourceLabel,
};
use tx_subsystems::page_backed::FsPageBacking;
use tx_subsystems::process::{step_chdir, step_set_mount_namespace};
use tx_subsystems::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, StructPayload,
    S_IFDIR, S_IFREG,
};
use tx_subsystems::vfs::FsOps;

use crate::linux_syscall::{
    AT_FDCWD, CLONE_NEWNET, CLONE_NEWUSER, CLONE_NEWUTS, EFAULT_VALUE, EINVAL_VALUE, EPERM_VALUE,
    NR_OPENAT, NR_SETDOMAINNAME, NR_SETHOSTNAME, NR_SETNS, NR_UNAME, NR_UNSHARE, NR_WRITE, O_CREAT,
    O_RDONLY, O_TRUNC, O_WRONLY,
};

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

fn make_unprivileged(process: &Cap<tx_subsystems::process::ProcessIdentity>) {
    set_cred_ids_for_test(process, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(process);
}

fn open_netns_path(ctx: &SyscallCtx<'static>, path: &[u8], flags: u32) -> u32 {
    let path = nul_terminate(path);
    match netns_req(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            flags as u64,
            0o644,
            0,
            0,
        ],
        ctx,
    ) {
        SyscallResult::Return(fd) if fd >= 0 => fd as u32,
        other => panic!("openat({}) failed: {other:?}", path_display(&path)),
    }
}

fn write_netns_fd(ctx: &SyscallCtx<'static>, fd: u32, bytes: &[u8]) -> SyscallResult {
    netns_req(
        NR_WRITE,
        [
            fd as u64,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            0,
            0,
            0,
        ],
        ctx,
    )
}

fn path_display(path: &[u8]) -> alloc::string::String {
    let without_nul = path.strip_suffix(&[0]).unwrap_or(path);
    alloc::string::String::from_utf8_lossy(without_nul).into_owned()
}

fn uts_field(buf: &[u8; 6 * 65], idx: usize) -> &[u8] {
    let start = idx * 65;
    let field = &buf[start..start + 65];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

fn uname_fields(ctx: &SyscallCtx<'static>) -> [u8; 6 * 65] {
    let mut buf = [0u8; 6 * 65];
    let result = netns_req(NR_UNAME, [buf.as_mut_ptr() as u64, 0, 0, 0, 0, 0], ctx);
    assert_eq!(result, SyscallResult::Return(0));
    buf
}

fn set_hostname(ctx: &SyscallCtx<'static>, name: &[u8]) -> SyscallResult {
    netns_req(
        NR_SETHOSTNAME,
        [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
        ctx,
    )
}

fn set_domainname(ctx: &SyscallCtx<'static>, name: &[u8]) -> SyscallResult {
    netns_req(
        NR_SETDOMAINNAME,
        [name.as_ptr() as u64, name.len() as u64, 0, 0, 0, 0],
        ctx,
    )
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

fn chdir_to_root_with_procfs_mount(
    process: &Cap<tx_subsystems::process::ProcessIdentity>,
    dev_id: u32,
) {
    tx_subsystems::cross_crate_test_support::reset_mount_table();

    let tmpfs = Arc::new(Tmpfs::new());
    let root_payload = MountPayload::new_cap(
        tmpfs.clone() as Arc<dyn FsOps>,
        tmpfs.clone() as Arc<dyn FsPageBacking>,
        None,
        DevId::new(dev_id),
        MountOptions::default(),
        "netns-test-rootfs",
        SourceLabel::Static("netns-test-rootfs"),
    )
    .expect("rootfs mount payload");
    let root_rnode = RNode::new_cap_in_mount(
        TMPFS_ROOT_OBJECT_ID,
        InodeMeta::new(InodeKind::Directory, S_IFDIR | 0o755),
        RNodeBacking::Directory,
        &root_payload,
    )
    .expect("rootfs root rnode");
    let root_mount = MountIdentity::new_cap(
        MountId::new(dev_id as u64),
        None,
        root_rnode.clone(),
        None,
        root_payload.clone(),
        MountFlags::empty(),
    )
    .expect("rootfs mount identity");
    let root_dentry = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");

    let guard = guard();
    let (proc_id, proc_meta) = match tmpfs.mkdir(
        TMPFS_ROOT_OBJECT_ID,
        b"proc",
        0o555,
        &Credential::root(),
        &guard,
    ) {
        StepOutcome::Done(created) => created,
        other => panic!("mkdir /proc failed: {other:?}"),
    };
    let proc_mountpoint_rnode =
        RNode::new_cap_in_mount(proc_id, proc_meta, RNodeBacking::Directory, &root_payload)
            .expect("/proc mountpoint rnode");
    let proc_mountpoint = DEntry::new_cap(
        InlineName::new(b"proc").expect("proc name"),
        proc_mountpoint_rnode,
    )
    .expect("/proc mountpoint dentry");

    let procfs = tx_fs::procfs::Procfs::new();
    let proc_payload = MountPayload::new_cap(
        tx_fs::procfs::Procfs::fs_ops_arc(),
        Arc::new(tx_fs::procfs::Procfs::new()) as Arc<dyn FsPageBacking>,
        None,
        DevId::new(dev_id + 1000),
        MountOptions::default(),
        "proc",
        SourceLabel::Static("proc"),
    )
    .expect("procfs mount payload");
    let proc_root_meta = match procfs.load_inode_meta(tx_fs::procfs::PROCFS_ROOT_ID, &guard) {
        StepOutcome::Done(meta) => meta,
        other => panic!("load procfs root meta failed: {other:?}"),
    };
    let proc_root_rnode = match procfs.materialise_rnode(
        tx_fs::procfs::PROCFS_ROOT_ID,
        proc_root_meta,
        &proc_payload,
        &guard,
    ) {
        StepOutcome::Done(rnode) => rnode,
        other => panic!("materialise procfs root failed: {other:?}"),
    };
    let proc_mount = MountIdentity::new_cap(
        MountId::new(dev_id as u64 + 1000),
        Some(proc_mountpoint),
        proc_root_rnode,
        None,
        proc_payload,
        MountFlags::empty(),
    )
    .expect("procfs mount identity");
    tx_subsystems::mount::register_mount(&root_payload, proc_id, proc_mount.clone());

    let mnt_ns = MountNamespace::new_cap(root_mount).expect("mount namespace");
    mnt_ns.register_mount(&root_payload, proc_id, proc_mount);
    step_set_mount_namespace(process, mnt_ns).expect("publish test mount namespace");

    match step_chdir(process, root_dentry) {
        tx_subsystems::process::ChdirOutcome::Replaced { .. } => {}
        tx_subsystems::process::ChdirOutcome::ZombieIgnored => {
            panic!("process zombified while installing rootfs")
        }
    }
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
    assert_eq!(
        after
            .owner_user_namespace()
            .expect("new net namespace owner")
            .key(),
        process
            .user_namespace_cap()
            .expect("process user namespace")
            .key(),
        "new net namespace must be owned by the caller's current user namespace"
    );
}

#[test]
fn dispatch_unshare_clone_newnet_requires_privilege_in_current_user_namespace() {
    let _setup = setup();
    let process = bootstrap();
    make_unprivileged(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let before = process.net_namespace().expect("initial net namespace");

    let result = netns_req(NR_UNSHARE, [CLONE_NEWNET, 0, 0, 0, 0, 0], &ctx);

    assert_eq!(result, SyscallResult::Error(EPERM_VALUE));
    assert_eq!(
        process
            .net_namespace()
            .expect("unchanged net namespace")
            .key(),
        before.key()
    );
}

#[test]
fn dispatch_unshare_clone_newuser_replaces_user_namespace() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let before = process
        .user_namespace_cap()
        .expect("initial user namespace");

    let result = netns_req(NR_UNSHARE, [CLONE_NEWUSER, 0, 0, 0, 0, 0], &ctx);

    assert_eq!(result, SyscallResult::Return(0));
    let after = process
        .user_namespace_cap()
        .expect("unshared user namespace");
    assert_ne!(
        before.key(),
        after.key(),
        "unshare(CLONE_NEWUSER) must publish a fresh user namespace"
    );
    assert_eq!(
        after.parent.as_ref().map(|parent| parent.key()),
        Some(before.key())
    );
    assert_eq!(after.owner_uid, 0);
    assert_eq!(after.owner_gid, 0);
    assert!(after.uid_map.lock().is_empty());
    assert!(after.gid_map.lock().is_empty());
}

#[test]
fn dispatch_unshare_newuser_then_newnet_preserves_user_namespace() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);

    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWUSER, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    let userns = process
        .user_namespace_cap()
        .expect("unshared user namespace");
    let netns_before = process.net_namespace().expect("initial net namespace");

    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWNET, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    assert_eq!(
        process
            .user_namespace_cap()
            .expect("user namespace after net unshare")
            .key(),
        userns.key(),
        "unshare(CLONE_NEWNET) must not replace the user namespace"
    );
    assert_ne!(
        process.net_namespace().expect("new net namespace").key(),
        netns_before.key(),
        "unshare(CLONE_NEWNET) must still publish a fresh net namespace"
    );
    assert_eq!(
        process
            .net_namespace()
            .expect("new net namespace")
            .owner_user_namespace()
            .expect("net namespace owner")
            .key(),
        userns.key(),
        "net namespace created after CLONE_NEWUSER must be owned by that userns"
    );
}

#[test]
fn dispatch_unshare_newuser_newnet_combined_replaces_both_namespaces() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let user_before = process.user_namespace_cap().expect("initial userns");
    let net_before = process.net_namespace().expect("initial netns");

    let result = netns_req(
        NR_UNSHARE,
        [CLONE_NEWUSER | CLONE_NEWNET, 0, 0, 0, 0, 0],
        &ctx,
    );

    assert_eq!(result, SyscallResult::Return(0));
    let new_userns = process.user_namespace_cap().expect("new userns");
    assert_ne!(new_userns.key(), user_before.key());
    assert_ne!(
        process.net_namespace().expect("new netns").key(),
        net_before.key()
    );
    assert_eq!(
        process
            .net_namespace()
            .expect("new netns")
            .owner_user_namespace()
            .expect("netns owner")
            .key(),
        new_userns.key()
    );
}

#[test]
fn dispatch_unprivileged_newuser_newnet_combined_uses_new_userns_authority() {
    let _setup = setup();
    let process = bootstrap();
    make_unprivileged(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let user_before = process.user_namespace_cap().expect("initial userns");
    let net_before = process.net_namespace().expect("initial netns");

    let result = netns_req(
        NR_UNSHARE,
        [CLONE_NEWUSER | CLONE_NEWNET, 0, 0, 0, 0, 0],
        &ctx,
    );

    assert_eq!(result, SyscallResult::Return(0));
    let user_after = process.user_namespace_cap().expect("new userns");
    let net_after = process.net_namespace().expect("new netns");
    assert_ne!(user_after.key(), user_before.key());
    assert_ne!(net_after.key(), net_before.key());
    assert_eq!(
        net_after.owner_user_namespace().expect("netns owner").key(),
        user_after.key(),
        "combined NEWUSER|NEWNET must authorize netns creation using the new userns"
    );
}

#[test]
fn dispatch_sethostname_and_setdomainname_update_current_uts_uname() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);

    assert_eq!(set_hostname(&ctx, b"tx-uts-a"), SyscallResult::Return(0));
    assert_eq!(
        set_domainname(&ctx, b"lab.example"),
        SyscallResult::Return(0)
    );

    let uts = uname_fields(&ctx);
    assert_eq!(uts_field(&uts, 1), b"tx-uts-a");
    assert_eq!(uts_field(&uts, 5), b"lab.example");
}

#[test]
fn dispatch_unshare_clone_newuts_copies_then_isolates_hostname() {
    let _setup = setup();
    let parent = bootstrap();
    let parent_thread = first_thread(&parent);
    let parent_ctx = make_ctx(parent.clone(), parent_thread);
    assert_eq!(
        set_hostname(&parent_ctx, b"parent-host"),
        SyscallResult::Return(0)
    );

    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child process");
    let child_thread = first_thread(&child);
    let child_ctx = make_ctx(child.clone(), child_thread);
    let inherited = uname_fields(&child_ctx);
    assert_eq!(uts_field(&inherited, 1), b"parent-host");

    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWUTS, 0, 0, 0, 0, 0], &child_ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        set_hostname(&child_ctx, b"child-host"),
        SyscallResult::Return(0)
    );

    let parent_uts = uname_fields(&parent_ctx);
    let child_uts = uname_fields(&child_ctx);
    assert_eq!(uts_field(&parent_uts, 1), b"parent-host");
    assert_eq!(uts_field(&child_uts, 1), b"child-host");
}

#[test]
fn dispatch_unprivileged_newuser_newuts_combined_allows_uts_mutation() {
    let _setup = setup();
    let process = bootstrap();
    make_unprivileged(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);

    assert_eq!(
        set_hostname(&ctx, b"denied"),
        SyscallResult::Error(EPERM_VALUE)
    );
    assert_eq!(
        netns_req(
            NR_UNSHARE,
            [CLONE_NEWUSER | CLONE_NEWUTS, 0, 0, 0, 0, 0],
            &ctx
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(set_hostname(&ctx, b"user-uts"), SyscallResult::Return(0));

    let uts = uname_fields(&ctx);
    assert_eq!(uts_field(&uts, 1), b"user-uts");
}

#[test]
fn dispatch_sethostname_validates_pointer_and_length() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let too_long = [b'x'; 65];

    assert_eq!(
        netns_req(NR_SETHOSTNAME, [0, 1, 0, 0, 0, 0], &ctx),
        SyscallResult::Error(EFAULT_VALUE)
    );
    assert_eq!(
        set_hostname(&ctx, &too_long),
        SyscallResult::Error(EINVAL_VALUE)
    );
}

#[test]
fn dispatch_proc_self_userns_maps_accept_ltp_setup_sequence() {
    let _setup = setup();
    let process = bootstrap();
    chdir_to_root_with_procfs_mount(&process, 74);
    make_unprivileged(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);

    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWUSER, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWNET, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let write_flags = O_WRONLY | O_CREAT | O_TRUNC;
    let setgroups_fd = open_netns_path(&ctx, b"/proc/self/setgroups", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, setgroups_fd, b"deny"),
        SyscallResult::Return(4)
    );

    let uid_map_fd = open_netns_path(&ctx, b"/proc/self/uid_map", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, uid_map_fd, b"0 1000 1"),
        SyscallResult::Return(8)
    );

    let gid_map_fd = open_netns_path(&ctx, b"/proc/self/gid_map", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, gid_map_fd, b"0 1000 1"),
        SyscallResult::Return(8)
    );

    let userns = process.user_namespace_cap().expect("current userns");
    assert_eq!(
        userns.uid_map_snapshot(),
        alloc::vec![tx_subsystems::process::nsproxy::UserIdMapEntry {
            inside: 0,
            outside: 1000,
            length: 1,
        }]
    );
    assert_eq!(
        userns.gid_map_snapshot(),
        alloc::vec![tx_subsystems::process::nsproxy::UserIdMapEntry {
            inside: 0,
            outside: 1000,
            length: 1,
        }]
    );
    assert_eq!(
        userns.setgroups_policy(),
        tx_subsystems::process::nsproxy::SetgroupsPolicy::Deny
    );
}

#[test]
fn dispatch_proc_self_userns_maps_reject_rewrites_and_gid_before_deny() {
    let _setup = setup();
    let process = bootstrap();
    chdir_to_root_with_procfs_mount(&process, 75);
    make_unprivileged(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);

    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWUSER, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let write_flags = O_WRONLY | O_CREAT | O_TRUNC;
    let gid_map_fd = open_netns_path(&ctx, b"/proc/self/gid_map", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, gid_map_fd, b"0 1000 1"),
        SyscallResult::Error(EPERM_VALUE)
    );

    let uid_map_fd = open_netns_path(&ctx, b"/proc/self/uid_map", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, uid_map_fd, b"0 1000 1"),
        SyscallResult::Return(8)
    );
    let uid_map_fd = open_netns_path(&ctx, b"/proc/self/uid_map", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, uid_map_fd, b"1 1000 1"),
        SyscallResult::Error(EPERM_VALUE)
    );

    let setgroups_fd = open_netns_path(&ctx, b"/proc/self/setgroups", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, setgroups_fd, b"deny"),
        SyscallResult::Return(4)
    );
    let setgroups_fd = open_netns_path(&ctx, b"/proc/self/setgroups", write_flags);
    assert_eq!(
        write_netns_fd(&ctx, setgroups_fd, b"allow"),
        SyscallResult::Error(EPERM_VALUE)
    );
}

#[test]
fn dispatch_setns_clone_newnet_joins_namespace_fd_payload() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let owner = process.user_namespace_cap().expect("setns target owner");
    let target =
        tx_subsystems::net::create_isolated_net_namespace_with_owner("setns-target", Some(owner))
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
fn dispatch_setns_clone_newuts_joins_procfs_namespace_fd_payload() {
    let _setup = setup();
    let parent = bootstrap();
    chdir_to_root_with_procfs_mount(&parent, 77);
    let parent_thread = first_thread(&parent);
    let parent_ctx = make_ctx(parent.clone(), parent_thread);
    assert_eq!(
        set_hostname(&parent_ctx, b"parent-host"),
        SyscallResult::Return(0)
    );

    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
        .expect("fork child process");
    let child_thread = first_thread(&child);
    let child_ctx = make_ctx(child.clone(), child_thread);
    assert_eq!(
        netns_req(NR_UNSHARE, [CLONE_NEWUTS, 0, 0, 0, 0, 0], &child_ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        set_hostname(&child_ctx, b"child-host"),
        SyscallResult::Return(0)
    );

    let path = nul_terminate(alloc::format!("/proc/{}/ns/uts", child.pid.0).as_bytes());
    let fd = match netns_req(
        NR_OPENAT,
        [
            AT_FDCWD as i64 as u64,
            path.as_ptr() as u64,
            O_RDONLY as u64,
            0,
            0,
            0,
        ],
        &parent_ctx,
    ) {
        SyscallResult::Return(fd) if fd >= 0 => fd as u32,
        other => panic!("openat(/proc/<child>/ns/uts) failed: {other:?}"),
    };

    assert_eq!(
        netns_req(NR_SETNS, [fd as u64, CLONE_NEWUTS, 0, 0, 0, 0], &parent_ctx),
        SyscallResult::Return(0)
    );
    let parent_uts = uname_fields(&parent_ctx);
    assert_eq!(uts_field(&parent_uts, 1), b"child-host");
}

#[test]
fn dispatch_setns_clone_newuts_rejects_net_namespace_fd_type() {
    let _setup = setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    let owner = process.user_namespace_cap().expect("setns target owner");
    let target =
        tx_subsystems::net::create_isolated_net_namespace_with_owner("setns-target", Some(owner))
            .expect("target namespace")
            .payload_cap()
            .expect("target namespace payload");
    let file = tx_subsystems::net::net_namespace_open_file_from_payload(target).expect("netns fd");
    process.set_fd(8, Some(file));

    let result = netns_req(NR_SETNS, [8, CLONE_NEWUTS, 0, 0, 0, 0], &ctx);

    assert_eq!(result, SyscallResult::Error(EINVAL_VALUE));
}

#[test]
fn dispatch_openat_proc_self_ns_net_installs_calling_namespace_fd_payload() {
    let _setup = setup();
    let process = bootstrap();
    chdir_to_root_with_procfs_mount(&process, 76);
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
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
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
    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&parent, false, false)
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
