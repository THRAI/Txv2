//! Feature-gated trap-trace instrumentation.
//!
//! Enabled via the `trap-trace` cargo feature on the board crate (or
//! the forwarding feature on `tx-kernel-riscv64-qemu-virt`). Off by
//! default — production runs do not emit per-trap log lines.
//!
//! ## Wire format
//!
//! Each record is one line, prefixed `txdbg:` for grep, followed by a
//! record kind and `key=0xHEX` pairs. Hex values are 16 nibbles
//! (`console_write_hex` outputs full 64-bit width). Records are
//! emitted in monotonic order from the local hart; under `-smp 1`
//! the stream is the canonical execution timeline.
//!
//! Two record kinds today:
//!
//! - `txdbg:trap n=N kind=K pc=PC ...` — emitted from
//!   `dispatch_trap_frame` on every user-mode trap. `N` is a global
//!   counter, `K` is one of `SY` (syscall), `iPF` (instruction page
//!   fault), `lPF` (load page fault), `sPF` (store page fault), `?`
//!   (other). For syscalls the line carries `a7`, `a0`, `a1`, `a2`;
//!   for faults it carries `stval`, `ra`, `a0`, `a1`.
//!
//! - `txdbg:ent n=N pc=PC a0=A0 sp=SP` — emitted from
//!   `enter_userspace_with_context` immediately before the user-mode
//!   `sret`. `N` is the same counter family as `txdbg:trap`. Pairing
//!   `txdbg:trap n=K` with `txdbg:ent n=K+1` shows which return
//!   value the kernel handed back to userspace for trap `K`.
//!
//! ## Parser contract
//!
//! `cargo xtask trap-trace --serial PATH` reads a serial log,
//! extracts these lines, and prints a paired summary (syscall NR
//! mnemonic + return value, page-fault sepc + stval). The parser
//! rejects malformed records loudly — the format is intended to be
//! grep-stable, not free-form.
//!
//! ## Cost when disabled
//!
//! `record_trap` and `record_entry` are `#[inline]` and have empty
//! bodies under `cfg(not(feature = "trap-trace"))`. The compiler
//! optimises them away in release builds; even in debug builds the
//! cost is one no-op call per trap.

use tx_hal::UserTrapContext;

use crate::trap::Rv64TrapFrame;

#[cfg(all(target_arch = "riscv64", feature = "trap-trace"))]
use crate::trap::{X_A0, X_A1, X_A2, X_A7, X_RA, X_SP};

#[cfg(all(target_arch = "riscv64", feature = "trap-trace"))]
use core::sync::atomic::{AtomicUsize, Ordering};

/// Monotonic trap counter shared by `record_trap` and `record_entry`.
/// `record_trap` increments before emit; `record_entry` reads
/// (without increment) so a `txdbg:trap n=K` record paired with the
/// next `txdbg:ent n=K+1` corresponds to the same userspace
/// round-trip.
#[cfg(all(target_arch = "riscv64", feature = "trap-trace"))]
static TRACE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Emit a `txdbg:trap` record for a from-user trap. Caller is
/// responsible for the `from_user` check (the macro is no-op if the
/// trap was from kernel mode — kernel-mode traps are panics in v1).
#[inline]
#[cfg_attr(
    not(all(target_arch = "riscv64", feature = "trap-trace")),
    allow(unused_variables)
)]
pub(crate) fn record_trap(scause_low: u8, frame: &Rv64TrapFrame) {
    #[cfg(all(target_arch = "riscv64", feature = "trap-trace"))]
    {
        let n = TRACE_COUNTER.fetch_add(1, Ordering::AcqRel);
        let kind: &[u8] = match scause_low {
            8 => b"SY",
            12 => b"iPF",
            13 => b"lPF",
            15 => b"sPF",
            _ => b"?",
        };
        crate::trap::console_write_literal(b"\ntxdbg:trap n=0x");
        crate::trap::console_write_hex(n);
        crate::trap::console_write_literal(b" kind=");
        crate::trap::console_write_literal(kind);
        crate::trap::console_write_literal(b" pc=0x");
        crate::trap::console_write_hex(frame.sepc);
        if scause_low == 8 {
            crate::trap::console_write_literal(b" a7=0x");
            crate::trap::console_write_hex(frame.x[X_A7]);
            crate::trap::console_write_literal(b" a0=0x");
            crate::trap::console_write_hex(frame.x[X_A0]);
            crate::trap::console_write_literal(b" a1=0x");
            crate::trap::console_write_hex(frame.x[X_A1]);
            crate::trap::console_write_literal(b" a2=0x");
            crate::trap::console_write_hex(frame.x[X_A2]);
        } else {
            crate::trap::console_write_literal(b" stval=0x");
            crate::trap::console_write_hex(frame.stval);
            crate::trap::console_write_literal(b" ra=0x");
            crate::trap::console_write_hex(frame.x[X_RA]);
            crate::trap::console_write_literal(b" a0=0x");
            crate::trap::console_write_hex(frame.x[X_A0]);
            crate::trap::console_write_literal(b" a1=0x");
            crate::trap::console_write_hex(frame.x[X_A1]);
        }
        crate::trap::console_write_literal(b"\n");
    }
}

/// Emit a `txdbg:ent` record immediately before sret'ing back to
/// userspace. The `n` counter is the trap counter's *current* value
/// (without increment), so `txdbg:ent n=K` follows the most recent
/// `txdbg:trap n=K-1` and reflects the return value the kernel is
/// handing back to userspace for that trap.
#[inline]
#[cfg_attr(
    not(all(target_arch = "riscv64", feature = "trap-trace")),
    allow(unused_variables)
)]
pub(crate) fn record_entry(ctx: &UserTrapContext) {
    #[cfg(all(target_arch = "riscv64", feature = "trap-trace"))]
    {
        let n = TRACE_COUNTER.load(Ordering::Acquire);
        crate::trap::console_write_literal(b"txdbg:ent n=0x");
        crate::trap::console_write_hex(n);
        crate::trap::console_write_literal(b" pc=0x");
        crate::trap::console_write_hex(ctx.pc);
        crate::trap::console_write_literal(b" ra=0x");
        crate::trap::console_write_hex(ctx.regs[X_RA]);
        crate::trap::console_write_literal(b" a0=0x");
        crate::trap::console_write_hex(ctx.regs[X_A0]);
        crate::trap::console_write_literal(b" sp=0x");
        crate::trap::console_write_hex(ctx.regs[X_SP]);
        crate::trap::console_write_literal(b"\n");
    }
}
