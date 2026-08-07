//! Raw U-Boot argument capture for the Loongson 2K1000 bootm ABI.

use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{CpuId, PhysAddr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct La2k1000BootArgs {
    pub(crate) cpu_id: CpuId,
    pub(crate) boot_flag: usize,
    pub(crate) cmdline: PhysAddr,
    pub(crate) system_table: PhysAddr,
    pub(crate) reserved: usize,
}

static CPU_ID: AtomicUsize = AtomicUsize::new(0);
static BOOT_FLAG: AtomicUsize = AtomicUsize::new(0);
static CMDLINE: AtomicUsize = AtomicUsize::new(0);
static SYSTEM_TABLE: AtomicUsize = AtomicUsize::new(0);
static RESERVED: AtomicUsize = AtomicUsize::new(0);

pub fn capture_loongarch64_2k1000_boot_args(
    cpu_id: usize,
    boot_flag: usize,
    cmdline: usize,
    system_table: usize,
    reserved: usize,
) {
    CPU_ID.store(cpu_id, Ordering::Release);
    BOOT_FLAG.store(boot_flag, Ordering::Release);
    CMDLINE.store(cmdline, Ordering::Release);
    SYSTEM_TABLE.store(system_table, Ordering::Release);
    RESERVED.store(reserved, Ordering::Release);
}

pub(crate) fn snapshot() -> La2k1000BootArgs {
    La2k1000BootArgs {
        cpu_id: CpuId(CPU_ID.load(Ordering::Acquire)),
        boot_flag: BOOT_FLAG.load(Ordering::Acquire),
        cmdline: PhysAddr(CMDLINE.load(Ordering::Acquire)),
        system_table: PhysAddr(SYSTEM_TABLE.load(Ordering::Acquire)),
        reserved: RESERVED.load(Ordering::Acquire),
    }
}
