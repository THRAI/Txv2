use crate::adapter::step_engine::{epoch, page_allocator, zone, Zone, ZoneAllocated, ZoneError};
use tx_hal::{console_write_str, TxPlatform};
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
        epoch: epoch::summary(),
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

pub fn shutdown_with_quiet_zone_cleanup<P: TxPlatform>() -> ! {
    let _ = freeze_for_shutdown();
    try_best_effort_maintenance_tick();
    P::system_off()
}

pub fn dump_summary<P: TxPlatform>() {
    let summary = summary();
    console_write_str::<P>("txkernel:zone:summary:epoch=");
    write_usize::<P>(summary.epoch.global_epoch as usize);
    console_write_str::<P>(":guards=");
    write_usize::<P>(summary.epoch.active_guards);
    console_write_str::<P>(":zones=");
    write_usize::<P>(summary.zone_count);
    console_write_str::<P>(":captured=");
    write_usize::<P>(summary.captured_zones);
    console_write_str::<P>(":retired=");
    write_usize::<P>(summary.epoch.retired_count);
    console_write_str::<P>(":collect_requested=");
    write_usize::<P>(summary.epoch.collection_requested as usize);
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
    if let Ok(diag) = page_allocator::frame_role_diagnostics() {
        console_write_str::<P>("txkernel:pagealloc:roles:free=");
        write_usize::<P>(diag.free_pages);
        console_write_str::<P>(":reserved=");
        write_usize::<P>(diag.reserved_pages);
        console_write_str::<P>(":reserved_live=");
        write_usize::<P>(diag.reserved_live_pages);
        console_write_str::<P>(":unowned_unavailable=");
        write_usize::<P>(diag.unowned_unavailable_pages);
        console_write_str::<P>(":owned=");
        write_usize::<P>(diag.owned_pages);
        console_write_str::<P>(":mapped=");
        write_usize::<P>(diag.mapped_pages);
        console_write_str::<P>(":cached=");
        write_usize::<P>(diag.cached_pages);
        console_write_str::<P>(":pinned=");
        write_usize::<P>(diag.pinned_pages);
        console_write_str::<P>(":mixed=");
        write_usize::<P>(diag.mixed_role_pages);
        console_write_str::<P>("\n");

        console_write_str::<P>("txkernel:pagealloc:refs:owner=");
        write_usize::<P>(diag.owner_refs);
        console_write_str::<P>(":map=");
        write_usize::<P>(diag.map_refs);
        console_write_str::<P>(":cache=");
        write_usize::<P>(diag.cache_refs);
        console_write_str::<P>(":pin=");
        write_usize::<P>(diag.pin_refs);
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
        console_write_str::<P>("\n");

        let heap = slab::heap_diagnostics();
        console_write_str::<P>("txkernel:heap:pages:small_live=");
        write_usize::<P>(heap.small_live_pages);
        console_write_str::<P>(":small_peak=");
        write_usize::<P>(heap.small_peak_pages);
        console_write_str::<P>(":large_live=");
        write_usize::<P>(heap.large_live_pages);
        console_write_str::<P>(":large_peak=");
        write_usize::<P>(heap.large_peak_pages);
        console_write_str::<P>(":large_allocations=");
        write_usize::<P>(heap.large_live_allocations);
        console_write_str::<P>(":alloc_calls=");
        write_usize::<P>(heap.large_alloc_calls);
        console_write_str::<P>(":free_calls=");
        write_usize::<P>(heap.large_free_calls);
        console_write_str::<P>("\n");

        console_write_str::<P>("txkernel:heap:large_bins:allocs_1=");
        write_usize::<P>(heap.large_live_allocations_by_bin[0]);
        console_write_str::<P>(":allocs_2_15=");
        write_usize::<P>(heap.large_live_allocations_by_bin[1]);
        console_write_str::<P>(":allocs_16_255=");
        write_usize::<P>(heap.large_live_allocations_by_bin[2]);
        console_write_str::<P>(":allocs_256_plus=");
        write_usize::<P>(heap.large_live_allocations_by_bin[3]);
        console_write_str::<P>(":pages_1=");
        write_usize::<P>(heap.large_live_pages_by_bin[0]);
        console_write_str::<P>(":pages_2_15=");
        write_usize::<P>(heap.large_live_pages_by_bin[1]);
        console_write_str::<P>(":pages_16_255=");
        write_usize::<P>(heap.large_live_pages_by_bin[2]);
        console_write_str::<P>(":pages_256_plus=");
        write_usize::<P>(heap.large_live_pages_by_bin[3]);
        console_write_str::<P>("\n");

        for index in 0..heap.small_class_sizes.len() {
            if heap.small_class_pages[index] == 0 {
                continue;
            }
            console_write_str::<P>("txkernel:heap:class:size=");
            write_usize::<P>(heap.small_class_sizes[index]);
            console_write_str::<P>(":pages=");
            write_usize::<P>(heap.small_class_pages[index]);
            console_write_str::<P>(":used=");
            write_usize::<P>(heap.small_class_used_objects[index]);
            console_write_str::<P>(":capacity=");
            write_usize::<P>(heap.small_class_capacity_objects[index]);
            console_write_str::<P>(":empty=");
            write_usize::<P>(heap.small_class_empty_pages[index]);
            console_write_str::<P>(":partial=");
            write_usize::<P>(heap.small_class_partial_pages[index]);
            console_write_str::<P>(":full=");
            write_usize::<P>(heap.small_class_full_pages[index]);
            console_write_str::<P>("\n");
        }

        let vmalloc = slab::vmalloc_diagnostics();
        console_write_str::<P>("txkernel:vmalloc:ready=");
        write_usize::<P>(vmalloc.ready as usize);
        console_write_str::<P>(":initialized=");
        write_usize::<P>(vmalloc.initialized as usize);
        console_write_str::<P>(":window_pages=");
        write_usize::<P>(vmalloc.window_pages);
        console_write_str::<P>(":bitmap_used=");
        write_usize::<P>(vmalloc.bitmap_used_pages);
        console_write_str::<P>(":bitmap_free=");
        write_usize::<P>(vmalloc.bitmap_free_pages);
        console_write_str::<P>(":bitmap_max_run=");
        write_usize::<P>(vmalloc.bitmap_max_free_run);
        console_write_str::<P>(":hint=");
        write_usize::<P>(vmalloc.hint);
        console_write_str::<P>("\n");

        console_write_str::<P>("txkernel:vmalloc:alloc:attempts=");
        write_usize::<P>(vmalloc.alloc_attempts);
        console_write_str::<P>(":successes=");
        write_usize::<P>(vmalloc.alloc_successes);
        console_write_str::<P>(":live_allocations=");
        write_usize::<P>(vmalloc.live_allocations);
        console_write_str::<P>(":live_pages=");
        write_usize::<P>(vmalloc.live_pages);
        console_write_str::<P>(":peak_live_pages=");
        write_usize::<P>(vmalloc.peak_live_pages);
        console_write_str::<P>(":unmap_attempts=");
        write_usize::<P>(vmalloc.unmap_attempts);
        console_write_str::<P>(":partial_unmaps=");
        write_usize::<P>(vmalloc.partial_unmaps);
        console_write_str::<P>(":bad_deallocs=");
        write_usize::<P>(vmalloc.bad_deallocs);
        console_write_str::<P>(":quarantined_ranges=");
        write_usize::<P>(vmalloc.quarantined_ranges);
        console_write_str::<P>(":quarantined_pages=");
        write_usize::<P>(vmalloc.quarantined_pages);
        console_write_str::<P>("\n");

        console_write_str::<P>("txkernel:vmalloc:last_fail:stage=");
        console_write_str::<P>(slab::vmalloc_failure_stage_name(vmalloc.last_failure_stage));
        console_write_str::<P>(":stage_code=");
        write_usize::<P>(vmalloc.last_failure_stage);
        console_write_str::<P>(":cause=");
        console_write_str::<P>(slab::vmalloc_failure_stage_name(
            vmalloc.last_failure_cause_stage,
        ));
        console_write_str::<P>(":request_pages=");
        write_usize::<P>(vmalloc.last_failure_request_pages);
        console_write_str::<P>(":mapped_pages=");
        write_usize::<P>(vmalloc.last_failure_mapped_pages);
        console_write_str::<P>(":start_page=");
        write_usize::<P>(vmalloc.last_failure_start_page);
        console_write_str::<P>(":pmap_error=");
        console_write_str::<P>(slab::vmalloc_pmap_error_name(
            vmalloc.last_failure_pmap_error,
        ));
        console_write_str::<P>(":alloc_error=");
        console_write_str::<P>(slab::vmalloc_alloc_error_name(
            vmalloc.last_failure_alloc_error,
        ));
        console_write_str::<P>("\n");

        console_write_str::<P>("txkernel:vmalloc:last_unmap:request_pages=");
        write_usize::<P>(vmalloc.last_unmap_request_pages);
        console_write_str::<P>(":removed_pages=");
        write_usize::<P>(vmalloc.last_unmap_removed_pages);
        console_write_str::<P>(":missing_pages=");
        write_usize::<P>(vmalloc.last_unmap_missing_pages);
        console_write_str::<P>(":frame_lookup_failures=");
        write_usize::<P>(vmalloc.last_unmap_frame_lookup_failures);
        console_write_str::<P>(":first_failed_page=");
        write_usize::<P>(vmalloc.last_unmap_first_failed_page);
        console_write_str::<P>(":pmap_error=");
        console_write_str::<P>(slab::vmalloc_pmap_error_name(vmalloc.last_unmap_pmap_error));
        console_write_str::<P>("\n");
    }

    let wait_sources = tx_substrate::wake::registry_summary();
    console_write_str::<P>("txkernel:wait_source:registered=");
    write_usize::<P>(wait_sources.live);
    console_write_str::<P>(":slots=");
    write_usize::<P>(wait_sources.slots);
    console_write_str::<P>("\n");

    let wake_sources = crate::adapter::step_engine::wake::registry_summary();
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

fn dump_process_summary<P: TxPlatform>() {
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

/// Emit a one-shot process/wait snapshot for an SMP reactor-wide idle stall.
///
/// This is intentionally separate from the OOM dump above: the kernel calls
/// it only after every online hart has reported an idle reactor for a bounded
/// interval.  It does not run on ordinary syscall or scheduler hot paths.
pub fn dump_smp_wait_diagnostics<P: TxPlatform>() {
    console_write_str::<P>("txkernel:smp-stall:processes:begin\n");

    for (pid, _) in crate::process::all_pids() {
        let Some(process) = crate::process::process_by_pid(pid) else {
            continue;
        };
        let children = process.children();
        let zombie_children = children.iter().filter(|child| child.is_zombie()).count();
        let threads = process.threads_snapshot().unwrap_or_default();
        let fds = process.open_fds();
        let comm_bytes = process.comm();

        console_write_str::<P>("txkernel:smp-stall:proc:pid=");
        write_usize::<P>(pid.0 as usize);
        console_write_str::<P>(":ppid=");
        write_usize::<P>(process.parent_pid().0 as usize);
        console_write_str::<P>(":state=");
        write_char::<P>(process.state_char() as char);
        console_write_str::<P>(":children=");
        write_usize::<P>(children.len());
        console_write_str::<P>(":zombie_children=");
        write_usize::<P>(zombie_children);
        console_write_str::<P>(":threads=");
        write_usize::<P>(threads.len());
        console_write_str::<P>(":fds=");
        write_usize::<P>(fds.len());
        console_write_str::<P>(":comm=");
        console_write_str::<P>(proc_comm_bytes(&comm_bytes));
        if let Some(source) = process.exit_wait_source() {
            console_write_str::<P>(":exit_source=");
            write_hex_u64::<P>(source.id().raw());
            console_write_str::<P>(":exit_subscribers=");
            write_usize::<P>(source.subscriber_count());
            console_write_str::<P>(":exit_pending=");
            write_hex_u64::<P>(source.pending_mask_snapshot());
            let registry_matches = tx_substrate::wake::lookup_source(source.id())
                .is_some_and(|registered| alloc::sync::Arc::ptr_eq(&registered, &source));
            console_write_str::<P>(":exit_registry_matches=");
            write_usize::<P>(registry_matches as usize);
        } else {
            console_write_str::<P>(":exit_source=none");
        }
        console_write_str::<P>("\n");

        for thread in threads {
            let payload = thread.payload_cap();
            console_write_str::<P>("txkernel:smp-stall:thread:pid=");
            write_usize::<P>(pid.0 as usize);
            console_write_str::<P>(":tid=");
            write_usize::<P>(thread.tid.0 as usize);
            console_write_str::<P>(":state=");
            write_char::<P>(thread.proc_state_char() as char);
            console_write_str::<P>(":futex=");
            write_usize::<P>(crate::futex::thread_has_waiter(thread.tid.0) as usize);

            if let Some(payload) = payload {
                console_write_str::<P>(":sleeping=");
                write_usize::<P>(payload.proc_sleeping() as usize);
                if let Some(mailbox) = payload.mailbox_handle().and_then(|weak| weak.upgrade()) {
                    console_write_str::<P>(":mailbox_task=");
                    write_usize::<P>(mailbox.task_id_low() as usize);
                    console_write_str::<P>(":mailbox_len=");
                    write_usize::<P>(mailbox.len());
                    console_write_str::<P>(":mailbox_overflow=");
                    write_usize::<P>(mailbox.overflow() as usize);
                    console_write_str::<P>(":mailbox_waker=");
                    write_usize::<P>(mailbox.has_waker() as usize);
                    console_write_str::<P>(":mailbox_generation=");
                    write_usize::<P>(mailbox.current_generation().raw() as usize);
                } else {
                    console_write_str::<P>(":mailbox=none");
                }
                if let Some(task) = payload.task() {
                    console_write_str::<P>(":task=");
                    write_usize::<P>(task.id().index());
                    console_write_str::<P>("/");
                    write_usize::<P>(task.generation().value() as usize);
                } else {
                    console_write_str::<P>(":task=none");
                }
                if let Some((nr, arg0, arg1)) = payload.active_syscall_diagnostic() {
                    console_write_str::<P>(":syscall=");
                    console_write_str::<P>(syscall_wait_name(nr));
                    console_write_str::<P>(":nr=");
                    write_usize::<P>(nr as usize);
                    console_write_str::<P>(":a0=");
                    write_hex_u64::<P>(arg0);
                    console_write_str::<P>(":a1=");
                    write_hex_u64::<P>(arg1);
                } else {
                    console_write_str::<P>(":syscall=none");
                }
            } else {
                console_write_str::<P>(":payload=none");
            }
            console_write_str::<P>("\n");
        }

        for (fd, file) in fds {
            if let Some((pipe, side)) = file.pipe_endpoint() {
                dump_pipe_fd::<P>(
                    pid.0,
                    fd,
                    match side {
                        crate::pipe::PipeSide::Reader => "read",
                        crate::pipe::PipeSide::Writer => "write",
                    },
                    &pipe,
                );
            }
            if let Some((rx, tx)) = file.socketpair_endpoint() {
                dump_pipe_fd::<P>(pid.0, fd, "socketpair-rx", &rx);
                dump_pipe_fd::<P>(pid.0, fd, "socketpair-tx", &tx);
            }
        }
    }

    console_write_str::<P>("txkernel:smp-stall:processes:end\n");
}

fn dump_pipe_fd<P: TxPlatform>(
    pid: u32,
    fd: u32,
    side: &str,
    pipe: &crate::adapter::step_engine::Cap<crate::pipe::PipePayload>,
) {
    let state = pipe.diagnostic_snapshot();
    console_write_str::<P>("txkernel:smp-stall:pipe:pid=");
    write_usize::<P>(pid as usize);
    console_write_str::<P>(":fd=");
    write_usize::<P>(fd as usize);
    console_write_str::<P>(":side=");
    console_write_str::<P>(side);
    console_write_str::<P>(":raw=");
    write_hex_u64::<P>(pipe.raw() as u64);
    console_write_str::<P>(":readers=");
    write_usize::<P>(state.readers as usize);
    console_write_str::<P>(":writers=");
    write_usize::<P>(state.writers as usize);
    console_write_str::<P>(":bytes=");
    write_usize::<P>(state.buffered_bytes);
    console_write_str::<P>(":slots=");
    write_usize::<P>(state.occupied_slots);
    console_write_str::<P>("/");
    write_usize::<P>(state.max_slots);
    console_write_str::<P>(":read_source=");
    write_hex_u64::<P>(state.reader_wait_source_id);
    console_write_str::<P>(":write_source=");
    write_hex_u64::<P>(state.writer_wait_source_id);
    console_write_str::<P>("\n");
}

fn syscall_wait_name(nr: u64) -> &'static str {
    match nr {
        23 => "dup",
        24 => "dup3",
        57 => "close",
        59 => "pipe2",
        63 => "read",
        64 => "write",
        65 => "readv",
        66 => "writev",
        72 => "pselect6",
        73 => "ppoll",
        93 => "exit",
        94 => "exit_group",
        98 => "futex",
        101 => "nanosleep",
        198 => "socket",
        199 => "socketpair",
        202 => "accept",
        203 => "connect",
        207 => "recvfrom",
        211 => "sendmsg",
        212 => "recvmsg",
        220 => "clone",
        221 => "execve",
        260 => "wait4",
        281 => "epoll_pwait",
        435 => "clone3",
        _ => "other",
    }
}

fn proc_comm_bytes(bytes: &[u8; 16]) -> &str {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("?")
}

fn write_char<P: TxPlatform>(value: char) {
    let mut buf = [0u8; 4];
    let s = value.encode_utf8(&mut buf);
    console_write_str::<P>(s);
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

fn write_hex_u64<P: TxPlatform>(value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [b'0'; 18];
    out[1] = b'x';
    for index in 0..16 {
        let shift = (15 - index) * 4;
        out[index + 2] = HEX[((value >> shift) & 0xf) as usize];
    }
    console_write_str::<P>(core::str::from_utf8(&out).unwrap_or("0x?"));
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
