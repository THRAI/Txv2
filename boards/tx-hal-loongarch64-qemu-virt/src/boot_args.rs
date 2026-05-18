//! LA64 direct-boot argument capture.
//!
//! This is the single home for raw boot inputs that arrive before boot facts
//! are published.

use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{CpuId, PhysAddr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct La64BootArgs {
    pub cpu_id: CpuId,
    pub efi_boot: usize,
    pub cmdline_phys: PhysAddr,
    pub system_table_phys: PhysAddr,
    pub legacy_firmware_arg: PhysAddr,
}

static LA64_BOOT_CPU_ID: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_EFI_BOOT: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_CMDLINE_PTR: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_SYSTEM_TABLE: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_LEGACY_FIRMWARE_ARG: AtomicUsize = AtomicUsize::new(0);

pub fn capture_loongarch64_qemu_boot_args(
    cpu_id: usize,
    efi_boot: usize,
    cmdline_phys: usize,
    system_table_phys: usize,
) {
    LA64_BOOT_CPU_ID.store(cpu_id, Ordering::Release);
    LA64_BOOT_EFI_BOOT.store(efi_boot, Ordering::Release);
    LA64_BOOT_CMDLINE_PTR.store(cmdline_phys, Ordering::Release);
    LA64_BOOT_SYSTEM_TABLE.store(system_table_phys, Ordering::Release);
}

pub(crate) fn record_legacy_firmware_arg(firmware_arg: usize) {
    LA64_BOOT_LEGACY_FIRMWARE_ARG.store(firmware_arg, Ordering::Release);
}

pub(crate) fn snapshot() -> La64BootArgs {
    La64BootArgs {
        cpu_id: CpuId(LA64_BOOT_CPU_ID.load(Ordering::Acquire)),
        efi_boot: LA64_BOOT_EFI_BOOT.load(Ordering::Acquire),
        cmdline_phys: PhysAddr(LA64_BOOT_CMDLINE_PTR.load(Ordering::Acquire)),
        system_table_phys: PhysAddr(LA64_BOOT_SYSTEM_TABLE.load(Ordering::Acquire)),
        legacy_firmware_arg: PhysAddr(LA64_BOOT_LEGACY_FIRMWARE_ARG.load(Ordering::Acquire)),
    }
}
