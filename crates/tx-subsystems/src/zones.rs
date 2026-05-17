use crate::adapter::step_engine::{epoch, zone, Zone, ZoneAllocated, ZoneError};
use tx_hal::{console_write_str, TxPlatform};

use crate::{
    mount::{MountIdentity, MountNamespace, MountPayload},
    page_backed::PageContainer,
    vfs::{DEntry, OpenFile, RNode},
    vm::AddressSpace,
};

pub(crate) struct ZoneSmokeObj {
    pub(crate) value: usize,
}

static ZONE_SMOKE: Zone<ZoneSmokeObj> = Zone::const_new();

unsafe impl ZoneAllocated for ZoneSmokeObj {
    fn zone() -> &'static Zone<Self> {
        &ZONE_SMOKE
    }
}

pub fn register_all() -> Result<(), ZoneError> {
    smoke::register_zones()?;
    process::register_zones()?;
    thread::register_zones()?;
    vm::register_zones()?;
    page_backed::register_zones()?;
    mount::register_zones()?;
    vfs::register_zones()?;
    tty::register_zones()?;
    pipe::register_zones()?;
    futex::register_zones()?;
    cred::register_zones()?;
    userfaultfd::register_zones()?;
    aio::register_zones()?;
    io_uring::register_zones()?;
    signalfd::register_zones()?;
    epoll::register_zones()?;
    eventfd::register_zones()?;
    timerfd::register_zones()?;
    subject_placeholders::register_zones()?;
    Ok(())
}

pub fn run_smoke<P: TxPlatform>() -> Result<(), ZoneError> {
    register_all()?;

    let reservation = zone::reserve_for::<ZoneSmokeObj>()?;
    let cap = zone::sign_for(reservation, ZoneSmokeObj { value: 7 });
    let weak = cap.downgrade();
    let upgraded = {
        let guard = epoch::guard();
        let ident = weak.observe(&guard).ok_or(ZoneError::SlotNotFound)?;
        if ident.value != 7 {
            return Err(ZoneError::InvalidState);
        }
        ident.to_cap().map_err(|_| ZoneError::SlotNotFound)?
    };
    if upgraded.value != 7 {
        return Err(ZoneError::InvalidState);
    }

    drop(cap);
    drop(upgraded);
    let _ = epoch::drain_with_budget(usize::MAX);
    let _ = epoch::drain_with_budget(usize::MAX);

    console_write_str::<P>("txkernel:zone:smoke:ok\n");
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KernelZoneSummary {
    pub epoch: crate::adapter::step_engine::EpochSummary,
    pub zone_count: usize,
    pub zones: [Option<crate::adapter::step_engine::ZoneInfo>; 32],
}

pub(crate) fn summary() -> KernelZoneSummary {
    let mut zones = [const { None }; 32];
    let zone_count = zone::snapshot(&mut zones);
    KernelZoneSummary {
        epoch: epoch::summary(),
        zone_count,
        zones,
    }
}

pub(crate) fn freeze_for_shutdown() -> Result<(), ZoneError> {
    zone::freeze_for_shutdown()
}

pub(crate) fn best_effort_maintenance_tick() -> zone::ZoneMaintenanceStats {
    zone::maintenance_tick(zone::ZoneMaintenanceBudget {
        epoch_reclaim_budget: usize::MAX,
        empty_slab_budget: usize::MAX,
    })
}

pub(crate) fn bounded_maintenance_tick() -> zone::ZoneMaintenanceStats {
    zone::maintenance_tick(zone::ZoneMaintenanceBudget {
        epoch_reclaim_budget: 32,
        empty_slab_budget: 4,
    })
}

pub fn try_bounded_maintenance_tick() {
    if !zone::is_initialized() {
        return;
    }
    let _ = bounded_maintenance_tick();
}

pub(crate) fn try_best_effort_maintenance_tick() {
    if !zone::is_initialized() {
        return;
    }
    let _ = best_effort_maintenance_tick();
}

pub fn panic_shutdown<P: TxPlatform>() -> ! {
    let _ = freeze_for_shutdown();
    dump_summary::<P>();
    loop {
        core::hint::spin_loop();
    }
}

pub fn shutdown_with_zone_cleanup<P: TxPlatform>() -> ! {
    let _ = freeze_for_shutdown();
    try_best_effort_maintenance_tick();
    dump_summary::<P>();
    P::system_off()
}

pub(crate) fn dump_summary<P: TxPlatform>() {
    let summary = summary();
    console_write_str::<P>("txkernel:zone:summary:epoch=");
    write_usize::<P>(summary.epoch.global_epoch as usize);
    console_write_str::<P>(":guards=");
    write_usize::<P>(summary.epoch.active_guards);
    console_write_str::<P>(":zones=");
    write_usize::<P>(summary.zone_count);
    console_write_str::<P>("\n");
}

fn write_usize<P: TxPlatform>(value: usize) {
    let mut digits = [0u8; 20];
    let mut len = 0usize;
    let mut n = value;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }

    let mut out = [0u8; 20];
    for (dst, src) in out[..len].iter_mut().zip(digits[..len].iter().rev()) {
        *dst = *src;
    }
    console_write_str::<P>(core::str::from_utf8(&out[..len]).unwrap_or("?"));
}

mod smoke {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<ZoneSmokeObj>()?;
        Ok(())
    }
}

mod process {
    use super::*;
    use crate::process::structure::{ProcessGroup, ProcessIdentity, ProcessPayload, Session};

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<ProcessIdentity>()?;
        zone::register_zone_for::<ProcessPayload>()?;
        zone::register_zone_for::<ProcessGroup>()?;
        zone::register_zone_for::<Session>()?;
        Ok(())
    }
}

mod thread {
    use super::*;
    use crate::thread_runtime::structure::{ThreadIdentity, ThreadPayload};

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<ThreadIdentity>()?;
        zone::register_zone_for::<ThreadPayload>()?;
        Ok(())
    }
}

mod vm {
    use super::*;
    use crate::vm::PrivatePageSet;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<AddressSpace>()?;
        zone::register_zone_for::<PrivatePageSet>()?;
        Ok(())
    }
}

mod page_backed {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<PageContainer>()?;
        Ok(())
    }
}

mod mount {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<MountIdentity>()?;
        zone::register_zone_for::<MountPayload>()?;
        zone::register_zone_for::<MountNamespace>()?;
        Ok(())
    }
}

mod vfs {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<DEntry>()?;
        zone::register_zone_for::<RNode>()?;
        zone::register_zone_for::<OpenFile>()?;
        Ok(())
    }
}

mod tty {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::tty::structure::registry::register_zones()
    }
}

mod pipe {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::pipe::register_zones()
    }
}

mod futex {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::futex::register_zones()
    }
}

mod cred {
    use super::*;
    use crate::cred::Cred;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<Cred>()?;
        Ok(())
    }
}

mod userfaultfd {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::userfaultfd::register_zones()
    }
}

mod aio {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::aio::register_zones()
    }
}

mod io_uring {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::io_uring::register_zones()
    }
}

mod signalfd {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::signalfd::register_zones()
    }
}

mod epoll {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        // TODO: epoll not yet landed
        Ok(())
    }
}

mod eventfd {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::eventfd::register_zones()
    }
}

mod timerfd {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::timerfd::register_zones()
    }
}

/// Register the `step_v3` placeholder zones used by PR-9 phase 5 to
/// satisfy `SubjectAuthority`'s `Cap<RestrictionStackHandle>` slot.
///
/// `RestrictionStackHandle` is a substrate-side placeholder
/// (per D5 §7); the real append-only stack lands in PR-K. Until then,
/// shim arms mint a fresh placeholder cap per syscall entry through
/// [`crate::cred::placeholder_restrictions_cap`].
mod subject_placeholders {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<crate::adapter::step_engine::RestrictionStackHandle>()
            .map(|_| ())?;
        Ok(())
    }
}
