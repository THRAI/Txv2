#![no_std]

pub mod vm;

use tx_hal::{BootHandoff, TxPlatform};

#[cfg(all(not(target_os = "none"), not(test)))]
mod host_check_allocator {
    use core::alloc::{GlobalAlloc, Layout};
    use core::ptr;

    struct HostCheckAllocator;

    unsafe impl GlobalAlloc for HostCheckAllocator {
        unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
            ptr::null_mut()
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }

    #[global_allocator]
    static HOST_CHECK_ALLOCATOR: HostCheckAllocator = HostCheckAllocator;
}

pub fn kernel_main<P: TxPlatform + 'static>(handoff: BootHandoff) -> ! {
    P::init_early(handoff);
    if P::SUBSTRATE_BOOT_READY {
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
    }
    tx_hal::console_write_str::<P>("txkernel:");
    tx_hal::console_write_str::<P>(P::BOARD);
    tx_hal::console_write_str::<P>(":boot:ok\n");
    P::system_off()
}
