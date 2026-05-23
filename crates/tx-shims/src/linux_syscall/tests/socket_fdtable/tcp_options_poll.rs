use super::*;

fn dispatch_bind_listen_getsockname_round_trips_inet_addr() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);

    let addr = sockaddr_in([127, 0, 0, 1], 49_101);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
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
        socket_req(NR_LISTEN, [fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut out_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                fd as u64,
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
    assert_eq!(out_len, SOCKADDR_IN_BYTES);
    assert_eq!(u16::from_le_bytes([out[0], out[1]]), AF_INET);
    assert_eq!(u16::from_be_bytes([out[2], out[3]]), 49_101);
    assert_eq!(&out[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_tcp_listener_survives_parent_close_after_fork() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listen_addr = sockaddr_in([0, 0, 0, 0], 49_111);
    let connect_addr = sockaddr_in([0, 0, 0, 0], 49_111);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listen_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let child = tx_subsystems::process::step_fork::<ShimsTestPmap>(&process, false, false)
        .expect("fork child");
    assert!(
        child.fd(listener_fd as u32).is_some(),
        "fork must inherit the listening fd"
    );
    assert_eq!(
        socket_req(NR_CLOSE, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                connect_addr.as_ptr() as u64,
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
fn dispatch_tcp_autobind_skips_wildcard_listener_port() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([0, 0, 0, 0], 49_152);
    let connect_addr = sockaddr_in([0, 0, 0, 0], 49_152);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                connect_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut client_name = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut client_name_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                client_fd as u64,
                client_name.as_mut_ptr() as u64,
                (&mut client_name_len as *mut u32) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let client_port = u16::from_be_bytes([client_name[2], client_name[3]]);
    assert_ne!(client_port, 49_152);
    assert!((49_152..49_216).contains(&client_port));
    assert_eq!(&client_name[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_tcp_connects_to_loopback_ephemeral_listener() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([127, 0, 0, 1], 0);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 1, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let mut listener_name = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut listener_name_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                listener_fd as u64,
                listener_name.as_mut_ptr() as u64,
                (&mut listener_name_len as *mut u32) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(&listener_name[4..8], &[127, 0, 0, 1]);

    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                listener_name.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("accept failed: {other:?}"),
    };
    assert_eq!(
        socket_req(NR_CLOSE, [client_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_CLOSE, [accepted_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    let second_client_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                second_client_fd as u64,
                listener_name.as_ptr() as u64,
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
fn dispatch_tcp_accept_write_survives_client_close_without_drain() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([0, 0, 0, 0], 49_153);
    let connect_addr = sockaddr_in([0, 0, 0, 0], 49_153);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );

    for _ in 0..3 {
        let client_fd = socket_stream(&ctx, SOCK_STREAM);
        assert_eq!(
            socket_req(
                NR_CONNECT,
                [
                    client_fd as u64,
                    connect_addr.as_ptr() as u64,
                    SOCKADDR_IN_BYTES as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );

        let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx) {
            SyscallResult::Return(fd) => fd as u32,
            other => panic!("accept failed: {other:?}"),
        };
        let greeting = *b"hoser\n";
        assert_eq!(
            socket_req(
                NR_WRITE,
                [
                    accepted_fd as u64,
                    greeting.as_ptr() as u64,
                    greeting.len() as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(greeting.len() as i64)
        );

        let mut readfds = 1u64 << client_fd;
        let mut timeout = [0u64, 0u64];
        assert_eq!(
            socket_req(
                NR_PSELECT6,
                [
                    client_fd as u64 + 1,
                    &mut readfds as *mut u64 as u64,
                    0,
                    0,
                    timeout.as_mut_ptr() as u64,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(1)
        );
        assert_eq!(readfds, 1u64 << client_fd);

        assert_eq!(
            socket_req(NR_CLOSE, [client_fd as u64, 0, 0, 0, 0, 0], &ctx),
            SyscallResult::Return(0)
        );

        let mut readfds = 1u64 << accepted_fd;
        assert_eq!(
            socket_req(
                NR_PSELECT6,
                [
                    accepted_fd as u64 + 1,
                    &mut readfds as *mut u64 as u64,
                    0,
                    0,
                    timeout.as_mut_ptr() as u64,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(1)
        );
        assert_eq!(readfds, 1u64 << accepted_fd);

        let mut one = [0u8; 1];
        assert_eq!(
            socket_req(
                NR_RECVFROM,
                [
                    accepted_fd as u64,
                    one.as_mut_ptr() as u64,
                    one.len() as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );
        assert_eq!(
            socket_req(NR_CLOSE, [accepted_fd as u64, 0, 0, 0, 0, 0], &ctx),
            SyscallResult::Return(0)
        );
    }
}

#[test]
fn dispatch_tcp_read_write_allows_socket_sized_inline_batch() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([0, 0, 0, 0], 49_154);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_154);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                connect_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("accept failed: {other:?}"),
    };

    let payload_len = SOCKET_IO_MAX_INLINE.min(48 * 1024);
    assert!(
        payload_len > TTY_WRITE_MAX_INLINE,
        "test must cover the socket cap rather than the tty cap"
    );
    let payload = vec![0x5au8; payload_len];
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                client_fd as u64,
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

    let mut read_back = vec![0u8; payload.len()];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                accepted_fd as u64,
                read_back.as_mut_ptr() as u64,
                read_back.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(read_back.len() as i64)
    );
    assert_eq!(read_back, payload);
}

#[test]
fn dispatch_bind_zero_port_assigns_ephemeral_port() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);

    let addr = sockaddr_in([0, 0, 0, 0], 0);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
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

    let mut out = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut out_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                fd as u64,
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
    assert_eq!(out_len, SOCKADDR_IN_BYTES);
    let port = u16::from_be_bytes([out[2], out[3]]);
    assert!((49_152..49_216).contains(&port));
    assert_eq!(&out[4..8], &[0, 0, 0, 0]);
}

#[test]
fn dispatch_udp_reuseaddr_bind_zero_skips_occupied_ephemeral_port() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let first_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let second_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let addr = sockaddr_in([0, 0, 0, 0], 0);
    let one: i32 = 1;

    for fd in [first_fd, second_fd] {
        assert_eq!(
            socket_req(
                NR_SETSOCKOPT,
                [
                    fd as u64,
                    SOL_SOCKET as u64,
                    SO_REUSEADDR as u64,
                    (&one as *const i32) as u64,
                    core::mem::size_of::<i32>() as u64,
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
                    fd as u64,
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

    let mut first_name = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut second_name = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut first_len = SOCKADDR_IN_BYTES;
    let mut second_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_GETSOCKNAME,
            [
                first_fd as u64,
                first_name.as_mut_ptr() as u64,
                (&mut first_len as *mut u32) as u64,
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
            NR_GETSOCKNAME,
            [
                second_fd as u64,
                second_name.as_mut_ptr() as u64,
                (&mut second_len as *mut u32) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let first_port = u16::from_be_bytes([first_name[2], first_name[3]]);
    let second_port = u16::from_be_bytes([second_name[2], second_name[3]]);
    assert_ne!(
        first_port, second_port,
        "bind(port=0) must allocate a fresh ephemeral port even when SO_REUSEADDR is set"
    );
}

#[test]
fn dispatch_tcp_maxseg_reports_loopback_route_mss() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    let listen_addr = sockaddr_in([0, 0, 0, 0], 49_123);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_123);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listen_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                connect_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut maxseg: i32 = 0;
    let mut maxseg_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                client_fd as u64,
                IPPROTO_TCP as u64,
                TCP_MAXSEG as u64,
                (&mut maxseg as *mut i32) as u64,
                (&mut maxseg_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(
        maxseg >= 32_768,
        "loopback TCP_MAXSEG should reflect the loopback MTU, got {maxseg}"
    );
    assert_eq!(maxseg_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_udp_default_send_buffer_can_hold_loopback_datagram() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);

    let mut sndbuf: i32 = 0;
    let mut sndbuf_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_SNDBUF as u64,
                (&mut sndbuf as *mut i32) as u64,
                (&mut sndbuf_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(
        sndbuf >= 65_535,
        "default UDP send buffer should allow a loopback-sized datagram, got {sndbuf}"
    );
    assert_eq!(sndbuf_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_setsockopt_getsockopt_round_trips_reuseaddr() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);

    let one: i32 = 1;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_REUSEADDR as u64,
                (&one as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out: i32 = 0;
    let mut out_len: u32 = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_REUSEADDR as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, 1);
    assert_eq!(out_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_setsockopt_getsockopt_round_trips_dontroute() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);

    let one: i32 = 1;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_DONTROUTE as u64,
                (&one as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out: i32 = 0;
    let mut out_len: u32 = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_DONTROUTE as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, 1);
    assert_eq!(out_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_setsockopt_getsockopt_round_trips_ip_recverr() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);

    let one: i32 = 1;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IP_RECVERR as u64,
                (&one as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out: i32 = 0;
    let mut out_len: u32 = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IP_RECVERR as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, 1);
    assert_eq!(out_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_netlink_setsockopt_accepts_ext_ack() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);

    let one: i32 = 1;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_NETLINK as u64,
                NETLINK_EXT_ACK as u64,
                (&one as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out: i32 = 0;
    let mut out_len: u32 = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_NETLINK as u64,
                NETLINK_EXT_ACK as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, 1);
    assert_eq!(out_len, core::mem::size_of::<i32>() as u32);
}

#[test]
fn dispatch_iptables_legacy_sockopt_reports_empty_tables() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);

    let mut info = [0xaa; IPT_GETINFO_BYTES];
    let mut info_len: u32 = info.len() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IPT_SO_GET_INFO as u64,
                info.as_mut_ptr() as u64,
                (&mut info_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(info_len, IPT_GETINFO_BYTES as u32);

    let mut entries = [0xaa; IPT_GET_ENTRIES_EMPTY_BYTES];
    let mut entries_len: u32 = entries.len() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IPT_SO_GET_ENTRIES as u64,
                entries.as_mut_ptr() as u64,
                (&mut entries_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(entries_len, IPT_GET_ENTRIES_EMPTY_BYTES as u32);

    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IPT_SO_SET_REPLACE as u64,
                info.as_ptr() as u64,
                info.len() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(95)
    );
}

#[test]
fn dispatch_getsockopt_reports_socket_type_error_and_timeout() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);

    let mut out: i32 = 0;
    let mut out_len: u32 = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_TYPE as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, SOCK_STREAM as i32);

    out = -1;
    out_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_ERROR as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, 0);

    let mut timeout = [0i64, 250_000i64];
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_RCVTIMEO as u64,
                timeout.as_ptr() as u64,
                16,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    timeout = [0, 0];
    let mut timeout_len = 16u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_RCVTIMEO as u64,
                timeout.as_mut_ptr() as u64,
                (&mut timeout_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(timeout_len, 16);
    assert_eq!(timeout, [0, 250_000]);
}

#[test]
fn dispatch_fcntl_setfl_nonblock_affects_socket_recvfrom() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);

    assert_eq!(
        socket_req(
            NR_FCNTL,
            [fd as u64, F_SETFL as u64, O_NONBLOCK as u64, 0, 0, 0],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_FCNTL, [fd as u64, F_GETFL as u64, 0, 0, 0, 0], &ctx),
        SyscallResult::Return((O_RDWR | O_NONBLOCK) as i64)
    );

    let mut buf = [0u8; 8];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                fd as u64,
                buf.as_mut_ptr() as u64,
                buf.len() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Error(11)
    );
}

#[test]
fn pselect_socket_read_ready_includes_hup_and_errors() {
    assert!(crate::linux_syscall::io::pselect_socket_read_ready(
        true,
        PollMask::HUP
    ));
    assert!(crate::linux_syscall::io::pselect_socket_read_ready(
        true,
        PollMask::RDHUP
    ));
    assert!(crate::linux_syscall::io::pselect_socket_read_ready(
        true,
        PollMask::ERR
    ));
    assert!(!crate::linux_syscall::io::pselect_socket_read_ready(
        false,
        PollMask::IN
    ));
}

#[test]
fn dispatch_pselect_udp_write_ready_with_readfds_pointer() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let remote = sockaddr_in([127, 0, 0, 1], 49_109);

    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                fd as u64,
                remote.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut readfds = 0u64;
    let mut writefds = 1u64 << fd;
    let mut timeout = [0u64, 0u64];
    assert_eq!(
        socket_req(
            NR_PSELECT6,
            [
                fd as u64 + 1,
                &mut readfds as *mut u64 as u64,
                &mut writefds as *mut u64 as u64,
                0,
                timeout.as_mut_ptr() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(1)
    );
    assert_eq!(readfds, 0);
    assert_eq!(writefds, 1u64 << fd);
}

#[test]
fn dispatch_pselect_udp_read_timeout_returns_zero_without_reactor_timer() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let remote = sockaddr_in([127, 0, 0, 1], 49_110);

    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                fd as u64,
                remote.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut readfds = 1u64 << fd;
    let mut timeout = [1u64, 0u64];
    assert_eq!(
        socket_req(
            NR_PSELECT6,
            [
                fd as u64 + 1,
                &mut readfds as *mut u64 as u64,
                0,
                0,
                timeout.as_mut_ptr() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(readfds, 0);
}

#[test]
fn dispatch_pselect_pipe_read_ready_after_write() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let mut pipefd = [u32::MAX; 2];

    assert_eq!(
        socket_req(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0], &ctx,),
        SyscallResult::Return(0)
    );
    let reader_fd = pipefd[0];
    let writer_fd = pipefd[1];

    let mut readfds = 1u64 << reader_fd;
    let mut timeout = [0u64, 0u64];
    assert_eq!(
        socket_req(
            NR_PSELECT6,
            [
                reader_fd as u64 + 1,
                &mut readfds as *mut u64 as u64,
                0,
                0,
                timeout.as_mut_ptr() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(readfds, 0);

    let byte = *b"x";
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                writer_fd as u64,
                byte.as_ptr() as u64,
                byte.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(byte.len() as i64)
    );

    readfds = 1u64 << reader_fd;
    assert_eq!(
        socket_req(
            NR_PSELECT6,
            [
                reader_fd as u64 + 1,
                &mut readfds as *mut u64 as u64,
                0,
                0,
                timeout.as_mut_ptr() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(1)
    );
    assert_eq!(readfds, 1u64 << reader_fd);
}
