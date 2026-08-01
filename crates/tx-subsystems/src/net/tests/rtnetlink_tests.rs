use super::*;

use alloc::vec;
use alloc::vec::Vec;

use crate::net::rtnetlink::{
    rtnetlink_handle_request, rtnetlink_handle_request_with_netns_resolver, NLMSG_DONE,
    NLMSG_ERROR, NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST, RTM_DELADDR, RTM_DELLINK, RTM_DELNEIGH,
    RTM_DELROUTE, RTM_GETADDR, RTM_GETLINK, RTM_GETNEIGH, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK,
    RTM_NEWNEIGH, RTM_NEWROUTE, RTM_SETLINK,
};

const NLM_F_CREATE: u16 = 0x0400;
const NLM_F_EXCL: u16 = 0x0200;
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const IFF_UP: u32 = 0x1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const NLA_F_NESTED: u16 = 0x8000;

#[test]
fn rtnetlink_getlink_dump_reports_loopback_and_done() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-getlink")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");

    let request = nlmsg(
        RTM_GETLINK,
        NLM_F_REQUEST | NLM_F_DUMP,
        10,
        &ifinfomsg(0, 0, 0),
    );
    let responses = rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &request);

    assert!(responses
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWLINK && contains_bytes(msg, b"lo\0")));
    assert!(responses.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));
}

#[test]
fn rtnetlink_getlink_single_by_name_reports_requested_link() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-getlink-one")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let bridge_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        11,
        &newlink_payload("docker0", bridge_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &bridge_req)[0]);

    let mut payload = ifinfomsg(0, 0, 0);
    push_attr_string(&mut payload, IFLA_IFNAME, "docker0");
    let request = nlmsg(RTM_GETLINK, NLM_F_REQUEST, 12, &payload);
    let responses = rtnetlink_handle_request(&ns, root, &request);

    assert_eq!(responses.len(), 1);
    assert_eq!(nlmsg_type(&responses[0]), RTM_NEWLINK);
    assert!(contains_bytes(&responses[0], b"docker0\0"));
    assert!(!contains_bytes(&responses[0], b"lo\0"));
}

#[test]
fn rtnetlink_newlink_setlink_and_newaddr_mutate_namespace_snapshot() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-mut")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let bridge_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        20,
        &newlink_payload("docker0", bridge_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &bridge_req)[0]);

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        21,
        &newlink_payload("veth0", veth_linkinfo("eth0")),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &veth_req)[0]);

    let snapshot = ns.network_snapshot();
    let bridge_ifindex = ifindex_for(&snapshot.links, "docker0");
    let veth_ifindex = ifindex_for(&snapshot.links, "veth0");
    let eth_ifindex = ifindex_for(&snapshot.links, "eth0");

    let mut set_master = ifinfomsg(veth_ifindex, 0, 0);
    push_attr_u32(&mut set_master, IFLA_MASTER, bridge_ifindex);
    let set_master_req = nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 22, &set_master);
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &set_master_req)[0]);

    let set_up_req = nlmsg(
        RTM_SETLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        23,
        &ifinfomsg(eth_ifindex, IFF_UP, IFF_UP),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &set_up_req)[0]);

    let docker_addr = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        24,
        &newaddr_payload_label(0, 16, [172, 17, 0, 1], "docker0"),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &docker_addr)[0]);

    let eth_addr = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        25,
        &newaddr_payload(eth_ifindex, 16, [172, 17, 0, 2]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &eth_addr)[0]);

    let snapshot = ns.network_snapshot();
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "docker0" && link.kind == NetDeviceKind::Bridge));
    assert!(snapshot.links.iter().any(|link| {
        link.name == "veth0" && link.kind == NetDeviceKind::Veth && link.master == Some("docker0")
    }));
    assert!(snapshot.links.iter().any(|link| {
        link.name == "eth0"
            && link.kind == NetDeviceKind::Veth
            && link.is_up
            && link.ipv4_addr == Some(Ipv4Address::new([172, 17, 0, 2]))
            && link.ipv4_prefix_len == Some(16)
    }));

    let getaddr_req = nlmsg(
        RTM_GETADDR,
        NLM_F_REQUEST | NLM_F_DUMP,
        26,
        &ifaddrmsg(0, 0),
    );
    let getaddr = rtnetlink_handle_request(&ns, root, &getaddr_req);
    assert!(getaddr
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWADDR && contains_bytes(msg, &[172, 17, 0, 1])));
    assert!(getaddr
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWADDR && contains_bytes(msg, &[172, 17, 0, 2])));

    let del_eth_addr = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        27,
        &newaddr_payload(eth_ifindex, 16, [172, 17, 0, 2]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_eth_addr)[0]);

    let snapshot = ns.network_snapshot();
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "eth0" && link.ipv4_addr.is_none()));
}

#[test]
fn rtnetlink_newlink_without_create_updates_existing_link_flags() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-newlink-set")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let bridge_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        27,
        &newlink_payload("docker0", bridge_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &bridge_req)[0]);
    let bridge_ifindex = ifindex_for(&ns.link_snapshot(), "docker0");

    let set_down_req = nlmsg(
        RTM_SETLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        28,
        &ifinfomsg(bridge_ifindex, 0, IFF_UP),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &set_down_req)[0]);
    assert!(ns
        .link_snapshot()
        .iter()
        .any(|link| link.name == "docker0" && !link.is_up));

    let set_up_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        29,
        &ifinfomsg(bridge_ifindex, IFF_UP, IFF_UP),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &set_up_req)[0]);
    assert!(ns
        .link_snapshot()
        .iter()
        .any(|link| link.name == "docker0" && link.is_up));
}

#[test]
fn rtnetlink_newlink_dummy_and_setlink_mtu() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-dummy")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        31,
        &newlink_payload("dummy0", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);

    let dummy = ns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == "dummy0")
        .expect("dummy link");
    assert_eq!(dummy.kind, NetDeviceKind::Dummy);
    assert_eq!(dummy.mtu, 1500);

    let mut set_mtu = ifinfomsg(dummy.ifindex, 0, 0);
    push_attr_u32(&mut set_mtu, IFLA_MTU, 1281);
    let set_mtu_req = nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 32, &set_mtu);
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &set_mtu_req)[0]);

    assert!(ns
        .link_snapshot()
        .iter()
        .any(|link| link.name == "dummy0" && link.mtu == 1281));
}

#[test]
fn rtnetlink_newaddr_and_deladdr_support_loopback_alias() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-lo-addr")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let add = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        33,
        &newaddr_payload(1, 24, [127, 6, 6, 6]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &add)[0]);
    assert!(ns.link_snapshot().iter().any(|link| {
        link.name == "lo"
            && link.ipv4_addr == Some(Ipv4Address::new([127, 6, 6, 6]))
            && link.ipv4_prefix_len == Some(24)
    }));

    let del = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        34,
        &newaddr_payload(1, 24, [127, 6, 6, 6]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del)[0]);
    assert!(ns.link_snapshot().iter().any(|link| {
        link.name == "lo"
            && link.ipv4_addr == Some(Ipv4Address::new([127, 0, 0, 1]))
            && link.ipv4_prefix_len == Some(8)
    }));
}

#[test]
fn rtnetlink_ipv6_newaddr_getaddr_and_deladdr_mutate_namespace_snapshot() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-ipv6-addr")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        35,
        &newlink_payload("dummy6", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);
    let ifindex = ifindex_for(&ns.link_snapshot(), "dummy6");

    let addr = [0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2];
    let add = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        36,
        &newaddr6_payload(ifindex, 64, addr),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &add)[0]);
    assert!(ns.link_snapshot().iter().any(|link| {
        link.name == "dummy6"
            && link.ipv6_addr == Some(Ipv6Address::new(addr))
            && link.ipv6_prefix_len == Some(64)
    }));

    let getaddr_req = nlmsg(
        RTM_GETADDR,
        NLM_F_REQUEST | NLM_F_DUMP,
        37,
        &ifaddrmsg_family(AF_INET6, 0, 0),
    );
    let getaddr = rtnetlink_handle_request(&ns, root, &getaddr_req);
    assert!(getaddr
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWADDR && contains_bytes(msg, &addr)));
    assert!(!getaddr
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWADDR && contains_bytes(msg, &[127, 0, 0, 1])));

    let del = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        38,
        &newaddr6_payload(ifindex, 64, addr),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del)[0]);
    assert!(ns
        .link_snapshot()
        .iter()
        .any(|link| link.name == "dummy6" && link.ipv6_addr.is_none()));
}

#[test]
fn rtnetlink_addr_flush_without_addresses_is_idempotent() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-addr-flush")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        39,
        &newlink_payload("flush0", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);
    let ifindex = ifindex_for(&ns.link_snapshot(), "flush0");

    let flush_empty = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        40,
        &ifaddrmsg_family(AF_UNSPEC, ifindex, 0),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &flush_empty)[0]);

    let ipv4 = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        41,
        &newaddr_payload(ifindex, 24, [10, 9, 8, 7]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &ipv4)[0]);
    let ipv6_addr = [0xfd, 0, 0, 9, 0, 8, 0, 7, 0, 0, 0, 0, 0, 0, 0, 6];
    let ipv6 = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        42,
        &newaddr6_payload(ifindex, 64, ipv6_addr),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &ipv6)[0]);

    let flush = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        43,
        &ifaddrmsg_family(AF_UNSPEC, ifindex, 0),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &flush)[0]);
    assert!(ns.link_snapshot().iter().any(|link| {
        link.name == "flush0" && link.ipv4_addr.is_none() && link.ipv6_addr.is_none()
    }));
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &flush_empty)[0]);

    let stale_ipv4_delete = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        44,
        &newaddr_payload(ifindex, 24, [10, 9, 8, 7]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &stale_ipv4_delete)[0]);
    let stale_ipv6_delete = nlmsg(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        45,
        &newaddr6_payload(ifindex, 64, ipv6_addr),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &stale_ipv6_delete)[0]);
}

#[test]
fn rtnetlink_getroute_and_getneigh_dump_configured_namespace_iface() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-route-neigh")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "eth-route0",
            devt: DevT::new(95, 1),
            mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 1]),
        },
        right: VethEndpointConfig {
            name: "veth-route0",
            devt: DevT::new(95, 2),
            mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 2]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach eth-route0");
    let ifindex = ifindex_for(&ns.link_snapshot(), "eth-route0");
    ns.set_device_ipv4_addr_by_ifindex(
        auth,
        ifindex,
        Some(Ipv4Address::new([172, 17, 0, 2])),
        Some(16),
    )
    .expect("set iface addr");

    let route_req = nlmsg(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 30, &rtmsg());
    let routes = rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &route_req);
    assert!(routes
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWROUTE && contains_bytes(msg, &[172, 17, 0, 0])));
    assert!(routes
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWROUTE && contains_bytes(msg, &[172, 17, 0, 2])));
    assert!(routes.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));

    let peer_ip = Ipv4Address::new([172, 17, 0, 3]);
    let peer_mac = EthernetAddress::new([0x02, 0, 0, 0x72, 0, 3]);
    let ifaces = ns.ether_ifaces_snapshot();
    assert_eq!(ifaces.len(), 1);
    ifaces[0].install_arp_for_test_or_bootstrap(
        peer_ip,
        peer_mac,
        smoltcp::time::Instant::from_secs(60),
    );

    let neigh_req = nlmsg(RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP, 31, &ndmsg(0));
    let neigh = rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &neigh_req);
    let neigh_msg = neigh
        .iter()
        .find(|msg| {
            nlmsg_type(msg) == RTM_NEWNEIGH
                && contains_bytes(msg, &peer_ip.octets())
                && contains_bytes(msg, &peer_mac.octets())
        })
        .expect("neighbor dump entry");
    assert_eq!(neigh_msg[27], 1, "ndm_type should be RTN_UNICAST");
    assert!(neigh.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWNEIGH
            && contains_bytes(msg, &peer_ip.octets())
            && contains_bytes(msg, &peer_mac.octets())
    }));
    assert!(neigh.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));
}

#[test]
fn rtnetlink_newneigh_and_delneigh_mutate_neighbor_dump() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-neigh-mut")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        35,
        &newlink_payload("dummy0", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);
    let ifindex = ifindex_for(&ns.link_snapshot(), "dummy0");

    let addr_req = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        36,
        &newaddr_payload(ifindex, 24, [192, 0, 2, 1]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &addr_req)[0]);

    let peer_ip = [192, 0, 2, 99];
    let peer_mac = [0x02, 0, 0, 0, 0, 0x63];
    let add_req = nlmsg(
        RTM_NEWNEIGH,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        37,
        &newneigh_payload(ifindex, peer_ip, peer_mac),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &add_req)[0]);

    let neigh_req = nlmsg(RTM_GETNEIGH, NLM_F_REQUEST | NLM_F_DUMP, 38, &ndmsg(0));
    let neigh = rtnetlink_handle_request(&ns, root, &neigh_req);
    assert!(neigh.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWNEIGH
            && contains_bytes(msg, &peer_ip)
            && contains_bytes(msg, &peer_mac)
    }));

    let del_req = nlmsg(
        RTM_DELNEIGH,
        NLM_F_REQUEST | NLM_F_ACK,
        39,
        &delneigh_payload(ifindex, peer_ip),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_req)[0]);

    let after_del = rtnetlink_handle_request(&ns, root, &neigh_req);
    assert!(!after_del
        .iter()
        .any(|msg| nlmsg_type(msg) == RTM_NEWNEIGH && contains_bytes(msg, &peer_ip)));
    assert!(after_del.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));
}

#[test]
fn rtnetlink_newroute_delroute_default_gateway_updates_namespace_routes() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-default-route")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let auth = NetAdminAuthority::for_test_or_bootstrap();
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "eth-default0",
            devt: DevT::new(95, 11),
            mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 11]),
        },
        right: VethEndpointConfig {
            name: "veth-default0",
            devt: DevT::new(95, 12),
            mac: EthernetAddress::new([0x02, 0, 0, 0x72, 0, 12]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach eth-default0");
    let ifindex = ifindex_for(&ns.link_snapshot(), "eth-default0");
    ns.set_device_ipv4_addr_by_ifindex(
        auth,
        ifindex,
        Some(Ipv4Address::new([172, 17, 0, 2])),
        Some(16),
    )
    .expect("set iface addr");

    let route_req = nlmsg(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        60,
        &newroute_payload(0, None, Some([172, 17, 0, 1]), ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &route_req)[0]);

    let iface = ns.ether_ifaces_snapshot()[0];
    assert_eq!(
        iface.common.gateway(),
        Some(Ipv4Address::new([172, 17, 0, 1]))
    );

    let dump_req = nlmsg(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 61, &rtmsg());
    let routes = rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &dump_req);
    assert!(routes.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWROUTE
            && contains_bytes(msg, &[172, 17, 0, 1])
            && contains_bytes(msg, &ifindex.to_le_bytes())
    }));

    let del_req = nlmsg(
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        62,
        &newroute_payload(0, None, Some([172, 17, 0, 1]), ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, crate::cred::Cred::root(), &del_req)[0]);
    assert_eq!(ns.ether_ifaces_snapshot()[0].common.gateway(), None);
}

#[test]
fn rtnetlink_newroute_delroute_ipv6_updates_namespace_routes6() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-ipv6-route")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    // A dummy link carrying a v6 address, so the gateway is on-link and the
    // route's oif can be inferred from its connected route.
    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        80,
        &newlink_payload("dummy6r", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);
    let ifindex = ifindex_for(&ns.link_snapshot(), "dummy6r");
    let link_addr = [0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
    let add_addr = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        81,
        &newaddr6_payload(ifindex, 64, link_addr),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &add_addr)[0]);

    // Add an off-link /64 reachable via a gateway inside the connected subnet.
    let dst = [0xfd, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let gateway = [0xfd, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xfe];
    let route_req = nlmsg(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        82,
        &newroute6_payload(64, Some(dst), Some(gateway), ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &route_req)[0]);

    let decision = ns
        .best_ipv6_route(Ipv6Address::new([
            0xfd, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5,
        ]))
        .expect("v6 route decision");
    assert_eq!(decision.oif_name, "dummy6r");
    assert_eq!(decision.next_hop, Ipv6Address::new(gateway));
    assert_eq!(decision.prefix_len, 64);

    // The GETROUTE dump (family AF_INET6) must carry the route's gateway bytes.
    let dump_req = nlmsg(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 83, &rtmsg6());
    let routes = rtnetlink_handle_request(&ns, root, &dump_req);
    assert!(routes.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWROUTE
            && contains_bytes(msg, &gateway)
            && contains_bytes(msg, &ifindex.to_le_bytes())
    }));

    // Deleting the route removes it from the FIB.
    let del_req = nlmsg(
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        84,
        &newroute6_payload(64, Some(dst), Some(gateway), ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_req)[0]);
    assert!(ns
        .best_ipv6_route(Ipv6Address::new([
            0xfd, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5,
        ]))
        .is_none());
}

#[test]
fn namespace_add_ipv6_route_longest_prefix_wins() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-ipv6-lpm")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let auth = NetAdminAuthority::for_test_or_bootstrap();

    // A dummy link with a v6 address so route oifs resolve/validate.
    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "eth6lpm0",
            devt: DevT::new(96, 21),
            mac: EthernetAddress::new([0x02, 0, 0, 0x76, 0, 21]),
        },
        right: VethEndpointConfig {
            name: "veth6lpm0",
            devt: DevT::new(96, 22),
            mac: EthernetAddress::new([0x02, 0, 0, 0x76, 0, 22]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach eth6lpm0");
    let ifindex = ifindex_for(&ns.link_snapshot(), "eth6lpm0");
    ns.set_device_ipv6_addr_by_ifindex(
        auth,
        ifindex,
        Some(Ipv6Address::new([
            0xfd, 0, 0, 0xa, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ])),
        Some(64),
    )
    .expect("set v6 addr");

    // A broad /32 and a specific /64 both cover the target; /64 must win.
    ns.add_ipv6_route(
        auth,
        crate::net::NetNamespaceRoute6Config {
            dst: Ipv6Address::new([0xfd, 0, 0, 0xb, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            prefix_len: 32,
            gateway: None,
            oif_name: Some("eth6lpm0"),
            preferred_src: None,
            table: 254,
            protocol: 4,
            scope: 253,
            route_type: 1,
        },
    )
    .expect("add /32 route");
    ns.add_ipv6_route(
        auth,
        crate::net::NetNamespaceRoute6Config {
            dst: Ipv6Address::new([0xfd, 0, 0, 0xb, 0, 0xc, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            prefix_len: 64,
            gateway: None,
            oif_name: Some("eth6lpm0"),
            preferred_src: None,
            table: 254,
            protocol: 4,
            scope: 253,
            route_type: 1,
        },
    )
    .expect("add /64 route");

    let target = Ipv6Address::new([0xfd, 0, 0, 0xb, 0, 0xc, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
    let decision = ns.best_ipv6_route(target).expect("v6 route decision");
    assert_eq!(decision.prefix_len, 64);
    assert_eq!(decision.oif_name, "eth6lpm0");
}

#[test]
fn namespace_route6_snapshot_includes_connected_route() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-ipv6-connected")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let auth = NetAdminAuthority::for_test_or_bootstrap();

    let pair = create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: "eth6conn0",
            devt: DevT::new(96, 31),
            mac: EthernetAddress::new([0x02, 0, 0, 0x76, 0, 31]),
        },
        right: VethEndpointConfig {
            name: "veth6conn0",
            devt: DevT::new(96, 32),
            mac: EthernetAddress::new([0x02, 0, 0, 0x76, 0, 32]),
        },
        mtu: VETH_DEFAULT_MTU,
    });
    ns.attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach eth6conn0");
    let ifindex = ifindex_for(&ns.link_snapshot(), "eth6conn0");
    ns.set_device_ipv6_addr_by_ifindex(
        auth,
        ifindex,
        Some(Ipv6Address::new([
            0xfd, 0, 0, 0xd, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3,
        ])),
        Some(64),
    )
    .expect("set v6 addr");

    // The connected /64 (network address, gateway None, oif = the link) is
    // synthesized into the snapshot from the link's v6 address.
    let network = Ipv6Address::new([0xfd, 0, 0, 0xd, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert!(ns.route6_snapshot().iter().any(|route| {
        route.dst == network
            && route.prefix_len == 64
            && route.oif_name == Some("eth6conn0")
            && route.gateway.is_none()
    }));
}

#[test]
fn rtnetlink_delroute_connected_route_is_idempotent_until_addr_changes() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-route-flush")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let dummy_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        65,
        &newlink_payload("routeflush0", dummy_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &dummy_req)[0]);
    let ifindex = ifindex_for(&ns.link_snapshot(), "routeflush0");

    let addr_req = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        66,
        &newaddr_payload(ifindex, 24, [192, 0, 2, 7]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &addr_req)[0]);
    assert!(ns.route_snapshot().iter().any(|route| {
        route.dst == Ipv4Address::new([192, 0, 2, 0])
            && route.prefix_len == 24
            && route.oif_name == Some("routeflush0")
            && route.gateway.is_none()
    }));

    let del_connected_no_ack = nlmsg(
        RTM_DELROUTE,
        NLM_F_REQUEST,
        67,
        &newroute_payload(24, Some([192, 0, 2, 0]), None, ifindex),
    );
    assert!(rtnetlink_handle_request(&ns, root, &del_connected_no_ack).is_empty());
    assert!(!ns.route_snapshot().iter().any(|route| {
        route.dst == Ipv4Address::new([192, 0, 2, 0])
            && route.prefix_len == 24
            && route.oif_name == Some("routeflush0")
            && route.gateway.is_none()
    }));

    let del_connected = nlmsg(
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        68,
        &newroute_payload(24, Some([192, 0, 2, 0]), None, ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_connected)[0]);
    assert!(!ns.route_snapshot().iter().any(|route| {
        route.dst == Ipv4Address::new([192, 0, 2, 0])
            && route.prefix_len == 24
            && route.oif_name == Some("routeflush0")
            && route.gateway.is_none()
    }));
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_connected)[0]);

    let restore_addr = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        69,
        &newaddr_payload(ifindex, 24, [192, 0, 2, 7]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &restore_addr)[0]);
    assert!(ns.route_snapshot().iter().any(|route| {
        route.dst == Ipv4Address::new([192, 0, 2, 0])
            && route.prefix_len == 24
            && route.oif_name == Some("routeflush0")
            && route.gateway.is_none()
    }));
}

#[test]
fn rtnetlink_newroute_infers_loopback_oif_for_loopback_gateway() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-route-lo")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();
    let lo_ifindex = ifindex_for(&ns.link_snapshot(), "lo");

    let add_req = nlmsg(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        63,
        &newroute_payload(32, Some([10, 6, 6, 6]), Some([127, 0, 0, 1]), 0),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &add_req)[0]);

    let dump_req = nlmsg(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 64, &rtmsg());
    let routes = rtnetlink_handle_request(&ns, root, &dump_req);
    assert!(routes.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWROUTE
            && contains_bytes(msg, &[10, 6, 6, 6])
            && contains_bytes(msg, &[127, 0, 0, 1])
            && contains_bytes(msg, &lo_ifindex.to_le_bytes())
    }));
}

#[test]
fn net_namespace_fd_round_trips_payload() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-fd")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");

    let file =
        crate::net::net_namespace_open_file_from_payload(ns.clone()).expect("net namespace fd");
    let resolved = crate::net::net_namespace_payload_from_file(&file).expect("payload from fd");

    assert_eq!(resolved.key(), ns.key());
}

#[test]
fn rtnetlink_newlink_veth_without_peer_attr_uses_default_eth_peer() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-veth-default-peer")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        26,
        &newlink_payload("veth0", veth_linkinfo_without_peer()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &veth_req)[0]);

    let snapshot = ns.network_snapshot();
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "veth0" && link.kind == NetDeviceKind::Veth));
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "eth0" && link.kind == NetDeviceKind::Veth));
}

#[test]
fn rtnetlink_newlink_veth_accepts_peer_payload_without_ifinfomsg_header() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-veth-busybox-peer")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        27,
        &newlink_payload(
            "ltp_ns_veth1",
            veth_linkinfo_peer_attrs_only("ltp_ns_veth2"),
        ),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &veth_req)[0]);

    let snapshot = ns.network_snapshot();
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "ltp_ns_veth1" && link.kind == NetDeviceKind::Veth));
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "ltp_ns_veth2" && link.kind == NetDeviceKind::Veth));
}

#[test]
fn rtnetlink_setlink_netns_fd_moves_veth_peer_between_namespaces() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let host = crate::net::create_isolated_net_namespace_for_test("rtnl-host")
        .expect("host namespace")
        .payload_cap()
        .expect("host namespace payload");
    let container = crate::net::create_isolated_net_namespace_for_test("rtnl-container")
        .expect("container namespace")
        .payload_cap()
        .expect("container namespace payload");
    let root = crate::cred::Cred::root();

    let bridge_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        40,
        &newlink_payload("docker0", bridge_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&host, root, &bridge_req)[0]);

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        41,
        &newlink_payload("veth0", veth_linkinfo("eth0")),
    );
    assert_ack_ok(&rtnetlink_handle_request(&host, root, &veth_req)[0]);

    let host_snapshot = host.network_snapshot();
    let bridge_ifindex = ifindex_for(&host_snapshot.links, "docker0");
    let veth_ifindex = ifindex_for(&host_snapshot.links, "veth0");
    let eth_ifindex = ifindex_for(&host_snapshot.links, "eth0");

    let mut set_master = ifinfomsg(veth_ifindex, 0, 0);
    push_attr_u32(&mut set_master, IFLA_MASTER, bridge_ifindex);
    let set_master_req = nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 42, &set_master);
    assert_ack_ok(&rtnetlink_handle_request(&host, root, &set_master_req)[0]);

    let mut move_eth = ifinfomsg(eth_ifindex, 0, 0);
    push_attr_i32(&mut move_eth, IFLA_NET_NS_FD, 7);
    let move_req = nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 43, &move_eth);
    let mut resolve_netns_fd = |fd: i32| (fd == 7).then(|| container.clone());
    assert_ack_ok(
        &rtnetlink_handle_request_with_netns_resolver(
            &host,
            root,
            &move_req,
            &mut resolve_netns_fd,
        )[0],
    );

    let host_snapshot = host.network_snapshot();
    assert!(host_snapshot.links.iter().all(|link| link.name != "eth0"));
    assert!(host_snapshot.links.iter().any(|link| {
        link.name == "veth0" && link.master == Some("docker0") && link.kind == NetDeviceKind::Veth
    }));
    let bridge_info = host_snapshot
        .bridges
        .iter()
        .find(|bridge| bridge.name == "docker0")
        .expect("docker0 bridge");
    assert_eq!(bridge_info.ports, alloc::vec!["veth0"]);

    let container_snapshot = container.network_snapshot();
    let moved_eth = container_snapshot
        .links
        .iter()
        .find(|link| link.name == "eth0")
        .expect("eth0 moved to container namespace");
    assert_eq!(moved_eth.kind, NetDeviceKind::Veth);
    assert_eq!(moved_eth.master, None);

    let eth_ifindex = moved_eth.ifindex;
    let set_up_req = nlmsg(
        RTM_SETLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        44,
        &ifinfomsg(eth_ifindex, IFF_UP, IFF_UP),
    );
    assert_ack_ok(&rtnetlink_handle_request(&container, root, &set_up_req)[0]);

    let addr_req = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        45,
        &newaddr_payload(eth_ifindex, 16, [172, 17, 0, 2]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&container, root, &addr_req)[0]);

    let container_snapshot = container.network_snapshot();
    assert!(container_snapshot.links.iter().any(|link| {
        link.name == "eth0"
            && link.is_up
            && link.ipv4_addr == Some(Ipv4Address::new([172, 17, 0, 2]))
            && link.ipv4_prefix_len == Some(16)
    }));
}

#[test]
fn rtnetlink_setlink_netns_pid_moves_veth_peer_between_namespaces() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let host = crate::net::create_isolated_net_namespace_for_test("rtnl-host-pid")
        .expect("host namespace")
        .payload_cap()
        .expect("host namespace payload");
    let container = crate::net::create_isolated_net_namespace_for_test("rtnl-container-pid")
        .expect("container namespace")
        .payload_cap()
        .expect("container namespace payload");
    let root = crate::cred::Cred::root();

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        50,
        &newlink_payload("veth0", veth_linkinfo("eth0")),
    );
    assert_ack_ok(&rtnetlink_handle_request(&host, root, &veth_req)[0]);

    let eth_ifindex = ifindex_for(&host.network_snapshot().links, "eth0");
    let mut move_eth = ifinfomsg(eth_ifindex, 0, 0);
    push_attr_i32(&mut move_eth, IFLA_NET_NS_PID, 42);
    let move_req = nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 51, &move_eth);
    let mut resolve_netns_fd = |_fd: i32| None;
    let mut resolve_netns_pid = |pid: u32| (pid == 42).then(|| container.clone());
    assert_ack_ok(
        &crate::net::rtnetlink::rtnetlink_handle_request_with_netns_resolvers(
            &host,
            root,
            &move_req,
            &mut resolve_netns_fd,
            &mut resolve_netns_pid,
        )[0],
    );

    assert!(host
        .network_snapshot()
        .links
        .iter()
        .all(|link| link.name != "eth0"));
    assert!(container
        .network_snapshot()
        .links
        .iter()
        .any(|link| link.name == "eth0" && link.kind == NetDeviceKind::Veth));
}

#[test]
fn rtnetlink_setlink_master_zero_detaches_bridge_port() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-detach-master")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let bridge_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        70,
        &newlink_payload("docker0", bridge_linkinfo()),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &bridge_req)[0]);
    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        71,
        &newlink_payload("veth0", veth_linkinfo("eth0")),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &veth_req)[0]);

    let snapshot = ns.network_snapshot();
    let bridge_ifindex = ifindex_for(&snapshot.links, "docker0");
    let veth_ifindex = ifindex_for(&snapshot.links, "veth0");
    let mut set_master = ifinfomsg(veth_ifindex, 0, 0);
    push_attr_u32(&mut set_master, IFLA_MASTER, bridge_ifindex);
    assert_ack_ok(
        &rtnetlink_handle_request(
            &ns,
            root,
            &nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 72, &set_master),
        )[0],
    );
    assert!(ns
        .network_snapshot()
        .links
        .iter()
        .any(|link| link.name == "veth0" && link.master == Some("docker0")));

    let mut detach_master = ifinfomsg(veth_ifindex, 0, 0);
    push_attr_u32(&mut detach_master, IFLA_MASTER, 0);
    assert_ack_ok(
        &rtnetlink_handle_request(
            &ns,
            root,
            &nlmsg(RTM_SETLINK, NLM_F_REQUEST | NLM_F_ACK, 73, &detach_master),
        )[0],
    );

    let snapshot = ns.network_snapshot();
    assert!(snapshot
        .links
        .iter()
        .any(|link| link.name == "veth0" && link.master.is_none()));
    assert_eq!(
        snapshot
            .bridges
            .iter()
            .find(|bridge| bridge.name == "docker0")
            .expect("docker bridge")
            .ports,
        Vec::<&'static str>::new()
    );
}

#[test]
fn rtnetlink_dellink_removes_dynamic_device_routes_and_netfilter_rules() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    reset_netfilter_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-delete-link")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");
    let root = crate::cred::Cred::root();

    let veth_req = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        80,
        &newlink_payload("veth-del0", veth_linkinfo("eth-del0")),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &veth_req)[0]);

    let eth_ifindex = ifindex_for(&ns.network_snapshot().links, "eth-del0");
    let addr_req = nlmsg(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        81,
        &newaddr_payload(eth_ifindex, 16, [172, 17, 0, 2]),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &addr_req)[0]);
    let route_req = nlmsg(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        82,
        &newroute_payload(0, None, Some([172, 17, 0, 1]), eth_ifindex),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &route_req)[0]);
    crate::net::netfilter::add_masquerade_rule_in_namespace_for_test_or_bootstrap(
        &ns,
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "eth-del0",
    )
    .expect("masquerade rule");
    crate::net::netfilter::add_dnat_rule_in_namespace_for_test_or_bootstrap(
        &ns,
        NetfilterConntrackProtocol::Tcp,
        Ipv4Address::new([10, 0, 2, 15]),
        8080,
        Ipv4Address::new([172, 17, 0, 2]),
        80,
    )
    .expect("dnat rule");
    assert_eq!(
        crate::net::netfilter_rules_snapshot_for_namespace(&ns).len(),
        2
    );
    assert!(ns
        .route_snapshot()
        .iter()
        .any(|route| route.oif_name == Some("eth-del0")));

    let del_req = nlmsg(
        RTM_DELLINK,
        NLM_F_REQUEST | NLM_F_ACK,
        83,
        &ifinfomsg(eth_ifindex, 0, 0),
    );
    assert_ack_ok(&rtnetlink_handle_request(&ns, root, &del_req)[0]);

    let snapshot = ns.network_snapshot();
    assert!(snapshot.links.iter().all(|link| link.name != "eth-del0"));
    assert!(snapshot.links.iter().any(|link| link.name == "veth-del0"));
    assert!(ns
        .route_snapshot()
        .iter()
        .all(|route| route.oif_name != Some("eth-del0")));
    assert!(crate::net::netfilter_rules_snapshot_for_namespace(&ns).is_empty());
}

#[test]
fn rtnetlink_newlink_requires_cap_net_admin() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let ns = crate::net::create_isolated_net_namespace_for_test("rtnl-deny")
        .expect("namespace")
        .payload_cap()
        .expect("namespace payload");

    let request = nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        30,
        &newlink_payload("denied0", bridge_linkinfo()),
    );
    let response = rtnetlink_handle_request(&ns, unprivileged_cred(), &request);

    assert_ack_error(&response[0], -1);
    assert!(ns
        .network_snapshot()
        .links
        .iter()
        .all(|link| link.name != "denied0"));
}

fn newlink_payload(name: &str, linkinfo: Vec<u8>) -> Vec<u8> {
    let mut payload = ifinfomsg(0, 0, 0);
    push_attr_string(&mut payload, IFLA_IFNAME, name);
    push_attr(&mut payload, IFLA_LINKINFO | NLA_F_NESTED, &linkinfo);
    payload
}

fn bridge_linkinfo() -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "bridge");
    out
}

fn dummy_linkinfo() -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "dummy");
    out
}

fn veth_linkinfo(peer_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "veth");
    let mut data = Vec::new();
    let mut peer = ifinfomsg(0, 0, 0);
    push_attr_string(&mut peer, IFLA_IFNAME, peer_name);
    push_attr(&mut data, VETH_INFO_PEER | NLA_F_NESTED, &peer);
    push_attr(&mut out, IFLA_INFO_DATA | NLA_F_NESTED, &data);
    out
}

fn veth_linkinfo_peer_attrs_only(peer_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "veth");
    let mut data = Vec::new();
    let mut peer = Vec::new();
    push_attr_string(&mut peer, IFLA_IFNAME, peer_name);
    push_attr(&mut data, VETH_INFO_PEER | NLA_F_NESTED, &peer);
    push_attr(&mut out, IFLA_INFO_DATA | NLA_F_NESTED, &data);
    out
}

fn veth_linkinfo_without_peer() -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "veth");
    push_attr(&mut out, IFLA_INFO_DATA | NLA_F_NESTED, &[]);
    out
}

fn newaddr_payload(ifindex: u32, prefix_len: u8, addr: [u8; 4]) -> Vec<u8> {
    let mut payload = vec![AF_INET, prefix_len, 0, 0];
    payload.extend_from_slice(&ifindex.to_le_bytes());
    push_attr(&mut payload, IFA_ADDRESS, &addr);
    push_attr(&mut payload, IFA_LOCAL, &addr);
    payload
}

fn newaddr6_payload(ifindex: u32, prefix_len: u8, addr: [u8; 16]) -> Vec<u8> {
    let mut payload = vec![AF_INET6, prefix_len, 0, 0];
    payload.extend_from_slice(&ifindex.to_le_bytes());
    push_attr(&mut payload, IFA_ADDRESS, &addr);
    push_attr(&mut payload, IFA_LOCAL, &addr);
    payload
}

fn newaddr_payload_label(ifindex: u32, prefix_len: u8, addr: [u8; 4], label: &str) -> Vec<u8> {
    let mut payload = newaddr_payload(ifindex, prefix_len, addr);
    push_attr_string(&mut payload, IFA_LABEL, label);
    payload
}

fn ifinfomsg(index: u32, flags: u32, change: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&(index as i32).to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&change.to_le_bytes());
    payload
}

fn ifaddrmsg(index: u32, prefix_len: u8) -> Vec<u8> {
    ifaddrmsg_family(AF_INET, index, prefix_len)
}

fn ifaddrmsg_family(family: u8, index: u32, prefix_len: u8) -> Vec<u8> {
    let mut payload = vec![family, prefix_len, 0, 0];
    payload.extend_from_slice(&index.to_le_bytes());
    payload
}

fn rtmsg() -> Vec<u8> {
    let mut payload = vec![2, 0, 0, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload
}

fn newroute_payload(
    prefix_len: u8,
    dst: Option<[u8; 4]>,
    gateway: Option<[u8; 4]>,
    ifindex: u32,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(prefix_len);
    payload.push(0);
    payload.push(0);
    payload.push(254);
    payload.push(4);
    payload.push(if gateway.is_some() { 0 } else { 253 });
    payload.push(1);
    payload.extend_from_slice(&0u32.to_le_bytes());
    if let Some(dst) = dst {
        push_attr(&mut payload, RTA_DST, &dst);
    }
    if let Some(gateway) = gateway {
        push_attr(&mut payload, RTA_GATEWAY, &gateway);
    }
    if ifindex != 0 {
        push_attr_u32(&mut payload, RTA_OIF, ifindex);
    }
    payload
}

fn rtmsg6() -> Vec<u8> {
    let mut payload = vec![AF_INET6, 0, 0, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload
}

fn newroute6_payload(
    prefix_len: u8,
    dst: Option<[u8; 16]>,
    gateway: Option<[u8; 16]>,
    ifindex: u32,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(AF_INET6);
    payload.push(prefix_len);
    payload.push(0);
    payload.push(0);
    payload.push(254);
    payload.push(4);
    payload.push(if gateway.is_some() { 0 } else { 253 });
    payload.push(1);
    payload.extend_from_slice(&0u32.to_le_bytes());
    if let Some(dst) = dst {
        push_attr(&mut payload, RTA_DST, &dst);
    }
    if let Some(gateway) = gateway {
        push_attr(&mut payload, RTA_GATEWAY, &gateway);
    }
    if ifindex != 0 {
        push_attr_u32(&mut payload, RTA_OIF, ifindex);
    }
    payload
}

fn ndmsg(ifindex: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(0);
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&(ifindex as i32).to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.push(0);
    payload.push(0);
    payload
}

fn newneigh_payload(ifindex: u32, ip: [u8; 4], mac: [u8; 6]) -> Vec<u8> {
    let mut payload = ndmsg(ifindex);
    payload[8..10].copy_from_slice(&2u16.to_le_bytes());
    push_attr(&mut payload, NDA_DST, &ip);
    push_attr(&mut payload, NDA_LLADDR, &mac);
    payload
}

fn delneigh_payload(ifindex: u32, ip: [u8; 4]) -> Vec<u8> {
    let mut payload = ndmsg(ifindex);
    push_attr(&mut payload, NDA_DST, &ip);
    payload
}

fn nlmsg(kind: u16, flags: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut out = Vec::new();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(&mut out);
    out
}

fn push_attr_string(out: &mut Vec<u8>, kind: u16, value: &str) {
    let mut payload = Vec::from(value.as_bytes());
    payload.push(0);
    push_attr(out, kind, &payload);
}

fn push_attr_u32(out: &mut Vec<u8>, kind: u16, value: u32) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn push_attr_i32(out: &mut Vec<u8>, kind: u16, value: i32) {
    push_attr(out, kind, &value.to_le_bytes());
}

fn push_attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    pad_to_align4(out);
}

fn pad_to_align4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn nlmsg_type(msg: &[u8]) -> u16 {
    u16::from_le_bytes([msg[4], msg[5]])
}

fn assert_ack_ok(msg: &[u8]) {
    assert_eq!(nlmsg_type(msg), NLMSG_ERROR);
    assert_eq!(i32::from_le_bytes(msg[16..20].try_into().unwrap()), 0);
}

fn assert_ack_error(msg: &[u8], error: i32) {
    assert_eq!(nlmsg_type(msg), NLMSG_ERROR);
    assert_eq!(i32::from_le_bytes(msg[16..20].try_into().unwrap()), error);
}

fn ifindex_for(links: &[NetNamespaceLinkInfo], name: &str) -> u32 {
    links
        .iter()
        .find(|link| link.name == name)
        .map(|link| link.ifindex)
        .expect("link ifindex")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn unprivileged_cred() -> crate::cred::Cred {
    crate::cred::Cred {
        uid: crate::cred::Uid(1000),
        euid: crate::cred::Uid(1000),
        suid: crate::cred::Uid(1000),
        gid: crate::cred::Gid(1000),
        egid: crate::cred::Gid(1000),
        sgid: crate::cred::Gid(1000),
        effective_caps: crate::cred::CapabilitySet::EMPTY,
        permitted_caps: crate::cred::CapabilitySet::EMPTY,
    }
}
