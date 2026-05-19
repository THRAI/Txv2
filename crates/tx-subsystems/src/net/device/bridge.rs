//! Ethernet bridge staging device.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::admin::NetAdminAuthority;
use crate::net::netfilter::{
    run_frame_hook, NetfilterFrameContext, NetfilterHook, NetfilterVerdict,
};
use crate::net::packet::{PacketTxReadiness, RxFrame};
use crate::sync::SpinMutex;

use super::{EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration};

pub const BRIDGE_FORWARD_BUDGET_DEFAULT: usize = 32;

pub struct BridgeDevice {
    name: &'static str,
    mac: EthernetAddress,
    mtu: u16,
    ports: SpinMutex<Vec<BridgePort>>,
    learned: SpinMutex<BTreeMap<EthernetAddress, usize>>,
    local_rx: SpinMutex<VecDeque<RxFrame>>,
    rx_packets: AtomicU64,
    forwarded_packets: AtomicU64,
    flooded_packets: AtomicU64,
    dropped_packets: AtomicU64,
    tx_errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BridgeConfig {
    pub name: &'static str,
    pub devt: DevT,
    pub mac: EthernetAddress,
    pub mtu: u16,
}

#[derive(Clone, Copy)]
pub struct BridgeInstance {
    pub registration: &'static NetDeviceRegistration,
    pub device: &'static BridgeDevice,
}

#[derive(Clone, Copy)]
struct BridgePort {
    registration: &'static NetDeviceRegistration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BridgeForwardOutcome {
    pub frames_seen: usize,
    pub learned: usize,
    pub local_delivered: usize,
    pub forwarded: usize,
    pub flooded: usize,
    pub dropped: usize,
    pub busy: usize,
    pub tx_errors: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgePortSnapshot {
    pub ifindex: u32,
    pub name: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeSnapshot {
    pub name: &'static str,
    pub ports: Vec<BridgePortSnapshot>,
    pub learned_entries: usize,
}

impl BridgeDevice {
    pub fn new(name: &'static str, mac: EthernetAddress, mtu: u16) -> Self {
        Self {
            name,
            mac,
            mtu,
            ports: SpinMutex::new(Vec::new()),
            learned: SpinMutex::new(BTreeMap::new()),
            local_rx: SpinMutex::new(VecDeque::new()),
            rx_packets: AtomicU64::new(0),
            forwarded_packets: AtomicU64::new(0),
            flooded_packets: AtomicU64::new(0),
            dropped_packets: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
        }
    }

    pub fn add_port(
        &self,
        _authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        self.add_port_inner(registration)
    }

    pub fn add_port_for_test_or_bootstrap(
        &self,
        registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        self.add_port_inner(registration)
    }

    pub fn remove_port(
        &self,
        _authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        self.remove_port_inner(registration)
    }

    pub fn poll_once(&self, guard: &Guard<'_>) -> BridgeForwardOutcome {
        self.poll_budget(BRIDGE_FORWARD_BUDGET_DEFAULT, guard)
    }

    pub fn poll_budget(&self, budget: usize, guard: &Guard<'_>) -> BridgeForwardOutcome {
        let ports = self.ports.lock().clone();
        let mut outcome = BridgeForwardOutcome::default();
        let mut remaining = budget;

        for (ingress_idx, port) in ports.iter().copied().enumerate() {
            while remaining != 0 {
                let Some(frame) = port.registration.ops.receive() else {
                    break;
                };
                remaining -= 1;
                self.process_ingress_frame(ingress_idx, &ports, frame, guard, &mut outcome);
            }
        }

        outcome
    }

    pub fn snapshot(&self) -> BridgeSnapshot {
        let ports = self.ports.lock();
        BridgeSnapshot {
            name: self.name,
            ports: ports
                .iter()
                .enumerate()
                .map(|(idx, port)| BridgePortSnapshot {
                    ifindex: (idx as u32) + 1,
                    name: port.registration.name,
                })
                .collect(),
            learned_entries: self.learned.lock().len(),
        }
    }

    pub fn learned_port_name(&self, mac: EthernetAddress) -> Option<&'static str> {
        let idx = *self.learned.lock().get(&mac)?;
        self.ports
            .lock()
            .get(idx)
            .map(|port| port.registration.name)
    }

    fn add_port_inner(&self, registration: &'static NetDeviceRegistration) -> Result<(), Errno> {
        let mut ports = self.ports.lock();
        if registration.name == self.name
            || ports.iter().any(|port| {
                port.registration.name == registration.name
                    || port.registration.devt == registration.devt
            })
        {
            return Err(Errno::EEXIST);
        }
        ports.push(BridgePort { registration });
        Ok(())
    }

    fn remove_port_inner(&self, registration: &'static NetDeviceRegistration) -> Result<(), Errno> {
        let mut ports = self.ports.lock();
        let Some(idx) = ports
            .iter()
            .position(|port| port.registration.devt == registration.devt)
        else {
            return Err(Errno::ENODEV);
        };
        ports.remove(idx);
        self.learned.lock().clear();
        Ok(())
    }

    fn process_ingress_frame(
        &self,
        ingress_idx: usize,
        ports: &[BridgePort],
        frame: RxFrame,
        guard: &Guard<'_>,
        outcome: &mut BridgeForwardOutcome,
    ) {
        outcome.frames_seen += 1;
        self.rx_packets.fetch_add(1, Ordering::Relaxed);

        let Some((dst, src)) = ethernet_addresses(frame.as_bytes()) else {
            outcome.dropped += 1;
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        };

        let ingress_name = ports[ingress_idx].registration.name;
        if run_frame_hook(
            NetfilterFrameContext {
                hook: NetfilterHook::Prerouting,
                bridge: Some(self.name),
                ingress: Some(ingress_name),
                egress: None,
            },
            frame.as_bytes(),
        ) == NetfilterVerdict::Drop
        {
            outcome.dropped += 1;
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        }

        if !src.is_multicast() {
            let mut learned = self.learned.lock();
            if learned.insert(src, ingress_idx) != Some(ingress_idx) {
                outcome.learned += 1;
            }
        }

        let local_delivered =
            self.maybe_deliver_ingress_to_local(dst, ingress_name, frame.as_bytes(), outcome);

        let egresses = self.egress_ports_for_ingress(dst, ingress_idx, ports);
        if egresses.is_empty() {
            if local_delivered {
                return;
            }
            outcome.dropped += 1;
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let flood = dst.is_multicast() || egresses.len() > 1;
        for egress_idx in egresses {
            let egress = ports[egress_idx].registration;
            if run_frame_hook(
                NetfilterFrameContext {
                    hook: NetfilterHook::Forward,
                    bridge: Some(self.name),
                    ingress: Some(ingress_name),
                    egress: Some(egress.name),
                },
                frame.as_bytes(),
            ) == NetfilterVerdict::Drop
            {
                outcome.dropped += 1;
                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if run_frame_hook(
                NetfilterFrameContext {
                    hook: NetfilterHook::Postrouting,
                    bridge: Some(self.name),
                    ingress: Some(ingress_name),
                    egress: Some(egress.name),
                },
                frame.as_bytes(),
            ) == NetfilterVerdict::Drop
            {
                outcome.dropped += 1;
                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            match egress.ops.transmit(frame.as_bytes(), guard) {
                StepOutcome::Done(()) | StepOutcome::Continue { .. } => {
                    outcome.forwarded += 1;
                    self.forwarded_packets.fetch_add(1, Ordering::Relaxed);
                    if flood {
                        outcome.flooded += 1;
                        self.flooded_packets.fetch_add(1, Ordering::Relaxed);
                    }
                }
                StepOutcome::Yield { .. } => {
                    outcome.busy += 1;
                }
                StepOutcome::Err(_) => {
                    outcome.tx_errors += 1;
                    self.tx_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn maybe_deliver_ingress_to_local(
        &self,
        dst: EthernetAddress,
        ingress_name: &'static str,
        frame: &[u8],
        outcome: &mut BridgeForwardOutcome,
    ) -> bool {
        if !self.should_deliver_local(dst) {
            return false;
        }

        if run_frame_hook(
            NetfilterFrameContext {
                hook: NetfilterHook::Input,
                bridge: Some(self.name),
                ingress: Some(ingress_name),
                egress: Some(self.name),
            },
            frame,
        ) == NetfilterVerdict::Drop
        {
            outcome.dropped += 1;
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        self.push_local_rx(frame);
        outcome.local_delivered += 1;
        true
    }

    fn push_local_rx(&self, frame: &[u8]) {
        let mut bytes = Vec::with_capacity(frame.len());
        bytes.extend_from_slice(frame);
        self.local_rx.lock().push_back(RxFrame::new(bytes));
    }

    fn should_deliver_local(&self, dst: EthernetAddress) -> bool {
        dst == self.mac || dst.is_multicast()
    }

    fn egress_ports_for_ingress(
        &self,
        dst: EthernetAddress,
        ingress_idx: usize,
        ports: &[BridgePort],
    ) -> Vec<usize> {
        if dst == self.mac {
            return Vec::new();
        }

        if !dst.is_multicast() {
            if let Some(target_idx) = self.learned.lock().get(&dst).copied() {
                return (target_idx != ingress_idx)
                    .then_some(target_idx)
                    .into_iter()
                    .collect();
            }
        }

        ports
            .iter()
            .enumerate()
            .filter_map(|(idx, _)| (idx != ingress_idx).then_some(idx))
            .collect()
    }

    fn egress_ports_for_local(&self, dst: EthernetAddress, ports: &[BridgePort]) -> Vec<usize> {
        if dst == self.mac {
            return Vec::new();
        }

        if !dst.is_multicast() {
            if let Some(target_idx) = self.learned.lock().get(&dst).copied() {
                return (target_idx < ports.len())
                    .then_some(target_idx)
                    .into_iter()
                    .collect();
            }
        }

        (0..ports.len()).collect()
    }
}

impl NetDeviceOps for BridgeDevice {
    fn receive(&self) -> Option<RxFrame> {
        self.local_rx.lock().pop_front()
    }

    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> StepOutcome<()> {
        if frame.len() < 14 || frame.len() > usize::from(self.mtu) + 14 {
            return StepOutcome::Err(Errno::EINVAL);
        }

        let Some((dst, _src)) = ethernet_addresses(frame) else {
            return StepOutcome::Err(Errno::EINVAL);
        };

        if run_frame_hook(
            NetfilterFrameContext {
                hook: NetfilterHook::Output,
                bridge: Some(self.name),
                ingress: None,
                egress: None,
            },
            frame,
        ) == NetfilterVerdict::Drop
        {
            return StepOutcome::Err(Errno::EACCES);
        }

        let ports = self.ports.lock().clone();
        let egresses = self.egress_ports_for_local(dst, &ports);
        if egresses.is_empty() {
            return StepOutcome::Done(());
        }

        let mut forwarded = 0usize;
        let mut last_errno = None;
        for egress_idx in egresses {
            let egress = ports[egress_idx].registration;
            if run_frame_hook(
                NetfilterFrameContext {
                    hook: NetfilterHook::Postrouting,
                    bridge: Some(self.name),
                    ingress: Some(self.name),
                    egress: Some(egress.name),
                },
                frame,
            ) == NetfilterVerdict::Drop
            {
                continue;
            }

            match egress.ops.transmit(frame, guard) {
                StepOutcome::Done(()) | StepOutcome::Continue { .. } => {
                    forwarded += 1;
                    self.forwarded_packets.fetch_add(1, Ordering::Relaxed);
                    if dst.is_multicast() {
                        self.flooded_packets.fetch_add(1, Ordering::Relaxed);
                    }
                }
                StepOutcome::Yield { progress, shape } => {
                    return StepOutcome::Yield { progress, shape };
                }
                StepOutcome::Err(errno) => {
                    last_errno = Some(errno);
                    self.tx_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if forwarded != 0 {
            StepOutcome::Done(())
        } else {
            StepOutcome::Err(last_errno.unwrap_or(Errno::ENODEV))
        }
    }

    fn tx_readiness(&self, _guard: &Guard<'_>) -> PacketTxReadiness {
        PacketTxReadiness::Ready
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }

    fn device_kind(&self) -> NetDeviceKind {
        NetDeviceKind::Bridge
    }

    fn bridge_snapshot(&self) -> Option<BridgeSnapshot> {
        Some(self.snapshot())
    }

    fn bridge_add_port(
        &self,
        authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        self.add_port(authority, registration)
    }

    fn bridge_remove_port(
        &self,
        authority: NetAdminAuthority,
        registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        self.remove_port(authority, registration)
    }

    fn bridge_poll(&self, guard: &Guard<'_>) -> Option<BridgeForwardOutcome> {
        Some(self.poll_once(guard))
    }
}

pub fn create_bridge_for_test_or_bootstrap(config: BridgeConfig) -> BridgeInstance {
    let device = Box::leak(Box::new(BridgeDevice::new(
        config.name,
        config.mac,
        config.mtu,
    )));
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: config.devt,
        name: config.name,
        ops: device,
    }));
    BridgeInstance {
        registration,
        device,
    }
}

fn ethernet_addresses(frame: &[u8]) -> Option<(EthernetAddress, EthernetAddress)> {
    if frame.len() < 14 {
        return None;
    }

    let dst = EthernetAddress::new([frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]]);
    let src = EthernetAddress::new([frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]]);
    Some((dst, src))
}
