//! Packet ingress and demux staging types.

use crate::execution::{Errno, Guard};
use crate::net::structure::Ipv4Address;
use smoltcp::time::Instant;

mod demux;
mod frame;
mod publish;
mod smoltcp_demux;

pub use demux::{PacketDispatch, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent};
pub use frame::{LoopbackIpPacket, RxFrame};
pub use publish::{NetworkPublish, NetworkPublishTarget};
pub use smoltcp_demux::demux_rx_frame_with_smoltcp;

pub trait PacketSource {
    fn next_packet(&self) -> Option<PacketDispatch>;

    fn next_packet_at(&self, now: Instant, guard: &Guard<'_>) -> Option<PacketDispatch> {
        let _now = now;
        let _guard = guard;
        self.next_packet()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketTxReadiness {
    Ready,
    Busy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketTxResult {
    Accepted { frame_len: usize },
    Busy,
    PendingResolution { next_hop: Ipv4Address },
    Failed { errno: Errno },
}

pub trait PacketTxSink {
    fn readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        let _guard = guard;
        PacketTxReadiness::Ready
    }

    /// Maximum IP-packet size accepted by this egress path.
    ///
    /// TCP consumes this before dispatch so segmentation happens in the
    /// transport engine instead of relying on the device to reject an
    /// oversized packet after the fact.
    fn ip_mtu(&self) -> u16;

    fn readiness_at(&self, now: Instant, guard: &Guard<'_>) -> PacketTxReadiness {
        let _now = now;
        self.readiness(guard)
    }

    fn source_ipv4(&self) -> Option<Ipv4Address> {
        None
    }

    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult;

    fn transmit_at(&self, frame: &[u8], now: Instant, guard: &Guard<'_>) -> PacketTxResult {
        let _now = now;
        self.transmit(frame, guard)
    }
}
