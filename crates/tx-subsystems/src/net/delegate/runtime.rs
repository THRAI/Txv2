use smoltcp::time::Instant;
use tx_reactor::wait::WaitOutcome;
use tx_substrate::zone::PayloadCap;

use crate::execution::{Guard, StepOutcome};
use crate::net::execution::{
    step_flush_pending_arp, step_process_device_tx_pending_in_namespace_at,
    step_process_loopback_pending_in_namespace, step_process_network_events_in_namespace_at,
    step_process_network_tick_in_namespace, step_process_network_tick_loopback_in_namespace,
    ArpFlushOutcome, DeviceTxBudget, DeviceTxOutcome, LoopbackPendingOutcome, LoopbackPollBudget,
    ARP_FLUSH_BUDGET_DEFAULT,
};
use crate::net::namespace::{
    drive_all_net_namespace_runtimes_at, initial_net_namespace_payload, NetNamespacePayload,
};
use crate::net::packet::{PacketSource, PacketTxSink};
use crate::net::protocol::{EtherIface, LoopbackIface};
use crate::wait_source;

use super::{
    net_delegate_clear, net_delegate_kick_poll, net_delegate_queue, net_delegate_wait_token,
    DelegateWireSet,
};

pub trait NetDelegateDriver {
    fn now(&self) -> Instant;
    fn packet_source(&self) -> &dyn PacketSource;

    fn net_namespace(&self) -> PayloadCap<NetNamespacePayload> {
        initial_net_namespace_payload()
    }

    fn packet_tx_sink(&self) -> Option<&dyn PacketTxSink> {
        None
    }

    fn ether_iface(&self) -> Option<&EtherIface> {
        None
    }

    fn arp_flush_budget(&self) -> usize {
        ARP_FLUSH_BUDGET_DEFAULT
    }

    fn device_tx_budget(&self) -> DeviceTxBudget {
        DeviceTxBudget::default()
    }

    fn loopback_iface(&self) -> Option<&LoopbackIface> {
        None
    }

    fn loopback_budget(&self) -> LoopbackPollBudget {
        LoopbackPollBudget::default()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetDelegateRuntimeOutcome {
    pub poll_seen: bool,
    pub tick_seen: bool,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub wakes_fired: usize,
    pub backlog_retransmitted: usize,
    pub backlog_expired: usize,
    pub backlog_failed: usize,
    pub device_tx: DeviceTxOutcome,
    pub arp_flush: ArpFlushOutcome,
    pub loopback: LoopbackPendingOutcome,
    pub namespace_runtime: crate::net::namespace::NetNamespaceRuntimeOutcome,
    pub next_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetDelegateTaskConfig {
    pub max_ready_steps: Option<usize>,
}

impl NetDelegateTaskConfig {
    pub const fn run_forever() -> Self {
        Self {
            max_ready_steps: None,
        }
    }

    pub const fn run_steps(max_ready_steps: usize) -> Self {
        Self {
            max_ready_steps: Some(max_ready_steps),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetDelegateTaskReport {
    pub ready_steps: usize,
    pub waits_ready: usize,
    pub waits_failed: usize,
    pub runtime: NetDelegateRuntimeOutcome,
    pub last_deadline: Option<Instant>,
}

pub async fn net_delegate_task_loop(
    driver: &dyn NetDelegateDriver,
    config: NetDelegateTaskConfig,
) -> NetDelegateTaskReport {
    net_delegate_task_loop_with_deadline_hook(driver, config, |_| {}).await
}

pub async fn net_delegate_task_loop_owned<D>(
    driver: D,
    config: NetDelegateTaskConfig,
) -> NetDelegateTaskReport
where
    D: NetDelegateDriver + Send + 'static,
{
    net_delegate_task_loop_owned_with_deadline_hook(driver, config, |_| {}).await
}

pub async fn net_delegate_task_loop_owned_with_deadline_hook<D, H>(
    driver: D,
    config: NetDelegateTaskConfig,
    mut on_deadline: H,
) -> NetDelegateTaskReport
where
    D: NetDelegateDriver + Send + 'static,
    H: FnMut(Option<Instant>) + Send + 'static,
{
    let mut report = NetDelegateTaskReport::default();
    while config
        .max_ready_steps
        .is_none_or(|max_ready_steps| report.ready_steps < max_ready_steps)
    {
        let Some(wait) = wait_source::wait_on_token(net_delegate_wait_token()) else {
            report.waits_failed += 1;
            break;
        };
        if wait.await != WaitOutcome::Ready {
            report.waits_failed += 1;
            break;
        }

        report.waits_ready += 1;
        report.ready_steps += 1;
        let guard = tx_substrate::epoch::guard();
        let outcome = net_delegate_step_once(&driver, &guard);
        report.runtime.merge(outcome);
        report.last_deadline = outcome.next_deadline;
        on_deadline(outcome.next_deadline);
    }

    report
}

pub async fn net_delegate_task_loop_with_deadline_hook(
    driver: &dyn NetDelegateDriver,
    config: NetDelegateTaskConfig,
    mut on_deadline: impl FnMut(Option<Instant>),
) -> NetDelegateTaskReport {
    let mut report = NetDelegateTaskReport::default();
    while config
        .max_ready_steps
        .is_none_or(|max_ready_steps| report.ready_steps < max_ready_steps)
    {
        let Some(wait) = wait_source::wait_on_token(net_delegate_wait_token()) else {
            report.waits_failed += 1;
            break;
        };
        if wait.await != WaitOutcome::Ready {
            report.waits_failed += 1;
            break;
        }

        report.waits_ready += 1;
        report.ready_steps += 1;
        let guard = tx_substrate::epoch::guard();
        let outcome = net_delegate_step_once(driver, &guard);
        report.runtime.merge(outcome);
        report.last_deadline = outcome.next_deadline;
        on_deadline(outcome.next_deadline);
    }

    report
}

pub fn net_delegate_step_once(
    driver: &dyn NetDelegateDriver,
    guard: &Guard<'_>,
) -> NetDelegateRuntimeOutcome {
    let ready = net_delegate_queue().peek();
    let poll_seen = ready & DelegateWireSet::POLL.bits() != 0;
    let tick_seen = ready & DelegateWireSet::TICK.bits() != 0;
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    let mut outcome = NetDelegateRuntimeOutcome {
        poll_seen,
        tick_seen,
        ..NetDelegateRuntimeOutcome::default()
    };
    let net_namespace = driver.net_namespace();

    if poll_seen {
        let StepOutcome::Done(events) = step_process_network_events_in_namespace_at(
            driver.packet_source(),
            net_namespace.clone(),
            driver.now(),
            guard,
        ) else {
            return outcome;
        };
        outcome.packets_seen += events.packets_seen;
        outcome.sockets_touched += events.sockets_touched;
        outcome.wakes_fired += events.wakes_fired;
        outcome.backlog_expired += events.backlog.half_open_expired;
        outcome.backlog_failed += events.backlog.half_open_failed;
        outcome.next_deadline =
            earliest_deadline(outcome.next_deadline, events.backlog.next_deadline);

        if let Some(iface) = driver.loopback_iface() {
            let StepOutcome::Done(loopback) = step_process_loopback_pending_in_namespace(
                driver.now(),
                net_namespace.clone(),
                iface,
                driver.loopback_budget(),
                guard,
            ) else {
                return outcome;
            };
            outcome.loopback.merge(loopback);
            outcome.packets_seen += loopback.packets_seen;
            outcome.sockets_touched += loopback.sockets_touched;
            outcome.wakes_fired += loopback.wakes_fired;
        }

        if let Some(sink) = driver.packet_tx_sink() {
            let StepOutcome::Done(device_tx) = step_process_device_tx_pending_in_namespace_at(
                sink,
                net_namespace.clone(),
                driver.now(),
                driver.device_tx_budget(),
                guard,
            ) else {
                return outcome;
            };
            outcome.device_tx.merge(device_tx);
            outcome.sockets_touched += device_tx.sockets_touched;
            outcome.wakes_fired += device_tx.wakes_fired;
            if device_tx.tcp_packets != 0
                || device_tx.udp_packets != 0
                || device_tx.raw_icmp_packets != 0
            {
                outcome.wakes_fired += net_delegate_kick_poll();
            }
        }

        if let Some(iface) = driver.ether_iface() {
            let StepOutcome::Done(arp_flush) =
                step_flush_pending_arp(iface, driver.now(), driver.arp_flush_budget(), guard)
            else {
                return outcome;
            };
            outcome.arp_flush = arp_flush;
            if arp_flush.sent != 0 {
                outcome.wakes_fired += net_delegate_kick_poll();
            }
        }

        let namespace_runtime = drive_all_net_namespace_runtimes_at(driver.now(), guard);
        if namespace_runtime.made_progress() {
            outcome.wakes_fired += net_delegate_kick_poll();
        }
        outcome.sockets_touched += namespace_runtime.sockets_touched;
        outcome.wakes_fired += namespace_runtime.wakes_fired;
        outcome.namespace_runtime.merge(namespace_runtime);
    }

    if tick_seen {
        let tick = match driver.loopback_iface() {
            Some(iface) => step_process_network_tick_loopback_in_namespace(
                driver.now(),
                net_namespace,
                iface,
                guard,
            ),
            None => step_process_network_tick_in_namespace(driver.now(), net_namespace, guard),
        };
        let StepOutcome::Done(tick) = tick else {
            return outcome;
        };
        outcome.backlog_retransmitted += tick.half_open_retransmitted;
        outcome.backlog_expired += tick.half_open_expired;
        outcome.backlog_failed += tick.half_open_failed;
        outcome.next_deadline = earliest_deadline(outcome.next_deadline, tick.next_deadline);
    }

    outcome
}

impl NetDelegateRuntimeOutcome {
    fn merge(&mut self, other: Self) {
        self.poll_seen |= other.poll_seen;
        self.tick_seen |= other.tick_seen;
        self.packets_seen += other.packets_seen;
        self.sockets_touched += other.sockets_touched;
        self.wakes_fired += other.wakes_fired;
        self.backlog_retransmitted += other.backlog_retransmitted;
        self.backlog_expired += other.backlog_expired;
        self.backlog_failed += other.backlog_failed;
        self.device_tx.merge(other.device_tx);
        self.arp_flush.attempted += other.arp_flush.attempted;
        self.arp_flush.sent += other.arp_flush.sent;
        self.arp_flush.busy += other.arp_flush.busy;
        self.arp_flush.failed += other.arp_flush.failed;
        self.arp_flush.tx_bytes += other.arp_flush.tx_bytes;
        self.arp_flush.remaining = other.arp_flush.remaining;
        self.loopback.merge(other.loopback);
        self.namespace_runtime.merge(other.namespace_runtime);
        self.next_deadline = earliest_deadline(self.next_deadline, other.next_deadline);
    }
}

fn earliest_deadline(current: Option<Instant>, candidate: Option<Instant>) -> Option<Instant> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.min(candidate)),
        (Some(current), None) => Some(current),
        (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}
