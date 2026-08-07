#![no_std]

mod boot_asm;

pub struct Platform;

const UART_PHYS_BASE: usize = 0x1fe2_0000;
const LA64_UNCACHED_DMW_BASE: usize = 0x8000_0000_0000_0000;
const UART_THR: usize = 0;
const UART_IER: usize = 1;
const UART_FCR: usize = 2;
const UART_LCR: usize = 3;
const UART_MCR: usize = 4;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u8 = 1 << 5;
const UART_LSR_TEMT: u8 = 1 << 6;
const UART_LCR_DLAB: u8 = 1 << 7;
const UART_DIVISOR_125MHZ_115200: u16 = 68;

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn uart_reg(offset: usize) -> *mut u8 {
    (LA64_UNCACHED_DMW_BASE | UART_PHYS_BASE | offset) as *mut u8
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn uart_read(offset: usize) -> u8 {
    unsafe { uart_reg(offset).read_volatile() }
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn uart_write(offset: usize, value: u8) {
    unsafe { uart_reg(offset).write_volatile(value) };
}

#[cfg(target_arch = "loongarch64")]
fn uart_putc(byte: u8) {
    while uart_read(UART_LSR) & UART_LSR_THRE == 0 {
        core::hint::spin_loop();
    }
    uart_write(UART_THR, byte);
}

#[cfg(not(target_arch = "loongarch64"))]
fn uart_putc(_byte: u8) {}

pub fn early_console_write(bytes: &[u8]) {
    for &byte in bytes {
        if byte == b'\n' {
            uart_putc(b'\r');
        }
        uart_putc(byte);
    }
}

#[cfg(target_arch = "loongarch64")]
fn reinit_uart_115200() {
    while uart_read(UART_LSR) & UART_LSR_TEMT == 0 {
        core::hint::spin_loop();
    }
    uart_write(UART_IER, 0);
    uart_write(UART_LCR, UART_LCR_DLAB);
    uart_write(UART_THR, UART_DIVISOR_125MHZ_115200 as u8);
    uart_write(UART_IER, (UART_DIVISOR_125MHZ_115200 >> 8) as u8);
    uart_write(UART_LCR, 0x03);
    uart_write(UART_FCR, 0x07);
    uart_write(UART_MCR, 0x03);
}

#[cfg(not(target_arch = "loongarch64"))]
fn reinit_uart_115200() {}

pub fn phase1_early_boot(
    cpu_id: usize,
    _boot_flag: usize,
    _cmdline_phys: usize,
    _system_table_phys: usize,
    _reserved: usize,
) -> ! {
    if cpu_id == 0 {
        early_console_write(b"txkernel:loongson-2k1000:h1:uart-inherited:ok\n");
        reinit_uart_115200();
        early_console_write(b"txkernel:loongson-2k1000:h1:uart-reinit:ok\n");
    }

    loop {
        core::hint::spin_loop();
    }
}
