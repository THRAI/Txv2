//! Device and block-handle shells shared by devfs, bdev-fs, and backends.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::adapter::step_engine::{
    self as step_engine, ByteProgress, Cap, NoProgress, ScriptCtx, SpinMutex, StepOp, StepOutcome,
    SubjectIdentity,
};
use crate::adapter::wait_routing::WaitOutcome;

use crate::execution::{Errno, Guard};
use crate::io_manager::backend::{BlockPageCompletion, BlockPageRequestTracker, PageFrameRef};
use crate::io_manager::block::{
    BioVec, BlockCompletionSource, BlockDeviceCompletion, BlockDispatch, BlockDispatchExecutor,
    BlockFlags, BlockOp, BlockQueue, BlockServiceDriver, BlockServiceNext, BlockTagTable,
    DeviceKey,
};
use crate::io_manager::page::PageIoOp;
use crate::io_manager::page::service::{
    PageService, PageServiceBackendDriven, PageServiceTaggedBlockCompletionError,
};
use crate::io_manager::runtime::{
    IoServiceKind, QueueDepth, ServiceBudget, ServiceKick, ServiceWakeSource,
};
use crate::page_backed::{FileBlockServiceTurn, Frame, PageContainer};
use tx_services::time::DeadlineRegistrar;

const MAX_STATIC_BLOCK_DEVICES: usize = 16;
const FILE_IO_SERVICE_SOURCE_ID_BASE: u64 = 0x7200;

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

impl BlockDeviceHandle {
    pub fn whole(reg: &'static BlockDeviceRegistration) -> Self {
        Self {
            reg,
            start_lba: 0,
            len_lba: reg.ops.total_blocks(),
        }
    }

    pub const fn partition(
        reg: &'static BlockDeviceRegistration,
        start_lba: u64,
        len_lba: u64,
    ) -> Self {
        Self {
            reg,
            start_lba,
            len_lba,
        }
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

    fn block_id_for(self, lba_offset: u64, count: u64) -> Option<PhysicalBlockNumber> {
        let end = lba_offset.checked_add(count)?;
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

pub struct BlockDeviceDispatchAdapter<'a, 'g> {
    handle: BlockDeviceHandle,
    guard: &'a Guard<'g>,
    completions: VecDeque<BlockDeviceCompletion>,
}

impl<'a, 'g> BlockDeviceDispatchAdapter<'a, 'g> {
    pub fn new(handle: BlockDeviceHandle, guard: &'a Guard<'g>) -> Self {
        Self {
            handle,
            guard,
            completions: VecDeque::new(),
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
}

impl BlockDispatchExecutor for BlockDeviceDispatchAdapter<'_, '_> {
    fn submit(&mut self, dispatch: &BlockDispatch) {
        let result = self.execute_dispatch(dispatch);
        self.completions
            .push_back(BlockDeviceCompletion::new(dispatch.tag, result));
    }
}

impl BlockCompletionSource for BlockDeviceDispatchAdapter<'_, '_> {
    fn poll_completion(&mut self) -> Option<BlockDeviceCompletion> {
        self.completions.pop_front()
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
    let mut adapter = BlockDeviceDispatchAdapter::new(handle, guard);
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

#[derive(Clone)]
pub struct PageContainerFileIoServiceRuntime {
    container: Cap<PageContainer>,
    handle: BlockDeviceHandle,
    wake_source: Arc<ServiceWakeSource>,
}

impl PageContainerFileIoServiceRuntime {
    pub fn new(
        container: Cap<PageContainer>,
        handle: BlockDeviceHandle,
        wake_source: Arc<ServiceWakeSource>,
    ) -> Self {
        Self {
            container,
            handle,
            wake_source,
        }
    }

    pub fn container(&self) -> &Cap<PageContainer> {
        &self.container
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
}

struct RegisteredFileIoServiceRuntime {
    runtime: PageContainerFileIoServiceRuntime,
    submitted: bool,
}

/// Kernel-owned task submission for a registered file-I/O service runtime.
///
/// The device subsystem retains the runtime registry and exactly-once claim
/// state. The reactor owner installs this narrow callback after it is ready to
/// accept long-lived service futures.
pub trait FileIoServiceRuntimeSpawner: Send + Sync {
    fn spawn_file_io_service(&self, runtime: PageContainerFileIoServiceRuntime);
}

static FILE_IO_SERVICE_RUNTIMES: SpinMutex<Vec<RegisteredFileIoServiceRuntime>> =
    SpinMutex::new(Vec::new());
static FILE_IO_SERVICE_RUNTIME_SPAWNER: SpinMutex<Option<Arc<dyn FileIoServiceRuntimeSpawner>>> =
    SpinMutex::new(None);
static NEXT_FILE_IO_SERVICE_SOURCE_ID: AtomicU64 = AtomicU64::new(FILE_IO_SERVICE_SOURCE_ID_BASE);

pub fn register_page_container_file_io_service(
    container: Cap<PageContainer>,
    handle: BlockDeviceHandle,
) -> PageContainerFileIoServiceRuntime {
    let source_id = NEXT_FILE_IO_SERVICE_SOURCE_ID.fetch_add(1, Ordering::AcqRel);
    let wake_source = Arc::new(ServiceWakeSource::new(source_id));
    let _ = container.attach_file_io_wake_source(Arc::clone(&wake_source));
    let runtime = PageContainerFileIoServiceRuntime::new(container, handle, wake_source);
    FILE_IO_SERVICE_RUNTIMES
        .lock()
        .push(RegisteredFileIoServiceRuntime {
            runtime: runtime.clone(),
            submitted: false,
        });
    let _ = submit_pending_file_io_service_runtimes();
    runtime
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
    let pending = {
        let mut runtimes = FILE_IO_SERVICE_RUNTIMES.lock();
        let mut pending = Vec::new();
        for entry in runtimes.iter_mut() {
            if !entry.submitted {
                entry.submitted = true;
                pending.push(entry.runtime.clone());
            }
        }
        pending
    };
    let submitted = pending.len();
    for runtime in pending {
        spawner.spawn_file_io_service(runtime);
    }
    submitted
}

pub fn page_container_file_io_service_runtimes_snapshot() -> Vec<PageContainerFileIoServiceRuntime>
{
    FILE_IO_SERVICE_RUNTIMES
        .lock()
        .iter()
        .map(|entry| entry.runtime.clone())
        .collect()
}

pub fn page_container_file_io_service_runtime_count() -> usize {
    FILE_IO_SERVICE_RUNTIMES.lock().len()
}

pub async fn page_container_file_io_service_task_loop_owned(
    runtime: PageContainerFileIoServiceRuntime,
    config: PageContainerFileIoServiceTaskConfig,
) -> PageContainerFileIoServiceTaskReport {
    page_container_file_io_service_task_loop(
        &runtime.container,
        runtime.handle,
        &runtime.wake_source,
        config,
    )
    .await
}

pub async fn page_container_file_io_service_task_loop(
    container: &PageContainer,
    handle: BlockDeviceHandle,
    wake_source: &ServiceWakeSource,
    config: PageContainerFileIoServiceTaskConfig,
) -> PageContainerFileIoServiceTaskReport {
    let mut report = PageContainerFileIoServiceTaskReport::default();
    while config
        .max_ready_turns
        .is_none_or(|max_ready_turns| report.ready_turns < max_ready_turns)
    {
        let wait = crate::wait_source::wait_on_registered_endpoint(
            wake_source.wake_endpoint(),
            file_io_service_interest_mask(),
        );
        if wait.await != WaitOutcome::Ready {
            report.waits_failed += 1;
            break;
        }

        report.waits_ready += 1;
        report.ready_turns += 1;
        let guard = step_engine::guard();
        let turn = match drive_page_container_file_io_service_once(
            container,
            config.page_budget,
            config.block_budget,
            handle,
            &guard,
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
        report.last_turn = Some(turn);
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
    guard: &Guard<'_>,
    mut kick: F,
) -> Result<PageContainerFileIoServiceTurn, PageServiceTaggedBlockCompletionError>
where
    F: FnMut(ServiceKick) -> bool,
{
    let page_before = container.drive_file_io_service_once_owned(page_budget, &mut kick);
    let block = drive_page_container_file_block_device_service_once(
        container,
        block_budget,
        handle,
        guard,
        &mut kick,
    )?;
    let page_after = if block.page_completions == 0 {
        None
    } else {
        container.drive_file_io_service_once_owned(page_budget, &mut kick)
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
        let guard = step_engine::guard();
        match drive_page_container_file_io_service_once(
            self.container,
            self.page_budget,
            self.block_budget,
            self.handle,
            &guard,
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
static BLOCK_REGISTRY_LEN: AtomicUsize = AtomicUsize::new(0);
static mut BLOCK_REGISTRY: [Option<&'static BlockDeviceRegistration>; MAX_STATIC_BLOCK_DEVICES] =
    [None; MAX_STATIC_BLOCK_DEVICES];

pub fn register_block_devices(
    regs: &'static [&'static BlockDeviceRegistration],
) -> StepOutcome<(), NoProgress> {
    if BLOCK_REGISTRY_INITIALIZED.swap(true, Ordering::AcqRel) {
        return StepOutcome::Err(Errno::EEXIST.into());
    }
    if regs.len() > MAX_STATIC_BLOCK_DEVICES {
        return StepOutcome::Err(Errno::ENOMEM.into());
    }

    for (idx, reg) in regs.iter().copied().enumerate() {
        if regs[..idx]
            .iter()
            .copied()
            .any(|seen| seen.devt == reg.devt || seen.name == reg.name)
        {
            return StepOutcome::Err(Errno::EEXIST.into());
        }
        unsafe {
            BLOCK_REGISTRY[idx] = Some(reg);
        }
    }
    BLOCK_REGISTRY_LEN.store(regs.len(), Ordering::Release);
    StepOutcome::Done(())
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
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_page_container_file_io_service_registry_for_test() {
    FILE_IO_SERVICE_RUNTIMES.lock().clear();
    *FILE_IO_SERVICE_RUNTIME_SPAWNER.lock() = None;
    NEXT_FILE_IO_SERVICE_SOURCE_ID.store(FILE_IO_SERVICE_SOURCE_ID_BASE, Ordering::Release);
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

    #[test]
    fn block_device_handle_translates_partition_relative_lbas() {
        use crate::adapter::step_engine::{StepOutcome as V3, guard};
        tx_test_support::init_host();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = guard();
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 4);
        let mut frames = [Frame::new(Ppn(0))];

        assert_eq!(handle.read_blocks(2, &mut frames, &guard), V3::Done(()));
        assert_eq!(frames[0].ppn(), Ppn(34));
        assert_eq!(
            handle.read_blocks(4, &mut frames, &guard),
            V3::err(Errno::EINVAL.into())
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
    fn static_block_registry_rejects_duplicate_names_or_devts() {
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        reset_block_registry_for_test();
        static DUP_NAME: BlockDeviceRegistration = BlockDeviceRegistration {
            devt: DevT::new(8, 2),
            name: "vda1",
            ops: &BLOCK_OPS,
        };
        static REGS: &[&BlockDeviceRegistration] = &[&BLOCK_REG, &DUP_NAME];

        assert_eq!(
            register_block_devices(REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
        assert!(block_device_snapshot().is_empty());
    }

    #[test]
    fn file_io_service_registry_snapshots_owned_runtime_for_reactor_submission() {
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

        let runtime = register_page_container_file_io_service(
            pc.clone(),
            BlockDeviceHandle::whole(&BLOCK_REG),
        );
        let snapshot = page_container_file_io_service_runtimes_snapshot();

        assert_eq!(page_container_file_io_service_runtime_count(), 1);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(runtime.container().page_count(), 1);
        assert_eq!(snapshot[0].container().page_count(), 1);
        assert_eq!(snapshot[0].handle().registration().devt, BLOCK_REG.devt);
        assert_eq!(snapshot[0].handle().start_lba(), 0);
        assert_eq!(snapshot[0].handle().len_lba(), BLOCK_REG.ops.total_blocks());
        assert_eq!(
            snapshot[0].wake_source().source_id(),
            FILE_IO_SERVICE_SOURCE_ID_BASE
        );

        reset_page_container_file_io_service_registry_for_test();
        assert!(page_container_file_io_service_runtimes_snapshot().is_empty());
    }

    #[test]
    fn file_io_runtime_spawner_drains_boot_backlog_and_submits_late_registration_once() {
        struct CountingSpawner(AtomicUsize);

        impl FileIoServiceRuntimeSpawner for CountingSpawner {
            fn spawn_file_io_service(&self, _runtime: PageContainerFileIoServiceRuntime) {
                self.0.fetch_add(1, Ordering::AcqRel);
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
        register_page_container_file_io_service(first, BlockDeviceHandle::whole(&BLOCK_REG));

        let spawner = Arc::new(CountingSpawner(AtomicUsize::new(0)));
        assert_eq!(
            install_file_io_service_runtime_spawner(spawner.clone()),
            Some(1)
        );
        assert_eq!(spawner.0.load(Ordering::Acquire), 1);
        assert_eq!(submit_pending_file_io_service_runtimes(), 0);

        let second = PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("second page container");
        register_page_container_file_io_service(second, BlockDeviceHandle::whole(&BLOCK_REG));

        assert_eq!(spawner.0.load(Ordering::Acquire), 2);
        assert_eq!(submit_pending_file_io_service_runtimes(), 0);
        assert_eq!(
            install_file_io_service_runtime_spawner(Arc::new(CountingSpawner(
                AtomicUsize::new(0,)
            ))),
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
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 16);
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
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 16);

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
