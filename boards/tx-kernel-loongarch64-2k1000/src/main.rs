#![no_std]
#![no_main]

#[cfg(not(test))]
use core::panic::PanicInfo;

#[no_mangle]
pub extern "C" fn rust_entry(
    cpu_id: usize,
    boot_flag: usize,
    cmdline_phys: usize,
    system_table_phys: usize,
    reserved: usize,
) -> ! {
    tx_hal_loongarch64_2k1000::phase1_early_boot(
        cpu_id,
        boot_flag,
        cmdline_phys,
        system_table_phys,
        reserved,
    )
}

#[panic_handler]
#[cfg(not(test))]
fn panic(_info: &PanicInfo<'_>) -> ! {
    tx_hal_loongarch64_2k1000::early_console_write(b"txkernel:loongson-2k1000:h1:panic\n");
    loop {
        core::hint::spin_loop();
    }
}
