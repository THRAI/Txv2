//! DWMAC 3.x legacy GMAC and DMA register definitions.

#[derive(Clone, Copy)]
pub(super) struct RegisterBlock {
    base: usize,
}

impl RegisterBlock {
    pub(super) const fn new(base: usize) -> Self {
        Self { base }
    }

    pub(super) fn read(self, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    pub(super) fn write(self, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    pub(super) fn modify(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }
}

pub(super) const GMAC_CONFIGURATION: usize = 0x0000;
pub(super) const GMAC_FRAME_FILTER: usize = 0x0004;
pub(super) const GMAC_MII_ADDRESS: usize = 0x0010;
pub(super) const GMAC_MII_DATA: usize = 0x0014;
pub(super) const GMAC_FLOW_CONTROL: usize = 0x0018;
pub(super) const GMAC_VERSION: usize = 0x0020;
pub(super) const GMAC_INTERRUPT_MASK: usize = 0x003c;
pub(super) const GMAC_ADDRESS0_HIGH: usize = 0x0040;
pub(super) const GMAC_ADDRESS0_LOW: usize = 0x0044;

// DWMAC 3.x places the MMC block at MAC + 0x100. The kernel does not consume
// MMC counter interrupts, so mask all three banks before enabling the shared
// GMAC interrupt line.
pub(super) const MMC_RX_INTERRUPT_MASK: usize = 0x010c;
pub(super) const MMC_TX_INTERRUPT_MASK: usize = 0x0110;
pub(super) const MMC_RX_IPC_INTERRUPT_MASK: usize = 0x0200;
pub(super) const MMC_INTERRUPT_MASK_ALL: u32 = u32::MAX;

pub(super) const GMAC_CONFIGURATION_JD: u32 = 1 << 22;
pub(super) const GMAC_CONFIGURATION_BE: u32 = 1 << 21;
pub(super) const GMAC_CONFIGURATION_DCRS: u32 = 1 << 16;
pub(super) const GMAC_CONFIGURATION_PS: u32 = 1 << 15;
pub(super) const GMAC_CONFIGURATION_FES: u32 = 1 << 14;
pub(super) const GMAC_CONFIGURATION_DO: u32 = 1 << 13;
pub(super) const GMAC_CONFIGURATION_DM: u32 = 1 << 11;
pub(super) const GMAC_CONFIGURATION_TE: u32 = 1 << 3;
pub(super) const GMAC_CONFIGURATION_RE: u32 = 1 << 2;

pub(super) const GMAC_CORE_INIT: u32 =
    GMAC_CONFIGURATION_JD | GMAC_CONFIGURATION_BE | GMAC_CONFIGURATION_DCRS | GMAC_CONFIGURATION_DO;
pub(super) const GMAC_INTERRUPT_MASK_RGMII: u32 = 1 << 0;
pub(super) const GMAC_INTERRUPT_MASK_PCS_LINK: u32 = 1 << 1;
pub(super) const GMAC_INTERRUPT_MASK_PCS_AN: u32 = 1 << 2;
pub(super) const GMAC_INTERRUPT_MASK_PMT: u32 = 1 << 3;
pub(super) const GMAC_INTERRUPT_MASK_TIMESTAMP: u32 = 1 << 9;
pub(super) const GMAC_INTERRUPT_MASK_LPI: u32 = 1 << 10;
pub(super) const GMAC_INTERRUPT_MASK_GPIO: u32 = 1 << 11;
pub(super) const GMAC_INTERRUPT_UNSUPPORTED_MASK: u32 = GMAC_INTERRUPT_MASK_RGMII
    | GMAC_INTERRUPT_MASK_PCS_LINK
    | GMAC_INTERRUPT_MASK_PCS_AN
    | GMAC_INTERRUPT_MASK_PMT
    | GMAC_INTERRUPT_MASK_TIMESTAMP
    | GMAC_INTERRUPT_MASK_LPI
    | GMAC_INTERRUPT_MASK_GPIO;
pub(super) const MII_ADDRESS_PHY_SHIFT: u32 = 11;
pub(super) const MII_ADDRESS_REGISTER_SHIFT: u32 = 6;
pub(super) const MII_ADDRESS_CLOCK_SHIFT: u32 = 2;
pub(super) const MII_ADDRESS_WRITE: u32 = 1 << 1;
pub(super) const MII_ADDRESS_BUSY: u32 = 1;
// Linux's Loongson stmmac glue fixes clk_csr_i at 100-150 MHz. Encoding 1
// selects the matching /62 MDC divisor.
pub(super) const MII_CLOCK_100_150_MHZ: u32 = 1;

pub(super) const DMA_BUS_MODE: usize = 0x1000;
pub(super) const DMA_TX_POLL_DEMAND: usize = 0x1004;
pub(super) const DMA_RX_POLL_DEMAND: usize = 0x1008;
pub(super) const DMA_RX_DESCRIPTOR_BASE: usize = 0x100c;
pub(super) const DMA_TX_DESCRIPTOR_BASE: usize = 0x1010;
pub(super) const DMA_STATUS: usize = 0x1014;
pub(super) const DMA_OPERATION_MODE: usize = 0x1018;
pub(super) const DMA_INTERRUPT_ENABLE: usize = 0x101c;

pub(super) const DMA_BUS_MODE_SWR: u32 = 1;
pub(super) const DMA_BUS_MODE_PBL_SHIFT: u32 = 8;
pub(super) const DMA_BUS_MODE_RPBL_SHIFT: u32 = 17;
pub(super) const DMA_BUS_MODE_USP: u32 = 1 << 23;
pub(super) const DMA_BUS_MODE_PBLX8: u32 = 1 << 24;

pub(super) const DMA_OPERATION_MODE_RSF: u32 = 1 << 25;
pub(super) const DMA_OPERATION_MODE_TSF: u32 = 1 << 21;
pub(super) const DMA_OPERATION_MODE_ST: u32 = 1 << 13;
pub(super) const DMA_OPERATION_MODE_OSF: u32 = 1 << 2;
pub(super) const DMA_OPERATION_MODE_SR: u32 = 1 << 1;

#[cfg(test)]
pub(super) const DMA_STATUS_NIS: u32 = 1 << 16;
#[cfg(test)]
pub(super) const DMA_STATUS_AIS: u32 = 1 << 15;
#[cfg(test)]
pub(super) const DMA_STATUS_FBI: u32 = 1 << 13;
pub(super) const DMA_STATUS_TX_STATE_MASK: u32 = 0x7 << 20;
pub(super) const DMA_STATUS_RX_STATE_MASK: u32 = 0x7 << 17;
pub(super) const DMA_STATUS_RPS: u32 = 1 << 8;
pub(super) const DMA_STATUS_RU: u32 = 1 << 7;
pub(super) const DMA_STATUS_RI: u32 = 1 << 6;
pub(super) const DMA_STATUS_OVF: u32 = 1 << 4;
#[cfg(test)]
pub(super) const DMA_STATUS_TU: u32 = 1 << 2;
#[cfg(test)]
pub(super) const DMA_STATUS_TPS: u32 = 1 << 1;
pub(super) const DMA_STATUS_TI: u32 = 1;

pub(super) const DMA_INTERRUPT_NIE: u32 = 1 << 16;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_AIE: u32 = 1 << 15;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_FBE: u32 = 1 << 13;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_RSE: u32 = 1 << 8;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_RUE: u32 = 1 << 7;
pub(super) const DMA_INTERRUPT_RIE: u32 = 1 << 6;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_TUE: u32 = 1 << 2;
#[cfg(test)]
pub(super) const DMA_INTERRUPT_TSE: u32 = 1 << 1;
pub(super) const DMA_INTERRUPT_TIE: u32 = 1;

// Only arm causes with a complete wake/recovery path. Abnormal DMA causes are
// still cleared by DMA_STATUS_W1C_MASK, but require an explicit recovery state
// machine before they can safely drive the 2K1000's level-triggered IRQ line.
pub(super) const DMA_INTERRUPT_MASK: u32 =
    DMA_INTERRUPT_NIE | DMA_INTERRUPT_RIE | DMA_INTERRUPT_TIE;

// CSR5 bits 16:0 are write-one-to-clear. Keep this independent from the
// subset of causes that the driver classifies or enables: leaving an
// unclassified status bit latched can continuously assert a level IRQ.
pub(super) const DMA_STATUS_W1C_MASK: u32 = 0x0001_ffff;
