use tx_hal::VdsoCounterInfo;

/// Fixed-point conversion parameters published to VVAR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VdsoCounterCalibration {
    mult: u64,
    shift: u32,
    mask: u64,
}

impl VdsoCounterCalibration {
    pub const fn from_counter(info: VdsoCounterInfo) -> Option<Self> {
        if info.frequency_hz == 0 {
            return None;
        }

        // 24 preserves sub-nanosecond conversion precision while keeping every
        // shifted sub-second VVAR base in u64.
        let shift = 24;
        let mult = ((1_000_000_000_u128 << shift) / info.frequency_hz as u128) as u64;
        if mult == 0 {
            return None;
        }

        Some(Self {
            mult,
            shift,
            mask: info.mask,
        })
    }

    pub const fn mult(self) -> u64 {
        self.mult
    }

    pub const fn shift(self) -> u32 {
        self.shift
    }

    pub const fn mask(self) -> u64 {
        self.mask
    }
}
