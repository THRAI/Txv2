use alloc::vec::Vec;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_write_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::device::{EthernetAddress, NetDeviceRegistration};
use crate::net::namespace::NetNamespacePayload;
use crate::net::packet::{PacketTxReadiness, RxFrame};
use crate::net::structure::{
    SendRecvFlags, SockAddrLl, SocketIdentity, SocketProtocol, SocketType,
};

const ETHERNET_HEADER_LEN: usize = 14;
const ETH_P_ALL: u16 = 0x0003;
const ARPHRD_ETHER: u16 = 1;
const PACKET_HOST: u8 = 0;
const PACKET_BROADCAST: u8 = 1;
const PACKET_MULTICAST: u8 = 2;
const PACKET_OTHERHOST: u8 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketIngressOutcome {
    pub sockets_matched: usize,
    pub frames_queued: usize,
    pub frames_dropped: usize,
    pub wakes_fired: usize,
}

/// Fan one device-owned Ethernet frame out to matching packet sockets.
///
/// The caller remains the single owner of `NetDeviceOps::receive()` and passes
/// the same frame onward to the normal bridge/L3 path after this function
/// returns. Each packet socket gets an independent queue copy.
pub fn step_packet_ingress_fanout(
    net_namespace: &NetNamespacePayload,
    registration: &'static NetDeviceRegistration,
    frame: &RxFrame,
    guard: &Guard<'_>,
) -> PacketIngressOutcome {
    let bytes = frame.as_bytes();
    if bytes.len() < ETHERNET_HEADER_LEN {
        return PacketIngressOutcome::default();
    }

    let Some(link) = net_namespace
        .link_snapshot()
        .into_iter()
        .find(|link| {
            link.ifindex != 0
                && link.is_up
                && net_namespace
                    .find_device_by_ifindex(link.ifindex)
                    .is_some_and(|candidate| candidate.devt == registration.devt)
        })
    else {
        return PacketIngressOutcome::default();
    };

    let protocol = u16::from_be_bytes([bytes[12], bytes[13]]);
    let source_mac =
        EthernetAddress::new([bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11]]);
    let destination_mac =
        EthernetAddress::new([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]]);
    let packet_type = incoming_packet_type(destination_mac, registration.ops.mac_addr());
    let mut source_addr = [0u8; 8];
    source_addr[..6].copy_from_slice(&source_mac.octets());
    let source = SockAddrLl::with_link_layer_addr(
        protocol,
        link.ifindex as i32,
        ARPHRD_ETHER,
        packet_type,
        source_addr,
        6,
    );

    let mut outcome = PacketIngressOutcome::default();
    for socket in net_namespace.socket_table().snapshot_packet_sockets(guard) {
        let Some(identity) = socket.downgrade().observe(guard) else {
            continue;
        };
        let Some(payload) = identity.acquire_operational() else {
            continue;
        };
        let Some(bound) = payload.packet_sockaddr() else {
            continue;
        };
        if bound.protocol == 0
            || (bound.protocol != ETH_P_ALL && bound.protocol != protocol)
            || (bound.ifindex != 0 && bound.ifindex != link.ifindex as i32)
        {
            continue;
        }
        outcome.sockets_matched += 1;

        let socket_type = payload.with_options(|options| options.socket.sock_type);
        let delivered = match socket_type {
            SocketType::Raw => bytes.to_vec(),
            SocketType::Dgram => bytes[ETHERNET_HEADER_LEN..].to_vec(),
            SocketType::Stream | SocketType::SeqPacket => {
                outcome.frames_dropped += 1;
                continue;
            }
        };
        match payload.record_packet_frame(source, delivered) {
            Some(became_readable) => {
                outcome.frames_queued += 1;
                if became_readable {
                    outcome.wakes_fired += socket
                        .readiness
                        .fire_recv(crate::net::structure::RecvWireSet::HAS_DATA);
                }
            }
            None => outcome.frames_dropped += 1,
        }
    }
    outcome
}

/// Send one packet-socket record through the namespace-selected real device.
///
/// `SOCK_RAW` supplies a complete Ethernet frame. `SOCK_DGRAM` supplies only
/// the network-layer payload; this step constructs the Ethernet header from
/// the selected namespace link and `sockaddr_ll`.
pub fn step_packet_send(
    socket: &Cap<SocketIdentity>,
    destination: SockAddrLl,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> StepOutcome<usize> {
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = witness.identity.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if payload.shutdown_wr() {
        return StepOutcome::Err(Errno::EPIPE);
    }
    if !matches!(payload.protocol_snapshot(), SocketProtocol::Packet(_)) {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if destination.ifindex <= 0 {
        return StepOutcome::Err(Errno::ENODEV);
    }

    let net_namespace = payload.net_namespace();
    let Some(link) = net_namespace
        .link_snapshot()
        .into_iter()
        .find(|link| link.ifindex == destination.ifindex as u32)
    else {
        return StepOutcome::Err(Errno::ENODEV);
    };
    if !link.is_up || link.is_loopback {
        return StepOutcome::Err(Errno::ENODEV);
    }
    let Some(registration) = net_namespace.find_device_by_ifindex(link.ifindex) else {
        return StepOutcome::Err(Errno::ENODEV);
    };

    let socket_type = payload.with_options(|options| options.socket.sock_type);
    let frame = match socket_type {
        SocketType::Raw => {
            if bytes.len() < ETHERNET_HEADER_LEN {
                return StepOutcome::Err(Errno::EINVAL);
            }
            if bytes.len() > usize::from(link.mtu) + ETHERNET_HEADER_LEN {
                return StepOutcome::Err(Errno::EMSGSIZE);
            }
            bytes.to_vec()
        }
        SocketType::Dgram => {
            if destination.halen < 6 {
                return StepOutcome::Err(Errno::EINVAL);
            }
            if bytes.len() > usize::from(link.mtu) {
                return StepOutcome::Err(Errno::EMSGSIZE);
            }
            let protocol = if destination.protocol != 0 {
                destination.protocol
            } else {
                payload.packet_sockaddr().map_or(0, |addr| addr.protocol)
            };
            if protocol == 0 {
                return StepOutcome::Err(Errno::EINVAL);
            }
            build_cooked_ethernet_frame(
                &destination.addr[..6],
                registration.ops.mac_addr(),
                protocol,
                bytes,
            )
        }
        SocketType::Stream | SocketType::SeqPacket => {
            return StepOutcome::Err(Errno::EOPNOTSUPP);
        }
    };

    if registration.ops.tx_readiness(guard) == PacketTxReadiness::Busy {
        return StepOutcome::Err(Errno::EAGAIN);
    }
    match registration.ops.transmit(&frame, guard) {
        StepOutcome::Done(()) => {
            net_delegate_kick_poll();
            StepOutcome::Done(bytes.len())
        }
        StepOutcome::Continue { .. } => {
            net_delegate_kick_poll();
            StepOutcome::Done(bytes.len())
        }
        StepOutcome::Yield { .. } => StepOutcome::Err(Errno::EAGAIN),
        StepOutcome::Err(errno) => StepOutcome::Err(errno),
    }
}

fn build_cooked_ethernet_frame(
    destination: &[u8],
    source: EthernetAddress,
    protocol: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + payload.len());
    frame.extend_from_slice(destination);
    frame.extend_from_slice(&source.octets());
    frame.extend_from_slice(&protocol.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn incoming_packet_type(destination: EthernetAddress, local: EthernetAddress) -> u8 {
    if destination == local {
        PACKET_HOST
    } else if destination.is_broadcast() {
        PACKET_BROADCAST
    } else if destination.is_multicast() {
        PACKET_MULTICAST
    } else {
        PACKET_OTHERHOST
    }
}
