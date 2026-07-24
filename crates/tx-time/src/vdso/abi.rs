use core::mem::{offset_of, size_of};

use super::{VdsoClockMode, VvarSnapshot};

pub const VVAR_PAGE_SIZE: usize = 4096;
pub const VVAR_ABI_VERSION: u32 = 1;

/// The sole C-compatible VVAR payload definition.
///
/// `seq` is written with the VVAR seqlock protocol by the mapped-page owner;
/// all other fields are a complete timekeeper snapshot.
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
pub struct VvarData {
    pub seq: u64,
    pub abi_version: u32,
    pub clock_mode: u32,
    pub realtime_sec: u64,
    pub realtime_nsec_shifted: u64,
    pub monotonic_sec: u64,
    pub monotonic_nsec_shifted: u64,
    pub cycle_last: u64,
    pub mult: u64,
    pub shift: u64,
    pub mask: u64,
    pub realtime_generation: u64,
    _reserved: [u8; VVAR_PAGE_SIZE - 88],
}

impl VvarData {
    pub const fn from_snapshot(snapshot: VvarSnapshot, clock_mode: VdsoClockMode) -> Self {
        Self {
            seq: 0,
            abi_version: VVAR_ABI_VERSION,
            clock_mode: clock_mode as u32,
            realtime_sec: snapshot.realtime_sec,
            realtime_nsec_shifted: snapshot.realtime_nsec_shifted,
            monotonic_sec: snapshot.monotonic_sec,
            monotonic_nsec_shifted: snapshot.monotonic_nsec_shifted,
            cycle_last: snapshot.cycle_last,
            mult: snapshot.mult,
            shift: snapshot.shift,
            mask: snapshot.mask,
            realtime_generation: snapshot.realtime_generation,
            _reserved: [0; VVAR_PAGE_SIZE - 88],
        }
    }
}

pub const VVAR_DATA_SIZE: usize = size_of::<VvarData>();
pub const VVAR_SEQ_OFFSET: usize = offset_of!(VvarData, seq);
pub const VVAR_ABI_VERSION_OFFSET: usize = offset_of!(VvarData, abi_version);
pub const VVAR_CLOCK_MODE_OFFSET: usize = offset_of!(VvarData, clock_mode);
pub const VVAR_REALTIME_SEC_OFFSET: usize = offset_of!(VvarData, realtime_sec);
pub const VVAR_REALTIME_NSEC_SHIFTED_OFFSET: usize = offset_of!(VvarData, realtime_nsec_shifted);
pub const VVAR_MONOTONIC_SEC_OFFSET: usize = offset_of!(VvarData, monotonic_sec);
pub const VVAR_MONOTONIC_NSEC_SHIFTED_OFFSET: usize = offset_of!(VvarData, monotonic_nsec_shifted);
pub const VVAR_CYCLE_LAST_OFFSET: usize = offset_of!(VvarData, cycle_last);
pub const VVAR_MULT_OFFSET: usize = offset_of!(VvarData, mult);
pub const VVAR_SHIFT_OFFSET: usize = offset_of!(VvarData, shift);
pub const VVAR_MASK_OFFSET: usize = offset_of!(VvarData, mask);
pub const VVAR_REALTIME_GENERATION_OFFSET: usize = offset_of!(VvarData, realtime_generation);

const _: [(); VVAR_PAGE_SIZE] = [(); VVAR_DATA_SIZE];
