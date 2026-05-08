#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod zones;

pub mod init;
pub mod irq;
pub mod thread_future;
pub mod trap;
pub mod trap_handoff;

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
    init::CoreInit::<P>::boot(handoff)
}

pub fn panic_shutdown<P: TxPlatform>() -> ! {
    zones::panic_shutdown::<P>()
}

#[cfg(test)]
mod test_serialise {
    //! Process-wide serialisation lock for every tx-kernel host test
    //! that bootstraps the global `INIT_PROCESS` slot, the per-hart
    //! payload table, or the mount/console singletons. The init tests
    //! and the thread_future tests both touch these slots; running
    //! them concurrently breaks bootstrap re-init. One Mutex serialises
    //! the whole tx-kernel test set so the default
    //! `cargo test -p tx-kernel --lib` invocation stays green without
    //! requiring `--test-threads=1`.
    use std::sync::Mutex;
    pub(crate) static KERNEL_TEST_LOCK: Mutex<()> = Mutex::new(());
}
