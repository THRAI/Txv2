#![no_std]
#![no_main]

use core::panic::PanicInfo;

type ActivePlatform = tx_hal_riscv64_m1dock_mock::Platform;

#[no_mangle]
pub extern "C" fn rust_entry(cpu_id: usize, firmware_arg: usize) -> ! {
    tx_kernel::kernel_main::<ActivePlatform>(cpu_id, firmware_arg)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    tx_kernel::panic_shutdown::<ActivePlatform>()
}
