use alloc::boxed::Box;
use core::marker::PhantomData;

use crate::adapter::step_engine::{NoProgress, StepOutcome};
use tx_hal::{Arch, DeviceInfo, DeviceKind, IrqIf, MmioRegion, PlatformInfoIf, TxPlatform};
use tx_subsystems::device::{
    register_block_devices, BlockDevice, BlockDeviceOps, BlockDeviceRegistration, DevT,
    PhysicalBlockNumber,
};
use tx_subsystems::execution::Guard;
use tx_subsystems::net::{register_net_devices, NetDeviceRegistration};
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
                devt: DevT::new(179, 0), // Linux mmcblk major
                name: "mmcblk0",
                ops: sd,
            })));
        }

        regs.push(scratch_block_registration());
        let registrations: &'static [&'static BlockDeviceRegistration] =
            Box::leak(regs.into_boxed_slice());
        register_block_devices(registrations)
    }

    /// Probe the device table for a DesignWare MSHC SD controller (the
    /// VisionFive 2 SD card slot, discovered from the JH7110 device tree as
    /// `DeviceKind::SdController`). Maps its registers through the kernel
    /// direct map and runs the card-init handshake; returns the initialized
    /// block device, or `None` when no card came ready (or on QEMU, which has
    /// no such controller).
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

    /// Find the virtio block device by probing every discovered
    /// virtio-mmio slot: the block device lands on whichever slot
    /// QEMU's device ordering picked, and the driver's device-type
    /// peek skips net and empty slots without touching them. Boards
    /// without a device table fall back to the legacy "virtio0"
    /// named region.
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

/// Resolve a discovered device to its mapped MMIO region (published
/// by the board alongside the device table; matched by physical
/// base).
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

pub struct KernelNetDevices<P: TxPlatform> {
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> KernelNetDevices<P> {
    pub const fn new() -> Self {
        Self {
            _platform: PhantomData,
        }
    }

    pub fn init_and_register(&'static self) -> StepOutcome<(), NoProgress> {
        match P::ARCH {
            Arch::Riscv64 => self.init_rv64_qemu_virt(),
            Arch::LoongArch64 => self.init_la64_qemu_virt(),
        }
    }

    fn init_rv64_qemu_virt(&'static self) -> StepOutcome<(), NoProgress> {
        let Some(net) = Self::probe_virtio_mmio_net() else {
            return StepOutcome::Done(());
        };
        // Publish `eth0` before allowing the device to assert its line. The
        // controller handler is installed earlier in boot, while virtio
        // notifications remain disabled throughout `init()`.
        match self.register_eth0(net) {
            StepOutcome::Done(()) => {
                if <P as IrqIf>::NET_IRQ != 0 {
                    net.enable_interrupts();
                }
                StepOutcome::Done(())
            }
            other => other,
        }
    }

    /// Same slot-probing as the block path: the net device sits on
    /// whichever virtio-mmio slot QEMU assigned; the driver's
    /// device-type peek rejects non-net slots without touching them.
    fn probe_virtio_mmio_net() -> Option<&'static tx_drivers::virtio::VirtioMmioNet<P, 256>> {
        let devices = P::devices();
        let mut last_error = None;
        for device in devices {
            if device.kind != DeviceKind::VirtioMmio {
                continue;
            }
            let Some(region) = mmio_region_for::<P>(device) else {
                continue;
            };
            let net = Box::leak(Box::new(
                tx_drivers::virtio::VirtioMmioNet::<P, 256>::from_region(region),
            ));
            match net.init() {
                Ok(()) => return Some(net),
                Err(err) => last_error = Some(err),
            }
        }
        if devices.is_empty() {
            let net = Box::leak(Box::new(tx_drivers::virtio::VirtioMmioNet::<P, 256>::new(
                "virtio0",
            )));
            match net.init() {
                Ok(()) => return Some(net),
                Err(err) => last_error = Some(err),
            }
        }
        if let Some(err) = last_error {
            Self::write_net_init_error::<P>(err);
        }
        None
    }

    fn init_la64_qemu_virt(&'static self) -> StepOutcome<(), NoProgress> {
        let net = Box::leak(Box::new(tx_drivers::virtio::VirtioPciNet::<P, 256>::new(
            "pcie-ecam",
            "pcie-mmio32",
        )));
        if let Err(err) = net.init() {
            Self::write_net_init_error::<P>(err);
            return StepOutcome::Done(());
        }

        self.register_eth0(net)
    }

    fn register_eth0(
        &'static self,
        ops: &'static dyn tx_subsystems::net::NetDeviceOps,
    ) -> StepOutcome<(), NoProgress> {
        let registration = Box::leak(Box::new(NetDeviceRegistration {
            devt: DevT::new(97, 0),
            name: "eth0",
            ops,
        }));
        let registrations: &'static [&'static NetDeviceRegistration] =
            Box::leak(Box::new([registration as &'static NetDeviceRegistration]));
        register_net_devices(registrations)
    }

    fn write_net_init_error<Q: TxPlatform>(err: tx_drivers::virtio::net::VirtioNetError) {
        let reason = match err {
            tx_drivers::virtio::net::VirtioNetError::MissingMmioRegion(_) => "missing-mmio",
            tx_drivers::virtio::net::VirtioNetError::WrongDeviceType(_) => "wrong-device",
            tx_drivers::virtio::net::VirtioNetError::Mmio(_) => "mmio",
            tx_drivers::virtio::net::VirtioNetError::Pci(_) => "pci",
            tx_drivers::virtio::net::VirtioNetError::Transport => "transport",
        };
        tx_hal::console_write_str::<Q>("txkernel:");
        tx_hal::console_write_str::<Q>(Q::BOARD);
        tx_hal::console_write_str::<Q>(":devices:net:init-skip:");
        tx_hal::console_write_str::<Q>(reason);
        tx_hal::console_write_str::<Q>("\n");
    }
}

impl<P: TxPlatform> Default for KernelNetDevices<P> {
    fn default() -> Self {
        Self::new()
    }
}
