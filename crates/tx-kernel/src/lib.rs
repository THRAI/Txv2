#![no_std]

use tx_hal::{BootHandoff, TxPlatform};

pub fn kernel_main<P: TxPlatform>(handoff: BootHandoff) -> ! {
    P::init_early(handoff);
    tx_substrate::init::<P>();
    P::init_later(handoff);
    P::install_kernel_trap_vector();
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":boot:ok\n");
    P::system_off()
}
