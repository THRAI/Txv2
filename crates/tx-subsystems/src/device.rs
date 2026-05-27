//! Device and block-handle shells shared by devfs, bdev-fs, and backends.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::adapter::step_engine::{ByteProgress, NoProgress, StepOutcome};

use crate::execution::{Errno, Guard};
use crate::page_backed::Frame;

const MAX_STATIC_BLOCK_DEVICES: usize = 16;

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

pub trait CharDeviceOps: Send + Sync + 'static {
    fn read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress>;
    fn write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize, ByteProgress>;
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
            return StepOutcome::err(Errno::EINVAL);
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
            return StepOutcome::err(Errno::EINVAL);
        };
        self.reg.ops.write_blocks(block_id, source, guard)
    }

    pub fn barrier(self, guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
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

static BLOCK_REGISTRY_INITIALIZED: AtomicBool = AtomicBool::new(false);
static BLOCK_REGISTRY_LEN: AtomicUsize = AtomicUsize::new(0);
static mut BLOCK_REGISTRY: [Option<&'static BlockDeviceRegistration>; MAX_STATIC_BLOCK_DEVICES] =
    [None; MAX_STATIC_BLOCK_DEVICES];

pub fn register_block_devices(
    regs: &'static [&'static BlockDeviceRegistration],
) -> StepOutcome<(), NoProgress> {
    if BLOCK_REGISTRY_INITIALIZED.swap(true, Ordering::AcqRel) {
        return StepOutcome::Err(Errno::EEXIST);
    }
    if regs.len() > MAX_STATIC_BLOCK_DEVICES {
        return StepOutcome::Err(Errno::ENOMEM);
    }

    for (idx, reg) in regs.iter().copied().enumerate() {
        if regs[..idx]
            .iter()
            .copied()
            .any(|seen| seen.devt == reg.devt || seen.name == reg.name)
        {
            return StepOutcome::Err(Errno::EEXIST);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tx_hal::Ppn;

    struct RecordingBlockDevice;

    impl BlockDeviceOps for RecordingBlockDevice {
        fn read_blocks(
            &self,
            block_id: PhysicalBlockNumber,
            target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
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
        use crate::adapter::step_engine::{guard, StepOutcome as V3};
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
            V3::err(Errno::EINVAL)
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
            StepOutcome::Err(Errno::EEXIST)
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
            StepOutcome::Err(Errno::EEXIST)
        );
        assert!(block_device_snapshot().is_empty());
    }
}
