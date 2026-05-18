use tx_substrate::zone::{self, Cap, PayloadCap};
use tx_substrate::zone::{register_zone_for, PayloadPolicy, Zone, ZoneAllocated, ZoneError};

use super::identity::SocketIdentity;
use super::payload::{SocketPayload, SocketProtocol};
use super::types::{IpEndpoint, SocketKind, SocketOptionSet, TcpState};

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

pub(crate) fn create_socket_for_test_or_bootstrap(
    kind: SocketKind,
    options: SocketOptionSet,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    let identity_res = zone::reserve_for::<SocketIdentity>()?;
    let payload_res = zone::reserve_for::<SocketPayload>()?;
    let identity = zone::sign_for(identity_res, SocketIdentity::new(kind));
    let payload = PayloadCap::from_cap(zone::sign_for(
        payload_res,
        SocketPayload::new(kind, options),
    ));
    identity.install_payload(payload);
    Ok(identity)
}

pub(crate) fn create_connected_stream_for_accept(
    local: IpEndpoint,
    peer: IpEndpoint,
    options: SocketOptionSet,
) -> Result<Cap<SocketIdentity>, ZoneError> {
    let child = create_socket_for_test_or_bootstrap(SocketKind::Tcp, options)?;
    let payload = child.live_payload().expect("new socket payload");
    payload.with_protocol_mut(|protocol| {
        *protocol = SocketProtocol::Tcp(TcpState::Connected {
            local,
            remote: peer,
        });
    });
    Ok(child)
}
