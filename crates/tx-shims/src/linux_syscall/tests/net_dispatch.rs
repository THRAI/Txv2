use super::*;
use crate::adapter::step_engine::{reserve_for, sign_for};
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};

const AF_INET: u64 = 2;
const SOCK_DGRAM: u64 = 2;

fn path_only_regular_file() -> Cap<OpenFile> {
    let rnode = {
        let raw = RNode::new(
            FsObjectId::new(10_700),
            InodeMeta::new(InodeKind::Directory, 0o040755),
            RNodeBacking::Directory,
        );
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };
    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: false,
            write: false,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
    .expect("path-only open file cap")
}

#[test]
fn dispatch_accept_on_open_non_socket_fd_returns_enotsock() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let pid = proc_cap.pid.0 as u64;
    let ctx = make_ctx(proc_cap, thread);

    let pidfd = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [pid, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    let fd = match pidfd {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("pidfd_open(self) failed: {other:?}"),
    };

    let accept = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_ACCEPT, [fd, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(accept, SyscallResult::Error(ENOTSOCK_VALUE));
}

#[test]
fn dispatch_accept_on_path_only_fd_returns_ebadf() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(3, Some(path_only_regular_file()));
    let ctx = make_ctx(proc_cap, thread);

    let accept = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_ACCEPT, [3, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(accept, SyscallResult::Error(EBADF_VALUE));
}

#[test]
fn dispatch_accept_on_udp_socket_returns_eopnotsupp() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let socket = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SOCKET, [AF_INET, SOCK_DGRAM, 0, 0, 0, 0]),
        &ctx,
    ));
    let fd = match socket {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("socket(AF_INET, SOCK_DGRAM) failed: {other:?}"),
    };

    let accept = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_ACCEPT, [fd, 0, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(accept, SyscallResult::Error(EOPNOTSUPP_VALUE));
}

#[test]
fn dispatch_close_releases_fake_socket_fd() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let socket = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SOCKET, [AF_INET, SOCK_DGRAM, 0, 0, 0, 0]),
        &ctx,
    ));
    let fd = match socket {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("socket(AF_INET, SOCK_DGRAM) failed: {other:?}"),
    };

    let first_close = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE, [fd, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(first_close, SyscallResult::Return(0));

    let second_close = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE, [fd, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(second_close, SyscallResult::Error(EBADF_VALUE));
}
