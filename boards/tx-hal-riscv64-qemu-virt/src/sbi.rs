// Auto-extracted from `boards/tx-hal-riscv64-qemu-virt/src/lib.rs` (2026-05-08
// jumbo split).
//
// Thin wrappers over the SBI ecall ABI used by the QEMU-virt board: legacy
// console putchar / getchar (EID 1, 2), legacy shutdown (EID 8), HSM hart_start
// (EID 0x48534D / fid 0), sPI send_ipi (EID 0x735049), and RFNC remote-fence
// variants (EID 0x52464E43, fids 0/1/2). Each ecall site owns its own
// `core::arch::asm!` so the constraint set stays local; non-rv64 builds expose
// stubs where appropriate so the host test target links cleanly.

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_console_putchar(byte: u8) {
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") byte as usize => _,
            in("a7") 1usize,
            clobber_abi("C"),
            options(nostack)
        );
    }
}

pub(super) fn read_sbi_console_bytes(buf: &mut [u8]) -> usize {
    let mut read = 0;
    for byte in buf {
        let Some(next) = sbi_console_getchar() else {
            break;
        };
        *byte = next;
        read += 1;
    }
    read
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_console_getchar() -> Option<u8> {
    let value: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            lateout("a0") value,
            in("a7") 2usize,
            clobber_abi("C"),
            options(nostack)
        );
    }

    if value < 0 {
        None
    } else {
        Some(value as u8)
    }
}

#[cfg(not(target_arch = "riscv64"))]
pub(super) fn sbi_console_getchar() -> Option<u8> {
    None
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_hart_start(hart_id: usize, start_addr: usize, opaque: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_id => error,
            in("a1") start_addr,
            in("a2") opaque,
            in("a6") 0usize,
            in("a7") 0x48534dusize,
            lateout("a1") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_send_ipi(hart_mask: u64, hart_mask_base: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a6") 0usize,
            in("a7") 0x735049usize,
            lateout("a1") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_remote_fence_i(hart_mask: u64, hart_mask_base: usize) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a6") 0usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_remote_sfence_vma(
    hart_mask: u64,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a2") start_addr,
            in("a3") size,
            in("a6") 1usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_remote_sfence_vma_asid(
    hart_mask: u64,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
    asid: usize,
) -> isize {
    let error: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") hart_mask as usize => error,
            in("a1") hart_mask_base,
            in("a2") start_addr,
            in("a3") size,
            in("a4") asid,
            in("a6") 2usize,
            in("a7") 0x52464e43usize,
            lateout("a1") _,
            clobber_abi("C"),
            options(nostack)
        );
    }
    error
}

#[cfg(target_arch = "riscv64")]
pub(super) fn sbi_shutdown() {
    unsafe {
        core::arch::asm!("ecall", in("a7") 8usize, clobber_abi("C"), options(nostack));
    }
}
