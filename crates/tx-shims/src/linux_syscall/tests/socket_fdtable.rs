// Auto-extracted-style syscall dispatch tests for N39 socket fdtable wiring.
#![cfg_attr(test, allow(unused_imports))]

use super::*;

use super::super::{errno_to_i32, Errno};
use crate::linux_syscall::{
    AF_INET, AF_NETLINK, AF_PACKET, AF_UNIX, EACCES_VALUE, EAGAIN_VALUE, EBADF_VALUE, EFAULT_VALUE,
    FD_CLOEXEC, F_GETFD, F_GETFL, F_SETFL, IPPROTO_ICMP, IPPROTO_IP, IPPROTO_TCP, IPPROTO_UDP,
    IPPROTO_UDPLITE, IPT_SO_GET_ENTRIES, IPT_SO_GET_INFO, IPT_SO_SET_REPLACE, IP_RECVERR,
    NETLINK_EXT_ACK, NETLINK_NETFILTER, NETLINK_ROUTE, NR_ACCEPT, NR_BIND, NR_CLOSE, NR_CONNECT,
    NR_DUP, NR_FCNTL, NR_GETSOCKNAME, NR_GETSOCKOPT, NR_IOCTL, NR_LISTEN, NR_PIPE2, NR_PPOLL,
    NR_PSELECT6, NR_READ, NR_RECVFROM, NR_RECVMMSG, NR_RECVMSG, NR_SENDMMSG, NR_SENDMSG, NR_SENDTO,
    NR_SETSOCKOPT, NR_SOCKET, NR_SOCKETPAIR, NR_WRITE, O_CLOEXEC, O_NONBLOCK, O_RDWR,
    PACKET_RESERVE, PACKET_RX_RING, PACKET_VERSION, SIOCGIFFLAGS, SIOCGIFINDEX, SIOCGIFTXQLEN,
    SIOCSIFFLAGS, SOCKET_IO_MAX_INLINE, SOL_NETLINK, SOL_PACKET, SOL_SOCKET, SO_DONTROUTE,
    SO_ERROR, SO_PEERCRED, SO_RCVTIMEO, SO_REUSEADDR, SO_SNDBUF, SO_SNDBUFFORCE, SO_TYPE,
    TCP_MAXSEG, TPACKET_V3, TTY_WRITE_MAX_INLINE,
};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::task::{Context, Poll, Waker};
use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};
use tx_subsystems::net::execution::{step_process_loopback_pending, LoopbackPollBudget};
use tx_subsystems::net::protocol::{
    build_icmpv4_echo_request_message, loopback_iface, parse_icmpv4_payload,
};
use tx_subsystems::net::PollMask;
use tx_subsystems::net::{Icmpv4EchoPacket, Icmpv4Event, Ipv4Address, UnixSocketPath};
use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCK_SEQPACKET: u64 = 5;
const SOCKADDR_IN_BYTES: u32 = 16;
const SOCKADDR_UN_BYTES: u32 = 110;
const SOCKADDR_NL_BYTES: u32 = 12;
const SOCKADDR_LL_BYTES: u32 = 20;
const TEST_POLLIN: i16 = 0x0001;
const TEST_POLLOUT: i16 = 0x0004;
const ETH_P_ALL: u16 = 0x0003;
const ETH_P_ALL_NET: u16 = 0x0300;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_MORE: u64 = 0x8000;
const E_PERM: i32 = 1;
const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_DUMP: u16 = 0x0300;
const MSG_PEEK: u64 = 0x02;
const MSG_TRUNC: u64 = 0x20;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFT_MSG_GETTABLE: u16 = 1;
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_DELTABLE: u16 = 2;
const IPT_GETINFO_BYTES: usize = 84;
const IPT_GET_ENTRIES_EMPTY_BYTES: usize = 36;

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

#[repr(C)]
struct TestMmsghdr {
    hdr: TestMsghdr,
    len: u32,
    _pad: u32,
}

#[repr(C)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TestTPacketReq3 {
    block_size: u32,
    block_nr: u32,
    frame_size: u32,
    frame_nr: u32,
    retire_blk_tov: u32,
    sizeof_priv: u32,
    feature_req_word: u32,
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

fn assert_udp_delivery_progress(bytes_moved: usize, expected: usize) {
    assert!(
        bytes_moved == 0 || bytes_moved == expected,
        "UDP loopback should either be delivered inline by sendto or by the explicit poll step"
    );
}

fn sockaddr_in(addr: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&port.to_be_bytes());
    bytes[4..8].copy_from_slice(&addr);
    bytes
}

fn sockaddr_un(path: &[u8]) -> [u8; SOCKADDR_UN_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_UN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
    bytes[2..2 + path.len()].copy_from_slice(path);
    bytes
}

fn sockaddr_nl() -> [u8; SOCKADDR_NL_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_NL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
    bytes
}

fn sockaddr_ll(protocol: u16, ifindex: i32) -> [u8; SOCKADDR_LL_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_LL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_PACKET.to_le_bytes());
    bytes[2..4].copy_from_slice(&protocol.to_be_bytes());
    bytes[4..8].copy_from_slice(&ifindex.to_le_bytes());
    bytes
}

fn rtnl_getlink_request(seq: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&0i32.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());

    let len = 16 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&RTM_GETLINK.to_le_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn nft_gettable_request(seq: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_be_bytes());

    let len = 16 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&nft_msg(NFT_MSG_GETTABLE).to_le_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn nft_table_request(seq: u32, op: u16, name: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_be_bytes());
    push_nla_string(&mut payload, 1, name);

    let len = 16 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&nft_msg(op).to_le_bytes());
    out.extend_from_slice(&NLM_F_REQUEST.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&payload);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn nft_msg(op: u16) -> u16 {
    (NFNL_SUBSYS_NFTABLES << 8) | op
}

fn nlmsg_type(msg: &[u8]) -> u16 {
    u16::from_le_bytes([msg[4], msg[5]])
}

fn nlmsg_error_code(msg: &[u8]) -> i32 {
    i32::from_le_bytes(msg[16..20].try_into().unwrap())
}

fn push_nla_string(out: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = Vec::from(value.as_bytes());
    payload.push(0);
    push_nla(out, kind, &payload);
}

fn push_nla(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
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

fn socket_unix_stream(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(NR_SOCKET, [AF_UNIX as u64, SOCK_STREAM, 0, 0, 0, 0], ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_UNIX, STREAM) failed: {other:?}"),
    }
}

fn socket_unix_seqpacket(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(NR_SOCKET, [AF_UNIX as u64, SOCK_SEQPACKET, 0, 0, 0, 0], ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_UNIX, SEQPACKET) failed: {other:?}"),
    }
}

#[test]
fn dispatch_unix_seqpacket_socket_reports_so_type() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = match socket_req(
        NR_SOCKET,
        [AF_UNIX as u64, SOCK_SEQPACKET, 0, 0, 0, 0],
        &ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_UNIX, SEQPACKET) failed: {other:?}"),
    };

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
    assert_eq!(out, SOCK_SEQPACKET as i32);
}

#[test]
fn dispatch_udplite_socket_reports_datagram_type() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = match socket_req(
        NR_SOCKET,
        [AF_INET as u64, SOCK_DGRAM, IPPROTO_UDPLITE as u64, 0, 0, 0],
        &ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_INET, DGRAM, UDPLITE) failed: {other:?}"),
    };

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
    assert_eq!(out, SOCK_DGRAM as i32);
}

fn socket_netlink(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(
        NR_SOCKET,
        [
            AF_NETLINK as u64,
            SOCK_RAW | O_CLOEXEC as u64,
            NETLINK_ROUTE as u64,
            0,
            0,
            0,
        ],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_NETLINK, RAW, NETLINK_ROUTE) failed: {other:?}"),
    }
}

fn socket_netfilter(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(
        NR_SOCKET,
        [
            AF_NETLINK as u64,
            SOCK_RAW | O_CLOEXEC as u64,
            NETLINK_NETFILTER as u64,
            0,
            0,
            0,
        ],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_NETLINK, RAW, NETLINK_NETFILTER) failed: {other:?}"),
    }
}

fn socket_packet(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(
        NR_SOCKET,
        [
            AF_PACKET as u64,
            SOCK_RAW | O_CLOEXEC as u64,
            ETH_P_ALL_NET as u64,
            0,
            0,
            0,
        ],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_PACKET, RAW, ETH_P_ALL) failed: {other:?}"),
    }
}

#[test]
fn dispatch_packet_ring_sockopts_validate_and_round_trip_reserve() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_packet(&ctx);

    let version = TPACKET_V3;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_PACKET as u64,
                PACKET_VERSION as u64,
                (&version as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let reserve: i32 = 16;
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_PACKET as u64,
                PACKET_RESERVE as u64,
                (&reserve as *const i32) as u64,
                core::mem::size_of::<i32>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let mut out: i32 = 0;
    let mut out_len = core::mem::size_of::<i32>() as u32;
    assert_eq!(
        socket_req(
            NR_GETSOCKOPT,
            [
                fd as u64,
                SOL_PACKET as u64,
                PACKET_RESERVE as u64,
                (&mut out as *mut i32) as u64,
                (&mut out_len as *mut u32) as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(out, reserve);

    let good = TestTPacketReq3 {
        block_size: 4096,
        block_nr: 2,
        frame_size: 4096,
        frame_nr: 2,
        retire_blk_tov: 100,
        sizeof_priv: 512,
        feature_req_word: 0,
    };
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_PACKET as u64,
                PACKET_RX_RING as u64,
                (&good as *const TestTPacketReq3) as u64,
                core::mem::size_of::<TestTPacketReq3>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let bad = TestTPacketReq3 {
        sizeof_priv: 3u32 << 30,
        ..good
    };
    assert_eq!(
        socket_req(
            NR_SETSOCKOPT,
            [
                fd as u64,
                SOL_PACKET as u64,
                PACKET_RX_RING as u64,
                (&bad as *const TestTPacketReq3) as u64,
                core::mem::size_of::<TestTPacketReq3>() as u64,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::EINVAL))
    );
}

fn socket_unix_dgram(ctx: &SyscallCtx<'static>) -> i64 {
    match socket_req(
        NR_SOCKET,
        [AF_UNIX as u64, SOCK_DGRAM | O_CLOEXEC as u64, 0, 0, 0, 0],
        ctx,
    ) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_UNIX, DGRAM) failed: {other:?}"),
    }
}

mod message_batch;
mod netlink_packet;
mod tcp_options_poll;
mod udp_loopback;
mod unix_close;
