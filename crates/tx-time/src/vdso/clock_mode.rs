use tx_hal::{MonotonicCounterIf, VdsoCounterInfo, VdsoCounterMode};

use super::VdsoCounterCalibration;

/// The VVAR clock mode consumed by the vDSO assembly.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VdsoClockMode {
    Syscall = 0,
    RiscvTime = 1,
}

impl VdsoClockMode {
    const fn from_hal(mode: VdsoCounterMode) -> Option<Self> {
        match mode {
            VdsoCounterMode::RiscvTime => Some(Self::RiscvTime),
            VdsoCounterMode::None => None,
        }
    }
}

/// A validated raw counter and its VVAR conversion parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VdsoEligibleCounter {
    info: VdsoCounterInfo,
    mode: VdsoClockMode,
    calibration: VdsoCounterCalibration,
}

impl VdsoEligibleCounter {
    pub const fn clock_mode(self) -> VdsoClockMode {
        self.mode
    }

    pub const fn calibration(self) -> VdsoCounterCalibration {
        self.calibration
    }

    pub const fn info(self) -> VdsoCounterInfo {
        self.info
    }

    pub fn read_counter<P: MonotonicCounterIf>(self) -> u64 {
        P::read_vdso_counter()
    }
}

/// Why a platform cannot use the vDSO counter fast path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VdsoCounterEligibility {
    Unavailable,
    Unstable,
    KernelOnly,
    UnsupportedMode,
    InvalidCalibration,
    Ready(VdsoEligibleCounter),
}

impl VdsoCounterEligibility {
    pub const fn is_unavailable(self) -> bool {
        matches!(self, Self::Unavailable)
    }

    pub const fn is_unstable(self) -> bool {
        matches!(self, Self::Unstable)
    }

    pub const fn is_kernel_only(self) -> bool {
        matches!(self, Self::KernelOnly)
    }

    pub const fn ready(self) -> Option<VdsoEligibleCounter> {
        match self {
            Self::Ready(counter) => Some(counter),
            _ => None,
        }
    }
}

/// Validate the static HAL descriptor before a VVAR writer reads its counter.
pub fn counter_eligibility<P: MonotonicCounterIf>() -> VdsoCounterEligibility {
    let Some(info) = P::vdso_counter_info() else {
        return VdsoCounterEligibility::Unavailable;
    };
    if !info.stable {
        return VdsoCounterEligibility::Unstable;
    }
    if !info.user_readable {
        return VdsoCounterEligibility::KernelOnly;
    }
    let Some(mode) = VdsoClockMode::from_hal(info.mode) else {
        return VdsoCounterEligibility::UnsupportedMode;
    };
    let Some(calibration) = VdsoCounterCalibration::from_counter(info) else {
        return VdsoCounterEligibility::InvalidCalibration;
    };
    VdsoCounterEligibility::Ready(VdsoEligibleCounter {
        info,
        mode,
        calibration,
    })
}
