use tx_substrate::zone::{self, Cap, PayloadCap};
use tx_substrate::zone::{register_zone_for, PayloadPolicy, Zone, ZoneAllocated, ZoneError};

#[cfg(test)]
use crate::net::namespace::initial_net_namespace_payload;
use crate::net::namespace::NetNamespacePayload;

use super::identity::SocketIdentity;
use super::payload::{SocketPayload, SocketProtocol};
use super::types::{AddressFamily, IpEndpoint, SocketKind, SocketOptionSet, TcpState};

static SOCKET_IDENTITY_ZONE: Zone<SocketIdentity> = Zone::const_new();
static SOCKET_PAYLOAD_ZONE: Zone<SocketPayload> = Zone::const_new();

// SAFETY: `SOCKET_IDENTITY_ZONE` is the single process-wide zone for
// `SocketIdentity`; all retained identity handles must resolve through it.
unsafe impl ZoneAllocated for SocketIdentity {
    fn zone() -> &'static Zone<Self> {
        &SOCKET_IDENTITY_ZONE
    }
}

// SAFETY: `SOCKET_PAYLOAD_ZONE` is the single process-wide zone for
// `SocketPayload`; operational evidence is retained through this zone.
unsafe impl ZoneAllocated for SocketPayload {
    type Policy = PayloadPolicy<Self>;

    fn zone() -> &'static Zone<Self> {
        &SOCKET_PAYLOAD_ZONE
    }
}

pub(crate) fn register_zones() -> Result<(), ZoneError> {
    register_zone_for::<SocketIdentity>()?;
    register_zone_for::<SocketPayload>()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn create_socket_for_test_or_bootstrap(
    kind: SocketKind,
    options: SocketOptionSet,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    create_socket_in_namespace(kind, options, initial_net_namespace_payload())
}

pub(crate) fn create_socket_in_namespace(
    kind: SocketKind,
    options: SocketOptionSet,
    net_namespace: PayloadCap<NetNamespacePayload>,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    create_socket_in_namespace_with_family(
        kind,
        default_family_for_kind(kind),
        options,
        net_namespace,
    )
}

pub(crate) fn create_socket_in_namespace_with_family(
    kind: SocketKind,
    family: AddressFamily,
    options: SocketOptionSet,
    net_namespace: PayloadCap<NetNamespacePayload>,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    let identity_res = zone::reserve_for::<SocketIdentity>()?;
    let payload_res = zone::reserve_for::<SocketPayload>()?;
    let identity = zone::sign_for(identity_res, SocketIdentity::new_with_family(kind, family));
    let payload = PayloadCap::from_cap(zone::sign_for(
        payload_res,
        SocketPayload::new_in_namespace_with_family(kind, family, options, net_namespace),
    ));
    identity.install_payload(payload);
    Ok(identity)
}

pub(crate) fn create_connected_stream_for_accept_in_namespace_with_family(
    local: IpEndpoint,
    peer: IpEndpoint,
    family: AddressFamily,
    options: SocketOptionSet,
    net_namespace: PayloadCap<NetNamespacePayload>,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    let child =
        create_socket_in_namespace_with_family(SocketKind::Tcp, family, options, net_namespace)?;
    set_connected_stream_state(&child, local, peer);
    Ok(child)
}

pub(crate) fn create_connected_sctp_for_accept_in_namespace(
    local: IpEndpoint,
    peer: IpEndpoint,
    options: SocketOptionSet,
    net_namespace: PayloadCap<NetNamespacePayload>,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    let child = create_socket_in_namespace_with_family(
        SocketKind::Sctp,
        local.family,
        options,
        net_namespace,
    )?;
    set_connected_sctp_state(&child, local, peer);
    Ok(child)
}

fn set_connected_stream_state(child: &Cap<SocketIdentity>, local: IpEndpoint, peer: IpEndpoint) {
    let payload = child.live_payload().expect("new socket payload");
    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Connected {
            local,
            remote: peer,
        });
    });
}

fn set_connected_sctp_state(child: &Cap<SocketIdentity>, local: IpEndpoint, peer: IpEndpoint) {
    let payload = child.live_payload().expect("new socket payload");
    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Sctp(TcpState::Connected {
            local,
            remote: peer,
        });
    });
}

const fn default_family_for_kind(kind: SocketKind) -> AddressFamily {
    match kind {
        SocketKind::UnixDatagram | SocketKind::UnixStream => AddressFamily::Unix,
        SocketKind::Tcp | SocketKind::Udp | SocketKind::Sctp | SocketKind::RawIcmp => {
            AddressFamily::Inet
        }
        SocketKind::NetlinkRoute | SocketKind::NetlinkXfrm | SocketKind::NetlinkNetfilter => {
            AddressFamily::Netlink
        }
        SocketKind::Packet => AddressFamily::Packet,
        SocketKind::RdsSeqPacket => AddressFamily::Rds,
    }
}
