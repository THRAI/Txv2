//! Boot-reactor submission hooks for cloned userspace threads.
//!
//! These helpers are split from `init.rs` to keep the boot spine small while
//! preserving the clone submission boundary: concurrent polling releases the
//! reactor lock while a userspace thread handles `clone`, so that path submits
//! children immediately; the non-concurrent fallback still queues child
//! threads for the hart loops to drain between reactor steps.

use super::*;

use core::sync::atomic::{AtomicU64, Ordering};

static THREAD_REACTOR_TASKS: SpinMutex<alloc::vec::Vec<(u32, boot_runtime::TaskKey)>> = spin_mutex(
    alloc::vec::Vec::new(),
    b"debug.lock.kernel.thread_reactor_tasks",
);

struct PendingChildSubmit {
    submit_cpu: CpuId,
    child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
}

static PENDING_CHILD_SUBMITS: SpinMutex<alloc::vec::Vec<PendingChildSubmit>> = spin_mutex(
    alloc::vec::Vec::new(),
    b"debug.lock.kernel.pending_child_submits",
);

static BENCH_CHILD_QUEUE: AtomicU64 = AtomicU64::new(0);
static BENCH_CHILD_DRAIN: AtomicU64 = AtomicU64::new(0);
static BENCH_CHILD_SUBMIT: AtomicU64 = AtomicU64::new(0);
static BENCH_CHILD_DIRECT: AtomicU64 = AtomicU64::new(0);
static BENCH_CHILD_PAYLOAD_MISSING: AtomicU64 = AtomicU64::new(0);
static BENCH_CHILD_TERMINAL_DRAIN: AtomicU64 = AtomicU64::new(0);

const TERMINAL_THREAD_EBR_DRAIN_BUDGET: usize = 64;

#[inline(always)]
fn clone_path_metrics_enabled() -> bool {
    cfg!(tx_clone_path_metrics) && tx_observe::current().is_some()
}

#[inline(always)]
fn clone_path_clock_now() -> Option<u64> {
    if clone_path_metrics_enabled() {
        Some(tx_observe::clock_now_ns())
    } else {
        None
    }
}

fn emit_clone_path_duration(name: &[u8], start: Option<u64>) {
    let Some(start) = start else {
        return;
    };
    let duration = tx_observe::clock_now_ns().saturating_sub(start);
    emit_clone_path_count(name, duration);
}

fn emit_clone_path_count(name: &[u8], value: u64) {
    if !clone_path_metrics_enabled() {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value as i64);
    }
}

impl<P: TxPlatform> CoreInit<P> {
    /// Reactor-submission hook body. Captured by
    /// [`Self::install_reactor_submit_seam`] as a plain `fn` pointer
    /// (the platform parameter `P` is monomorphised at install time
    /// so the resulting fn pointer is parameter-free).
    pub(crate) fn submit_child_thread_into_boot_reactor(
        _child_process: Cap<tx_subsystems::process::ProcessIdentity>,
        child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) -> tx_subsystems::reactor_submit::SubmitChildThreadStatus {
        let submit_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        if Self::submit_child_thread_now(submit_cpu, child_thread.clone()) {
            bench_child_tick::<P>("direct", &BENCH_CHILD_DIRECT, 256);
            return tx_subsystems::reactor_submit::SubmitChildThreadStatus::Published;
        }
        Self::queue_pending_child_submit(submit_cpu, child_thread);
        tx_subsystems::reactor_submit::SubmitChildThreadStatus::QueuedFallback
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

    pub(super) fn unregister_thread_reactor_task(task: boot_runtime::TaskKey) {
        THREAD_REACTOR_TASKS
            .lock()
            .retain(|(_, existing)| *existing != task);
    }

    fn thread_reactor_task(tid: u32) -> Option<boot_runtime::TaskKey> {
        THREAD_REACTOR_TASKS
            .lock()
            .iter()
            .find(|(existing_tid, _)| *existing_tid == tid)
            .map(|(_, task)| *task)
    }

    pub(super) fn drain_terminal_thread_reactor_tasks() -> bool {
        let Some(drained) = BOOT_REACTOR.with(|reactor| {
            let mut drained = alloc::vec::Vec::new();
            drained.extend(
                reactor
                    .drain_completed()
                    .into_iter()
                    .map(|record| record.handle),
            );
            drained.extend(
                reactor
                    .drain_cancelled()
                    .into_iter()
                    .map(|record| record.handle),
            );
            drained
        }) else {
            return false;
        };
        Self::finish_terminal_thread_reactor_drain(&drained)
    }

    fn finish_terminal_thread_reactor_drain(drained: &[boot_runtime::TaskKey]) -> bool {
        if drained.is_empty() {
            return false;
        }
        emit_child_submit_marker("debug.child_submit.terminal_drain", drained.len() as i64);

        let mut tasks = THREAD_REACTOR_TASKS.lock();
        let before = tasks.len();
        tasks.retain(|(_, task)| !drained.iter().any(|drained_task| drained_task == task));
        let removed = before.saturating_sub(tasks.len());
        if removed != 0 {
            bench_child_add::<P>(
                "terminal_drained",
                &BENCH_CHILD_TERMINAL_DRAIN,
                removed as u64,
                256,
            );
        }
        drop(tasks);

        if removed != 0 && step_engine::borrow_current_guard().is_none() {
            let first = step_engine::drain_with_budget(TERMINAL_THREAD_EBR_DRAIN_BUDGET);
            let second = step_engine::drain_with_budget(TERMINAL_THREAD_EBR_DRAIN_BUDGET);
            emit_child_submit_marker(
                "debug.child_submit.ebr_reclaimed",
                first
                    .bag_reclaimed
                    .saturating_add(second.bag_reclaimed)
                    .saturating_add(first.publication_dropped)
                    .saturating_add(second.publication_dropped) as i64,
            );
            emit_child_submit_marker(
                "debug.child_submit.ebr_remaining",
                second
                    .bag_remaining
                    .saturating_add(second.publication_remaining) as i64,
            );
            crate::zones::try_bounded_maintenance_tick();
            crate::zones::try_bounded_maintenance_tick();
        } else if removed != 0 {
            emit_child_submit_marker(
                "debug.child_submit.ebr_deferred_active_guard",
                removed as i64,
            );
        }
        removed != 0
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
        bench_child_tick::<P>("queued", &BENCH_CHILD_QUEUE, 256);
    }

    pub(super) fn drain_pending_child_submits() -> bool {
        let mut submitted_any = false;
        loop {
            let next = PENDING_CHILD_SUBMITS.lock().pop();
            let Some(pending) = next else {
                break;
            };
            bench_child_tick::<P>("drain", &BENCH_CHILD_DRAIN, 256);
            if Self::submit_child_thread_now(pending.submit_cpu, pending.child_thread) {
                submitted_any = true;
            }
        }
        submitted_any
    }

    fn submit_child_thread_now(
        submit_cpu: CpuId,
        child_thread: Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) -> bool {
        let total_start = clone_path_clock_now();
        emit_child_submit_marker("debug.child_submit.enter", child_thread.tid.0 as i64);
        let payload_lookup_start = clone_path_clock_now();
        let Some(payload) = child_thread.payload_cap() else {
            emit_clone_path_duration(
                b"debug.clone_path.child_submit.payload_lookup_ns",
                payload_lookup_start,
            );
            bench_child_tick::<P>("payload_missing", &BENCH_CHILD_PAYLOAD_MISSING, 1);
            return false;
        };
        emit_clone_path_duration(
            b"debug.clone_path.child_submit.payload_lookup_ns",
            payload_lookup_start,
        );
        emit_child_submit_marker(
            "debug.child_submit.payload.after",
            child_thread.tid.0 as i64,
        );
        let payload_clone_start = clone_path_clock_now();
        let task_payload = payload.clone();
        emit_clone_path_duration(
            b"debug.clone_path.child_submit.payload_clone_ns",
            payload_clone_start,
        );
        emit_child_submit_marker(
            "debug.child_submit.payload_clone.after",
            child_thread.tid.0 as i64,
        );
        let submit_hart = boot_runtime::HartId(submit_cpu.0);
        let child_tid = child_thread.tid.0;
        let mut signal = SmpRescheduleSignal::<P>::new();
        let mut drained = alloc::vec::Vec::new();
        emit_child_submit_marker("debug.child_submit.reactor.with.before", child_tid as i64);
        let reactor_with_start = clone_path_clock_now();
        let may_drain_terminal = step_engine::borrow_current_guard().is_none();
        let initial_meta = {
            let spread_child = Self::spread_child_submit_for_current_process(&child_thread);
            if spread_child {
                // The explicit scheduler witness lane still wants the freshly
                // submitted child to be placed aggressively so it can measure
                // cross-hart child publication.  Ordinary OSComp clones should
                // not enter through the Preempted queue, though: the parent is
                // still the thread that must receive the clone return and keep
                // the test harness moving.  Putting every child ahead of the
                // parent can starve the parent on SMP4 clone-heavy prefixes.
                Self::userspace_child_thread_sched_meta_for(submit_cpu)
                    .spread_on_submit()
                    .preempted_on_submit()
            } else {
                Self::userspace_thread_sched_meta_for(submit_cpu)
            }
        };
        let submitted = BOOT_REACTOR.with(|reactor| {
            if may_drain_terminal {
                // Concurrent userspace polling can run an entire hot pthread
                // batch inside one reactor step. Reclaim terminal children in
                // the same reactor critical section as submission so the task
                // table can reuse slots without an extra reactor lock
                // round-trip. When child submission runs inside an existing
                // EBR guard, defer this drain: dropping terminal futures may
                // run destructors that need Weak upgrades, and nested guards
                // are forbidden by the epoch substrate.
                drained.extend(
                    reactor
                        .drain_completed()
                        .into_iter()
                        .map(|record| record.handle),
                );
                drained.extend(
                    reactor
                        .drain_cancelled()
                        .into_iter()
                        .map(|record| record.handle),
                );
            } else {
                emit_child_submit_marker(
                    "debug.child_submit.terminal_drain_deferred",
                    child_tid as i64,
                );
            }
            emit_child_submit_marker("debug.child_submit.submit_call.before", child_tid as i64);
            reactor.submit_task_publish_ack(
                crate::thread_future::PerHartSlotted::<P, _>::new(
                    child_thread.clone(),
                    task_payload.clone(),
                    crate::thread_future::run_thread::<P>(child_thread, task_payload),
                ),
                initial_meta,
                submit_hart,
                &mut signal,
            )
        });
        emit_clone_path_duration(
            b"debug.clone_path.child_submit.reactor_with_ns",
            reactor_with_start,
        );
        emit_child_submit_marker("debug.child_submit.reactor.with.after", child_tid as i64);
        let terminal_drain_start = clone_path_clock_now();
        let _ = Self::finish_terminal_thread_reactor_drain(&drained);
        emit_clone_path_duration(
            b"debug.clone_path.child_submit.terminal_drain_ns",
            terminal_drain_start,
        );
        if let Some(report) = submitted {
            let task_key = report.task;
            Self::emit_smp_witness_child_submit(
                submit_hart,
                report.publish.hart,
                report.publish.queue,
                report.dispatch.placements,
                report.dispatch.remote_ipis,
                report.dispatch.local_reschedules,
            );
            emit_child_submit_marker(
                "debug.child_submit.publish.queue",
                phase1_queue_code(report.publish.queue),
            );
            emit_child_submit_marker(
                "debug.child_submit.publish.hart",
                report.publish.hart.0 as i64,
            );
            emit_child_submit_marker(
                "debug.child_submit.publish.dispatch",
                (report.dispatch.placements as i64)
                    | ((report.dispatch.remote_ipis as i64) << 16)
                    | ((report.dispatch.local_reschedules as i64) << 32),
            );
            bench_child_tick::<P>("submitted", &BENCH_CHILD_SUBMIT, 256);
            let register_start = clone_path_clock_now();
            Self::register_thread_reactor_task(child_tid, task_key);
            emit_clone_path_duration(
                b"debug.clone_path.child_submit.register_task_ns",
                register_start,
            );
            emit_child_submit_marker("debug.child_submit.register.after", child_tid as i64);
            emit_clone_path_count(b"debug.clone_path.child_submit.count", 1);
            emit_clone_path_duration(b"debug.clone_path.child_submit.total_ns", total_start);
            true
        } else {
            emit_clone_path_count(b"debug.clone_path.child_submit.not_submitted", 1);
            emit_clone_path_duration(b"debug.clone_path.child_submit.total_ns", total_start);
            false
        }
    }

    fn spread_child_submit_for_current_process(
        child_thread: &Cap<tx_subsystems::thread_runtime::ThreadIdentity>,
    ) -> bool {
        if !cfg!(any(
            tx_userspace_child_spread_smp1,
            tx_userspace_child_spread_smp4
        )) {
            return false;
        }
        let Some(owner) = child_thread.upgrade_owner_proc() else {
            return false;
        };
        let Some(leader) = owner.nth_thread(0) else {
            return false;
        };
        if leader.tid == child_thread.tid {
            return false;
        }
        let Some(cmdline) = owner.ident_cmdline() else {
            return false;
        };
        cmdline
            .windows(b"smp-scheduler-witness".len())
            .any(|window| window == b"smp-scheduler-witness")
    }

    fn emit_smp_witness_child_submit(
        submit_hart: boot_runtime::HartId,
        publish_hart: boot_runtime::HartId,
        queue: boot_runtime::Phase1QueueKind,
        placements: usize,
        remote_ipis: usize,
        local_reschedules: usize,
    ) {
        if !cfg!(tx_smp_scheduler_witness) {
            let _ = (
                submit_hart,
                publish_hart,
                queue,
                placements,
                remote_ipis,
                local_reschedules,
            );
            return;
        }
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":sched-witness:child-submit:submit=");
        Self::write_signed_decimal(submit_hart.0 as i32);
        tx_hal::console_write_str::<P>(":publish=");
        Self::write_signed_decimal(publish_hart.0 as i32);
        tx_hal::console_write_str::<P>(":queue=");
        Self::write_signed_decimal(phase1_queue_code(queue) as i32);
        tx_hal::console_write_str::<P>(":placements=");
        Self::write_decimal_unsigned(placements);
        tx_hal::console_write_str::<P>(":remote-ipis=");
        Self::write_decimal_unsigned(remote_ipis);
        tx_hal::console_write_str::<P>(":local-reschedules=");
        Self::write_decimal_unsigned(local_reschedules);
        tx_hal::console_write_str::<P>("\n");
    }
}

fn emit_child_submit_marker(name: &str, value: i64) {
    if !cfg!(tx_thread_lifecycle_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name.as_bytes(), value);
        tx_observe::dump_registered_if_requested();
    }
}

fn phase1_queue_code(queue: boot_runtime::Phase1QueueKind) -> i64 {
    match queue {
        boot_runtime::Phase1QueueKind::Kernel => 1,
        boot_runtime::Phase1QueueKind::Boosted => 2,
        boot_runtime::Phase1QueueKind::New => 3,
        boot_runtime::Phase1QueueKind::Preempted => 4,
    }
}

fn bench_child_tick<P: TxPlatform>(phase: &str, counter: &AtomicU64, every: u64) {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n % every == 0 {
        write_bench_child_count::<P>(phase, n);
    }
}

fn bench_child_add<P: TxPlatform>(phase: &str, counter: &AtomicU64, amount: u64, every: u64) {
    if amount == 0 {
        return;
    }
    let prev = counter.fetch_add(amount, Ordering::Relaxed);
    let n = prev.saturating_add(amount);
    if n <= 8 || prev == 0 || n / every != prev / every {
        write_bench_child_count::<P>(phase, n);
    }
}

fn write_bench_child_count<P: TxPlatform>(phase: &str, n: u64) {
    // Bench/debug trace of reactor child-thread submission counts. Off by
    // default to keep oscomp/test serial output clean; flip the cfg to debug
    // child-submission scheduling.
    if !cfg!(tx_thread_roundtrip_metrics) {
        let _ = (phase, n);
        return;
    }
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":bench:child_submit:");
    tx_hal::console_write_str::<P>(phase);
    tx_hal::console_write_str::<P>("=");
    write_dec::<P>(n);
    tx_hal::console_write_str::<P>("\n");
}

fn write_dec<P: TxPlatform>(mut value: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if value == 0 {
        tx_hal::console_write_str::<P>("0");
        return;
    }
    while value != 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    if let Ok(s) = core::str::from_utf8(&buf[i..]) {
        tx_hal::console_write_str::<P>(s);
    }
}
