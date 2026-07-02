// Boot-time network delegate wiring for `CoreInit<P>`.
//
// The protocol and socket semantics remain in `tx-subsystems::net`; this
// module only owns the kernel boot decision to submit a long-lived delegate
// future to the boot reactor.

use alloc::boxed::Box;
use core::marker::PhantomData;

use smoltcp::time::Instant;
use tx_hal::TxPlatform;
use tx_substrate::SpinMutex;
use tx_subsystems::net::delegate::{
    net_delegate_kick_tick, net_delegate_task_loop_owned_with_deadline_hook, NetDelegateDriver,
    NetDelegateSupervisor, NetDelegateTaskConfig, NetDelegateTimerArm, NetDelegateTimerWake,
};
#[cfg(test)]
use tx_subsystems::net::device::VIRTIO_NET0_DEVICE;
use tx_subsystems::net::device::{
    net_device_by_name, net_device_snapshot, VIRTIO_NET0_REGISTRATION,
};
use tx_subsystems::net::execution::{DeviceTxBudget, LoopbackPollBudget};
use tx_subsystems::net::packet::{
    PacketDispatch, PacketSource, PacketTxReadiness, PacketTxResult, PacketTxSink,
};
use tx_subsystems::net::protocol::{EtherIface, IfaceCommon, LoopbackIface};
use tx_subsystems::net::structure::Ipv4Address;
use tx_subsystems::net::{
    initial_loopback_iface, initial_net_namespace_payload, NetAdminAuthority,
};

use super::{CoreInit, BOOT_REACTOR};

const DEADLINE_UPDATED: tx_reactor::wait::Mask = tx_reactor::wait::Mask::from_bits(0x1);
const BOOT_ETH_IPV4: Ipv4Address = Ipv4Address::new([10, 0, 2, 15]);
const BOOT_ETH_NETMASK: Ipv4Address = Ipv4Address::new([255, 255, 255, 0]);
const BOOT_ETH_GATEWAY: Ipv4Address = Ipv4Address::new([10, 0, 2, 2]);
const BOOT_ETH_NAME: &str = "eth0";

static BOOT_NET_RUNTIME: SpinMutex<Option<&'static BootNetRuntime>> = SpinMutex::new(None);

struct BootNetDeadlineState {
    supervisor: NetDelegateSupervisor,
}

struct BootNetRuntime {
    ether_iface: EtherIface,
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
        let netdev = boot_net_registration();
        publish_boot_net_device_to_namespace(netdev);
        Self {
            ether_iface: EtherIface::new(
                netdev,
                IfaceCommon::with_gateway(
                    BOOT_ETH_IPV4,
                    BOOT_ETH_NETMASK,
                    Some(BOOT_ETH_GATEWAY),
                    netdev.ops.mtu(),
                ),
                netdev.ops.mac_addr(),
                BOOT_ETH_NAME,
            ),
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

fn boot_net_registration() -> &'static tx_subsystems::net::device::NetDeviceRegistration {
    net_device_by_name(b"eth0")
        .or_else(|| net_device_snapshot().into_iter().next())
        .unwrap_or(&VIRTIO_NET0_REGISTRATION)
}

fn publish_boot_net_device_to_namespace(
    registration: &'static tx_subsystems::net::device::NetDeviceRegistration,
) {
    let namespace = initial_net_namespace_payload();
    let authority = NetAdminAuthority::for_test_or_bootstrap();
    if let Some(link) = namespace
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == registration.name)
    {
        let _ = namespace.set_device_ipv4_addr_by_ifindex(
            authority,
            link.ifindex,
            Some(BOOT_ETH_IPV4),
            Some(24),
        );
        return;
    }
    let _ = namespace.attach_device_for_test_or_bootstrap(registration, Some(BOOT_ETH_IPV4));
}

impl PacketSource for BootNetRuntime {
    fn next_packet(&self) -> Option<PacketDispatch> {
        let frame = self.ether_iface.netdev.ops.receive()?;
        Some(self.ether_iface.process_frame_at(
            frame,
            Instant::ZERO,
            Option::<&tx_subsystems::execution::Guard<'_>>::None,
        ))
    }

    fn next_packet_at(
        &self,
        now: Instant,
        guard: &tx_subsystems::execution::Guard<'_>,
    ) -> Option<PacketDispatch> {
        let frame = self.ether_iface.netdev.ops.receive()?;
        Some(self.ether_iface.process_frame_at(frame, now, Some(guard)))
    }
}

impl PacketTxSink for BootNetRuntime {
    fn readiness(&self, guard: &tx_subsystems::execution::Guard<'_>) -> PacketTxReadiness {
        self.ether_iface.netdev.ops.tx_readiness(guard)
    }

    fn readiness_at(
        &self,
        _now: Instant,
        guard: &tx_subsystems::execution::Guard<'_>,
    ) -> PacketTxReadiness {
        self.readiness(guard)
    }

    fn source_ipv4(&self) -> Option<Ipv4Address> {
        Some(BOOT_ETH_IPV4)
    }

    fn transmit(
        &self,
        frame: &[u8],
        guard: &tx_subsystems::execution::Guard<'_>,
    ) -> PacketTxResult {
        self.ether_iface.dispatch_ip_at(frame, Instant::ZERO, guard)
    }

    fn transmit_at(
        &self,
        frame: &[u8],
        now: Instant,
        guard: &tx_subsystems::execution::Guard<'_>,
    ) -> PacketTxResult {
        self.ether_iface.dispatch_ip_at(frame, now, guard)
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
        let micros = P::read_ns() / 1_000;
        Instant::from_micros(micros.min(i64::MAX as u64) as i64)
    }

    fn packet_source(&self) -> &dyn PacketSource {
        self.runtime
    }

    fn packet_tx_sink(&self) -> Option<&dyn PacketTxSink> {
        Some(self.runtime)
    }

    fn ether_iface(&self) -> Option<&EtherIface> {
        Some(&self.runtime.ether_iface)
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
                            let _ = runtime.refresh_delegate_deadline(next_deadline);
                        },
                    )
                    .await;
                },
                tx_reactor::InitialSchedMeta::kernel()
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
                    .with_affinity(tx_hal::CpuMask::single(current_cpu).bits()),
            )
        })
    }

    fn init_boot_net_runtime() -> Option<&'static BootNetRuntime> {
        if let Some(runtime) = *BOOT_NET_RUNTIME.lock() {
            return Some(runtime);
        }

        let now_ns = P::read_ns();
        let smoltcp_now = instant_from_ns(now_ns);
        let runtime = BOOT_REACTOR.with(|reactor| {
            let runtime: &'static BootNetRuntime = Box::leak(Box::new(BootNetRuntime::new(
                reactor.channel(),
                smoltcp_now,
                now_ns,
            )));
            runtime
        })?;

        // Static ARP for the SLIRP gateway (10.0.2.2 -> 52:55:0a:00:02:02):
        // dynamic ARP replies are learned by the per-namespace device iface,
        // not this one, so without this the SYN is dropped pending resolution
        // and re-ARPs forever. Harmless for loopback-only boots (never routed).
        runtime.ether_iface.install_static_arp(
            BOOT_ETH_GATEWAY,
            tx_subsystems::net::EthernetAddress::new([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]),
        );

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
    VIRTIO_NET0_DEVICE.clear_for_test_or_bootstrap();
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
                }
            }
            tx_reactor::wait::WaitOutcome::Ready
            | tx_reactor::wait::WaitOutcome::Interrupted
            | tx_reactor::wait::WaitOutcome::Killed => {}
        }
    }
}
