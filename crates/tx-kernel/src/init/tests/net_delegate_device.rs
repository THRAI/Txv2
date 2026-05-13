use std::vec::Vec;

use tx_substrate::step::StepOutcome;
use tx_subsystems::net::delegate::{net_delegate_clear, DelegateWireSet, NetDelegateTaskConfig};
use tx_subsystems::net::device::{
    EthernetAddress, VirtioNetIrqEvent, VIRTIO_NET0_DEVICE, VIRTIO_NET0_REGISTRATION,
};
use tx_subsystems::net::facade::{socket_create_facade, SocketBindOps, SocketRecvOps};
use tx_subsystems::net::packet::RxFrame;
use tx_subsystems::net::protocol::UdpTxDatagram;
use tx_subsystems::net::step_send_to_kernel_bytes;
use tx_subsystems::net::structure::{
    IpEndpoint, Ipv4Address, KernelSockAddr, RecvWireSet, SendRecvFlags, SockAddrIn,
};

use super::{setup, CoreInit, TestPlatform};

const BOOT_ETH_IPV4_FOR_TEST: Ipv4Address = Ipv4Address::new([10, 0, 2, 15]);
const REMOTE_IPV4_FOR_TEST: Ipv4Address = Ipv4Address::new([10, 0, 2, 44]);
const SERVER_PORT: u16 = 55_043;
const REMOTE_PORT: u16 = 45_043;
const CLIENT_PORT: u16 = 55_143;

#[test]
fn boot_net_runtime_delivers_virtio_rx_udp_to_socket() {
    let _serial = setup();
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    let server = {
        let guard = tx_substrate::epoch::guard();
        let output = match socket_create_facade(2, 2, 17, &guard) {
            StepOutcome::Done(output) => output,
            StepOutcome::Err(errno) => panic!("socket create failed: {errno:?}"),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                panic!("socket create did not complete synchronously")
            }
        };
        assert_eq!(
            output
                .handle
                .bind_capability(KernelSockAddr::V4(SockAddrIn::new(
                    SERVER_PORT,
                    BOOT_ETH_IPV4_FOR_TEST,
                )))
                .bind(&guard),
            StepOutcome::Done(())
        );
        output.handle
    };

    CoreInit::<TestPlatform>::submit_net_delegate_task_for_test(NetDelegateTaskConfig::run_steps(
        1,
    ))
    .expect("delegate task submit");
    let parked = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("delegate should park before rx irq");
    assert_eq!(parked.stats.completed, 0);

    let payload = b"hello";
    let packet = UdpTxDatagram {
        dst: IpEndpoint::new(BOOT_ETH_IPV4_FOR_TEST, SERVER_PORT),
        payload: payload.to_vec(),
    }
    .emit_ipv4_packet(IpEndpoint::new(REMOTE_IPV4_FOR_TEST, REMOTE_PORT))
    .expect("udp ipv4 packet");
    let frame = ethernet_ipv4_frame(
        VIRTIO_NET0_REGISTRATION.ops.mac_addr(),
        EthernetAddress::new([0x02, 0, 0, 0, 0, 0x44]),
        packet.as_bytes(),
    );

    let injected = VIRTIO_NET0_DEVICE.inject_rx_for_test_or_irq(RxFrame::new(frame));
    assert!(injected.accepted);
    let irq = VIRTIO_NET0_DEVICE.handle_irq(VirtioNetIrqEvent::RxAvailable);
    assert!(irq.rx_ready);
    assert!(irq.poll_wakes >= 1, "parked delegate should be woken");

    let driven = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("delegate should consume device poll");
    assert!(
        driven.stats.completed >= 1,
        "bounded delegate should finish after device POLL"
    );
    assert_eq!(VIRTIO_NET0_DEVICE.rx_len(), 0);
    assert!(
        server.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0,
        "device RX should publish socket receive readiness"
    );

    let guard = tx_substrate::epoch::guard();
    assert_eq!(
        server
            .recv_capability(payload.len(), SendRecvFlags::empty())
            .recv(&guard),
        StepOutcome::Done(payload.len())
    );
}

#[test]
fn boot_net_runtime_transmits_udp_socket_tx_to_virtio_device() {
    let _serial = setup();
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    let client = {
        let guard = tx_substrate::epoch::guard();
        let output = match socket_create_facade(2, 2, 17, &guard) {
            StepOutcome::Done(output) => output,
            StepOutcome::Err(errno) => panic!("socket create failed: {errno:?}"),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                panic!("socket create did not complete synchronously")
            }
        };
        assert_eq!(
            output
                .handle
                .bind_capability(KernelSockAddr::V4(SockAddrIn::new(
                    CLIENT_PORT,
                    BOOT_ETH_IPV4_FOR_TEST,
                )))
                .bind(&guard),
            StepOutcome::Done(())
        );
        output.handle
    };
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    CoreInit::<TestPlatform>::submit_net_delegate_task_for_test(NetDelegateTaskConfig::run_steps(
        1,
    ))
    .expect("delegate task submit");
    let parked = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("delegate should park before socket tx");
    assert_eq!(parked.stats.completed, 0);

    {
        let guard = tx_substrate::epoch::guard();
        assert_eq!(
            step_send_to_kernel_bytes(
                &client.identity,
                Some(IpEndpoint::new(Ipv4Address::BROADCAST, REMOTE_PORT)),
                b"hello",
                SendRecvFlags::empty(),
                &guard,
            ),
            StepOutcome::Done(5)
        );
    }

    let driven = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("delegate should consume socket tx poll");
    assert!(
        driven.stats.completed >= 1,
        "bounded delegate should finish after socket TX POLL"
    );
    assert_eq!(VIRTIO_NET0_DEVICE.tx_inflight_len(), 1);
    assert_eq!(VIRTIO_NET0_DEVICE.stats.snapshot().tx_submitted, 1);
}

fn ethernet_ipv4_frame(dst: EthernetAddress, src: EthernetAddress, ipv4_packet: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + ipv4_packet.len());
    frame.extend_from_slice(&dst.octets());
    frame.extend_from_slice(&src.octets());
    frame.extend_from_slice(&[0x08, 0x00]);
    frame.extend_from_slice(ipv4_packet);
    frame
}
