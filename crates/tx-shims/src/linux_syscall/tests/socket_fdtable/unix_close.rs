use super::*;

#[test]
fn dispatch_socketpair_reports_linux_error_ordering() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let mut fds = [-1i32; 2];

    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [0, SOCK_STREAM, 0, fds.as_mut_ptr() as u64, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EAFNOSUPPORT))
    );
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [AF_INET as u64, 75, 0, fds.as_mut_ptr() as u64, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EINVAL))
    );
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_INET as u64,
                SOCK_DGRAM,
                IPPROTO_UDP as u64,
                fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP))
    );
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_INET as u64,
                SOCK_DGRAM,
                IPPROTO_TCP as u64,
                fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EPROTONOSUPPORT))
    );
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [AF_UNIX as u64, SOCK_STREAM, 0, 0, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(EFAULT_VALUE)
    );
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [AF_UNIX as u64, SOCK_STREAM, 0, 7, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(EFAULT_VALUE)
    );
}

#[test]
fn dispatch_socketpair_stream_and_datagram_round_trip() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let mut stream_fds = [-1i32; 2];
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_UNIX as u64,
                SOCK_STREAM,
                0,
                stream_fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let stream_payload = *b"stream-pair";
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                stream_fds[0] as u64,
                stream_payload.as_ptr() as u64,
                stream_payload.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(stream_payload.len() as i64)
    );
    let mut stream_out = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                stream_fds[1] as u64,
                stream_out.as_mut_ptr() as u64,
                stream_out.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(stream_payload.len() as i64)
    );
    assert_eq!(&stream_out[..stream_payload.len()], &stream_payload);

    let mut dgram_fds = [-1i32; 2];
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_UNIX as u64,
                SOCK_DGRAM,
                0,
                dgram_fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let dgram_payload = *b"dgram-pair";
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                dgram_fds[0] as u64,
                dgram_payload.as_ptr() as u64,
                dgram_payload.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(dgram_payload.len() as i64)
    );
    let mut dgram_out = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                dgram_fds[1] as u64,
                dgram_out.as_mut_ptr() as u64,
                dgram_out.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(dgram_payload.len() as i64)
    );
    assert_eq!(&dgram_out[..dgram_payload.len()], &dgram_payload);
}

#[test]
fn dispatch_socketpair_sets_cloexec_and_nonblock_on_both_fds() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();

    let mut cloexec_fds = [-1i32; 2];
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_UNIX as u64,
                SOCK_STREAM | O_CLOEXEC as u64,
                0,
                cloexec_fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    for fd in cloexec_fds {
        assert_eq!(
            socket_req(NR_FCNTL, [fd as u64, F_GETFD as u64, 0, 0, 0, 0], &ctx),
            SyscallResult::Return(FD_CLOEXEC as i64)
        );
    }

    let mut nonblock_fds = [-1i32; 2];
    assert_eq!(
        socket_req(
            NR_SOCKETPAIR,
            [
                AF_UNIX as u64,
                SOCK_STREAM | O_NONBLOCK as u64,
                0,
                nonblock_fds.as_mut_ptr() as u64,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    for fd in nonblock_fds {
        assert_eq!(
            socket_req(NR_FCNTL, [fd as u64, F_GETFL as u64, 0, 0, 0, 0], &ctx),
            SyscallResult::Return((O_RDWR | O_NONBLOCK) as i64)
        );
    }
}

#[test]
fn dispatch_unix_getsockname_reports_bound_path_addresses() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();

    let pathname_fd = socket_unix_stream(&ctx);
    let pathname = b"ux_getsockname";
    let pathname_addr = sockaddr_un(pathname);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                pathname_fd as u64,
                pathname_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let mut out = [0u8; SOCKADDR_UN_BYTES as usize];
    let mut out_len = SOCKADDR_UN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                pathname_fd as u64,
                out.as_mut_ptr() as u64,
                (&mut out_len as *mut u32) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out_len, 2 + pathname.len() as u32 + 1);
    assert_eq!(u16::from_le_bytes([out[0], out[1]]), AF_UNIX);
    assert_eq!(&out[2..2 + pathname.len()], pathname);

    let abstract_fd = socket_unix_dgram(&ctx);
    let abstract_name = b"\0ux_getsockname";
    let abstract_addr = sockaddr_un(abstract_name);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                abstract_fd as u64,
                abstract_addr.as_ptr() as u64,
                SOCKADDR_UN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let mut abstract_out = [0u8; SOCKADDR_UN_BYTES as usize];
    let mut abstract_out_len = SOCKADDR_UN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                abstract_fd as u64,
                abstract_out.as_mut_ptr() as u64,
                (&mut abstract_out_len as *mut u32) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(abstract_out_len, SOCKADDR_UN_BYTES);
    assert_eq!(
        u16::from_le_bytes([abstract_out[0], abstract_out[1]]),
        AF_UNIX
    );
    assert_eq!(&abstract_out[2..2 + abstract_name.len()], abstract_name);
}

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
fn dispatch_unix_datagram_recvfrom_reports_bound_source_address() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_unix_dgram(&ctx);
    let peer_fd = socket_unix_dgram(&ctx);
    let server_path = b"ux_dgram_recvfrom_server";
    let peer_path = b"ux_dgram_recvfrom_peer";
    let server_addr = sockaddr_un(server_path);
    let peer_addr = sockaddr_un(peer_path);

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
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                peer_fd as u64,
                peer_addr.as_ptr() as u64,
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
        socket_req(
            NR_CONNECT,
            [
                peer_fd as u64,
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

    let payload = *b"unix-recvfrom";
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                peer_fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let mut inbound = [0u8; 32];
    let mut source_addr = [0u8; SOCKADDR_UN_BYTES as usize];
    let mut source_len = SOCKADDR_UN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                inbound.as_mut_ptr() as u64,
                inbound.len() as u64,
                0,
                source_addr.as_mut_ptr() as u64,
                (&mut source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );
    assert_eq!(&inbound[..payload.len()], &payload);
    assert_eq!(source_len, 2 + peer_path.len() as u32 + 1);
    assert_eq!(
        u16::from_le_bytes([source_addr[0], source_addr[1]]),
        AF_UNIX
    );
    assert_eq!(&source_addr[2..2 + peer_path.len()], peer_path);

    let reply = *b"unix-reply";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                server_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                source_addr.as_ptr() as u64,
                source_len as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );

    let mut echoed = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                peer_fd as u64,
                echoed.as_mut_ptr() as u64,
                echoed.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(&echoed[..reply.len()], &reply);
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

#[repr(C)]
#[derive(Debug, Default)]
struct TestUcred {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[test]
fn dispatch_unix_stream_accepted_socket_reports_peercred() {
    let _setup = socket_setup();
    let (server_process, server_ctx) = socket_ctx();
    let client_process =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&server_process, false, false)
            .expect("fork client");
    set_cred_ids_for_test(&client_process, 2000, 2000, 2000, 3000, 3000, 3000);
    let client_thread = first_thread(&client_process);
    let client_ctx = make_ctx(client_process.clone(), client_thread);
    let listener_fd = socket_unix_stream(&server_ctx);
    let client_fd = socket_unix_stream(&client_ctx);
    let listener_addr = sockaddr_un(b"ux_stream_peercred");

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
            &server_ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_LISTEN, [listener_fd as u64, 10, 0, 0, 0, 0], &server_ctx,),
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
            &client_ctx,
        ),
        SyscallResult::Return(0)
    );

    let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &server_ctx)
    {
        SyscallResult::Return(fd) => fd,
        other => panic!("accept(AF_UNIX) failed: {other:?}"),
    };
    let mut cred = TestUcred::default();
    let mut optlen = core::mem::size_of::<TestUcred>() as u32;

    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                accepted_fd as u64,
                SOL_SOCKET as u64,
                SO_PEERCRED as u64,
                (&mut cred as *mut TestUcred) as u64,
                (&mut optlen as *mut u32) as u64,
                0,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(optlen, core::mem::size_of::<TestUcred>() as u32);
    assert_eq!(cred.pid, client_process.pid.0 as i32);
    assert_eq!(cred.uid, 2000);
    assert_eq!(cred.gid, 3000);
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
