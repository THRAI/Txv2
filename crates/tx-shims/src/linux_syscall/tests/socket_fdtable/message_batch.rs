use super::*;

#[test]
fn dispatch_zero_length_udp_sendmsg_recvmsg_consumes_one_datagram() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_102);

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
            NR_CONNECT,
            [
                client_fd as u64,
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

    let mut send_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: 0,
        iovlen: 0,
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
        SyscallResult::Return(0)
    );

    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut recv_hdr = TestMsghdr {
        name: source_addr.as_mut_ptr() as u64,
        namelen: source_addr.len() as u32,
        _pad0: 0,
        iov: 0,
        iovlen: 0,
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
        SyscallResult::Return(0)
    );
    assert_eq!(recv_hdr.namelen, SOCKADDR_IN_BYTES);

    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [server_fd as u64, 0, 0, MSG_DONTWAIT, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(EAGAIN_VALUE),
        "the zero-length recvmsg must consume exactly one datagram"
    );

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
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(NR_RECVFROM, [server_fd as u64, 0, 0, 0, 0, 0], &ctx),
        SyscallResult::Return(0),
        "recvfrom with a zero-length buffer must consume one datagram"
    );
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [server_fd as u64, 0, 0, MSG_DONTWAIT, 0, 0],
            &ctx,
        ),
        SyscallResult::Error(EAGAIN_VALUE)
    );
}

#[test]
fn dispatch_sendmsg_recvmsg_udp_loopback_round_trips_source_addr() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_103);
    let client_addr = sockaddr_in([127, 0, 0, 1], 49_104);

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
    assert_eq!(
        socket_req(
            NR_CONNECT,
            [
                client_fd as u64,
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

    let part_a = *b"he";
    let part_b = *b"llo";
    let send_iov = [
        TestIovec {
            base: part_a.as_ptr() as u64,
            len: part_a.len() as u64,
        },
        TestIovec {
            base: part_b.as_ptr() as u64,
            len: part_b.len() as u64,
        },
    ];
    let mut send_hdr = TestMsghdr {
        name: server_addr.as_ptr() as u64,
        namelen: SOCKADDR_IN_BYTES,
        _pad0: 0,
        iov: send_iov.as_ptr() as u64,
        iovlen: send_iov.len() as u64,
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
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(5)
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
    assert_udp_delivery_progress(moved.udp_bytes_moved, 5);

    let mut out_a = [0u8; 2];
    let mut out_b = [0u8; 3];
    let recv_iov = [
        TestIovec {
            base: out_a.as_mut_ptr() as u64,
            len: out_a.len() as u64,
        },
        TestIovec {
            base: out_b.as_mut_ptr() as u64,
            len: out_b.len() as u64,
        },
    ];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut recv_hdr = TestMsghdr {
        name: source_addr.as_mut_ptr() as u64,
        namelen: SOCKADDR_IN_BYTES,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
        control: 0,
        controllen: 0,
        flags: 0xFFFF_FFFF,
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
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(5)
    );
    assert_eq!(&out_a, b"he");
    assert_eq!(&out_b, b"llo");
    assert_eq!(recv_hdr.namelen, SOCKADDR_IN_BYTES);
    assert_eq!(recv_hdr.flags, 0);
    assert_eq!(
        u16::from_le_bytes([source_addr[0], source_addr[1]]),
        AF_INET
    );
    assert_eq!(u16::from_be_bytes([source_addr[2], source_addr[3]]), 49_104);
    assert_eq!(&source_addr[4..8], &[127, 0, 0, 1]);
}

#[test]
fn dispatch_recvmsg_empty_socket_is_interrupted_by_due_itimer() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_105);
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

    let timer = TestItimerval {
        interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };
    assert_eq!(
        socket_req(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    for _ in 0..1_100 {
        let _ = <ShimsTestPmap as tx_hal::MonotonicCounterIf>::read_ns();
    }

    let mut out = [0u8; 8];
    let recv_iov = [TestIovec {
        base: out.as_mut_ptr() as u64,
        len: out.len() as u64,
    }];
    let mut recv_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
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
        SyscallResult::Error(EINTR_VALUE)
    );
}

#[test]
fn dispatch_inet6_udp_recvmsg_peek_preserves_datagram_and_source_addr() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram6(&ctx, SOCK_DGRAM, IPPROTO_IP as u64);
    let client_fd = socket_dgram6(&ctx, SOCK_DGRAM, IPPROTO_IP as u64);
    let loopback6 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let server_addr = sockaddr_in6(loopback6, 49_125);
    let client_addr = sockaddr_in6(loopback6, 49_126);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_addr.as_ptr() as u64,
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
        socket_req(
            NR_BIND,
            [
                client_fd as u64,
                client_addr.as_ptr() as u64,
                SOCKADDR_IN6_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"hello";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                server_addr.as_ptr() as u64,
                SOCKADDR_IN6_BYTES as u64,
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

    let mut peek_out = [0u8; 5];
    let peek_iov = [TestIovec {
        base: peek_out.as_mut_ptr() as u64,
        len: peek_out.len() as u64,
    }];
    let mut source_addr = [0u8; SOCKADDR_IN6_BYTES as usize];
    let mut peek_hdr = TestMsghdr {
        name: source_addr.as_mut_ptr() as u64,
        namelen: SOCKADDR_IN6_BYTES,
        _pad0: 0,
        iov: peek_iov.as_ptr() as u64,
        iovlen: peek_iov.len() as u64,
        control: 0,
        controllen: 0,
        flags: 0xFFFF_FFFF,
        _pad1: 0,
    };
    assert_eq!(
        socket_req(
            NR_RECVMSG,
            [
                server_fd as u64,
                (&mut peek_hdr as *mut TestMsghdr) as u64,
                MSG_PEEK,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );
    assert_eq!(&peek_out, b"hello");
    assert_eq!(peek_hdr.namelen, SOCKADDR_IN6_BYTES);
    assert_eq!(peek_hdr.flags, 0);
    assert_eq!(
        u16::from_le_bytes([source_addr[0], source_addr[1]]),
        AF_INET6
    );
    assert_eq!(u16::from_be_bytes([source_addr[2], source_addr[3]]), 49_126);
    assert_eq!(&source_addr[8..24], &loopback6);

    let mut recv_out = [0u8; 5];
    let recv_iov = [TestIovec {
        base: recv_out.as_mut_ptr() as u64,
        len: recv_out.len() as u64,
    }];
    let mut recv_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
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
    assert_eq!(&recv_out, b"hello");
}

#[test]
fn dispatch_inet6_udp_loopback_reaches_wildcard_bound_receiver() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram6(&ctx, SOCK_DGRAM, IPPROTO_UDP as u64);
    let client_fd = socket_dgram6(&ctx, SOCK_DGRAM, IPPROTO_UDP as u64);
    let any6 = [0u8; 16];
    let loopback6 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let server_bind_addr = sockaddr_in6(any6, 49_127);
    let server_connect_addr = sockaddr_in6(loopback6, 49_127);
    let client_addr = sockaddr_in6(loopback6, 49_128);

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                server_fd as u64,
                server_bind_addr.as_ptr() as u64,
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
        socket_req(
            NR_BIND,
            [
                client_fd as u64,
                client_addr.as_ptr() as u64,
                SOCKADDR_IN6_BYTES as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let payload = *b"v6";
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                client_fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                server_connect_addr.as_ptr() as u64,
                SOCKADDR_IN6_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let mut out = [0u8; 2];
    let mut source_addr = [0u8; SOCKADDR_IN6_BYTES as usize];
    let mut source_len = SOCKADDR_IN6_BYTES;
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
    assert_eq!(&out, b"v6");
    assert_eq!(source_len, SOCKADDR_IN6_BYTES);
    assert_eq!(
        u16::from_le_bytes([source_addr[0], source_addr[1]]),
        AF_INET6
    );
    assert_eq!(u16::from_be_bytes([source_addr[2], source_addr[3]]), 49_128);
    assert_eq!(&source_addr[8..24], &loopback6);
}

#[test]
fn dispatch_udp_sendmsg_autobinds_and_corks_msg_more() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_124);

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

    let first = [0x42u8; 16];
    let first_iov = [TestIovec {
        base: first.as_ptr() as u64,
        len: first.len() as u64,
    }];
    let mut first_hdr = TestMsghdr {
        name: server_addr.as_ptr() as u64,
        namelen: SOCKADDR_IN_BYTES,
        _pad0: 0,
        iov: first_iov.as_ptr() as u64,
        iovlen: first_iov.len() as u64,
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
                (&mut first_hdr as *mut TestMsghdr) as u64,
                MSG_MORE,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(first.len() as i64)
    );

    let mut early = [0u8; 32];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                early.as_mut_ptr() as u64,
                early.len() as u64,
                MSG_DONTWAIT,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EAGAIN_VALUE)
    );

    let last = [0x21u8; 1];
    let last_iov = [TestIovec {
        base: last.as_ptr() as u64,
        len: last.len() as u64,
    }];
    let mut last_hdr = TestMsghdr {
        name: server_addr.as_ptr() as u64,
        namelen: SOCKADDR_IN_BYTES,
        _pad0: 0,
        iov: last_iov.as_ptr() as u64,
        iovlen: last_iov.len() as u64,
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
                (&mut last_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(last.len() as i64)
    );

    let mut combined = [0u8; 32];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                server_fd as u64,
                combined.as_mut_ptr() as u64,
                combined.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(17)
    );
    assert_eq!(&combined[..16], &first);
    assert_eq!(combined[16], last[0]);
}

#[test]
fn dispatch_sendmmsg_recvmmsg_udp_loopback_batch_round_trips() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let server_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let client_fd = socket_dgram(&ctx, SOCK_DGRAM);
    let server_addr = sockaddr_in([127, 0, 0, 1], 49_123);

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
            NR_CONNECT,
            [
                client_fd as u64,
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

    let first_a = *b"one";
    let first_b = *b"two";
    let second = *b"three3";
    let send_iov0 = [
        TestIovec {
            base: first_a.as_ptr() as u64,
            len: first_a.len() as u64,
        },
        TestIovec {
            base: first_b.as_ptr() as u64,
            len: first_b.len() as u64,
        },
    ];
    let send_iov1 = [TestIovec {
        base: second.as_ptr() as u64,
        len: second.len() as u64,
    }];
    let mut send_msgs = [
        TestMmsghdr {
            hdr: TestMsghdr {
                name: 0,
                namelen: 0,
                _pad0: 0,
                iov: send_iov0.as_ptr() as u64,
                iovlen: send_iov0.len() as u64,
                control: 0,
                controllen: 0,
                flags: 0,
                _pad1: 0,
            },
            len: 0,
            _pad: 0,
        },
        TestMmsghdr {
            hdr: TestMsghdr {
                name: 0,
                namelen: 0,
                _pad0: 0,
                iov: send_iov1.as_ptr() as u64,
                iovlen: send_iov1.len() as u64,
                control: 0,
                controllen: 0,
                flags: 0,
                _pad1: 0,
            },
            len: 0,
            _pad: 0,
        },
    ];

    assert_eq!(
        socket_req(
            NR_SENDMMSG,
            [
                client_fd as u64,
                send_msgs.as_mut_ptr() as u64,
                send_msgs.len() as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(2)
    );
    assert_eq!(send_msgs[0].len, 6);
    assert_eq!(send_msgs[1].len, 6);

    let mut recv_first = [0u8; 6];
    let mut recv_second = [0u8; 5];
    let recv_iov0 = [TestIovec {
        base: recv_first.as_mut_ptr() as u64,
        len: recv_first.len() as u64,
    }];
    let recv_iov1 = [TestIovec {
        base: recv_second.as_mut_ptr() as u64,
        len: recv_second.len() as u64,
    }];
    let mut recv_msgs = [
        TestMmsghdr {
            hdr: TestMsghdr {
                name: 0,
                namelen: 0,
                _pad0: 0,
                iov: recv_iov0.as_ptr() as u64,
                iovlen: recv_iov0.len() as u64,
                control: 0,
                controllen: 0,
                flags: 0,
                _pad1: 0,
            },
            len: 0,
            _pad: 0,
        },
        TestMmsghdr {
            hdr: TestMsghdr {
                name: 0,
                namelen: 0,
                _pad0: 0,
                iov: recv_iov1.as_ptr() as u64,
                iovlen: recv_iov1.len() as u64,
                control: 0,
                controllen: 0,
                flags: 0,
                _pad1: 0,
            },
            len: 0,
            _pad: 0,
        },
    ];
    let mut timeout = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };

    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                server_fd as u64,
                recv_msgs.as_mut_ptr() as u64,
                recv_msgs.len() as u64,
                0,
                (&mut timeout as *mut TestTimespec) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(2)
    );
    assert_eq!(&recv_first, b"onetwo");
    assert_eq!(&recv_second, b"three");
    assert_eq!(recv_msgs[0].len, 6);
    assert_eq!(recv_msgs[1].len, 5);
}

#[test]
fn dispatch_sendmmsg_recvmmsg_error_order_matches_socket_abi() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut byte = [0u8; 1];
    let iov = [TestIovec {
        base: byte.as_mut_ptr() as u64,
        len: byte.len() as u64,
    }];
    let mut msg = [TestMmsghdr {
        hdr: TestMsghdr {
            name: 0,
            namelen: 0,
            _pad0: 0,
            iov: iov.as_ptr() as u64,
            iovlen: iov.len() as u64,
            control: 0,
            controllen: 0,
            flags: 0,
            _pad1: 0,
        },
        len: 0,
        _pad: 0,
    }];
    let mut timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    assert_eq!(
        socket_req(
            NR_SENDMMSG,
            [u64::MAX, msg.as_mut_ptr() as u64, msg.len() as u64, 0, 0, 0,],
            &ctx,
        ),
        SyscallResult::Error(EBADF_VALUE)
    );
    assert_eq!(
        socket_req(NR_SENDMMSG, [fd as u64, 0, 1, 0, 0, 0], &ctx),
        SyscallResult::Error(EFAULT_VALUE)
    );
    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                u64::MAX,
                msg.as_mut_ptr() as u64,
                msg.len() as u64,
                0,
                (&mut timeout as *mut TestTimespec) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EBADF_VALUE)
    );
    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                fd as u64,
                0,
                1,
                0,
                (&mut timeout as *mut TestTimespec) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EFAULT_VALUE)
    );

    timeout.tv_sec = -1;
    timeout.tv_nsec = 0;
    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                fd as u64,
                msg.as_mut_ptr() as u64,
                msg.len() as u64,
                0,
                (&mut timeout as *mut TestTimespec) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EINVAL_VALUE)
    );
    timeout.tv_sec = 1;
    timeout.tv_nsec = 1_000_000_000;
    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                fd as u64,
                msg.as_mut_ptr() as u64,
                msg.len() as u64,
                0,
                (&mut timeout as *mut TestTimespec) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(EINVAL_VALUE)
    );
    assert_eq!(
        socket_req(
            NR_RECVMMSG,
            [
                fd as u64,
                msg.as_mut_ptr() as u64,
                msg.len() as u64,
                0,
                1,
                0
            ],
            &ctx,
        ),
        SyscallResult::Error(EFAULT_VALUE)
    );
}

#[test]
fn dispatch_ping_socket_sendto_recvfrom_loopback_echo_reply() {
    let _setup = socket_setup();
    loopback_iface().clear_for_test_or_bootstrap();
    let (_process, ctx) = socket_ctx();
    let fd = socket_icmp(&ctx, SOCK_DGRAM);
    let local_addr = sockaddr_in([127, 0, 0, 1], 0);
    let peer_addr = sockaddr_in([127, 0, 0, 1], 0);

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

    let mut pollfd = TestPollfd {
        fd: fd as i32,
        events: TEST_POLLIN,
        revents: -1,
    };
    let zero_timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        socket_req(
            NR_PPOLL,
            [
                (&mut pollfd as *mut TestPollfd) as u64,
                1,
                (&zero_timeout as *const TestTimespec) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(pollfd.revents, 0);

    let request = Icmpv4EchoPacket {
        src: Ipv4Address::LOOPBACK,
        dst: Ipv4Address::LOOPBACK,
        ident: 0x5050,
        seq_no: 1,
        payload: b"txkernel-ping".to_vec(),
    };
    let request_bytes = build_icmpv4_echo_request_message(&request);
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request_bytes.as_ptr() as u64,
                request_bytes.len() as u64,
                0,
                peer_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request_bytes.len() as i64)
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
    assert!(moved.icmp_transfer_attempted >= 1);
    assert_eq!(moved.icmp_transfer_failed, 0);
    assert_eq!(moved.icmp_bytes_moved, request_bytes.len());

    pollfd.revents = 0;
    assert_eq!(
        socket_req(
            NR_PPOLL,
            [(&mut pollfd as *mut TestPollfd) as u64, 1, 0, 0, 0, 0,],
            &ctx,
        ),
        SyscallResult::Return(1)
    );
    assert_ne!(pollfd.revents & TEST_POLLIN, 0);

    let mut reply = [0u8; 64];
    let mut source_addr = [0u8; SOCKADDR_IN_BYTES as usize];
    let mut source_len = SOCKADDR_IN_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                fd as u64,
                reply.as_mut_ptr() as u64,
                reply.len() as u64,
                0,
                source_addr.as_mut_ptr() as u64,
                (&mut source_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request_bytes.len() as i64)
    );
    assert_eq!(source_len, SOCKADDR_IN_BYTES);
    assert_eq!(
        u16::from_le_bytes([source_addr[0], source_addr[1]]),
        AF_INET
    );
    assert_eq!(u16::from_be_bytes([source_addr[2], source_addr[3]]), 0);
    assert_eq!(&source_addr[4..8], &[127, 0, 0, 1]);
    assert_eq!(
        parse_icmpv4_payload(
            Ipv4Address::LOOPBACK,
            Ipv4Address::LOOPBACK,
            &reply[..request_bytes.len()],
        ),
        Icmpv4Event::EchoReply(request.reply_packet())
    );

    pollfd.revents = -1;
    assert_eq!(
        socket_req(
            NR_PPOLL,
            [
                (&mut pollfd as *mut TestPollfd) as u64,
                1,
                (&zero_timeout as *const TestTimespec) as u64,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(pollfd.revents, 0);
}
