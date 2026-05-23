use super::*;
use crate::net::step_socket_close;
use crate::net::structure::SocketIdentity;
use crate::net::SocketRecvBytesOutcome;
use tx_substrate::zone::Cap;

struct LoopbackDelegateDriver<'a> {
    now: smoltcp::time::Instant,
    source: &'a ScriptedPacketSource,
    iface: Option<&'a LoopbackIface>,
}

impl NetDelegateDriver for LoopbackDelegateDriver<'_> {
    fn now(&self) -> smoltcp::time::Instant {
        self.now
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.source
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        self.iface
    }
}

fn assert_tcp_payload_round_trip(
    label: &str,
    source: &Cap<SocketIdentity>,
    destination: &Cap<SocketIdentity>,
    bytes: &[u8],
    guard: &tx_subsystems::execution::Guard<'_>,
) {
    assert_eq!(
        step_send_kernel_bytes(source, bytes, SendRecvFlags::empty(), guard),
        StepOutcome::Done(bytes.len())
    );
    let transfer = match step_tcp_loopback_transfer(source, bytes.len(), guard) {
        StepOutcome::Done(transfer) => transfer,
        _ => panic!("unexpected tcp loopback transfer outcome"),
    };
    assert_eq!(transfer.bytes_moved, bytes.len(), "{label}");

    let mut out = alloc::vec![0u8; bytes.len()];
    assert_eq!(
        step_recv_kernel_bytes(destination, &mut out, SendRecvFlags::empty(), guard),
        StepOutcome::Done(crate::net::structure::SocketRecvBytesOutcome {
            bytes: bytes.len(),
            source: None,
            destination: None,
            unix_source: None,
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(out, bytes);
}

fn prepare_loopback_connect(
    server_port: u16,
    client_port: u16,
) -> (
    Cap<SocketIdentity>,
    Cap<SocketIdentity>,
    IpEndpoint,
    IpEndpoint,
) {
    prepare_loopback_connect_with_client_send_buf(server_port, client_port, 16_384)
}

fn prepare_loopback_connect_with_client_send_buf(
    server_port: u16,
    client_port: u16,
    client_send_buf: usize,
) -> (
    Cap<SocketIdentity>,
    Cap<SocketIdentity>,
    IpEndpoint,
    IpEndpoint,
) {
    let guard = tx_substrate::epoch::guard();

    let listener = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::default_tcp(),
    )
    .expect("listener");
    assert_eq!(
        step_bind(&listener, inet(server_port), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 8, &guard), StepOutcome::Done(()));

    let mut options = SocketOptionSet::default_tcp();
    options.socket.send_buf_size = client_send_buf;
    let client =
        registry::create_socket_for_test_or_bootstrap(SocketKind::Tcp, options).expect("client");
    assert_eq!(
        step_bind(&client, inet(client_port), &guard),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_connect(&client, inet(server_port), &guard),
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource { .. },
            ..
        }
    ));

    (
        client,
        listener,
        endpoint(client_port),
        endpoint(server_port),
    )
}

mod delegate_tick;
mod tcp_lifecycle;
mod tcp_loopback;
mod udp;
