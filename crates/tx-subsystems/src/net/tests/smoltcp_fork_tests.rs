use alloc::vec;

use smoltcp::iface::{Config, Interface};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpEndpoint, IpProtocol, IpRepr, Ipv4Address, Ipv4Repr, TcpControl,
    TcpRepr, TcpSeqNumber,
};

#[test]
fn asterinas_smoltcp_fork_exposes_tcp_process_and_dispatch() {
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device, Instant::ZERO);

    let rx = tcp::SocketBuffer::new(vec![0; 1024]);
    let tx = tcp::SocketBuffer::new(vec![0; 1024]);
    let mut socket = tcp::Socket::new(rx, tx);

    let local = Ipv4Address::new(127, 0, 0, 1);
    let remote = Ipv4Address::new(127, 0, 0, 2);
    socket
        .listen(IpEndpoint::new(IpAddress::Ipv4(local), 40_180))
        .expect("listen should accept a concrete endpoint");

    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: remote,
        dst_addr: local,
        next_header: IpProtocol::Tcp,
        payload_len: 0,
        hop_limit: 64,
    });
    let syn = TcpRepr {
        src_port: 50_180,
        dst_port: 40_180,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(1),
        ack_number: None,
        window_len: 1024,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload: &[],
    };

    // This is the API surface needed by the Asterinas/bigtcp-style path:
    // txKernel owns the socket table and poll loop, while smoltcp owns the
    // TCP state machine step.
    let _ = socket.accepts(iface.context(), &ip_repr, &syn);
    let _ = socket.process(iface.context(), &ip_repr, &syn);
    socket
        .dispatch(iface.context(), |_cx, _packet| {
            Ok::<(), core::convert::Infallible>(())
        })
        .expect("dispatch closure is infallible");
}
