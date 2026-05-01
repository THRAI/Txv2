#![no_std]
#![no_main]

use core::panic::PanicInfo;
use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_riscv64_qemu_virt::Platform;

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

#[no_mangle]
pub extern "C" fn tx_kernel_riscv64_qemu_trap_dispatch(
    frame: *mut tx_hal_riscv64_qemu_virt::Rv64TrapFrame,
) -> tx_hal::TrapAction {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        return tx_hal::TrapAction::Terminate;
    };

    tx_hal_riscv64_qemu_virt::dispatch_trap_frame::<tx_kernel::trap::KernelTrapDispatcher>(frame)
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
