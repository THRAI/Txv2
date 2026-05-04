//! Device and block-handle shells shared by devfs, bdev-fs, and backends.

use core::fmt;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::page_backed::Frame;

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
    fn read(&self, out: &mut [u8], guard: &Guard<'_>) -> StepOutcome<usize>;
    fn write(&self, bytes: &[u8], guard: &Guard<'_>) -> StepOutcome<usize>;
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
    ) -> StepOutcome<()>;

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<()>;

    fn barrier(&self, guard: &Guard<'_>) -> StepOutcome<()>;
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
    ) -> StepOutcome<()> {
        let Some(block_id) = self.block_id_for(lba_offset, target.len() as u64) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        self.reg.ops.read_blocks(block_id, target, guard)
    }

    pub fn write_blocks(
        self,
        lba_offset: u64,
        source: &[Frame],
        guard: &Guard<'_>,
    ) -> StepOutcome<()> {
        let Some(block_id) = self.block_id_for(lba_offset, source.len() as u64) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        self.reg.ops.write_blocks(block_id, source, guard)
    }

    pub fn barrier(self, guard: &Guard<'_>) -> StepOutcome<()> {
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
        ) -> StepOutcome<()> {
            target[0] = Frame::new(Ppn(block_id.as_u64() as usize));
            StepOutcome::Done(())
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
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
        tx_substrate::testing::init_host_for_test_once();
        let _lock = crate::test_support::EPOCH_TEST_LOCK
            .lock()
            .expect("epoch test lock");
        let guard = tx_substrate::epoch::guard();
        let handle = BlockDeviceHandle::partition(&BLOCK_REG, 32, 4);
        let mut frames = [Frame::new(Ppn(0))];

        assert_eq!(
            handle.read_blocks(2, &mut frames, &guard),
            StepOutcome::Done(())
        );
        assert_eq!(frames[0].ppn(), Ppn(34));
        assert_eq!(
            handle.read_blocks(4, &mut frames, &guard),
            StepOutcome::Err(Errno::EINVAL)
        );
    }
}
