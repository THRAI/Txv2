use super::*;

use alloc::vec::Vec;

use crate::net::rtnetlink::{
    rtnetlink_handle_request, rtnetlink_handle_request_with_netns_resolver, NLMSG_DONE,
    NLMSG_ERROR, NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST, RTM_DELLINK, RTM_DELROUTE, RTM_GETADDR,
    RTM_GETLINK, RTM_GETNEIGH, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWNEIGH, RTM_NEWROUTE,
    RTM_SETLINK,
};

const NLM_F_CREATE: u16 = 0x0400;
const NLM_F_EXCL: u16 = 0x0200;
const IFF_UP: u32 = 0x1;
const IFLA_IFNAME: u16 = 3;
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
    assert!(neigh.iter().any(|msg| {
        nlmsg_type(msg) == RTM_NEWNEIGH
            && contains_bytes(msg, &peer_ip.octets())
            && contains_bytes(msg, &peer_mac.octets())
    }));
    assert!(neigh.iter().any(|msg| nlmsg_type(msg) == NLMSG_DONE));
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
    add_masquerade_rule_for_test_or_bootstrap(
        NetfilterIpv4Cidr {
            addr: Ipv4Address::new([172, 17, 0, 0]),
            prefix_len: 16,
        },
        "eth-del0",
    )
    .expect("masquerade rule");
    add_dnat_rule_for_test_or_bootstrap(
        NetfilterConntrackProtocol::Tcp,
        Ipv4Address::new([10, 0, 2, 15]),
        8080,
        Ipv4Address::new([172, 17, 0, 2]),
        80,
    )
    .expect("dnat rule");
    assert_eq!(netfilter_rules_snapshot().len(), 2);
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
    assert!(netfilter_rules_snapshot().is_empty());
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

fn veth_linkinfo_without_peer() -> Vec<u8> {
    let mut out = Vec::new();
    push_attr_string(&mut out, IFLA_INFO_KIND, "veth");
    push_attr(&mut out, IFLA_INFO_DATA | NLA_F_NESTED, &[]);
    out
}

fn newaddr_payload(ifindex: u32, prefix_len: u8, addr: [u8; 4]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(prefix_len);
    payload.push(0);
    payload.push(0);
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
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(prefix_len);
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(&index.to_le_bytes());
    payload
}

fn rtmsg() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.push(0);
    payload.push(0);
    payload.push(0);
    payload.push(0);
    payload.push(0);
    payload.push(0);
    payload.push(0);
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
    while out.len() % 4 != 0 {
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
