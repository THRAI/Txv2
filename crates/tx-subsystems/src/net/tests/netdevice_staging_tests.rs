use super::*;
use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

struct MockNetDevice {
    rx: Mutex<VecDeque<RxFrame>>,
    tx: Mutex<std::vec::Vec<std::vec::Vec<u8>>>,
    tx_ready: AtomicBool,
    fail_tx: AtomicBool,
    mac: EthernetAddress,
    mtu: u16,
}

struct DeviceDelegateDriver<'a> {
    source: SmoltcpPacketSource<'a>,
    tx_sink: Option<&'a dyn PacketTxSink>,
}

struct MockPacketTxSink<'a> {
    inner: SmoltcpPacketTxSink<'a>,
    device: &'static MockNetDevice,
}

impl NetDelegateDriver for DeviceDelegateDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::ZERO
    }

    fn packet_source(&self) -> &dyn PacketSource {
        &self.source
    }

    fn packet_tx_sink(&self) -> Option<&dyn PacketTxSink> {
        self.tx_sink
    }

    fn device_tx_budget(&self) -> DeviceTxBudget {
        DeviceTxBudget {
            tcp_connecting: 0,
            tcp_connected: 0,
            udp_bound: 256,
        }
    }
}

impl MockNetDevice {
    fn new() -> Self {
        Self {
            rx: Mutex::new(VecDeque::new()),
            tx: Mutex::new(std::vec::Vec::new()),
            tx_ready: AtomicBool::new(true),
            fail_tx: AtomicBool::new(false),
            mac: EthernetAddress::new([0x02, 0, 0, 0, 0, 1]),
            mtu: 1500,
        }
    }

    fn push_rx(&self, frame: RxFrame) {
        self.rx.lock().expect("mock rx").push_back(frame);
    }

    fn rx_len(&self) -> usize {
        self.rx.lock().expect("mock rx").len()
    }

    fn tx_frames(&self) -> std::vec::Vec<std::vec::Vec<u8>> {
        self.tx.lock().expect("mock tx").clone()
    }

    fn set_tx_ready(&self, ready: bool) {
        self.tx_ready.store(ready, Ordering::Release);
    }

    fn set_fail_tx(&self, fail: bool) {
        self.fail_tx.store(fail, Ordering::Release);
    }
}

impl NetDeviceOps for MockNetDevice {
    fn receive(&self) -> Option<RxFrame> {
        self.rx.lock().expect("mock rx").pop_front()
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> crate::execution::StepOutcome<()> {
        if !self.tx_ready.load(Ordering::Acquire) {
            return StepOutcome::yield_on_wait_source(NoProgress, 0, 0);
        }
        if self.fail_tx.load(Ordering::Acquire) {
            return StepOutcome::Err(Errno::EIO);
        }
        self.tx.lock().expect("mock tx").push(frame.to_vec());
        StepOutcome::Done(())
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }
}

impl PacketTxSink for MockPacketTxSink<'_> {
    fn readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        let _guard = guard;
        if self.device.tx_ready.load(Ordering::Acquire) {
            PacketTxReadiness::Ready
        } else {
            PacketTxReadiness::Busy
        }
    }

    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> crate::net::packet::PacketTxResult {
        self.inner.transmit(frame, guard)
    }
}

#[test]
fn net_delegate_poll_drains_mock_device_rx_to_socket() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    clear_delegate_queue();

    let device = leak_mock_device();
    let registration = leak_registration(device, 30);
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: device.mac_addr(),
        local_ipv4: Ipv4Address::LOOPBACK,
        mtu: device.mtu(),
    });
    let server = {
        let guard = tx_substrate::epoch::guard();
        let server = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Udp,
            SocketOptionSet::default_udp(),
        )
        .expect("server");
        assert_eq!(
            step_bind(&server, inet(40_230), &guard),
            StepOutcome::Done(())
        );
        server
    };
    let packet = UdpTxDatagram {
        dst: endpoint(40_230),
        payload: b"hello".to_vec(),
    }
    .emit_ipv4_packet(endpoint(50_230))
    .expect("udp packet");
    device.push_rx(RxFrame::new(adapter.emit_tx_frame(packet.as_bytes())));

    let driver = DeviceDelegateDriver {
        source: SmoltcpPacketSource {
            adapter: &adapter,
            device: registration,
        },
        tx_sink: None,
    };
    let guard = tx_substrate::epoch::guard();
    crate::net::delegate::net_delegate_kick_poll();
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.packets_seen, 1);
    assert_eq!(device.rx_len(), 0);
    assert_eq!(
        server
            .acquire_operational()
            .expect("server payload")
            .io_snapshot()
            .recv_len,
        5
    );
    assert!(server.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0);
    assert_eq!(
        step_recv(&server, 5, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(5)
    );
}

#[test]
fn net_delegate_poll_transmits_socket_udp_to_mock_device() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    clear_delegate_queue();

    let device = leak_mock_device();
    let registration = leak_registration(device, 31);
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: device.mac_addr(),
        local_ipv4: Ipv4Address::LOOPBACK,
        mtu: device.mtu(),
    });
    let client = {
        let guard = tx_substrate::epoch::guard();
        let client = registry::create_socket_for_test_or_bootstrap(
            SocketKind::Udp,
            SocketOptionSet::default_udp(),
        )
        .expect("client");
        assert_eq!(
            step_bind(&client, inet(50_231), &guard),
            StepOutcome::Done(())
        );
        assert_eq!(
            step_connect(&client, inet(40_231), &guard),
            StepOutcome::Done(())
        );
        clear_delegate_queue();
        assert_eq!(
            step_send_kernel_bytes(&client, b"hello", SendRecvFlags::empty(), &guard),
            StepOutcome::Done(5)
        );
        client
    };

    let tx_sink = SmoltcpPacketTxSink {
        adapter: &adapter,
        device: registration,
    };
    let driver = DeviceDelegateDriver {
        source: SmoltcpPacketSource {
            adapter: &adapter,
            device: registration,
        },
        tx_sink: Some(&tx_sink),
    };
    let guard = tx_substrate::epoch::guard();
    let outcome = net_delegate_step_once(&driver, &guard);

    assert!(outcome.poll_seen);
    assert_eq!(outcome.device_tx.udp_attempted, 1);
    assert_eq!(outcome.device_tx.udp_packets, 1);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        SocketOptionSet::default_udp().socket.send_buf_size
    );

    let tx = device.tx_frames();
    assert_eq!(tx.len(), 1);
    assert!(matches!(
        adapter.demux_rx_frame(&RxFrame::new(tx[0].clone())),
        PacketDispatch::Udp(event)
            if event.src == endpoint(50_231)
                && event.dst == endpoint(40_231)
                && event.payload == b"hello"
    ));
}

#[test]
fn udp_device_tx_busy_keeps_datagram_for_retry() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    clear_delegate_queue();

    let device = leak_mock_device();
    let registration = leak_registration(device, 32);
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: device.mac_addr(),
        local_ipv4: Ipv4Address::LOOPBACK,
        mtu: device.mtu(),
    });
    let client = connected_udp_client(50_232, 40_232, b"hello");
    let send_space_after_send = client
        .acquire_operational()
        .expect("client payload")
        .io_snapshot()
        .send_space;
    assert_eq!(
        send_space_after_send,
        SocketOptionSet::default_udp().socket.send_buf_size - 5
    );

    let tx_sink = MockPacketTxSink {
        inner: SmoltcpPacketTxSink {
            adapter: &adapter,
            device: registration,
        },
        device,
    };
    let driver = DeviceDelegateDriver {
        source: SmoltcpPacketSource {
            adapter: &adapter,
            device: registration,
        },
        tx_sink: Some(&tx_sink),
    };
    let guard = tx_substrate::epoch::guard();

    device.set_tx_ready(false);
    crate::net::delegate::net_delegate_kick_poll();
    let busy = net_delegate_step_once(&driver, &guard);
    assert!(busy.poll_seen);
    assert!(busy.device_tx.udp_busy >= 1);
    assert_eq!(busy.device_tx.udp_packets, 0);
    assert_eq!(device.tx_frames().len(), 0);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        send_space_after_send
    );

    device.set_tx_ready(true);
    crate::net::delegate::net_delegate_kick_poll();
    let retry = net_delegate_step_once(&driver, &guard);
    assert!(retry.poll_seen);
    assert!(retry.device_tx.udp_attempted >= 1);
    assert!(retry.device_tx.udp_packets >= 1);
    assert_eq!(retry.device_tx.udp_busy, 0);
    assert!(tx_frames_contain_udp(
        &adapter,
        device.tx_frames(),
        endpoint(50_232),
        endpoint(40_232),
        b"hello"
    ));
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        SocketOptionSet::default_udp().socket.send_buf_size
    );
}

#[test]
fn udp_device_tx_failed_keeps_datagram_and_counts_error() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    clear_delegate_queue();

    let device = leak_mock_device();
    let registration = leak_registration(device, 33);
    let adapter = SmoltcpAdapter::new(SmoltcpAdapterConfig {
        local_mac: device.mac_addr(),
        local_ipv4: Ipv4Address::LOOPBACK,
        mtu: device.mtu(),
    });
    let client = connected_udp_client(50_233, 40_233, b"hello");
    let send_space_after_send = client
        .acquire_operational()
        .expect("client payload")
        .io_snapshot()
        .send_space;

    device.set_fail_tx(true);
    let tx_sink = MockPacketTxSink {
        inner: SmoltcpPacketTxSink {
            adapter: &adapter,
            device: registration,
        },
        device,
    };
    let driver = DeviceDelegateDriver {
        source: SmoltcpPacketSource {
            adapter: &adapter,
            device: registration,
        },
        tx_sink: Some(&tx_sink),
    };
    let guard = tx_substrate::epoch::guard();
    let failed = net_delegate_step_once(&driver, &guard);

    assert!(failed.poll_seen);
    assert!(failed.device_tx.udp_attempted >= 1);
    assert!(failed.device_tx.udp_failed >= 1);
    assert_eq!(failed.device_tx.udp_packets, 0);
    assert_eq!(device.tx_frames().len(), 0);
    assert_eq!(
        client
            .acquire_operational()
            .expect("client payload")
            .io_snapshot()
            .send_space,
        send_space_after_send
    );

    device.set_fail_tx(false);
    crate::net::delegate::net_delegate_kick_poll();
    let retry = net_delegate_step_once(&driver, &guard);
    assert!(retry.device_tx.udp_packets >= 1);
    assert!(tx_frames_contain_udp(
        &adapter,
        device.tx_frames(),
        endpoint(50_233),
        endpoint(40_233),
        b"hello"
    ));
}

fn leak_mock_device() -> &'static MockNetDevice {
    Box::leak(Box::new(MockNetDevice::new()))
}

fn leak_registration(device: &'static MockNetDevice, minor: u32) -> &'static NetDeviceRegistration {
    Box::leak(Box::new(NetDeviceRegistration {
        devt: DevT::new(90, minor),
        name: "mock-net",
        ops: device,
    }))
}

fn connected_udp_client(
    local_port: u16,
    remote_port: u16,
    bytes: &[u8],
) -> tx_substrate::zone::Cap<SocketIdentity> {
    let guard = tx_substrate::epoch::guard();
    let client = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Udp,
        SocketOptionSet::default_udp(),
    )
    .expect("client");
    assert_eq!(
        step_bind(&client, inet(local_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(remote_port), &guard),
        StepOutcome::Done(())
    );
    clear_delegate_queue();
    assert_eq!(
        step_send_kernel_bytes(&client, bytes, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(bytes.len())
    );
    client
}

fn tx_frames_contain_udp(
    adapter: &SmoltcpAdapter,
    frames: std::vec::Vec<std::vec::Vec<u8>>,
    src: IpEndpoint,
    dst: IpEndpoint,
    payload: &[u8],
) -> bool {
    frames.into_iter().any(|frame| {
        matches!(
            adapter.demux_rx_frame(&RxFrame::new(frame)),
            PacketDispatch::Udp(event)
                if event.src == src && event.dst == dst && event.payload == payload
        )
    })
}

fn clear_delegate_queue() {
    crate::net::delegate::net_delegate_clear(
        crate::net::delegate::DelegateWireSet::POLL | crate::net::delegate::DelegateWireSet::TICK,
    );
}
