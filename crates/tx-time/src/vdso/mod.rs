//! vDSO/VVAR time ABI and counter eligibility.
//!
//! This module is deliberately independent from `timer`: it only translates
//! timekeeper snapshots into the user-visible VVAR contract.

mod abi;
mod calibration;
mod clock_mode;
mod snapshot;

pub use abi::{
    VvarData, VVAR_ABI_VERSION, VVAR_ABI_VERSION_OFFSET, VVAR_CLOCK_MODE_OFFSET,
    VVAR_CYCLE_LAST_OFFSET, VVAR_DATA_SIZE, VVAR_MASK_OFFSET, VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET,
    VVAR_MONOTONIC_SEC_OFFSET, VVAR_MULT_OFFSET, VVAR_PAGE_SIZE, VVAR_REALTIME_GENERATION_OFFSET,
    VVAR_REALTIME_NSEC_SHIFTED_OFFSET, VVAR_REALTIME_SEC_OFFSET, VVAR_SEQ_OFFSET,
    VVAR_SHIFT_OFFSET,
};
pub use calibration::VdsoCounterCalibration;
pub use clock_mode::{
    counter_eligibility, VdsoClockMode, VdsoCounterEligibility, VdsoEligibleCounter,
};
pub use snapshot::VvarSnapshot;
