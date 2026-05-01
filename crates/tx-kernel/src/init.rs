use core::{
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
};

use tx_hal::{BootHandoff, CpuId, CpuMask, IpiKind, TxPlatform};

const AP_REACTOR_WAIT_SPINS: usize = 100_000;

static BOOT_REACTOR: tx_reactor::SharedReactor = tx_reactor::SharedReactor::empty();
static AP_REACTOR_TASK_DONE_CPUS: AtomicU64 = AtomicU64::new(0);
static BSP_REACTOR_TIMER_DONE_CPUS: AtomicU64 = AtomicU64::new(0);

struct SmpRescheduleSignal<P: TxPlatform> {
    _platform: PhantomData<P>,
}

impl<P: TxPlatform> SmpRescheduleSignal<P> {
    const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }
}

impl<P: TxPlatform> tx_reactor::RescheduleSignal for SmpRescheduleSignal<P> {
    fn send_reschedule_ipi(&mut self, target_hart: tx_reactor::HartId) {
        <P as tx_hal::SmpIf>::send_ipi(CpuId(target_hart.0), IpiKind::Reschedule);
    }
}

/// Skeleton H3 boot spine for the generic kernel mainline.
///
/// This type names the ordering that used to live inline in `kernel_main`:
/// platform early init, substrate bring-up when the board supports it,
/// platform later init, full kernel trap vector installation, a tiny reactor
/// smoke task, the board boot sentinel, and shutdown. VFS, device, scheduler,
/// process, and userspace init are intentionally deferred until their
/// substrate contracts exist.
pub struct CoreInit<P: TxPlatform> {
    _platform: PhantomData<P>,
}

impl<P: TxPlatform> CoreInit<P> {
    pub fn boot(handoff: BootHandoff) -> ! {
        Self::init_early(handoff);
        Self::init_substrate_if_ready(handoff);
        Self::boot_sentinel();
        P::system_off()
    }

    fn init_early(handoff: BootHandoff) {
        P::init_early(handoff);
    }

    fn init_substrate_if_ready(handoff: BootHandoff) {
        if P::SUBSTRATE_BOOT_READY {
            tx_substrate::init::<P>();
            Self::init_later(handoff);
            Self::install_kernel_trap_vector();
            Self::init_boot_reactor();
            Self::boot_secondary_cpus();
            Self::run_smp_shootdown_smoke();
            Self::run_smp_ipi_smoke();
            Self::run_reactor_dispatcher_smoke();
            Self::run_zone_smoke();
            Self::run_bsp_reactor_runtime_smoke();
            Self::run_bsp_reactor_timer_idle_smoke();

            // Deferred H4 spine slots:
            // - post-substrate init hooks
            // - VFS before device init
            // - post-device init hooks
            // - scheduler/process/userspace init
            //
            // Keep these as explicit placeholders until the named subsystems
            // have concrete no_std initialization contracts.
        }
    }

    fn init_later(handoff: BootHandoff) {
        P::init_later(handoff);
    }

    fn install_kernel_trap_vector() {
        P::install_kernel_trap_vector();
    }

    fn init_boot_reactor() {
        let _ = BOOT_REACTOR.init();
    }

    fn boot_secondary_cpus() {
        let started = P::boot_secondary_cpus(Self::secondary_cpu_entry);
        if started > 0 {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":smp:aps:online\n");
        }
    }

    fn run_smp_shootdown_smoke() {
        if P::online_cpu_count() <= 1 {
            return;
        }

        P::shootdown_kernel_mapping(tx_hal::PmapInvalidation::new(tx_hal::VirtAddr(0), 4096));
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":smp:shootdown:ok\n");
    }

    fn run_smp_ipi_smoke() {
        let targets = CpuMask::from_bits(
            P::online_cpus().bits()
                & !CpuMask::single(<P as tx_hal::SmpIf>::current_cpu_id()).bits(),
        );
        if targets.is_empty() {
            return;
        }

        P::clear_ipi_ack_cpus(IpiKind::Reschedule, targets);
        P::broadcast_ipi(targets, IpiKind::Reschedule);
        let acked = P::wait_for_ipi_ack_cpus(targets, IpiKind::Reschedule);
        assert_eq!(acked, targets.count(), "SMP IPI smoke acknowledgements");

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":smp:ipi:ok\n");
    }

    fn run_reactor_dispatcher_smoke() {
        let Some(target_cpu) = Self::first_remote_online_cpu() else {
            return;
        };

        let target_hart = tx_reactor::HartId(target_cpu.0);
        let current_hart = tx_reactor::HartId(<P as tx_hal::SmpIf>::current_cpu_id().0);
        let mask = tx_reactor::wait::Mask::from_bits(0x1);
        let targets = CpuMask::single(target_cpu);
        Self::clear_ap_reactor_task_done(targets);

        let channel = BOOT_REACTOR
            .with(|reactor| {
                let channel = reactor.channel();
                reactor.submit_task_with_meta(
                    {
                        let channel = channel.clone();
                        async move {
                            let _ = channel.wait(mask).await;
                            Self::mark_ap_reactor_task_done(target_cpu);
                        }
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(target_cpu).bits()),
                );
                channel
            })
            .expect("boot reactor must be initialized before AP dispatcher smoke");

        let parked = BOOT_REACTOR
            .with(|reactor| reactor.run_until_idle_on_hart(target_hart))
            .expect("boot reactor must be initialized before AP dispatcher smoke");
        assert_eq!(parked.polled, 1, "reactor dispatcher smoke initial poll");

        P::clear_ipi_ack_cpus(IpiKind::Reschedule, targets);
        assert_eq!(
            channel.fire(mask),
            1,
            "reactor dispatcher wake registration"
        );

        let mut signal = SmpRescheduleSignal::<P>::new();
        let report = BOOT_REACTOR
            .with(|reactor| reactor.drain_wakes_for_hart(current_hart, &mut signal))
            .expect("boot reactor must be initialized before AP dispatcher smoke");
        assert_eq!(report.remote_ipis, 1, "reactor dispatcher remote IPI count");

        let acked = P::wait_for_ipi_ack_cpus(targets, IpiKind::Reschedule);
        assert_eq!(acked, targets.count(), "reactor dispatcher IPI ack");

        let ran = Self::wait_for_ap_reactor_task_done(targets);
        assert_eq!(ran, targets.count(), "reactor AP loop work completion");

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:dispatch:ipi:ok\n");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:ap-loop:ok\n");
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:ap-runqueue:ok\n");
    }

    fn first_remote_online_cpu() -> Option<CpuId> {
        let online = P::online_cpus();
        let current = <P as tx_hal::SmpIf>::current_cpu_id();
        let mut cpu = 0;
        while cpu < u64::BITS as usize {
            let candidate = CpuId(cpu);
            if candidate != current && online.contains(candidate) {
                return Some(candidate);
            }
            cpu += 1;
        }
        None
    }

    unsafe extern "C" fn secondary_cpu_entry(cpu_id: usize) -> ! {
        let cpu_id = CpuId(cpu_id);
        P::install_early_percpu(cpu_id);
        P::init_early_secondary(cpu_id);
        tx_substrate::init_on_ap(cpu_id).expect("tx_kernel AP substrate initialization failed");
        P::init_later_secondary(cpu_id);
        P::install_kernel_trap_vector();
        P::mark_cpu_online(cpu_id);
        Self::secondary_reactor_loop(cpu_id)
    }

    fn secondary_reactor_loop(cpu_id: CpuId) -> ! {
        P::enable_ipi_wakeups();
        P::enable_timer_wakeups();
        loop {
            if Self::run_secondary_reactor_once(cpu_id) {
                continue;
            }

            P::wait_for_interrupt_once();
            if P::pending_ipi(IpiKind::Reschedule) {
                P::ack_ipi(IpiKind::Reschedule);
            }
        }
    }

    fn run_secondary_reactor_once(cpu_id: CpuId) -> bool {
        Self::step_boot_reactor_once(cpu_id).is_some_and(|step| !step.should_idle())
    }

    fn step_boot_reactor_once(cpu_id: CpuId) -> Option<tx_reactor::hart_loop::HartLoopStep> {
        let hart = tx_reactor::HartId(cpu_id.0);
        let now_ns = P::read_ns();
        let mut signal = SmpRescheduleSignal::<P>::new();
        let step = BOOT_REACTOR.with(|reactor| {
            tx_reactor::hart_loop::step_hart_loop_at(reactor, hart, now_ns, &mut signal)
        })?;
        Self::program_hart_loop_deadline(step.deadline_action);
        Some(step)
    }

    fn program_hart_loop_deadline(action: tx_reactor::hart_loop::HartLoopDeadlineAction) {
        match action {
            tx_reactor::hart_loop::HartLoopDeadlineAction::Arm { deadline_ns } => {
                P::set_deadline_ns(deadline_ns)
            }
            tx_reactor::hart_loop::HartLoopDeadlineAction::Cancel => P::cancel_deadline(),
        }
    }

    fn clear_ap_reactor_task_done(cpus: CpuMask) {
        let bits = cpus.bits();
        AP_REACTOR_TASK_DONE_CPUS.fetch_and(!bits, Ordering::AcqRel);
    }

    fn mark_ap_reactor_task_done(cpu_id: CpuId) {
        let bit = Self::cpu_bit(cpu_id);
        if bit == 0 {
            return;
        }

        AP_REACTOR_TASK_DONE_CPUS.fetch_or(bit, Ordering::Release);
    }

    fn wait_for_ap_reactor_task_done(cpus: CpuMask) -> usize {
        let target = cpus.bits();
        for _ in 0..AP_REACTOR_WAIT_SPINS {
            let done = AP_REACTOR_TASK_DONE_CPUS.load(Ordering::Acquire) & target;
            if done == target {
                return cpus.count();
            }
            core::hint::spin_loop();
        }

        (AP_REACTOR_TASK_DONE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
    }

    fn cpu_bit(cpu_id: CpuId) -> u64 {
        CpuMask::single(cpu_id).bits()
    }

    fn run_zone_smoke() {
        crate::zones::run_smoke::<P>().expect("tx_kernel zone smoke failed");
    }

    fn run_bsp_reactor_runtime_smoke() {
        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        BOOT_REACTOR
            .with(|reactor| {
                reactor.submit_task_with_meta(
                    async {
                        Self::write_board_sentinel_prefix();
                        tx_hal::console_write_str::<P>(":reactor:task:ok\n");
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(current_cpu).bits()),
                );
            })
            .expect("boot reactor must be initialized before BSP runtime smoke");

        let step =
            Self::step_boot_reactor_once(current_cpu).expect("boot reactor runtime step failed");
        assert_eq!(step.stats.polled, 1, "BSP runtime smoke task poll count");
        assert_eq!(
            step.stats.completed, 1,
            "BSP runtime smoke task completion count"
        );

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":reactor:runtime-loop:ok\n");
    }

    fn run_bsp_reactor_timer_idle_smoke() {
        const TIMER_SMOKE_DELTA_NS: u64 = 5_000_000;

        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        let cpu_bit = Self::cpu_bit(current_cpu);
        if cpu_bit == 0 {
            return;
        }

        BSP_REACTOR_TIMER_DONE_CPUS.fetch_and(!cpu_bit, Ordering::AcqRel);
        let deadline_ns = P::read_ns().saturating_add(TIMER_SMOKE_DELTA_NS);

        BOOT_REACTOR
            .with(|reactor| {
                let channel = reactor.channel();
                let mask = tx_reactor::wait::Mask::from_bits(0x1);
                reactor.submit_task_with_meta(
                    async move {
                        let outcome = channel
                            .wait_event(
                                mask,
                                tx_reactor::wait::WaitProtocol::InterruptibleTimeout(deadline_ns),
                                || false,
                            )
                            .await;
                        assert_eq!(outcome, tx_reactor::wait::WaitOutcome::TimedOut);
                        BSP_REACTOR_TIMER_DONE_CPUS.fetch_or(cpu_bit, Ordering::Release);
                    },
                    tx_reactor::InitialSchedMeta::kernel()
                        .with_affinity(CpuMask::single(current_cpu).bits()),
                );
            })
            .expect("boot reactor must be initialized before BSP timer smoke");

        let armed =
            Self::step_boot_reactor_once(current_cpu).expect("boot reactor timer arm step failed");
        assert_eq!(
            armed.next_deadline_ns,
            Some(deadline_ns),
            "BSP timer smoke deadline"
        );

        P::enable_timer_wakeups();
        for _ in 0..AP_REACTOR_WAIT_SPINS {
            P::wait_for_interrupt_once();
            let step = Self::step_boot_reactor_once(current_cpu)
                .expect("boot reactor timer idle step failed");
            if Self::bsp_timer_smoke_done(cpu_bit) {
                assert!(step.observed_timer_wakes(), "BSP timer smoke wake");
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":reactor:timer-idle:ok\n");
                return;
            }
        }

        panic!("BSP reactor timer idle smoke did not complete");
    }

    fn bsp_timer_smoke_done(cpu_bit: u64) -> bool {
        BSP_REACTOR_TIMER_DONE_CPUS.load(Ordering::Acquire) & cpu_bit != 0
    }

    fn boot_sentinel() {
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":boot:ok\n");
    }

    fn write_board_sentinel_prefix() {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
    }
}
