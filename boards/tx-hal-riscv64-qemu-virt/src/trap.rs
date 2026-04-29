use crate::{boot_static::BootStaticBag, boot_static::IdentityLive, Platform};
use tx_hal::{TrapClass, TrapFrameSnapshot, TrapIf};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trap, "ax"
    .align 2
    .globl tx_rv64_qemu_minimal_trap_vector
tx_rv64_qemu_minimal_trap_vector:
    csrr a0, scause
    csrr a1, sepc
    csrr a2, stval
    call tx_rv64_qemu_trap_panic
4:
    wfi
    j 4b
"#
);

impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_rv64_trap_vector();
    }

    fn install_kernel_trap_vector() {
        install_rv64_trap_vector();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_rv64_trap(snapshot.scause)
    }
}

pub(crate) fn classify_rv64_trap(scause: usize) -> TrapClass {
    let interrupt_bit = 1usize << (usize::BITS as usize - 1);
    let is_interrupt = scause & interrupt_bit != 0;
    let code = scause & !interrupt_bit;

    match (is_interrupt, code) {
        (false, 0) => TrapClass::AlignmentFault {
            write: false,
            instruction: true,
        },
        (false, 2) => TrapClass::IllegalInstruction,
        (false, 3) => TrapClass::Breakpoint,
        (false, 4) => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        (false, 6) => TrapClass::AlignmentFault {
            write: true,
            instruction: false,
        },
        (false, 8) => TrapClass::Syscall,
        (false, 12) => TrapClass::PageFault {
            write: false,
            instruction: true,
        },
        (false, 13) => TrapClass::PageFault {
            write: false,
            instruction: false,
        },
        (false, 15) => TrapClass::PageFault {
            write: true,
            instruction: false,
        },
        (true, 1) => TrapClass::InterprocessorInterrupt,
        (true, 5) => TrapClass::TimerInterrupt,
        (true, 9) => TrapClass::ExternalInterrupt,
        (false, _) => TrapClass::UnknownSync,
        (true, _) => TrapClass::UnknownInterrupt,
    }
}

fn install_rv64_trap_vector() {
    let vector = BootStaticBag::<IdentityLive>::current_trap_vector_kernel_alias();

    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "csrw stvec, {vector}",
            vector = in(reg) vector.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = vector;
}

#[cfg(target_arch = "riscv64")]
#[no_mangle]
extern "C" fn tx_rv64_qemu_trap_panic(scause: usize, sepc: usize, stval: usize) -> ! {
    console_write_literal(b"txkernel:qemu-riscv64-virt:trap\nscause=0x");
    console_write_hex(scause);
    console_write_literal(b" sepc=0x");
    console_write_hex(sepc);
    console_write_literal(b" stval=0x");
    console_write_hex(stval);
    console_write_literal(b"\n");

    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_literal(bytes: &[u8]) {
    for &byte in bytes {
        crate::sbi_console_putchar(byte);
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_hex(value: usize) {
    for shift in (0..usize::BITS).rev().step_by(4) {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        crate::sbi_console_putchar(byte);
    }
}
