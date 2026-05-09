//! Integration tests over VFS structure + execution dispatch.

use super::*;
use crate::device::{CharDeviceBinding, CharDeviceOps, DevT};
use crate::execution::{Errno, Guard, StepOutcome};
use crate::page_backed::{AnonSwapPolicy, PageContainer, PageContainerKind};
use crate::process::execution::reset_init_process_for_test;
use crate::process::structure::{reset_pid_counter_for_test, Pgid};
use crate::process::{bootstrap_init_process, step_fork, step_setpgid, ProcessIdentity};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::thread_runtime::structure::reset_tid_counter_for_test;
use crate::tty::execution::IoctlSideEffect;
use crate::tty::structure::{Termios, TtyIdentity, TtyKind, TtyPayload, Winsize};
use crate::vm::{AddressSpace, TestPmap};
use crate::zones;
use tx_substrate::zone::{self, Cap, PayloadCap};

struct EchoCharOps;

impl CharDeviceOps for EchoCharOps {
    fn read(&self, out: &mut [u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
        if out.is_empty() {
            return StepOutcome::Done(0);
        }
        out[0] = b'R';
        StepOutcome::Done(1)
    }

    fn write(&self, bytes: &[u8], _guard: &Guard<'_>) -> StepOutcome<usize> {
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
    tx_substrate::testing::init_host_for_test_once();
    let _ = zones::register_all();
    crate::tty::structure::registry::reset_for_tests();
}

fn setup_process_world() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_substrate::testing::init_host_for_test_once();
    let _ = zones::register_all();
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
    let _ = tx_substrate::epoch::drain_with_budget(usize::MAX);
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
    let id_res = zone::reserve_for::<TtyIdentity>().expect("tty identity reservation");
    let payload_res = zone::reserve_for::<TtyPayload>().expect("tty payload reservation");
    let payload = PayloadCap::from_cap(zone::sign_for(payload_res, payload));
    let identity = zone::sign_for(id_res, TtyIdentity::new(kind, index, name));
    identity.install_payload(payload);
    identity
}

#[test]
fn inline_name_rejects_empty_slash_and_oversized_names() {
    assert_eq!(InlineName::new(b"etc").unwrap().as_bytes(), b"etc");
    assert_eq!(InlineName::new(b""), Err(Errno::ENAMETOOLONG));
    assert_eq!(InlineName::new(b"a/b"), Err(Errno::ENAMETOOLONG));
    assert_eq!(
        InlineName::new(&[b'x'; VFS_NAME_MAX + 1]),
        Err(Errno::ENAMETOOLONG)
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
    let guard = tx_substrate::epoch::guard();
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
        },
    );
    let mut out = [0u8; 8];

    assert!(matches!(
        tty_file.step_read(&mut out, &guard),
        StepOutcome::Blocked(_)
    ));
    {
        use tx_substrate::step_v3::StepOutcome as V3Out;
        assert_eq!(
            crate::tty::execution::step_ingest(&tty, b"ok\n", &guard),
            V3Out::Done(crate::tty::execution::IngestOutcome {
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
        },
    );
    let caller = OpenFileIoctlCaller::from_process(&init);

    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &tx_substrate::epoch::guard()),
        StepOutcome::Done(OpenFileIoctlResult::Termios(Termios::default_cooked()))
    );

    let raw = Termios::zeroed();
    assert_eq!(
        tty_file.step_ioctl(
            caller,
            OpenFileIoctl::Tcsets { termios: raw },
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(Default::default()))
    );
    assert_eq!(
        tty_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &tx_substrate::epoch::guard()),
        StepOutcome::Done(OpenFileIoctlResult::Termios(raw))
    );

    let winsize = Winsize::new(40, 100);
    assert_eq!(
        tty_file.step_ioctl(
            caller,
            OpenFileIoctl::Tiocswinsz { winsize },
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(
            caller,
            OpenFileIoctl::Tiocgwinsz,
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::Winsize(winsize))
    );
}

#[test]
fn open_file_step_ioctl_dispatches_process_aware_tty_session_ops() {
    let _g = setup_process_world();
    let init = fresh_init();
    let peer = step_fork::<TestPmap>(&init).expect("fork");
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
        },
    );
    let init_caller = OpenFileIoctlCaller::from_process(&init);

    assert_eq!(
        tty_file.step_ioctl(
            init_caller,
            OpenFileIoctl::Tiocsctty,
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(
            init_caller,
            OpenFileIoctl::Tiocgpgrp,
            &tx_substrate::epoch::guard()
        ),
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
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::SideEffect(IoctlSideEffect {
            session_ctl_fired: true,
            signal: None,
        }))
    );
    assert_eq!(
        tty_file.step_ioctl(
            init_caller,
            OpenFileIoctl::Tiocgpgrp,
            &tx_substrate::epoch::guard()
        ),
        StepOutcome::Done(OpenFileIoctlResult::Pgrp(peer_pgrp.pgid.0))
    );

    assert_eq!(
        tty_file.step_ioctl(
            init_caller,
            OpenFileIoctl::Tiocnotty,
            &tx_substrate::epoch::guard()
        ),
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
        },
    );
    assert_eq!(
        char_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &tx_substrate::epoch::guard()),
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
        },
    );
    assert_eq!(
        dir_file.step_ioctl(caller, OpenFileIoctl::Tcgets, &tx_substrate::epoch::guard()),
        StepOutcome::Err(Errno::EISDIR)
    );
}
