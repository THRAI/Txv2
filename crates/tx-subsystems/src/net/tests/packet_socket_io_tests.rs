use super::*;
use alloc::vec::Vec;

const ETH_P_ALL: u16 = 0x0003;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_ARP: u16 = 0x0806;

#[test]
fn packet_ingress_fans_out_raw_and_cooked_views_without_an_ipv4_address() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let namespace_identity = crate::net::create_isolated_net_namespace_for_test("packet-rx-ns")
        .expect("packet namespace");
    let namespace = namespace_identity
        .payload_cap()
        .expect("packet namespace payload");
    let pair = packet_test_veth_pair("packet-rx-left", "packet-rx-right", 121);
    namespace
        .attach_device_for_test_or_bootstrap(pair.right, None)
        .expect("attach unconfigured packet RX device");
    let link = namespace
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == pair.right.name)
        .expect("packet RX link");
    assert!(link.is_up);
    assert_eq!(link.ipv4_addr, None);

    let guard = tx_substrate::epoch::guard();
    let cooked = create_packet_socket(namespace.clone(), SocketType::Dgram, ETH_P_IP, &guard);
    cooked
        .acquire_operational()
        .expect("cooked payload")
        .bind_packet_socket(SockAddrLl::new(ETH_P_IP, link.ifindex as i32))
        .expect("bind cooked packet socket");
    let raw = create_packet_socket(namespace.clone(), SocketType::Raw, ETH_P_ALL, &guard);
    raw.acquire_operational()
        .expect("raw payload")
        .bind_packet_socket(SockAddrLl::new(ETH_P_ALL, link.ifindex as i32))
        .expect("bind raw packet socket");
    let wrong_protocol =
        create_packet_socket(namespace.clone(), SocketType::Dgram, ETH_P_ARP, &guard);

    let network_payload = [0x45, 0, 0, 20, 0, 0, 0, 0];
    let frame = ethernet_frame(
        pair.right.ops.mac_addr(),
        pair.left.ops.mac_addr(),
        ETH_P_IP,
        &network_payload,
    );
    assert_eq!(
        pair.left.ops.transmit(&frame, &guard),
        StepOutcome::Done(())
    );

    let runtime = drive_net_namespace_runtime_at(namespace, smoltcp::time::Instant::ZERO, &guard);
    assert_eq!(runtime.ifaces_seen, 1);
    assert_eq!(runtime.packets_seen, 1);
    assert!(matches!(
        step_poll_ready(&cooked, &guard),
        StepOutcome::Done(mask) if mask.intersects(PollMask::IN)
    ));
    assert!(matches!(
        step_poll_ready(&raw, &guard),
        StepOutcome::Done(mask) if mask.intersects(PollMask::IN)
    ));
    assert!(matches!(
        step_poll_ready(&wrong_protocol, &guard),
        StepOutcome::Done(mask) if !mask.intersects(PollMask::IN)
    ));

    let mut cooked_bytes = [0u8; 32];
    let StepOutcome::Done(cooked_recv) =
        step_recv_kernel_bytes(&cooked, &mut cooked_bytes, SendRecvFlags::empty(), &guard)
    else {
        panic!("cooked packet receive should complete");
    };
    assert_eq!(cooked_recv.bytes, network_payload.len());
    assert_eq!(&cooked_bytes[..cooked_recv.bytes], &network_payload);
    let cooked_source = cooked_recv.packet_source.expect("cooked source");
    assert_eq!(cooked_source.protocol, ETH_P_IP);
    assert_eq!(cooked_source.ifindex, link.ifindex as i32);
    assert_eq!(&cooked_source.addr[..6], &pair.left.ops.mac_addr().octets());

    let mut raw_bytes = [0u8; 64];
    let StepOutcome::Done(raw_recv) =
        step_recv_kernel_bytes(&raw, &mut raw_bytes, SendRecvFlags::empty(), &guard)
    else {
        panic!("raw packet receive should complete");
    };
    assert_eq!(raw_recv.bytes, frame.len());
    assert_eq!(&raw_bytes[..raw_recv.bytes], frame.as_slice());
}

#[test]
fn packet_send_uses_selected_veth_for_raw_and_cooked_frames() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();

    let namespace_identity = crate::net::create_isolated_net_namespace_for_test("packet-tx-ns")
        .expect("packet namespace");
    let namespace = namespace_identity
        .payload_cap()
        .expect("packet namespace payload");
    let pair = packet_test_veth_pair("packet-tx-left", "packet-tx-right", 122);
    namespace
        .attach_device_for_test_or_bootstrap(pair.left, None)
        .expect("attach unconfigured packet TX device");
    let link = namespace
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == pair.left.name)
        .expect("packet TX link");

    let guard = tx_substrate::epoch::guard();
    let cooked = create_packet_socket(namespace.clone(), SocketType::Dgram, ETH_P_IP, &guard);
    let mut destination_addr = [0u8; 8];
    destination_addr[..6].copy_from_slice(&pair.right.ops.mac_addr().octets());
    let destination =
        SockAddrLl::with_link_layer_addr(ETH_P_IP, link.ifindex as i32, 1, 0, destination_addr, 6);
    let network_payload = [0x45, 0, 0, 20, 1, 2, 3, 4];
    assert_eq!(
        step_packet_send(
            &cooked,
            destination,
            &network_payload,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(network_payload.len())
    );
    let cooked_frame = pair.right.ops.receive().expect("cooked peer frame");
    assert_eq!(
        &cooked_frame.as_bytes()[..6],
        &pair.right.ops.mac_addr().octets()
    );
    assert_eq!(
        &cooked_frame.as_bytes()[6..12],
        &pair.left.ops.mac_addr().octets()
    );
    assert_eq!(
        u16::from_be_bytes(cooked_frame.as_bytes()[12..14].try_into().unwrap()),
        ETH_P_IP
    );
    assert_eq!(&cooked_frame.as_bytes()[14..], &network_payload);

    let raw = create_packet_socket(namespace, SocketType::Raw, ETH_P_ALL, &guard);
    let raw_frame = ethernet_frame(
        pair.right.ops.mac_addr(),
        pair.left.ops.mac_addr(),
        ETH_P_ARP,
        &[1, 2, 3, 4],
    );
    assert_eq!(
        step_packet_send(
            &raw,
            SockAddrLl::new(ETH_P_ARP, link.ifindex as i32),
            &raw_frame,
            SendRecvFlags::empty(),
            &guard,
        ),
        StepOutcome::Done(raw_frame.len())
    );
    assert_eq!(
        pair.right.ops.receive().expect("raw peer frame").as_bytes(),
        raw_frame.as_slice()
    );
}

fn create_packet_socket(
    namespace: tx_substrate::zone::PayloadCap<crate::net::NetNamespacePayload>,
    socket_type: SocketType,
    protocol: u16,
    guard: &Guard<'_>,
) -> tx_substrate::zone::Cap<SocketIdentity> {
    let raw_socket_type = match socket_type {
        SocketType::Dgram => 2,
        SocketType::Raw => 3,
        SocketType::Stream | SocketType::SeqPacket => panic!("unsupported packet test type"),
    };
    let valid = ValidSocketType::validate(17, raw_socket_type, i32::from(u16::to_be(protocol)))
        .expect("valid packet socket");
    match step_socket_create_in_namespace(valid, namespace, guard) {
        StepOutcome::Done(socket) => socket,
        other => panic!("packet socket create failed: {other:?}"),
    }
}

fn packet_test_veth_pair(
    left_name: &'static str,
    right_name: &'static str,
    minor: u32,
) -> crate::net::device::VethPair {
    create_veth_pair_for_test_or_bootstrap(VethPairConfig {
        left: VethEndpointConfig {
            name: left_name,
            devt: DevT::new(123, minor * 2),
            mac: EthernetAddress::new([0x02, 0, 0, 0x7b, minor as u8, 1]),
        },
        right: VethEndpointConfig {
            name: right_name,
            devt: DevT::new(123, minor * 2 + 1),
            mac: EthernetAddress::new([0x02, 0, 0, 0x7b, minor as u8, 2]),
        },
        mtu: VETH_DEFAULT_MTU,
    })
}

fn ethernet_frame(
    destination: EthernetAddress,
    source: EthernetAddress,
    protocol: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&destination.octets());
    frame.extend_from_slice(&source.octets());
    frame.extend_from_slice(&protocol.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}
