#![no_std]
#![no_main]

use core::panic::PanicInfo;
use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_loongarch64_qemu_virt::Platform;

struct Kernel;

impl KernelMain<ActivePlatform> for Kernel {
    fn kernel_main(handoff: BootHandoff) -> ! {
        tx_kernel::kernel_main::<ActivePlatform>(handoff)
    }
}

#[no_mangle]
pub extern "C" fn rust_entry(
    cpu_id: usize,
    efi_boot: usize,
    cmdline_phys: usize,
    system_table_phys: usize,
) -> ! {
    tx_hal_loongarch64_qemu_virt::capture_loongarch64_qemu_boot_args(
        efi_boot,
        cmdline_phys,
        system_table_phys,
    );
    tx_hal::entry::<ActivePlatform, Kernel>(cpu_id, system_table_phys)
}

#[no_mangle]
pub extern "C" fn tx_kernel_loongarch64_qemu_trap_dispatch(
    frame: *mut tx_hal_loongarch64_qemu_virt::La64TrapFrame,
) -> tx_hal::TrapAction {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        return tx_hal::TrapAction::Terminate;
    };

    tx_hal_loongarch64_qemu_virt::dispatch_trap_frame::<tx_kernel::trap::KernelTrapDispatcher>(
        frame,
    )
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b"txkernel:panic:");
    if let Some(location) = info.location() {
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(location.file().as_bytes());
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b":");
        write_decimal(location.line() as usize);
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b":");
        write_decimal(location.column() as usize);
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b":");
    }
    if let Some(message) = info.message().as_str() {
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(message.as_bytes());
    }
    <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b"\n");
    tx_kernel::panic_shutdown::<ActivePlatform>()
}

fn write_decimal(mut value: usize) {
    if value == 0 {
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(b"0");
        return;
    }

    let mut digits = [0u8; 20];
    let mut len = 0;
    while value != 0 {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
    }
    while len != 0 {
        len -= 1;
        <ActivePlatform as tx_hal::ConsoleIf>::write_bytes(&digits[len..len + 1]);
    }
}
