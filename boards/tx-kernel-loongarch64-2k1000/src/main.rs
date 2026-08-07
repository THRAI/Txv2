#![no_std]
#![no_main]

#[cfg(not(test))]
use core::panic::PanicInfo;
use tx_hal::{BootHandoff, KernelMain};

type ActivePlatform = tx_hal_loongarch64_2k1000::Platform;

struct ActiveDeviceBundle;

static ACTIVE_DRIVERS: [tx_kernel::devices::binder::StaticDriverDescriptor<ActivePlatform>; 1] =
    [tx_kernel::devices::ahci_block::driver_descriptor::<
        ActivePlatform,
    >()];

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
pub extern "C" fn rust_entry(
    cpu_id: usize,
    boot_flag: usize,
    cmdline_phys: usize,
    system_table_phys: usize,
    reserved: usize,
) -> ! {
    tx_hal_loongarch64_2k1000::initialize_early_board();
    tx_hal_loongarch64_2k1000::capture_loongarch64_2k1000_boot_args(
        cpu_id,
        boot_flag,
        cmdline_phys,
        system_table_phys,
        reserved,
    );
    tx_hal::entry::<ActivePlatform, Kernel>(cpu_id, system_table_phys)
}

#[no_mangle]
pub extern "C" fn tx_kernel_loongarch64_trap_dispatch(
    frame: *mut tx_hal_loongarch64_2k1000::La64TrapFrame,
) -> tx_hal::TrapAction {
    let Some(frame) = (unsafe { frame.as_mut() }) else {
        return tx_hal::TrapAction::Terminate;
    };
    tx_hal_loongarch64_2k1000::dispatch_trap_frame::<tx_kernel::trap::KernelTrapDispatcher>(frame)
}

#[panic_handler]
#[cfg(not(test))]
fn panic(info: &PanicInfo<'_>) -> ! {
    use core::fmt::Write;

    struct ConsoleWriter;
    impl Write for ConsoleWriter {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            tx_hal::console_write_str::<ActivePlatform>(text);
            Ok(())
        }
    }

    let _ = writeln!(ConsoleWriter, "txkernel:panic:{info}");
    tx_kernel::panic_shutdown::<ActivePlatform>()
}
