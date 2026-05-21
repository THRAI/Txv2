// Auto-extracted-style syscall dispatch tests for N39 socket fdtable wiring.
#![cfg_attr(test, allow(unused_imports))]

use super::*;

use crate::linux_syscall::{
    AF_INET, AF_NETLINK, AF_PACKET, AF_UNIX, EBADF_VALUE, EFAULT_VALUE, F_GETFL, F_SETFL,
    IPPROTO_ICMP, IPPROTO_IP, IPPROTO_UDP, IPT_SO_GET_ENTRIES, IPT_SO_GET_INFO, IPT_SO_SET_REPLACE,
    IP_RECVERR, NETLINK_EXT_ACK, NETLINK_NETFILTER, NETLINK_ROUTE, NR_ACCEPT, NR_BIND, NR_CLOSE,
    NR_CONNECT, NR_FCNTL, NR_GETSOCKNAME, NR_GETSOCKOPT, NR_IOCTL, NR_LISTEN, NR_PIPE2, NR_PPOLL,
    NR_PSELECT6, NR_RECVFROM, NR_RECVMMSG, NR_RECVMSG, NR_SENDMMSG, NR_SENDMSG, NR_SENDTO,
    NR_SETSOCKOPT, NR_SOCKET, NR_WRITE, O_CLOEXEC, O_NONBLOCK, O_RDWR, SIOCGIFFLAGS, SIOCGIFINDEX,
    SIOCGIFTXQLEN, SIOCSIFFLAGS, SOL_NETLINK, SOL_SOCKET, SO_DONTROUTE, SO_ERROR, SO_RCVTIMEO,
    SO_REUSEADDR, SO_TYPE, TTY_WRITE_MAX_INLINE,
};
use alloc::vec;
use alloc::vec::Vec;
use tx_subsystems::cross_crate_test_support::{clear_caps_for_test, set_cred_ids_for_test};
use tx_subsystems::net::execution::{step_process_loopback_pending, LoopbackPollBudget};
use tx_subsystems::net::protocol::{
    build_icmpv4_echo_request_message, loopback_iface, parse_icmpv4_payload,
};
use tx_subsystems::net::PollMask;
use tx_subsystems::net::{Icmpv4EchoPacket, Icmpv4Event, Ipv4Address};
use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_RAW: u64 = 3;
const SOCKADDR_IN_BYTES: u32 = 16;
const SOCKADDR_NL_BYTES: u32 = 12;
const SOCKADDR_LL_BYTES: u32 = 20;
const TEST_POLLIN: i16 = 0x0001;
const ETH_P_ALL: u16 = 0x0003;
const ETH_P_ALL_NET: u16 = 0x0300;
const MSG_DONTWAIT: u64 = 0x40;
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
    while out.len() % 4 != 0 {
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
fn dispatch_netlink_route_bind_accepts_sockaddr_nl() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netlink(&ctx);
    let nladdr = sockaddr_nl();

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_netlink_netfilter_getsockname_returns_sockaddr_nl() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);
    let nladdr = sockaddr_nl();

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; SOCKADDR_NL_BYTES as usize];
    let mut out_len = SOCKADDR_NL_BYTES;
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
    assert_eq!(out_len, SOCKADDR_NL_BYTES);
    assert_eq!(u16::from_le_bytes([out[0], out[1]]), AF_NETLINK);
}

#[test]
fn dispatch_packet_bind_getsockname_and_ioctl_round_trip_sockaddr_ll() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_packet(&ctx);

    let mut ifreq = [0u8; 40];
    ifreq[0..2].copy_from_slice(b"lo");
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFINDEX as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let ifindex = i32::from_le_bytes(ifreq[16..20].try_into().unwrap());
    assert_eq!(ifindex, 1);

    let addr = sockaddr_ll(ETH_P_ALL, ifindex);
    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                addr.as_ptr() as u64,
                SOCKADDR_LL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; SOCKADDR_LL_BYTES as usize];
    let mut out_len = SOCKADDR_LL_BYTES;
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
    assert_eq!(out_len, SOCKADDR_LL_BYTES);
    assert_eq!(u16::from_le_bytes([out[0], out[1]]), AF_PACKET);
    assert_eq!(u16::from_be_bytes([out[2], out[3]]), ETH_P_ALL);
    assert_eq!(i32::from_le_bytes(out[4..8].try_into().unwrap()), ifindex);

    let mut buf = [0u8; 8];
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                fd as u64,
                buf.as_mut_ptr() as u64,
                buf.len() as u64,
                MSG_DONTWAIT,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Error(11)
    );
}

#[test]
fn dispatch_unprivileged_socket_denies_net_raw_families() {
    let _setup = socket_setup();
    let process = bootstrap();
    set_cred_ids_for_test(&process, 1000, 1000, 1000, 1000, 1000, 1000);
    clear_caps_for_test(&process);
    let thread = first_thread(&process);
    let ctx = make_ctx(process, thread);

    assert_eq!(
        socket_req(
            NR_SOCKET,
            [
                AF_PACKET as u64,
                SOCK_RAW | O_CLOEXEC as u64,
                ETH_P_ALL_NET as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Error(E_PERM)
    );
    assert_eq!(
        socket_req(
            NR_SOCKET,
            [
                AF_INET as u64,
                SOCK_RAW | O_CLOEXEC as u64,
                IPPROTO_ICMP as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Error(E_PERM)
    );
    assert!(matches!(
        socket_req(
            NR_SOCKET,
            [
                AF_INET as u64,
                SOCK_DGRAM | O_CLOEXEC as u64,
                IPPROTO_UDP as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(fd) if fd >= 0
    ));
}

#[test]
fn dispatch_netlink_route_sendto_recvfrom_returns_dump() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netlink(&ctx);
    let nladdr = sockaddr_nl();
    let request = rtnl_getlink_request(0x55);
    let mut recv_buf = [0u8; 512];
    let mut recv_addr = [0u8; SOCKADDR_NL_BYTES as usize];
    let mut recv_addr_len = SOCKADDR_NL_BYTES;

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let recv = socket_req(
        NR_RECVFROM,
        [
            fd as u64,
            recv_buf.as_mut_ptr() as u64,
            recv_buf.len() as u64,
            0,
            recv_addr.as_mut_ptr() as u64,
            (&mut recv_addr_len as *mut u32) as u64,
        ],
        &ctx,
    );

    assert!(matches!(recv, SyscallResult::Return(n) if n > 0));
    assert_eq!(
        u16::from_le_bytes([recv_addr[0], recv_addr[1]]),
        AF_NETLINK as u16
    );
    assert_eq!(recv_addr_len, SOCKADDR_NL_BYTES);
}

#[test]
fn dispatch_netlink_route_write_read_returns_dump() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netlink(&ctx);
    let nladdr = sockaddr_nl();
    let request = rtnl_getlink_request(0x66);
    let mut recv_buf = [0u8; 512];

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_WRITE,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let recv = socket_req(
        NR_READ,
        [
            fd as u64,
            recv_buf.as_mut_ptr() as u64,
            recv_buf.len() as u64,
            0,
            0,
            0,
        ],
        &ctx,
    );

    assert!(matches!(recv, SyscallResult::Return(n) if n > 0));
    assert!(
        contains_bytes(&recv_buf, b"lo\0"),
        "read(2) should return the rtnetlink dump bytes"
    );
}

#[test]
fn dispatch_netlink_netfilter_sendto_recvfrom_returns_table_dump() {
    let _setup = socket_setup();
    tx_subsystems::net::flush_netfilter_rules_and_conntrack_for_test_or_bootstrap();
    tx_subsystems::net::add_masquerade_rule_for_test_or_bootstrap(
        tx_subsystems::net::NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "uplink-nft-shim0",
    )
    .expect("masquerade rule");

    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);
    let nladdr = sockaddr_nl();
    let request = nft_gettable_request(0x77);
    let mut recv_buf = [0u8; 512];
    let mut recv_addr = [0u8; SOCKADDR_NL_BYTES as usize];
    let mut recv_addr_len = SOCKADDR_NL_BYTES;

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let recv = socket_req(
        NR_RECVFROM,
        [
            fd as u64,
            recv_buf.as_mut_ptr() as u64,
            recv_buf.len() as u64,
            0,
            recv_addr.as_mut_ptr() as u64,
            (&mut recv_addr_len as *mut u32) as u64,
        ],
        &ctx,
    );

    assert!(matches!(recv, SyscallResult::Return(n) if n > 0));
    assert_eq!(nlmsg_type(&recv_buf), nft_msg(NFT_MSG_NEWTABLE));
    assert!(contains_bytes(&recv_buf, b"nat\0"));
    assert_eq!(
        u16::from_le_bytes([recv_addr[0], recv_addr[1]]),
        AF_NETLINK as u16
    );
    assert_eq!(recv_addr_len, SOCKADDR_NL_BYTES);
    tx_subsystems::net::flush_netfilter_rules_and_conntrack_for_test_or_bootstrap();
}

#[test]
fn dispatch_netlink_netfilter_newtable_returns_ack() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);
    let nladdr = sockaddr_nl();
    let request = nft_table_request(0x78, NFT_MSG_NEWTABLE, "txshimnft");
    let cleanup = nft_table_request(0x79, NFT_MSG_DELTABLE, "txshimnft");
    let mut recv_buf = [0u8; 128];

    assert_eq!(
        socket_req(
            NR_BIND,
            [
                fd as u64,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let recv = socket_req(
        NR_RECVFROM,
        [
            fd as u64,
            recv_buf.as_mut_ptr() as u64,
            recv_buf.len() as u64,
            0,
            0,
            0,
        ],
        &ctx,
    );
    assert!(matches!(recv, SyscallResult::Return(n) if n > 0));
    assert_eq!(nlmsg_type(&recv_buf), 2);
    assert_eq!(nlmsg_error_code(&recv_buf), 0);

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                cleanup.as_ptr() as u64,
                cleanup.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64
            ],
            &ctx,
        ),
        SyscallResult::Return(cleanup.len() as i64)
    );
}

#[test]
fn dispatch_socket_ioctl_resolves_loopback_ifindex_and_txqlen() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifreq = [0u8; 40];
    ifreq[0..2].copy_from_slice(b"lo");

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFINDEX as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(i32::from_le_bytes(ifreq[16..20].try_into().unwrap()), 1);

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFTXQLEN as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(i32::from_le_bytes(ifreq[16..20].try_into().unwrap()), 0);
}

#[test]
fn dispatch_socket_ioctl_reads_and_writes_interface_flags() {
    const IFF_UP: i16 = 0x0001;
    const IFF_LOOPBACK: i16 = 0x0008;

    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifreq = [0u8; 40];
    ifreq[0..2].copy_from_slice(b"lo");

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFFLAGS as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let flags = i16::from_le_bytes(ifreq[16..18].try_into().unwrap());
    assert_ne!(flags & IFF_UP, 0);
    assert_ne!(flags & IFF_LOOPBACK, 0);

    ifreq[16..18].copy_from_slice(&(flags | IFF_UP).to_le_bytes());
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCSIFFLAGS as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_unix_dgram_socket_ioctl_resolves_loopback_ifindex() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_unix_dgram(&ctx);
    let mut ifreq = [0u8; 40];
    ifreq[0..2].copy_from_slice(b"lo");

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFINDEX as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(i32::from_le_bytes(ifreq[16..20].try_into().unwrap()), 1);
}

#[test]
fn dispatch_netlink_route_getlink_sendmsg_recvmsg_returns_dump() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netlink(&ctx);
    let nladdr = sockaddr_nl();
    let request = rtnl_getlink_request(0x44);
    let send_iov = [TestIovec {
        base: request.as_ptr() as u64,
        len: request.len() as u64,
    }];
    let mut send_hdr = TestMsghdr {
        name: nladdr.as_ptr() as u64,
        namelen: SOCKADDR_NL_BYTES,
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
                fd as u64,
                (&mut send_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut response = [0u8; 8192];
    let recv_iov = [TestIovec {
        base: response.as_mut_ptr() as u64,
        len: response.len() as u64,
    }];
    let mut source = [0u8; SOCKADDR_NL_BYTES as usize];
    let mut recv_hdr = TestMsghdr {
        name: source.as_mut_ptr() as u64,
        namelen: SOCKADDR_NL_BYTES,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
        control: 0,
        controllen: 0,
        flags: 0xFFFF_FFFF,
        _pad1: 0,
    };

    let recv = match socket_req(
        NR_RECVMSG,
        [
            fd as u64,
            (&mut recv_hdr as *mut TestMsghdr) as u64,
            0,
            0,
            0,
            0,
        ],
        &ctx,
    ) {
        SyscallResult::Return(recv) => recv as usize,
        other => panic!("recvmsg netlink failed: {other:?}"),
    };

    assert!(recv >= 20);
    assert_eq!(recv_hdr.namelen, SOCKADDR_NL_BYTES);
    assert_eq!(recv_hdr.flags, 0);
    assert_eq!(u16::from_le_bytes([source[0], source[1]]), AF_NETLINK);
    assert_eq!(u16::from_le_bytes([response[4], response[5]]), RTM_NEWLINK);
    assert!(response[..recv].windows(3).any(|window| window == b"lo\0"));
}

#[test]
fn dispatch_netlink_route_recvmsg_peek_trunc_reports_datagram_len() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netlink(&ctx);
    let nladdr = sockaddr_nl();
    let request = rtnl_getlink_request(0x45);

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut peek_hdr = TestMsghdr {
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
    let peek_len = match socket_req(
        NR_RECVMSG,
        [
            fd as u64,
            (&mut peek_hdr as *mut TestMsghdr) as u64,
            MSG_PEEK | MSG_TRUNC,
            0,
            0,
            0,
        ],
        &ctx,
    ) {
        SyscallResult::Return(recv) => recv as usize,
        other => panic!("peek-trunc recvmsg netlink failed: {other:?}"),
    };

    assert!(peek_len >= 20);
    assert_eq!(peek_hdr.flags, MSG_TRUNC as u32);

    let mut response = [0u8; 8192];
    let recv_iov = [TestIovec {
        base: response.as_mut_ptr() as u64,
        len: response.len() as u64,
    }];
    let mut recv_hdr = TestMsghdr {
        name: 0,
        namelen: 0,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
        control: 0,
        controllen: 0,
        flags: 0xFFFF_FFFF,
        _pad1: 0,
    };
    let recv = match socket_req(
        NR_RECVMSG,
        [
            fd as u64,
            (&mut recv_hdr as *mut TestMsghdr) as u64,
            0,
            0,
            0,
            0,
        ],
        &ctx,
    ) {
        SyscallResult::Return(recv) => recv as usize,
        other => panic!("post-peek recvmsg netlink failed: {other:?}"),
    };

    assert_eq!(recv, peek_len);
    assert_eq!(recv_hdr.flags, 0);
    assert_eq!(u16::from_le_bytes([response[4], response[5]]), RTM_NEWLINK);
    assert!(response[..recv].windows(3).any(|window| window == b"lo\0"));
}

#[test]
fn dispatch_netlink_netfilter_sendmsg_accepts_large_batch() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);
    let nladdr = sockaddr_nl();
    let request = vec![0u8; TTY_WRITE_MAX_INLINE + 64];
    let send_iov = [TestIovec {
        base: request.as_ptr() as u64,
        len: request.len() as u64,
    }];
    let mut send_hdr = TestMsghdr {
        name: nladdr.as_ptr() as u64,
        namelen: SOCKADDR_NL_BYTES,
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
                fd as u64,
                (&mut send_hdr as *mut TestMsghdr) as u64,
                0,
                0,
                0,
                0,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );
}

#[test]
fn dispatch_netlink_netfilter_recvmsg_accepts_large_user_buffer() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_netfilter(&ctx);
    let nladdr = sockaddr_nl();
    let request = [0u8; 20];

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                nladdr.as_ptr() as u64,
                SOCKADDR_NL_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut response = vec![0u8; 128 * 1024];
    let recv_iov = [TestIovec {
        base: response.as_mut_ptr() as u64,
        len: response.len() as u64,
    }];
    let mut source = [0u8; SOCKADDR_NL_BYTES as usize];
    let mut recv_hdr = TestMsghdr {
        name: source.as_mut_ptr() as u64,
        namelen: SOCKADDR_NL_BYTES,
        _pad0: 0,
        iov: recv_iov.as_ptr() as u64,
        iovlen: recv_iov.len() as u64,
        control: 0,
        controllen: 0,
        flags: 0,
        _pad1: 0,
    };

    match socket_req(
        NR_RECVMSG,
        [
            fd as u64,
            (&mut recv_hdr as *mut TestMsghdr) as u64,
            0,
            0,
            0,
            0,
        ],
        &ctx,
    ) {
        SyscallResult::Return(bytes) => assert!(bytes > 0),
        other => panic!("large netlink recvmsg buffer failed: {other:?}"),
    }
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
