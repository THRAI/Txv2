//! Loongson 2K1000 legacy IO interrupt controller (liointc / "icu").
//!
//! Ported from Del0n1x `drivers/irqchip/la64/loongson_liointc.rs`
//! (their register sketch and route-byte format), with the register
//! map cross-checked against the real LS2K1000-DP device tree we
//! archived at `boards/dtbs/ls2k1000-dp-v10.dts`:
//!
//! ```dts
//! interrupt-controller@1fe01400 {
//!     compatible = "loongson,2k1000-icu";
//!     reg = <0x00 0x1fe01400 0x00 0x40    // route bytes + ctrl regs
//!            0x00 0x1fe01040 0x00 0x10>;  // per-core ISR (unused here)
//! };
//! serial@0x1fe20000 { interrupts = <0x00>; };  // UART0 = source 0
//! ```
//!
//! Layout used (all inside the first reg window):
//! - `0x1400 + n`: per-source route byte, `[3:0]` target core bit,
//!   `[7:4]` target INT pin bit (pin0 → CPU HWI0 → ESTAT.IS bit 2).
//! - `0x1420` INTISR (global pending), `0x1424` INTEN,
//!   `0x1428` INTENSET, `0x142c` INTENCLR.
//!
//! Del0n1x's per-core INTISR offsets (0x1440..) sit OUTSIDE the dts
//! window, so claim works from global `INTISR & INTEN` instead — we
//! run single-core and only enable UART0, which keeps that
//! unambiguous. UART0..3 share source 0 (both serial0 and serial1
//! carry `interrupts = <0x00>` in the vendor tree). The UART is
//! level-triggered; draining the RX FIFO deasserts the line, so
//! complete/EOI is a no-op and INTEDGE is left at its reset default.

#![cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]

use super::la64_pmap::la64_uncached_virt;

const LS2K1000_LIOINTC_ROUTE_BASE: usize = 0x1fe0_1400;
const LS2K1000_LIOINTC_INTISR: usize = 0x1fe0_1420;
const LS2K1000_LIOINTC_INTEN: usize = 0x1fe0_1424;
const LS2K1000_LIOINTC_INTENSET: usize = 0x1fe0_1428;
const LS2K1000_LIOINTC_INTENCLR: usize = 0x1fe0_142c;

/// UART0 interrupt source (dts: serial@0x1fe20000 `interrupts = <0>`).
pub(crate) const LS2K1000_UART0_SOURCE: u32 = 0;

/// Route byte: core 0, INT pin 0 (bit0 = core0, bit4 = pin0).
const LS2K1000_ROUTE_CORE0_PIN0: u8 = 0x11;

#[cfg(target_arch = "loongarch64")]
fn liointc_read_u32(phys: usize) -> u32 {
    unsafe { core::ptr::read_volatile(la64_uncached_virt(phys) as *const u32) }
}

#[cfg(target_arch = "loongarch64")]
fn liointc_write_u32(phys: usize, value: u32) {
    unsafe { core::ptr::write_volatile(la64_uncached_virt(phys) as *mut u32, value) }
}

#[cfg(target_arch = "loongarch64")]
fn liointc_write_route(source: u32, config: u8) {
    unsafe {
        core::ptr::write_volatile(
            la64_uncached_virt(LS2K1000_LIOINTC_ROUTE_BASE + source as usize) as *mut u8,
            config,
        )
    }
}

/// Enable a source: route it to core 0 / pin 0, then set its enable
/// bit. INTENSET/INTENCLR are write-1-to-set/clear, so no RMW races.
/// Register map verified on the real LS2K1000-DP (2026-07-04 flight:
/// post-enable read-back showed inten=0x1, route0=0x11).
pub(crate) fn ls2k1000_liointc_enable(source: u32) {
    #[cfg(target_arch = "loongarch64")]
    {
        liointc_write_route(source, LS2K1000_ROUTE_CORE0_PIN0);
        liointc_write_u32(LS2K1000_LIOINTC_INTENSET, 1 << source);
    }
    #[cfg(not(target_arch = "loongarch64"))]
    let _ = source;
}

pub(crate) fn ls2k1000_liointc_disable(source: u32) {
    #[cfg(target_arch = "loongarch64")]
    liointc_write_u32(LS2K1000_LIOINTC_INTENCLR, 1 << source);
    #[cfg(not(target_arch = "loongarch64"))]
    let _ = source;
}

/// Claim the lowest pending-and-enabled source, or `None`. A pending
/// source we never enabled (cannot happen while only UART0 is ever
/// ENSET) is disabled defensively so a stuck line cannot storm.
pub(crate) fn ls2k1000_liointc_claim() -> Option<u32> {
    #[cfg(target_arch = "loongarch64")]
    {
        let pending =
            liointc_read_u32(LS2K1000_LIOINTC_INTISR) & liointc_read_u32(LS2K1000_LIOINTC_INTEN);
        if pending == 0 {
            return None;
        }
        let source = pending.trailing_zeros();
        if source == LS2K1000_UART0_SOURCE {
            return Some(source);
        }
        ls2k1000_liointc_disable(source);
        None
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        None
    }
}
