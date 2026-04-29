use core::marker::PhantomData;

use tx_hal::{BootHandoff, TxPlatform};

/// Skeleton H3 boot spine for the generic kernel mainline.
///
/// This type names the ordering that used to live inline in `kernel_main`:
/// platform early init, substrate bring-up when the board supports it,
/// platform later init, full kernel trap vector installation, a tiny reactor
/// smoke task, the board boot sentinel, and shutdown. VFS, device, scheduler,
/// process, and userspace init are intentionally deferred until their
/// substrate contracts exist.
pub struct CoreInit<P: TxPlatform> {
    _platform: PhantomData<P>,
}

impl<P: TxPlatform> CoreInit<P> {
    pub fn boot(handoff: BootHandoff) -> ! {
        Self::init_early(handoff);
        Self::init_substrate_if_ready(handoff);
        Self::boot_sentinel();
        P::system_off()
    }

    fn init_early(handoff: BootHandoff) {
        P::init_early(handoff);
    }

    fn init_substrate_if_ready(handoff: BootHandoff) {
        if P::SUBSTRATE_BOOT_READY {
            tx_substrate::init::<P>();
            Self::init_later(handoff);
            Self::install_kernel_trap_vector();
            Self::run_reactor_smoke_task();

            // Deferred H4 spine slots:
            // - post-substrate init hooks
            // - VFS before device init
            // - post-device init hooks
            // - scheduler/process/userspace init
            //
            // Keep these as explicit placeholders until the named subsystems
            // have concrete no_std initialization contracts.
        }
    }

    fn init_later(handoff: BootHandoff) {
        P::init_later(handoff);
    }

    fn install_kernel_trap_vector() {
        P::install_kernel_trap_vector();
    }

    fn run_reactor_smoke_task() {
        let mut reactor = tx_reactor::Reactor::new();
        reactor.submit(async {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":reactor:task:ok\n");
        });
        reactor.run_until_idle();
    }

    fn boot_sentinel() {
        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":boot:ok\n");
    }

    fn write_board_sentinel_prefix() {
        tx_hal::console_write_str::<P>("txkernel:");
        tx_hal::console_write_str::<P>(P::BOARD);
    }
}
