use super::*;
use crate::linux_syscall::{SO_KEEPALIVE, TCP_NODELAY};
use tx_subsystems::net::{IpEndpoint, RecvWireSet, SendWireSet, SocketProtocol, TcpState};

#[repr(C)]
struct TestTlsCryptoInfo {
    version: u16,
    cipher_type: u16,
}

struct TcpConnectCountWake {
    wakes: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
}

impl alloc::task::Wake for TcpConnectCountWake {
    fn wake(self: alloc::sync::Arc<Self>) {
        self.wakes
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }

    fn wake_by_ref(self: &alloc::sync::Arc<Self>) {
        self.wakes
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }
}

fn tcp_connect_counting_waker(wakes: alloc::sync::Arc<core::sync::atomic::AtomicUsize>) -> Waker {
    Waker::from(alloc::sync::Arc::new(TcpConnectCountWake { wakes }))
}

#[test]
fn dispatch_blocking_accept_is_interrupted_by_sigterm_after_parking() {
    let _setup = socket_setup();
    let process = bootstrap();
    let thread = first_thread(&process);
    let mailbox = alloc::sync::Arc::new(tx_substrate::wake::TaskMailbox::new());
    thread
        .payload_cap()
        .expect("thread payload alive")
        .bind_mailbox(alloc::sync::Arc::downgrade(&mailbox));
    let _ = tx_subsystems::signal::step_sigaction(
        &process,
        tx_subsystems::signal::Signum::SIGTERM,
        tx_subsystems::signal::SigDisposition::Handler(0xCAFE),
    );
    let ctx = make_ctx(process.clone(), thread).with_mailbox(alloc::sync::Arc::clone(&mailbox));

    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([127, 0, 0, 1], 49_100);
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

    let wakes = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let waker = tcp_connect_counting_waker(alloc::sync::Arc::clone(&wakes));
    let mut cx = core::task::Context::from_waker(&waker);
    let mut accept = alloc::boxed::Box::pin(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert!(matches!(
        accept.as_mut().poll(&mut cx),
        core::task::Poll::Pending
    ));

    assert_eq!(
        tx_subsystems::signal::step_kill_process_with_post(
            &process,
            tx_subsystems::signal::Signum::SIGTERM,
            None,
            |weak, event| {
                if let Some(mailbox) = weak.upgrade() {
                    let _ = mailbox.post(event);
                }
            },
        ),
        tx_subsystems::signal::KillOutcome::Delivered
    );
    assert!(
        wakes.load(core::sync::atomic::Ordering::SeqCst) > 0,
        "SIGTERM must wake the task parked in accept"
    );
    assert_eq!(
        accept.as_mut().poll(&mut cx),
        core::task::Poll::Ready(SyscallResult::Error(EINTR_VALUE))
    );
    assert!(
        mailbox.is_empty(),
        "accept must consume its SignalDelivered wake hint after observing pending signal state"
    );
}

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
fn dispatch_tls_ulp_disconnect_rebind_listen_returns_einval() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let listener_addr = sockaddr_in([127, 0, 0, 1], 49_118);

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

    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
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
    let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("accept failed: {other:?}"),
    };

    let ulp_name = *b"tls";
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                client_fd as u64,
                IPPROTO_TCP as u64,
                TCP_ULP as u64,
                ulp_name.as_ptr() as u64,
                ulp_name.len() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let tls = TestTlsCryptoInfo {
        version: 0x0303,
        cipher_type: 51,
    };
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                client_fd as u64,
                SOL_TLS as u64,
                TLS_TX as u64,
                (&tls as *const TestTlsCryptoInfo) as u64,
                core::mem::size_of::<TestTlsCryptoInfo>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let unspec_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
                unspec_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let rebind_addr = sockaddr_in([127, 0, 0, 1], 49_119);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                client_fd as u64,
                rebind_addr.as_ptr() as u64,
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
        socket_req(NR_LISTEN, [client_fd as u64, 1, 0, 0, 0, 0], &ctx),
        SyscallResult::Error(errno_to_i32(Errno::EINVAL))
    );

    for fd in [accepted_fd, client_fd, listener_fd] {
        assert_eq!(
            socket_req(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0], &ctx),
            SyscallResult::Return(0)
        );
    }
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
fn dispatch_ipv6_addrform_reset_rebinds_accepted_tcp_as_ipv4_listener() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream6(&ctx, SOCK_STREAM);
    let any6 = [0u8; 16];
    let listener_addr = sockaddr_in6(any6, 49_152);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
                SOCKADDR_IN6_BYTES as u64,
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

    let listener_v4_addr = sockaddr_in([127, 0, 0, 1], 49_152);

    for _ in 0..4 {
        let client_fd = socket_stream(&ctx, SOCK_STREAM);
        assert_eq!(
            socket_req(
                NR_CONNECT,
                [
                    client_fd as u64,
                    listener_v4_addr.as_ptr() as u64,
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
        let optval = AF_INET as i32;
        assert_eq!(
            socket_req(
                NR_SETSOCKOPT,
                [
                    accepted_fd as u64,
                    SOL_IPV6 as u64,
                    IPV6_ADDRFORM as u64,
                    (&optval as *const i32) as u64,
                    core::mem::size_of::<i32>() as u64,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );

        let reset_addr = [0u8; SOCKADDR_IN_BYTES as usize];
        assert_eq!(
            socket_req(
                NR_CONNECT,
                [
                    accepted_fd as u64,
                    reset_addr.as_ptr() as u64,
                    SOCKADDR_IN_BYTES as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );

        let rebind_addr = sockaddr_in([0, 0, 0, 0], 0);
        assert_eq!(
            socket_req(
                NR_BIND,
                [
                    accepted_fd as u64,
                    rebind_addr.as_ptr() as u64,
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
            socket_req(NR_LISTEN, [accepted_fd as u64, 8, 0, 0, 0, 0], &ctx),
            SyscallResult::Return(0)
        );

        let mut rebound_name = [0u8; SOCKADDR_IN_BYTES as usize];
        let mut rebound_name_len = SOCKADDR_IN_BYTES;
        assert_eq!(
            socket_req(
                NR_GETSOCKNAME,
                [
                    accepted_fd as u64,
                    rebound_name.as_mut_ptr() as u64,
                    (&mut rebound_name_len as *mut u32) as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );
        assert_eq!(
            u16::from_le_bytes([rebound_name[0], rebound_name[1]]),
            AF_INET
        );
        let rebound_port = u16::from_be_bytes([rebound_name[2], rebound_name[3]]);
        assert_ne!(rebound_port, 0);

        let second_client_fd = socket_stream(&ctx, SOCK_STREAM);
        let rebound_connect_addr = sockaddr_in([127, 0, 0, 1], rebound_port);
        assert_eq!(
            socket_req(
                NR_CONNECT,
                [
                    second_client_fd as u64,
                    rebound_connect_addr.as_ptr() as u64,
                    SOCKADDR_IN_BYTES as u64,
                    0,
                    0,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );
        let second_accepted_fd =
            match socket_req(NR_ACCEPT, [accepted_fd as u64, 0, 0, 0, 0, 0], &ctx) {
                SyscallResult::Return(fd) => fd as u32,
                other => panic!("second accept failed: {other:?}"),
            };

        for fd in [
            second_accepted_fd,
            second_client_fd as u32,
            client_fd as u32,
            accepted_fd,
        ] {
            assert_eq!(
                socket_req(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0], &ctx),
                SyscallResult::Return(0)
            );
        }
    }
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

    let mut empty = [0u8; 1];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                accepted_fd as u64,
                empty.as_mut_ptr() as u64,
                empty.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EAGAIN_VALUE),
        "a mailbox-less bootstrap read must not park after the queue is drained"
    );
}

#[test]
fn dispatch_tcp_read_write_rejects_unconnected_socket() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);
    let byte = [0x5au8; 1];

    assert_eq!(
        socket_req(
            NR_WRITE,
            [fd as u64, byte.as_ptr() as u64, byte.len() as u64, 0, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EPIPE))
    );

    let mut out = [0u8; 1];
    assert_eq!(
        socket_req(
            NR_READ,
            [
                fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::ENOTCONN))
    );
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
fn dispatch_tcp_msg_more_defers_until_uncork_send() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let listener_fd = socket_stream(&ctx, SOCK_STREAM);
    let client_fd = socket_stream(&ctx, SOCK_STREAM);
    let listen_addr = sockaddr_in([0, 0, 0, 0], 49_124);
    let connect_addr = sockaddr_in([127, 0, 0, 1], 49_124);

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
    let accepted_fd = match socket_req(NR_ACCEPT, [listener_fd as u64, 0, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("accept(AF_INET) failed: {other:?}"),
    };

    let first = [0x42u8; 16];
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                first.as_ptr() as u64,
                first.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(first.len() as i64)
    );
    let mut out = [0u8; 32];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                accepted_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                MSG_DONTWAIT,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(first.len() as i64)
    );
    assert_eq!(&out[..first.len()], &first);

    let corked = [0x43u8; 16];
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                corked.as_ptr() as u64,
                corked.len() as u64,
                MSG_MORE,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(corked.len() as i64)
    );
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                accepted_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                MSG_DONTWAIT,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EAGAIN_VALUE)
    );

    let uncork = [0x44u8; 1];
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                uncork.as_ptr() as u64,
                uncork.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(uncork.len() as i64)
    );
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                accepted_fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                MSG_DONTWAIT,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return((corked.len() + uncork.len()) as i64)
    );
    assert_eq!(&out[..corked.len()], &corked);
    assert_eq!(out[corked.len()], uncork[0]);
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
fn dispatch_so_sndbufforce_clamps_large_unsigned_value() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let sndbuf = 0xffffff00u32;

    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_SNDBUFFORCE as u64,
                (&sndbuf as *const u32) as u64,
                core::mem::size_of::<u32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut rec_sndbuf: i32 = 0;
    let mut optlen = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_SNDBUF as u64,
                (&mut rec_sndbuf as *mut i32) as u64,
                (&mut optlen as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(rec_sndbuf >= 0);
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
fn dispatch_tcp_dynamic_nodelay_and_keepalive_update_live_engine() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);
    let file = process.fd(fd as u32).expect("resolve TCP file");
    let socket =
        crate::linux_syscall::socket::socket_identity_from_file(&file).expect("resolve TCP socket");
    let payload = socket.acquire_operational().expect("TCP payload");
    let raw = payload.raw_tcp_socket().expect("raw TCP engine");
    assert!(!raw.nodelay());
    assert!(!raw.keep_alive_enabled());

    let one: i32 = 1;
    for (level, option) in [(IPPROTO_TCP, TCP_NODELAY), (SOL_SOCKET, SO_KEEPALIVE)] {
        assert_eq!(
            socket_req(
                NR_SETSOCKOPT,
                [
                    fd as u64,
                    level as u64,
                    option as u64,
                    (&one as *const i32) as u64,
                    core::mem::size_of::<i32>() as u64,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );
    }
    assert!(raw.nodelay(), "TCP_NODELAY must update the existing engine");
    assert!(
        raw.keep_alive_enabled(),
        "SO_KEEPALIVE must update the existing engine"
    );
    for (level, option) in [(IPPROTO_TCP, TCP_NODELAY), (SOL_SOCKET, SO_KEEPALIVE)] {
        let mut value = 0i32;
        let mut value_len = core::mem::size_of::<i32>() as u32;
        assert_eq!(
            socket_req(
                NR_GETSOCKOPT,
                [
                    fd as u64,
                    level as u64,
                    option as u64,
                    (&mut value as *mut i32) as u64,
                    (&mut value_len as *mut u32) as u64,
                    0,
                ],
                &ctx,
            ),
            SyscallResult::Return(0)
        );
        assert_eq!(value, 1);
    }

    let zero: i32 = 0;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_KEEPALIVE as u64,
                (&zero as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(!raw.keep_alive_enabled());

    let udp_fd = socket_dgram(&ctx, SOCK_DGRAM);
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                udp_fd as u64,
                IPPROTO_TCP as u64,
                TCP_NODELAY as u64,
                (&one as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT))
    );
    let mut value = 0i32;
    let mut value_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                udp_fd as u64,
                IPPROTO_TCP as u64,
                TCP_NODELAY as u64,
                (&mut value as *mut i32) as u64,
                (&mut value_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT))
    );
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
fn dispatch_setsockopt_getsockopt_round_trips_ip_ttl() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_icmp(&ctx, SOCK_RAW);

    let ttl: i32 = 1;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IP_TTL as u64,
                (&ttl as *const i32) as u64,
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
                IP_TTL as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, ttl);
    assert_eq!(out_len, core::mem::size_of::<i32>() as u32);

    let invalid: i32 = 0;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IP_TTL as u64,
                (&invalid as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EINVAL))
    );
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
        SyscallResult::Error(errno_to_i32(Errno::EINVAL))
    );

    let replace = [0u8; 96];
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                IPPROTO_IP as u64,
                IPT_SO_SET_REPLACE as u64,
                replace.as_ptr() as u64,
                replace.len() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP))
    );
}

fn tcp_rst_frame(
    src: IpEndpoint,
    dst: IpEndpoint,
    ack_number: i32,
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
) -> Vec<u8> {
    let src_addr = smoltcp::wire::Ipv4Address::new(
        src.addr.octets()[0],
        src.addr.octets()[1],
        src.addr.octets()[2],
        src.addr.octets()[3],
    );
    let dst_addr = smoltcp::wire::Ipv4Address::new(
        dst.addr.octets()[0],
        dst.addr.octets()[1],
        dst.addr.octets()[2],
        dst.addr.octets()[3],
    );
    let tcp_repr = smoltcp::wire::TcpRepr {
        src_port: src.port,
        dst_port: dst.port,
        control: smoltcp::wire::TcpControl::Rst,
        seq_number: smoltcp::wire::TcpSeqNumber(0x2929),
        ack_number: Some(smoltcp::wire::TcpSeqNumber(ack_number)),
        window_len: 4096,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let tcp_len = tcp_repr.buffer_len();
    let ip_repr = smoltcp::wire::IpRepr::Ipv4(smoltcp::wire::Ipv4Repr {
        src_addr,
        dst_addr,
        next_header: smoltcp::wire::IpProtocol::Tcp,
        payload_len: tcp_len,
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let mut ip_bytes = vec![0u8; ip_header_len + tcp_len];
    let checksum_caps = smoltcp::phy::ChecksumCapabilities::default();
    ip_repr.emit(&mut ip_bytes[..ip_header_len], &checksum_caps);
    let mut tcp_packet = smoltcp::wire::TcpPacket::new_unchecked(&mut ip_bytes[ip_header_len..]);
    tcp_repr.emit(
        &mut tcp_packet,
        &smoltcp::wire::IpAddress::Ipv4(src_addr),
        &smoltcp::wire::IpAddress::Ipv4(dst_addr),
        &checksum_caps,
    );

    let mut frame = Vec::with_capacity(14 + ip_bytes.len());
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&src_mac);
    frame.extend_from_slice(&[0x08, 0x00]);
    frame.extend_from_slice(&ip_bytes);
    frame
}

#[test]
fn dispatch_getsockopt_so_error_consumes_pending_error() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let fd = socket_stream(&ctx, SOCK_STREAM);
    let file = process.fd(fd as u32).expect("resolve TCP file");
    let socket =
        crate::linux_syscall::socket::socket_identity_from_file(&file).expect("resolve TCP socket");
    let payload = socket.acquire_operational().expect("TCP payload");
    payload.set_socket_error(Errno::ECONNREFUSED);
    socket.readiness.fire_send(SendWireSet::CONNECT_DONE);

    let mut invalid_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_SOCKET as u64,
                SO_ERROR as u64,
                0,
                (&mut invalid_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EFAULT))
    );
    assert_eq!(
        payload.socket_error(),
        None,
        "Linux consumes SO_ERROR before a failing copyout"
    );

    payload.set_socket_error(Errno::ECONNREFUSED);
    for expected in [errno_to_i32(Errno::ECONNREFUSED), 0] {
        let mut out = -1i32;
        let mut out_len = core::mem::size_of::<i32>() as u32;
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
        assert_eq!(out, expected);
        assert_eq!(out_len, core::mem::size_of::<i32>() as u32);
    }
    assert_eq!(
        socket.readiness.send_wq.peek() & SendWireSet::CONNECT_DONE.bits(),
        0
    );
}

#[test]
fn dispatch_nonblocking_tcp_connect_exposes_a_poll_wait_source() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "nb-connect-left",
            devt: tx_subsystems::device::DevT::new(91, 250),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 2, 50]),
        },
        right: VethEndpointConfig {
            name: "nb-connect-right",
            devt: tx_subsystems::device::DevT::new(91, 251),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 2, 51]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    tx_subsystems::net::initial_net_namespace_payload()
        .attach_device_for_test_or_bootstrap(pair.left, Some(Ipv4Address::new([192, 0, 2, 2])))
        .expect("attach outbound veth");
    let fd = socket_stream(&ctx, SOCK_STREAM | O_NONBLOCK as u64);
    let local = sockaddr_in([192, 0, 2, 2], 0);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                local.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let remote = sockaddr_in([192, 0, 2, 1], 49_177);

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
        SyscallResult::Error(errno_to_i32(Errno::EINPROGRESS))
    );

    let file = process.fd(fd as u32).expect("resolve TCP file");
    let socket =
        crate::linux_syscall::socket::socket_identity_from_file(&file).expect("resolve TCP socket");
    let guard = tx_substrate::epoch::guard();
    assert!(matches!(
        tx_subsystems::net::step_poll_wait_token(&socket, PollMask::OUT, &guard),
        StepOutcome::Done(Some(_))
    ));
    drop(guard);

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
        SyscallResult::Error(errno_to_i32(Errno::EALREADY))
    );
}

#[test]
fn dispatch_blocking_tcp_connect_returns_refused_after_rst_wakeup() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    const LEFT_MAC: [u8; 6] = [0x02, 0, 0, 0, 2, 60];
    const RIGHT_MAC: [u8; 6] = [0x02, 0, 0, 0, 2, 61];
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "blocking-connect-left",
            devt: tx_subsystems::device::DevT::new(91, 252),
            mac: EthernetAddress::new(LEFT_MAC),
        },
        right: VethEndpointConfig {
            name: "blocking-connect-right",
            devt: tx_subsystems::device::DevT::new(91, 253),
            mac: EthernetAddress::new(RIGHT_MAC),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    let namespace = tx_subsystems::net::initial_net_namespace_payload();
    namespace
        .attach_device_for_test_or_bootstrap(pair.left, Some(Ipv4Address::new([192, 0, 2, 2])))
        .expect("attach outbound veth");

    let fd = socket_stream(&ctx, SOCK_STREAM);
    let local_addr = sockaddr_in([192, 0, 2, 2], 0);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                local_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let remote_addr = sockaddr_in([192, 0, 2, 1], 49_178);
    let request = SyscallRequest::new(
        NR_CONNECT,
        [
            fd as u64,
            remote_addr.as_ptr() as u64,
            SOCKADDR_IN_BYTES as u64,
            0,
            0,
            0,
        ],
    );
    let mut connect = Box::pin(dispatch::<ShimsTestPmap>(request, &ctx));
    let wake_count = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let waker = tcp_connect_counting_waker(alloc::sync::Arc::clone(&wake_count));
    let mut cx = Context::from_waker(&waker);
    assert!(matches!(connect.as_mut().poll(&mut cx), Poll::Pending));

    let file = process.fd(fd as u32).expect("resolve TCP file");
    let socket =
        crate::linux_syscall::socket::socket_identity_from_file(&file).expect("resolve TCP socket");
    let payload = socket.acquire_operational().expect("TCP payload");
    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => (local, remote),
        state => panic!("blocking connect should be in progress, got {state:?}"),
    };
    let raw = payload.raw_tcp_socket().expect("raw TCP");
    let syn = raw.dispatch_segment().expect("initial SYN");
    let rst = tcp_rst_frame(
        remote,
        local,
        syn.tcp.seq_number.0.wrapping_add(1),
        LEFT_MAC,
        RIGHT_MAC,
    );

    let guard = tx_substrate::epoch::guard();
    assert_eq!(pair.right.ops.transmit(&rst, &guard), StepOutcome::Done(()));
    let runtime = tx_subsystems::net::drive_net_namespace_runtime_at(
        namespace,
        smoltcp::time::Instant::ZERO,
        &guard,
    );
    assert_eq!(runtime.packets_seen, 1);
    drop(guard);
    assert!(
        wake_count.load(core::sync::atomic::Ordering::SeqCst) > 0,
        "RST CONNECT_DONE must wake the parked blocking connect"
    );

    assert_eq!(
        connect.as_mut().poll(&mut cx),
        Poll::Ready(SyscallResult::Error(errno_to_i32(Errno::ECONNREFUSED)))
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
fn select_fd_read_ready_includes_hup_and_errors() {
    assert!(crate::linux_syscall::io::select_fd_read_ready(
        true,
        FdReadyMask::HUP
    ));
    assert!(crate::linux_syscall::io::select_fd_read_ready(
        true,
        FdReadyMask::RDHUP
    ));
    assert!(crate::linux_syscall::io::select_fd_read_ready(
        true,
        FdReadyMask::ERR
    ));
    assert!(!crate::linux_syscall::io::select_fd_read_ready(
        false,
        FdReadyMask::READ
    ));
}

#[test]
fn select_fd_blocked_directions_keeps_read_and_write_distinct() {
    assert_eq!(
        crate::linux_syscall::io::select_fd_blocked_directions(true, true, FdReadyMask::empty()),
        (true, true)
    );
    assert_eq!(
        crate::linux_syscall::io::select_fd_blocked_directions(true, true, FdReadyMask::WRITE),
        (true, false)
    );
    assert_eq!(
        crate::linux_syscall::io::select_fd_blocked_directions(true, true, FdReadyMask::ERR),
        (false, false)
    );
    assert_eq!(
        crate::linux_syscall::io::select_fd_blocked_directions(true, true, FdReadyMask::HUP),
        (false, true)
    );
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
fn dispatch_pselect_time64_udp_write_ready_uses_pselect6_path() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let remote = sockaddr_in([127, 0, 0, 1], 49_161);

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

    let mut writefds = 1u64 << fd;
    let mut timeout = [0u64, 0u64];
    assert_eq!(
        socket_req(
            NR_PSELECT6_TIME64,
            [
                fd as u64 + 1,
                0,
                &mut writefds as *mut u64 as u64,
                0,
                timeout.as_mut_ptr() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(1)
    );
    assert_eq!(writefds, 1u64 << fd);
}

#[test]
fn dispatch_ppoll_udp_reports_write_ready_when_read_is_also_requested() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let remote = sockaddr_in([127, 0, 0, 1], 49_160);

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

    let mut pollfd = TestPollfd {
        fd: fd as i32,
        events: TEST_POLLIN | TEST_POLLOUT,
        revents: -1,
    };
    let mut timeout = [0u64, 0u64];
    assert_eq!(
        socket_req(
            NR_PPOLL,
            [
                (&mut pollfd as *mut TestPollfd) as u64,
                1,
                timeout.as_mut_ptr() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(1)
    );
    assert_eq!(pollfd.revents & TEST_POLLIN, 0);
    assert_ne!(pollfd.revents & TEST_POLLOUT, 0);
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
