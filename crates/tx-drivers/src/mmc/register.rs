//! Minimal DesignWare MSHC register model used by the VisionFive-2 PIO
//! driver. All wrappers are transparent so volatile reads/writes retain the
//! controller's native 32-bit register layout.

#![allow(dead_code)]

macro_rules! reg32 {
    ($name:ident, $offset:expr) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Default)]
        pub struct $name(u32);

        impl $name {
            pub const fn new() -> Self {
                Self(0)
            }

            pub const fn offset() -> usize {
                $offset
            }
        }

        impl From<u32> for $name {
            fn from(value: u32) -> Self {
                Self(value)
            }
        }
    };
}

reg32!(CMD, 0x2c);

impl CMD {
    const RESPONSE_EXPECTED: u32 = 1 << 6;
    const RESPONSE_LENGTH: u32 = 1 << 7;
    const CHECK_RESPONSE_CRC: u32 = 1 << 8;
    const DATA_EXPECTED: u32 = 1 << 9;
    const READ_WRITE: u32 = 1 << 10;
    const WAIT_PREVDATA_COMPLETE: u32 = 1 << 13;
    const SEND_INITIALIZATION: u32 = 1 << 15;
    const UPDATE_CLOCK_ONLY: u32 = 1 << 21;
    const USE_HOLD_REG: u32 = 1 << 29;
    const START_CMD: u32 = 1 << 31;

    fn set(mut self, bit: u32, enabled: bool) -> Self {
        if enabled {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
        self
    }

    pub fn with_response_length(self, enabled: bool) -> Self {
        self.set(Self::RESPONSE_LENGTH, enabled)
    }

    pub fn with_read_write(self, enabled: bool) -> Self {
        self.set(Self::READ_WRITE, enabled)
    }

    pub fn response_expected(self) -> bool {
        self.0 & Self::RESPONSE_EXPECTED != 0
    }

    pub fn data_expected(self) -> bool {
        self.0 & Self::DATA_EXPECTED != 0
    }

    pub fn read_write(self) -> bool {
        self.0 & Self::READ_WRITE != 0
    }

    pub fn update_clock_register_only(self) -> bool {
        self.0 & Self::UPDATE_CLOCK_ONLY != 0
    }

    pub fn can_send_cmd(self) -> bool {
        self.0 & Self::START_CMD == 0
    }

    fn base(card_number: usize, cmd_index: usize) -> Self {
        let value = (cmd_index as u32 & 0x3f)
            | Self::START_CMD
            | Self::USE_HOLD_REG
            | Self::RESPONSE_EXPECTED
            | Self::WAIT_PREVDATA_COMPLETE
            | Self::CHECK_RESPONSE_CRC
            | ((card_number as u32 & 0x1f) << 16);
        Self(value)
    }

    pub fn no_data_cmd(card_number: usize, cmd_index: usize) -> Self {
        Self::base(card_number, cmd_index)
    }

    pub fn no_data_cmd_no_crc(card_number: usize, cmd_index: usize) -> Self {
        Self::base(card_number, cmd_index).set(Self::CHECK_RESPONSE_CRC, false)
    }

    pub fn data_cmd(card_number: usize, cmd_index: usize) -> Self {
        Self::base(card_number, cmd_index).set(Self::DATA_EXPECTED, true)
    }

    pub fn clock_cmd() -> Self {
        Self(Self::START_CMD | Self::WAIT_PREVDATA_COMPLETE | Self::UPDATE_CLOCK_ONLY)
    }

    pub fn reset_cmd0(card_number: usize) -> Self {
        Self::base(card_number, 0).set(Self::SEND_INITIALIZATION, true)
    }
}

reg32!(RINSTS, 0x44);

impl RINSTS {
    pub fn command_done(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    pub fn data_transfer_over(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    pub fn transmit_data_request(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    pub fn receive_data_request(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    pub fn command_conflict(self) -> bool {
        self.0 & (1 << 12) != 0
    }

    pub fn no_error(self) -> bool {
        const ERROR_BITS: u32 = (1 << 1) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 13) | (1 << 15);
        self.0 & ERROR_BITS == 0
    }
}

reg32!(CMDARG, 0x28);

impl CMDARG {
    pub const fn empty() -> Self {
        Self::new()
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct RESP(u128);

impl RESP {
    pub const fn offset() -> usize {
        0x30
    }

    pub fn resp(self, index: usize) -> u32 {
        assert!(index < 4);
        (self.0 >> (index * 32)) as u32
    }

    pub fn resps_u128(self) -> u128 {
        self.0
    }

    pub fn ocr(self) -> u32 {
        self.resp(0)
    }
}

reg32!(CTRL, 0x00);

impl CTRL {
    fn set(mut self, bit: u32, enabled: bool) -> Self {
        if enabled {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
        self
    }

    pub fn with_fifo_reset(self, enabled: bool) -> Self {
        self.set(1 << 1, enabled)
    }

    pub fn with_dma_reset(self, enabled: bool) -> Self {
        self.set(1 << 2, enabled)
    }

    pub fn with_use_internal_dmac(self, enabled: bool) -> Self {
        self.set(1 << 25, enabled)
    }
}

reg32!(CLKDIV, 0x08);

impl CLKDIV {
    pub fn with_clk_divider0(mut self, divider: u8) -> Self {
        self.0 = (self.0 & !0xff) | divider as u32;
        self
    }
}

reg32!(CLKENA, 0x10);

impl CLKENA {
    pub fn with_cclk_enable(mut self, enabled: u16) -> Self {
        self.0 = (self.0 & !0xffff) | enabled as u32;
        self
    }
}

reg32!(CTYPE, 0x18);

#[derive(Clone, Copy, Debug)]
pub enum CtypeCardWidth {
    Width8,
    Width4,
    Width1,
}

impl CTYPE {
    pub fn set_card_width(index: usize, width: CtypeCardWidth) -> Self {
        debug_assert!(index < 16);
        match width {
            CtypeCardWidth::Width1 => Self::new(),
            CtypeCardWidth::Width4 => Self(1 << index),
            CtypeCardWidth::Width8 => Self(1 << (16 + index)),
        }
    }
}

reg32!(BLKSIZ, 0x1c);

impl BLKSIZ {
    pub fn with_block_size(mut self, size: usize) -> Self {
        self.0 = (self.0 & !0xffff) | (size as u32 & 0xffff);
        self
    }
}

reg32!(BYTCNT, 0x20);

impl BYTCNT {
    pub fn with_byte_count(mut self, count: usize) -> Self {
        self.0 = count as u32;
        self
    }
}

reg32!(STATUS, 0x48);

impl STATUS {
    pub fn data_busy(self) -> bool {
        self.0 & (1 << 9) != 0
    }

    pub fn fifo_count(self) -> usize {
        ((self.0 >> 17) & 0x1fff) as usize
    }
}

reg32!(CDETECT, 0x50);

impl CDETECT {
    pub fn card_detect_n(self) -> usize {
        (self.0 & 0x3fff_ffff) as usize
    }
}

reg32!(BMOD, 0x80);

impl BMOD {
    fn set(mut self, bit: u32, enabled: bool) -> Self {
        if enabled {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
        self
    }

    pub fn idmac_enable(self) -> bool {
        self.0 & (1 << 7) != 0
    }

    pub fn with_idmac_enable(self, enabled: bool) -> Self {
        self.set(1 << 7, enabled)
    }

    pub fn with_software_reset(self, enabled: bool) -> Self {
        self.set(1, enabled)
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct CID(u128);

impl From<u128> for CID {
    fn from(value: u128) -> Self {
        Self(value)
    }
}
