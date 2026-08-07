use core::ptr::{read_volatile, write_volatile};

pub(super) const HOST_CAP: usize = 0x00;
pub(super) const HOST_GHC: usize = 0x04;
pub(super) const HOST_IS: usize = 0x08;
pub(super) const HOST_PI: usize = 0x0c;
pub(super) const HOST_VS: usize = 0x10;
pub(super) const HOST_CAP2: usize = 0x24;
pub(super) const HOST_BOHC: usize = 0x28;

pub(super) const HOST_GHC_HR: u32 = 1 << 0;
pub(super) const HOST_GHC_IE: u32 = 1 << 1;
pub(super) const HOST_GHC_AE: u32 = 1 << 31;
pub(super) const HOST_CAP_SSS: u32 = 1 << 27;
pub(super) const HOST_CAP2_BOH: u32 = 1;
pub(super) const HOST_BOHC_BOS: u32 = 1 << 0;
pub(super) const HOST_BOHC_OOS: u32 = 1 << 1;
pub(super) const HOST_BOHC_BB: u32 = 1 << 4;

pub(super) const PORT_BASE: usize = 0x100;
pub(super) const PORT_STRIDE: usize = 0x80;
pub(super) const PORT_CLB: usize = 0x00;
pub(super) const PORT_CLBU: usize = 0x04;
pub(super) const PORT_FB: usize = 0x08;
pub(super) const PORT_FBU: usize = 0x0c;
pub(super) const PORT_IS: usize = 0x10;
pub(super) const PORT_IE: usize = 0x14;
pub(super) const PORT_CMD: usize = 0x18;
pub(super) const PORT_TFD: usize = 0x20;
pub(super) const PORT_SIG: usize = 0x24;
pub(super) const PORT_SSTS: usize = 0x28;
pub(super) const PORT_SERR: usize = 0x30;
pub(super) const PORT_SACT: usize = 0x34;
pub(super) const PORT_CI: usize = 0x38;

pub(super) const PORT_CMD_ST: u32 = 1 << 0;
pub(super) const PORT_CMD_SUD: u32 = 1 << 1;
pub(super) const PORT_CMD_FRE: u32 = 1 << 4;
pub(super) const PORT_CMD_FR: u32 = 1 << 14;
pub(super) const PORT_CMD_CR: u32 = 1 << 15;
pub(super) const PORT_CMD_ICC_MASK: u32 = 0x0f << 28;
pub(super) const PORT_CMD_ICC_ACTIVE: u32 = 0x01 << 28;
pub(super) const PORT_TFD_ERR: u32 = 1 << 0;
pub(super) const PORT_TFD_DRQ: u32 = 1 << 3;
pub(super) const PORT_TFD_BSY: u32 = 1 << 7;
pub(super) const PORT_IS_TFES: u32 = 1 << 30;
pub(super) const PORT_IS_OFS: u32 = 1 << 24;
pub(super) const PORT_IS_INFS: u32 = 1 << 26;
pub(super) const PORT_IS_IFS: u32 = 1 << 27;
pub(super) const PORT_IS_HBDS: u32 = 1 << 28;
pub(super) const PORT_IS_HBFS: u32 = 1 << 29;
pub(super) const PORT_IS_ERROR: u32 =
    PORT_IS_OFS | PORT_IS_INFS | PORT_IS_IFS | PORT_IS_HBDS | PORT_IS_HBFS | PORT_IS_TFES;
pub(super) const PORT_SSTS_DET_MASK: u32 = 0x0f;
pub(super) const PORT_SSTS_DET_PRESENT: u32 = 0x03;
pub(super) const PORT_SSTS_IPM_MASK: u32 = 0x0f << 8;
pub(super) const PORT_SSTS_IPM_ACTIVE: u32 = 0x01 << 8;
pub(super) const SATA_SIG_ATA: u32 = 0x0000_0101;

pub(super) const fn port_reg(port: usize, register: usize) -> usize {
    PORT_BASE + port * PORT_STRIDE + register
}

pub(super) trait AhciRegisterIo {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&self, offset: usize, value: u32);

    fn modify32(&self, offset: usize, clear: u32, set: u32) {
        self.write32(offset, (self.read32(offset) & !clear) | set);
    }
}

#[derive(Clone, Copy)]
pub(super) struct VolatileMmio {
    base: usize,
}

impl VolatileMmio {
    pub(super) const fn new(base: usize) -> Self {
        Self { base }
    }
}

impl AhciRegisterIo for VolatileMmio {
    #[inline]
    fn read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    fn write32(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }
}
