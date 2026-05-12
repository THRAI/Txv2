#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod zones;

pub mod devices;
pub mod init;
pub mod irq;
pub mod thread_future;
pub mod trap;
pub mod trap_handoff;

/// DIAGNOSTIC (temp, 2026-05-12): captures the most-recent SIGSEGV
/// triggered by a page-fault under `thread_future`. Useful for
/// post-mortem in shell-test runs.
pub static FAULT_SIGSEGV_ADDR: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static FAULT_SIGSEGV_ACCESS: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub static FAULT_SIGSEGV_HITS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static FAULT_SIGSEGV_PID: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

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
