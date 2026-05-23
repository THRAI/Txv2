//! Boot-reactor submission hooks for cloned userspace threads.
//!
//! These helpers are split from `init.rs` to keep the boot spine small while
//! preserving the deferred-submit rule: syscall polling cannot re-enter the
//! boot reactor lock, so `sys_clone` queues child threads and the hart loops
//! drain them between reactor steps.

use super::*;

static THREAD_REACTOR_TASKS: SpinMutex<alloc::vec::Vec<(u32, boot_runtime::TaskKey)>> =
    SpinMutex::new(alloc::vec::Vec::new());

struct PendingChildSubmit {
    submit_cpu: CpuId,
    child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
}

static PENDING_CHILD_SUBMITS: SpinMutex<alloc::vec::Vec<PendingChildSubmit>> =
    SpinMutex::new(alloc::vec::Vec::new());

impl<P: TxPlatform> CoreInit<P> {
    /// Reactor-submission hook body. Captured by
    /// [`Self::install_reactor_submit_seam`] as a plain `fn` pointer
    /// (the platform parameter `P` is monomorphised at install time
    /// so the resulting fn pointer is parameter-free).
    pub(crate) fn submit_child_thread_into_boot_reactor(
        _child_process: Cap<tx_subsystems::process::ProcessIdentity>,
        child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) {
        // The reactor's `BOOT_REACTOR.with(...)` lock is held by
        // `step_boot_reactor_once` while polling tasks. The currently-polled
        // task may be sys_clone; defer child submit to the between-step drain.
        Self::queue_pending_child_submit(<P as tx_hal::SmpIf>::current_cpu_id(), child_thread);
    }

    pub(super) fn register_thread_reactor_task(tid: u32, task: boot_runtime::TaskKey) {
        let mut tasks = THREAD_REACTOR_TASKS.lock();
        if let Some((_, existing)) = tasks
            .iter_mut()
            .find(|(existing_tid, _)| *existing_tid == tid)
        {
            *existing = task;
        } else {
            tasks.push((tid, task));
        }
    }

    fn thread_reactor_task(tid: u32) -> Option<boot_runtime::TaskKey> {
        THREAD_REACTOR_TASKS
            .lock()
            .iter()
            .find(|(existing_tid, _)| *existing_tid == tid)
            .map(|(_, task)| *task)
    }

    pub(super) fn set_thread_reactor_affinity(
        tid: u32,
        affinity: u64,
    ) -> Result<(), tx_subsystems::reactor_affinity::ReactorAffinityError> {
        if affinity == 0 || (affinity & P::online_cpus().bits()) == 0 {
            return Err(tx_subsystems::reactor_affinity::ReactorAffinityError::InvalidMask);
        }
        let task = Self::thread_reactor_task(tid)
            .ok_or(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)?;
        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        let current_hart = boot_runtime::HartId(current_cpu.0);
        let mut signal = SmpRescheduleSignal::<P>::new();
        BOOT_REACTOR
            .with(|reactor| reactor.set_task_affinity(task, affinity, current_hart, &mut signal))
            .ok_or(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)?
            .map(|_| ())
            .map_err(|_| tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)
    }

    pub(super) fn get_thread_reactor_affinity(
        tid: u32,
    ) -> Result<u64, tx_subsystems::reactor_affinity::ReactorAffinityError> {
        let task = Self::thread_reactor_task(tid)
            .ok_or(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)?;
        BOOT_REACTOR
            .with(|reactor| reactor.task_affinity(task))
            .ok_or(tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)?
            .map_err(|_| tx_subsystems::reactor_affinity::ReactorAffinityError::NoSuchThread)
    }

    fn queue_pending_child_submit(
        submit_cpu: CpuId,
        child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) {
        PENDING_CHILD_SUBMITS.lock().push(PendingChildSubmit {
            submit_cpu,
            child_thread,
        });
    }

    pub(super) fn drain_pending_child_submits() -> bool {
        let mut submitted_any = false;
        loop {
            let next = PENDING_CHILD_SUBMITS.lock().pop();
            let Some(pending) = next else {
                break;
            };
            let child_thread = pending.child_thread;
            let Some(payload) = child_thread.payload_cap() else {
                continue;
            };
            let task_payload = payload.clone();
            let submit_hart = boot_runtime::HartId(pending.submit_cpu.0);
            let child_tid = child_thread.tid.0;
            let mut signal = SmpRescheduleSignal::<P>::new();
            let submitted = BOOT_REACTOR.with(|reactor| {
                reactor.submit_task_with_meta_from_hart(
                    crate::thread_future::PerHartSlotted::<P, _>::new(
                        task_payload.clone(),
                        crate::thread_future::run_thread::<P>(child_thread, task_payload),
                    ),
                    Self::userspace_thread_sched_meta_for(pending.submit_cpu).preempted_on_submit(),
                    submit_hart,
                    &mut signal,
                )
            });
            if let Some((task_key, _report)) = submitted {
                submitted_any = true;
                Self::register_thread_reactor_task(child_tid, task_key);
            }
        }
        submitted_any
    }
}
