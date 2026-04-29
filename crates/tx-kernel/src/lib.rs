#![no_std]

use tx_hal::{BootHandoff, TxPlatform};

pub fn kernel_main<P: TxPlatform + 'static>(handoff: BootHandoff) -> ! {
    P::init_early(handoff);
    tx_substrate::init::<P>();
    P::init_later(handoff);
    P::install_kernel_trap_vector();
    let mut reactor = tx_reactor::Reactor::new();
    reactor.submit(async {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
        tx_hal::console_write_str::<P>(":reactor:task:ok\n");
    });
    reactor.run_until_idle();
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":boot:ok\n");
    P::system_off()
}
