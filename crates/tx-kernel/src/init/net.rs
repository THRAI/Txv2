// Boot-time network delegate wiring for `CoreInit<P>`.
//
// The protocol and socket semantics remain in `tx-subsystems::net`; this
// module only owns the kernel boot decision to submit a long-lived delegate
// future to the boot reactor.

use alloc::boxed::Box;
use core::marker::PhantomData;

use smoltcp::time::Instant;
use tx_hal::TxPlatform;
use tx_services::time::{timekeeper_clock, ClockRead};
use tx_substrate::SpinMutex;
use tx_subsystems::net::delegate::{
    net_delegate_kick_poll, net_delegate_kick_tick,
    net_delegate_task_loop_owned_with_deadline_hook, NetDelegateDriver, NetDelegateSupervisor,
    NetDelegateTaskConfig, NetDelegateTimerArm, NetDelegateTimerWake,
};
use tx_subsystems::net::execution::{DeviceTxBudget, LoopbackPollBudget};
use tx_subsystems::net::initial_loopback_iface;
use tx_subsystems::net::packet::{PacketDispatch, PacketSource};
use tx_subsystems::net::protocol::LoopbackIface;

use super::{CoreInit, BOOT_REACTOR};

const DEADLINE_UPDATED: tx_reactor::wait::Mask = tx_reactor::wait::Mask::from_bits(0x1);

static BOOT_NET_RUNTIME: SpinMutex<Option<&'static BootNetRuntime>> = SpinMutex::new(None);

struct BootNetDeadlineState {
    supervisor: NetDelegateSupervisor,
}

struct BootNetRuntime {
    deadline_channel: tx_reactor::wait::Channel,
    deadline_state: SpinMutex<BootNetDeadlineState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootNetDeadlineRefresh {
    Unchanged,
    Armed(NetDelegateTimerArm),
    Cancelled,
}

struct BootNetDelegateDriver<P: TxPlatform> {
    runtime: &'static BootNetRuntime,
    _platform: PhantomData<fn() -> P>,
}

impl BootNetRuntime {
    fn new(
        deadline_channel: tx_reactor::wait::Channel,
        smoltcp_base: Instant,
        reactor_base_ns: u64,
    ) -> Self {
        Self {
            deadline_channel,
            deadline_state: SpinMutex::new(BootNetDeadlineState {
                supervisor: NetDelegateSupervisor::new(smoltcp_base, reactor_base_ns),
            }),
        }
    }

    fn refresh_delegate_deadline(&self, next_deadline: Option<Instant>) -> BootNetDeadlineRefresh {
        let refresh = {
            let mut state = self.deadline_state.lock();
            let before = state.supervisor.armed();
            let _ = state.supervisor.refresh_deadline(next_deadline);
            let after = state.supervisor.armed();

            if before == after {
                BootNetDeadlineRefresh::Unchanged
            } else if let Some(arm) = after {
                BootNetDeadlineRefresh::Armed(arm)
            } else {
                BootNetDeadlineRefresh::Cancelled
            }
        };

        match refresh {
            BootNetDeadlineRefresh::Unchanged => {}
            BootNetDeadlineRefresh::Armed(arm) => {
                let _ = arm;
                self.deadline_channel.fire(DEADLINE_UPDATED);
            }
            BootNetDeadlineRefresh::Cancelled => {
                self.deadline_channel.fire(DEADLINE_UPDATED);
            }
        }

        refresh
    }

    fn current_deadline_arm(&self) -> Option<NetDelegateTimerArm> {
        self.deadline_state.lock().supervisor.armed()
    }

    fn deadline_generation_changed(&self, generation: u64) -> bool {
        self.current_deadline_arm()
            .is_none_or(|arm| arm.generation != generation)
    }

    fn consume_timer_wake(&self, wake: NetDelegateTimerWake) -> bool {
        self.deadline_state.lock().supervisor.consume_wake(wake)
    }
}

impl PacketSource for BootNetRuntime {
    fn next_packet(&self) -> Option<PacketDispatch> {
        None
    }

    fn next_packet_at(
        &self,
        _now: Instant,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> Option<PacketDispatch> {
        // Physical-device RX is owned by `drive_all_net_namespace_runtimes_at`.
        // Keeping this source empty prevents a private boot iface from draining
        // frames before the namespace's current address/FIB view can see them.
        None
    }
}

impl<P: TxPlatform> BootNetDelegateDriver<P> {
    const fn new(runtime: &'static BootNetRuntime) -> Self {
        Self {
            runtime,
            _platform: PhantomData,
        }
    }
}

impl<P: TxPlatform> NetDelegateDriver for BootNetDelegateDriver<P> {
    fn now(&self) -> Instant {
        // Absorbed from main: the platform monotonic counter is read through
        // the `tx-time` timekeeper now, not `P::read_ns()` directly.
        let micros = timekeeper_clock::<P>().monotonic_now_ns() / 1_000;
        Instant::from_micros(micros.min(i64::MAX as u64) as i64)
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.runtime
    }

    fn device_tx_budget(&self) -> DeviceTxBudget {
        DeviceTxBudget::default()
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        Some(initial_loopback_iface())
    }

    fn loopback_budget(&self) -> LoopbackPollBudget {
        LoopbackPollBudget::default()
    }
}

impl<P: TxPlatform> CoreInit<P> {
    pub(super) fn submit_net_runtime_tasks() {
        let Some(runtime) = Self::init_boot_net_runtime() else {
            return;
        };
        let _ = Self::submit_net_deadline_task(runtime);
        let _ = Self::submit_net_delegate_task_with_config(
            runtime,
            NetDelegateTaskConfig::run_forever(),
        );
    }

    #[cfg(test)]
    pub(crate) fn submit_net_delegate_task_for_test(
        config: NetDelegateTaskConfig,
    ) -> Option<tx_reactor::TaskKey> {
        let runtime = Self::init_boot_net_runtime()?;
        Self::submit_net_delegate_task_with_config(runtime, config)
    }

    #[cfg(test)]
    pub(crate) fn init_boot_net_runtime_for_test() -> bool {
        Self::init_boot_net_runtime().is_some()
    }

    #[cfg(test)]
    pub(crate) fn submit_net_deadline_task_for_test() -> Option<tx_reactor::TaskKey> {
        let runtime = Self::init_boot_net_runtime()?;
        Self::submit_net_deadline_task(runtime)
    }

    #[cfg(test)]
    pub(crate) fn refresh_net_deadline_for_test(
        next_deadline: Option<Instant>,
    ) -> Option<NetDelegateTimerArm> {
        let runtime = Self::init_boot_net_runtime()?;
        let _ = runtime.refresh_delegate_deadline(next_deadline);
        runtime.current_deadline_arm()
    }

    fn submit_net_delegate_task_with_config(
        runtime: &'static BootNetRuntime,
        config: NetDelegateTaskConfig,
    ) -> Option<tx_reactor::TaskKey> {
        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        BOOT_REACTOR.with(|reactor| {
            reactor.submit_task_with_meta(
                async move {
                    let _report = net_delegate_task_loop_owned_with_deadline_hook(
                        BootNetDelegateDriver::<P>::new(runtime),
                        config,
                        move |next_deadline| {
                            // RX watchdog: the real NET_IRQ path is wired on
                            // RV64, but QEMU virtio-mmio has historically
                            // admitted a cold idle RX frame without a usable
                            // interrupt. A quiet established stream (reader
                            // blocked in read(), smoltcp with no pending
                            // timers) can otherwise yield `next_deadline =
                            // None` and sleep forever. Keep a 10 ms backstop
                            // until that transport behavior has a stronger
                            // witness; normal traffic wakes through IRQ first.
                            let micros = timekeeper_clock::<P>().monotonic_now_ns() / 1_000;
                            let now = Instant::from_micros(micros.min(i64::MAX as u64) as i64);
                            let floor = now + smoltcp::time::Duration::from_millis(10);
                            let clamped = Some(match next_deadline {
                                Some(deadline) if deadline < floor => deadline,
                                _ => floor,
                            });
                            let _ = runtime.refresh_delegate_deadline(clamped);
                        },
                    )
                    .await;
                },
                // The network state machine has one protocol owner. Pin both
                // its future and its timer publisher to the same hart so the
                // delegate's wait registration, wake routing, and protocol
                // ownership cannot move independently between polls. User
                // processes remain movable across every online CPU.
                tx_reactor::InitialSchedMeta::kernel()
                    .pinned()
                    .with_affinity(tx_hal::CpuMask::single(current_cpu).bits()),
            )
        })
    }

    fn submit_net_deadline_task(runtime: &'static BootNetRuntime) -> Option<tx_reactor::TaskKey> {
        let current_cpu = <P as tx_hal::SmpIf>::current_cpu_id();
        BOOT_REACTOR.with(|reactor| {
            reactor.submit_task_with_meta(
                boot_net_deadline_task(runtime),
                tx_reactor::InitialSchedMeta::kernel()
                    .pinned()
                    .with_affinity(tx_hal::CpuMask::single(current_cpu).bits()),
            )
        })
    }

    fn init_boot_net_runtime() -> Option<&'static BootNetRuntime> {
        if let Some(runtime) = *BOOT_NET_RUNTIME.lock() {
            return Some(runtime);
        }

        let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
        let smoltcp_now = instant_from_ns(now_ns);
        let runtime = BOOT_REACTOR.with(|reactor| {
            let runtime: &'static BootNetRuntime = Box::leak(Box::new(BootNetRuntime::new(
                reactor.channel(),
                smoltcp_now,
                now_ns,
            )));
            runtime
        })?;

        let mut slot = BOOT_NET_RUNTIME.lock();
        if let Some(existing) = *slot {
            Some(existing)
        } else {
            *slot = Some(runtime);
            Some(runtime)
        }
    }
}

#[cfg(test)]
pub(super) fn reset_boot_net_runtime_for_test() {
    *BOOT_NET_RUNTIME.lock() = None;
}

fn instant_from_ns(ns: u64) -> Instant {
    let micros = ns / 1_000;
    Instant::from_micros(micros.min(i64::MAX as u64) as i64)
}

async fn boot_net_deadline_task(runtime: &'static BootNetRuntime) {
    loop {
        let Some(arm) = runtime.current_deadline_arm() else {
            let _ = runtime.deadline_channel.wait(DEADLINE_UPDATED).await;
            continue;
        };

        let outcome = runtime
            .deadline_channel
            .wait_event(
                DEADLINE_UPDATED,
                tx_reactor::wait::WaitProtocol::InterruptibleTimeout(arm.deadline_ns),
                move || runtime.deadline_generation_changed(arm.generation),
            )
            .await;

        match outcome {
            tx_reactor::wait::WaitOutcome::TimedOut => {
                let wake = NetDelegateTimerWake {
                    generation: arm.generation,
                    outcome,
                    tick_fired: true,
                };
                if runtime.consume_timer_wake(wake) {
                    net_delegate_kick_tick();
                    // Also raise POLL: the whole established-connection
                    // pipeline (device RX read, smoltcp dispatch — and with
                    // it RTO/fast retransmits — device-TX drain) lives in
                    // the delegate's poll_seen branch; the tick branch only
                    // walks the half-open handshake backlog. Without this a
                    // fully quiet wire is fatal to a connection with a lost
                    // segment: no RX ⇒ no POLL ⇒ dispatch never runs ⇒ the
                    // RTO retransmit is never emitted and both ends wait
                    // forever (observed: 17MB git push over slirp wedged
                    // mid-upload; peer stuck at dup-ACK 18121 while we sat
                    // on unacked data at the window edge, silent for 400s+).
                    net_delegate_kick_poll();
                }
            }
            tx_reactor::wait::WaitOutcome::Ready
            | tx_reactor::wait::WaitOutcome::Interrupted
            | tx_reactor::wait::WaitOutcome::Killed => {}
        }
    }
}
