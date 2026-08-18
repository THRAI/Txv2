use core::ptr::{read_volatile, write_volatile};

pub(super) const MAC_CONFIGURATION: usize = 0x000;
pub(super) const MAC_PACKET_FILTER: usize = 0x008;
pub(super) const MAC_Q0_TX_FLOW_CTRL: usize = 0x070;
pub(super) const MAC_RX_FLOW_CTRL: usize = 0x090;
pub(super) const MAC_RXQ_CTRL0: usize = 0x0a0;
pub(super) const MAC_RXQ_CTRL1: usize = 0x0a4;
pub(super) const MAC_VERSION: usize = 0x110;
pub(super) const MAC_HW_FEATURE1: usize = 0x120;
pub(super) const MAC_MDIO_ADDRESS: usize = 0x200;
pub(super) const MAC_MDIO_DATA: usize = 0x204;
pub(super) const MAC_ADDRESS0_HIGH: usize = 0x300;
pub(super) const MAC_ADDRESS0_LOW: usize = 0x304;

pub(super) const MTL_TXQ0_OPERATION_MODE: usize = 0x0d00;
pub(super) const MTL_TXQ0_QUANTUM_WEIGHT: usize = 0x0d18;
pub(super) const MTL_RXQ0_OPERATION_MODE: usize = 0x0d30;

pub(super) const DMA_MODE: usize = 0x1000;
pub(super) const DMA_SYSBUS_MODE: usize = 0x1004;
pub(super) const DMA_CH0_CONTROL: usize = 0x1100;
pub(super) const DMA_CH0_TX_CONTROL: usize = 0x1104;
pub(super) const DMA_CH0_RX_CONTROL: usize = 0x1108;
pub(super) const DMA_CH0_TXDESC_LIST_HI: usize = 0x1110;
pub(super) const DMA_CH0_TXDESC_LIST_LO: usize = 0x1114;
pub(super) const DMA_CH0_RXDESC_LIST_HI: usize = 0x1118;
pub(super) const DMA_CH0_RXDESC_LIST_LO: usize = 0x111c;
pub(super) const DMA_CH0_TXDESC_TAIL: usize = 0x1120;
pub(super) const DMA_CH0_RXDESC_TAIL: usize = 0x1128;
pub(super) const DMA_CH0_TXDESC_RING_LEN: usize = 0x112c;
pub(super) const DMA_CH0_RXDESC_RING_LEN: usize = 0x1130;
pub(super) const DMA_CH0_INTR_ENABLE: usize = 0x1134;
pub(super) const DMA_CH0_CURRENT_RXDESC: usize = 0x114c;
pub(super) const DMA_CH0_CURRENT_RXBUF: usize = 0x115c;
pub(super) const DMA_CH0_STATUS: usize = 0x1160;

pub(super) const MAC_CONFIGURATION_PS: u32 = 1 << 15;
pub(super) const MAC_CONFIGURATION_FES: u32 = 1 << 14;
pub(super) const MAC_CONFIGURATION_DM: u32 = 1 << 13;
pub(super) const MAC_CONFIGURATION_TE: u32 = 1 << 1;
pub(super) const MAC_CONFIGURATION_RE: u32 = 1;
pub(super) const MAC_CONFIGURATION_GPSLCE: u32 = 1 << 23;
pub(super) const MAC_CONFIGURATION_CST: u32 = 1 << 21;
pub(super) const MAC_CONFIGURATION_ACS: u32 = 1 << 20;
pub(super) const MAC_CONFIGURATION_WD: u32 = 1 << 19;
pub(super) const MAC_CONFIGURATION_JD: u32 = 1 << 17;
pub(super) const MAC_CONFIGURATION_JE: u32 = 1 << 16;

pub(super) const DMA_MODE_SWR: u32 = 1;
pub(super) const DMA_CH0_CONTROL_DSL_SHIFT: u32 = 18;
pub(super) const DMA_CH0_CONTROL_DSL_MASK: u32 = 0x1f;
pub(super) const DMA_CH0_CONTROL_PBLX8: u32 = 1 << 16;
pub(super) const DMA_CH0_TX_CONTROL_ST: u32 = 1;
pub(super) const DMA_CH0_RX_CONTROL_SR: u32 = 1;
pub(super) const DMA_CH0_STATUS_NIS: u32 = 1 << 15;
pub(super) const DMA_CH0_STATUS_AIS: u32 = 1 << 14;
pub(super) const DMA_CH0_STATUS_FBE: u32 = 1 << 12;
pub(super) const DMA_CH0_STATUS_RPS: u32 = 1 << 8;
pub(super) const DMA_CH0_STATUS_RBU: u32 = 1 << 7;
pub(super) const DMA_CH0_STATUS_RI: u32 = 1 << 6;
pub(super) const DMA_CH0_STATUS_TI: u32 = 1;
pub(super) const DMA_CH0_INTR_NORMAL: u32 = (1 << 15) | (1 << 6) | 1;
pub(super) const DMA_CH0_INTR_ABNORMAL: u32 = (1 << 14) | (1 << 12) | (1 << 8) | (1 << 7);
pub(super) const DMA_CH0_INTR_RI: u32 = 1 << 6;
pub(super) const DMA_CH0_INTR_RX_STALL: u32 = (1 << 8) | (1 << 7);

#[derive(Clone, Copy)]
pub(super) struct RegisterBlock {
    base: usize,
}

impl RegisterBlock {
    pub(super) const fn new(base: usize) -> Self {
        Self { base }
    }

    #[inline]
    pub(super) fn read(self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    pub(super) fn write(self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    #[inline]
    pub(super) fn modify(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }
}
