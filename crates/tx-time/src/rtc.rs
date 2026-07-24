//! RTC typed-device facade.

use crate::types::TimeError;

/// Device-facing RTC operations used by devfs and boot seed adapters.
pub trait RtcDeviceOps {
    fn read_time_ns(&self) -> Result<u64, TimeError>;

    fn set_time_ns(&self, ns: u64) -> Result<(), TimeError>;

    fn set_alarm_ns(&self, ns: u64) -> Result<(), TimeError>;

    fn clear_alarm(&self) -> Result<(), TimeError>;

    fn acknowledge_alarm_irq(&self) -> Result<(), TimeError>;
}
