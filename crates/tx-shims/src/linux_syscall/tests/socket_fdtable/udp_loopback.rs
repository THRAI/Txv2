use super::*;

#[test]
fn dispatch_bind_privileged_port_requires_root_euid() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let process = bootstrap();
    set_cred_ids_for_test(&process, 65_534, 65_534, 65_534, 65_534, 65_534, 65_534);
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);
    let fd = socket_stream(&ctx, SOCK_STREAM);
    let low_port = sockaddr_in([0, 0, 0, 0], 463);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                low_port.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EACCES_VALUE)
    );
}

#[test]
fn dispatch_recvfrom_large_user_buffer_returns_short_read() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_105);
    let client_addr = sockaddr_in([127, 0, 0, 1], 49_106);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_addr.as_ptr() as u64,
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
        socket_req(
            NR_BIND,
            [
                client_fd as u64,
                client_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"netperf-drain";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                server_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, payload.len());

    let mut out = vec![0u8; 0x40000];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                0,
                source_addr.as_mut_ptr() as u64,
                (&mut source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );
    assert_eq!(&out[..payload.len()], &payload);
    assert_eq!(source_len, SOCKADDR_IN_BYTES);
    assert_eq!(u16::from_be_bytes([source_addr[2], source_addr[3]]), 49_106);
    assert_eq!(&source_addr[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_external_udp_sendto_keeps_datagram_for_device_tx() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let dns_addr = sockaddr_in([10, 0, 2, 3], 53);
    let query = *b"dns-query";

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                query.as_ptr() as u64,
                query.len() as u64,
                0,
                dns_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(query.len() as i64)
    );

    let file = process.fd(fd as u32).expect("resolve UDP file");
    let socket =
        crate::linux_syscall::socket::socket_identity_from_file(&file).expect("resolve UDP socket");
    let payload = socket.acquire_operational().expect("UDP payload");
    let datagram = payload
        .raw_udp_socket()
        .and_then(|raw| raw.peek_tx_datagram())
        .expect("external datagram must remain queued for device TX");
    assert_eq!(datagram.dst.port, 53);
    assert!(!datagram.dst.is_loopback());
}

#[test]
fn dispatch_udp_connect_autobinds_and_reaches_wildcard_bound_peer() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_107);
    let server_connect_addr = sockaddr_in([127, 0, 0, 1], 49_107);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
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
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                server_connect_addr.as_ptr() as u64,
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
    assert!((49_152..49_216).contains(&client_port));
    assert_eq!(&client_name[4..8], &[127, 0, 0, 1]);

    let hello = *b"ping";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                hello.as_ptr() as u64,
                hello.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(hello.len() as i64)
    );
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_eq!(moved.udp_bytes_moved, 0);

    let mut inbound = [0u8; 8];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
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
        SyscallResult::Return(hello.len() as i64)
    );
    assert_eq!(&inbound[..hello.len()], &hello);
    assert_eq!(
        u16::from_be_bytes([source_addr[2], source_addr[3]]),
        client_port
    );
    assert_eq!(&source_addr[4..8], &[127, 0, 0, 1]);

    let pong = *b"pong";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                server_fd as u64,
                pong.as_ptr() as u64,
                pong.len() as u64,
                0,
                source_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(pong.len() as i64)
    );
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_eq!(moved.udp_bytes_moved, 0);

    let mut reply = [0u8; 8];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                client_fd as u64,
                reply.as_mut_ptr() as u64,
                reply.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(pong.len() as i64)
    );
    assert_eq!(&reply[..pong.len()], &pong);
}

#[test]
fn dispatch_udp_connected_write_reaches_bound_peer() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_109);
    let server_connect_addr = sockaddr_in([127, 0, 0, 1], 49_109);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
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
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                server_connect_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"iperf-udp";
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

    let mut inbound = [0u8; 16];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
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
    assert_eq!(&source_addr[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_iperf_udp_connect_handshake_round_trips() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_110);
    let server_connect_addr = sockaddr_in([127, 0, 0, 1], 49_110);
    let one: i32 = 1;

    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                server_fd as u64,
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
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
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
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                server_connect_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let connect_msg = 0x36373839u32.to_ne_bytes();
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                client_fd as u64,
                connect_msg.as_ptr() as u64,
                connect_msg.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(connect_msg.len() as i64)
    );

    let mut accepted_msg = [0u8; 4];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                accepted_msg.as_mut_ptr() as u64,
                accepted_msg.len() as u64,
                0,
                source_addr.as_mut_ptr() as u64,
                (&mut source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(connect_msg.len() as i64)
    );
    assert_eq!(accepted_msg, connect_msg);

    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                server_fd as u64,
                source_addr.as_ptr() as u64,
                source_len as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let new_listener_fd = socket_dgram(&ctx, SOCK_DGRAM);
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                new_listener_fd as u64,
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
                new_listener_fd as u64,
                server_bind_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let reply = 0x39383736u32.to_ne_bytes();
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                server_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );

    let mut client_reply = [0u8; 4];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                client_fd as u64,
                client_reply.as_mut_ptr() as u64,
                client_reply.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(client_reply, reply);
}

#[test]
fn dispatch_netperf_udp_rr_unconnected_sendto_recvfrom_round_trips() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_120);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_120);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let request = *b"netperf-rr-request";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                server_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut inbound = [0u8; 64];
    let mut client_addr = [0u8; 128];
    let mut client_addr_len: u32 = client_addr.len() as u32;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                inbound.as_mut_ptr() as u64,
                inbound.len() as u64,
                0,
                client_addr.as_mut_ptr() as u64,
                (&mut client_addr_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );
    assert_eq!(&inbound[..request.len()], &request);
    assert_eq!(client_addr_len, SOCKADDR_IN_BYTES);
    assert_eq!(&client_addr[4..8], &[127, 0, 0, 1]);
    assert_ne!(u16::from_be_bytes([client_addr[2], client_addr[3]]), 0);

    let reply = *b"netperf-rr-reply";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                server_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                client_addr.as_ptr() as u64,
                client_addr_len as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );

    let mut response = [0u8; 64];
    let mut reply_source = [0u8; 128];
    let mut reply_source_len: u32 = reply_source.len() as u32;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                client_fd as u64,
                response.as_mut_ptr() as u64,
                response.len() as u64,
                0,
                reply_source.as_mut_ptr() as u64,
                (&mut reply_source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(&response[..reply.len()], &reply);
    assert_eq!(reply_source_len, SOCKADDR_IN_BYTES);
    assert_eq!(
        u16::from_be_bytes([reply_source[2], reply_source[3]]),
        49_120
    );
    assert_eq!(&reply_source[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_netperf_udp_rr_two_process_contexts_round_trip() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (server_process, server_ctx) = socket_ctx();
    let client_process =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&server_process, false, false)
            .expect("fork client");
    let client_thread = first_thread(&client_process);
    let client_ctx = make_ctx(client_process, client_thread);
    let server_fd = socket_dgram(&server_ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&client_ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_121);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_121);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(0)
    );

    let request = *b"netperf-rr-cross-process";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                server_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &client_ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut inbound = [0u8; 64];
    let mut client_addr = [0u8; 128];
    let mut client_addr_len: u32 = client_addr.len() as u32;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                inbound.as_mut_ptr() as u64,
                inbound.len() as u64,
                0,
                client_addr.as_mut_ptr() as u64,
                (&mut client_addr_len as *mut u32) as u64,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );
    assert_eq!(&inbound[..request.len()], &request);
    assert_eq!(client_addr_len, SOCKADDR_IN_BYTES);
    assert_eq!(&client_addr[4..8], &[127, 0, 0, 1]);

    let reply = *b"netperf-rr-cross-reply";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                server_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                client_addr.as_ptr() as u64,
                client_addr_len as u64,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );

    let mut response = [0u8; 64];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                client_fd as u64,
                response.as_mut_ptr() as u64,
                response.len() as u64,
                0,
                0,
                0,
            ],
            &client_ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(&response[..reply.len()], &reply);
}

#[test]
fn dispatch_blocked_udp_rr_client_recv_wakes_after_server_reply() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (server_process, server_ctx) = socket_ctx();
    let client_process =
        tx_subsystems::process::step_fork::<ShimsTestPmap>(&server_process, false, false)
            .expect("fork client");
    let client_thread = first_thread(&client_process);
    let client_ctx = make_ctx(client_process, client_thread);
    let server_fd = socket_dgram(&server_ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&client_ctx, SOCK_DGRAM);
    let server_bind_addr = sockaddr_in([0, 0, 0, 0], 49_122);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_122);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(0)
    );

    let request = *b"netperf-client-waits";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                server_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &client_ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut response = [0u8; 64];
    let client_recv_req = SyscallRequest::new(
        NR_RECVFROM,
        [
            client_fd as u64,
            response.as_mut_ptr() as u64,
            response.len() as u64,
            0,
            0,
            0,
        ],
    );
    let mut client_recv = Box::pin(dispatch::<ShimsTestPmap>(client_recv_req, &client_ctx));
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    assert!(
        matches!(client_recv.as_mut().poll(&mut cx), Poll::Pending),
        "client recvfrom should park before the server replies"
    );

    let mut inbound = [0u8; 64];
    let mut client_addr = [0u8; 128];
    let mut client_addr_len: u32 = client_addr.len() as u32;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                inbound.as_mut_ptr() as u64,
                inbound.len() as u64,
                0,
                client_addr.as_mut_ptr() as u64,
                (&mut client_addr_len as *mut u32) as u64,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );
    assert_eq!(&inbound[..request.len()], &request);

    let reply = *b"netperf-server-reply";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                server_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                client_addr.as_ptr() as u64,
                client_addr_len as u64,
            ],
            &server_ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );

    for _ in 0..256 {
        if let Poll::Ready(result) = client_recv.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(reply.len() as i64));
            drop(client_recv);
            assert_eq!(&response[..reply.len()], &reply);
            return;
        }
    }
    panic!("client recvfrom did not wake after the server reply");
}

#[test]
fn dispatch_udp_reuseaddr_rebind_keeps_new_owner_after_old_close() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let old_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let new_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let bind_addr = sockaddr_in([0, 0, 0, 0], 49_108);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_108);
    let one: i32 = 1;

    for fd in [old_fd, new_fd] {
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
    }

    for fd in [old_fd, new_fd] {
        assert_eq!(
            socket_req(
                NR_BIND,
                [
                    fd as u64,
                    bind_addr.as_ptr() as u64,
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

    assert_eq!(
        socket_req(NR_CLOSE, [old_fd as u64, 0, 0, 0, 0, 0], &ctx),
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

    let payload = *b"new-owner";
    assert_eq!(
        socket_req(
            NR_SENDTO,
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
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, payload.len());

    let mut out = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                new_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
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
fn dispatch_udp_reuseaddr_rebind_restores_old_owner_after_new_close() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let old_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let new_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let bind_addr = sockaddr_in([0, 0, 0, 0], 49_111);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_111);
    let one: i32 = 1;

    for fd in [old_fd, new_fd] {
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
    }

    for fd in [old_fd, new_fd] {
        assert_eq!(
            socket_req(
                NR_BIND,
                [
                    fd as u64,
                    bind_addr.as_ptr() as u64,
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

    assert_eq!(
        socket_req(NR_CLOSE, [new_fd as u64, 0, 0, 0, 0, 0], &ctx),
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

    let payload = *b"restored-owner";
    assert_eq!(
        socket_req(
            NR_SENDTO,
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
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, payload.len());

    let mut out = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                old_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
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
fn dispatch_udp_reuseaddr_listener_does_not_steal_connected_flow() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let session_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let listener_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let bind_addr = sockaddr_in([0, 0, 0, 0], 49_109);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_109);
    let one: i32 = 1;

    for fd in [session_fd, listener_fd] {
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
    }

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                session_fd as u64,
                bind_addr.as_ptr() as u64,
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

    let probe = *b"probe";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                probe.as_ptr() as u64,
                probe.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(probe.len() as i64)
    );
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, probe.len());

    let mut first = [0u8; 16];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                session_fd as u64,
                first.as_mut_ptr() as u64,
                first.len() as u64,
                0,
                source_addr.as_mut_ptr() as u64,
                (&mut source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(probe.len() as i64)
    );
    assert_eq!(&first[..probe.len()], &probe);

    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                session_fd as u64,
                source_addr.as_ptr() as u64,
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
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                bind_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let stream = *b"stream";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                stream.as_ptr() as u64,
                stream.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(stream.len() as i64)
    );
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, stream.len());

    let mut out = [0u8; 16];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                session_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(stream.len() as i64)
    );
    assert_eq!(&out[..stream.len()], &stream);

    let reply = *b"reply";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                session_fd as u64,
                reply.as_ptr() as u64,
                reply.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    let moved = {
        let guard = tx_substrate::epoch::guard();
        match step_process_loopback_pending(
            smoltcp::time::Instant::ZERO,
            loopback_iface(),
            LoopbackPollBudget::default(),
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(outcome) => outcome,
            other => panic!("unexpected loopback outcome: {other:?}"),
        }
    };
    assert_udp_delivery_progress(moved.udp_bytes_moved, reply.len());

    let mut client_in = [0u8; 16];
    let mut reply_source = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut reply_source_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                client_fd as u64,
                client_in.as_mut_ptr() as u64,
                client_in.len() as u64,
                0,
                reply_source.as_mut_ptr() as u64,
                (&mut reply_source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(&client_in[..reply.len()], &reply);
    assert_eq!(reply_source_len, SOCKADDR_IN_BYTES);
    assert_eq!(
        u16::from_be_bytes([reply_source[2], reply_source[3]]),
        49_109
    );
    assert_eq!(&reply_source[4..8], &[127, 0, 0, 1]);
}
