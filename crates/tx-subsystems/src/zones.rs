use crate::adapter::step_engine::{
    drain_with_budget, guard, page_allocator, summary as epoch_summary, zone, Zone, ZoneAllocated,
    ZoneError,
};
use tx_hal::{console_write_str, ConsoleIf, TxPlatform};
use tx_substrate::slab;

use crate::{
    mount::{MountIdentity, MountNamespace, MountPayload},
    net,
    page_backed::PageContainer,
    vfs::structure::FsNotifyInstance,
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
    nsproxy::register_zones()?;
    process::register_zones()?;
    thread::register_zones()?;
    vm::register_zones()?;
    page_backed::register_zones()?;
    mount::register_zones()?;
    vfs::register_zones()?;
    tty::register_zones()?;
    pipe::register_zones()?;
    futex::register_zones()?;
    ipc::register_zones()?;
    cred::register_zones()?;
    userfaultfd::register_zones()?;
    aio::register_zones()?;
    io_uring::register_zones()?;
    signalfd::register_zones()?;
    epoll::register_zones()?;
    eventfd::register_zones()?;
    timerfd::register_zones()?;
    net::register_zones()?;
    subject_placeholders::register_zones()?;
    Ok(())
}

pub fn run_smoke<P: TxPlatform>() -> Result<(), ZoneError> {
    register_all()?;

    let reservation = zone::reserve_for::<ZoneSmokeObj>()?;
    let cap = zone::sign_for(reservation, ZoneSmokeObj { value: 7 });
    let weak = cap.downgrade();
    let upgraded = {
        let guard = guard();
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
    let _ = drain_with_budget(usize::MAX);
    let _ = drain_with_budget(usize::MAX);

    console_write_str::<P>("txkernel:zone:smoke:ok\n");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelZoneSummary {
    pub epoch: crate::adapter::step_engine::EpochSummary,
    pub zone_count: usize,
    pub captured_zones: usize,
    pub zones: [Option<crate::adapter::step_engine::ZoneInfo>; 64],
}

pub(crate) fn summary() -> KernelZoneSummary {
    let mut zones = [const { None }; 64];
    let captured_zones = zone::snapshot(&mut zones);
    KernelZoneSummary {
        epoch: epoch_summary(),
        zone_count: zone::registered_zone_count(),
        captured_zones,
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
        epoch_reclaim_budget: 512,
        empty_slab_budget: 512,
    })
}

pub fn try_bounded_maintenance_tick() {
    if !zone::is_initialized() {
        return;
    }
    let _ = bounded_maintenance_tick();
}

pub fn try_best_effort_maintenance_tick() {
    if !zone::is_initialized() {
        return;
    }
    let _ = best_effort_maintenance_tick();
}

pub fn try_memory_pressure_maintenance_tick() {
    if !zone::is_initialized() {
        return;
    }
    let _ = crate::page_backed::reclaim_clean_file_pages(usize::MAX);
    let _ = best_effort_maintenance_tick();
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

pub fn shutdown_with_quiet_zone_cleanup<P: TxPlatform>() -> ! {
    let _ = freeze_for_shutdown();
    try_best_effort_maintenance_tick();
    P::system_off()
}

pub fn dump_summary<P: ConsoleIf>() {
    let summary = summary();
    console_write_str::<P>("txkernel:zone:summary:epoch=");
    write_usize::<P>(summary.epoch.global_epoch as usize);
    console_write_str::<P>(":guards=");
    write_usize::<P>(summary.epoch.active_guards);
    console_write_str::<P>(":zones=");
    write_usize::<P>(summary.zone_count);
    console_write_str::<P>(":captured=");
    write_usize::<P>(summary.captured_zones);
    console_write_str::<P>("\n");

    if let Ok(diag) = page_allocator::backend_diagnostics() {
        console_write_str::<P>("txkernel:pagealloc:free=");
        write_usize::<P>(diag.free_count);
        console_write_str::<P>(":total=");
        write_usize::<P>(diag.total_count);
        console_write_str::<P>(":max_run=");
        write_usize::<P>(diag.max_contiguous_free_run);
        console_write_str::<P>("\n");
    }
    if let Some(fail) = slab::last_allocation_failure() {
        console_write_str::<P>("txkernel:heap:last_alloc_fail:size=");
        write_usize::<P>(fail.size);
        console_write_str::<P>(":align=");
        write_usize::<P>(fail.align);
        console_write_str::<P>(":pages=");
        write_usize::<P>(fail.pages);
        console_write_str::<P>(":free=");
        write_usize::<P>(fail.free_count);
        console_write_str::<P>(":total=");
        write_usize::<P>(fail.total_count);
        console_write_str::<P>(":max_run=");
        write_usize::<P>(fail.max_contiguous_free_run);
        console_write_str::<P>(":caller_ra=0x");
        write_hex_usize::<P>(fail.caller_ra);
        console_write_str::<P>("\n");
    }

    let wait_sources = crate::wait_source::registry_summary();
    console_write_str::<P>("txkernel:wait_source:registered=");
    write_usize::<P>(wait_sources.total);
    console_write_str::<P>(":wait=");
    write_usize::<P>(wait_sources.wait_sources);
    console_write_str::<P>(":queues=");
    write_usize::<P>(wait_sources.raw_queues);
    console_write_str::<P>(":ports=");
    write_usize::<P>(wait_sources.raw_ports);
    console_write_str::<P>("\n");

    let wake_sources = crate::adapter::step_engine::wake_registry_summary();
    console_write_str::<P>("txkernel:wake_source:slots=");
    write_usize::<P>(wake_sources.slots);
    console_write_str::<P>(":live=");
    write_usize::<P>(wake_sources.live);
    console_write_str::<P>("\n");

    dump_process_summary::<P>();

    for zone in summary.zones.iter().flatten() {
        if zone.allocated_slots == 0 && zone.slab_count == 0 {
            continue;
        }
        console_write_str::<P>("txkernel:zone:detail:id=");
        write_usize::<P>(zone.id.0);
        console_write_str::<P>(":type=");
        console_write_str::<P>(zone.type_name);
        console_write_str::<P>(":alloc=");
        write_usize::<P>(zone.allocated_slots);
        console_write_str::<P>(":slabs=");
        write_usize::<P>(zone.slab_count);
        console_write_str::<P>(":empty=");
        write_usize::<P>(zone.empty_slab_count);
        console_write_str::<P>("\n");
    }
}

fn dump_process_summary<P: ConsoleIf>() {
    let pids = crate::process::all_pids();
    let mut live = 0usize;
    let mut zombies = 0usize;
    let mut total_threads = 0usize;
    let mut total_recipes = 0usize;
    let mut total_mapped_pages = 0usize;
    let mut total_vm_bytes = 0usize;

    for (pid, _) in pids {
        let Some(proc_cap) = crate::process::process_by_pid(pid) else {
            continue;
        };
        let state = proc_cap.state_char() as char;
        let threads = proc_cap.live_thread_count();
        let comm_bytes = proc_cap.comm();
        let comm = proc_comm_bytes(&comm_bytes);
        let (recipes, vm_size, mapped_pages, zombie) = match proc_cap.aspace_cap() {
            Some(aspace) => {
                let stats = aspace.stats();
                let pmap = aspace.pmap().stats();
                (stats.recipe_count, stats.vm_size, pmap.mapped_pages, false)
            }
            None => (0, 0, 0, true),
        };

        if zombie {
            zombies += 1;
        } else {
            live += 1;
        }
        total_threads += threads;
        total_recipes += recipes;
        total_mapped_pages += mapped_pages;
        total_vm_bytes += vm_size;

        console_write_str::<P>("txkernel:proc:pid=");
        write_usize::<P>(pid.0 as usize);
        console_write_str::<P>(":state=");
        write_char::<P>(state);
        console_write_str::<P>(":threads=");
        write_usize::<P>(threads);
        console_write_str::<P>(":recipes=");
        write_usize::<P>(recipes);
        console_write_str::<P>(":mapped=");
        write_usize::<P>(mapped_pages);
        console_write_str::<P>(":vm=");
        write_usize::<P>(vm_size);
        console_write_str::<P>(":comm=");
        console_write_str::<P>(comm);
        console_write_str::<P>("\n");
    }

    console_write_str::<P>("txkernel:proc:summary:live=");
    write_usize::<P>(live);
    console_write_str::<P>(":zombies=");
    write_usize::<P>(zombies);
    console_write_str::<P>(":threads=");
    write_usize::<P>(total_threads);
    console_write_str::<P>(":recipes=");
    write_usize::<P>(total_recipes);
    console_write_str::<P>(":mapped=");
    write_usize::<P>(total_mapped_pages);
    console_write_str::<P>(":vm=");
    write_usize::<P>(total_vm_bytes);
    console_write_str::<P>("\n");
}

fn proc_comm_bytes(bytes: &[u8; 16]) -> &str {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("?")
}

fn write_char<P: ConsoleIf>(value: char) {
    let mut buf = [0u8; 4];
    let s = value.encode_utf8(&mut buf);
    console_write_str::<P>(s);
}

fn write_usize<P: ConsoleIf>(value: usize) {
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

fn write_hex_usize<P: ConsoleIf>(value: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [0u8; core::mem::size_of::<usize>() * 2];
    let mut len = 0usize;
    let mut n = value;
    loop {
        digits[len] = HEX[n & 0xf];
        len += 1;
        n >>= 4;
        if n == 0 {
            break;
        }
    }

    let mut out = [0u8; core::mem::size_of::<usize>() * 2];
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
        zone::register_zone_for::<crate::mount::MountApiFile>()?;
        Ok(())
    }
}

mod vfs {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<DEntry>()?;
        zone::register_zone_for::<RNode>()?;
        zone::register_zone_for::<OpenFile>()?;
        zone::register_zone_for::<FsNotifyInstance>()?;
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

mod ipc {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::ipc::register_zones()
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
        crate::epoll::register_zones()
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
mod nsproxy {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        crate::process::nsproxy::register_zones()
    }
}

mod subject_placeholders {
    use super::*;

    pub(super) fn register_zones() -> Result<(), ZoneError> {
        zone::register_zone_for::<crate::adapter::step_engine::RestrictionStackHandle>()
            .map(|_| ())?;
        Ok(())
    }
}
