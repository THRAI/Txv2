use alloc::vec::Vec;

use crate::execution::{Guard, StepOutcome};
use crate::net::device::{EthernetAddress, NetDeviceRegistration};
use crate::net::packet::{
    demux_rx_frame_with_smoltcp, PacketDispatch, PacketSource, PacketTxReadiness, PacketTxResult,
    PacketTxSink,
};
use crate::net::structure::Ipv4Address;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmoltcpAdapterConfig {
    pub local_mac: EthernetAddress,
    pub local_ipv4: Ipv4Address,
    pub mtu: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmoltcpAdapter {
    pub config: SmoltcpAdapterConfig,
}

pub struct SmoltcpPacketSource<'a> {
    pub adapter: &'a SmoltcpAdapter,
    pub device: &'static NetDeviceRegistration,
}

pub struct SmoltcpPacketTxSink<'a> {
    pub adapter: &'a SmoltcpAdapter,
    pub device: &'static NetDeviceRegistration,
}

impl SmoltcpAdapter {
    pub const fn new(config: SmoltcpAdapterConfig) -> Self {
        Self { config }
    }

    pub fn demux_rx_frame(&self, frame: &crate::net::packet::RxFrame) -> PacketDispatch {
        let _config = self.config;
        demux_rx_frame_with_smoltcp(frame)
    }

    pub fn emit_tx_frame(&self, ipv4_packet: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + ipv4_packet.len());
        frame.extend_from_slice(&EthernetAddress::BROADCAST.octets());
        frame.extend_from_slice(&self.config.local_mac.octets());
        frame.extend_from_slice(&[0x08, 0x00]);
        frame.extend_from_slice(ipv4_packet);
        frame
    }
}

impl PacketSource for SmoltcpPacketSource<'_> {
    fn next_packet(&self) -> Option<PacketDispatch> {
        let frame = self.device.ops.receive()?;
        Some(self.adapter.demux_rx_frame(&frame))
    }
}

impl SmoltcpPacketTxSink<'_> {
    pub fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult {
        let _config = self.adapter.config;
        let frame = self.adapter.emit_tx_frame(frame);
        match self.device.ops.transmit(&frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Continue { .. } => PacketTxResult::Accepted {
                frame_len: frame.len(),
            },
            StepOutcome::Yield { .. } => PacketTxResult::Busy,
            StepOutcome::Err(errno) => PacketTxResult::Failed { errno },
        }
    }

    pub fn mtu(&self) -> u16 {
        self.device.ops.mtu()
    }

    pub fn mac_addr(&self) -> EthernetAddress {
        self.device.ops.mac_addr()
    }
}

impl PacketTxSink for SmoltcpPacketTxSink<'_> {
    fn readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        self.device.ops.tx_readiness(guard)
    }

    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult {
        SmoltcpPacketTxSink::transmit(self, frame, guard)
    }
}
