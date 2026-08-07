use super::*;

#[cfg(not(target_arch = "loongarch64"))]
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

pub(crate) const MAIN_PHYS_BASE: usize = 0x1fe0_1400;
pub(crate) const MAIN_MMIO_SIZE: usize = 0x80;
pub(crate) const PUBLIC_IRQ_LIMIT: u32 = 65;
pub(crate) const UART_SHARED_PUBLIC_IRQ: u32 = 1;
/// The device tree's AHCI hwirq 19 in the public domain that reserves zero.
pub(crate) const AHCI_PUBLIC_IRQ: u32 = 20;

const BANKS: usize = 2;
const SOURCES_PER_BANK: usize = 32;
#[cfg(target_arch = "loongarch64")]
const BANK_STRIDE: usize = 0x40;
const ROUTE_CPU0_HWI1: u8 = 0x21;

const STATUS: usize = 0x20;
const ENABLE_STATUS: usize = 0x24;
const ENABLE_SET: usize = 0x28;
const ENABLE_CLEAR: usize = 0x2c;
const POLARITY: usize = 0x30;
const EDGE: usize = 0x34;
const BOUNCE: usize = 0x38;
const AUTO: usize = 0x3c;

const UART_COUNT_ON_SHARED_SOURCE: usize = 4;
#[cfg(target_arch = "loongarch64")]
const UART_STRIDE: usize = 0x100;
const UART_IIR: usize = 2;
const UART_MSR: usize = 6;
const UART_IER_ERBFI: u8 = 1 << 0;

pub(crate) fn prepare_uart0_irq() {
    for bank in 0..BANKS {
        write_bank_u32(bank, ENABLE_CLEAR, u32::MAX);
    }
    device_barrier();

    for uart in 0..UART_COUNT_ON_SHARED_SOURCE {
        let lcr = read_uart_u8(uart, UART_LCR);
        if lcr & UART_LCR_DLAB != 0 {
            write_uart_u8(uart, UART_LCR, lcr & !UART_LCR_DLAB);
        }
        write_uart_u8(uart, UART_IER, 0);
    }

    let prepared = prepare_level_high_source(UART_SHARED_PUBLIC_IRQ);
    debug_assert!(prepared);

    let _ = read_uart_u8(0, UART_IIR);
    for _ in 0..64 {
        if read_uart_u8(0, UART_LSR) & UART_LSR_DR == 0 {
            break;
        }
        let _ = read_uart_u8(0, UART_RBR);
    }
    let _ = read_uart_u8(0, UART_LSR);
    let _ = read_uart_u8(0, UART_MSR);
    write_uart_u8(0, UART_IER, UART_IER_ERBFI);
    device_barrier();
}

/// Route one level-high source to CPU0 HWI1 without enabling it.
///
/// The static binder can prepare a device before its handler is published;
/// unmasking remains the binder's final post-publication operation.
pub(crate) fn prepare_level_high_source(public_irq: u32) -> bool {
    let Some((bank, bit)) = decode_public_irq(public_irq) else {
        return false;
    };
    let source = public_irq as usize - 1;

    write_bank_u32(bank, ENABLE_CLEAR, bit);
    clear_config_bit(source, POLARITY);
    clear_config_bit(source, EDGE);
    clear_config_bit(source, BOUNCE);
    clear_config_bit(source, AUTO);
    write_route_u8(source, ROUTE_CPU0_HWI1);
    device_barrier();
    true
}

pub(crate) fn claim() -> u32 {
    for bank in 0..BANKS {
        let pending = read_bank_u32(bank, STATUS) & read_bank_u32(bank, ENABLE_STATUS);
        if pending != 0 {
            let source = bank * SOURCES_PER_BANK + pending.trailing_zeros() as usize;
            return source as u32 + 1;
        }
    }
    0
}

pub(crate) fn mask(public_irq: u32) {
    let Some((bank, bit)) = decode_public_irq(public_irq) else {
        return;
    };
    write_bank_u32(bank, ENABLE_CLEAR, bit);
    device_barrier();
}

pub(crate) fn unmask(public_irq: u32) {
    let Some((bank, bit)) = decode_public_irq(public_irq) else {
        return;
    };
    write_bank_u32(bank, ENABLE_SET, bit);
    device_barrier();
}

pub(crate) fn complete(_public_irq: u32) {
    // LIOINTC level interrupts have no controller EOI. The device handler
    // removes the source; UART RX does so by draining the 16550 receive FIFO.
}

fn decode_public_irq(public_irq: u32) -> Option<(usize, u32)> {
    let source = public_irq.checked_sub(1)? as usize;
    if source >= BANKS * SOURCES_PER_BANK {
        return None;
    }
    Some((
        source / SOURCES_PER_BANK,
        1u32 << (source % SOURCES_PER_BANK),
    ))
}

fn clear_config_bit(source: usize, offset: usize) {
    let bank = source / SOURCES_PER_BANK;
    let bit = 1u32 << (source % SOURCES_PER_BANK);
    let value = read_bank_u32(bank, offset) & !bit;
    write_bank_u32(bank, offset, value);
}

#[cfg(target_arch = "loongarch64")]
fn read_bank_u32(bank: usize, offset: usize) -> u32 {
    let phys = MAIN_PHYS_BASE + bank * BANK_STRIDE + offset;
    unsafe { core::ptr::read_volatile(la64_uncached_virt(phys) as *const u32) }
}

#[cfg(target_arch = "loongarch64")]
fn write_bank_u32(bank: usize, offset: usize, value: u32) {
    let phys = MAIN_PHYS_BASE + bank * BANK_STRIDE + offset;
    unsafe {
        core::ptr::write_volatile(la64_uncached_virt(phys) as *mut u32, value);
    }
}

#[cfg(target_arch = "loongarch64")]
fn write_route_u8(source: usize, value: u8) {
    let bank = source / SOURCES_PER_BANK;
    let local_source = source % SOURCES_PER_BANK;
    let phys = MAIN_PHYS_BASE + bank * BANK_STRIDE + local_source;
    unsafe {
        core::ptr::write_volatile(la64_uncached_virt(phys) as *mut u8, value);
    }
}

#[cfg(target_arch = "loongarch64")]
fn read_uart_u8(uart: usize, offset: usize) -> u8 {
    let phys = LA2K1000_UART_BASE + uart * UART_STRIDE + offset;
    unsafe { core::ptr::read_volatile(la64_uncached_virt(phys) as *const u8) }
}

#[cfg(target_arch = "loongarch64")]
fn write_uart_u8(uart: usize, offset: usize, value: u8) {
    let phys = LA2K1000_UART_BASE + uart * UART_STRIDE + offset;
    unsafe {
        core::ptr::write_volatile(la64_uncached_virt(phys) as *mut u8, value);
    }
}

#[cfg(target_arch = "loongarch64")]
fn device_barrier() {
    la64_irq_trap::la64_dbar();
}

#[cfg(not(target_arch = "loongarch64"))]
static HOST_STATUS: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_ENABLE: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_POLARITY: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_EDGE: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_BOUNCE: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_AUTO: [AtomicU32; BANKS] = [const { AtomicU32::new(0) }; BANKS];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_ROUTE: [AtomicU8; BANKS * SOURCES_PER_BANK] =
    [const { AtomicU8::new(0) }; BANKS * SOURCES_PER_BANK];
#[cfg(not(target_arch = "loongarch64"))]
static HOST_UART_IER: [AtomicU8; UART_COUNT_ON_SHARED_SOURCE] =
    [const { AtomicU8::new(0) }; UART_COUNT_ON_SHARED_SOURCE];

#[cfg(not(target_arch = "loongarch64"))]
fn read_bank_u32(bank: usize, offset: usize) -> u32 {
    let registers = match offset {
        STATUS => &HOST_STATUS,
        ENABLE_STATUS => &HOST_ENABLE,
        POLARITY => &HOST_POLARITY,
        EDGE => &HOST_EDGE,
        BOUNCE => &HOST_BOUNCE,
        AUTO => &HOST_AUTO,
        _ => return 0,
    };
    registers[bank].load(Ordering::Acquire)
}

#[cfg(not(target_arch = "loongarch64"))]
fn write_bank_u32(bank: usize, offset: usize, value: u32) {
    match offset {
        ENABLE_SET => {
            HOST_ENABLE[bank].fetch_or(value, Ordering::AcqRel);
        }
        ENABLE_CLEAR => {
            HOST_ENABLE[bank].fetch_and(!value, Ordering::AcqRel);
        }
        POLARITY => HOST_POLARITY[bank].store(value, Ordering::Release),
        EDGE => HOST_EDGE[bank].store(value, Ordering::Release),
        BOUNCE => HOST_BOUNCE[bank].store(value, Ordering::Release),
        AUTO => HOST_AUTO[bank].store(value, Ordering::Release),
        _ => {}
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn write_route_u8(source: usize, value: u8) {
    HOST_ROUTE[source].store(value, Ordering::Release);
}

#[cfg(not(target_arch = "loongarch64"))]
fn read_uart_u8(uart: usize, offset: usize) -> u8 {
    match offset {
        UART_IER => HOST_UART_IER[uart].load(Ordering::Acquire),
        _ => 0,
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn write_uart_u8(uart: usize, offset: usize, value: u8) {
    if offset == UART_IER {
        HOST_UART_IER[uart].store(value, Ordering::Release);
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn device_barrier() {
    core::sync::atomic::fence(Ordering::SeqCst);
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset() {
        for bank in 0..BANKS {
            HOST_STATUS[bank].store(0, Ordering::Release);
            HOST_ENABLE[bank].store(0, Ordering::Release);
            HOST_POLARITY[bank].store(u32::MAX, Ordering::Release);
            HOST_EDGE[bank].store(u32::MAX, Ordering::Release);
            HOST_BOUNCE[bank].store(u32::MAX, Ordering::Release);
            HOST_AUTO[bank].store(u32::MAX, Ordering::Release);
        }
        for route in &HOST_ROUTE {
            route.store(0, Ordering::Release);
        }
        for ier in &HOST_UART_IER {
            ier.store(u8::MAX, Ordering::Release);
        }
    }

    #[test]
    fn public_irq_domain_preserves_zero_as_no_claim() {
        let _guard = TEST_LOCK.lock().expect("LIOINTC test lock poisoned");
        reset();
        HOST_STATUS[0].store(1, Ordering::Release);
        assert_eq!(claim(), 0);

        unmask(UART_SHARED_PUBLIC_IRQ);
        assert_eq!(claim(), UART_SHARED_PUBLIC_IRQ);
        mask(UART_SHARED_PUBLIC_IRQ);
        assert_eq!(claim(), 0);

        HOST_STATUS[1].store(1 << 31, Ordering::Release);
        unmask(64);
        assert_eq!(claim(), 64);
    }

    #[test]
    fn uart_prepare_masks_other_sources_and_programs_cpu0_hwi1_route() {
        let _guard = TEST_LOCK.lock().expect("LIOINTC test lock poisoned");
        reset();
        HOST_ENABLE[0].store(u32::MAX, Ordering::Release);
        HOST_ENABLE[1].store(u32::MAX, Ordering::Release);

        prepare_uart0_irq();

        assert_eq!(HOST_ENABLE[0].load(Ordering::Acquire), 0);
        assert_eq!(HOST_ENABLE[1].load(Ordering::Acquire), 0);
        assert_eq!(HOST_ROUTE[0].load(Ordering::Acquire), ROUTE_CPU0_HWI1);
        assert_eq!(HOST_POLARITY[0].load(Ordering::Acquire) & 1, 0);
        assert_eq!(HOST_EDGE[0].load(Ordering::Acquire) & 1, 0);
        assert_eq!(HOST_BOUNCE[0].load(Ordering::Acquire) & 1, 0);
        assert_eq!(HOST_AUTO[0].load(Ordering::Acquire) & 1, 0);
        assert_eq!(HOST_UART_IER[0].load(Ordering::Acquire), UART_IER_ERBFI);
        for uart in 1..UART_COUNT_ON_SHARED_SOURCE {
            assert_eq!(HOST_UART_IER[uart].load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn invalid_public_irqs_do_not_change_controller_state() {
        let _guard = TEST_LOCK.lock().expect("LIOINTC test lock poisoned");
        reset();
        unmask(0);
        unmask(PUBLIC_IRQ_LIMIT);
        assert_eq!(HOST_ENABLE[0].load(Ordering::Acquire), 0);
        assert_eq!(HOST_ENABLE[1].load(Ordering::Acquire), 0);
    }

    #[test]
    fn ahci_platform_prepare_routes_level_high_and_leaves_source_masked() {
        let _guard = TEST_LOCK.lock().expect("LIOINTC test lock poisoned");
        reset();
        HOST_ENABLE[0].store(u32::MAX, Ordering::Release);

        <Platform as PlatformInfoIf>::prepare_platform_device(&LA2K1000_PLATFORM_DEVICES[0])
            .expect("AHCI platform preparation");

        let source = (AHCI_PUBLIC_IRQ - 1) as usize;
        let bit = 1u32 << source;
        assert_eq!(HOST_ENABLE[0].load(Ordering::Acquire) & bit, 0);
        assert_ne!(HOST_ENABLE[0].load(Ordering::Acquire) & (1 << 18), 0);
        assert_eq!(HOST_ROUTE[source].load(Ordering::Acquire), ROUTE_CPU0_HWI1);
        assert_eq!(HOST_POLARITY[0].load(Ordering::Acquire) & bit, 0);
        assert_eq!(HOST_EDGE[0].load(Ordering::Acquire) & bit, 0);
        assert_eq!(HOST_BOUNCE[0].load(Ordering::Acquire) & bit, 0);
        assert_eq!(HOST_AUTO[0].load(Ordering::Acquire) & bit, 0);
    }
}
