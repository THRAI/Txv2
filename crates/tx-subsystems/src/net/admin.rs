//! Network administration authority checks.

use crate::cred::{Capability, Cred};
use crate::execution::Errno;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetAdminAuthority {
    _private: (),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetRawAuthority {
    _private: (),
}

impl NetAdminAuthority {
    pub const fn for_test_or_bootstrap() -> Self {
        Self { _private: () }
    }
}

impl NetRawAuthority {
    pub const fn for_test_or_bootstrap() -> Self {
        Self { _private: () }
    }
}

pub fn require_net_admin(cred: Cred) -> Result<NetAdminAuthority, Errno> {
    if cred.is_privileged_for(Capability::NET_ADMIN) {
        Ok(NetAdminAuthority { _private: () })
    } else {
        Err(Errno::EPERM)
    }
}

pub fn require_net_raw(cred: Cred) -> Result<NetRawAuthority, Errno> {
    if cred.is_privileged_for(Capability::NET_RAW) {
        Ok(NetRawAuthority { _private: () })
    } else {
        Err(Errno::EPERM)
    }
}
