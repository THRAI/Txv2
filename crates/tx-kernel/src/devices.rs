use alloc::boxed::Box;
use core::marker::PhantomData;

use crate::adapter::step_engine::{NoProgress, StepOutcome};
use tx_hal::{Arch, TxPlatform};
use tx_subsystems::device::{register_block_devices, BlockDeviceRegistration, DevT};

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
        let block = Box::leak(Box::new(tx_drivers::virtio::VirtioPciBlock::<P>::new(
            "pcie-ecam",
            "pcie-mmio32",
        )));
        if block.init().is_err() {
            return StepOutcome::Done(());
        }

        let registration = Box::leak(Box::new(BlockDeviceRegistration {
            devt: DevT::new(254, 0),
            name: "vda",
            ops: block,
        }));
        let registrations: &'static [&'static BlockDeviceRegistration] =
            Box::leak(Box::new([registration as &'static BlockDeviceRegistration]));
        register_block_devices(registrations)
    }

    fn init_rv64_qemu_virt(&'static self) -> StepOutcome<(), NoProgress> {
        let block = Box::leak(Box::new(tx_drivers::virtio::VirtioMmioBlock::<P>::new(
            "virtio0",
        )));
        if let Err(err) = block.init() {
            let _ = err;
            return StepOutcome::Done(());
        }

        let registration = Box::leak(Box::new(BlockDeviceRegistration {
            devt: DevT::new(254, 0),
            name: "vda",
            ops: block,
        }));
        let registrations: &'static [&'static BlockDeviceRegistration] =
            Box::leak(Box::new([registration as &'static BlockDeviceRegistration]));
        register_block_devices(registrations)
    }
}

impl<P: TxPlatform> Default for KernelBlockDevices<P> {
    fn default() -> Self {
        Self::new()
    }
}
