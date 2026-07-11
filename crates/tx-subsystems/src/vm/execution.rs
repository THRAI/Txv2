//! VM mutating and step-like execution entrypoints.
//!
//! These methods consume or create structure-side evidence, reserve declared
//! ranges, commit recipe changes, and publish pmap materializations. The
//! broader syscall scripts still live outside the VM subsystem.

use alloc::collections::BTreeSet;
use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use tx_hal::PmapIf;

use crate::execution::Guard;
use crate::execution::WaitToken;
use crate::page_backed::{step_fsync, PageContainerKind};
use crate::vm::adapter::step_engine::{
    self as step_engine, AbortReason, AgentCancelPolicy, DelegateRegistry, DelegateReply,
    DelegateRequest, StepOutcome, StepOutcome as V3StepOutcome, TaskMailbox, TokenDropPolicy,
    UfdAccessKind, UfdReply, UfdRequest,
};
use crate::vm::checks::{
    require_fault_publication, require_fault_recipe, require_map_admission, require_remap_shape,
};
use crate::vm::pmap::PmapBatchPage;
use crate::vm::structure::{PrivatePageError, PrivatePageSet};
use crate::vm::{
    AccessMode, AddressSpace, LockMode, MapPlacement, PmapPublishOutcome, Prot, RangeGuard,
    UserRange, UserVirtAddr, VmBacking, VmEntry, VmEntryBacking, VmFault, VmFaultError,
    VmFaultMaterialization, VmFaultMaterializationStep, VmFaultOutcome, VmMapCommit, VmMapError,
    VmMapOutcome, VmMapRequest, VmMapTarget, VmRemapOutcome, VmRemapPlacement, VmRemapRequest,
    USER_PAGE_SIZE,
};

const PRIVATE_ANON_FAULT_BATCH_PAGES: usize = 16;
const PRIVATE_ANON_FAULT_BATCH_MIN_VMA_BYTES: usize = USER_PAGE_SIZE * 2;

pub fn page_align_up(addr: usize) -> usize {
    checked_page_align_up(addr).expect("page_align_up overflow")
}

pub fn checked_page_align_up(addr: usize) -> Option<usize> {
    addr.checked_next_multiple_of(USER_PAGE_SIZE)
}

impl AddressSpace {
    pub fn resolve_fault(&self, fault: VmFault) -> Result<VmFaultOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        let V3StepOutcome::Done(_guard) = self
            .range_lock
            .acquire_step(page_range, LockMode::Materializer)
        else {
            return Err(VmFaultError::WouldBlock);
        };

        require_fault_recipe(self, fault)
    }

    pub fn publish_fault_materialization(
        &self,
        outcome: VmFaultOutcome,
        materialization: VmFaultMaterialization,
    ) -> Result<PmapPublishOutcome, VmFaultError> {
        let V3StepOutcome::Done(_guard) = self
            .range_lock
            .acquire_step(outcome.page_range, LockMode::Materializer)
        else {
            return Err(VmFaultError::WouldBlock);
        };

        require_fault_publication(self, &outcome, &materialization)?;

        self.pmap
            .publish_page_with_replacement(
                outcome.page_range.start().containing_page(),
                materialization.page.ppn,
                materialization.publish_prot,
                materialization.page.map_pin,
                materialization.replace_existing,
            )
            .map_err(VmFaultError::Pmap)
    }

    /// Duplicate `parent`'s recipes into a fresh child `AddressSpace` and
    /// demote `parent`'s MAP_PRIVATE PTEs so subsequent writes refault and
    /// CoW.
    ///
    /// VM_v1_2 §5.6. The child is constructed via the same platform pmap
    /// type as `parent`. The child recipe index starts as an O(1)
    /// structurally shared clone of the parent's published root. Private
    /// CoW entries with resident private sets are then path-copied in the
    /// child only, so the child can carry sibling `SharedCow` sets while
    /// unchanged recipes continue sharing tree nodes. For every MAP_PRIVATE
    /// entry, the parent's pmap range is demoted read-only so the next write
    /// on either side refaults and the existing `materialize_page_recipe`
    /// CoW path produces a private frame for the writer. MAP_SHARED entries
    /// leave the parent's PTEs intact; the child's pmap starts empty and
    /// rebuilds via refault.
    ///
    /// Per VM_v1_2 §9.5, fork acquires an `ExclusiveWriter` reservation on
    /// the full user range (`UserRange::full_user_v1()`) so no concurrent
    /// VM operation runs against `parent` for the duration of the call.
    /// V1 fork is intentionally serialized end to end with respect to
    /// every parent VM operation; range-by-range pmap walks are a
    /// potential v2 optimization.
    ///
    /// Returns `VmMapError::WouldBlock` if a concurrent operation already
    /// holds an overlapping reservation. Callers may retry; no async
    /// retry is built in at the VM layer because the Process subsystem
    /// orchestrates fork above this function.
    pub fn fork_aspace<P: PmapIf>(parent: &AddressSpace) -> Result<AddressSpace, VmMapError> {
        let V3StepOutcome::Done(_full_guard) = parent
            .range_lock
            .acquire_step(UserRange::full_user_v1(), LockMode::ExclusiveWriter)
        else {
            return Err(VmMapError::WouldBlock);
        };

        let guard = step_engine::guard();
        let parent_recipes = parent.recipes_snapshot();
        let child_recipes = parent.recipes.clone_shared(&guard);
        let child = AddressSpace::new_with_recipes_for_platform::<P>(child_recipes)?;

        for entry in parent_recipes {
            let private = !entry.flags.shared;
            let range = entry.range;
            if let (true, Some(parent_set)) = (private, entry.private()) {
                let child_set = parent_set.fork_share().map_err(VmMapError::Private)?;
                let child_entry = entry.clone().with_private(Some(child_set));
                child.recipes.replace_entry(child_entry)?;
            }
            if private {
                parent
                    .pmap
                    .protect_range(range, entry.prot.without_write())?;
            }
        }

        child.stats.store(child.recipes.stats(&guard));
        Ok(child)
    }

    /// Reset `old_aspace` for exec by tearing down every materialized PTE
    /// across all current recipes.
    ///
    /// VM_v1_2 §5.7. Caller is responsible for replacing the recipes (and
    /// for choosing whether to drop and recreate the `AddressSpace` or
    /// reuse the same instance with new content). This function only
    /// performs the pmap teardown step; recipe management belongs to the
    /// caller because the new image's recipe shape is determined by the
    /// exec image loader, which is Process-side and not yet implemented.
    /// V1 callers typically follow with `aspace.recipes` modifications
    /// that withdraw old entries and install the new image's mappings, or
    /// drop `old_aspace` entirely and use a fresh AddressSpace.
    ///
    /// Caller invariant (per VM §5.7 prologue): exec has already reduced
    /// the thread group to one and no concurrent VM operations exist on
    /// `old_aspace`.
    ///
    /// Returns the count of pages torn down for observability.
    pub fn exec_aspace(old_aspace: &AddressSpace) -> usize {
        old_aspace.teardown_all_pmap()
    }

    /// Async wrapper around `resolve_fault` + `materialize_pagebacked` +
    /// `publish_fault_materialization` that yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel.
    ///
    /// VM_v1_2 §5.1 + §3.6 cross-async-wait discipline. Each iteration:
    ///
    /// 1. Acquires a Materializer reservation, observes the recipe, and
    ///    drops the reservation. On WouldBlock the future awaits on the
    ///    `RangeLock`'s wait channel and retries.
    /// 2. Materializes the page through `materialize_pagebacked`. PC-side
    ///    Blocked outcomes (File-variant `step_fsync` / FsPageBacking)
    ///    are not yet exposed through this script — they remain a
    ///    follow-up that requires per-`PageContainer` wait channels.
    /// 3. Re-acquires the Materializer reservation and publishes the
    ///    materialization through the pmap. WouldBlock here drops the
    ///    materialization (releasing the MapPin) and retries from
    ///    step 1, re-observing the recipe afresh.
    pub async fn fault_script(&self, fault: VmFault) -> Result<PmapPublishOutcome, VmFaultError> {
        // The OnAgent dispatcher slot is empty for the simple
        // entrypoint — callers that want userfaultfd-aware fault
        // resolution use [`Self::fault_script_for_process`] (PR-10
        // phase 6 production entrypoint, walks the calling process's
        // fd-table) or [`Self::fault_script_with_ufd_dispatch`] (test
        // / non-process callers, supply their own `UfdDispatch`).
        //
        // This entrypoint preserves the pre-PR-10 fault-only behaviour
        // for callers that never need OnAgent (e.g. kernel-internal
        // fault scripts that have no userspace process context).
        self.fault_script_with_ufd_dispatch(fault, NullUfdDispatch)
            .await
    }

    /// PR-10 phase 6 — production fault-script entrypoint.
    ///
    /// Builds a [`crate::userfaultfd::ProcessUfdDispatch`] from the
    /// supplied process cap and faulting thread's `Weak<TaskMailbox>`,
    /// then drives the canonical async fault script through
    /// [`Self::fault_script_with_ufd_dispatch`]. The dispatcher walks
    /// `process.fd_table` looking for a matching `Cap<UserfaultFd>`
    /// (linear scan — fine for the canary; an indexed map shortcut
    /// is a follow-up if production profiling shows it). On a hit the
    /// fault is routed through the OnAgent branch (install request,
    /// push fault message, await reply via `await_agent_reply`); on a
    /// miss (or no ufd registered against the faulting VMA) the
    /// fault-script falls through to the normal materialize path
    /// unchanged.
    ///
    /// This is the entrypoint `thread_future` will call once the
    /// production thread loop wires the per-thread mailbox handle
    /// through to the page-fault dispatch. The phase-6 e2e canary
    /// test exercises this entrypoint end-to-end without bootstrapping
    /// the trap shell.
    pub async fn fault_script_for_process(
        &self,
        fault: VmFault,
        process: &step_engine::Cap<crate::process::ProcessIdentity>,
        mailbox: Weak<TaskMailbox>,
    ) -> Result<PmapPublishOutcome, VmFaultError> {
        let dispatch = crate::userfaultfd::ProcessUfdDispatch::new(process, mailbox);
        self.fault_script_with_ufd_dispatch(fault, dispatch).await
    }

    /// PR-10 phase 4 — userfaultfd-aware fault script.
    ///
    /// Identical to [`Self::fault_script`] except that when the
    /// resolved VMA carries a `ufd_registration` tag (per phase 3),
    /// the script installs a `DelegateRequest::Ufd(UfdRequest::PageFault)`
    /// into the per-ufd [`DelegateRegistry`] reachable through
    /// `dispatch`, yields `OnAgent` semantically (the substrate
    /// `await_agent_reply` helper drives the same mailbox plumbing),
    /// and resumes once the agent thread drives `mark_replied` via
    /// `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` (phase 5).
    ///
    /// **Phase 4 stub semantics.** Actual page-copy from
    /// `src_kernel_addr` → `dst_uaddr` is deferred to phase 5; phase 4
    /// lands the interception + delegation + reply consumption. The
    /// `UfdReply` payload is acknowledged as "applied" — on
    /// `UfdReply::ZeroPage` the existing private-anon materialization
    /// path runs (a zero page is correct semantics); on `UfdReply::Copy`
    /// / `UfdReply::Continue` the materialize path is skipped because
    /// the agent is supposed to have already installed the contents
    /// (the actual `src` → `dst` byte move lands in phase 5).
    ///
    /// On `AbortReason::AgentDied` / `Canceled` / `TimedOut` the fault
    /// returns `VmFaultError::WouldBlock` — the trap dispatcher in
    /// `thread_future` will see that and route SIGSEGV/SIGBUS per
    /// `SIGNAL_v1` §15.1. (PR-10 phase 6 reviews whether a dedicated
    /// `VmFaultError::AgentDied` variant should land — for phase 4 the
    /// existing WouldBlock path is sufficient.)
    pub async fn fault_script_with_ufd_dispatch<D: UfdDispatch>(
        &self,
        fault: VmFault,
        dispatch: D,
    ) -> Result<PmapPublishOutcome, VmFaultError> {
        let page_range = UserRange::containing_page(fault.addr).map_err(VmFaultError::Range)?;
        loop {
            let outcome = match self.try_fault_script_resolve(page_range, fault)? {
                FaultScriptResolve::Done(outcome) => outcome,
                FaultScriptResolve::Wait(token) => {
                    await_range_lock(token).await;
                    continue;
                }
            };

            // PR-10 phase 4 OnAgent branch. Read the per-VMA
            // `ufd_registration` tag (phase 3) at the same point W-V's
            // catchup pinned: at the require_fault_recipe result,
            // before any materialize_pagebacked call. Single-branch
            // insertion in the existing async loop.
            //
            // PR-10 phase 6 byte-move: the OnAgent reply variant
            // selects the materialize lane. `UfdReply::Copy` allocates
            // a fresh private page and memcpy's `len` bytes from the
            // agent's `src_kernel_addr`; `UfdReply::ZeroPage` falls
            // through to the canonical `materialize_pagebacked` lane
            // (zero-frame is the correct content); `UfdReply::Continue`
            // is out of scope for phase 6 (only MISSING mode is
            // implemented). A dispatcher miss / no-tag falls through
            // to the normal materialize path (graceful degradation).
            let ufd_reply = if let Some(tag) = outcome.entry.ufd_registration {
                dispatch_ufd_fault(&dispatch, tag.ufd_id, fault).await?
            } else {
                None
            };

            emit_vm_trace(b"debug.vm.fault.script.phase", 0);
            match self.try_fault_script_materialize_and_publish(&outcome, ufd_reply)? {
                FaultScriptPublish::Done(published) => {
                    emit_vm_trace(b"debug.vm.fault.script.phase", 1);
                    self.prefault_private_anon_write_batch(&outcome);
                    emit_vm_trace(b"debug.vm.fault.script.phase", 2);
                    return Ok(published);
                }
                FaultScriptPublish::Wait(token) => {
                    emit_vm_trace(b"debug.vm.fault.script.wait", 1);
                    await_range_lock(token).await;
                    continue;
                }
            }
        }
    }

    fn try_fault_script_resolve(
        &self,
        page_range: UserRange,
        fault: VmFault,
    ) -> Result<FaultScriptResolve, VmFaultError> {
        emit_vm_trace(b"debug.vm.fault.resolve.phase", 0);
        let _guard = match self
            .range_lock
            .acquire_step(page_range, LockMode::Materializer)
        {
            V3StepOutcome::Done(guard) => {
                emit_vm_trace(b"debug.vm.fault.resolve.phase", 1);
                guard
            }
            V3StepOutcome::Yield { shape, .. } => {
                if let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) {
                    emit_vm_trace(b"debug.vm.fault.resolve.wait", 1);
                    return Ok(FaultScriptResolve::Wait(token));
                }
                unreachable_acquire_step()
            }
            _ => unreachable_acquire_step(),
        };
        let outcome = require_fault_recipe(self, fault)?;
        emit_vm_trace(
            b"debug.vm.fault.resolve.access",
            vm_fault_access_trace_id(outcome.access),
        );
        emit_vm_trace(
            b"debug.vm.fault.resolve.backing",
            vm_backing_trace_id(outcome.entry.backing_kind()),
        );
        emit_vm_trace(b"debug.vm.fault.resolve.phase", 2);
        Ok(FaultScriptResolve::Done(outcome))
    }

    fn try_fault_script_publish(
        &self,
        outcome: &VmFaultOutcome,
        materialization: VmFaultMaterialization,
    ) -> Result<FaultScriptPublish, VmFaultError> {
        emit_vm_trace(b"debug.vm.fault.publish.phase", 0);
        let _guard = match self
            .range_lock
            .acquire_step(outcome.page_range, LockMode::Materializer)
        {
            V3StepOutcome::Done(guard) => {
                emit_vm_trace(b"debug.vm.fault.publish.phase", 1);
                guard
            }
            V3StepOutcome::Yield { shape, .. } => {
                if let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) {
                    drop(materialization);
                    emit_vm_trace(b"debug.vm.fault.publish.wait", 1);
                    return Ok(FaultScriptPublish::Wait(token));
                }
                unreachable_acquire_step()
            }
            _ => unreachable_acquire_step(),
        };
        emit_vm_trace(b"debug.vm.fault.publish.phase", 2);
        emit_vm_trace(
            b"debug.vm.fault.publish.pmap_mapped_pages",
            self.pmap.stats().mapped_pages as i64,
        );
        emit_vm_trace(
            b"debug.vm.fault.publish.private_len",
            outcome.entry.private().map(|set| set.len()).unwrap_or(0) as i64,
        );
        let _entry = require_fault_publication(self, outcome, &materialization)?;
        emit_vm_trace(b"debug.vm.fault.publish.phase", 3);
        let published = self
            .pmap
            .publish_page_with_replacement(
                outcome.page_range.start().containing_page(),
                materialization.page.ppn,
                materialization.publish_prot,
                materialization.page.map_pin,
                materialization.replace_existing,
            )
            .map_err(VmFaultError::Pmap)?;
        emit_vm_trace(b"debug.vm.fault.publish.phase", 4);
        Ok(FaultScriptPublish::Done(published))
    }

    fn try_fault_script_materialize_and_publish(
        &self,
        outcome: &VmFaultOutcome,
        ufd_reply: Option<DelegateReply>,
    ) -> Result<FaultScriptPublish, VmFaultError> {
        emit_vm_trace(b"debug.vm.fault.materialize.phase", 0);
        let materialization = match ufd_reply {
            Some(DelegateReply::Ufd(UfdReply::Copy {
                src_kernel_addr,
                dst_uaddr: _,
                len,
            })) => {
                emit_vm_trace(b"debug.vm.fault.materialize.ufd_copy", 1);
                materialize_ufd_copy(outcome, src_kernel_addr, len)?
            }
            Some(DelegateReply::Ufd(UfdReply::ZeroPage { .. })) | None => {
                let guard = step_engine::guard();
                emit_vm_trace(b"debug.vm.fault.materialize.phase", 1);
                match outcome.materialize_pagebacked_step(&guard) {
                    VmFaultMaterializationStep::Done(materialization) => {
                        emit_vm_trace(b"debug.vm.fault.materialize.phase", 2);
                        materialization
                    }
                    VmFaultMaterializationStep::Blocked(token) => {
                        drop(guard);
                        emit_vm_trace(b"debug.vm.fault.materialize.wait", 1);
                        return Ok(FaultScriptPublish::Wait(token));
                    }
                    VmFaultMaterializationStep::Err(error) => {
                        emit_vm_trace(b"debug.vm.fault.materialize.err", 1);
                        return Err(error);
                    }
                }
            }
            Some(DelegateReply::Ufd(UfdReply::Continue { .. })) => {
                // `UFFDIO_CONTINUE` is the MINOR-mode path (ufd-shm
                // / page-cache shared mappings). Phase 3 only
                // admitted `UFFDIO_REGISTER_MODE_MISSING`, so this
                // variant should never appear in PR-10's wired
                // surface. Reject with `WouldBlock` rather than
                // pretending to install; the trap dispatcher will
                // surface SIGBUS per the agent-dropped-reply mapping.
                return Err(VmFaultError::WouldBlock);
            }
        };
        emit_vm_trace(b"debug.vm.fault.materialize.phase", 3);
        self.try_fault_script_publish(outcome, materialization)
    }

    fn prefault_private_anon_write_batch(&self, first: &VmFaultOutcome) {
        if !private_anon_write_batch_shape(first) {
            return;
        }
        let current_page = first.page_range.start().containing_page().0;
        let next_expected = current_page.saturating_add(1);
        let previous_expected = self
            .next_private_anon_write_fault_page
            .swap(next_expected, Ordering::Relaxed);
        if previous_expected != current_page {
            return;
        }

        let Some(batch_bytes) = PRIVATE_ANON_FAULT_BATCH_PAGES.checked_mul(USER_PAGE_SIZE) else {
            return;
        };
        let mut addr = first.page_range.end().0;
        let batch_end = first.page_range.start().0.saturating_add(batch_bytes);
        let end = core::cmp::min(first.entry.range.end().0, batch_end);
        emit_vm_trace(b"debug.vm.fault.prefault.begin", current_page as i64);
        emit_vm_trace(
            b"debug.vm.fault.prefault.limit_pages",
            ((end.saturating_sub(addr)) / USER_PAGE_SIZE) as i64,
        );

        let Some(batch_range) =
            UserRange::new_aligned(UserVirtAddr(addr), end.saturating_sub(addr)).ok()
        else {
            return;
        };
        if batch_range.is_empty() {
            return;
        }
        let _batch_guard = match self
            .range_lock
            .acquire_step(batch_range, LockMode::Materializer)
        {
            V3StepOutcome::Done(guard) => guard,
            V3StepOutcome::Yield { .. } => {
                emit_vm_trace(b"debug.vm.fault.prefault.published_pages", 0);
                return;
            }
            _ => unreachable_acquire_step(),
        };
        emit_vm_trace(b"debug.vm.fault.prefault.phase", 0);

        let mut batch_pages = Vec::new();
        while addr < end {
            let page_addr = UserVirtAddr(addr);
            emit_vm_trace(b"debug.vm.fault.prefault.phase", 1);
            if self.pmap.lookup(page_addr.containing_page()).is_some() {
                addr = match addr.checked_add(USER_PAGE_SIZE) {
                    Some(next) => next,
                    None => break,
                };
                continue;
            }

            let page_range = match UserRange::new_aligned(page_addr, USER_PAGE_SIZE) {
                Ok(range) => range,
                Err(_) => break,
            };
            let fault = VmFault::new(page_addr, first.access);
            emit_vm_trace(b"debug.vm.fault.prefault.phase", 2);
            let outcome = match require_fault_recipe(self, fault) {
                Ok(outcome) if outcome.page_range == page_range => outcome,
                _ => break,
            };
            if !private_anon_write_batch_shape(&outcome) {
                break;
            }

            let guard = step_engine::guard();
            emit_vm_trace(b"debug.vm.fault.prefault.phase", 3);
            let materialization = match outcome.materialize_pagebacked_step(&guard) {
                VmFaultMaterializationStep::Done(materialization) => materialization,
                VmFaultMaterializationStep::Blocked(_) | VmFaultMaterializationStep::Err(_) => {
                    break;
                }
            };
            drop(guard);
            emit_vm_trace(b"debug.vm.fault.prefault.phase", 4);
            batch_pages.push(PmapBatchPage {
                page: outcome.page_range.start().containing_page(),
                ppn: materialization.page.ppn,
                prot: materialization.publish_prot,
                map_pin: materialization.page.map_pin,
            });
            emit_vm_trace(b"debug.vm.fault.prefault.phase", 5);

            addr = match addr.checked_add(USER_PAGE_SIZE) {
                Some(next) => next,
                None => break,
            };
        }
        emit_vm_trace(b"debug.vm.fault.prefault.phase", 6);
        let published_pages = self.pmap.publish_new_pages_best_effort(batch_pages);
        emit_vm_trace(b"debug.vm.fault.prefault.phase", 7);
        if published_pages > 0 {
            let next = current_page
                .saturating_add(1)
                .saturating_add(published_pages);
            self.next_private_anon_write_fault_page
                .store(next, Ordering::Relaxed);
        }
        emit_vm_trace(
            b"debug.vm.fault.prefault.published_pages",
            published_pages as i64,
        );
    }

    pub fn try_mmap(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
        let total_start = vm_map_path_clock_now();
        emit_vm_trace(b"debug.vm.mmap.enter", 0);
        let (range, placement) = match request.target {
            VmMapTarget::Anywhere { window, page_count } => {
                emit_vm_trace(b"debug.vm.mmap.target", 0);
                emit_vm_trace(b"debug.vm.mmap.pages", page_count as i64);
                let search_start = vm_map_path_clock_now();
                let range = self
                    .find_free_range_for_anywhere(window, page_count)
                    .ok_or(VmMapError::NoFreeRange)?;
                emit_vm_map_path_duration(
                    b"debug.vm.map_path.mmap.anywhere_search_ns",
                    search_start,
                );
                (range, MapPlacement::RequireFree)
            }
            VmMapTarget::Fixed { range, placement } => {
                emit_vm_trace(b"debug.vm.mmap.target", 1);
                emit_vm_trace(b"debug.vm.mmap.pages", range.page_count() as i64);
                (range, placement)
            }
        };
        emit_vm_trace(
            b"debug.vm.mmap.range_start",
            range.start().as_usize() as i64,
        );
        emit_vm_trace(b"debug.vm.mmap.placement", placement as i64);
        let entry = VmEntry::new(range, request.prot, request.flags, request.backing);

        let reserve_start = vm_map_path_clock_now();
        match self.reserve_map(entry, placement) {
            MapReserveResult::Reserved(reservation) => {
                emit_vm_map_path_duration(b"debug.vm.map_path.mmap.reserve_map_ns", reserve_start);
                emit_vm_trace(b"debug.vm.mmap.reserved", 1);
                let commit_start = vm_map_path_clock_now();
                let commit = reservation.commit()?;
                emit_vm_map_path_duration(b"debug.vm.map_path.mmap.commit_ns", commit_start);
                emit_vm_trace(
                    b"debug.vm.mmap.committed_pages",
                    commit.changed_pages as i64,
                );
                if matches!(request.target, VmMapTarget::Anywhere { .. }) {
                    self.next_mmap_search_start
                        .store(range.end().as_usize(), Ordering::Relaxed);
                }
                emit_vm_map_path_count(
                    b"debug.vm.map_path.mmap.changed_pages",
                    commit.changed_pages as u64,
                );
                emit_vm_map_path_duration(b"debug.vm.map_path.mmap.total_ns", total_start);
                Ok(VmMapOutcome { range, commit })
            }
            MapReserveResult::Blocked(_) => {
                emit_vm_map_path_duration(
                    b"debug.vm.map_path.mmap.reserve_blocked_ns",
                    reserve_start,
                );
                emit_vm_map_path_duration(b"debug.vm.map_path.mmap.total_ns", total_start);
                Err(VmMapError::WouldBlock)
            }
            MapReserveResult::Err(error) => {
                emit_vm_map_path_duration(
                    b"debug.vm.map_path.mmap.reserve_error_ns",
                    reserve_start,
                );
                emit_vm_map_path_duration(b"debug.vm.map_path.mmap.total_ns", total_start);
                Err(error)
            }
        }
    }

    fn find_free_range_for_anywhere(
        &self,
        window: UserRange,
        page_count: usize,
    ) -> Option<UserRange> {
        let len = page_count.checked_mul(USER_PAGE_SIZE)?;
        let hint = self.next_mmap_search_start.load(Ordering::Relaxed);
        if hint > window.start().as_usize()
            && hint < window.end().as_usize()
            && hint.is_multiple_of(USER_PAGE_SIZE)
            && hint.checked_add(len)? <= window.end().as_usize()
        {
            let hinted_window = UserRange::new_aligned(
                UserVirtAddr(hint),
                window.end().as_usize().checked_sub(hint)?,
            )
            .ok()?;
            if let Some(range) = self.find_free_range(hinted_window, page_count) {
                return Some(range);
            }
        }
        self.find_free_range(window, page_count)
    }

    /// Canonical async mmap script per VM_v1_2 §5.2. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel. Honors VM_v1_2 §3.6 cross-async-wait discipline by dropping
    /// every reservation and observation before each `.await`.
    pub async fn mmap_script(&self, request: VmMapRequest) -> Result<VmMapOutcome, VmMapError> {
        loop {
            let (range, placement) = match request.target {
                VmMapTarget::Anywhere { window, page_count } => {
                    let range = self
                        .find_free_range(window, page_count)
                        .ok_or(VmMapError::NoFreeRange)?;
                    (range, MapPlacement::RequireFree)
                }
                VmMapTarget::Fixed { range, placement } => (range, placement),
            };
            let entry = VmEntry::new(range, request.prot, request.flags, request.backing.clone());

            match self.reserve_map(entry, placement) {
                MapReserveResult::Reserved(reservation) => {
                    let commit = reservation.commit()?;
                    return Ok(VmMapOutcome { range, commit });
                }
                MapReserveResult::Blocked(token) => {
                    await_range_lock(token).await;
                    // continue loop to retry
                }
                MapReserveResult::Err(error) => return Err(error),
            }
        }
    }

    /// Canonical async munmap script per VM_v1_2 §5.3. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait
    /// channel. Honors VM_v1_2 §3.6 by dropping the blocked guard before
    /// each `.await`.
    pub async fn munmap_script(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self
                .range_lock
                .acquire_step(range, LockMode::ExclusiveWriter)
            {
                V3StepOutcome::Done(guard) => guard,
                V3StepOutcome::Yield { shape, .. } => {
                    let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) else {
                        unreachable_acquire_step();
                    };
                    await_range_lock(token).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
            };
            let commit = self.recipes.unmap(range)?;
            self.pmap.teardown_range(range)?;
            self.stats.apply_delta(commit.stats_delta);
            return Ok(commit);
        }
    }

    /// Canonical async mprotect script per VM_v1_2 §5.4. Yields on `RangeLock`
    /// `WouldBlock` and retries after a release wakes the lock's wait channel.
    pub async fn mprotect_script(
        &self,
        range: UserRange,
        prot: Prot,
    ) -> Result<VmMapCommit, VmMapError> {
        loop {
            let _guard = match self
                .range_lock
                .acquire_step(range, LockMode::ExclusiveWriter)
            {
                V3StepOutcome::Done(guard) => guard,
                V3StepOutcome::Yield { shape, .. } => {
                    let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) else {
                        unreachable_acquire_step();
                    };
                    await_range_lock(token).await;
                    continue;
                }
                _ => unreachable_acquire_step(),
            };
            let commit = self.recipes.protect(range, prot)?;
            self.pmap.teardown_range(range)?;
            self.stats.apply_delta(commit.stats_delta);
            return Ok(commit);
        }
    }

    /// Canonical async mremap script per VM_v1_2 §5.5. Yields on `RangeLock`
    /// pair `WouldBlock` and retries after either covered range's release
    /// wakes the lock's wait channel.
    pub async fn mremap_script(
        &self,
        request: VmRemapRequest,
    ) -> Result<VmRemapOutcome, VmMapError> {
        require_remap_shape(request.old_range, request.new_range, request.placement)?;
        loop {
            let _guard_pair;
            let _guard_single;
            match request.placement {
                VmRemapPlacement::Move => {
                    _guard_pair = match self.range_lock.acquire_pair_step(
                        (request.old_range, LockMode::ExclusiveWriter),
                        (request.new_range, LockMode::ExclusiveWriter),
                    ) {
                        V3StepOutcome::Done(pair) => pair,
                        V3StepOutcome::Yield { shape, .. } => {
                            let Some(token) =
                                crate::vm::notification::wait_token_from_shape(&shape)
                            else {
                                unreachable_acquire_step();
                            };
                            await_range_lock(token).await;
                            continue;
                        }
                        _ => unreachable_acquire_step(),
                    };
                }
                VmRemapPlacement::InPlace => {
                    let lock_range = remap_union_range(request.old_range, request.new_range)?;
                    _guard_single = match self
                        .range_lock
                        .acquire_step(lock_range, LockMode::ExclusiveWriter)
                    {
                        V3StepOutcome::Done(guard) => guard,
                        V3StepOutcome::Yield { shape, .. } => {
                            let Some(token) =
                                crate::vm::notification::wait_token_from_shape(&shape)
                            else {
                                unreachable_acquire_step();
                            };
                            await_range_lock(token).await;
                            continue;
                        }
                        _ => unreachable_acquire_step(),
                    };
                }
            }
            let commit = self.recipes.remap(
                request.old_range,
                request.new_range,
                request.placement,
                request.destination,
            )?;
            for teardown_range in
                remap_teardown_ranges(request.old_range, request.new_range, request.placement)?
            {
                self.pmap.teardown_range(teardown_range)?;
            }
            self.stats.apply_delta(commit.stats_delta);
            return Ok(VmRemapOutcome {
                old_range: request.old_range,
                new_range: request.new_range,
                commit,
            });
        }
    }

    /// Async brk script: grow or shrink the program break of an Anon
    /// mapping anchored at `brk_base`.
    ///
    /// VM_v1_2 §5.8. The current implementation models brk as a single
    /// `VmBacking::PrivateAnon` mapping whose extent is
    /// `[brk_base, current_brk)`. The script:
    ///
    /// - returns `Ok(current_brk)` when `requested_brk == current_brk`.
    /// - rejects with `InvalidRange` when `requested_brk < brk_base`.
    /// - on grow, calls `mmap_script` to map the page-aligned range
    ///   `[current_brk, requested_brk)`.
    /// - on shrink, calls `munmap_script` on `[requested_brk, current_brk)`.
    ///
    /// All three of `brk_base`, `current_brk`, and `requested_brk` must be
    /// page-aligned. Process-level tracking of the brk value (which hart
    /// holds it, exec-time base, fork inheritance) lives in the Process
    /// subsystem and is out of scope for VM.
    pub fn try_brk(
        &self,
        brk_base: crate::vm::UserVirtAddr,
        current_brk: crate::vm::UserVirtAddr,
        requested_brk: crate::vm::UserVirtAddr,
    ) -> Result<crate::vm::UserVirtAddr, VmMapError> {
        if requested_brk.0 < brk_base.0 {
            return Err(VmMapError::InvalidRange);
        }
        if requested_brk.0 == current_brk.0 {
            return Ok(current_brk);
        }
        if requested_brk.0 > current_brk.0 {
            let old_committed =
                checked_page_align_up(current_brk.0).ok_or(VmMapError::InvalidRange)?;
            let new_committed =
                checked_page_align_up(requested_brk.0).ok_or(VmMapError::InvalidRange)?;
            if new_committed > old_committed {
                let range = UserRange::new_aligned(
                    UserVirtAddr(old_committed),
                    new_committed - old_committed,
                )
                .map_err(|_| VmMapError::InvalidRange)?;
                let request = VmMapRequest::fixed(
                    range,
                    MapPlacement::RequireFree,
                    Prot::READ_WRITE,
                    crate::vm::VmEntryFlags::PRIVATE,
                    VmBacking::PrivateAnon,
                );
                self.try_mmap(request)?;
            }
        } else {
            let old_committed =
                checked_page_align_up(current_brk.0).ok_or(VmMapError::InvalidRange)?;
            let new_committed =
                checked_page_align_up(requested_brk.0).ok_or(VmMapError::InvalidRange)?;
            if new_committed < old_committed {
                let range = UserRange::new_aligned(
                    UserVirtAddr(new_committed),
                    old_committed - new_committed,
                )
                .map_err(|_| VmMapError::InvalidRange)?;
                self.try_munmap(range)?;
            }
        }
        Ok(requested_brk)
    }

    pub async fn brk_script(
        &self,
        brk_base: crate::vm::UserVirtAddr,
        current_brk: crate::vm::UserVirtAddr,
        requested_brk: crate::vm::UserVirtAddr,
    ) -> Result<crate::vm::UserVirtAddr, VmMapError> {
        loop {
            match self.try_brk(brk_base, current_brk, requested_brk) {
                Ok(new_brk) => return Ok(new_brk),
                Err(VmMapError::WouldBlock) => {
                    let token = WaitToken::new(
                        self.range_lock.wait_source_id(),
                        crate::vm::RANGE_LOCK_RELEASE_MASK,
                    );
                    await_range_lock(token).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn try_mremap(&self, request: VmRemapRequest) -> Result<VmRemapOutcome, VmMapError> {
        require_remap_shape(request.old_range, request.new_range, request.placement)?;

        let _guard_pair;
        let _guard_single;
        match request.placement {
            VmRemapPlacement::Move => {
                _guard_pair = match self.range_lock.acquire_pair_step(
                    (request.old_range, LockMode::ExclusiveWriter),
                    (request.new_range, LockMode::ExclusiveWriter),
                ) {
                    V3StepOutcome::Done(pair) => pair,
                    V3StepOutcome::Yield { .. } => return Err(VmMapError::WouldBlock),
                    _ => unreachable_acquire_step(),
                };
            }
            VmRemapPlacement::InPlace => {
                let lock_range = remap_union_range(request.old_range, request.new_range)?;
                _guard_single = match self
                    .range_lock
                    .acquire_step(lock_range, LockMode::ExclusiveWriter)
                {
                    V3StepOutcome::Done(guard) => guard,
                    V3StepOutcome::Yield { .. } => return Err(VmMapError::WouldBlock),
                    _ => unreachable_acquire_step(),
                };
            }
        };
        let commit = self.recipes.remap(
            request.old_range,
            request.new_range,
            request.placement,
            request.destination,
        )?;
        for teardown_range in
            remap_teardown_ranges(request.old_range, request.new_range, request.placement)?
        {
            self.pmap.teardown_range(teardown_range)?;
        }
        self.stats.apply_delta(commit.stats_delta);
        Ok(VmRemapOutcome {
            old_range: request.old_range,
            new_range: request.new_range,
            commit,
        })
    }

    pub fn reserve_map(&self, entry: VmEntry, placement: MapPlacement) -> MapReserveResult<'_> {
        let guard = match self
            .range_lock
            .acquire_step(entry.range, LockMode::ExclusiveWriter)
        {
            V3StepOutcome::Done(guard) => guard,
            V3StepOutcome::Yield { shape, .. } => {
                let Some(token) = crate::vm::notification::wait_token_from_shape(&shape) else {
                    unreachable_acquire_step();
                };
                return MapReserveResult::Blocked(token);
            }
            _ => unreachable_acquire_step(),
        };

        if let Err(error) = require_map_admission(self, &entry, placement) {
            return MapReserveResult::Err(error);
        }

        // Auto-attach a fresh `PrivatePageSet` to private mappings that
        // didn't already provide one (the call paths that don't go
        // through `build_mmap_entry`, e.g. `register_recipe` in
        // `vm::scripts`). Without this, private write faults would
        // allocate pages straight into the pmap but skip the per-VmEntry
        // CoW store — fork would then have nothing to share.
        let entry = if !entry.flags.shared && entry.prot.write && entry.private().is_none() {
            match PrivatePageSet::new_cap() {
                Ok(set) => entry.with_private(Some(set)),
                Err(e) => {
                    return MapReserveResult::Err(VmMapError::Private(PrivatePageError::Zone(e)));
                }
            }
        } else {
            entry
        };

        MapReserveResult::Reserved(MapReservation {
            aspace: self,
            entry,
            placement,
            _guard: guard,
        })
    }

    pub fn try_munmap(&self, range: UserRange) -> Result<VmMapCommit, VmMapError> {
        let total_start = vm_map_path_clock_now();
        emit_vm_trace(b"debug.vm.unmap.phase", 0);
        let acquire_start = vm_map_path_clock_now();
        let _guard = self.acquire_writer(range)?;
        emit_vm_map_path_duration(b"debug.vm.map_path.munmap.acquire_ns", acquire_start);
        emit_vm_trace(b"debug.vm.unmap.phase", 1);
        let recipe_start = vm_map_path_clock_now();
        let commit = self.recipes.unmap(range)?;
        emit_vm_map_path_duration(b"debug.vm.map_path.munmap.recipe_ns", recipe_start);
        emit_vm_trace(b"debug.vm.unmap.phase", 2);
        emit_vm_trace(b"debug.vm.unmap.changed_pages", commit.changed_pages as i64);
        let pmap_start = vm_map_path_clock_now();
        let pmap_removed = self.pmap.teardown_range(range)?;
        emit_vm_map_path_duration(b"debug.vm.map_path.munmap.pmap_teardown_ns", pmap_start);
        emit_vm_trace(b"debug.vm.unmap.phase", 3);
        emit_vm_trace(b"debug.vm.unmap.pmap_removed", pmap_removed as i64);
        let stats_start = vm_map_path_clock_now();
        self.stats.apply_delta(commit.stats_delta);
        emit_vm_map_path_duration(b"debug.vm.map_path.munmap.stats_ns", stats_start);
        emit_vm_trace(b"debug.vm.unmap.phase", 4);
        emit_vm_map_path_count(
            b"debug.vm.map_path.munmap.changed_pages",
            commit.changed_pages as u64,
        );
        emit_vm_map_path_count(
            b"debug.vm.map_path.munmap.pmap_removed",
            pmap_removed as u64,
        );
        emit_vm_map_path_duration(b"debug.vm.map_path.munmap.total_ns", total_start);
        Ok(commit)
    }

    pub fn try_mprotect(&self, range: UserRange, prot: Prot) -> Result<VmMapCommit, VmMapError> {
        emit_vm_trace(b"debug.vm.protect.phase", 0);
        let _guard = self.acquire_writer(range)?;
        emit_vm_trace(b"debug.vm.protect.phase", 1);
        let commit = self.recipes.protect(range, prot)?;
        emit_vm_trace(b"debug.vm.protect.phase", 2);
        emit_vm_trace(
            b"debug.vm.protect.changed_pages",
            commit.changed_pages as i64,
        );
        let pmap_removed = self.pmap.teardown_range(range)?;
        emit_vm_trace(b"debug.vm.protect.phase", 3);
        emit_vm_trace(b"debug.vm.protect.pmap_removed", pmap_removed as i64);
        self.stats.apply_delta(commit.stats_delta);
        emit_vm_trace(b"debug.vm.protect.phase", 4);
        Ok(commit)
    }

    /// Set or clear the `locked` flag on every recipe overlapping `range`.
    ///
    /// Under no-swap, locked is purely observational: it sets
    /// `VmEntryFlags.locked` for `/proc/<pid>/maps` reporting and takes no
    /// further kernel action. The range must be fully mapped; partial holes
    /// return [`VmMapError::MissingMapping`].
    pub fn try_mlock(&self, range: UserRange, locked: bool) -> Result<VmMapCommit, VmMapError> {
        let _guard = self.acquire_writer(range)?;
        self.recipes.set_locked(range, locked)
    }

    fn acquire_writer(&self, range: UserRange) -> Result<RangeGuard<'_>, VmMapError> {
        match self
            .range_lock
            .acquire_step(range, LockMode::ExclusiveWriter)
        {
            V3StepOutcome::Done(guard) => Ok(guard),
            V3StepOutcome::Yield { .. } => Err(VmMapError::WouldBlock),
            _ => unreachable_acquire_step(),
        }
    }

    fn commit_reserved_map(
        &self,
        entry: VmEntry,
        placement: MapPlacement,
    ) -> Result<VmMapCommit, VmMapError> {
        let range = entry.range;
        emit_vm_trace(b"debug.vm.mmap.commit.phase", 0);
        let recipe_start = vm_map_path_clock_now();
        let commit = self.recipes.commit_map(entry, placement)?;
        emit_vm_map_path_duration(b"debug.vm.map_path.mmap.commit_recipe_ns", recipe_start);
        emit_vm_trace(b"debug.vm.mmap.commit.phase", 1);
        emit_vm_trace(
            b"debug.vm.mmap.commit.changed_pages",
            commit.changed_pages as i64,
        );
        if placement == MapPlacement::FixedReplace {
            emit_vm_trace(b"debug.vm.mmap.commit.fixed_teardown", 1);
            let teardown_start = vm_map_path_clock_now();
            self.pmap.teardown_range(range)?;
            emit_vm_map_path_duration(
                b"debug.vm.map_path.mmap.commit_fixed_pmap_teardown_ns",
                teardown_start,
            );
            emit_vm_trace(b"debug.vm.mmap.commit.phase", 2);
        }
        emit_vm_trace(b"debug.vm.mmap.commit.phase", 3);
        let stats_start = vm_map_path_clock_now();
        self.stats.apply_delta(commit.stats_delta);
        emit_vm_map_path_duration(b"debug.vm.map_path.mmap.commit_stats_ns", stats_start);
        emit_vm_trace(b"debug.vm.mmap.commit.phase", 4);
        Ok(commit)
    }
}

fn remap_union_range(old_range: UserRange, new_range: UserRange) -> Result<UserRange, VmMapError> {
    let start = old_range
        .start()
        .as_usize()
        .min(new_range.start().as_usize());
    let end = old_range.end().as_usize().max(new_range.end().as_usize());
    UserRange::new_aligned(UserVirtAddr(start), end - start).map_err(|_| VmMapError::InvalidRange)
}

fn emit_vm_trace(name: &[u8], value: i64) {
    if !cfg!(tx_vm_phase_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
    }
}

#[inline(always)]
fn vm_map_path_metrics_enabled() -> bool {
    cfg!(tx_vm_map_path_metrics) && tx_observe::current().is_some()
}

#[inline(always)]
fn vm_map_path_clock_now() -> Option<u64> {
    if vm_map_path_metrics_enabled() {
        Some(tx_observe::clock_now_ns())
    } else {
        None
    }
}

fn emit_vm_map_path_duration(name: &[u8], start: Option<u64>) {
    let Some(start) = start else {
        return;
    };
    let duration = tx_observe::clock_now_ns().saturating_sub(start);
    emit_vm_map_path_count(name, duration);
}

fn emit_vm_map_path_count(name: &[u8], value: u64) {
    if !vm_map_path_metrics_enabled() {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value as i64);
    }
}

fn vm_fault_access_trace_id(access: AccessMode) -> i64 {
    match access {
        AccessMode::Read => 1,
        AccessMode::Write => 2,
        AccessMode::Execute => 3,
    }
}

fn vm_backing_trace_id(backing: VmEntryBacking) -> i64 {
    match backing {
        VmEntryBacking::None => 0,
        VmEntryBacking::PrivateAnon => 1,
        VmEntryBacking::Page { .. } => 2,
    }
}

fn remap_teardown_ranges(
    old_range: UserRange,
    new_range: UserRange,
    placement: VmRemapPlacement,
) -> Result<Vec<UserRange>, VmMapError> {
    match placement {
        VmRemapPlacement::Move => Ok(alloc::vec![old_range, new_range]),
        VmRemapPlacement::InPlace if new_range.len() < old_range.len() => {
            let start = old_range
                .start()
                .as_usize()
                .checked_add(new_range.len())
                .ok_or(VmMapError::InvalidRange)?;
            let len = old_range.len() - new_range.len();
            UserRange::new_aligned(UserVirtAddr(start), len)
                .map(|range| alloc::vec![range])
                .map_err(|_| VmMapError::InvalidRange)
        }
        VmRemapPlacement::InPlace => Ok(Vec::new()),
    }
}

pub enum MapReserveResult<'a> {
    Reserved(MapReservation<'a>),
    Blocked(WaitToken),
    Err(VmMapError),
}

enum FaultScriptResolve {
    Done(VmFaultOutcome),
    Wait(WaitToken),
}

enum FaultScriptPublish {
    Done(PmapPublishOutcome),
    Wait(WaitToken),
}

fn private_anon_write_batch_shape(outcome: &VmFaultOutcome) -> bool {
    outcome.access == AccessMode::Write
        && !outcome.entry.flags.shared
        && outcome.entry.ufd_registration.is_none()
        && matches!(outcome.entry.backing_kind(), VmEntryBacking::PrivateAnon)
        && outcome.entry.prot.write
        && outcome.entry.range.len() >= PRIVATE_ANON_FAULT_BATCH_MIN_VMA_BYTES
}

impl core::fmt::Debug for MapReserveResult<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reserved(_) => f.write_str("Reserved(..)"),
            Self::Blocked(token) => f.debug_tuple("Blocked").field(token).finish(),
            Self::Err(error) => f.debug_tuple("Err").field(error).finish(),
        }
    }
}

/// Hint values passed to `AddressSpace::madvise`.
///
/// Per VM_v1_2 §5.9: `WillNeed`, `Normal`, `Random`, `Sequential` are
/// observation-only no-ops (§9.7). `DontNeed` and `Free` perform a
/// range-scoped pmap teardown (mini-munmap that preserves recipes) so the
/// next access refaults clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MadviseAdvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
    Free,
}

impl AddressSpace {
    /// Returns one boolean per page in `range`: `true` if the page currently
    /// has a published pmap entry, `false` otherwise. The boolean at index
    /// `i` corresponds to `range`'s page at offset `i`.
    ///
    /// V1 implementation reads the snapshot returned by `VmPmap::walk_range`;
    /// concurrent publishes after the call returns are not reflected.
    pub fn mincore(&self, range: UserRange) -> Vec<bool> {
        let walked = self.pmap().walk_range(range);
        let mut walked_iter = walked.into_iter().peekable();
        range
            .iter_pages()
            .map(|page| match walked_iter.peek() {
                Some((p, _)) if *p == page => {
                    walked_iter.next();
                    true
                }
                _ => false,
            })
            .collect()
    }

    /// Apply `advice` to `range`. V1 implements:
    ///
    /// - `DontNeed` / `Free` — range-scoped pmap teardown plus shootdown,
    ///   preserving recipes (next access refaults clean). Acquires an
    ///   `ExclusiveWriter` reservation per VM_v1_2 §5.9. Returns
    ///   `VmMapError::WouldBlock` on contention; callers should retry on
    ///   the wait channel.
    /// - `WillNeed`, `Normal`, `Random`, `Sequential` — observation-only
    ///   no-ops per §9.7; readahead and layout hints are deferred.
    pub fn madvise(&self, range: UserRange, advice: MadviseAdvice) -> Result<(), VmMapError> {
        match advice {
            MadviseAdvice::DontNeed | MadviseAdvice::Free => {
                let _guard = self.acquire_writer(range)?;
                self.pmap.teardown_range(range)?;
                let guard = step_engine::guard();
                self.stats.store(self.recipes.stats(&guard));
                Ok(())
            }
            MadviseAdvice::Normal
            | MadviseAdvice::Random
            | MadviseAdvice::Sequential
            | MadviseAdvice::WillNeed => Ok(()),
        }
    }

    /// Synchronously flushes dirty pages of any File-backed `PageContainer`
    /// intersecting `range` by calling `page_backed::step_fsync` per unique
    /// PC. Anon, PrivateAnon, Device, and `None` backings are no-op. Returns
    /// the first non-`Done` outcome from any underlying fsync; `Done(())` if
    /// every visited PC flushed cleanly.
    pub fn msync(
        &self,
        range: UserRange,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), step_engine::PageProgress> {
        use crate::page_backed::adapter::step_engine::StepOutcome as V3;
        let entries = self.recipes.snapshot(guard);
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        for entry in entries {
            if !entry.range.overlaps(range) {
                continue;
            }
            let Some((pc, _)) = entry.page_backing() else {
                continue;
            };
            if !matches!(pc.kind(), PageContainerKind::File { .. }) {
                continue;
            }
            if !visited.insert(pc.raw()) {
                continue;
            }
            match step_fsync(pc, guard) {
                V3::Done(()) => continue,
                other => return other,
            }
        }
        V3::done(())
    }
}

pub struct MapReservation<'a> {
    aspace: &'a AddressSpace,
    entry: VmEntry,
    placement: MapPlacement,
    _guard: RangeGuard<'a>,
}

impl MapReservation<'_> {
    pub fn entry(&self) -> VmEntry {
        self.entry.clone()
    }

    pub fn placement(&self) -> MapPlacement {
        self.placement
    }

    pub fn commit(self) -> Result<VmMapCommit, VmMapError> {
        self.aspace.commit_reserved_map(self.entry, self.placement)
    }
}

/// Await a `RangeLock` release after the canonical `acquire_step` returned
/// `StepOutcome::Yield { shape: YieldShape::OnWaitSource { .. } }` (carrying
/// a `WaitToken`). Resolved through the global wait-source registry; if the
/// token's channel has been retired the await is a no-op and the caller's
/// retry loop runs immediately.
async fn await_range_lock(token: WaitToken) {
    if let Some(wait) = crate::wait_source::wait_on_token(token) {
        let _ = wait.await;
    }
}

/// `RangeLock::acquire_step` only ever produces `V3StepOutcome::Done`
/// or `V3StepOutcome::Yield { OnWaitSource }`; `acquire_pair_step` only
/// ever produces `StepOutcome::Done` or `StepOutcome::Yield { shape:
/// YieldShape::OnWaitSource { .. } }`. Other variants are unreachable
/// by construction; this helper centralises the panic message so the
/// asserts stay terse at the call sites.
#[inline(always)]
fn unreachable_acquire_step() -> ! {
    unreachable!(
        "RangeLock::acquire_step / acquire_pair_step never produce \
         Continue / Yield / Err / Done"
    );
}

// =========================================================================
// PR-10 phase 4 — userfaultfd OnAgent branch helpers.
// =========================================================================

/// Resolution context the fault-script consults when a faulting page
/// hits a ufd-registered VMA.
///
/// PR-10 phase 4 takes the dispatcher as a trait object (rather than
/// hard-coding a global registry lookup) so:
///
/// - production callers (`thread_future`) can resolve via the
///   per-process fd-table — owning process → fd-table → `Cap<OpenFile>`
///   → `OpenFileBacking::Ufd { ufd }` → `&UserfaultFd`,
/// - integration tests can provide a synthetic dispatcher that wraps
///   a single `Cap<UserfaultFd>` without bootstrapping a process,
/// - the existing `fault_script(VmFault)` entrypoint keeps working
///   unchanged by passing a [`NullUfdDispatch`] that always returns
///   `None` (the VMA's `ufd_registration` field is silently ignored
///   on this entrypoint until phase 5 wires the real per-process
///   dispatcher).
pub trait UfdDispatch {
    /// Resolution context returned by [`Self::resolve`]: the per-ufd
    /// [`DelegateRegistry`] the script installs a request against,
    /// plus a `Weak<TaskMailbox>` pointing at the faulting thread's
    /// mailbox so the registry's `mark_replied` / `mark_canceled`
    /// fires the wake event.
    fn resolve(&self, ufd_id: u64) -> Option<UfdDispatchTarget<'_>>;
}

/// Resolution result returned by [`UfdDispatch::resolve`]. Holds a
/// borrow on the per-ufd [`DelegateRegistry`] and a `Weak<TaskMailbox>`
/// for the faulting task. The lifetime is tied to the dispatcher
/// argument's lifetime — the borrow is short-lived and released
/// before the await on the agent reply.
pub struct UfdDispatchTarget<'a> {
    /// Per-ufd [`DelegateRegistry`] (D7 §3.3: per-ufd grouping). The
    /// fault-script calls `install_request` against this registry and
    /// `await_agent_reply` keys against the same registry to drain
    /// the reply payload.
    pub registry: &'a DelegateRegistry,
    /// Mailbox of the faulting task. Stored `Weak` so a dead task
    /// (script frame torn down before the agent replies) doesn't
    /// pin its mailbox alive — `mark_replied`'s `Weak::upgrade`
    /// failure silently drops the wake event (correct per PR-7B).
    pub mailbox: Weak<TaskMailbox>,
    /// PR-10 phase 5: per-ufd pending-fault-message queue handle.
    /// Set when the dispatcher can also drive the agent-side
    /// `read(uffd_fd, &mut uffd_msg)` arm; `None` keeps the phase-4
    /// path behaving unchanged (no message is enqueued — the
    /// `mark_replied` path still drives resume via the
    /// `await_agent_reply` helper, but the agent has no way to
    /// `read()` the fault). Production callers always set this; tests
    /// that want to exercise the OnAgent path *without* the queue
    /// (state-machine isolation) leave it `None`.
    pub fault_pusher: Option<&'a crate::userfaultfd::UserfaultFd>,
}

/// Null dispatcher: `resolve` returns `None` for every id. Used by
/// the legacy `fault_script(VmFault)` entrypoint so the existing
/// `thread_future` callsite continues to compile unchanged. The
/// fault-script falls through to the normal `materialize_pagebacked`
/// path when the dispatcher reports no live ufd — i.e. the VMA's
/// `ufd_registration` tag is silently ignored on this entrypoint
/// until phase 5 wires the real per-process dispatcher.
pub struct NullUfdDispatch;

impl UfdDispatch for NullUfdDispatch {
    fn resolve(&self, _ufd_id: u64) -> Option<UfdDispatchTarget<'_>> {
        None
    }
}

/// Drive the OnAgent branch: install a `DelegateRequest::Ufd(PageFault)`
/// request, await the reply via the `await_agent_reply` helper.
///
/// Returns:
/// - `Ok(Some(reply))` on a `mark_replied` Applied transition.
/// - `Ok(None)` if the dispatcher could not resolve the ufd id
///   (graceful fall-through — the fault path materializes normally).
/// - `Err(WouldBlock)` on agent-died / canceled / timed-out abort
///   (phase 4 stub mapping; phase 6 may add a dedicated variant).
async fn dispatch_ufd_fault<D: UfdDispatch>(
    dispatch: &D,
    ufd_id: u64,
    fault: VmFault,
) -> Result<Option<DelegateReply>, VmFaultError> {
    let Some(target) = dispatch.resolve(ufd_id) else {
        return Ok(None);
    };
    // Upgrade the mailbox once before installing the request so we
    // can register on `await_agent_reply` against a live mailbox. If
    // the mailbox has already died, treat as agent-side-dead.
    let Some(mailbox_arc) = target.mailbox.upgrade() else {
        return Err(VmFaultError::WouldBlock);
    };
    let request = DelegateRequest::Ufd(UfdRequest::PageFault {
        faulting_addr: fault.addr.0 as u64,
        access_kind: UfdAccessKind::Missing,
        faulting_tid: 0,
    });
    let guard = target.registry.install_request(
        request,
        ufd_id,
        AgentCancelPolicy::BestEffort,
        TokenDropPolicy::CancelOnDrop,
        target.mailbox.clone(),
        None,
    );
    let token_id = guard.id();
    // PR-10 phase 5: push the fault message onto the per-ufd pending
    // queue so the agent thread's `read(uffd_fd, &mut uffd_msg)`
    // arm can dequeue and learn the faulting address. Order: push
    // **after** `install_request` returns so `token_id` is stable
    // and the agent's `UFFDIO_COPY` / `_ZEROPAGE` / `_CONTINUE`
    // reply can resolve token_id → `mark_replied`. `fault_pusher`
    // is `None` for state-machine-isolation tests; production
    // dispatchers always supply it.
    if let Some(pusher) = target.fault_pusher {
        pusher.push_fault_msg(crate::userfaultfd::UffdMsg {
            event: 0x12, // UFFD_EVENT_PAGEFAULT
            fault_addr: fault.addr.0 as u64,
            ufd_thread_id: 0,
            token_id,
        });
    }
    // Yield OnAgent semantically by parking on the mailbox via the
    // await_agent_reply helper. The helper consumes
    // `AgentReplied`/`Abort` events matching `token_id` and re-posts
    // anything else so the rightful consumer can drain it.
    //
    // The agent-side mark_replied happens via UFFDIO_COPY / ZEROPAGE
    // (phase 5). For phase 4 the test drives mark_replied directly
    // to exercise the plumbing.
    let outcome = crate::vm::adapter::wait_routing::await_agent_reply(
        token_id,
        &mailbox_arc,
        target.registry,
    )
    .await;
    // Drop the guard *after* the await — if the agent replied
    // successfully the CancelOnDrop transition is a no-op
    // (LateNoOp(Replied)); if the await aborted (AgentDied / Canceled
    // / TimedOut) the guard's drop is also a no-op against the
    // already-terminal state. DTOK-3 carry-through preserved.
    drop(guard);
    match outcome {
        Ok(reply) => Ok(Some(reply)),
        Err(AbortReason::AgentDied)
        | Err(AbortReason::Canceled)
        | Err(AbortReason::TimedOut)
        | Err(AbortReason::Interrupted)
        | Err(AbortReason::Killed)
        | Err(AbortReason::ScopeAbandoned) => Err(VmFaultError::WouldBlock),
    }
}

/// PR-10 phase 6 — `UFFDIO_COPY` byte-move.
///
/// Allocate a fresh private page (via
/// [`VmFaultOutcome::materialize_pagebacked_for_ufd_copy`]) and
/// memcpy `len` bytes from the agent's `src_kernel_addr` into the
/// kernel-VA of the destination PPN. The materialization is returned
/// with `replace_existing = true` so the caller's
/// `publish_page_with_replacement` swaps in the agent-filled page
/// even if a stale zero-frame is installed.
///
/// **Validation.** Per phase 6 prompt constraint #1:
/// `src_kernel_addr` must be non-null and `len` must be a positive
/// page-multiple. The agent supplies these via `UFFDIO_COPY`; the
/// shim layer (`step_uffdio_copy`) is supposed to validate before
/// `mark_replied`, but we re-check here under the materialize lane to
/// keep the substrate's invariants tight (A-15 fresh-epoch re-validation).
///
/// **Safety.** `src_kernel_addr` is a kernel pointer to a buffer the
/// agent populated via `read(uffd_fd)` → `UFFDIO_COPY`. The substrate
/// trusts the shim to validate that `src` is kernel-addressable
/// before `mark_replied`. The destination is the freshly-allocated
/// frame's kernel direct-map VA; the page is held alive by the
/// returned [`VmFaultMaterialization`]'s `map_pin` for the duration
/// of the copy and the subsequent publish.
///
/// **Scope.** PrivateAnon only (per phase 6 prompt constraint #4).
/// Page-backed / Shared backings return `BackingMismatch`; supporting
/// `UFFDIO_COPY` against those is a follow-up.
fn materialize_ufd_copy(
    outcome: &VmFaultOutcome,
    src_kernel_addr: u64,
    len: u64,
) -> Result<VmFaultMaterialization, VmFaultError> {
    // Validate src + len. The shim layer's `step_uffdio_copy` is the
    // primary gate, but A-15 says fresh-epoch consumers must
    // re-validate.
    if src_kernel_addr == 0 || len == 0 {
        return Err(VmFaultError::BackingMismatch);
    }
    if !(len as usize).is_multiple_of(crate::vm::USER_PAGE_SIZE) {
        return Err(VmFaultError::BackingMismatch);
    }
    let materialization = outcome.materialize_pagebacked_for_ufd_copy()?;
    // Look up the destination's kernel direct-map address. On
    // bare-metal this is the linear-map VA of the freshly-allocated
    // frame; in tests the page_allocator's test backend installs a
    // direct-map hook that returns a usable `*mut u8`.
    let dst_kernel_ptr = step_engine::page_allocator::frame_kernel_addr(materialization.page.ppn)
        .map_err(|alloc_err| {
        VmFaultError::PageCache(crate::page_backed::PageCacheError::Alloc(alloc_err))
    })?;
    // Copy `len` bytes from the agent's `src` buffer into the
    // freshly-allocated frame. `len` is page-multiple and bounded to
    // a single page (the fault-script materializes one user page at
    // a time), so for phase-6 PrivateAnon we clamp to USER_PAGE_SIZE.
    let copy_len = core::cmp::min(len as usize, crate::vm::USER_PAGE_SIZE);
    // SAFETY:
    // - `src_kernel_addr` is a kernel pointer supplied by the agent's
    //   `UFFDIO_COPY` payload. The shim layer validates this is in
    //   kernel-readable memory before `mark_replied`; A-15 says the
    //   substrate trusts that gating.
    // - `dst_kernel_ptr` points at the freshly-allocated frame's
    //   direct-map VA. The page is held alive by
    //   `materialization.page.map_pin` for the entire substrate
    //   resume window.
    // - `copy_len` is bounded to USER_PAGE_SIZE so the write stays
    //   inside the allocated frame.
    // - src and dst are non-overlapping: src is the agent's buffer in
    //   kernel-bookkept storage, dst is a fresh frame just minted by
    //   `reserve_frame`.
    unsafe {
        core::ptr::copy_nonoverlapping(src_kernel_addr as *const u8, dst_kernel_ptr, copy_len);
    }
    Ok(materialization)
}
