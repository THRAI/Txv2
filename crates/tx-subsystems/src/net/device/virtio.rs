use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::delegate::net_delegate_kick_poll_with_post;
use crate::net::execution::yield_on_token;
use crate::net::packet::{PacketTxReadiness, RxFrame};
use crate::sync::SpinMutex;
use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};

use super::{EthernetAddress, NetDeviceOps, NetDeviceRegistration};

pub const VIRTIO_NET_DEFAULT_MTU: u16 = 1500;
pub const VIRTIO_NET_STAGING_MAJOR: u32 = 96;

pub static VIRTIO_NET0_DEVICE: VirtioNetDevice = VirtioNetDevice::new(
    VirtioNetConfig::new(
        EthernetAddress::new([0x02, 0, 0, 0, 0, 1]),
        VIRTIO_NET_DEFAULT_MTU,
        VirtioNetFeatureSet::software_checksum(),
    ),
    VirtioNetQueueConfig::new(256, 256),
);

pub static VIRTIO_NET0_REGISTRATION: NetDeviceRegistration = NetDeviceRegistration {
    devt: DevT::new(VIRTIO_NET_STAGING_MAJOR, 0),
    // Linux-conventional boot NIC name. The netlink link table, ioctl
    // SIOC* lookups, /proc/net rows, and libc if_nameindex all read this
    // one registration name — `eth0` lets `if_nametoindex($LHOST_IFACES)`
    // resolve in the initial namespace (LTP in6_02) without an
    // ioctl-only alias split.
    name: "eth0",
    ops: &VIRTIO_NET0_DEVICE,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioNetFeatureSet {
    pub checksum_offload: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioNetConfig {
    pub mac: EthernetAddress,
    pub mtu: u16,
    pub features: VirtioNetFeatureSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioNetQueueConfig {
    pub rx_capacity: usize,
    pub tx_capacity: usize,
}

#[derive(Debug, Default)]
pub struct VirtioNetStats {
    pub rx_packets: AtomicU64,
    pub tx_submitted: AtomicU64,
    pub tx_completed: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub tx_completed_bytes: AtomicU64,
    pub rx_dropped: AtomicU64,
    pub tx_busy: AtomicU64,
    pub tx_errors: AtomicU64,
    pub irq_polls: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtioNetStatsSnapshot {
    pub rx_packets: u64,
    pub tx_submitted: u64,
    pub tx_completed: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub tx_completed_bytes: u64,
    pub rx_dropped: u64,
    pub tx_busy: u64,
    pub tx_errors: u64,
    pub irq_polls: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtioNetRxInjectOutcome {
    pub accepted: bool,
    pub dropped: bool,
    pub poll_wakes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtioNetTxCompleteOutcome {
    pub completed: usize,
    pub bytes: usize,
    pub poll_wakes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioNetIrqEvent {
    RxAvailable,
    TxComplete { budget: usize },
    ConfigChanged,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtioNetIrqOutcome {
    pub rx_ready: bool,
    pub tx_completed: usize,
    pub tx_completed_bytes: usize,
    pub poll_wakes: usize,
}

pub struct VirtioNetDevice {
    config: VirtioNetConfig,
    queues: VirtioNetQueueConfig,
    rx_queue: SpinMutex<VecDeque<RxFrame>>,
    tx_inflight: SpinMutex<VecDeque<Vec<u8>>>,
    tx_completed: SpinMutex<VecDeque<Vec<u8>>>,
    link_up: AtomicBool,
    pub stats: VirtioNetStats,
}

impl VirtioNetFeatureSet {
    pub const fn software_checksum() -> Self {
        Self {
            checksum_offload: false,
        }
    }
}

impl VirtioNetConfig {
    pub const fn new(mac: EthernetAddress, mtu: u16, features: VirtioNetFeatureSet) -> Self {
        Self { mac, mtu, features }
    }
}

impl VirtioNetQueueConfig {
    pub const fn new(rx_capacity: usize, tx_capacity: usize) -> Self {
        Self {
            rx_capacity,
            tx_capacity,
        }
    }
}

impl VirtioNetStats {
    pub const fn new() -> Self {
        Self {
            rx_packets: AtomicU64::new(0),
            tx_submitted: AtomicU64::new(0),
            tx_completed: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_completed_bytes: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
            tx_busy: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
            irq_polls: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> VirtioNetStatsSnapshot {
        VirtioNetStatsSnapshot {
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            tx_submitted: self.tx_submitted.load(Ordering::Relaxed),
            tx_completed: self.tx_completed.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_completed_bytes: self.tx_completed_bytes.load(Ordering::Relaxed),
            rx_dropped: self.rx_dropped.load(Ordering::Relaxed),
            tx_busy: self.tx_busy.load(Ordering::Relaxed),
            tx_errors: self.tx_errors.load(Ordering::Relaxed),
            irq_polls: self.irq_polls.load(Ordering::Relaxed),
        }
    }

    fn clear_for_test_or_bootstrap(&self) {
        self.rx_packets.store(0, Ordering::Relaxed);
        self.tx_submitted.store(0, Ordering::Relaxed);
        self.tx_completed.store(0, Ordering::Relaxed);
        self.rx_bytes.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
        self.tx_completed_bytes.store(0, Ordering::Relaxed);
        self.rx_dropped.store(0, Ordering::Relaxed);
        self.tx_busy.store(0, Ordering::Relaxed);
        self.tx_errors.store(0, Ordering::Relaxed);
        self.irq_polls.store(0, Ordering::Relaxed);
    }
}

impl VirtioNetDevice {
    pub const fn new(config: VirtioNetConfig, queues: VirtioNetQueueConfig) -> Self {
        Self {
            config,
            queues,
            rx_queue: SpinMutex::new(VecDeque::new()),
            tx_inflight: SpinMutex::new(VecDeque::new()),
            tx_completed: SpinMutex::new(VecDeque::new()),
            link_up: AtomicBool::new(true),
            stats: VirtioNetStats::new(),
        }
    }

    pub const fn config(&self) -> VirtioNetConfig {
        self.config
    }

    pub const fn queue_config(&self) -> VirtioNetQueueConfig {
        self.queues
    }

    pub fn inject_rx_for_test_or_irq(&self, frame: RxFrame) -> VirtioNetRxInjectOutcome {
        let mut rx_queue = self.rx_queue.lock();
        if rx_queue.len() >= self.queues.rx_capacity {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return VirtioNetRxInjectOutcome {
                accepted: false,
                dropped: true,
                poll_wakes: 0,
            };
        }
        rx_queue.push_back(frame);
        VirtioNetRxInjectOutcome {
            accepted: true,
            dropped: false,
            poll_wakes: 0,
        }
    }

    pub fn inject_rx_and_fire_poll_with_post_for_test_or_irq<F>(
        &self,
        frame: RxFrame,
        mut post: F,
    ) -> VirtioNetRxInjectOutcome
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mut outcome = self.inject_rx_for_test_or_irq(frame);
        if outcome.accepted {
            outcome.poll_wakes = self.fire_poll_with_post(&mut post);
        }
        outcome
    }

    pub fn complete_tx_for_test_or_irq(&self, budget: usize) -> VirtioNetTxCompleteOutcome {
        let mut inflight = self.tx_inflight.lock();
        let mut completed = self.tx_completed.lock();
        let mut outcome = VirtioNetTxCompleteOutcome::default();

        for _ in 0..budget {
            let Some(frame) = inflight.pop_front() else {
                break;
            };
            outcome.completed += 1;
            outcome.bytes += frame.len();
            completed.push_back(frame);
        }

        if outcome.completed != 0 {
            self.stats
                .tx_completed
                .fetch_add(outcome.completed as u64, Ordering::Relaxed);
            self.stats
                .tx_completed_bytes
                .fetch_add(outcome.bytes as u64, Ordering::Relaxed);
        }

        outcome
    }

    pub fn complete_tx_and_fire_poll_with_post_for_test_or_irq<F>(
        &self,
        budget: usize,
        mut post: F,
    ) -> VirtioNetTxCompleteOutcome
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mut outcome = self.complete_tx_for_test_or_irq(budget);
        if outcome.completed != 0 {
            outcome.poll_wakes = self.fire_poll_with_post(&mut post);
        }
        outcome
    }

    pub fn handle_irq_with_post<F>(
        &self,
        event: VirtioNetIrqEvent,
        mut post: F,
    ) -> VirtioNetIrqOutcome
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        match event {
            VirtioNetIrqEvent::RxAvailable => VirtioNetIrqOutcome {
                rx_ready: true,
                poll_wakes: self.fire_poll_with_post(&mut post),
                ..VirtioNetIrqOutcome::default()
            },
            VirtioNetIrqEvent::TxComplete { budget } => {
                let completion = self.complete_tx_for_test_or_irq(budget);
                VirtioNetIrqOutcome {
                    tx_completed: completion.completed,
                    tx_completed_bytes: completion.bytes,
                    poll_wakes: if completion.completed == 0 {
                        0
                    } else {
                        self.fire_poll_with_post(&mut post)
                    },
                    ..VirtioNetIrqOutcome::default()
                }
            }
            VirtioNetIrqEvent::ConfigChanged => VirtioNetIrqOutcome {
                poll_wakes: self.fire_poll_with_post(&mut post),
                ..VirtioNetIrqOutcome::default()
            },
        }
    }

    pub fn drain_completed_tx_frames_for_test_or_driver(&self) -> Vec<Vec<u8>> {
        self.tx_completed.lock().drain(..).collect()
    }

    pub fn rx_len(&self) -> usize {
        self.rx_queue.lock().len()
    }

    pub fn tx_inflight_len(&self) -> usize {
        self.tx_inflight.lock().len()
    }

    pub fn tx_completed_len(&self) -> usize {
        self.tx_completed.lock().len()
    }

    pub fn set_link_up_for_test_or_bootstrap(&self, link_up: bool) {
        self.link_up.store(link_up, Ordering::Release);
    }

    pub fn clear_for_test_or_bootstrap(&self) {
        self.rx_queue.lock().clear();
        self.tx_inflight.lock().clear();
        self.tx_completed.lock().clear();
        self.link_up.store(true, Ordering::Release);
        self.stats.clear_for_test_or_bootstrap();
    }

    fn fire_poll_with_post<F>(&self, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let wakes = net_delegate_kick_poll_with_post(post);
        self.stats.irq_polls.fetch_add(1, Ordering::Relaxed);
        wakes
    }

    fn tx_has_space(&self) -> bool {
        self.tx_inflight.lock().len() < self.queues.tx_capacity
    }
}

impl NetDeviceOps for VirtioNetDevice {
    fn receive(&self) -> Option<RxFrame> {
        let frame = self.rx_queue.lock().pop_front()?;
        self.stats
            .rx_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        self.stats.rx_packets.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<()> {
        if !self.link_up.load(Ordering::Acquire) {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return StepOutcome::Err(Errno::ENODEV);
        }

        let mut inflight = self.tx_inflight.lock();
        if inflight.len() >= self.queues.tx_capacity {
            self.stats.tx_busy.fetch_add(1, Ordering::Relaxed);
            return yield_on_token(crate::net::delegate::net_delegate_wait_token());
        }

        let mut owned = Vec::with_capacity(frame.len());
        owned.extend_from_slice(frame);
        inflight.push_back(owned);
        self.stats.tx_submitted.fetch_add(1, Ordering::Relaxed);
        self.stats
            .tx_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        StepOutcome::Done(())
    }

    fn tx_readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        let _guard = guard;
        if self.link_up.load(Ordering::Acquire) && self.tx_has_space() {
            PacketTxReadiness::Ready
        } else {
            PacketTxReadiness::Busy
        }
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.config.mac
    }

    fn mtu(&self) -> u16 {
        self.config.mtu
    }
}
