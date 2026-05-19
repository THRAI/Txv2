use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::structure::{
    SocketIdentity, SocketKind, SocketOptionSet, SocketPayload, ValidSocketType,
};

pub fn step_socket_create(
    valid: ValidSocketType,
    guard: &Guard<'_>,
) -> StepOutcome<Cap<SocketIdentity>> {
    step_socket_create_in_namespace(valid, initial_net_namespace_payload(), guard)
}

pub fn step_socket_create_in_namespace(
    valid: ValidSocketType,
    net_namespace: PayloadCap<NetNamespacePayload>,
    _guard: &Guard<'_>,
) -> StepOutcome<Cap<SocketIdentity>> {
    let kind = match SocketKind::from_valid_socket_type(valid) {
        Ok(kind) => kind,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let identity_res = match zone::reserve_for::<SocketIdentity>() {
        Ok(reservation) => reservation,
        Err(_) => return StepOutcome::Err(Errno::ENOMEM),
    };
    let payload_res = match zone::reserve_for::<SocketPayload>() {
        Ok(reservation) => reservation,
        Err(_) => return StepOutcome::Err(Errno::ENOMEM),
    };

    let identity = zone::sign_for(identity_res, SocketIdentity::new(kind));
    let payload = PayloadCap::from_cap(zone::sign_for(
        payload_res,
        SocketPayload::new_in_namespace(kind, SocketOptionSet::for_kind(kind), net_namespace),
    ));
    identity.install_payload(payload);

    let Some(payload) = identity.live_payload() else {
        return StepOutcome::Err(Errno::ENOMEM);
    };
    if kind == SocketKind::RawIcmp
        && payload
            .socket_table()
            .register_raw_icmp(identity.clone())
            .is_err()
    {
        return StepOutcome::Err(Errno::ENOMEM);
    }

    StepOutcome::Done(identity)
}
