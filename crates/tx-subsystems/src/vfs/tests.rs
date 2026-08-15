//! Integration tests over VFS structure + execution dispatch.

use super::*;
use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Errno as V4Errno, Guard};
use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, Pgid};
use crate::process::{bootstrap_init_process, step_fork, step_setpgid, ProcessIdentity};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::tty::execution::IoctlSideEffect;
use crate::tty::structure::{Termios, TtyIdentity, TtyKind, TtyPayload, Winsize};
use crate::vfs::adapter::step_engine::{
    guard, reserve_for, sign_for, ByteProgress, Cap, Errno, PayloadCap, StepOutcome,
};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;

struct EchoCharOps;

impl CharDeviceOps for EchoCharOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        if out.is_empty() {
            return StepOutcome::Done(0);
        }
        out[0] = b'R';
        StepOutcome::Done(1)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress> {
        StepOutcome::Done(bytes.len())
    }
}

static ECHO_CHAR_OPS: EchoCharOps = EchoCharOps;
static ECHO_CHAR_BINDING: CharDeviceBinding = CharDeviceBinding {
    devt: DevT::new(240, 0),
    name: "echo-char",
    ops: &ECHO_CHAR_OPS,
};

fn init_tty_zones() {
    tx_test_support::init_host();
    let _ = zones::register_all();
    crate::tty::structure::registry::reset_for_tests();
}

fn setup_process_world() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    crate::tty::structure::registry::reset_for_tests();
    reset_pid_counter_for_test();
    reset_tid_counter_for_test();
    reset_init_process_for_test();
    guard
}

fn fresh_init() -> Cap<ProcessIdentity> {
    bootstrap_init_process(AddressSpace::new_cap_for_platform::<TestPmap>().expect("aspace"))
        .expect("init")
}

fn alloc_tty(kind: TtyKind, index: u32, name: &str, payload: TtyPayload) -> Cap<TtyIdentity> {
    let id_res = reserve_for::<TtyIdentity>().expect("tty identity reservation");
    let payload_res = reserve_for::<TtyPayload>().expect("tty payload reservation");
    let payload = PayloadCap::from_cap(sign_for(payload_res, payload));
    let identity = sign_for(id_res, TtyIdentity::new(kind, index, name));
    identity.install_payload(payload);
    identity
}

#[test]
fn inline_name_rejects_empty_slash_and_oversized_names() {
    assert_eq!(InlineName::new(b"etc").unwrap().as_bytes(), b"etc");
    assert_eq!(InlineName::new(b""), Err(V4Errno::ENAMETOOLONG));
    assert_eq!(InlineName::new(b"a/b"), Err(V4Errno::ENAMETOOLONG));
    assert_eq!(
        InlineName::new(&[b'x'; VFS_NAME_MAX + 1]),
        Err(V4Errno::ENAMETOOLONG)
    );
}

#[test]
fn rnode_backing_uses_page_container_cap_without_backend_live_nodes() {
    let _g = setup_process_world();
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Persistent,
        },
        8,
    )
    .expect("page container");
    let rnode = RNode::new(
        FsObjectId::new(42),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc: pc.clone() },
    );

    assert_eq!(rnode.fs_object_id(), FsObjectId::new(42));
    assert!(matches!(rnode.backing(), RNodeBacking::PageBacked { pc: r_pc } if *r_pc == pc));
}

#[test]
fn positive_dentry_cache_survives_external_child_close_until_invalidation() {
    let _g = setup_process_world();

    let root_rnode = RNode::new_cap(
        FsObjectId::new(90),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("root rnode");
    let root = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    let name = InlineName::new(b"cached").expect("child name");
    let child_rnode = RNode::new_cap(
        FsObjectId::new(91),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::Directory,
    )
    .expect("child rnode");
    let mut child_raw = DEntry::new(name, child_rnode);
    child_raw.set_parent_hint(&root);
    let child = sign_for(
        reserve_for::<DEntry>().expect("child dentry reservation"),
        child_raw,
    );
    let weak_child = child.downgrade();
    root.cache_child(child.clone());
    drop(child);
    tx_test_support::drain_to_quiescence();

    assert!(root.cached_child(name).is_some());
    {
        let guard = guard();
        assert!(weak_child.upgrade(&guard).is_some());
    }

    root.remove_cached_child(name);
    tx_test_support::drain_to_quiescence();
    let guard = guard();
    assert!(weak_child.upgrade(&guard).is_none());
}

#[test]
fn cached_removed_directory_subtree_does_not_retain_parent_cycle() {
    let _g = setup_process_world();

    let root_rnode = RNode::new_cap(
        FsObjectId::new(100),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("root rnode");
    let root = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");

    let child_rnode = RNode::new_cap(
        FsObjectId::new(101),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("child rnode");
    let mut child_raw = DEntry::new(InlineName::new(b"child").unwrap(), child_rnode);
    child_raw.set_parent_hint(&root);
    let child = sign_for(
        reserve_for::<DEntry>().expect("child dentry reservation"),
        child_raw,
    );
    root.cache_child(child.clone());

    let grand_rnode = RNode::new_cap(
        FsObjectId::new(102),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("grandchild rnode");
    let mut grand_raw = DEntry::new(InlineName::new(b"grand").unwrap(), grand_rnode);
    grand_raw.set_parent_hint(&child);
    let grand = sign_for(
        reserve_for::<DEntry>().expect("grandchild dentry reservation"),
        grand_raw,
    );
    child.cache_child(grand.clone());

    let weak_child = child.downgrade();
    let weak_grand = grand.downgrade();
    root.remove_cached_child(InlineName::new(b"child").unwrap());
    drop(grand);
    drop(child);
    tx_test_support::drain_to_quiescence();

    let guard = guard();
    assert!(weak_child.upgrade(&guard).is_none());
    assert!(weak_grand.upgrade(&guard).is_none());
}

#[test]
fn duplicate_child_publication_returns_existing_canonical_identity() {
    let _g = setup_process_world();

    let root_rnode = RNode::new_cap(
        FsObjectId::new(200),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("root rnode");
    let root = DEntry::new_cap(InlineName::ROOT, root_rnode).expect("root dentry");
    let name = InlineName::new(b"same").expect("child name");

    let first_rnode = RNode::new_cap(
        FsObjectId::new(201),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::Directory,
    )
    .expect("first rnode");
    let mut first_raw = DEntry::new(name, first_rnode);
    first_raw.set_parent_hint(&root);
    let first = sign_for(
        reserve_for::<DEntry>().expect("first dentry reservation"),
        first_raw,
    );

    let second_rnode = RNode::new_cap(
        FsObjectId::new(202),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::Directory,
    )
    .expect("second rnode");
    let mut second_raw = DEntry::new(name, second_rnode);
    second_raw.set_parent_hint(&root);
    let second = sign_for(
        reserve_for::<DEntry>().expect("second dentry reservation"),
        second_raw,
    );

    let first_published = root.cache_child(first.clone());
    let second_published = root.cache_child(second.clone());

    assert_eq!(first_published.key(), first.key());
    assert_eq!(second_published.key(), first.key());
    assert_ne!(second_published.key(), second.key());
    assert_eq!(
        root.cached_child(name)
            .expect("cached canonical child")
            .key(),
        first.key()
    );
    assert_eq!(
        second_published.rnode().fs_object_id(),
        FsObjectId::new(201)
    );
}

#[test]
fn rnode_backing_carries_tty_identity_payload() {
    let _g = setup_process_world();
    init_tty_zones();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        0,
        "ttyS0",
        TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
    );
    let rnode = RNode::new(
        FsObjectId::new(43),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty.clone()),
        },
    );

    assert!(matches!(
        rnode.backing(),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(r_tty)
        } if *r_tty == tty
    ));
}

#[test]
fn open_file_dispatches_struct_payload_read_write() {
    let _g = setup_process_world();
    init_tty_zones();
    let guard = guard();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        1,
        "ttyS1",
        TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
    );
    let tty_rnode = RNode::new_cap(
        FsObjectId::new(44),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty.clone()),
        },
    )
    .expect("tty rnode");
    let tty_file = OpenFile::new(
        tty_rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    let mut out = [0u8; 8];

    assert!(matches!(
        tty_file.step_read(&mut out, &guard),
        StepOutcome::Yield { .. }
    ));
    {
        assert_eq!(
            crate::tty::execution::step_ingest_with_post(
                &tty,
                b"ok\n",
                &guard,
                |mailbox, event, hint| mailbox.post_with_scheduler_hint(event, hint),
            ),
            StepOutcome::Done(crate::tty::execution::IngestOutcome {
                consumed: 3,
                readable_fired: true,
                writable_fired: true,
                ..Default::default()
            })
        );
    }
    assert_eq!(tty_file.step_read(&mut out, &guard), StepOutcome::Done(3));
    assert_eq!(&out[..3], b"ok\n");
    assert_eq!(tty_file.step_write(b"x", &guard), StepOutcome::Done(1));

    let char_rnode = RNode::new_cap(
        FsObjectId::new(45),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::CharDevice(&ECHO_CHAR_BINDING),
        },
    )
    .expect("char rnode");
    let char_file = OpenFile::new(
        char_rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    let mut char_out = [0u8; 1];

    assert_eq!(
        char_file.step_read(&mut char_out, &guard),
        StepOutcome::Done(1)
    );
    assert_eq!(char_out, [b'R']);
    assert_eq!(char_file.step_write(b"abc", &guard), StepOutcome::Done(3));
}

#[test]
fn fd_ready_facade_reports_eventfd_readiness_and_waits() {
    let _g = setup_process_world();
    let efd = crate::eventfd::eventfd_create(7, 0).expect("eventfd");
    let file = OpenFile::new_eventfd_cap(
        efd.clone(),
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("eventfd file");

    let guard = guard();
    let report = crate::vfs::fd_ready::query_fd_ready(
        crate::vfs::fd_ready::FdReadyQuery {
            file: &file,
            interest: crate::vfs::fd_ready::FdReadyMask::READ
                | crate::vfs::fd_ready::FdReadyMask::WRITE,
            now_monotonic_ns: None,
        },
        &guard,
    );

    assert!(report
        .ready
        .contains(crate::vfs::fd_ready::FdReadyMask::READ));
    assert!(report
        .ready
        .contains(crate::vfs::fd_ready::FdReadyMask::WRITE));
    assert!(report.epoll_watchable);
    assert!(report
        .waits
        .iter()
        .any(|wait| wait.source.raw() == efd.reader_source_id()));
    assert!(report
        .waits
        .iter()
        .any(|wait| wait.source.raw() == efd.writer_source_id()));
    let reader_wait = report
        .waits
        .iter()
        .find(|wait| wait.source.raw() == efd.reader_source_id())
        .expect("reader wait");
    assert_eq!(
        reader_wait
            .endpoint()
            .map(tx_substrate::wake::WaitEndpoint::source_id),
        Some(tx_substrate::step::WaitSourceId::new(
            efd.reader_source_id()
        ))
    );
    let writer_wait = report
        .waits
        .iter()
        .find(|wait| wait.source.raw() == efd.writer_source_id())
        .expect("writer wait");
    assert_eq!(
        writer_wait
            .endpoint()
            .map(tx_substrate::wake::WaitEndpoint::source_id),
        Some(tx_substrate::step::WaitSourceId::new(
            efd.writer_source_id()
        ))
    );
}

// ============================================================
// DAC + setuid Wave 1: Credential type extension
// ============================================================

#[test]
fn credential_default_is_non_root_unprivileged() {
    let cred = Credential::default();
    // uid happens to be 0 (the u32 Default), but the unprivileged
    // semantic is encoded in the empty capability set.
    assert_eq!(cred.uid, 0);
    assert_eq!(cred.gid, 0);
    assert_eq!(cred.effective_caps, CapabilitySet::EMPTY);
}

#[test]
fn credential_root_has_all_caps() {
    let cred = Credential::root();
    assert_eq!(cred.uid, 0);
    assert_eq!(cred.gid, 0);
    assert_eq!(cred.effective_caps, CapabilitySet::FULL);
}

#[test]
fn open_file_step_ioctl_dispatches_basic_tty_requests() {
    let _g = setup_process_world();
    let init = fresh_init();
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        2,
        "ttyS2",
        TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
    );
    let tty_rnode = RNode::new_cap(
        FsObjectId::new(46),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty.clone()),
        },
    )
    .expect("tty rnode");
    let tty_file = OpenFile::new(
        tty_rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    let caller = OpenFileIoctlCaller::from_process(&init);

    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::Termios(Termios::default_cooked()))
    );

    let raw = Termios::zeroed();
    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tcsets { termios: raw }, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(Default::default()))
    );
    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::Termios(raw))
    );

    let winsize = Winsize::new(40, 100);
    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tiocswinsz { winsize }, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tiocgwinsz, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::Winsize(winsize))
    );
}

#[test]
fn open_file_step_ioctl_dispatches_process_aware_tty_session_ops() {
    let _g = setup_process_world();
    let init = fresh_init();
    let peer = step_fork::<TestPmap>(&init, false, false).expect("fork");
    let tty = alloc_tty(
        TtyKind::SerialHardware,
        3,
        "ttyS3",
        TtyPayload::new_hardware(&ECHO_CHAR_BINDING),
    );
    let tty_rnode = RNode::new_cap(
        FsObjectId::new(47),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty.clone()),
        },
    )
    .expect("tty rnode");
    let tty_file = OpenFile::new(
        tty_rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    let init_caller = OpenFileIoctlCaller::from_process(&init);

    assert_eq!(
        tty_file.step_ioctl(init_caller, OpenFileIoctl::Tiocsctty, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(init_caller, OpenFileIoctl::Tiocgpgrp, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::Pgrp(init.pgrp_cap().pgid.0))
    );
    assert!(init.pgrp_cap().session_cap().has_controlling_tty());

    step_setpgid(&peer, Pgid(peer.pid.0)).expect("peer gets own pgrp");
    let peer_pgrp = peer.pgrp_cap();
    assert_eq!(
        tty_file.step_ioctl(
            init_caller,
            OpenFileIoctl::Tiocspgrp {
                new_pgrp: &peer_pgrp
            },
            &guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(init_caller, OpenFileIoctl::Tiocgpgrp, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::Pgrp(peer_pgrp.pgid.0))
    );

    assert_eq!(
        tty_file.step_ioctl(init_caller, OpenFileIoctl::Tiocnotty, &guard()),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert!(!init.pgrp_cap().session_cap().has_controlling_tty());
}

#[test]
fn open_file_step_ioctl_rejects_non_tty_backings() {
    let _g = setup_process_world();
    let init = fresh_init();
    let caller = OpenFileIoctlCaller::from_process(&init);

    let char_rnode = RNode::new_cap(
        FsObjectId::new(48),
        InodeMeta::new(InodeKind::CharDevice, 0o020600),
        RNodeBacking::StructBacked {
            payload: StructPayload::CharDevice(&ECHO_CHAR_BINDING),
        },
    )
    .expect("char rnode");
    let char_file = OpenFile::new(
        char_rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    assert_eq!(
        char_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &guard()),
        StepOutcome::Err(Errno::ENOSYS)
    );

    let dir_rnode = RNode::new_cap(
        FsObjectId::new(49),
        InodeMeta::new(InodeKind::Directory, 0o040755),
        RNodeBacking::Directory,
    )
    .expect("dir rnode");
    let dir_file = OpenFile::new(
        dir_rnode,
        OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    );
    assert_eq!(
        dir_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &guard()),
        StepOutcome::Err(Errno::EISDIR)
    );
}
