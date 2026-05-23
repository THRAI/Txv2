use alloc::boxed::Box;
use core::marker::PhantomData;

use crate::adapter::step_engine::{NoProgress, StepOutcome};
use tx_hal::{Arch, TxPlatform};
use tx_subsystems::device::{
    register_block_devices, BlockDevice, BlockDeviceOps, BlockDeviceRegistration, DevT,
    PhysicalBlockNumber,
};
use tx_subsystems::execution::Guard;
use tx_subsystems::page_backed::Frame;

pub struct KernelBlockDevices<P: TxPlatform> {
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> KernelBlockDevices<P> {
    pub const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }

    pub fn init_and_register(&'static self) -> StepOutcome<(), NoProgress> {
        match P::ARCH {
            Arch::LoongArch64 => self.init_la64_qemu_virt(),
            Arch::Riscv64 => self.init_rv64_qemu_virt(),
        }
    }

    fn init_la64_qemu_virt(&'static self) -> StepOutcome<(), NoProgress> {
        let scratch = scratch_block_registration();
        let block = Box::leak(Box::new(tx_drivers::virtio::VirtioPciBlock::<P>::new(
            "pcie-ecam",
            "pcie-mmio32",
        )));
        if block.init().is_err() {
            let registrations: &'static [&'static BlockDeviceRegistration] =
                Box::leak(Box::new([scratch]));
            return register_block_devices(registrations);
        }

        let registration = Box::leak(Box::new(BlockDeviceRegistration {
            devt: DevT::new(254, 0),
            name: "vda",
            ops: block,
        }));
        let registrations: &'static [&'static BlockDeviceRegistration] = Box::leak(Box::new([
            registration as &'static BlockDeviceRegistration,
            scratch,
        ]));
        register_block_devices(registrations)
    }

    fn init_rv64_qemu_virt(&'static self) -> StepOutcome<(), NoProgress> {
        let scratch = scratch_block_registration();
        let block = Box::leak(Box::new(tx_drivers::virtio::VirtioMmioBlock::<P>::new(
            "virtio0",
        )));
        if let Err(err) = block.init() {
            let _ = err;
            let registrations: &'static [&'static BlockDeviceRegistration] =
                Box::leak(Box::new([scratch]));
            return register_block_devices(registrations);
        }

        let registration = Box::leak(Box::new(BlockDeviceRegistration {
            devt: DevT::new(254, 0),
            name: "vda",
            ops: block,
        }));
        let registrations: &'static [&'static BlockDeviceRegistration] = Box::leak(Box::new([
            registration as &'static BlockDeviceRegistration,
            scratch,
        ]));
        register_block_devices(registrations)
    }
}

impl<P: TxPlatform> Default for KernelBlockDevices<P> {
    fn default() -> Self {
        Self::new()
    }
}

struct ScratchBlockDevice;

impl BlockDeviceOps for ScratchBlockDevice {
    fn read_blocks(
        &self,
        _block_id: PhysicalBlockNumber,
        _target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }

    fn write_blocks(
        &self,
        _block_id: PhysicalBlockNumber,
        _source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }
}

impl BlockDevice for ScratchBlockDevice {
    fn total_blocks(&self) -> u64 {
        614_400
    }

    fn block_size(&self) -> u32 {
        512
    }
}

fn scratch_block_registration() -> &'static BlockDeviceRegistration {
    let block = Box::leak(Box::new(ScratchBlockDevice));
    Box::leak(Box::new(BlockDeviceRegistration {
        devt: DevT::new(254, 1),
        name: "ltpdev",
        ops: block,
    }))
}
