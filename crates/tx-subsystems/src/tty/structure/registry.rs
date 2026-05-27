//! Zone statics and `ZoneAllocated` implementations for TTY entity types.
//!
//! Called from `crate::zones::register_all()` during boot-time zone
//! initialisation.  Both zones must be registered before any TTY allocation
//! can succeed.

use crate::tty::adapter::step_engine::{
    register_zone_for, Cap, PayloadPolicy, SpinMutex, Zone, ZoneAllocated, ZoneError,
};
use crate::tty::structure::identity::FixedName;

use super::identity::TtyIdentity;
use super::payload::TtyPayload;

// ---------------------------------------------------------------------------
// Static zone storage
// ---------------------------------------------------------------------------

static TTY_IDENTITY_ZONE: Zone<TtyIdentity> = Zone::const_new();
static TTY_PAYLOAD_ZONE: Zone<TtyPayload> = Zone::const_new();

const MAX_HARDWARE_TTYS: usize = 8;
const MAX_DEVFS_ALIASES: usize = 16;
const MAX_PTYS: usize = 64;

pub const MAX_PTY_SLAVES: usize = MAX_PTYS;

static HARDWARE_TTYS: SpinMutex<RegistrySlots<MAX_HARDWARE_TTYS>> =
    SpinMutex::new(RegistrySlots::new());
static DEVFS_ALIASES: SpinMutex<AliasSlots<MAX_DEVFS_ALIASES>> = SpinMutex::new(AliasSlots::new());
static PTY_SLAVES: SpinMutex<RegistrySlots<MAX_PTYS>> = SpinMutex::new(RegistrySlots::new());
static NEXT_PTY_INDEX: SpinMutex<u32> = SpinMutex::new(0);

#[derive(Clone)]
pub struct TtyRegistryEntry {
    pub index: u32,
    pub tty: Cap<TtyIdentity>,
}

#[derive(Clone)]
pub struct TtyAliasEntry {
    pub name: FixedName<16>,
    pub tty: Cap<TtyIdentity>,
}

struct RegistrySlots<const N: usize> {
    entries: [Option<TtyRegistryEntry>; N],
}

impl<const N: usize> RegistrySlots<N> {
    const fn new() -> Self {
        Self {
            entries: [const { None }; N],
        }
    }

    fn insert(&mut self, index: u32, tty: Cap<TtyIdentity>) -> Result<(), RegistryError> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.index == index)
        {
            entry.tty = tty;
            return Ok(());
        }

        let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) else {
            return Err(RegistryError::Full);
        };
        *slot = Some(TtyRegistryEntry { index, tty });
        Ok(())
    }

    fn get(&self, index: u32) -> Option<Cap<TtyIdentity>> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.index == index)
            .map(|entry| entry.tty.clone())
    }

    fn contains(&self, index: u32) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|entry| entry.index == index)
    }

    fn remove(&mut self, index: u32) -> Option<Cap<TtyIdentity>> {
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|entry| entry.index == index))?;
        slot.take().map(|entry| entry.tty)
    }

    fn indices(&self) -> ([u32; N], usize) {
        let mut out = [0u32; N];
        let mut len = 0usize;

        for entry in self.entries.iter().flatten() {
            out[len] = entry.index;
            len += 1;
        }

        out[..len].sort_unstable();
        (out, len)
    }
}

struct AliasSlots<const N: usize> {
    entries: [Option<TtyAliasEntry>; N],
}

impl<const N: usize> AliasSlots<N> {
    const fn new() -> Self {
        Self {
            entries: [const { None }; N],
        }
    }

    fn insert(&mut self, name: &str, tty: Cap<TtyIdentity>) -> Result<(), RegistryError> {
        let name = FixedName::from_name(name);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.name.as_bytes() == name.as_bytes())
        {
            entry.tty = tty;
            return Ok(());
        }

        let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) else {
            return Err(RegistryError::Full);
        };
        *slot = Some(TtyAliasEntry { name, tty });
        Ok(())
    }

    fn get(&self, name: &[u8]) -> Option<Cap<TtyIdentity>> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.name.as_bytes() == name)
            .map(|entry| entry.tty.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    Full,
}

// ---------------------------------------------------------------------------
// ZoneAllocated implementations
// ---------------------------------------------------------------------------

// SAFETY: `TTY_IDENTITY_ZONE` is the single process-wide zone for
// `TtyIdentity`.  All `Cap<TtyIdentity>` and `Weak<TtyIdentity>` route
// through this static; returning any other zone would break slot-key lookups.
unsafe impl ZoneAllocated for TtyIdentity {
    fn zone() -> &'static Zone<Self> {
        &TTY_IDENTITY_ZONE
    }
}

// SAFETY: same guarantee for `TtyPayload`.
unsafe impl ZoneAllocated for TtyPayload {
    type Policy = PayloadPolicy<Self>;
    fn zone() -> &'static Zone<Self> {
        &TTY_PAYLOAD_ZONE
    }
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// Register TTY identity and payload zones.
///
/// Must be called exactly once, during the `register_all()` boot phase, before
/// any TTY zone allocation.
pub(crate) fn register_zones() -> Result<(), ZoneError> {
    register_zone_for::<TtyIdentity>()?;
    register_zone_for::<TtyPayload>()?;
    Ok(())
}

pub fn register_hardware_tty(index: u32, tty: Cap<TtyIdentity>) -> Result<(), RegistryError> {
    HARDWARE_TTYS.lock().insert(index, tty)
}

pub fn hardware_tty(index: u32) -> Option<Cap<TtyIdentity>> {
    HARDWARE_TTYS.lock().get(index)
}

pub fn register_devfs_alias(name: &str, tty: Cap<TtyIdentity>) -> Result<(), RegistryError> {
    DEVFS_ALIASES.lock().insert(name, tty)
}

pub fn devfs_alias(name: &[u8]) -> Option<Cap<TtyIdentity>> {
    DEVFS_ALIASES.lock().get(name)
}

/// Snapshot the currently-registered devfs alias entries.
///
/// Returned in registry-storage order, with each entry cloned out (the
/// alias table itself is not exposed). Used by the devfs `FsOps::readdir`
/// implementation in `tx-fs` to enumerate `/dev` without coupling to the
/// alias-slot storage type.
pub fn devfs_alias_snapshot() -> alloc::vec::Vec<TtyAliasEntry> {
    let guard = DEVFS_ALIASES.lock();
    let mut out = alloc::vec::Vec::new();
    for entry in guard.entries.iter().flatten() {
        out.push(entry.clone());
    }
    out
}

pub fn allocate_pty_index() -> Result<u32, RegistryError> {
    for _ in 0..MAX_PTYS {
        let index = {
            let mut next = NEXT_PTY_INDEX.lock();
            let index = *next;
            *next = next.wrapping_add(1);
            index
        };
        if !PTY_SLAVES.lock().contains(index) {
            return Ok(index);
        }
    }
    Err(RegistryError::Full)
}

pub fn register_pty_slave(index: u32, slave: Cap<TtyIdentity>) -> Result<(), RegistryError> {
    PTY_SLAVES.lock().insert(index, slave)
}

pub fn pty_slave(index: u32) -> Option<Cap<TtyIdentity>> {
    PTY_SLAVES.lock().get(index)
}

pub fn contains_pty_slave(index: u32) -> bool {
    PTY_SLAVES.lock().contains(index)
}

pub fn unregister_pty_slave(index: u32) -> Option<Cap<TtyIdentity>> {
    PTY_SLAVES.lock().remove(index)
}

pub fn list_pty_slave_indices() -> ([u32; MAX_PTY_SLAVES], usize) {
    PTY_SLAVES.lock().indices()
}

#[cfg(test)]
pub fn reset_for_tests() {
    *HARDWARE_TTYS.lock() = RegistrySlots::new();
    *DEVFS_ALIASES.lock() = AliasSlots::new();
    *PTY_SLAVES.lock() = RegistrySlots::new();
    *NEXT_PTY_INDEX.lock() = 0;
}
