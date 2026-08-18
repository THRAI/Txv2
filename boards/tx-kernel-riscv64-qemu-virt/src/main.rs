#![no_std]
#![no_main]

#[cfg(not(test))]
use core::panic::PanicInfo;
use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_riscv64_qemu_virt::Platform;

struct ActiveDeviceBundle;

const ACTIVE_DRIVERS: [tx_kernel::devices::binder::StaticDriverDescriptor<ActivePlatform>; 2] = [
    tx_kernel::devices::virtio_mmio_net::driver_descriptor::<ActivePlatform>(),
    tx_kernel::devices::dwmac_net::driver_descriptor::<ActivePlatform>(),
];

impl tx_kernel::devices::binder::StaticDeviceBundle<ActivePlatform> for ActiveDeviceBundle {
    fn resource_providers() -> &'static [tx_hal::ResourceProviderDescriptor<ActivePlatform>] {
        &[]
    }

    fn drivers() -> &'static [tx_kernel::devices::binder::StaticDriverDescriptor<ActivePlatform>] {
        &ACTIVE_DRIVERS
    }
}

struct Kernel;

impl KernelMain<ActivePlatform> for Kernel {
    fn kernel_main(handoff: BootHandoff) -> ! {
        tx_kernel::kernel_main::<ActivePlatform, ActiveDeviceBundle>(handoff)
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
    let _ = writeln!(ConsoleWriter, "\ntxkernel:panic: {info}");
    // Read ra/s0 early so fault-decode can reconstruct the panic call stack.
    // ra = return address from the panic call site (used as synthetic sepc).
    // s0 = frame pointer at panic entry (root of the fp-chain walk).
    let ra: usize;
    let fp: usize;
    unsafe {
        core::arch::asm!(
            "mv {ra}, ra",
            "mv {fp}, s0",
            ra = out(reg) ra,
            fp = out(reg) fp,
        );
        tx_hal_riscv64_qemu_virt::emit_panic_location(fp, ra);
    }
    tx_kernel::panic_shutdown::<ActivePlatform>()
}
