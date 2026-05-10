use tx_substrate::zone::{self, Cap, PayloadCap};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::structure::table::SOCKET_TABLE;
use crate::net::structure::{
    SocketIdentity, SocketKind, SocketOptionSet, SocketPayload, ValidSocketType,
};

pub fn step_socket_create(
    valid: ValidSocketType,
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
        SocketPayload::new(kind, SocketOptionSet::for_kind(kind)),
    ));
    identity.install_payload(payload);

    if kind == SocketKind::RawIcmp && SOCKET_TABLE.register_raw_icmp(identity.clone()).is_err() {
        return StepOutcome::Err(Errno::ENOMEM);
    }

    StepOutcome::Done(identity)
}
