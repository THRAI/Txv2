use alloc::boxed::Box;
use core::marker::PhantomData;

use tx_hal::{Arch, TxPlatform};
use tx_subsystems::{
    device::{register_block_devices, BlockDeviceRegistration, DevT},
    execution::StepOutcome,
};

pub struct KernelBlockDevices<P: TxPlatform> {
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> KernelBlockDevices<P> {
    pub const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }

    pub fn init_and_register(&'static self) -> StepOutcome<()> {
        match P::ARCH {
            Arch::LoongArch64 => self.init_la64_qemu_virt(),
            _ => StepOutcome::Done(()),
        }
    }

    fn init_la64_qemu_virt(&'static self) -> StepOutcome<()> {
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
}

impl<P: TxPlatform> Default for KernelBlockDevices<P> {
    fn default() -> Self {
        Self::new()
    }
}
