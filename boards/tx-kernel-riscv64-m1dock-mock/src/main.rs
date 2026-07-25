#![no_std]
#![no_main]

#[cfg(not(test))]
use core::panic::PanicInfo;
use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_riscv64_m1dock_mock::Platform;

struct Kernel;

impl KernelMain<ActivePlatform> for Kernel {
    fn kernel_main(handoff: BootHandoff) -> ! {
        tx_kernel::kernel_main::<ActivePlatform>(handoff)
    }
}

#[no_mangle]
pub extern "C" fn rust_entry(cpu_id: usize, firmware_arg: usize) -> ! {
    tx_hal::entry::<ActivePlatform, Kernel>(cpu_id, firmware_arg)
}

#[panic_handler]
#[cfg(not(test))]
fn panic(_info: &PanicInfo<'_>) -> ! {
    tx_kernel::panic_shutdown::<ActivePlatform>()
}
