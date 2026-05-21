//! Per-socket IPv4 multicast membership state.
//!
//! Socket options such as `MCAST_JOIN_GROUP` mutate runtime membership state,
//! not the cloneable socket-option defaults used when TCP accepts children.

use crate::execution::Errno;

use super::types::Ipv4Address;

const IPV4_MULTICAST_MEMBERSHIP_SLOTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4MulticastGroup {
    pub interface: u32,
    pub group: Ipv4Address,
}

impl Ipv4MulticastGroup {
    pub const fn new(interface: u32, group: Ipv4Address) -> Self {
        Self { interface, group }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4MulticastMemberships {
    entries: [Option<Ipv4MulticastGroup>; IPV4_MULTICAST_MEMBERSHIP_SLOTS],
}

impl Ipv4MulticastMemberships {
    pub(crate) const fn empty() -> Self {
        Self {
            entries: [None; IPV4_MULTICAST_MEMBERSHIP_SLOTS],
        }
    }

    pub(crate) fn join(&mut self, group: Ipv4MulticastGroup) -> Result<(), Errno> {
        if !group.group.is_multicast() {
            return Err(Errno::EINVAL);
        }
        if self.entries.iter().any(|entry| *entry == Some(group)) {
            return Ok(());
        }
        let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) else {
            return Err(Errno::ENOMEM);
        };
        *slot = Some(group);
        Ok(())
    }

    pub(crate) fn leave(&mut self, group: Ipv4MulticastGroup) -> Result<(), Errno> {
        if !group.group.is_multicast() {
            return Err(Errno::EINVAL);
        }
        let Some(slot) = self.entries.iter_mut().find(|entry| **entry == Some(group)) else {
            return Err(Errno::EADDRNOTAVAIL);
        };
        *slot = None;
        Ok(())
    }
}
