#![no_std]

extern crate alloc;
pub mod adapter;
#[cfg(test)]
extern crate std;

mod zones;

pub mod devices;
pub mod init;
pub mod irq;
pub mod thread_future;
pub mod trap;
pub mod trap_handoff;
pub mod vdso;

use tx_hal::{BootHandoff, TxPlatform};

/// Bounded-trace dump threshold.
///
/// When set to `N > 0`, the kernel dumps the observation ring over the
/// console and powers off after `N` records have been emitted. Set to
/// `0` (disabled) for normal runs where init eventually exits and the
/// existing `init/exec.rs` dump path captures the full trace.
///
/// Used by long-running workloads where init never naturally exits in
/// a useful wall clock (e.g. oscomp's continuous basic-musl → busybox
/// → libctest → cyclictest → LTP test-group sequence). A value of
/// roughly 2–4× the basic-musl record budget (~10 000 records) keeps
/// the dump fast while comfortably covering the early test groups
/// before the producer starts wrapping the ring.
///
/// The threshold is installed once at BSP substrate-init time
/// (`init.rs::init_substrate_if_ready`) via
/// `tx_observe::set_dump_threshold`.
pub const OBSERVE_DUMP_THRESHOLD: u64 = 0;

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
