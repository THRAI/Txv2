use tx_hal::{
    CpuId, FaultInfo, IpiKind, IrqHandled, KernelTrapSink, PercpuIf, SmpIf, TrapAction,
    TrapFrameMut, TxPlatform, VirtAddr,
};
use tx_services::time::{platform::HalDeadlineTimer, CurrentHartDeadlineTimer};
use tx_shims::linux_syscall::numbers::{
    NR_BRK, NR_FCNTL, NR_FSTAT, NR_LSEEK, NR_MMAP, NR_READ, NR_RT_SIGPROCMASK, NR_SET_TID_ADDRESS,
    NR_WRITE,
};
use tx_shims::linux_syscall::SyscallResult;

use crate::{adapter::boot_runtime, trap_handoff};

pub struct KernelTrapDispatcher;

/// Retry window for a scheduling deadline that fired in supervisor mode.
/// The durable per-hart marker is the correctness mechanism; this short re-arm
/// closes the final race between the entry-boundary check and `sret`.
const DEFERRED_USER_PREEMPT_RETRY_NS: u64 = 1_000_000;

impl<P: TxPlatform> KernelTrapSink<P> for KernelTrapDispatcher {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        // Kernel-mode page faults: keep the existing terminate
        // policy; only user-mode faults can be handed off to a
        // userspace-run wait.
        if !fault.from_user {
            log_kernel_fault_context::<P>(<P as PercpuIf>::current_cpu_id().0);
            return TrapAction::Terminate;
        }

        // Phase 1: snapshot context, resolve the userspace-run
        // wait, and reschedule. Per the Trio plan's "Cross-cutting
        // risks #1" Plan B writeback discipline, no trap-frame
        // mutation happens here.
        let info = trap_handoff::translate_user_pf::<P>(&view.view(), &fault);
        let hart = <P as PercpuIf>::current_cpu_id().0;
        // Keep VM/page-cache work out of this handler. Architecture trap
        // entry runs on a 64 KiB per-hart stack; the resident-file fast path
        // is retried by `thread_future` after this handoff on the ordinary
        // 512 KiB kernel stack.
        let outcome = trap_handoff::hand_off_user_pf(hart, &view, info);
        let action = trap_handoff::outcome_to_trap_action(&outcome);
        if matches!(action, TrapAction::Terminate) {
            log_page_fault_handoff_failure::<P>(hart, &outcome);
        }
        action
    }

    fn on_syscall(mut view: TrapFrameMut<'_>) -> TrapAction {
        // Phase 1: translate, snapshot context into the active
        // payload, resolve the userspace-run wait, and reschedule.
        // No trap-frame writeback (Plan B); the userspace-entry
        // shim drains `pending_syscall_return` and writes
        // `set_syscall_return` / `set_syscall_error` into the
        // *fresh* trap frame before `enter_userspace`.
        let req = trap_handoff::translate_syscall::<P>(&view.view());
        #[cfg(feature = "syscall-profile")]
        let profile_start = <P as tx_hal::TimeIf>::read_ns();
        #[cfg(feature = "syscall-profile")]
        let profile_compiler = record_profiled_syscall::<P>(&req);
        #[cfg(feature = "syscall-profile")]
        if profile_compiler {
            store_profiled_trap_start::<P>(req.nr, profile_start);
        }
        if let Some(action) = try_direct_trap_syscall::<P>(&mut view, &req) {
            #[cfg(feature = "syscall-profile")]
            if profile_compiler {
                record_profiled_syscall_duration::<P>(
                    req.nr,
                    <P as tx_hal::TimeIf>::read_ns().saturating_sub(profile_start),
                    true,
                );
                clear_profiled_trap_start::<P>();
            }
            return action;
        }
        emit_debug_counter(b"debug.trap.syscall", req.nr as i64);
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_syscall(hart, &view, req);
        trap_handoff::outcome_to_trap_action(&outcome)
    }

    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        HalDeadlineTimer::<P>::new().cancel_deadline();
        if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
            // A supervisor-mode expiry may have left a fallback bit just
            // before this user trap. This trap is now the authoritative
            // preemption, so avoid an extra empty yield on the next entry.
            let deferred = crate::init::take_deferred_userspace_preempt(cpu);
            // A lone compute-bound userspace task does not need to traverse
            // the complete trap-handoff/reactor/requeue path on every tick.
            // Preserve the normal handoff for a deferred expiry, runnable
            // competition, pending wake, or an already-due reactor timer.
            if !deferred && crate::init::try_extend_uncontended_userspace_slice::<P>(cpu) {
                return TrapAction::Resume;
            }
            emit_debug_counter(b"debug.trap.timer_user", view.view().pc.0 as i64);
            let hart = <P as PercpuIf>::current_cpu_id().0;
            let outcome = trap_handoff::hand_off_timer_preempt(hart, &view);
            if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                crate::init::mark_boot_reactor_userspace_preempt(cpu);
            }
            return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
        }

        // Ordinary kernel/reactor deadlines are consumed here and the outer
        // reactor loop decides the next arm.  A scheduling deadline may,
        // however, expire while a userspace-thread Future is still doing its
        // entry-side kernel work.  Dropping that expiry lets the Future enter
        // userspace with no timer; on one hart, a process spinning at a shared
        // start barrier can then prevent every sibling from ever running.
        //
        // The per-hart marker is lock-free because an interrupt here may have
        // interrupted the payload-slot update itself.  It also narrows the RV
        // fallback to actual userspace polls, so LA does not reintroduce the
        // old idle/reactor one-shot interrupt loop.
        if !crate::init::userspace_thread_poll_active(cpu) {
            return TrapAction::Resume;
        }

        // The userspace slice is armed before polling the thread Future.
        // Under TCG, entry-side VM/signal work can outlive a short slice, so
        // the interrupt may arrive in supervisor mode. Merely cancelling it
        // here lets this poll later enter user mode without any preemption.
        // Retain the expired slice and re-arm once to cover the final
        // check-to-sret race.
        crate::init::defer_userspace_preempt(cpu);
        let retry_deadline =
            <P as tx_hal::TimeIf>::read_ns().saturating_add(DEFERRED_USER_PREEMPT_RETRY_NS);
        HalDeadlineTimer::<P>::new().set_current_hart_deadline_ns(retry_deadline);

        TrapAction::Resume
    }

    fn on_external_irq(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        let irq = P::claim();
        if irq == 0 {
            return TrapAction::Resume;
        }

        let handled = crate::irq::dispatch_external_irq::<P>(irq);
        // Most handlers finish their controller transaction in the trap.
        // A deferred handler keeps the claim outstanding so a task-context
        // bottom half can clear a level-triggered device source first. That
        // bottom half owns the one matching same-context completion.
        if !matches!(handled, IrqHandled::DeferredWake) {
            P::complete(irq);
        }

        match handled {
            IrqHandled::Wake | IrqHandled::DeferredWake => {
                // A from-user reschedule longjmps out of the board trap shell.
                // Preserve the interrupted userspace run in its hart slot
                // before requesting that jump, exactly as the timer path does.
                if view.view().previous_mode == tx_hal::TrapPreviousMode::User {
                    let outcome = trap_handoff::hand_off_timer_preempt(cpu.0, &view);
                    if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                        crate::init::mark_boot_reactor_userspace_preempt(cpu);
                    }
                    return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
                }
                TrapAction::Reschedule
            }
            IrqHandled::Done | IrqHandled::NotMine => TrapAction::Resume,
        }
    }

    fn on_ipi(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction {
        let stop = P::pending_ipi(IpiKind::Stop);
        let maintenance = P::pending_ipi(IpiKind::Maintenance);
        let reschedule = P::pending_ipi(IpiKind::Reschedule);
        if stop {
            P::ack_ipi(IpiKind::Stop);
        }
        if maintenance {
            P::ack_ipi(IpiKind::Maintenance);
        }
        if P::pending_ipi(IpiKind::Membarrier) {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            P::ack_ipi(IpiKind::Membarrier);
        }
        if reschedule {
            P::ack_ipi(IpiKind::Reschedule);
        }
        if P::pending_ipi(IpiKind::TlbShootdown) {
            P::ack_ipi(IpiKind::TlbShootdown);
        }

        if (stop || maintenance || reschedule)
            && view.view().previous_mode == tx_hal::TrapPreviousMode::User
        {
            let outcome = trap_handoff::hand_off_timer_preempt(cpu.0, &view);
            if matches!(outcome, trap_handoff::TimerPreemptOutcome::Preempted) {
                crate::init::mark_boot_reactor_userspace_preempt(cpu);
            }
            return trap_handoff::timer_preempt_outcome_to_trap_action(&outcome);
        }

        if stop || maintenance {
            TrapAction::Reschedule
        } else {
            TrapAction::Resume
        }
    }

    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        // Kernel-mode illegal/sync faults are genuinely fatal: terminate
        // (the board maps this to a kernel panic, which is correct — a
        // kernel bug should stop the world).
        if !fault.from_user {
            log_kernel_fault_context::<P>(<P as PercpuIf>::current_cpu_id().0);
            return TrapAction::Terminate;
        }

        // User-mode illegal instruction / unrecoverable sync fault: deliver
        // a fatal signal to the offending thread and reschedule, exactly
        // like an unrecoverable user page fault (see `on_page_fault`). A
        // crashing *user* process must never panic the *kernel*; otherwise
        // a single bad user instruction (e.g. a test whose context is
        // corrupted on signal return) tears down the whole run instead of
        // just that one process.
        let hart = <P as PercpuIf>::current_cpu_id().0;
        let outcome = trap_handoff::hand_off_user_fatal(hart, &view, 0, fault.address.0 as u64);
        let action = trap_handoff::outcome_to_trap_action(&outcome);
        if matches!(action, TrapAction::Terminate) {
            log_page_fault_handoff_failure::<P>(hart, &outcome);
        }
        action
    }

    fn on_illegal_instruction(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction {
        if !fault.from_user {
            log_kernel_fault_context::<P>(<P as PercpuIf>::current_cpu_id().0);
            return TrapAction::Terminate;
        }

        let hart = <P as PercpuIf>::current_cpu_id().0;
        // RISC-V scause=2 is Illegal Instruction. The semantic kind is also
        // carried separately, so upper layers do not depend on this raw value.
        let raw_cause = if matches!(P::ARCH, tx_hal::Arch::Riscv64) {
            2
        } else {
            0
        };
        let outcome = trap_handoff::hand_off_user_illegal_instruction(
            hart,
            &view,
            raw_cause,
            fault.address.0 as u64,
        );
        let action = trap_handoff::outcome_to_trap_action(&outcome);
        if matches!(action, TrapAction::Terminate) {
            log_page_fault_handoff_failure::<P>(hart, &outcome);
        }
        action
    }
}

/// Temporary opt-in syscall mix profiler. Normal release builds do not carry
/// the counters or the process-name lookup. Enable only with
/// the `syscall-profile` feature for a diagnostic kernel.
#[cfg(feature = "syscall-profile")]
const PROFILE_NR_COUNT: usize = 512;
#[cfg(feature = "syscall-profile")]
static PROFILE_COUNTS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_TOTAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "syscall-profile")]
static PROFILE_NEXT_DUMP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1_000);
#[cfg(feature = "syscall-profile")]
static PROFILE_TIMED_COUNTS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_TIMED_NS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_TIMED_MAX_NS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_DIRECT_COUNTS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_DIRECT_NS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_VM_PAGES: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_VM_MAX_PAGES: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_TIMED_TOTAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "syscall-profile")]
static PROFILE_TIMED_NEXT_DUMP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(1_000);
#[cfg(feature = "syscall-profile")]
const PROFILE_HART_COUNT: usize = 64;
#[cfg(feature = "syscall-profile")]
static PROFILE_TRAP_START_NS: [core::sync::atomic::AtomicU64; PROFILE_HART_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_HART_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_TRAP_NR: [core::sync::atomic::AtomicU64; PROFILE_HART_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; PROFILE_HART_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_HANDOFF_COUNTS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_HANDOFF_NS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];
#[cfg(feature = "syscall-profile")]
static PROFILE_HANDOFF_MAX_NS: [core::sync::atomic::AtomicU64; PROFILE_NR_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROFILE_NR_COUNT];

#[cfg(feature = "syscall-profile")]
fn record_profiled_syscall<P: TxPlatform>(req: &boot_runtime::userspace::SyscallRequest) -> bool {
    use core::sync::atomic::Ordering;

    let hart = <P as PercpuIf>::current_cpu_id().0;
    let Some(thread) = tx_subsystems::thread_runtime::current_thread_identity(hart) else {
        return false;
    };
    let Some(process) = thread.upgrade_owner_proc() else {
        return false;
    };
    let comm = process.comm();
    let comm_len = comm
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(comm.len());
    let comm = &comm[..comm_len];
    if comm != b"cargo" && comm != b"tg-xtask" && comm != b"rustc" && comm != b"axbuild" {
        return false;
    }

    if let Some(counter) = PROFILE_COUNTS.get(req.nr as usize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    if matches!(req.nr, 215 | 222 | 226) {
        let pages = (req.args[1] as u64).div_ceil(4096);
        if let Some(total) = PROFILE_VM_PAGES.get(req.nr as usize) {
            total.fetch_add(pages, Ordering::Relaxed);
        }
        if let Some(maximum) = PROFILE_VM_MAX_PAGES.get(req.nr as usize) {
            maximum.fetch_max(pages, Ordering::Relaxed);
        }
    }
    let total = PROFILE_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    let threshold = PROFILE_NEXT_DUMP.load(Ordering::Relaxed);
    if total != threshold {
        return true;
    }
    let next = match threshold {
        1_000 => 5_000,
        5_000 => 20_000,
        20_000 => 50_000,
        _ => threshold.saturating_add(50_000),
    };
    if PROFILE_NEXT_DUMP
        .compare_exchange(threshold, next, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return true;
    }

    tx_hal::console_write_str::<P>("txkernel:syscall-profile:total=");
    write_u64::<P>(total);
    tx_hal::console_write_str::<P>("\n");
    for (profile_nr, counter) in PROFILE_COUNTS.iter().enumerate() {
        let count = counter.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        tx_hal::console_write_str::<P>("txkernel:syscall-profile:nr=");
        write_usize::<P>(profile_nr);
        tx_hal::console_write_str::<P>(":count=");
        write_u64::<P>(count);
        let pages = PROFILE_VM_PAGES[profile_nr].load(Ordering::Relaxed);
        if pages != 0 {
            tx_hal::console_write_str::<P>(":pages=");
            write_u64::<P>(pages);
            tx_hal::console_write_str::<P>(":max_pages=");
            write_u64::<P>(PROFILE_VM_MAX_PAGES[profile_nr].load(Ordering::Relaxed));
        }
        tx_hal::console_write_str::<P>("\n");
    }
    true
}

#[cfg(feature = "syscall-profile")]
fn store_profiled_trap_start<P: TxPlatform>(nr: u64, start_ns: u64) {
    use core::sync::atomic::Ordering;

    let hart = <P as PercpuIf>::current_cpu_id().0;
    if hart >= PROFILE_HART_COUNT {
        return;
    }
    PROFILE_TRAP_NR[hart].store(nr, Ordering::Relaxed);
    PROFILE_TRAP_START_NS[hart].store(start_ns, Ordering::Release);
}

#[cfg(feature = "syscall-profile")]
fn clear_profiled_trap_start<P: TxPlatform>() {
    use core::sync::atomic::Ordering;

    let hart = <P as PercpuIf>::current_cpu_id().0;
    if hart < PROFILE_HART_COUNT {
        PROFILE_TRAP_START_NS[hart].store(0, Ordering::Release);
        PROFILE_TRAP_NR[hart].store(u64::MAX, Ordering::Relaxed);
    }
}

#[cfg(feature = "syscall-profile")]
pub(crate) fn take_profiled_trap_start<P: TxPlatform>(nr: u64) -> Option<u64> {
    use core::sync::atomic::Ordering;

    let hart = <P as PercpuIf>::current_cpu_id().0;
    if hart >= PROFILE_HART_COUNT || PROFILE_TRAP_NR[hart].load(Ordering::Acquire) != nr {
        return None;
    }
    let start = PROFILE_TRAP_START_NS[hart].swap(0, Ordering::AcqRel);
    PROFILE_TRAP_NR[hart].store(u64::MAX, Ordering::Relaxed);
    (start != 0).then_some(start)
}

#[cfg(feature = "syscall-profile")]
pub(crate) fn record_profiled_syscall_handoff(nr: u64, elapsed_ns: u64) {
    use core::sync::atomic::Ordering;

    let Some(count) = PROFILE_HANDOFF_COUNTS.get(nr as usize) else {
        return;
    };
    count.fetch_add(1, Ordering::Relaxed);
    PROFILE_HANDOFF_NS[nr as usize].fetch_add(elapsed_ns, Ordering::Relaxed);
    PROFILE_HANDOFF_MAX_NS[nr as usize].fetch_max(elapsed_ns, Ordering::Relaxed);
}

#[cfg(feature = "syscall-profile")]
pub(crate) fn record_profiled_syscall_duration<P: TxPlatform>(
    nr: u64,
    elapsed_ns: u64,
    direct: bool,
) {
    use core::sync::atomic::Ordering;

    let Some(count) = PROFILE_TIMED_COUNTS.get(nr as usize) else {
        return;
    };
    count.fetch_add(1, Ordering::Relaxed);
    PROFILE_TIMED_NS[nr as usize].fetch_add(elapsed_ns, Ordering::Relaxed);
    PROFILE_TIMED_MAX_NS[nr as usize].fetch_max(elapsed_ns, Ordering::Relaxed);
    if direct {
        PROFILE_DIRECT_COUNTS[nr as usize].fetch_add(1, Ordering::Relaxed);
        PROFILE_DIRECT_NS[nr as usize].fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    let total = PROFILE_TIMED_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    let threshold = PROFILE_TIMED_NEXT_DUMP.load(Ordering::Relaxed);
    if total != threshold {
        return;
    }
    let next = match threshold {
        1_000 => 5_000,
        5_000 => 20_000,
        20_000 => 50_000,
        _ => threshold.saturating_add(50_000),
    };
    if PROFILE_TIMED_NEXT_DUMP
        .compare_exchange(threshold, next, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    tx_hal::console_write_str::<P>("txkernel:syscall-latency:total=");
    write_u64::<P>(total);
    tx_hal::console_write_str::<P>("\n");
    for profile_nr in 0..PROFILE_NR_COUNT {
        let count = PROFILE_TIMED_COUNTS[profile_nr].load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        tx_hal::console_write_str::<P>("txkernel:syscall-latency:nr=");
        write_usize::<P>(profile_nr);
        tx_hal::console_write_str::<P>(":count=");
        write_u64::<P>(count);
        tx_hal::console_write_str::<P>(":total_ns=");
        write_u64::<P>(PROFILE_TIMED_NS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":max_ns=");
        write_u64::<P>(PROFILE_TIMED_MAX_NS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":direct_count=");
        write_u64::<P>(PROFILE_DIRECT_COUNTS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":direct_ns=");
        write_u64::<P>(PROFILE_DIRECT_NS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":handoff_count=");
        write_u64::<P>(PROFILE_HANDOFF_COUNTS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":handoff_ns=");
        write_u64::<P>(PROFILE_HANDOFF_NS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>(":handoff_max_ns=");
        write_u64::<P>(PROFILE_HANDOFF_MAX_NS[profile_nr].load(Ordering::Relaxed));
        tx_hal::console_write_str::<P>("\n");
    }
}

fn dispatch_pending_ipis<P: SmpIf>() -> TrapAction {
    let mut action = TrapAction::Resume;
    if P::pending_ipi(IpiKind::Stop) {
        P::ack_ipi(IpiKind::Stop);
        action = TrapAction::Reschedule;
    }
    if P::pending_ipi(IpiKind::Maintenance) {
        P::ack_ipi(IpiKind::Maintenance);
        action = TrapAction::Reschedule;
    }
    if P::pending_ipi(IpiKind::Membarrier) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        P::ack_ipi(IpiKind::Membarrier);
    }
    if P::pending_ipi(IpiKind::Reschedule) {
        P::ack_ipi(IpiKind::Reschedule);
    }
    if P::pending_ipi(IpiKind::TlbShootdown) {
        P::ack_ipi(IpiKind::TlbShootdown);
    }
    action
}

#[cfg(test)]
mod ipi_tests {
    use core::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    static MAINTENANCE_PENDING: AtomicBool = AtomicBool::new(false);
    static MAINTENANCE_ACKED: AtomicBool = AtomicBool::new(false);
    static STOP_PENDING: AtomicBool = AtomicBool::new(false);
    static STOP_ACKED: AtomicBool = AtomicBool::new(false);

    struct TestSmp;

    impl SmpIf for TestSmp {
        fn pending_ipi(kind: IpiKind) -> bool {
            kind == IpiKind::Maintenance && MAINTENANCE_PENDING.load(Ordering::Acquire)
        }

        fn ack_ipi(kind: IpiKind) {
            if kind == IpiKind::Maintenance {
                MAINTENANCE_PENDING.store(false, Ordering::Release);
                MAINTENANCE_ACKED.store(true, Ordering::Release);
            }
        }
    }

    struct StopSmp;

    impl SmpIf for StopSmp {
        fn pending_ipi(kind: IpiKind) -> bool {
            kind == IpiKind::Stop && STOP_PENDING.load(Ordering::Acquire)
        }

        fn ack_ipi(kind: IpiKind) {
            if kind == IpiKind::Stop {
                STOP_PENDING.store(false, Ordering::Release);
                STOP_ACKED.store(true, Ordering::Release);
            }
        }
    }

    #[test]
    fn maintenance_ipi_only_acknowledges_and_reschedules() {
        MAINTENANCE_ACKED.store(false, Ordering::Release);
        MAINTENANCE_PENDING.store(true, Ordering::Release);

        let action = dispatch_pending_ipis::<TestSmp>();

        assert_eq!(action, TrapAction::Reschedule);
        assert!(MAINTENANCE_ACKED.load(Ordering::Acquire));
        assert!(!MAINTENANCE_PENDING.load(Ordering::Acquire));
    }

    #[test]
    fn stop_ipi_is_cleared_before_rescheduling() {
        STOP_ACKED.store(false, Ordering::Release);
        STOP_PENDING.store(true, Ordering::Release);

        let action = dispatch_pending_ipis::<StopSmp>();

        assert_eq!(action, TrapAction::Reschedule);
        assert!(STOP_ACKED.load(Ordering::Acquire));
        assert!(!STOP_PENDING.load(Ordering::Acquire));
    }
}

fn try_direct_trap_syscall<P: TxPlatform>(
    view: &mut TrapFrameMut<'_>,
    req: &trap_handoff::SyscallRequest,
) -> Option<TrapAction> {
    // Reject unsupported calls before resolving the current payload, thread,
    // process and address space. Previously every VFS/VM syscall paid all of
    // that direct-lane setup only for the dispatcher to return `None` and run
    // the ordinary async path anyway.
    if !tx_shims::linux_syscall::is_direct_trap_syscall(req.nr)
        || !direct_trap_syscall_is_stack_safe(P::ARCH, req.nr)
    {
        return None;
    }
    let direct_total_start = direct_sigprocmask_detail_now(req.nr);
    let hart = <P as PercpuIf>::current_cpu_id().0;
    let payload_start = direct_sigprocmask_detail_now(req.nr);
    let Some(payload) = tx_subsystems::thread_runtime::current_userspace_payload(hart) else {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_payload",
            1,
        );
        return None;
    };
    if payload.active_userspace_request().is_none() {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_active_request",
            1,
        );
        return None;
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.payload_ns",
        payload_start,
    );

    let context_start = direct_sigprocmask_detail_now(req.nr);
    let thread_lookup_start = direct_sigprocmask_detail_now(req.nr);
    // The trap runs inside `PerHartSlotted::poll`, which keeps the current
    // thread identity installed until the userspace round-trip longjmps back
    // and the wrapped poll returns.  Use that poll-scoped authority instead of
    // maintaining a second userspace identity cache with a wider lifetime.
    let Some(thread) = tx_subsystems::thread_runtime::current_thread_identity(hart) else {
        emit_direct_sigprocmask_detail_value(req.nr, b"debug.trap.direct_sigprocmask.no_thread", 1);
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_thread_ns",
        thread_lookup_start,
    );
    let owner_upgrade_start = direct_sigprocmask_detail_now(req.nr);
    let Some(process) = thread.upgrade_owner_proc() else {
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.no_process",
            1,
        );
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_owner_ns",
        owner_upgrade_start,
    );
    let aspace_start = direct_sigprocmask_detail_now(req.nr);
    let Some(aspace) = process.aspace_cap() else {
        emit_direct_sigprocmask_detail_value(req.nr, b"debug.trap.direct_sigprocmask.no_aspace", 1);
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_aspace_ns",
        aspace_start,
    );
    if !direct_trap_user_buffers_are_resident(req, &aspace) {
        return None;
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.context_ns",
        context_start,
    );

    let precondition_start = direct_sigprocmask_detail_now(req.nr);
    if !direct_syscall_preconditions(req.nr, &payload, &process) {
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.precondition_ns",
            precondition_start,
        );
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.precondition_failed",
            1,
        );
        return None;
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.precondition_ns",
        precondition_start,
    );

    let dispatch_start = direct_sigprocmask_detail_now(req.nr);
    let Some(result) = tx_shims::linux_syscall::dispatch_direct_trap_payload_oneshot::<P>(
        req, &process, &thread, &payload, &aspace,
    ) else {
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.dispatch_ns",
            dispatch_start,
        );
        emit_direct_sigprocmask_detail_value(
            req.nr,
            b"debug.trap.direct_sigprocmask.unsupported",
            1,
        );
        return None;
    };
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.dispatch_ns",
        dispatch_start,
    );

    let post_start = direct_sigprocmask_detail_now(req.nr);
    let needs_reschedule = direct_trap_syscall_needs_wake_handoff(req, &result);

    match result {
        SyscallResult::Return(value) => view.set_syscall_return(value),
        SyscallResult::CloneReturn { value, .. } => view.set_syscall_return(value),
        SyscallResult::Error(errno) => view.set_syscall_error(errno),
        SyscallResult::NoReturn
        | SyscallResult::ExecCommitted
        | SyscallResult::SigreturnRestored
        | SyscallResult::SigreturnContextRestored => return None,
    }

    const RV64_ECALL_INSN_BYTES: usize = 4;
    view.set_pc(VirtAddr(
        view.view().pc.0.wrapping_add(RV64_ECALL_INSN_BYTES),
    ));
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.writeback_ns",
        post_start,
    );
    emit_debug_counter(b"debug.trap.direct_syscall", req.nr as i64);
    if needs_reschedule {
        emit_debug_counter(b"debug.trap.direct_wake_handoff", req.nr as i64);
        let handoff_start = direct_sigprocmask_detail_now(req.nr);
        let outcome = trap_handoff::hand_off_timer_preempt(hart, view);
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.wake_handoff_ns",
            handoff_start,
        );
        emit_direct_sigprocmask_detail_duration(
            req.nr,
            b"debug.trap.direct_sigprocmask.total_ns",
            direct_total_start,
        );
        return Some(trap_handoff::timer_preempt_outcome_to_trap_action(&outcome));
    }
    emit_direct_sigprocmask_detail_duration(
        req.nr,
        b"debug.trap.direct_sigprocmask.total_ns",
        direct_total_start,
    );
    Some(TrapAction::Resume)
}

/// Syscalls that are small enough to finish on an architecture trap stack.
///
/// The shims-level direct dispatcher also exposes cached VFS/VM helpers for
/// ordinary kernel-stack call sites. Do not confuse that synchronous property
/// with trap-stack safety: pathname walking and resident file page faults can
/// form deep call chains even when they do not wait. Keep those proven unsafe
/// operations off the architecture stack. RV64 retains the already validated
/// cached syscall lanes; LA64 continues to hand deeper operations to the
/// ordinary kernel stack.
///
/// Keep this as an allow-list so a newly added direct syscall falls back to
/// the reactor until its maximum stack depth is reviewed explicitly.
const fn direct_trap_syscall_is_stack_safe(arch: tx_hal::Arch, nr: u64) -> bool {
    use tx_shims::linux_syscall::numbers::{
        NR_CLOCK_GETTIME, NR_FUTEX, NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETPID, NR_GETPPID,
        NR_GETTID, NR_GETTIMEOFDAY, NR_GETUID, NR_OPENAT, NR_STATX,
    };

    let shallow = matches!(
        nr,
        NR_GETPPID
            | NR_GETPID
            | NR_GETTID
            | NR_GETUID
            | NR_GETEUID
            | NR_GETGID
            | NR_GETEGID
            | NR_CLOCK_GETTIME
            | NR_GETTIMEOFDAY
            | NR_FUTEX
            | NR_RT_SIGPROCMASK
            | NR_SET_TID_ADDRESS
    );
    if shallow {
        return true;
    }

    // `openat` and resident file faults are the deep paths implicated by the
    // platform failure. The latter is handled after handoff on the ordinary
    // 512 KiB kernel stack. Keep the previously validated resident/cache-hit
    // RV64 syscall lanes direct; LA64 retains its conservative policy.
    matches!(arch, tx_hal::Arch::Riscv64)
        && !matches!(nr, NR_OPENAT)
        && matches!(
            nr,
            NR_READ | NR_WRITE | NR_FCNTL | NR_FSTAT | NR_LSEEK | NR_BRK | NR_MMAP | NR_STATX
        )
}

/// Direct trap calls may only touch user buffers whose translations are
/// already present when their direct implementation can otherwise enter a
/// fault-capable copy path.
fn direct_trap_user_buffers_are_resident(
    req: &trap_handoff::SyscallRequest,
    aspace: &tx_subsystems::vm::AddressSpace,
) -> bool {
    use tx_shims::linux_syscall::numbers::{NR_CLOCK_GETTIME, NR_GETTIMEOFDAY};
    use tx_subsystems::vm::UserAccessKind;

    match req.nr {
        NR_CLOCK_GETTIME => direct_trap_user_range_is_ready(
            aspace,
            req.args[1] as usize,
            2 * core::mem::size_of::<u64>(),
            UserAccessKind::Write,
        ),
        NR_GETTIMEOFDAY => direct_trap_user_range_is_ready(
            aspace,
            req.args[0] as usize,
            2 * core::mem::size_of::<u64>(),
            UserAccessKind::Write,
        ),
        _ => true,
    }
}

fn direct_trap_user_range_is_ready(
    aspace: &tx_subsystems::vm::AddressSpace,
    addr: usize,
    len: usize,
    access: tx_subsystems::vm::UserAccessKind,
) -> bool {
    use tx_subsystems::vm::{UserRange, UserVirtAddr, USER_PAGE_SIZE};

    // Null retains the syscall's normal EFAULT/query semantics without
    // entering a user-copy path. Every non-null buffer must be page-covered.
    if addr == 0 || len == 0 {
        return true;
    }
    let Some(last) = addr.checked_add(len - 1) else {
        return false;
    };
    let start = addr & !(USER_PAGE_SIZE - 1);
    let Some(end) = (last & !(USER_PAGE_SIZE - 1)).checked_add(USER_PAGE_SIZE) else {
        return false;
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr::new(start), end - start) else {
        return false;
    };
    aspace.user_range_is_ready_for_access(range, access)
}

#[cfg(test)]
mod direct_trap_stack_safety_tests {
    use super::direct_trap_syscall_is_stack_safe;
    use tx_hal::Arch;
    use tx_shims::linux_syscall::numbers::{
        NR_BRK, NR_CLOCK_GETTIME, NR_FCNTL, NR_FSTAT, NR_FUTEX, NR_GETPID, NR_LSEEK, NR_MMAP,
        NR_OPENAT, NR_READ, NR_RT_SIGPROCMASK, NR_SET_TID_ADDRESS, NR_STATX, NR_WRITE,
    };

    #[test]
    fn openat_stays_off_every_trap_stack() {
        for arch in [Arch::Riscv64, Arch::LoongArch64] {
            assert!(!direct_trap_syscall_is_stack_safe(arch, NR_OPENAT));
        }
    }

    #[test]
    fn rv64_keeps_validated_cached_lanes() {
        for nr in [
            NR_READ, NR_WRITE, NR_FCNTL, NR_FSTAT, NR_LSEEK, NR_BRK, NR_MMAP, NR_STATX,
        ] {
            assert!(direct_trap_syscall_is_stack_safe(Arch::Riscv64, nr));
            assert!(!direct_trap_syscall_is_stack_safe(Arch::LoongArch64, nr));
        }
    }

    #[test]
    fn shallow_direct_calls_are_portable() {
        for nr in [
            NR_GETPID,
            NR_CLOCK_GETTIME,
            NR_FUTEX,
            NR_RT_SIGPROCMASK,
            NR_SET_TID_ADDRESS,
        ] {
            assert!(direct_trap_syscall_is_stack_safe(Arch::Riscv64, nr));
            assert!(direct_trap_syscall_is_stack_safe(Arch::LoongArch64, nr));
        }
    }
}

pub(crate) fn direct_trap_syscall_needs_wake_handoff(
    req: &trap_handoff::SyscallRequest,
    result: &SyscallResult,
) -> bool {
    crate::thread_future::syscall_return_may_publish_wake_handoff(req, result)
}

fn direct_syscall_preconditions(
    nr: u64,
    payload: &tx_subsystems::thread_runtime::ThreadPayload,
    process: &tx_subsystems::process::ProcessIdentity,
) -> bool {
    use tx_shims::linux_syscall::numbers::{
        NR_CLOCK_GETTIME, NR_GETEGID, NR_GETEUID, NR_GETGID, NR_GETPID, NR_GETTID, NR_GETTIMEOFDAY,
        NR_GETUID,
    };
    match nr {
        NR_RT_SIGPROCMASK => {
            payload.pending().snapshot() == 0
                && process.group_pending_snapshot() == 0
                && payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        NR_SET_TID_ADDRESS => {
            payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        // Direct query lane (getpid-class + clock reads): only bypass the
        // run_thread AST checkpoint when no signal work is pending, so
        // delivery timing is identical to the slow path.
        NR_GETPID
        | NR_GETTID
        | NR_GETUID
        | NR_GETEUID
        | NR_GETGID
        | NR_GETEGID
        | NR_CLOCK_GETTIME
        | NR_GETTIMEOFDAY
        | NR_READ
        | NR_WRITE
        | NR_FCNTL
        | NR_FSTAT
        | NR_LSEEK
        | NR_BRK
        | NR_MMAP
        | tx_shims::linux_syscall::numbers::NR_OPENAT
        | tx_shims::linux_syscall::numbers::NR_STATX => {
            payload.pending().snapshot() == 0
                && process.group_pending_snapshot() == 0
                && payload.interrupt_summary() == tx_subsystems::signal::InterruptSummary::EMPTY
        }
        _ => true,
    }
}

fn emit_debug_counter(name: &[u8], value: i64) {
    if !cfg!(tx_thread_roundtrip_metrics) {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn direct_sigprocmask_detail_now(nr: u64) -> u64 {
    if nr == NR_RT_SIGPROCMASK {
        tx_observe::clock_now_ns()
    } else {
        0
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn direct_sigprocmask_detail_now(_nr: u64) -> u64 {
    0
}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_direct_sigprocmask_detail_duration(nr: u64, name: &[u8], start_ns: u64) {
    if nr != NR_RT_SIGPROCMASK {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        let dur = tx_observe::clock_now_ns().saturating_sub(start_ns);
        observer.debug_counter(name, dur.min(i64::MAX as u64) as i64);
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_direct_sigprocmask_detail_duration(_nr: u64, _name: &[u8], _start_ns: u64) {}

#[cfg(tx_sigprocmask_detail_metrics)]
fn emit_direct_sigprocmask_detail_value(nr: u64, name: &[u8], value: i64) {
    if nr != NR_RT_SIGPROCMASK {
        return;
    }
    if let Some(observer) = tx_observe::current() {
        observer.debug_counter(name, value);
        tx_observe::dump_registered_if_requested();
    }
}

#[cfg(not(tx_sigprocmask_detail_metrics))]
fn emit_direct_sigprocmask_detail_value(_nr: u64, _name: &[u8], _value: i64) {}

fn log_page_fault_handoff_failure<P: TxPlatform>(
    hart: usize,
    outcome: &trap_handoff::HandoffOutcome,
) {
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":trap-handoff:pf:");
    match outcome {
        trap_handoff::HandoffOutcome::Resolved => {
            tx_hal::console_write_str::<P>("resolved");
        }
        trap_handoff::HandoffOutcome::NoActivePayload => {
            tx_hal::console_write_str::<P>("no-active-payload");
        }
        trap_handoff::HandoffOutcome::NoActiveRequest => {
            tx_hal::console_write_str::<P>("no-active-request");
        }
        trap_handoff::HandoffOutcome::SlotError(err) => {
            tx_hal::console_write_str::<P>("slot-error:");
            match err {
                boot_runtime::userspace::UserspaceRunError::Busy(_) => {
                    tx_hal::console_write_str::<P>("busy");
                }
                boot_runtime::userspace::UserspaceRunError::NoActiveRequest => {
                    tx_hal::console_write_str::<P>("no-active-request");
                }
                boot_runtime::userspace::UserspaceRunError::StaleRequest { .. } => {
                    tx_hal::console_write_str::<P>("stale-request");
                }
                boot_runtime::userspace::UserspaceRunError::AlreadyResolved(_) => {
                    tx_hal::console_write_str::<P>("already-resolved");
                }
                boot_runtime::userspace::UserspaceRunError::NotRunning(_) => {
                    tx_hal::console_write_str::<P>("not-running");
                }
                boot_runtime::userspace::UserspaceRunError::RequestIdExhausted => {
                    tx_hal::console_write_str::<P>("request-id-exhausted");
                }
            }
        }
    }
    tx_hal::console_write_str::<P>(":hart=");
    write_usize::<P>(hart);
    let (last_set, last_clear, set_count, clear_count) =
        tx_subsystems::thread_runtime::userspace_payload_trace_counters();
    tx_hal::console_write_str::<P>(":last-set=");
    write_u64::<P>(last_set);
    tx_hal::console_write_str::<P>(":last-clear=");
    write_u64::<P>(last_clear);
    tx_hal::console_write_str::<P>(":sets=");
    write_u64::<P>(set_count);
    tx_hal::console_write_str::<P>(":clears=");
    write_u64::<P>(clear_count);
    tx_hal::console_write_str::<P>("\n");
}

/// Snapshot the task/userspace ownership visible on a hart immediately before
/// a fatal kernel trap is handed back to the board panic path. This is kept
/// allocation-free so it remains useful when the suspected failure is an EBR,
/// vmalloc, or trap-stack lifetime violation.
fn log_kernel_fault_context<P: TxPlatform>(hart: usize) {
    use tx_subsystems::thread_runtime::{
        current_thread_identity, current_thread_payload, current_userspace_payload,
        current_userspace_thread_identity,
    };

    let poll_thread = current_thread_identity(hart);
    let userspace_thread = current_userspace_thread_identity(hart);
    let payload = current_thread_payload(hart).or_else(|| current_userspace_payload(hart));

    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":kernel-fault-context:hart=");
    write_usize::<P>(hart);
    tx_hal::console_write_str::<P>(":poll-thread=");
    write_usize::<P>(usize::from(poll_thread.is_some()));
    tx_hal::console_write_str::<P>(":userspace-thread=");
    write_usize::<P>(usize::from(userspace_thread.is_some()));

    if let Some(thread) = poll_thread.as_ref().or(userspace_thread.as_ref()) {
        tx_hal::console_write_str::<P>(":tid=");
        write_usize::<P>(thread.tid.0 as usize);
        if let Some(process) = thread.upgrade_owner_proc() {
            tx_hal::console_write_str::<P>(":pid=");
            write_usize::<P>(process.pid.0 as usize);
        }
    }

    if let Some(payload) = payload {
        let (pc, ra, sp, tls, syscall, entry_hart) = payload.user_entry_diagnostic();
        tx_hal::console_write_str::<P>(":active-request=");
        write_usize::<P>(usize::from(payload.active_userspace_request().is_some()));
        tx_hal::console_write_str::<P>(":entry-pc=0x");
        write_hex_u64::<P>(pc);
        tx_hal::console_write_str::<P>(":entry-ra=0x");
        write_hex_u64::<P>(ra);
        tx_hal::console_write_str::<P>(":entry-sp=0x");
        write_hex_u64::<P>(sp);
        tx_hal::console_write_str::<P>(":entry-tls=0x");
        write_hex_u64::<P>(tls);
        tx_hal::console_write_str::<P>(":entry-syscall=0x");
        write_hex_u64::<P>(syscall);
        tx_hal::console_write_str::<P>(":entry-hart=");
        write_u64::<P>(entry_hart);
        if let Some((nr, arg0, arg1)) = payload.active_syscall_diagnostic() {
            tx_hal::console_write_str::<P>(":active-syscall=0x");
            write_hex_u64::<P>(nr);
            tx_hal::console_write_str::<P>(":arg0=0x");
            write_hex_u64::<P>(arg0);
            tx_hal::console_write_str::<P>(":arg1=0x");
            write_hex_u64::<P>(arg1);
        }
    }
    if let Some(trace) = tx_reactor::reactor_poll_trace(tx_reactor::HartId(hart)) {
        tx_hal::console_write_str::<P>(":reactor-seq=");
        write_u64::<P>(trace.sequence);
        tx_hal::console_write_str::<P>(":reactor-phase=");
        write_usize::<P>(trace.phase);
        tx_hal::console_write_str::<P>(":reactor-task=");
        write_usize::<P>(trace.task_id);
        tx_hal::console_write_str::<P>(":reactor-generation=");
        write_u64::<P>(trace.generation);
        tx_hal::console_write_str::<P>(":future-data=0x");
        write_hex_u64::<P>(trace.future_data as u64);
        tx_hal::console_write_str::<P>(":future-vtable=0x");
        write_hex_u64::<P>(trace.future_vtable as u64);
    }
    if let Some(trace) = tx_substrate::epoch::reclaim_trace(CpuId(hart)) {
        tx_hal::console_write_str::<P>(":ebr-seq=");
        write_u64::<P>(trace.sequence);
        tx_hal::console_write_str::<P>(":ebr-active=");
        write_usize::<P>(usize::from(trace.active));
        tx_hal::console_write_str::<P>(":ebr-kind=");
        write_usize::<P>(trace.kind);
        tx_hal::console_write_str::<P>(":ebr-object=0x");
        write_hex_u64::<P>(trace.object as u64);
        tx_hal::console_write_str::<P>(":ebr-callback=0x");
        write_hex_u64::<P>(trace.callback as u64);
        tx_hal::console_write_str::<P>(":ebr-next=0x");
        write_hex_u64::<P>(trace.next as u64);
    }
    tx_hal::console_write_str::<P>("\n");
}

pub(crate) fn write_usize<P: TxPlatform>(value: usize) {
    write_u64::<P>(value as u64);
}

pub(crate) fn write_u64<P: TxPlatform>(value: u64) {
    if value == 0 {
        tx_hal::console_write_str::<P>("0");
        return;
    }

    let mut digits = [0u8; 20];
    let mut n = value;
    let mut idx = digits.len();
    while n > 0 {
        idx -= 1;
        digits[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let s = core::str::from_utf8(&digits[idx..]).unwrap_or("");
    tx_hal::console_write_str::<P>(s);
}

fn write_hex_u64<P: TxPlatform>(value: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [b'0'; 16];
    for (index, digit) in digits.iter_mut().enumerate() {
        let shift = (15 - index) * 4;
        *digit = HEX[((value >> shift) & 0xf) as usize];
    }
    let text = core::str::from_utf8(&digits).unwrap_or("");
    tx_hal::console_write_str::<P>(text);
}
