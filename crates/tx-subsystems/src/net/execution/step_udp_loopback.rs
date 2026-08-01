use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_write_target;
use crate::net::namespace::initial_loopback_iface;
use crate::net::protocol::{LoopbackIface, PollContext, UDP_IPV4_MAX_PAYLOAD_BYTES};
use crate::net::structure::{IpEndpoint, SendRecvFlags, SocketIdentity, SocketProtocol, UdpInner};

use super::step_send::send_flags_error;
use super::{
    socket_send_wait_token, step_send::clear_send_space_if_full, yield_bytes_on_token,
    ByteStepOutcome,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackUdpTransferOutcome {
    pub tx_packets: usize,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub bytes_moved: usize,
    pub source_wake_fired: bool,
    pub peer_wake_fired: bool,
}

pub fn step_process_loopback_udp(
    source: &Cap<SocketIdentity>,
    budget: usize,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackUdpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_udp_with_post(source, budget, guard, |mailbox, event| mailbox.post(event))
}

pub fn step_process_loopback_udp_with_post<F>(
    source: &Cap<SocketIdentity>,
    budget: usize,
    guard: &Guard<'_>,
    post: F,
) -> StepOutcome<LoopbackUdpTransferOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    step_process_loopback_udp_on_iface_with_post(
        source,
        budget,
        initial_loopback_iface(),
        guard,
        post,
    )
}

pub fn step_process_loopback_udp_on_iface(
    source: &Cap<SocketIdentity>,
    budget: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackUdpTransferOutcome> {
    step_process_loopback_udp_on_iface_with_post(source, budget, iface, guard, |mailbox, event| {
        mailbox.post(event)
    })
}

pub fn step_process_loopback_udp_on_iface_with_post<F>(
    source: &Cap<SocketIdentity>,
    budget: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
    mut post: F,
) -> StepOutcome<LoopbackUdpTransferOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(source_payload) = source.acquire_operational() else {
        return StepOutcome::Done(LoopbackUdpTransferOutcome::default());
    };

    // P1-S4: 单通路——loopback UDP 一律经 lo 队列真包转运（egress 编包
    // → 队列 → ingress 解析投递），直拷捷径已删除。
    let mut ctx = PollContext::new_with_table(
        crate::net::clock::net_now_instant(),
        source_payload.socket_table(),
    );
    let mut source_wake_fired = false;

    if let Some(publish) = ctx.poll_udp_egress_one(source, iface, guard) {
        source_wake_fired = publish.publish.send_has_space;
        publish.publish_with_post(&mut post);
    }

    let ingress = ctx.poll_udp_ingress(iface, guard, budget);
    let mut peer_wake_fired = false;
    for publish in ingress.publishes {
        peer_wake_fired |= publish.publish.recv_has_data;
        publish.publish_with_post(&mut post);
    }

    StepOutcome::Done(LoopbackUdpTransferOutcome {
        tx_packets: ingress.tx_packets,
        packets_seen: ingress.packets_seen,
        sockets_touched: ingress.sockets_touched,
        bytes_moved: ingress.bytes_moved,
        source_wake_fired,
        peer_wake_fired,
    })
}

pub fn step_send_udp_loopback_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_send_udp_loopback_kernel_bytes_with_post(
        socket,
        dst,
        bytes,
        flags,
        guard,
        |mailbox, event| mailbox.post(event),
    )
}

pub fn step_send_udp_loopback_kernel_bytes_with_post<F>(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
    post: F,
) -> ByteStepOutcome<usize>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    step_send_udp_loopback_kernel_bytes_on_iface_with_post(
        socket,
        dst,
        bytes,
        flags,
        initial_loopback_iface(),
        guard,
        post,
    )
}

pub fn step_send_udp_loopback_kernel_bytes_on_iface(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    step_send_udp_loopback_kernel_bytes_on_iface_with_post(
        socket,
        dst,
        bytes,
        flags,
        iface,
        guard,
        |mailbox, event| mailbox.post(event),
    )
}

pub fn step_send_udp_loopback_kernel_bytes_on_iface_with_post<F>(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
    mut post: F,
) -> ByteStepOutcome<usize>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return tx_substrate::step::StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(source_payload) = socket.acquire_operational() else {
        return tx_substrate::step::StepOutcome::Err(Errno::ENOTCONN);
    };
    if let Some(errno) = send_flags_error(witness.flags) {
        return tx_substrate::step::StepOutcome::Err(errno);
    }
    if source_payload.shutdown_wr() {
        return tx_substrate::step::StepOutcome::Err(Errno::EPIPE);
    }
    let total_payload_len = source_payload.udp_corked_send_len() + bytes.len();
    if total_payload_len > UDP_IPV4_MAX_PAYLOAD_BYTES {
        return tx_substrate::step::StepOutcome::Err(Errno::EMSGSIZE);
    }
    if bytes.is_empty() {
        return tx_substrate::step::StepOutcome::Done(0);
    }

    let (local, destination) =
        match udp_loopback_endpoints(&source_payload.protocol_snapshot(), dst) {
            Some(endpoints) => endpoints,
            None => return tx_substrate::step::StepOutcome::Err(Errno::EDESTADDRREQ),
        };
    if !is_loopback_destination(destination)
        || !(local.is_unspecified() || local.same_family(destination) && local.is_loopback())
    {
        return tx_substrate::step::StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if total_payload_len + udp_packet_overhead(destination) > usize::from(iface.mtu()) {
        return tx_substrate::step::StepOutcome::Err(Errno::EMSGSIZE);
    }
    let reserve =
        match source_payload.reserve_send_bytes_to_with_flags(Some(destination), bytes, flags) {
            Ok(Some(reserve)) => reserve,
            Ok(None) => {
                clear_send_space_if_full(socket, &source_payload);
                return yield_bytes_on_token(
                    tx_substrate::step::ByteProgress::EMPTY,
                    socket_send_wait_token(socket),
                );
            }
            Err(errno) => return tx_substrate::step::StepOutcome::Err(errno),
        };
    if reserve.became_full {
        clear_send_space_if_full(socket, &source_payload);
    }
    if flags.contains(SendRecvFlags::MSG_MORE) {
        return tx_substrate::step::StepOutcome::Done(reserve.bytes);
    }

    let source = loopback_udp_source(local, destination, iface);
    if source.port == 0 || destination.port == 0 {
        return tx_substrate::step::StepOutcome::Err(Errno::EINVAL);
    }

    // P1-S4: 数据报经 lo 队列真包转运（不再查表直塞对端）。同步驱动
    // 一轮 egress+ingress，保持发送路径的内联时延特性。
    let mut ctx = PollContext::new_with_table(
        crate::net::clock::net_now_instant(),
        source_payload.socket_table(),
    );
    if let Some(publish) = ctx.poll_udp_egress_one(socket, iface, guard) {
        publish.publish_with_post(&mut post);
    }
    let ingress = ctx.poll_udp_ingress(iface, guard, 1);
    for publish in ingress.publishes {
        publish.publish_with_post(&mut post);
    }
    tx_substrate::step::StepOutcome::Done(reserve.bytes)
}

fn udp_loopback_endpoints(
    protocol: &SocketProtocol,
    dst: Option<IpEndpoint>,
) -> Option<(IpEndpoint, IpEndpoint)> {
    match protocol {
        SocketProtocol::Udp(UdpInner::Bound { local }) => dst.map(|dst| (*local, dst)),
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            Some((*local, dst.unwrap_or(*remote)))
        }
        _ => None,
    }
}

fn loopback_udp_source(
    local: IpEndpoint,
    destination: IpEndpoint,
    iface: &LoopbackIface,
) -> IpEndpoint {
    let addr = if local.is_unspecified() && is_loopback_destination(destination) {
        IpEndpoint::loopback_for_family(destination.family, local.port).ip_addr()
    } else {
        local.ip_addr()
    };
    let _ = iface;
    IpEndpoint::from_ip(addr, local.port)
}

fn is_loopback_destination(endpoint: IpEndpoint) -> bool {
    endpoint.is_loopback()
}

fn udp_packet_overhead(endpoint: IpEndpoint) -> usize {
    const IPV4_UDP_OVERHEAD: usize = 20 + 8;
    const IPV6_UDP_OVERHEAD: usize = 40 + 8;
    if endpoint.family == crate::net::structure::AddressFamily::Inet6 {
        IPV6_UDP_OVERHEAD
    } else {
        IPV4_UDP_OVERHEAD
    }
}
