//! Legacy block-device discovery kept separate from typed network binding.
//!
//! Block migration is outside the portable-network work. This module retains
//! the existing QEMU PCI/MMIO and VF2 SD-card behavior without exposing those
//! transport-specific choices to the network-device facade.

use alloc::boxed::Box;
use core::marker::PhantomData;

use crate::adapter::step_engine::{NoProgress, StepOutcome};
use tx_hal::{Arch, DeviceInfo, DeviceKind, MmioRegion, PlatformInfoIf, TxPlatform};
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
        let mut regs: alloc::vec::Vec<&'static BlockDeviceRegistration> = alloc::vec::Vec::new();

        // QEMU exposes the root disk as virtio-mmio (`vda`). The VisionFive 2
        // board has no virtio; there the SD card (DesignWare MSHC) is the root
        // disk and registers as `mmcblk0`. Probe virtio first; fall back to SD.
        if let Some(block) = Self::probe_virtio_mmio_block() {
            regs.push(Box::leak(Box::new(BlockDeviceRegistration {
                devt: DevT::new(254, 0),
                name: "vda",
                ops: block,
            })));
        } else if let Some(sd) = Self::probe_sd_block() {
            regs.push(Box::leak(Box::new(BlockDeviceRegistration {
                devt: DevT::new(179, 0),
                name: "mmcblk0",
                ops: sd,
            })));
        }

        regs.push(scratch_block_registration());
        let registrations: &'static [&'static BlockDeviceRegistration] =
            Box::leak(regs.into_boxed_slice());
        register_block_devices(registrations)
    }

    fn probe_sd_block() -> Option<&'static tx_drivers::mmc::Vf2Mmc> {
        for device in P::devices() {
            if device.kind != DeviceKind::SdController {
                continue;
            }
            let base = P::DIRECT_MAP_BASE.0 + device.mmio.start.0;
            let mmc = Box::leak(Box::new(tx_drivers::mmc::Vf2Mmc::new(base)));
            if mmc.card_init() {
                return Some(mmc);
            }
        }
        None
    }

    fn probe_virtio_mmio_block() -> Option<&'static tx_drivers::virtio::VirtioMmioBlock<P>> {
        let devices = P::devices();
        for device in devices {
            if device.kind != DeviceKind::VirtioMmio {
                continue;
            }
            let Some(region) = mmio_region_for::<P>(device) else {
                continue;
            };
            let block = Box::leak(Box::new(
                tx_drivers::virtio::VirtioMmioBlock::<P>::from_region(region),
            ));
            if block.init().is_ok() {
                return Some(block);
            }
        }
        if devices.is_empty() {
            let block = Box::leak(Box::new(tx_drivers::virtio::VirtioMmioBlock::<P>::new(
                "virtio0",
            )));
            if block.init().is_ok() {
                return Some(block);
            }
        }
        None
    }
}

fn mmio_region_for<P: TxPlatform>(device: &DeviceInfo) -> Option<MmioRegion> {
    <P as PlatformInfoIf>::platform_info()
        .mmio_regions
        .iter()
        .copied()
        .find(|region| region.phys.start == device.mmio.start)
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
