//! Kernel-mode user-space access for the RV64 QEMU board.
//!
//! # Design
//!
//! `copy_from_user` and `copy_to_user` access user virtual addresses from
//! supervisor mode by temporarily enabling `sstatus.SUM` around the raw copy
//! loop. Each faulting load or store instruction is bracketed by a pair of
//! exported assembly labels that form a fixup-table entry in
//! `RV64_FIXUP_TABLE`.
//!
//! When a kernel-mode page fault fires with `sepc` inside one of these
//! brackets, `dispatch_trap_frame` calls `fixup_lookup`, rewrites `sepc` to
//! the recovery stub, and places the fault address in `a0` (via `frame.stval`)
//! before issuing `sret`.  The recovery stub then returns to the Rust caller
//! with `a0 != 0`, which the Rust wrapper converts to `Err(FaultInfo)`.
//!
//! # Null-pointer note
//!
//! The success/failure convention uses `a0 == 0` for success and `a0 == stval`
//! for failure.  User virtual address 0 is reserved as the null-pointer zone
//! by the txKernel allocator and will never be mapped; a fault at VA 0 cannot
//! reach this path.

use tx_hal::{FaultInfo, UserPtr, VirtAddr};

#[cfg(target_arch = "riscv64")]
const RV64_SSTATUS_SUM: usize = 1 << 18;

// ---------------------------------------------------------------------------
// Assembly: copy_from_user and copy_to_user loops with fixup labels
// ---------------------------------------------------------------------------

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text, "ax"
    .align 2

    /*
     * tx_rv64_cfu_raw — copy_from_user byte loop
     *
     * a0 = dst (kernel VA, writable)
     * a1 = src (user VA, readable)
     * a2 = len (byte count)
     * Returns: a0 = 0 on success; a0 = fault VA on failure (set by trap shell).
     *
     * The load at tx_rv64_cfu_ld_s..tx_rv64_cfu_ld_e is covered by a fixup
     * entry.  On page fault, the trap shell patches sepc → tx_rv64_cfu_fault
     * and x[10] (a0) → stval before sret.
     */
    .globl tx_rv64_cfu_raw
    .type  tx_rv64_cfu_raw, @function
tx_rv64_cfu_raw:
    beqz    a2, .Lcfu_ok
.Lcfu_loop:
.globl tx_rv64_cfu_ld_s
tx_rv64_cfu_ld_s:
    lbu     t0, 0(a1)
.globl tx_rv64_cfu_ld_e
tx_rv64_cfu_ld_e:
    sb      t0, 0(a0)
    addi    a0, a0, 1
    addi    a1, a1, 1
    addi    a2, a2, -1
    bnez    a2, .Lcfu_loop
.Lcfu_ok:
    li      a0, 0
    ret
    /* Trap shell jumps here after: sepc = tx_rv64_cfu_fault, a0 = stval. */
.globl tx_rv64_cfu_fault
tx_rv64_cfu_fault:
    ret                     /* return with a0 = fault VA (non-zero) */
    .size tx_rv64_cfu_raw, . - tx_rv64_cfu_raw

    /*
     * tx_rv64_ctu_raw — copy_to_user byte loop
     *
     * a0 = dst (user VA, writable)
     * a1 = src (kernel VA, readable)
     * a2 = len
     * Returns: a0 = 0 on success; a0 = fault VA on failure (set by trap shell).
     *
     * The store at tx_rv64_ctu_st_s..tx_rv64_ctu_st_e is covered by a fixup
     * entry.  On page fault, stval = the user dst address being written (= a0
     * at the faulting iteration), which the trap shell puts back in a0.
     */
    .globl tx_rv64_ctu_raw
    .type  tx_rv64_ctu_raw, @function
tx_rv64_ctu_raw:
    beqz    a2, .Lctu_ok
.Lctu_loop:
    lbu     t0, 0(a1)       /* load from kernel (safe) */
.globl tx_rv64_ctu_st_s
tx_rv64_ctu_st_s:
    sb      t0, 0(a0)       /* store to user (may fault) */
.globl tx_rv64_ctu_st_e
tx_rv64_ctu_st_e:
    addi    a0, a0, 1
    addi    a1, a1, 1
    addi    a2, a2, -1
    bnez    a2, .Lctu_loop
.Lctu_ok:
    li      a0, 0
    ret
    /* Trap shell jumps here after: sepc = tx_rv64_ctu_fault, a0 = stval. */
.globl tx_rv64_ctu_fault
tx_rv64_ctu_fault:
    ret                     /* return with a0 = fault VA (non-zero) */
    .size tx_rv64_ctu_raw, . - tx_rv64_ctu_raw
"#
);

// ---------------------------------------------------------------------------
// Board-internal fixup table
// ---------------------------------------------------------------------------

/// An entry in the board's internal fixup table.
///
/// Fields hold function-pointer-typed labels so they can be stored in a
/// `static` without requiring a const fn-pointer-to-integer cast (which is
/// not available in stable no_std Rust).  The addresses are compared as
/// `usize` at lookup time.
#[cfg(target_arch = "riscv64")]
struct RawFixupEntry {
    pc_start: unsafe extern "C" fn(),
    pc_end: unsafe extern "C" fn(),
    recovery_pc: unsafe extern "C" fn(),
}

// SAFETY: entries are read-only after link time; no interior mutability.
#[cfg(target_arch = "riscv64")]
unsafe impl Sync for RawFixupEntry {}

#[cfg(target_arch = "riscv64")]
unsafe extern "C" {
    /// First PC of the copy_from_user load (inclusive).
    fn tx_rv64_cfu_ld_s();
    /// First PC past the copy_from_user load (exclusive).
    fn tx_rv64_cfu_ld_e();
    /// copy_from_user recovery stub (trap shell redirects here on fault).
    fn tx_rv64_cfu_fault();

    /// First PC of the copy_to_user store (inclusive).
    fn tx_rv64_ctu_st_s();
    /// First PC past the copy_to_user store (exclusive).
    fn tx_rv64_ctu_st_e();
    /// copy_to_user recovery stub.
    fn tx_rv64_ctu_fault();

    /// Byte-loop copyin function.
    fn tx_rv64_cfu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
    /// Byte-loop copyout function.
    fn tx_rv64_ctu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
}

#[cfg(target_arch = "riscv64")]
static RV64_FIXUP_TABLE: [RawFixupEntry; 2] = [
    // copy_from_user: the lbu instruction at tx_rv64_cfu_ld_s..tx_rv64_cfu_ld_e
    RawFixupEntry {
        pc_start: tx_rv64_cfu_ld_s,
        pc_end: tx_rv64_cfu_ld_e,
        recovery_pc: tx_rv64_cfu_fault,
    },
    // copy_to_user: the sb instruction at tx_rv64_ctu_st_s..tx_rv64_ctu_st_e
    RawFixupEntry {
        pc_start: tx_rv64_ctu_st_s,
        pc_end: tx_rv64_ctu_st_e,
        recovery_pc: tx_rv64_ctu_fault,
    },
];

/// Search the board-internal fixup table for a kernel-mode fault at `fault_pc`.
///
/// Returns `Some(recovery_pc)` when a fixup entry covers the PC, otherwise
/// `None` (the fault should be forwarded to the kernel trap sink).
pub(crate) fn fixup_lookup(fault_pc: usize) -> Option<usize> {
    #[cfg(target_arch = "riscv64")]
    {
        for entry in &RV64_FIXUP_TABLE {
            let start = entry.pc_start as usize;
            let end = entry.pc_end as usize;
            if fault_pc >= start && fault_pc < end {
                return Some(entry.recovery_pc as usize);
            }
        }
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = fault_pc;
    None
}

// ---------------------------------------------------------------------------
// SUM window
// ---------------------------------------------------------------------------

#[cfg(target_arch = "riscv64")]
struct UserMemoryAccessGuard {
    old_sum_enabled: bool,
}

#[cfg(target_arch = "riscv64")]
impl UserMemoryAccessGuard {
    unsafe fn enable() -> Self {
        let old_sstatus: usize;
        unsafe {
            core::arch::asm!(
                "csrr {old_sstatus}, sstatus",
                "csrs sstatus, {sum}",
                old_sstatus = lateout(reg) old_sstatus,
                sum = in(reg) RV64_SSTATUS_SUM,
                options(nomem, nostack)
            );
        }
        Self {
            old_sum_enabled: old_sstatus & RV64_SSTATUS_SUM != 0,
        }
    }
}

#[cfg(target_arch = "riscv64")]
impl Drop for UserMemoryAccessGuard {
    fn drop(&mut self) {
        if !self.old_sum_enabled {
            unsafe {
                core::arch::asm!(
                    "csrc sstatus, {sum}",
                    sum = in(reg) RV64_SSTATUS_SUM,
                    options(nomem, nostack)
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Board-internal user-access primitives
// ---------------------------------------------------------------------------
//
// Used by `signal_frame.rs` for the rare kernel-side "write a struct to a
// user-stack frame" path. The general user-access flow has moved to
// `tx_subsystems::vm::AddressSpace::copy_*_user`, which walks recipes
// eagerly and never enters the SUM/fixup window. Signal-frame writes
// are special: they happen synchronously while the user is suspended
// in the trap shell, and the user mapping is implicitly trusted to
// cover the chosen stack page.

/// Copy `dst.len()` bytes from user-space `src` to kernel-side `dst`.
///
/// # Safety
///
/// * `dst` must be a valid, writable kernel slice.
/// * `src` is interpreted in the currently installed user page table.
/// * The fixup table must be fully populated before this is called
///   (guaranteed after `init_early` returns).
pub(crate) unsafe fn board_copy_from_user(
    dst: &mut [u8],
    src: UserPtr<u8>,
) -> Result<(), FaultInfo> {
    if dst.is_empty() {
        return Ok(());
    }

    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: dst is a valid kernel slice (caller); src is the
        // user address passed by the caller; the fixup table covers
        // the load instruction so a fault becomes an Err rather than
        // a panic. SUM is restored when the guard drops.
        let _sum = unsafe { UserMemoryAccessGuard::enable() };
        let fault_va = unsafe { tx_rv64_cfu_raw(dst.as_mut_ptr(), src.as_ptr(), dst.len()) };
        if fault_va == 0 {
            Ok(())
        } else {
            Err(FaultInfo {
                address: VirtAddr(fault_va),
                write: false,
                instruction: false,
                from_user: false,
            })
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = dst;
        Err(FaultInfo {
            address: VirtAddr(src.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }
}

/// Copy `src.len()` bytes from kernel-side `src` to user-space `dst`.
///
/// # Safety
///
/// * `src` must be a valid, readable kernel slice.
/// * `dst` is interpreted in the currently installed user page table.
pub(crate) unsafe fn board_copy_to_user(dst: UserPtr<u8>, src: &[u8]) -> Result<(), FaultInfo> {
    if src.is_empty() {
        return Ok(());
    }

    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: src is a valid kernel slice (caller); dst is the
        // user address passed by the caller; the fixup table covers
        // the store instruction. SUM is restored when the guard drops.
        let _sum = unsafe { UserMemoryAccessGuard::enable() };
        let fault_va = unsafe { tx_rv64_ctu_raw(dst.as_ptr(), src.as_ptr() as *mut u8, src.len()) };
        if fault_va == 0 {
            Ok(())
        } else {
            Err(FaultInfo {
                address: VirtAddr(fault_va),
                write: true,
                instruction: false,
                from_user: false,
            })
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = src;
        Err(FaultInfo {
            address: VirtAddr(dst.addr()),
            write: true,
            instruction: false,
            from_user: false,
        })
    }
}
