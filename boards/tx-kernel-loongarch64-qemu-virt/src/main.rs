#![no_std]
#![no_main]

#[cfg(not(test))]
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
        cpu_id,
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
#[cfg(not(test))]
fn panic(info: &PanicInfo<'_>) -> ! {
    use core::fmt::Write;

    struct ConsoleWriter;
    impl Write for ConsoleWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            tx_hal::console_write_str::<ActivePlatform>(s);
            Ok(())
        }
    }

    let _ = writeln!(ConsoleWriter, "txkernel:panic:{info}");
    tx_kernel::panic_shutdown::<ActivePlatform>()
}
