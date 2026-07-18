use super::*;

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
fn dispatch_netlink_xfrm_dump_returns_empty_done() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_xfrm(&ctx);
    let nladdr = sockaddr_nl();
    let request = xfrm_dump_request(0x60);
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

    let mut response = [0u8; 128];
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
        flags: 0,
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
        other => panic!("recvmsg netlink xfrm failed: {other:?}"),
    };

    assert!(recv >= 20);
    assert!(contains_nlmsg_type(&response[..recv], NLMSG_DONE));
    assert_eq!(nlmsg_flags(&response[..recv]) & NLM_F_MULTI, NLM_F_MULTI);
}

#[test]
fn dispatch_packet_bind_getsockname_and_ioctl_round_trip_sockaddr_ll() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0x89, 0, 1]);
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "pktll0",
            devt: DevT::new(118, 1),
            mac: local_mac,
        },
        right: VethEndpointConfig {
            name: "pktll1",
            devt: DevT::new(118, 2),
            mac: EthernetAddress::new([0x02, 0, 0, 0x89, 0, 2]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    process
        .net_namespace()
        .expect("test net namespace")
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach packet test veth");
    let fd = socket_packet(&ctx);

    let mut ifreq = [0u8; 40];
    ifreq[0..6].copy_from_slice(b"pktll0");
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
    assert!(ifindex > 1);

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
    assert_eq!(u16::from_le_bytes(out[8..10].try_into().unwrap()), 1);
    assert_eq!(out[10], 0);
    assert_eq!(out[11], 6);
    assert_eq!(&out[12..18], &local_mac.octets());

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
fn dispatch_packet_sendto_accepts_sockaddr_ll_loopback_destination() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_packet(&ctx);
    let addr = sockaddr_ll(ETH_P_ALL, 1);
    let payload = [0x42u8; 64];

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                addr.as_ptr() as u64,
                SOCKADDR_LL_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(payload.len() as i64)
    );

    let bad_addr = sockaddr_ll(ETH_P_ALL, 9999);
    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                payload.as_ptr() as u64,
                payload.len() as u64,
                0,
                bad_addr.as_ptr() as u64,
                SOCKADDR_LL_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Error(errno_to_i32(Errno::ENODEV))
    );
}

#[test]
fn dispatch_packet_arp_request_queues_cooked_reply() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0x8a, 0, 1]);
    let remote_mac = EthernetAddress::new([0x02, 0, 0, 0x8a, 0, 2]);
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "arpq0",
            devt: DevT::new(119, 1),
            mac: local_mac,
        },
        right: VethEndpointConfig {
            name: "arpq1",
            devt: DevT::new(119, 2),
            mac: remote_mac,
        },
        mtu: VETH_DEFAULT_MTU,
    });
    let netns = process.net_namespace().expect("test net namespace");
    netns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach arpq0");
    netns
        .attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach arpq1");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let local = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == "arpq0")
        .expect("local veth");
    let remote = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == "arpq1")
        .expect("remote veth");
    netns
        .set_device_ipv4_addr_by_ifindex(
            auth,
            local.ifindex,
            Some(Ipv4Address::new([10, 0, 0, 2])),
            Some(24),
        )
        .expect("set local addr");
    netns
        .set_device_ipv4_addr_by_ifindex(
            auth,
            remote.ifindex,
            Some(Ipv4Address::new([10, 0, 0, 1])),
            Some(24),
        )
        .expect("set remote addr");

    let fd = socket_packet(&ctx);
    let addr = sockaddr_ll(ETH_P_ARP, local.ifindex as i32);
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

    let mut request = [0u8; 28];
    request[0..2].copy_from_slice(&1u16.to_be_bytes());
    request[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
    request[4] = 6;
    request[5] = 4;
    request[6..8].copy_from_slice(&1u16.to_be_bytes());
    request[8..14].copy_from_slice(&local_mac.octets());
    request[14..18].copy_from_slice(&[10, 0, 0, 2]);
    request[24..28].copy_from_slice(&[10, 0, 0, 1]);

    assert_eq!(
        socket_req(
            NR_SENDTO,
            [
                fd as u64,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                addr.as_ptr() as u64,
                SOCKADDR_LL_BYTES as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(request.len() as i64)
    );

    let mut reply = [0u8; 28];
    let mut from = [0u8; SOCKADDR_LL_BYTES as usize];
    let mut from_len = SOCKADDR_LL_BYTES;
    assert_eq!(
        socket_req(
            NR_RECVFROM,
            [
                fd as u64,
                reply.as_mut_ptr() as u64,
                reply.len() as u64,
                MSG_DONTWAIT,
                from.as_mut_ptr() as u64,
                (&mut from_len as *mut u32) as u64,
            ],
            &ctx,
        ),
        SyscallResult::Return(reply.len() as i64)
    );
    assert_eq!(u16::from_be_bytes(reply[6..8].try_into().unwrap()), 2);
    assert_eq!(&reply[8..14], &remote_mac.octets());
    assert_eq!(&reply[14..18], &[10, 0, 0, 1]);
    assert_eq!(&reply[18..24], &local_mac.octets());
    assert_eq!(&reply[24..28], &[10, 0, 0, 2]);
    assert_eq!(from_len, SOCKADDR_LL_BYTES);
    assert_eq!(u16::from_be_bytes([from[2], from[3]]), ETH_P_ARP);
    assert_eq!(
        i32::from_le_bytes(from[4..8].try_into().unwrap()),
        local.ifindex as i32
    );
    assert_eq!(u16::from_le_bytes(from[8..10].try_into().unwrap()), 1);
    assert_eq!(from[10], 0);
    assert_eq!(from[11], 6);
    assert_eq!(&from[12..18], &remote_mac.octets());
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

    let recv_len = match socket_req(
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
    ) {
        SyscallResult::Return(n) if n > 0 => n as usize,
        other => panic!("recvfrom netlink failed: {other:?}"),
    };

    assert!(contains_bytes(&recv_buf[..recv_len], b"lo\0"));
    assert!(contains_nlmsg_type(&recv_buf[..recv_len], NLMSG_DONE));
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
fn dispatch_socket_ioctl_resolves_loopback_ifname_from_index() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifreq = [0u8; 40];
    ifreq[16..20].copy_from_slice(&1i32.to_le_bytes());

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFNAME as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(&ifreq[..3], b"lo\0");

    ifreq.fill(0);
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFNAME as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Error(ENXIO_VALUE)
    );
}

/// musl/glibc `if_indextoname` issues SIOCGIFNAME on an AF_UNIX datagram
/// socket (LTP in6_02). The interface ioctls must work on any socket fd.
#[test]
fn dispatch_socket_ioctl_siocgifname_works_on_unix_socket_fd() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = match socket_req(NR_SOCKET, [1, SOCK_DGRAM, 0, 0, 0, 0], &ctx) {
        SyscallResult::Return(fd) => fd,
        other => panic!("socket(AF_UNIX, DGRAM) failed: {other:?}"),
    };
    let mut ifreq = [0u8; 40];
    ifreq[16..20].copy_from_slice(&1i32.to_le_bytes());

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFNAME as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(&ifreq[..3], b"lo\0");
}

#[test]
fn dispatch_socket_ioctl_enumerates_interfaces_with_siocgifconf() {
    let _setup = socket_setup();
    let (_process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifconf = [0u8; 16];
    let mut ifreqs = [0u8; 80];

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFCONF as u64,
                ifconf.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(i32::from_le_bytes(ifconf[0..4].try_into().unwrap()) >= 40);

    ifconf[0..4].copy_from_slice(&(ifreqs.len() as i32).to_le_bytes());
    ifconf[8..16].copy_from_slice(&(ifreqs.as_mut_ptr() as u64).to_le_bytes());
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFCONF as u64,
                ifconf.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    let written = i32::from_le_bytes(ifconf[0..4].try_into().unwrap()) as usize;
    assert!(written >= 40);
    assert_eq!(&ifreqs[..3], b"lo\0");
    assert_eq!(
        u16::from_le_bytes(ifreqs[16..18].try_into().unwrap()),
        AF_INET as u16
    );
    assert_eq!(&ifreqs[20..24], &[127, 0, 0, 1]);
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
fn dispatch_socket_ioctl_reads_and_writes_loopback_mtu() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifreq = [0u8; 40];
    ifreq[0..2].copy_from_slice(b"lo");

    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFMTU as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(i32::from_le_bytes(ifreq[16..20].try_into().unwrap()) >= 1500);

    ifreq[16..20].copy_from_slice(&1500i32.to_le_bytes());
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCSIFMTU as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );

    ifreq[16..20].fill(0);
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFMTU as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(i32::from_le_bytes(ifreq[16..20].try_into().unwrap()), 1500);

    let netns = process.net_namespace().expect("test net namespace");
    let lo = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == "lo")
        .expect("loopback link");
    assert_eq!(lo.mtu, 1500);
}

#[test]
fn dispatch_socket_ioctl_sets_deletes_arp_entry_and_reads_hwaddr() {
    let _setup = socket_setup();
    let (process, ctx) = socket_ctx();
    let local_mac = EthernetAddress::new([0x02, 0, 0, 0x8b, 0, 1]);
    let peer_mac = EthernetAddress::new([0x02, 0, 0, 0x8b, 0, 2]);
    let peer_ip = Ipv4Address::new([10, 0, 1, 1]);
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "arpio0",
            devt: DevT::new(120, 1),
            mac: local_mac,
        },
        right: VethEndpointConfig {
            name: "arpio1",
            devt: DevT::new(120, 2),
            mac: peer_mac,
        },
        mtu: VETH_DEFAULT_MTU,
    });
    let netns = process.net_namespace().expect("test net namespace");
    netns
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach arpio0");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let local = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == "arpio0")
        .expect("local veth");
    netns
        .set_device_ipv4_addr_by_ifindex(
            auth,
            local.ifindex,
            Some(Ipv4Address::new([10, 0, 1, 2])),
            Some(24),
        )
        .expect("set local addr");

    let fd = socket_dgram(&ctx, SOCK_DGRAM);
    let mut ifreq = [0u8; 40];
    ifreq[0..6].copy_from_slice(b"arpio0");
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCGIFHWADDR as u64,
                ifreq.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert_eq!(u16::from_le_bytes(ifreq[16..18].try_into().unwrap()), 1);
    assert_eq!(&ifreq[18..24], &local_mac.octets());

    let mut set_req = arpreq(peer_ip.octets(), "arpio0", Some(peer_mac));
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCSARP as u64,
                set_req.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    let iface = netns
        .ether_ifaces_snapshot()
        .into_iter()
        .find(|iface| iface.name == "arpio0")
        .expect("ether iface");
    assert_eq!(
        iface
            .arp_entry(peer_ip, smoltcp::time::Instant::ZERO)
            .expect("installed arp")
            .mac,
        peer_mac
    );

    let mut del_req = arpreq(peer_ip.octets(), "arpio0", None);
    assert_eq!(
        socket_req(
            NR_IOCTL,
            [
                fd as u64,
                SIOCDARP as u64,
                del_req.as_mut_ptr() as u64,
                0,
                0,
                0
            ],
            &ctx,
        ),
        SyscallResult::Return(0)
    );
    assert!(iface
        .arp_entry(peer_ip, smoltcp::time::Instant::ZERO)
        .is_none());
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
