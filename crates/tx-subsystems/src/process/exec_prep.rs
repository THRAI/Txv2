//! Process-side helpers for transactional exec preparation and the
//! infallible post-PoNR state commits.
//!
//! Split out of `execution.rs` to keep file sizes within the
//! per-file authoring cap (1500 lines). These three helpers form a
//! coherent "post-aspace-swap, pre-user-entry" group called only from
//! the exec script.

use alloc::vec::Vec;

use crate::process::adapter::step_engine::{Cap, PayloadCap};

use crate::process::structure::{ExecCollapseHandoff, ProcessIdentity, ProcessPayload};
use crate::thread_runtime::{ThreadExitOutcome, ThreadIdentity};
use crate::vfs::OpenFile;
use crate::vm::AddressSpace;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecPrepError {
    Again,
    OutOfMemory,
    Zombie,
    StaleBinding,
}

struct PreparedCloexecFd {
    fd: u32,
    expected_file: Option<Cap<OpenFile>>,
}

/// Fallibly allocated pre-PoNR snapshot of the complete CLOEXEC set.
///
/// Each populated fd retains the exact `OpenFile` capability observed during
/// preparation. Retention prevents slot reuse, while commit compares that
/// identity under the fd-table locks before making the address-space swap
/// visible.
pub struct PreparedCloexecClose {
    entries: Vec<PreparedCloexecFd>,
}

#[cfg(any(test, feature = "test-support"))]
static FAIL_NEXT_CLOEXEC_PLAN_ALLOCATION: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub fn fail_next_cloexec_plan_allocation_for_test() {
    FAIL_NEXT_CLOEXEC_PLAN_ALLOCATION.store(true, core::sync::atomic::Ordering::Release);
}

#[cfg(any(test, feature = "test-support"))]
pub fn cloexec_plan_allocation_fault_pending_for_test() -> bool {
    FAIL_NEXT_CLOEXEC_PLAN_ALLOCATION.load(core::sync::atomic::Ordering::Acquire)
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_cloexec_plan_allocation_fault_for_test() {
    FAIL_NEXT_CLOEXEC_PLAN_ALLOCATION.store(false, core::sync::atomic::Ordering::Release);
}

/// RAII owner of the process lifecycle episode used by exec.
///
/// The retained payload is evidence only while the identity's authoritative
/// payload slot still names the same cap. Every pre-PoNR mutation validates
/// that binding and the generation-owned `group_exit` episode.
pub struct ProcessExecPrep {
    process: Cap<ProcessIdentity>,
    payload: PayloadCap<ProcessPayload>,
    generation: u64,
    active: bool,
}

impl ProcessExecPrep {
    pub fn begin(
        process: &Cap<ProcessIdentity>,
        initiator: &Cap<ThreadIdentity>,
    ) -> Result<Self, ExecPrepError> {
        let payload_guard = process.payload_slot().lock();
        let payload = payload_guard
            .as_ref()
            .cloned()
            .ok_or(ExecPrepError::Zombie)?;
        if !payload
            .threads
            .snapshot()
            .iter()
            .any(|thread| thread.key() == initiator.key())
        {
            return Err(ExecPrepError::StaleBinding);
        }
        let generation = payload
            .reserve_exec_lifecycle(initiator.tid.0)
            .ok_or(ExecPrepError::Again)?;
        Ok(Self {
            process: process.clone(),
            payload,
            generation,
            active: true,
        })
    }

    pub(crate) fn payload(&self) -> &PayloadCap<ProcessPayload> {
        &self.payload
    }

    pub(crate) fn validate_binding(&self) -> Result<(), ExecPrepError> {
        if !self.active {
            return Err(ExecPrepError::StaleBinding);
        }
        let payload_guard = self.process.payload_slot().lock();
        let Some(bound) = payload_guard.as_ref() else {
            return Err(ExecPrepError::Zombie);
        };
        if bound.key() != self.payload.key()
            || !self.payload.exec_lifecycle_matches(self.generation)
        {
            return Err(ExecPrepError::StaleBinding);
        }
        Ok(())
    }

    pub fn collapse_threads(
        &mut self,
        initiator: &Cap<ThreadIdentity>,
    ) -> Result<usize, ExecPrepError> {
        self.collapse_threads_after_lane_transition(initiator, || {})
    }

    fn collapse_threads_after_lane_transition<H>(
        &mut self,
        initiator: &Cap<ThreadIdentity>,
        after_lane_transition: H,
    ) -> Result<usize, ExecPrepError>
    where
        H: FnOnce(),
    {
        self.validate_binding()?;
        let siblings = self
            .payload
            .threads
            .snapshot()
            .into_iter()
            .filter(|thread| thread.key() != initiator.key())
            .collect::<alloc::vec::Vec<_>>();
        if !self
            .payload
            .begin_exec_collapse(self.generation, siblings.len() as u32)
        {
            return Err(ExecPrepError::StaleBinding);
        }
        after_lane_transition();
        for sibling in siblings {
            match crate::thread_runtime::step_thread_exit(sibling, 0) {
                ThreadExitOutcome::Completed => {}
                ThreadExitOutcome::Retry => {}
            }
        }
        if !self.payload.finish_exec_collapse(self.generation) {
            match self.payload.handoff_exec_collapse_abort(self.generation) {
                ExecCollapseHandoff::Completed => {}
                ExecCollapseHandoff::Aborting => {
                    self.active = false;
                    return Err(ExecPrepError::Again);
                }
                ExecCollapseHandoff::Stale => return Err(ExecPrepError::StaleBinding),
            }
        }
        self.validate_binding()?;
        Ok(self.payload.threads.count())
    }

    #[cfg(test)]
    pub(crate) fn collapse_threads_after_lane_transition_for_test<H>(
        &mut self,
        initiator: &Cap<ThreadIdentity>,
        after_lane_transition: H,
    ) -> Result<usize, ExecPrepError>
    where
        H: FnOnce(),
    {
        self.collapse_threads_after_lane_transition(initiator, after_lane_transition)
    }

    /// Final pre-PoNR validation. After this succeeds the lifecycle episode
    /// excludes exit/detach until the authoritative commit consumes it.
    pub fn validate_commit_ready(&self) -> Result<(), ExecPrepError> {
        self.validate_binding()
    }

    /// Prepare the complete CLOEXEC close set while exec is still reversible.
    /// All vector allocation and `Cap` cloning happens here, before PoNR.
    pub fn prepare_cloexec_close(&self) -> Result<PreparedCloexecClose, ExecPrepError> {
        self.validate_binding()?;
        let fds = self.payload.fds.lock();
        let cloexec = self.payload.fd_cloexec.lock();
        #[cfg(any(test, feature = "test-support"))]
        if FAIL_NEXT_CLOEXEC_PLAN_ALLOCATION.swap(false, core::sync::atomic::Ordering::AcqRel) {
            return Err(ExecPrepError::OutOfMemory);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(cloexec.len())
            .map_err(|_| ExecPrepError::OutOfMemory)?;
        for &fd in cloexec.iter() {
            entries.push(PreparedCloexecFd {
                fd,
                expected_file: fds.get(&fd).cloned(),
            });
        }
        Ok(PreparedCloexecClose { entries })
    }

    /// Begin PoNR only after the prepared CLOEXEC snapshot still matches.
    ///
    /// The payload slot, fd table, and CLOEXEC set remain locked from final
    /// validation through address-space publication and fd removal. Therefore a
    /// concurrent close/dup/fcntl cannot redirect a prepared fd number to a
    /// different file between validation and commit. The post-swap path only
    /// removes B-tree nodes and releases already-retained capabilities; it does
    /// not clone or allocate.
    pub fn replace_aspace_and_close_cloexec(
        &self,
        new: Cap<AddressSpace>,
        plan: PreparedCloexecClose,
    ) -> Result<Cap<AddressSpace>, ExecPrepError> {
        if !self.active {
            return Err(ExecPrepError::StaleBinding);
        }
        let payload_guard = self.process.payload_slot().lock();
        let bound = payload_guard.as_ref().ok_or(ExecPrepError::Zombie)?;
        if bound.key() != self.payload.key()
            || !self.payload.exec_lifecycle_matches(self.generation)
        {
            return Err(ExecPrepError::StaleBinding);
        }

        let mut fds = self.payload.fds.lock();
        let mut cloexec = self.payload.fd_cloexec.lock();
        let plan_matches = cloexec.len() == plan.entries.len()
            && plan.entries.iter().all(|entry| {
                cloexec.contains(&entry.fd)
                    && match (&entry.expected_file, fds.get(&entry.fd)) {
                        (Some(expected), Some(current)) => expected.key() == current.key(),
                        (None, None) => true,
                        _ => false,
                    }
            });
        if !plan_matches {
            return Err(ExecPrepError::Again);
        }

        let previous = self
            .payload
            .frame
            .vm
            .swap(Some(new))
            .expect("live exec payload always owns an address space");
        for entry in &plan.entries {
            let removed_bit = cloexec.remove(&entry.fd);
            debug_assert!(removed_bit, "validated CLOEXEC bit disappeared under lock");
            if let Some(expected) = &entry.expected_file {
                let removed = fds
                    .remove(&entry.fd)
                    .expect("validated CLOEXEC fd disappeared under lock");
                debug_assert_eq!(removed.key(), expected.key());
                drop(removed);
            }
        }
        drop(cloexec);
        drop(fds);
        drop(payload_guard);

        // EXEC-PONR remains pending until endpoint close publication uses a
        // preallocated bounded WaitSource/mailbox path. A last pipe or
        // socketpair reference can currently publish from this loop.
        for entry in &plan.entries {
            if let Some(file) = &entry.expected_file {
                crate::process::structure::decr_pipe_fd_ref(file);
            }
        }
        Ok(previous)
    }

    #[cfg(test)]
    pub(crate) fn replace_aspace(
        &self,
        new: Cap<AddressSpace>,
    ) -> Result<Cap<AddressSpace>, ExecPrepError> {
        if !self.active {
            return Err(ExecPrepError::StaleBinding);
        }
        let payload_guard = self.process.payload_slot().lock();
        let bound = payload_guard.as_ref().ok_or(ExecPrepError::Zombie)?;
        if bound.key() != self.payload.key()
            || !self.payload.exec_lifecycle_matches(self.generation)
        {
            return Err(ExecPrepError::StaleBinding);
        }
        Ok(self
            .payload
            .frame
            .vm
            .swap(Some(new))
            .expect("live exec payload always owns an address space"))
    }

    /// Validate the identity-to-payload binding and perform the final exec
    /// mutation while the authoritative payload slot remains locked. The
    /// lifecycle reservation is consumed only after `commit` succeeds.
    pub(crate) fn commit_authoritative<R>(
        mut self,
        commit: impl FnOnce(&PayloadCap<ProcessPayload>) -> Result<R, ExecPrepError>,
    ) -> Result<R, ExecPrepError> {
        if !self.active {
            return Err(ExecPrepError::StaleBinding);
        }
        let payload_guard = self.process.payload_slot().lock();
        let bound = payload_guard.as_ref().ok_or(ExecPrepError::Zombie)?;
        if bound.key() != self.payload.key()
            || !self.payload.exec_lifecycle_matches(self.generation)
        {
            return Err(ExecPrepError::StaleBinding);
        }
        let result = commit(&self.payload)?;
        assert!(
            self.payload.release_exec_lifecycle(self.generation),
            "authoritative exec lifecycle changed during final commit"
        );
        self.active = false;
        drop(payload_guard);
        Ok(result)
    }
}

impl Drop for ProcessExecPrep {
    fn drop(&mut self) {
        if self.active {
            let _ = self.payload.release_exec_lifecycle(self.generation);
        }
    }
}

/// Reset every user-installed signal disposition on `process` to
/// `SigDisposition::Default`, preserving `Default` and `Ignore` slots.
///
/// Thin Phase-5 wrapper around
/// [`crate::signal::SigActionTable::step_reset_for_exec`] (Wave 2 P2)
/// that lets the exec script (`tx-scripts::process::exec`) reach the
/// per-process action table without touching the `pub(crate)` payload
/// field on [`ProcessIdentity`]. Per
/// `txdoc:EXEC-12-3-RESET-SIGNAL-DISPOSITIONS` and `SIGNAL_v1` §15.2:
/// exec resets handlers but does NOT clear pending signals or
/// SIG_IGN dispositions.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload).
pub(crate) fn reset_signal_dispositions_for_exec(process: &Cap<ProcessIdentity>) {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if let Some(payload) = process.payload.lock().as_ref() {
        payload.sig_actions().step_reset_for_exec();
    }
}

/// Install `new_brk_base` as both the brk base and the current brk
/// for `process`. Per `txdoc:EXEC-12-4-INSTALL-BRK` and the Wave 2
/// plan's Part 1 P3 sub-item.
///
/// The exec script (Part 5) computes `new_brk_base` from the image
/// plan: typically the highest LOAD segment's `vaddr + memsz`,
/// page-rounded up. Storing the same value into both fields seeds the
/// process at "no heap allocated yet" — `brk(2)` with a request above
/// `current_brk` then grows the heap on demand.
///
/// Infallible — by EXEC-PONR. No-op for zombies (no payload to seed).
pub(crate) fn install_brk_for_exec(process: &Cap<ProcessIdentity>, new_brk_base: u64) {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if let Some(payload) = process.payload.lock().as_ref() {
        payload
            .brk_base
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
        payload
            .current_brk
            .store(new_brk_base, core::sync::atomic::Ordering::Release);
    }
}
