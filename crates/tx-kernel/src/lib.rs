#![no_std]

pub mod vm;

use tx_hal::{BootHandoff, TxPlatform};

pub mod init;

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
    init::CoreInit::<P>::boot(handoff)
}
