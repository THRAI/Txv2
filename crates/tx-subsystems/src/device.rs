//! Device and block-handle shells shared by devfs, bdev-fs, and backends.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::future::{poll_fn, Future};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::task::Poll;
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

use crate::adapter::step_engine::{
    self as step_engine, ByteProgress, Cap, NoProgress, ScriptCtx, SpinMutex, StepOp, StepOutcome,
    SubjectIdentity, Weak,
};
use crate::adapter::wait_routing::WaitOutcome;

use crate::execution::{Errno, Guard};
use crate::io_manager::backend::{BlockPageCompletion, BlockPageRequestTracker, PageFrameRef};
use crate::io_manager::block::{
    BioVec, BlockCompletionSource, BlockDeviceCompletion, BlockDispatch, BlockDispatchExecutor,
    BlockFlags, BlockOp, BlockQueue, BlockServiceDriver, BlockServiceNext, BlockTag, BlockTagTable,
    DeviceKey,
};
use crate::io_manager::page::service::{
    PageService, PageServiceBackendDriven, PageServiceTaggedBlockCompletionError,
};
use crate::io_manager::page::PageIoOp;
use crate::io_manager::runtime::{
    IoServiceKind, QueueDepth, ServiceBudget, ServiceKick, ServiceWakeSource,
};
use crate::page_backed::{
    BlockSubmissionHandle, FileBlockServiceTurn, Frame, PageContainer, PageIoSubmissionHandle,
};
use tx_services::time::DeadlineRegistrar;

const MAX_STATIC_BLOCK_DEVICES: usize = 16;
const FILE_IO_SERVICE_SOURCE_ID_BASE: u64 = 0x7200;

/// 文件对象的通用读写接口。网络套接字通过该接口接入 VFS，避免 VFS
/// 直接依赖具体的套接字类型。
pub trait FileOps: Send + Sync {
    fn read(
        &self,
        out: &mut [u8],
        nonblocking: bool,
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress>;

    fn write(
        &self,
        bytes: &[u8],
        nonblocking: bool,
        guard: &Guard<'_>,
    ) -> StepOutcome<usize, ByteProgress>;

    fn on_set_fl_nonblock(&self, post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool) {
        let _ = post;
    }

    fn on_last_close(&self, guard: &Guard<'_>) {
        let _ = guard;
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DevT(u64);

impl DevT {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self(((major as u64) << 32) | minor as u64)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn major(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub const fn minor(self) -> u32 {
        self.0 as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhysicalBlockNumber(u64);

impl PhysicalBlockNumber {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcTime {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

impl RtcTime {
    pub fn from_unix_ns(ns: u64) -> Result<Self, RtcError> {
        let seconds = ns / 1_000_000_000;
        if seconds > i64::MAX as u64 {
            return Err(RtcError::Range);
        }
        Self::from_unix_seconds(seconds as i64)
    }

    pub fn from_unix_seconds(seconds: i64) -> Result<Self, RtcError> {
        let days = seconds.div_euclid(86_400);
        let rem = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days).ok_or(RtcError::Range)?;
        let tm_yday = day_of_year(year, month, day) as i32;
        Ok(Self {
            tm_sec: (rem % 60) as i32,
            tm_min: ((rem / 60) % 60) as i32,
            tm_hour: (rem / 3_600) as i32,
            tm_mday: day as i32,
            tm_mon: month as i32 - 1,
            tm_year: year - 1900,
            tm_wday: (days + 4).rem_euclid(7) as i32,
            tm_yday,
            tm_isdst: 0,
        })
    }

    pub fn to_unix_ns(self) -> Result<u64, RtcError> {
        let year = self.tm_year.checked_add(1900).ok_or(RtcError::Range)?;
        let month = self.tm_mon.checked_add(1).ok_or(RtcError::InvalidTime)?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&self.tm_mday)
            || !(0..=23).contains(&self.tm_hour)
            || !(0..=59).contains(&self.tm_min)
            || !(0..=59).contains(&self.tm_sec)
        {
            return Err(RtcError::InvalidTime);
        }
        let month = month as u32;
        let day = self.tm_mday as u32;
        let days = days_from_civil(year, month, day).ok_or(RtcError::Range)?;
        let roundtrip = civil_from_days(days).ok_or(RtcError::Range)?;
        if roundtrip != (year, month, day) {
            return Err(RtcError::InvalidTime);
        }
        if days < 0 {
            return Err(RtcError::Range);
        }
        let day_seconds = (self.tm_hour as i64)
            .checked_mul(3_600)
            .and_then(|v| v.checked_add((self.tm_min as i64) * 60))
            .and_then(|v| v.checked_add(self.tm_sec as i64))
            .ok_or(RtcError::Range)?;
        let seconds = days
            .checked_mul(86_400)
            .and_then(|v| v.checked_add(day_seconds))
            .ok_or(RtcError::Range)?;
        let seconds = u64::try_from(seconds).map_err(|_| RtcError::Range)?;
        seconds.checked_mul(1_000_000_000).ok_or(RtcError::Range)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcAlarm {
    pub time: RtcTime,
    pub enabled: bool,
    pub pending: bool,
}

#[derive(Clone, Copy)]
pub struct RtcAlarmEmulation<'a> {
    pub registrar: &'a dyn DeadlineRegistrar,
    pub monotonic_deadline_ns: u64,
}

impl<'a> RtcAlarmEmulation<'a> {
    pub fn new(registrar: &'a dyn DeadlineRegistrar, monotonic_deadline_ns: u64) -> Self {
        Self {
            registrar,
            monotonic_deadline_ns,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RtcEventMask(u32);

impl RtcEventMask {
    pub const UPDATE: Self = Self(1 << 0);
    pub const ALARM: Self = Self(1 << 1);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & (Self::UPDATE.bits() | Self::ALARM.bits()))
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcError {
    Unsupported,
    InvalidTime,
    Range,
    Hardware,
}

impl From<RtcError> for Errno {
    fn from(value: RtcError) -> Self {
        match value {
            RtcError::Unsupported => Errno::EOPNOTSUPP,
            RtcError::InvalidTime | RtcError::Range => Errno::EINVAL,
            RtcError::Hardware => Errno::ENODEV,
        }
    }
}

pub trait RtcDeviceOps: Send + Sync + 'static {
    fn read_time(&self, _guard: &Guard<'_>) -> Result<RtcTime, RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_time(&self, _time: RtcTime, _guard: &Guard<'_>) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn read_alarm(&self, _guard: &Guard<'_>) -> Result<RtcAlarm, RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_alarm(&self, _alarm: RtcAlarm, _guard: &Guard<'_>) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_alarm_with_emulation(
        &self,
        alarm: RtcAlarm,
        guard: &Guard<'_>,
        _emulation: Option<RtcAlarmEmulation<'_>>,
    ) -> Result<(), RtcError> {
        self.set_alarm(alarm, guard)
    }

    fn poll_events(&self, _guard: &Guard<'_>) -> Result<RtcEventMask, RtcError> {
        Ok(RtcEventMask::empty())
    }
}

pub trait CharDeviceOps: Send + Sync + 'static {
    fn read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress>;
    fn write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress>;

    fn rtc_ops(&self) -> Option<&dyn RtcDeviceOps> {
        None
    }
}

fn civil_from_days(days: i64) -> Option<(i32, u32, u32)> {
    let z = days.checked_add(719_468)?;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    if year < i32::MIN as i64 || year > i32::MAX as i64 {
        return None;
    }
    Some((year as i32, month as u32, day as u32))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 }.div_euclid(400);
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
}

fn day_of_year(year: i32, month: u32, day: u32) -> u32 {
    const COMMON: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    const LEAP: [u32; 12] = [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335];
    let idx = month.saturating_sub(1) as usize;
    let base = if is_leap_year(year) {
        LEAP.get(idx).copied().unwrap_or(0)
    } else {
        COMMON.get(idx).copied().unwrap_or(0)
    };
    base + day.saturating_sub(1)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[derive(Clone, Copy)]
pub struct CharDeviceBinding {
    pub devt: DevT,
    pub name: &'static str,
    pub ops: &'static dyn CharDeviceOps,
}

impl fmt::Debug for CharDeviceBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CharDeviceBinding")
            .field("devt", &self.devt)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

pub trait BlockDeviceOps: Send + Sync + 'static {
    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress>;

    fn barrier(&self, guard: &Guard<'_>) -> StepOutcome<(), NoProgress>;

    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        BlockDurabilityCapabilities::NONE
    }

    fn write_blocks_with_options(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        options: BlockWriteOptions,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        if options.fua {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        self.write_blocks(block_id, source, guard)
    }

    /// Whether requests may remain in the device after the submission call
    /// returns. Synchronous board drivers keep the default implementation.
    fn supports_async_blocks(&self) -> bool {
        false
    }

    fn submit_read_blocks_async(
        &self,
        cookie: u64,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        let _ = cookie;
        BlockAsyncSubmit::Complete(step_result_to_result(
            self.read_blocks(block_id, target, guard),
        ))
    }

    fn submit_write_blocks_async(
        &self,
        cookie: u64,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        options: BlockWriteOptions,
        guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        let _ = cookie;
        BlockAsyncSubmit::Complete(step_result_to_result(
            self.write_blocks_with_options(block_id, source, options, guard),
        ))
    }

    /// Drain at most `budget` terminal completions. Every completion must
    /// return the submission cookie unchanged.
    fn poll_async_blocks(&self, _budget: usize, _guard: &Guard<'_>) -> Vec<BlockAsyncCompletion> {
        Vec::new()
    }

    fn read_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
    ) -> StepOutcome<(), NoProgress> {
        let guard = crate::adapter::step_engine::guard();
        self.read_blocks(block_id, target, &guard)
    }

    fn write_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
    ) -> StepOutcome<(), NoProgress> {
        let guard = crate::adapter::step_engine::guard();
        self.write_blocks(block_id, source, &guard)
    }

    fn barrier_bootstrap(&self) -> StepOutcome<(), NoProgress> {
        let guard = crate::adapter::step_engine::guard();
        self.barrier(&guard)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockDurabilityCapabilities {
    pub fua: bool,
    pub flush: bool,
}

impl BlockDurabilityCapabilities {
    pub const NONE: Self = Self {
        fua: false,
        flush: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockWriteOptions {
    pub fua: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockAsyncSubmit {
    Complete(Result<(), Errno>),
    Submitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockAsyncCompletion {
    pub cookie: u64,
    pub result: Result<(), Errno>,
}

impl BlockAsyncCompletion {
    pub const fn new(cookie: u64, result: Result<(), Errno>) -> Self {
        Self { cookie, result }
    }
}

pub trait BlockDevice: BlockDeviceOps {
    fn total_blocks(&self) -> u64;
    fn block_size(&self) -> u32;
}

#[derive(Clone, Copy)]
pub struct BlockDeviceRegistration {
    pub devt: DevT,
    pub name: &'static str,
    pub ops: &'static dyn BlockDevice,
}

impl fmt::Debug for BlockDeviceRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockDeviceRegistration")
            .field("devt", &self.devt)
            .field("name", &self.name)
            .field("total_blocks", &self.ops.total_blocks())
            .field("block_size", &self.ops.block_size())
            .finish()
    }
}

#[derive(Clone, Copy)]
pub struct BlockDeviceHandle {
    reg: &'static BlockDeviceRegistration,
    start_lba: u64,
    len_lba: u64,
}

/// Why a requested partition cannot be represented as a bounded device slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockDeviceRangeError {
    Empty,
    Overflow,
    OutOfBounds,
}

impl BlockDeviceHandle {
    pub fn whole(reg: &'static BlockDeviceRegistration) -> Self {
        Self {
            reg,
            start_lba: 0,
            len_lba: reg.ops.total_blocks(),
        }
    }

    pub fn partition(
        reg: &'static BlockDeviceRegistration,
        start_lba: u64,
        len_lba: u64,
    ) -> Result<Self, BlockDeviceRangeError> {
        if len_lba == 0 {
            return Err(BlockDeviceRangeError::Empty);
        }
        let end_lba = start_lba
            .checked_add(len_lba)
            .ok_or(BlockDeviceRangeError::Overflow)?;
        if end_lba > reg.ops.total_blocks() {
            return Err(BlockDeviceRangeError::OutOfBounds);
        }
        Ok(Self {
            reg,
            start_lba,
            len_lba,
        })
    }

    pub const fn registration(self) -> &'static BlockDeviceRegistration {
        self.reg
    }

    pub const fn start_lba(self) -> u64 {
        self.start_lba
    }

    pub const fn len_lba(self) -> u64 {
        self.len_lba
    }

    /// Number of parent-device blocks covered by one 4 KiB `Frame`.
    pub fn blocks_per_frame(self) -> Option<u64> {
        let block_size = u64::from(self.reg.ops.block_size());
        let frame_size = crate::vm::USER_PAGE_SIZE as u64;
        if block_size == 0 || !frame_size.is_multiple_of(block_size) {
            return None;
        }
        Some(frame_size / block_size)
    }

    pub fn read_blocks(
        self,
        lba_offset: u64,
        target: &mut [Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(block_id) = self.block_id_for(lba_offset, target.len() as u64) else {
            return StepOutcome::err(Errno::EINVAL.into());
        };
        self.reg.ops.read_blocks(block_id, target, guard)
    }

    pub fn write_blocks(
        self,
        lba_offset: u64,
        source: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(block_id) = self.block_id_for(lba_offset, source.len() as u64) else {
            return StepOutcome::err(Errno::EINVAL.into());
        };
        self.reg.ops.write_blocks(block_id, source, guard)
    }

    pub fn write_blocks_with_options(
        self,
        lba_offset: u64,
        source: &[Frame],
        options: BlockWriteOptions,
        guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let Some(block_id) = self.block_id_for(lba_offset, source.len() as u64) else {
            return StepOutcome::err(Errno::EINVAL.into());
        };
        if options.fua && !self.reg.ops.durability_capabilities().fua {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        self.reg
            .ops
            .write_blocks_with_options(block_id, source, options, guard)
    }

    pub fn barrier(self, guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if !self.reg.ops.durability_capabilities().flush {
            return StepOutcome::err(Errno::EOPNOTSUPP.into());
        }
        self.reg.ops.barrier(guard)
    }

    pub fn supports_async_blocks(self) -> bool {
        self.reg.ops.supports_async_blocks()
    }

    pub fn submit_read_blocks_async(
        self,
        cookie: u64,
        lba_offset: u64,
        target: &mut [Frame],
        guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        let Some(block_id) = self.block_id_for(lba_offset, target.len() as u64) else {
            return BlockAsyncSubmit::Complete(Err(Errno::EINVAL));
        };
        self.reg
            .ops
            .submit_read_blocks_async(cookie, block_id, target, guard)
    }

    pub fn submit_write_blocks_async(
        self,
        cookie: u64,
        lba_offset: u64,
        source: &[Frame],
        options: BlockWriteOptions,
        guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        let Some(block_id) = self.block_id_for(lba_offset, source.len() as u64) else {
            return BlockAsyncSubmit::Complete(Err(Errno::EINVAL));
        };
        if options.fua && !self.reg.ops.durability_capabilities().fua {
            return BlockAsyncSubmit::Complete(Err(Errno::EOPNOTSUPP));
        }
        self.reg
            .ops
            .submit_write_blocks_async(cookie, block_id, source, options, guard)
    }

    pub fn poll_async_blocks(self, budget: usize, guard: &Guard<'_>) -> Vec<BlockAsyncCompletion> {
        self.reg.ops.poll_async_blocks(budget, guard)
    }

    fn block_id_for(self, lba_offset: u64, count: u64) -> Option<PhysicalBlockNumber> {
        if count == 0 {
            return None;
        }
        let parent_blocks = count.checked_mul(self.blocks_per_frame()?)?;
        let end = lba_offset.checked_add(parent_blocks)?;
        if end > self.len_lba {
            return None;
        }
        Some(PhysicalBlockNumber::new(
            self.start_lba.checked_add(lba_offset)?,
        ))
    }
}

impl fmt::Debug for BlockDeviceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockDeviceHandle")
            .field("reg", &self.reg)
            .field("start_lba", &self.start_lba)
            .field("len_lba", &self.len_lba)
            .finish()
    }
}

const ASYNC_BLOCK_POLL_BUDGET: usize = 32;
static NEXT_ASYNC_BLOCK_COOKIE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct AsyncBlockRoute {
    namespace: u64,
    tag: BlockTag,
}

static ASYNC_BLOCK_ROUTES: SpinMutex<BTreeMap<u64, AsyncBlockRoute>> =
    SpinMutex::new(BTreeMap::new());
static ROUTED_ASYNC_BLOCK_COMPLETIONS: SpinMutex<BTreeMap<u64, VecDeque<BlockDeviceCompletion>>> =
    SpinMutex::new(BTreeMap::new());

pub struct BlockDeviceDispatchAdapter<'a, 'g> {
    handle: BlockDeviceHandle,
    guard: &'a Guard<'g>,
    completions: VecDeque<BlockDeviceCompletion>,
    async_namespace: Option<u64>,
}

impl<'a, 'g> BlockDeviceDispatchAdapter<'a, 'g> {
    pub fn new(handle: BlockDeviceHandle, guard: &'a Guard<'g>) -> Self {
        Self {
            handle,
            guard,
            completions: VecDeque::new(),
            async_namespace: None,
        }
    }

    /// Build the production L6 adapter. The namespace belongs to the
    /// persistent block-submission manager, so completions remain routable
    /// even though this bounded adapter is reconstructed on every turn.
    pub fn new_async(handle: BlockDeviceHandle, guard: &'a Guard<'g>, namespace: u64) -> Self {
        Self {
            handle,
            guard,
            completions: VecDeque::new(),
            async_namespace: Some(namespace),
        }
    }

    fn execute_dispatch(&self, dispatch: &BlockDispatch) -> Result<(), Errno> {
        if dispatch.bio.plan.device != DeviceKey::new(self.handle.registration().devt.raw()) {
            return Err(Errno::ENODEV);
        }

        match dispatch.bio.plan.op {
            BlockOp::Read => {
                let mut frames = frames_from_bio_vecs(&dispatch.bio.plan.vecs)?;
                step_result_to_result(self.handle.read_blocks(
                    dispatch.bio.plan.lba.start_lba(),
                    &mut frames,
                    self.guard,
                ))
            }
            BlockOp::Write => {
                let frames = frames_from_bio_vecs(&dispatch.bio.plan.vecs)?;
                step_result_to_result(self.handle.write_blocks_with_options(
                    dispatch.bio.plan.lba.start_lba(),
                    &frames,
                    BlockWriteOptions {
                        fua: dispatch.bio.plan.flags.contains(BlockFlags::FUA),
                    },
                    self.guard,
                ))
            }
            BlockOp::Flush | BlockOp::Barrier => {
                step_result_to_result(self.handle.barrier(self.guard))
            }
        }
    }

    fn submit_async_dispatch(&mut self, dispatch: &BlockDispatch, namespace: u64) {
        if dispatch.bio.plan.device != DeviceKey::new(self.handle.registration().devt.raw()) {
            self.completions
                .push_back(BlockDeviceCompletion::new(dispatch.tag, Err(Errno::ENODEV)));
            return;
        }

        let cookie = NEXT_ASYNC_BLOCK_COOKIE
            .fetch_add(1, Ordering::AcqRel)
            .max(1);
        ASYNC_BLOCK_ROUTES.lock().insert(
            cookie,
            AsyncBlockRoute {
                namespace,
                tag: dispatch.tag,
            },
        );
        let submitted = match dispatch.bio.plan.op {
            BlockOp::Read => match frames_from_bio_vecs(&dispatch.bio.plan.vecs) {
                Ok(mut frames) => self.handle.submit_read_blocks_async(
                    cookie,
                    dispatch.bio.plan.lba.start_lba(),
                    &mut frames,
                    self.guard,
                ),
                Err(error) => BlockAsyncSubmit::Complete(Err(error)),
            },
            BlockOp::Write => match frames_from_bio_vecs(&dispatch.bio.plan.vecs) {
                Ok(frames) => self.handle.submit_write_blocks_async(
                    cookie,
                    dispatch.bio.plan.lba.start_lba(),
                    &frames,
                    BlockWriteOptions {
                        fua: dispatch.bio.plan.flags.contains(BlockFlags::FUA),
                    },
                    self.guard,
                ),
                Err(error) => BlockAsyncSubmit::Complete(Err(error)),
            },
            BlockOp::Flush | BlockOp::Barrier => {
                BlockAsyncSubmit::Complete(step_result_to_result(self.handle.barrier(self.guard)))
            }
        };

        if let BlockAsyncSubmit::Complete(result) = submitted {
            ASYNC_BLOCK_ROUTES.lock().remove(&cookie);
            self.completions
                .push_back(BlockDeviceCompletion::new(dispatch.tag, result));
        }
    }

    fn take_routed_completion(&mut self, namespace: u64) -> Option<BlockDeviceCompletion> {
        let mut routed = ROUTED_ASYNC_BLOCK_COMPLETIONS.lock();
        let completion = routed.get_mut(&namespace)?.pop_front();
        if routed.get(&namespace).is_some_and(VecDeque::is_empty) {
            routed.remove(&namespace);
        }
        completion
    }

    fn poll_async_completion(&mut self, namespace: u64) -> Option<BlockDeviceCompletion> {
        if let Some(completion) = self.take_routed_completion(namespace) {
            return Some(completion);
        }

        for completion in self
            .handle
            .poll_async_blocks(ASYNC_BLOCK_POLL_BUDGET, self.guard)
        {
            let Some(route) = ASYNC_BLOCK_ROUTES.lock().remove(&completion.cookie) else {
                continue;
            };
            let routed = BlockDeviceCompletion::new(route.tag, completion.result);
            if route.namespace == namespace {
                self.completions.push_back(routed);
            } else {
                ROUTED_ASYNC_BLOCK_COMPLETIONS
                    .lock()
                    .entry(route.namespace)
                    .or_default()
                    .push_back(routed);
            }
        }
        self.completions.pop_front()
    }
}

impl BlockDispatchExecutor for BlockDeviceDispatchAdapter<'_, '_> {
    fn submit(&mut self, dispatch: &BlockDispatch) {
        if let Some(namespace) = self.async_namespace {
            self.submit_async_dispatch(dispatch, namespace);
        } else {
            let result = self.execute_dispatch(dispatch);
            self.completions
                .push_back(BlockDeviceCompletion::new(dispatch.tag, result));
        }
    }
}

impl BlockCompletionSource for BlockDeviceDispatchAdapter<'_, '_> {
    fn poll_completion(&mut self) -> Option<BlockDeviceCompletion> {
        self.completions.pop_front().or_else(|| {
            self.async_namespace
                .and_then(|namespace| self.poll_async_completion(namespace))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockDeviceServiceTurn {
    pub dispatched: usize,
    pub device_completions: usize,
    pub page_completions: usize,
    pub next: BlockServiceNext,
    pub kicks: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockDeviceServiceError {
    Page(PageServiceTaggedBlockCompletionError),
}

impl From<PageServiceTaggedBlockCompletionError> for BlockDeviceServiceError {
    fn from(error: PageServiceTaggedBlockCompletionError) -> Self {
        Self::Page(error)
    }
}

pub fn drive_block_device_service_once<F>(
    handle: BlockDeviceHandle,
    guard: &Guard<'_>,
    block_driver: &mut BlockServiceDriver,
    block_queue: &mut BlockQueue,
    depth: &mut QueueDepth,
    tags: &mut BlockTagTable,
    tracker: &mut BlockPageRequestTracker,
    page_service: &mut PageService,
    kick: F,
) -> Result<BlockDeviceServiceTurn, BlockDeviceServiceError>
where
    F: FnMut(ServiceKick) -> bool,
{
    let mut adapter = BlockDeviceDispatchAdapter::new(handle, guard);
    let driven =
        block_driver.drive_once_with_executor(block_queue, depth, tags, &mut adapter, kick);
    let mut device_completions = 0usize;
    let mut page_completions = 0usize;

    while let Some(completion) = adapter.poll_completion() {
        device_completions += 1;
        let outcome = page_service.push_tagged_block_completion(
            tags,
            depth,
            tracker,
            completion.tag,
            completion.result,
            page_frame_ref_for_block_completion,
        )?;
        page_completions += outcome.queued;
    }

    Ok(BlockDeviceServiceTurn {
        dispatched: driven.step.dispatches.len(),
        device_completions,
        page_completions,
        next: driven.step.next,
        kicks: driven.kicks,
    })
}

pub fn drive_page_container_file_block_device_service_once<F>(
    container: &PageContainer,
    budget: ServiceBudget,
    handle: BlockDeviceHandle,
    guard: &Guard<'_>,
    kick: F,
) -> Result<FileBlockServiceTurn, PageServiceTaggedBlockCompletionError>
where
    F: FnMut(ServiceKick) -> bool,
{
    let mut adapter =
        BlockDeviceDispatchAdapter::new_async(handle, guard, container.file_io_block_namespace());
    container.drive_file_block_io_service_once(
        budget,
        &mut adapter,
        page_frame_ref_for_block_completion,
        kick,
    )
}

#[derive(Debug)]
pub struct PageContainerFileIoServiceTurn {
    pub page_before: Option<PageServiceBackendDriven>,
    pub block: FileBlockServiceTurn,
    pub page_after: Option<PageServiceBackendDriven>,
    pub next: PageContainerFileIoServiceNext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageContainerFileIoServiceNext {
    Runnable,
    WaitingForCompletion,
    Sleeping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageContainerFileIoServiceTaskConfig {
    pub max_ready_turns: Option<usize>,
    pub page_budget: ServiceBudget,
    pub block_budget: ServiceBudget,
}

impl PageContainerFileIoServiceTaskConfig {
    pub const fn run_forever(page_budget: ServiceBudget, block_budget: ServiceBudget) -> Self {
        Self {
            max_ready_turns: None,
            page_budget,
            block_budget,
        }
    }

    pub const fn run_turns(
        max_ready_turns: usize,
        page_budget: ServiceBudget,
        block_budget: ServiceBudget,
    ) -> Self {
        Self {
            max_ready_turns: Some(max_ready_turns),
            page_budget,
            block_budget,
        }
    }
}

#[derive(Debug, Default)]
pub struct PageContainerFileIoServiceTaskReport {
    pub waits_ready: usize,
    pub waits_failed: usize,
    pub ready_turns: usize,
    pub dispatched: usize,
    pub device_completions: usize,
    pub page_completions: usize,
    pub self_kicks: usize,
    pub last_turn: Option<PageContainerFileIoServiceTurn>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FileIoManagerRuntimeRegistrationId(u64);

impl FileIoManagerRuntimeRegistrationId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Move-only reactor custody for one file-I/O manager pair.
///
/// The weak endpoint is used only to check whether the PageBacked semantic
/// owner is still live for a bounded service turn. It is never upgraded across
/// an await. The typed L4/L6 handles keep request and block-manager state out
/// of the PageContainer lifetime.
pub struct FileIoManagerRuntimeClaim {
    registration: FileIoManagerRuntimeRegistrationId,
    container: Weak<PageContainer>,
    page_submission: PageIoSubmissionHandle,
    block_submission: BlockSubmissionHandle,
    handle: BlockDeviceHandle,
    wake_source: Arc<ServiceWakeSource>,
}

impl FileIoManagerRuntimeClaim {
    pub const fn registration_id(&self) -> FileIoManagerRuntimeRegistrationId {
        self.registration
    }

    pub const fn handle(&self) -> BlockDeviceHandle {
        self.handle
    }

    pub fn wake_source(&self) -> &Arc<ServiceWakeSource> {
        &self.wake_source
    }

    pub fn kick(&self, service: IoServiceKind) -> usize {
        post_file_io_service_kick(&self.wake_source, ServiceKick::new(service)) as usize
    }

    fn retain_manager_custody(&self) {
        // These handles are intentionally owned by the claim, rather than by
        // a long-lived PageContainer capability held in a reactor future.
        let _ = (&self.page_submission, &self.block_submission);
    }

    /// End this claim's reactor ownership and release its typed manager
    /// handles. Drop performs the same idempotent cleanup if the future exits
    /// early or is cancelled.
    pub fn retire(mut self) -> bool {
        let registration = core::mem::replace(
            &mut self.registration,
            FileIoManagerRuntimeRegistrationId(0),
        );
        retire_file_io_manager_runtime(registration)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn detached_for_test(
        container: Cap<PageContainer>,
        handle: BlockDeviceHandle,
        wake_source: Arc<ServiceWakeSource>,
    ) -> Self {
        let (page_submission, block_submission) = container.file_io_runtime_handles();
        Self {
            registration: FileIoManagerRuntimeRegistrationId(0),
            container: container.downgrade(),
            page_submission,
            block_submission,
            handle,
            wake_source,
        }
    }
}

impl Drop for FileIoManagerRuntimeClaim {
    fn drop(&mut self) {
        let registration = core::mem::replace(
            &mut self.registration,
            FileIoManagerRuntimeRegistrationId(0),
        );
        let _ = retire_file_io_manager_runtime(registration);
    }
}

struct FileIoManagerRuntimePayload {
    page_submission: PageIoSubmissionHandle,
    block_submission: BlockSubmissionHandle,
    handle: BlockDeviceHandle,
}

enum FileIoManagerRuntimeState {
    Pending(FileIoManagerRuntimePayload),
    Claimed,
}

struct RegisteredFileIoServiceRuntime {
    id: FileIoManagerRuntimeRegistrationId,
    container: Weak<PageContainer>,
    handle: BlockDeviceHandle,
    source_id: u64,
    wake_source: Arc<ServiceWakeSource>,
    state: FileIoManagerRuntimeState,
}

#[derive(Clone, Copy, Debug)]
pub struct PageContainerFileIoServiceRuntimeSnapshot {
    container: Weak<PageContainer>,
    handle: BlockDeviceHandle,
    source_id: u64,
    claimed: bool,
}

impl PageContainerFileIoServiceRuntimeSnapshot {
    pub const fn handle(&self) -> BlockDeviceHandle {
        self.handle
    }

    pub const fn source_id(&self) -> u64 {
        self.source_id
    }

    pub const fn is_claimed(&self) -> bool {
        self.claimed
    }

    pub fn is_live(&self, guard: &Guard<'_>) -> bool {
        self.container.upgrade(guard).is_some()
    }

    pub fn diagnostic_counts(
        &self,
        guard: &Guard<'_>,
    ) -> Option<(usize, usize, usize, usize, usize, usize, usize, bool)> {
        self.container
            .upgrade(guard)
            .map(|container| container.file_io_diagnostic_counts())
    }
}

/// Kernel-owned task submission for a registered file-I/O service runtime.
///
/// The device subsystem retains the runtime registry and exactly-once claim
/// state. The reactor owner installs this narrow callback after it is ready to
/// accept long-lived service futures.
pub trait FileIoServiceRuntimeSpawner: Send + Sync {
    fn spawn_file_io_service(&self, claim: FileIoManagerRuntimeClaim);
}

static FILE_IO_SERVICE_RUNTIMES: SpinMutex<Vec<RegisteredFileIoServiceRuntime>> =
    SpinMutex::new(Vec::new());
static FILE_IO_SERVICE_RUNTIME_SPAWNER: SpinMutex<Option<Arc<dyn FileIoServiceRuntimeSpawner>>> =
    SpinMutex::new(None);
static NEXT_FILE_IO_SERVICE_SOURCE_ID: AtomicU64 = AtomicU64::new(FILE_IO_SERVICE_SOURCE_ID_BASE);
static NEXT_FILE_IO_SERVICE_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

pub fn register_page_container_file_io_service(
    container: Cap<PageContainer>,
    handle: BlockDeviceHandle,
) -> FileIoManagerRuntimeRegistrationId {
    let source_id = NEXT_FILE_IO_SERVICE_SOURCE_ID.fetch_add(1, Ordering::AcqRel);
    let wake_source = Arc::new(ServiceWakeSource::new(source_id));
    let container_weak = container.downgrade();
    if !container.attach_file_io_wake_source(Arc::clone(&wake_source)) {
        // The PageContainer owns exactly one L4/L6 wake attachment. A second
        // registration must reuse its existing runtime metadata instead of
        // spawning a task on an unattached source that can never be kicked.
        return FILE_IO_SERVICE_RUNTIMES
            .lock()
            .iter()
            .find(|entry| {
                entry.container.raw() == container_weak.raw()
                    && entry.container.generation() == container_weak.generation()
            })
            .map(|entry| entry.id)
            .unwrap_or(FileIoManagerRuntimeRegistrationId(0));
    }
    let (page_submission, block_submission) = container.file_io_runtime_handles();
    let id = FileIoManagerRuntimeRegistrationId(
        NEXT_FILE_IO_SERVICE_RUNTIME_ID.fetch_add(1, Ordering::AcqRel),
    );
    FILE_IO_SERVICE_RUNTIMES
        .lock()
        .push(RegisteredFileIoServiceRuntime {
            id,
            container: container_weak,
            handle,
            source_id,
            wake_source: Arc::clone(&wake_source),
            state: FileIoManagerRuntimeState::Pending(FileIoManagerRuntimePayload {
                page_submission,
                block_submission,
                handle,
            }),
        });
    let _ = submit_pending_file_io_service_runtimes();
    id
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileIoServiceWait {
    Ready,
    OwnerRetired,
    Failed,
}

/// Install the edge subscription first, then re-check the manager's latched
/// owner-retired predicate. This closes both sides of the lost-wake race:
/// retirement before subscription is observed through the latch, while
/// retirement after subscription posts the normal wake edge.
async fn wait_for_file_io_service(claim: &FileIoManagerRuntimeClaim) -> FileIoServiceWait {
    let wait = crate::wait_source::wait_on_registered_endpoint(
        claim.wake_source.wake_endpoint(),
        file_io_service_interest_mask(),
    );
    let mut wait = core::pin::pin!(wait);
    poll_fn(|cx| {
        let outcome = wait.as_mut().poll(cx);
        if claim.page_submission.owner_retired() {
            return Poll::Ready(FileIoServiceWait::OwnerRetired);
        }
        match outcome {
            Poll::Ready(WaitOutcome::Ready) => Poll::Ready(FileIoServiceWait::Ready),
            Poll::Ready(_) => Poll::Ready(FileIoServiceWait::Failed),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Install the single kernel-owned spawner and drain every runtime registered
/// before reactor availability. A second install is rejected so an already
/// claimed service can never be submitted through another reactor owner.
pub fn install_file_io_service_runtime_spawner(
    spawner: Arc<dyn FileIoServiceRuntimeSpawner>,
) -> Option<usize> {
    {
        let mut slot = FILE_IO_SERVICE_RUNTIME_SPAWNER.lock();
        if slot.is_some() {
            return None;
        }
        *slot = Some(spawner);
    }
    Some(submit_pending_file_io_service_runtimes())
}

/// Submit each registered runtime at most once after a kernel spawner exists.
/// Runtime claims happen under the registry lock; invoking the kernel spawner
/// happens after both the registry and spawner locks have been released.
pub fn submit_pending_file_io_service_runtimes() -> usize {
    let Some(spawner) = FILE_IO_SERVICE_RUNTIME_SPAWNER.lock().clone() else {
        return 0;
    };
    let pending = claim_pending_file_io_service_runtimes();
    let submitted = pending.len();
    for runtime in pending {
        spawner.spawn_file_io_service(runtime);
    }
    submitted
}

fn claim_pending_file_io_service_runtimes() -> Vec<FileIoManagerRuntimeClaim> {
    // Registration can run from a VFS/exec path that already pins the current
    // CPU's epoch. Reuse that guard rather than nesting another EBR guard;
    // callers without an active guard still acquire a fresh one.
    let guard = tx_substrate::epoch::borrow_current_guard().unwrap_or_else(step_engine::guard);
    {
        let mut runtimes = FILE_IO_SERVICE_RUNTIMES.lock();
        let mut pending = Vec::new();
        runtimes.retain_mut(|entry| {
            if entry.container.upgrade(&guard).is_none() {
                return false;
            }
            if matches!(entry.state, FileIoManagerRuntimeState::Pending(_)) {
                let FileIoManagerRuntimeState::Pending(payload) =
                    core::mem::replace(&mut entry.state, FileIoManagerRuntimeState::Claimed)
                else {
                    unreachable!("pending claim state was checked above");
                };
                pending.push(FileIoManagerRuntimeClaim {
                    registration: entry.id,
                    container: entry.container,
                    page_submission: payload.page_submission,
                    block_submission: payload.block_submission,
                    handle: payload.handle,
                    wake_source: Arc::clone(&entry.wake_source),
                });
            }
            true
        });
        pending
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn claim_pending_file_io_service_runtimes_for_test() -> Vec<FileIoManagerRuntimeClaim> {
    claim_pending_file_io_service_runtimes()
}

pub fn page_container_file_io_service_runtimes_snapshot(
) -> Vec<PageContainerFileIoServiceRuntimeSnapshot> {
    FILE_IO_SERVICE_RUNTIMES
        .lock()
        .iter()
        .map(|entry| PageContainerFileIoServiceRuntimeSnapshot {
            container: entry.container,
            handle: entry.handle,
            source_id: entry.source_id,
            claimed: matches!(entry.state, FileIoManagerRuntimeState::Claimed),
        })
        .collect()
}

pub fn page_container_file_io_service_runtime_count() -> usize {
    FILE_IO_SERVICE_RUNTIMES.lock().len()
}

/// Wake claimed service tasks whose semantic `PageContainer` owner has
/// crossed the Zone no-upgrade barrier.
///
/// `Weak<PageContainer>` becomes stale at the final `Cap` transition, while
/// the payload destructor runs only after a later EBR grace period. A task
/// already parked on its service source therefore cannot rely on
/// `PageContainer::drop` as its only lifetime edge. Reactor idle maintenance
/// calls this bounded registry scan; the posted Page kick is latched by the
/// wake source and lets the task observe the stale Weak and retire itself.
pub fn wake_unowned_file_io_service_runtimes() -> usize {
    let guard = tx_substrate::epoch::borrow_current_guard().unwrap_or_else(step_engine::guard);
    let wake_sources = FILE_IO_SERVICE_RUNTIMES
        .lock()
        .iter()
        .filter(|entry| {
            matches!(entry.state, FileIoManagerRuntimeState::Claimed)
                && entry.container.upgrade(&guard).is_none()
        })
        .map(|entry| Arc::clone(&entry.wake_source))
        .collect::<Vec<_>>();
    drop(guard);

    let mut posted = 0;
    for wake_source in wake_sources {
        posted += usize::from(post_file_io_service_kick(
            &wake_source,
            ServiceKick::new(IoServiceKind::Page),
        ));
    }
    posted
}

fn retire_file_io_manager_runtime(id: FileIoManagerRuntimeRegistrationId) -> bool {
    if id.raw() == 0 {
        return false;
    }
    let mut runtimes = FILE_IO_SERVICE_RUNTIMES.lock();
    let initial_len = runtimes.len();
    runtimes.retain(|entry| entry.id != id);
    runtimes.len() != initial_len
}

pub async fn page_container_file_io_service_task_loop_owned(
    claim: FileIoManagerRuntimeClaim,
    config: PageContainerFileIoServiceTaskConfig,
) -> PageContainerFileIoServiceTaskReport {
    let report = page_container_file_io_service_task_loop_claim(&claim, config).await;
    let _ = claim.retire();
    report
}

async fn page_container_file_io_service_task_loop_claim(
    claim: &FileIoManagerRuntimeClaim,
    config: PageContainerFileIoServiceTaskConfig,
) -> PageContainerFileIoServiceTaskReport {
    let mut report = PageContainerFileIoServiceTaskReport::default();
    let mut next = PageContainerFileIoServiceNext::Sleeping;
    while config
        .max_ready_turns
        .is_none_or(|max_ready_turns| report.ready_turns < max_ready_turns)
    {
        // A bounded turn may publish follow-up Page/Block work after consuming
        // its mailbox registration. Honor the returned runnable state before
        // registering again; the yield below remains the fairness boundary.
        if next != PageContainerFileIoServiceNext::Runnable {
            match wait_for_file_io_service(claim).await {
                FileIoServiceWait::Ready => report.waits_ready += 1,
                FileIoServiceWait::OwnerRetired => break,
                FileIoServiceWait::Failed => {
                    report.waits_failed += 1;
                    break;
                }
            }
        }

        report.ready_turns += 1;
        let container = {
            let guard = step_engine::guard();
            claim.container.upgrade(&guard)
        };
        let Some(container) = container else {
            break;
        };
        // The Cap now retains the container. Do not pin one epoch across a
        // complete page/block/page service turn: page completions publish a
        // new immutable resident root and can consume more retire credits than
        // one guarded epoch can replenish.
        claim.retain_manager_custody();
        let turn = match drive_page_container_file_io_service_once_compact(
            &container,
            config.page_budget,
            config.block_budget,
            claim.handle,
            |kick| post_file_io_service_kick(&claim.wake_source, kick),
        ) {
            Ok(turn) => turn,
            Err(_) => {
                report.waits_failed += 1;
                break;
            }
        };
        drop(container);

        report.dispatched += turn.block.dispatched;
        report.device_completions += turn.block.device_completions;
        report.page_completions += turn.block.page_completions;
        report.self_kicks += turn.block.kicks
            + turn
                .page_before
                .as_ref()
                .map(|turn| turn.kicks)
                .unwrap_or(0)
            + turn.page_after.as_ref().map(|turn| turn.kicks).unwrap_or(0);
        next = turn.next;
        report.last_turn = Some(turn);
        if config
            .max_ready_turns
            .is_none_or(|max_ready_turns| report.ready_turns < max_ready_turns)
        {
            tx_reactor::yield_now().await;
        }
    }

    report
}

pub async fn page_container_file_io_service_task_loop(
    container: &PageContainer,
    handle: BlockDeviceHandle,
    wake_source: &ServiceWakeSource,
    config: PageContainerFileIoServiceTaskConfig,
) -> PageContainerFileIoServiceTaskReport {
    let mut report = PageContainerFileIoServiceTaskReport::default();
    let mut next = PageContainerFileIoServiceNext::Sleeping;
    while config
        .max_ready_turns
        .is_none_or(|max_ready_turns| report.ready_turns < max_ready_turns)
    {
        // A bounded turn may publish follow-up Page/Block work after consuming
        // its mailbox registration. Honor the returned runnable state before
        // registering again; the yield below remains the fairness boundary.
        if next != PageContainerFileIoServiceNext::Runnable {
            let wait = crate::wait_source::wait_on_registered_endpoint(
                wake_source.wake_endpoint(),
                file_io_service_interest_mask(),
            );
            if wait.await != WaitOutcome::Ready {
                report.waits_failed += 1;
                break;
            }
            report.waits_ready += 1;
        }

        report.ready_turns += 1;
        let turn = match drive_page_container_file_io_service_once_compact(
            container,
            config.page_budget,
            config.block_budget,
            handle,
            |kick| post_file_io_service_kick(wake_source, kick),
        ) {
            Ok(turn) => turn,
            Err(_) => {
                report.waits_failed += 1;
                break;
            }
        };

        report.dispatched += turn.block.dispatched;
        report.device_completions += turn.block.device_completions;
        report.page_completions += turn.block.page_completions;
        report.self_kicks += turn.block.kicks
            + turn
                .page_before
                .as_ref()
                .map(|turn| turn.kicks)
                .unwrap_or(0)
            + turn.page_after.as_ref().map(|turn| turn.kicks).unwrap_or(0);
        next = turn.next;
        report.last_turn = Some(turn);
        if config
            .max_ready_turns
            .is_none_or(|max_ready_turns| report.ready_turns < max_ready_turns)
        {
            tx_reactor::yield_now().await;
        }
    }

    report
}

fn file_io_service_interest_mask() -> u64 {
    IoServiceKind::Page.mask_bits()
        | IoServiceKind::Block.mask_bits()
        | IoServiceKind::Driver.mask_bits()
}

fn post_file_io_service_kick(wake_source: &ServiceWakeSource, kick: ServiceKick) -> bool {
    wake_source.kick_with_post(kick, |mailbox, event| mailbox.post(event)) != 0
}

pub fn drive_page_container_file_io_service_once<F>(
    container: &PageContainer,
    page_budget: ServiceBudget,
    block_budget: ServiceBudget,
    handle: BlockDeviceHandle,
    kick: F,
) -> Result<PageContainerFileIoServiceTurn, PageServiceTaggedBlockCompletionError>
where
    F: FnMut(ServiceKick) -> bool,
{
    drive_page_container_file_io_service_once_inner(
        container,
        page_budget,
        block_budget,
        handle,
        true,
        kick,
    )
}

fn drive_page_container_file_io_service_once_compact<F>(
    container: &PageContainer,
    page_budget: ServiceBudget,
    block_budget: ServiceBudget,
    handle: BlockDeviceHandle,
    kick: F,
) -> Result<PageContainerFileIoServiceTurn, PageServiceTaggedBlockCompletionError>
where
    F: FnMut(ServiceKick) -> bool,
{
    drive_page_container_file_io_service_once_inner(
        container,
        page_budget,
        block_budget,
        handle,
        false,
        kick,
    )
}

fn drive_page_container_file_io_service_once_inner<F>(
    container: &PageContainer,
    page_budget: ServiceBudget,
    block_budget: ServiceBudget,
    handle: BlockDeviceHandle,
    capture_work: bool,
    mut kick: F,
) -> Result<PageContainerFileIoServiceTurn, PageServiceTaggedBlockCompletionError>
where
    F: FnMut(ServiceKick) -> bool,
{
    let page_before = if capture_work {
        container.drive_file_io_service_once_owned(page_budget, &mut kick)
    } else {
        container.drive_file_io_service_once_owned_compact(page_budget, &mut kick)
    };
    // Device dispatch still needs an epoch guard for the registered block
    // handle, but page completion must run after that guard is gone so RCU
    // resident-root publication can replenish local retire credits.
    let block = {
        let guard = step_engine::guard();
        drive_page_container_file_block_device_service_once(
            container,
            block_budget,
            handle,
            &guard,
            &mut kick,
        )?
    };
    let page_after = if block.page_completions == 0 {
        None
    } else if capture_work {
        container.drive_file_io_service_once_owned(page_budget, &mut kick)
    } else {
        container.drive_file_io_service_once_owned_compact(page_budget, &mut kick)
    };
    let next = page_container_file_io_next(&page_before, &block, &page_after);

    Ok(PageContainerFileIoServiceTurn {
        page_before,
        block,
        page_after,
        next,
    })
}

/// `StepOp` wrapper for one bounded PageContainer file-I/O service turn.
#[allow(dead_code)] // scheduler glue lands incrementally; tests exercise the seam now.
pub struct PageContainerFileIoServiceOp<'a, F> {
    pub container: &'a PageContainer,
    pub page_budget: ServiceBudget,
    pub block_budget: ServiceBudget,
    pub handle: BlockDeviceHandle,
    pub kick: F,
}

impl<'a, F> PageContainerFileIoServiceOp<'a, F> {
    pub const fn new(
        container: &'a PageContainer,
        page_budget: ServiceBudget,
        block_budget: ServiceBudget,
        handle: BlockDeviceHandle,
        kick: F,
    ) -> Self {
        Self {
            container,
            page_budget,
            block_budget,
            handle,
            kick,
        }
    }
}

impl<I, F> StepOp<I> for PageContainerFileIoServiceOp<'_, F>
where
    I: SubjectIdentity,
    F: FnMut(ServiceKick) -> bool,
{
    type Output = PageContainerFileIoServiceTurn;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match drive_page_container_file_io_service_once(
            self.container,
            self.page_budget,
            self.block_budget,
            self.handle,
            &mut self.kick,
        ) {
            Ok(turn) => StepOutcome::Done(turn),
            Err(_) => StepOutcome::Err(Errno::EIO.into()),
        }
    }
}

fn page_container_file_io_next(
    page_before: &Option<PageServiceBackendDriven>,
    block: &FileBlockServiceTurn,
    page_after: &Option<PageServiceBackendDriven>,
) -> PageContainerFileIoServiceNext {
    let page_runnable = page_before.as_ref().is_some_and(|turn| {
        turn.next == crate::io_manager::page::service::PageServiceNext::Runnable
    }) || page_after.as_ref().is_some_and(|turn| {
        turn.next == crate::io_manager::page::service::PageServiceNext::Runnable
    });
    if page_runnable || block.next == BlockServiceNext::Runnable {
        PageContainerFileIoServiceNext::Runnable
    } else if block.next == BlockServiceNext::WaitingForCompletion {
        PageContainerFileIoServiceNext::WaitingForCompletion
    } else {
        PageContainerFileIoServiceNext::Sleeping
    }
}

fn page_frame_ref_for_block_completion(completion: &BlockPageCompletion) -> Option<PageFrameRef> {
    if completion.block_completion().result.is_err()
        || !matches!(
            completion.request().op,
            PageIoOp::Read | PageIoOp::Readahead
        )
    {
        return None;
    }
    let vec = completion.block_completion().plan.vecs.first()?;
    if vec.offset != 0 || vec.len == 0 {
        return None;
    }
    Some(PageFrameRef::new(tx_hal::Ppn(vec.buffer_key as usize)))
}

fn frames_from_bio_vecs(vecs: &[BioVec]) -> Result<Vec<Frame>, Errno> {
    if vecs.is_empty() {
        return Err(Errno::EINVAL);
    }
    let mut frames = Vec::with_capacity(vecs.len());
    for vec in vecs {
        if vec.offset != 0 || vec.len == 0 {
            return Err(Errno::EINVAL);
        }
        frames.push(Frame::new(tx_hal::Ppn(vec.buffer_key as usize)));
    }
    Ok(frames)
}

fn step_result_to_result(outcome: StepOutcome<(), NoProgress>) -> Result<(), Errno> {
    match outcome {
        StepOutcome::Done(()) => Ok(()),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Err(Errno::EAGAIN),
    }
}

static BLOCK_REGISTRY_INITIALIZED: AtomicBool = AtomicBool::new(false);
static BLOCK_REGISTRY_PREPARED: AtomicBool = AtomicBool::new(false);
static BLOCK_REGISTRY_LEN: AtomicUsize = AtomicUsize::new(0);
static mut BLOCK_REGISTRY: [Option<&'static BlockDeviceRegistration>; MAX_STATIC_BLOCK_DEVICES] =
    [None; MAX_STATIC_BLOCK_DEVICES];

/// Validated, unpublished block-device registry proposal.
///
/// Only one proposal may be live at a time. Dropping it without committing
/// releases that boot-time reservation and leaves the registry unchanged.
#[must_use = "a prepared block-device registry must be committed or dropped"]
pub struct PreparedBlockDeviceRegistry<'a> {
    regs: &'a [&'static BlockDeviceRegistration],
    committed: bool,
}

impl PreparedBlockDeviceRegistry<'_> {
    /// Publish the fully validated registry in one infallible boot-time step.
    ///
    /// The prepared-token reservation excludes another commit. Entries are
    /// written first and the length is the final reader-visible publication.
    pub fn commit(mut self) {
        debug_assert!(!BLOCK_REGISTRY_INITIALIZED.load(Ordering::Acquire));
        for (idx, reg) in self.regs.iter().copied().enumerate() {
            unsafe {
                BLOCK_REGISTRY[idx] = Some(reg);
            }
        }
        BLOCK_REGISTRY_LEN.store(self.regs.len(), Ordering::Release);
        BLOCK_REGISTRY_INITIALIZED.store(true, Ordering::Release);
        self.committed = true;
        BLOCK_REGISTRY_PREPARED.store(false, Ordering::Release);
    }
}

impl Drop for PreparedBlockDeviceRegistry<'_> {
    fn drop(&mut self) {
        if !self.committed {
            BLOCK_REGISTRY_PREPARED.store(false, Ordering::Release);
        }
    }
}

/// Validate and reserve one fixed block-device registry transaction.
///
/// Capacity, duplicate `name`, and duplicate `devt` checks complete before
/// acquiring the proposal token. Failure never sets `INITIALIZED`, writes an
/// entry, or publishes a non-zero length, so a later corrected proposal may
/// retry without a test-only reset.
pub fn prepare_block_devices<'a>(
    regs: &'a [&'static BlockDeviceRegistration],
) -> Result<PreparedBlockDeviceRegistry<'a>, Errno> {
    if BLOCK_REGISTRY_INITIALIZED.load(Ordering::Acquire) {
        return Err(Errno::EEXIST);
    }
    if regs.len() > MAX_STATIC_BLOCK_DEVICES {
        return Err(Errno::ENOMEM);
    }
    for (idx, reg) in regs.iter().copied().enumerate() {
        if regs[..idx]
            .iter()
            .copied()
            .any(|seen| seen.devt == reg.devt || seen.name == reg.name)
        {
            return Err(Errno::EEXIST);
        }
    }

    if BLOCK_REGISTRY_PREPARED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Errno::EEXIST);
    }
    if BLOCK_REGISTRY_INITIALIZED.load(Ordering::Acquire) {
        BLOCK_REGISTRY_PREPARED.store(false, Ordering::Release);
        return Err(Errno::EEXIST);
    }

    Ok(PreparedBlockDeviceRegistry {
        regs,
        committed: false,
    })
}

pub fn register_block_devices(
    regs: &'static [&'static BlockDeviceRegistration],
) -> StepOutcome<(), NoProgress> {
    match prepare_block_devices(regs) {
        Ok(prepared) => {
            prepared.commit();
            StepOutcome::Done(())
        }
        Err(errno) => StepOutcome::Err(errno.into()),
    }
}

pub fn block_device_by_name(name: &[u8]) -> Option<&'static BlockDeviceRegistration> {
    block_device_snapshot()
        .into_iter()
        .find(|reg| reg.name.as_bytes() == name)
}

pub fn block_device_by_devt(devt: DevT) -> Option<&'static BlockDeviceRegistration> {
    block_device_snapshot()
        .into_iter()
        .find(|reg| reg.devt == devt)
}

pub fn block_device_snapshot() -> alloc::vec::Vec<&'static BlockDeviceRegistration> {
    let len = BLOCK_REGISTRY_LEN.load(Ordering::Acquire);
    let mut out = alloc::vec::Vec::with_capacity(len);
    let mut idx = 0;
    while idx < len {
        let entry = unsafe { BLOCK_REGISTRY[idx] };
        if let Some(reg) = entry {
            out.push(reg);
        }
        idx += 1;
    }
    out
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_block_registry_for_test() {
    let mut idx = 0;
    while idx < MAX_STATIC_BLOCK_DEVICES {
        unsafe {
            BLOCK_REGISTRY[idx] = None;
        }
        idx += 1;
    }
    BLOCK_REGISTRY_LEN.store(0, Ordering::Release);
    BLOCK_REGISTRY_INITIALIZED.store(false, Ordering::Release);
    BLOCK_REGISTRY_PREPARED.store(false, Ordering::Release);
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_page_container_file_io_service_registry_for_test() {
    FILE_IO_SERVICE_RUNTIMES.lock().clear();
    *FILE_IO_SERVICE_RUNTIME_SPAWNER.lock() = None;
    NEXT_FILE_IO_SERVICE_SOURCE_ID.store(FILE_IO_SERVICE_SOURCE_ID_BASE, Ordering::Release);
    NEXT_FILE_IO_SERVICE_RUNTIME_ID.store(1, Ordering::Release);
    ASYNC_BLOCK_ROUTES.lock().clear();
    ROUTED_ASYNC_BLOCK_COMPLETIONS.lock().clear();
    NEXT_ASYNC_BLOCK_COOKIE.store(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_manager::backend::{BlockPageRequestTracker, PageFrameRef};
    use crate::io_manager::block::{
        Bio, BioPlan, BioVec, BlockCompletionSource, BlockDispatch, BlockFlags, BlockOp,
        BlockQueue, BlockRequestId, BlockTag, DeviceKey, LbaRange, SubmitOutcome,
    };
    use crate::io_manager::page::service::{PageService, PageServiceTurn, PageServiceWork};
    use crate::io_manager::page::{
        PageContainerKey, PageGeneration, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
        PageIoRequest, PageIoRequestId,
    };
    use crate::io_manager::runtime::{QueueDepth, ServiceBudget};
    use crate::page_backed::{AnonSwapPolicy, PageContainerKind};
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use tx_hal::Ppn;
    use tx_services::time::DEFAULT_REALTIME_EPOCH_BASE_NS;

    struct RecordingBlockDevice;
    static LAST_READ_BLOCK: AtomicU64 = AtomicU64::new(u64::MAX);
    static BARRIER_COUNT: AtomicUsize = AtomicUsize::new(0);
    static LAST_WRITE_FUA: AtomicBool = AtomicBool::new(false);

    struct DelayedAsyncBlockDevice;
    static DELAYED_ASYNC_COMPLETIONS: SpinMutex<VecDeque<BlockAsyncCompletion>> =
        SpinMutex::new(VecDeque::new());

    impl BlockDeviceOps for DelayedAsyncBlockDevice {
        fn read_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn supports_async_blocks(&self) -> bool {
            true
        }

        fn submit_read_blocks_async(
            &self,
            cookie: u64,
            _block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> BlockAsyncSubmit {
            DELAYED_ASYNC_COMPLETIONS
                .lock()
                .push_back(BlockAsyncCompletion::new(cookie, Ok(())));
            BlockAsyncSubmit::Submitted
        }

        fn poll_async_blocks(
            &self,
            budget: usize,
            _guard: &Guard<'_>,
        ) -> Vec<BlockAsyncCompletion> {
            let mut pending = DELAYED_ASYNC_COMPLETIONS.lock();
            let count = budget.min(pending.len());
            pending.drain(..count).collect()
        }
    }

    impl BlockDevice for DelayedAsyncBlockDevice {
        fn total_blocks(&self) -> u64 {
            128
        }

        fn block_size(&self) -> u32 {
            4096
        }
    }

    static DELAYED_ASYNC_OPS: DelayedAsyncBlockDevice = DelayedAsyncBlockDevice;
    static DELAYED_ASYNC_REG: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 11),
        name: "async-vda",
        ops: &DELAYED_ASYNC_OPS,
    };

    impl BlockDeviceOps for RecordingBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            LAST_READ_BLOCK.store(block_id.as_u64(), Ordering::SeqCst);
            target[0] = Frame::new(Ppn(block_id.as_u64() as usize));
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            BARRIER_COUNT.fetch_add(1, Ordering::SeqCst);
            StepOutcome::done(())
        }

        fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
            BlockDurabilityCapabilities {
                fua: true,
                flush: true,
            }
        }

        fn write_blocks_with_options(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            options: BlockWriteOptions,
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            LAST_WRITE_FUA.store(options.fua, Ordering::SeqCst);
            StepOutcome::done(())
        }
    }

    impl BlockDevice for RecordingBlockDevice {
        fn total_blocks(&self) -> u64 {
            128
        }

        fn block_size(&self) -> u32 {
            4096
        }
    }

    static BLOCK_OPS: RecordingBlockDevice = RecordingBlockDevice;
    static BLOCK_REG: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 1),
        name: "vda1",
        ops: &BLOCK_OPS,
    };

    struct SectorRecordingBlockDevice;

    impl BlockDeviceOps for SectorRecordingBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            LAST_READ_BLOCK.store(block_id.as_u64(), Ordering::SeqCst);
            StepOutcome::done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl BlockDevice for SectorRecordingBlockDevice {
        fn total_blocks(&self) -> u64 {
            256
        }

        fn block_size(&self) -> u32 {
            512
        }
    }

    static SECTOR_BLOCK_OPS: SectorRecordingBlockDevice = SectorRecordingBlockDevice;
    static SECTOR_BLOCK_REG: BlockDeviceRegistration = BlockDeviceRegistration {
        devt: DevT::new(8, 2),
        name: "sda",
        ops: &SECTOR_BLOCK_OPS,
    };

    #[test]
    fn block_device_handle_translates_partition_relative_lbas() {
        use crate::adapter::step_engine::{guard, StepOutcome as V3};
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = guard();
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 4)
            .expect("partition lies within its parent");
        let mut frames = [Frame::new(Ppn(0))];

        assert_eq!(handle.read_blocks(2, &mut frames, &guard), V3::Done(()));
        assert_eq!(frames[0].ppn(), Ppn(34));
        assert_eq!(
            handle.read_blocks(4, &mut frames, &guard),
            V3::err(Errno::EINVAL.into())
        );
    }

    #[test]
    fn block_device_handle_rejects_invalid_partition_geometry() {
        assert_eq!(
            BlockDeviceHandle::partition(&BLOCK_REG, 0, 0).unwrap_err(),
            BlockDeviceRangeError::Empty
        );
        assert_eq!(
            BlockDeviceHandle::partition(&BLOCK_REG, u64::MAX, 2).unwrap_err(),
            BlockDeviceRangeError::Overflow
        );
        assert_eq!(
            BlockDeviceHandle::partition(&BLOCK_REG, 127, 2).unwrap_err(),
            BlockDeviceRangeError::OutOfBounds
        );
    }

    #[test]
    fn block_device_handle_counts_every_sector_covered_by_a_frame() {
        use crate::adapter::step_engine::{guard, StepOutcome as V3};
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        LAST_READ_BLOCK.store(u64::MAX, Ordering::SeqCst);
        let guard = guard();
        // Deliberately not 4 KiB aligned: the first frame begins at LBA 33.
        let handle =
            BlockDeviceHandle::partition(&SECTOR_BLOCK_REG, 33, 9).expect("nine-sector partition");
        let mut frames = [Frame::new(Ppn(0))];

        assert_eq!(handle.blocks_per_frame(), Some(8));
        assert_eq!(handle.read_blocks(1, &mut frames, &guard), V3::Done(()));
        assert_eq!(LAST_READ_BLOCK.load(Ordering::SeqCst), 34);
        assert_eq!(
            handle.read_blocks(2, &mut frames, &guard),
            V3::err(Errno::EINVAL.into()),
            "one Frame spans eight sectors, so the partition tail must reject it"
        );
    }

    #[test]
    fn static_block_registry_indexes_by_name_and_devt_once() {
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        reset_block_registry_for_test();
        static REGS: &[&BlockDeviceRegistration] = &[&BLOCK_REG];

        assert_eq!(register_block_devices(REGS), StepOutcome::Done(()));
        assert!(core::ptr::eq(
            block_device_by_name(b"vda1").expect("vda1"),
            &BLOCK_REG
        ));
        assert!(core::ptr::eq(
            block_device_by_devt(DevT::new(8, 1)).expect("devt"),
            &BLOCK_REG
        ));
        let snapshot = block_device_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(core::ptr::eq(snapshot[0], &BLOCK_REG));
        assert_eq!(
            register_block_devices(REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
    }

    #[test]
    fn prepared_block_registry_is_unpublished_and_drop_is_retryable() {
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        reset_block_registry_for_test();
        let regs: alloc::vec::Vec<&'static BlockDeviceRegistration> = alloc::vec![&BLOCK_REG];

        let prepared = prepare_block_devices(&regs).expect("valid local-slice proposal");
        assert!(block_device_snapshot().is_empty());
        assert!(matches!(prepare_block_devices(&regs), Err(Errno::EEXIST)));
        drop(prepared);
        assert!(block_device_snapshot().is_empty());

        prepare_block_devices(&regs)
            .expect("dropped proposal releases reservation")
            .commit();
        assert!(core::ptr::eq(
            block_device_by_name(b"vda1").expect("committed vda1"),
            &BLOCK_REG
        ));
    }

    #[test]
    fn static_block_registry_validation_failures_are_retryable() {
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        reset_block_registry_for_test();
        static DUP_NAME: BlockDeviceRegistration = BlockDeviceRegistration {
            devt: DevT::new(8, 2),
            name: "vda1",
            ops: &BLOCK_OPS,
        };
        static DUP_DEVT: BlockDeviceRegistration = BlockDeviceRegistration {
            devt: DevT::new(8, 1),
            name: "other",
            ops: &BLOCK_OPS,
        };
        static DUP_NAME_REGS: &[&BlockDeviceRegistration] = &[&BLOCK_REG, &DUP_NAME];
        static DUP_DEVT_REGS: &[&BlockDeviceRegistration] = &[&BLOCK_REG, &DUP_DEVT];
        static TOO_MANY: [&BlockDeviceRegistration; MAX_STATIC_BLOCK_DEVICES + 1] =
            [&BLOCK_REG; MAX_STATIC_BLOCK_DEVICES + 1];
        static VALID: &[&BlockDeviceRegistration] = &[&BLOCK_REG];

        assert_eq!(
            register_block_devices(DUP_NAME_REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
        assert!(block_device_snapshot().is_empty());
        assert_eq!(
            register_block_devices(DUP_DEVT_REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
        assert!(block_device_snapshot().is_empty());
        assert_eq!(
            register_block_devices(&TOO_MANY),
            StepOutcome::Err(Errno::ENOMEM.into())
        );
        assert!(block_device_snapshot().is_empty());

        assert_eq!(register_block_devices(VALID), StepOutcome::Done(()));
        assert_eq!(block_device_snapshot().len(), 1);
    }

    #[test]
    fn file_io_service_registry_keeps_only_weak_liveness_and_manager_metadata() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");

        let registration = register_page_container_file_io_service(
            pc.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );
        let snapshot = page_container_file_io_service_runtimes_snapshot();

        assert_eq!(page_container_file_io_service_runtime_count(), 1);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(registration.raw(), 1);
        let guard = step_engine::guard();
        assert!(snapshot[0].is_live(&guard));
        drop(guard);
        assert_eq!(snapshot[0].handle().registration().devt, BLOCK_REG.devt);
        assert_eq!(snapshot[0].handle().start_lba(), 0);
        assert_eq!(snapshot[0].handle().len_lba(), BLOCK_REG.ops.total_blocks());
        assert_eq!(snapshot[0].source_id(), FILE_IO_SERVICE_SOURCE_ID_BASE);

        reset_page_container_file_io_service_registry_for_test();
        assert!(page_container_file_io_service_runtimes_snapshot().is_empty());
    }

    #[test]
    fn file_io_runtime_registry_does_not_retain_page_container() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");
        register_page_container_file_io_service(pc.clone(), BlockDeviceHandle::whole(&BLOCK_REG));
        let snapshot = page_container_file_io_service_runtimes_snapshot();
        drop(pc);

        let guard = step_engine::guard();
        assert!(
            !snapshot[0].is_live(&guard),
            "registry metadata must not keep a PageContainer capability alive"
        );
        drop(guard);
        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn page_container_drop_publishes_file_io_owner_retirement_wake() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let container = PageContainer::new(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        );
        let wake_source = Arc::new(ServiceWakeSource::new(0x72f0));
        assert!(container.attach_file_io_wake_source(Arc::clone(&wake_source)));
        let mailbox = Arc::new(TaskMailbox::new());
        let generation = mailbox.next_generation();
        let _subscription =
            wake_source.subscribe(IoServiceKind::Page, Arc::downgrade(&mailbox), generation);

        drop(container);

        assert!(matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: seen_generation,
                source,
                interests,
            }) if seen_generation == generation
                && source.raw() == 0x72f0
                && interests.raw() == IoServiceKind::Page.mask_bits()
        ));
    }

    #[test]
    fn idle_maintenance_wakes_claim_after_final_cap_before_payload_drop() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");
        register_page_container_file_io_service(pc.clone(), BlockDeviceHandle::whole(&BLOCK_REG));
        let mut claims = claim_pending_file_io_service_runtimes_for_test();
        let claim = claims.pop().expect("claimed runtime");
        let mailbox = Arc::new(TaskMailbox::new());
        let generation = mailbox.next_generation();
        let _subscription =
            claim
                .wake_source
                .subscribe(IoServiceKind::Page, Arc::downgrade(&mailbox), generation);

        // Keep an epoch guard live so the PageContainer destructor cannot be
        // the source of this wake. The final Cap still installs Retiring
        // immediately, which is the liveness predicate maintenance observes.
        let guard = step_engine::guard();
        drop(pc);
        assert!(!claim.page_submission.owner_retired());
        assert_eq!(wake_unowned_file_io_service_runtimes(), 1);
        assert!(matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: seen_generation,
                interests,
                ..
            }) if seen_generation == generation
                && interests.raw() == IoServiceKind::Page.mask_bits()
        ));
        drop(guard);
        drop(claim);
        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn repeated_file_io_runtime_registration_reuses_attached_runtime() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");

        let first = register_page_container_file_io_service(
            pc.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );
        let first_source = page_container_file_io_service_runtimes_snapshot()[0].source_id();
        let second = register_page_container_file_io_service(
            pc.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );

        assert_eq!(second, first);
        assert_eq!(page_container_file_io_service_runtime_count(), 1);
        assert_eq!(
            page_container_file_io_service_runtimes_snapshot()[0].source_id(),
            first_source,
            "duplicate attachment must not publish an unreachable wake source",
        );

        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn dropping_file_io_runtime_claim_retires_registry_entry() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");
        register_page_container_file_io_service(pc.clone(), BlockDeviceHandle::whole(&BLOCK_REG));
        let mut claims = claim_pending_file_io_service_runtimes_for_test();
        assert_eq!(claims.len(), 1);
        assert_eq!(page_container_file_io_service_runtime_count(), 1);

        drop(claims.pop().expect("claimed runtime"));

        assert_eq!(page_container_file_io_service_runtime_count(), 0);
        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn retired_owner_latch_closes_wake_before_subscription_race() {
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        let pc = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("page container cap");
        let wake_source = Arc::new(ServiceWakeSource::new(0x72f1));
        assert!(pc.attach_file_io_wake_source(Arc::clone(&wake_source)));
        let claim = FileIoManagerRuntimeClaim::detached_for_test(
            pc.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
            wake_source,
        );
        let page_submission = claim.page_submission.clone();

        // Retirement happens before the task creates its wait subscription, so
        // there is deliberately no mailbox edge for the first poll to consume.
        assert!(page_submission.retire_owner());
        drop(pc);
        let mut task = core::pin::pin!(page_container_file_io_service_task_loop_owned(
            claim,
            PageContainerFileIoServiceTaskConfig::run_forever(
                ServiceBudget::new(1),
                ServiceBudget::new(1),
            ),
        ));
        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);

        let report = match task.as_mut().poll(&mut cx) {
            Poll::Ready(report) => report,
            Poll::Pending => panic!("latched owner retirement must stop the parked runtime"),
        };
        assert_eq!(report.waits_ready, 0);
        assert_eq!(report.waits_failed, 0);
        assert_eq!(report.ready_turns, 0);
    }

    #[test]
    fn file_io_runtime_spawner_drains_boot_backlog_and_retires_claims_once() {
        struct CountingSpawner {
            spawned: AtomicUsize,
            retired: AtomicUsize,
        }

        impl FileIoServiceRuntimeSpawner for CountingSpawner {
            fn spawn_file_io_service(&self, claim: FileIoManagerRuntimeClaim) {
                self.spawned.fetch_add(1, Ordering::AcqRel);
                self.retired
                    .fetch_add(usize::from(claim.retire()), Ordering::AcqRel);
            }
        }

        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        crate::zones::register_all().expect("kernel zones");
        reset_page_container_file_io_service_registry_for_test();

        let first = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("first page container");
        register_page_container_file_io_service(
            first.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );

        let spawner = Arc::new(CountingSpawner {
            spawned: AtomicUsize::new(0),
            retired: AtomicUsize::new(0),
        });
        assert_eq!(
            install_file_io_service_runtime_spawner(spawner.clone()),
            Some(1)
        );
        assert_eq!(spawner.spawned.load(Ordering::Acquire), 1);
        assert_eq!(spawner.retired.load(Ordering::Acquire), 1);
        assert_eq!(submit_pending_file_io_service_runtimes(), 0);
        assert_eq!(page_container_file_io_service_runtime_count(), 0);

        let second = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("second page container");
        register_page_container_file_io_service(
            second.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );

        assert_eq!(spawner.spawned.load(Ordering::Acquire), 2);
        assert_eq!(spawner.retired.load(Ordering::Acquire), 2);
        assert_eq!(submit_pending_file_io_service_runtimes(), 0);
        assert_eq!(page_container_file_io_service_runtime_count(), 0);
        assert_eq!(
            install_file_io_service_runtime_spawner(Arc::new(CountingSpawner {
                spawned: AtomicUsize::new(0),
                retired: AtomicUsize::new(0),
            })),
            None
        );

        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn block_device_dispatch_adapter_executes_read_dispatch_and_polls_completion() {
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        LAST_READ_BLOCK.store(u64::MAX, Ordering::SeqCst);
        let guard = guard();
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 16)
            .expect("partition lies within its parent");
        let mut adapter = BlockDeviceDispatchAdapter::new(handle, &guard);
        let dispatch = BlockDispatch {
            tag: BlockTag::new(7),
            bio: Bio {
                id: BlockRequestId::new(3),
                plan: BioPlan::new(
                    DeviceKey::new(BLOCK_REG.devt.raw()),
                    BlockOp::Read,
                    LbaRange::new(2, 8),
                    alloc::vec![BioVec::new(0, 0, 4096)],
                    BlockFlags::EMPTY,
                ),
            },
        };

        adapter.submit(&dispatch);
        let completion = adapter.poll_completion().expect("completion");

        assert_eq!(completion.tag, BlockTag::new(7));
        assert_eq!(completion.result, Ok(()));
        assert_eq!(LAST_READ_BLOCK.load(Ordering::SeqCst), 34);
        assert_eq!(adapter.poll_completion(), None);
    }

    #[test]
    fn async_dispatch_completion_is_routed_to_its_manager_namespace() {
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        reset_page_container_file_io_service_registry_for_test();
        DELAYED_ASYNC_COMPLETIONS.lock().clear();
        let guard = guard();
        let handle = BlockDeviceHandle::whole(&DELAYED_ASYNC_REG);
        let mut first = BlockDeviceDispatchAdapter::new_async(handle, &guard, 41);
        let mut second = BlockDeviceDispatchAdapter::new_async(handle, &guard, 42);
        let dispatch = |tag| BlockDispatch {
            tag: BlockTag::new(tag),
            bio: Bio {
                id: BlockRequestId::new(tag),
                plan: BioPlan::new(
                    DeviceKey::new(DELAYED_ASYNC_REG.devt.raw()),
                    BlockOp::Read,
                    LbaRange::new(tag * 8, 8),
                    alloc::vec![BioVec::new(tag, 0, 4096)],
                    BlockFlags::EMPTY,
                ),
            },
        };

        first.submit(&dispatch(7));
        second.submit(&dispatch(9));

        // The second poll drains both device completions. The first one must
        // be retained for namespace 41 instead of being consumed by 42.
        assert_eq!(
            second.poll_completion(),
            Some(BlockDeviceCompletion::new(BlockTag::new(9), Ok(())))
        );
        assert_eq!(
            first.poll_completion(),
            Some(BlockDeviceCompletion::new(BlockTag::new(7), Ok(())))
        );
        assert_eq!(first.poll_completion(), None);
        assert_eq!(second.poll_completion(), None);
        reset_page_container_file_io_service_registry_for_test();
    }

    #[test]
    fn block_device_dispatch_adapter_executes_barrier_dispatch() {
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        BARRIER_COUNT.store(0, Ordering::SeqCst);
        let guard = guard();
        let handle = BlockDeviceHandle::whole(&BLOCK_REG);
        let mut adapter = BlockDeviceDispatchAdapter::new(handle, &guard);
        let dispatch = BlockDispatch {
            tag: BlockTag::new(8),
            bio: Bio {
                id: BlockRequestId::new(4),
                plan: BioPlan::new(
                    DeviceKey::new(BLOCK_REG.devt.raw()),
                    BlockOp::Barrier,
                    LbaRange::new(0, 0),
                    alloc::vec::Vec::new(),
                    BlockFlags::BARRIER,
                ),
            },
        };

        adapter.submit(&dispatch);
        let completion = adapter.poll_completion().expect("completion");

        assert_eq!(completion.tag, BlockTag::new(8));
        assert_eq!(completion.result, Ok(()));
        assert_eq!(BARRIER_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dispatch_adapter_passes_fua_to_capable_device() {
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        LAST_WRITE_FUA.store(false, Ordering::SeqCst);
        let guard = guard();
        let mut adapter =
            BlockDeviceDispatchAdapter::new(BlockDeviceHandle::whole(&BLOCK_REG), &guard);
        let dispatch = BlockDispatch {
            tag: BlockTag::new(9),
            bio: Bio {
                id: BlockRequestId::new(5),
                plan: BioPlan::new(
                    DeviceKey::new(BLOCK_REG.devt.raw()),
                    BlockOp::Write,
                    LbaRange::new(2, 8),
                    alloc::vec![BioVec::new(0, 0, 4096)],
                    BlockFlags::FUA,
                ),
            },
        };

        adapter.submit(&dispatch);

        assert_eq!(
            adapter.poll_completion().expect("completion").result,
            Ok(())
        );
        assert!(LAST_WRITE_FUA.load(Ordering::SeqCst));
    }

    #[test]
    fn default_device_rejects_fua_instead_of_ignoring_it() {
        struct FlushOnlyDevice;

        impl BlockDeviceOps for FlushOnlyDevice {
            fn read_blocks(
                &self,
                _block_id: PhysicalBlockNumber,
                _target: &mut [Frame],
                _guard: &Guard<'_>,
            ) -> StepOutcome<(), NoProgress> {
                StepOutcome::done(())
            }

            fn write_blocks(
                &self,
                _block_id: PhysicalBlockNumber,
                _source: &[Frame],
                _guard: &Guard<'_>,
            ) -> StepOutcome<(), NoProgress> {
                StepOutcome::done(())
            }

            fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
                StepOutcome::done(())
            }
        }

        impl BlockDevice for FlushOnlyDevice {
            fn total_blocks(&self) -> u64 {
                8
            }

            fn block_size(&self) -> u32 {
                4096
            }
        }

        static OPS: FlushOnlyDevice = FlushOnlyDevice;
        static REG: BlockDeviceRegistration = BlockDeviceRegistration {
            devt: DevT::new(8, 9),
            name: "fua-none",
            ops: &OPS,
        };
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = guard();
        let mut adapter = BlockDeviceDispatchAdapter::new(BlockDeviceHandle::whole(&REG), &guard);
        let dispatch = BlockDispatch {
            tag: BlockTag::new(10),
            bio: Bio {
                id: BlockRequestId::new(6),
                plan: BioPlan::new(
                    DeviceKey::new(REG.devt.raw()),
                    BlockOp::Write,
                    LbaRange::new(0, 8),
                    alloc::vec![BioVec::new(0, 0, 4096)],
                    BlockFlags::FUA,
                ),
            },
        };

        adapter.submit(&dispatch);

        assert_eq!(
            adapter.poll_completion().expect("completion").result,
            Err(Errno::EOPNOTSUPP)
        );
    }

    #[test]
    fn block_device_service_turn_routes_read_completion_into_page_service() {
        use crate::adapter::step_engine::guard;
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        LAST_READ_BLOCK.store(u64::MAX, Ordering::SeqCst);

        let request = PageIoRequest::new(
            PageIoRequestId::new(55),
            PageContainerKey::new(9),
            PageIoRange::new(6, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(77)),
        );
        let mut block_queue = BlockQueue::new(4);
        let submit = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(BLOCK_REG.devt.raw()),
                BlockOp::Read,
                LbaRange::new(2, 1),
                alloc::vec![BioVec::new(123, 0, 4096)],
                BlockFlags::EMPTY,
            ))
            .expect("queue read bio");
        assert_eq!(submit, SubmitOutcome::Queued(BlockRequestId::new(1)));

        let mut tracker = BlockPageRequestTracker::new();
        tracker.record_submit_outcomes(request, &[submit]);
        let mut page_service = PageService::new(4);
        let mut block_driver =
            crate::io_manager::block::BlockServiceDriver::new(ServiceBudget::new(1));
        let mut depth = QueueDepth::new(1);
        let mut tags = crate::io_manager::block::BlockTagTable::new();
        let guard = guard();
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 16)
            .expect("partition lies within its parent");

        let turn = drive_block_device_service_once(
            handle,
            &guard,
            &mut block_driver,
            &mut block_queue,
            &mut depth,
            &mut tags,
            &mut tracker,
            &mut page_service,
            |_| false,
        )
        .expect("service turn");

        assert_eq!(turn.dispatched, 1);
        assert_eq!(turn.device_completions, 1);
        assert_eq!(turn.page_completions, 1);
        assert_eq!(LAST_READ_BLOCK.load(Ordering::SeqCst), 34);
        assert_eq!(depth.in_flight(), 0);
        assert!(tags.is_empty());
        assert!(tracker.is_empty());

        match page_service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, PageIoRequestId::new(55));
                    assert_eq!(route.completion.generation, PageGeneration::new(77));
                    assert_eq!(route.frame, Some(PageFrameRef::new(Ppn(123))));
                }
                other => panic!("expected page completion, got {other:?}"),
            },
            other => panic!("expected page work, got {other:?}"),
        }
    }

    #[test]
    fn rtc_time_converts_default_realtime_epoch() {
        let rtc = RtcTime::from_unix_ns(DEFAULT_REALTIME_EPOCH_BASE_NS)
            .expect("default realtime epoch should be representable");

        assert_eq!(rtc.tm_sec, 0);
        assert_eq!(rtc.tm_min, 0);
        assert_eq!(rtc.tm_hour, 0);
        assert_eq!(rtc.tm_mday, 23);
        assert_eq!(rtc.tm_mon, 4);
        assert_eq!(rtc.tm_year, 126);
        assert_eq!(rtc.tm_isdst, 0);
        assert_eq!(
            rtc.to_unix_ns().expect("rtc converts back to unix ns"),
            DEFAULT_REALTIME_EPOCH_BASE_NS
        );
    }

    #[test]
    fn rtc_time_rejects_invalid_calendar_values() {
        let mut invalid = RtcTime::from_unix_seconds(0).expect("epoch converts");
        invalid.tm_mon = 12;
        assert_eq!(invalid.to_unix_ns(), Err(RtcError::InvalidTime));

        invalid = RtcTime::from_unix_seconds(0).expect("epoch converts");
        invalid.tm_year = 69;
        assert_eq!(invalid.to_unix_ns(), Err(RtcError::Range));

        invalid = RtcTime::from_unix_seconds(0).expect("epoch converts");
        invalid.tm_mday = 31;
        invalid.tm_mon = 1;
        assert_eq!(invalid.to_unix_ns(), Err(RtcError::InvalidTime));
    }
}
