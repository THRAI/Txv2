use super::*;

#[test]
fn dispatch_unix_datagram_sendmsg_reaches_bound_peer() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_unix_dgram(&ctx);
    let client_fd = socket_unix_dgram(&ctx);
    let server_addr = sockaddr_un(b"ux_dgram_sendmsg");

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"unix-dgram";
    let send_iov = TestIovec {
        base: payload.as_ptr() as u64,
        len: payload.len() as u64,
    };
    let mut send_hdr = TestMsghdr {
        name: server_addr.as_ptr() as u64,
        namelen: SOCKADDR_UN_BYTES,
        _pad0: 0,
        iov: (&send_iov as *const TestIovec) as u64,
        iovlen: 1,
        control: 0,
        controllen: 0,
        flags: 0,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_SENDMSG,
            [
                client_fd as u64,
                (&mut send_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let mut out = [0u8; 32];
    let recv_iov = TestIovec {
        base: out.as_mut_ptr() as u64,
        len: out.len() as u64,
    };
    let mut recv_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: (&recv_iov as *const TestIovec) as u64,
        iovlen: 1,
        control: 0,
        controllen: 0,
        flags: 0,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_RECVMSG,
            [
                server_fd as u64,
                (&mut recv_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );
    assert_eq!(&out[..payload.len()], &payload);
}

#[test]
fn dispatch_unix_sendmsg_invalid_control_pointer_returns_efault() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_unix_dgram(&ctx);
    let client_fd = socket_unix_dgram(&ctx);
    let server_addr = sockaddr_un(b"ux_dgram_control");

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"x";
    let send_iov = TestIovec {
        base: payload.as_ptr() as u64,
        len: payload.len() as u64,
    };
    let mut send_hdr = TestMsghdr {
        name: server_addr.as_ptr() as u64,
        namelen: SOCKADDR_UN_BYTES,
        _pad0: 0,
        iov: (&send_iov as *const TestIovec) as u64,
        iovlen: 1,
        control: u64::MAX,
        controllen: 16,
        flags: 0,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_SENDMSG,
            [
                client_fd as u64,
                (&mut send_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EFAULT_VALUE)
    );
}

#[test]
fn dispatch_unix_stream_connect_accept_and_sendmsg_round_trips() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_unix_stream(&ctx);
    let client_fd = socket_unix_stream(&ctx);
    let listener_addr = sockaddr_un(b"ux_stream_sendmsg");

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_LISTEN, [listener_fd as u64, 10, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                listener_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut peer_addr = [0u8; SOCKADDR_UN_BYTES as usize];
    let mut peer_len = SOCKADDR_UN_BYTES;
    let accepted_fd = match socket_req(
        NR_ACCEPT,
        [
            listener_fd as u64,
            peer_addr.as_mut_ptr() as u64,
            (&mut peer_len as *mut u32) as u64,
            0,
            0,
            0,
        ],
        &ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("accept(AF_UNIX) failed: {other:?}"),
    };
    assert_eq!(peer_len, 2);
    assert_eq!(u16::from_le_bytes([peer_addr[0], peer_addr[1]]), AF_UNIX);

    let payload = *b"unix-stream";
    let send_iov = TestIovec {
        base: payload.as_ptr() as u64,
        len: payload.len() as u64,
    };
    let mut send_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: (&send_iov as *const TestIovec) as u64,
        iovlen: 1,
        control: 0,
        controllen: 0,
        flags: 0,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_SENDMSG,
            [
                accepted_fd as u64,
                (&mut send_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let mut out = [0u8; 32];
    let recv_iov = TestIovec {
        base: out.as_mut_ptr() as u64,
        len: out.len() as u64,
    };
    let mut recv_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: (&recv_iov as *const TestIovec) as u64,
        iovlen: 1,
        control: 0,
        controllen: 0,
        flags: 0,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_RECVMSG,
            [
                client_fd as u64,
                (&mut recv_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );
    assert_eq!(&out[..payload.len()], &payload);
}

#[test]
fn dispatch_unix_pathname_survives_close_until_unlink() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_unix_stream(&ctx);
    let path = b"ux_path_lifecycle";
    let unix_path = UnixSocketPath::new(path).expect("valid AF_UNIX path");
    let addr = sockaddr_un(path);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let net_namespace = ctx.process.net_namespace().expect("test net namespace");
    let table = net_namespace.socket_table();
    let guard = step_engine::guard();
    assert!(table.lookup_unix_path_node(unix_path, &guard));
    assert!(table.lookup_unix_bound(unix_path, &guard).is_none());
    drop(guard);

    assert!(table.unlink_unix_path(unix_path).is_ok());
    assert!(table.unlink_unix_path(unix_path).is_err());
}

#[test]
fn dispatch_close_releases_bound_socket_port() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let addr = sockaddr_in([127, 0, 0, 1], 49_102);

    let first_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                first_fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_CLOSE, [first_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let second_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                second_fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_close_keeps_duplicated_bound_socket_port_until_last_fd() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let addr = sockaddr_in([127, 0, 0, 1], 49_103);

    let first_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                first_fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let dup_fd = match socket_req(NR_DUP, [first_fd as u64, 0, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("dup(socket) failed: {other:?}"),
    };
    assert_eq!(
        socket_req(NR_CLOSE, [first_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let competing_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                competing_fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EADDRINUSE))
    );
    assert_eq!(
        socket_req(NR_CLOSE, [dup_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                competing_fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
}
