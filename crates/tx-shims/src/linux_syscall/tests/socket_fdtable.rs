// Auto-extracted-style syscall dispatch tests for N39 socket fdtable wiring.
#![cfg_attr(test, allow(unused_imports))]

use super::*;

use crate::linux_syscall::{
    AF_INET, F_GETFL, F_SETFL, IPPROTO_ICMP, IPPROTO_UDP, NR_BIND, NR_CLOSE, NR_CONNECT, NR_FCNTL,
    NR_GETSOCKNAME, NR_GETSOCKOPT, NR_LISTEN, NR_PPOLL, NR_RECVFROM, NR_RECVMSG, NR_SENDMSG,
    NR_SENDTO, NR_SETSOCKOPT, NR_SOCKET, O_CLOEXEC, O_NONBLOCK, O_RDWR, SOL_SOCKET, SO_ERROR,
    SO_RCVTIMEO, SO_REUSEADDR, SO_TYPE,
};
use alloc::vec;
use tx_subsystems::net::execution::{step_process_loopback_pending, LoopbackPollBudget};
use tx_subsystems::net::protocol::{
    build_icmpv4_echo_request_message, loopback_iface, parse_icmpv4_payload,
};
use tx_subsystems::net::PollMask;
use tx_subsystems::net::{Icmpv4EchoPacket, Icmpv4Event, Ipv4Address};
use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCKADDR_IN_BYTES: u32 = 16;
const TEST_POLLIN: i16 = 0x0001;

#[repr(C)]
struct TestIovec {
    base: u64,
    len: u64,
}

#[repr(C)]
struct TestPollfd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct TestMsghdr {
    name: u64,
    namelen: u32,
    _pad0: u32,
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
    flags: u32,
    _pad1: u32,
}

fn socket_setup() -> TestSetup {
    setup()
}

fn socket_ctx() -> (Cap<ProcessIdentity>, SyscallCtx<'static>) {
    let process = bootstrap();
    let thread = first_thread(&process);
    let ctx = make_ctx(process.clone(), thread);
    (process, ctx)
}

fn socket_req(nr: u64, args: [u64; 6], ctx: &SyscallCtx<'static>) -> SyscallResult {
    block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(nr, args),
        ctx,
    ))
}

fn sockaddr_in(addr: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&port.to_be_bytes());
    bytes[4..8].copy_from_slice(&addr);
    bytes
}

fn socket_stream(ctx: &SyscallCtx<'static>, type_flags: u64) -> i64 {
    match socket_req(NR_SOCKET, [AF_INET as u64, type_flags, 0, 0, 0, 0], ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_INET, STREAM) failed: {other:?}"),
    }
}

fn socket_dgram(ctx: &SyscallCtx<'static>, type_flags: u64) -> i64 {
    match socket_req(
        NR_SOCKET,
        [AF_INET as u64, type_flags, IPPROTO_UDP as u64, 0, 0, 0],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_INET, DGRAM) failed: {other:?}"),
    }
}

fn socket_icmp(ctx: &SyscallCtx<'static>, type_flags: u64) -> i64 {
    match socket_req(
        NR_SOCKET,
        [AF_INET as u64, type_flags, IPPROTO_ICMP as u64, 0, 0, 0],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_INET, ICMP) failed: {other:?}"),
    }
}

#[test]
fn dispatch_socket_installs_struct_backed_socket_fd() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();

    let fd = socket_stream(&ctx, SOCK_STREAM | O_NONBLOCK as u64 | O_CLOEXEC as u64);

    let file = process.fd(fd as u32).expect("socket fd installed");
    assert!(file.flags().read);
    assert!(file.flags().write);
    assert!(file.flags().nonblocking);
    assert!(process.fd_cloexec(fd as u32));
    assert!(matches!(
        file.rnode().backing(),
        RNodeBacking::StructBacked {
            payload: StructPayload::Socket { .. }
        }
    ));
}

#[test]
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
    assert_eq!(moved.udp_bytes_moved, payload.len());

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
    assert_eq!(moved.udp_bytes_moved, 5);

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
    assert_eq!(
        socket_req(
            NR_PPOLL,
            [(&mut pollfd as *mut TestPollfd) as u64, 1, 0, 0, 0, 0,],
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
            [(&mut pollfd as *mut TestPollfd) as u64, 1, 0, 0, 0, 0,],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(pollfd.revents, 0);
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
